//! Drives [`ClipSync`] over the Clipboard stream on the `clipboard` thread (§1, §8).
//!
//! Promised formats, §8.6's open note: AppKit calls the promise provider on the owner's
//! main thread, and a synchronous fetch there would freeze the UI. So there are two
//! modes, and the UI side (the viewer) never lets a promise reach the pasteboard:
//!
//! - [`PromiseMode::Eager`]: fetch every non-inline format first, then write everything
//!   as ordinary data. The pasteboard entry appears when the data is here; until then
//!   the previous contents stay. Nothing ever blocks in a provider.
//! - [`PromiseMode::Lazy`]: write promises and let the provider block in the resolver,
//!   for the agent, whose main thread has no UI to freeze and where promises keep the
//!   §8.3 paste gate fast.

use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    sync::{
        Arc,
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use bytes::Bytes;
use nexa_clip::{
    ClipCommand, ClipConfig, ClipEvent, ClipKind, ClipMessage, ClipNotice, ClipPolicy, ClipSync,
    ClipboardBackend, DeviceId, FormatInfo, Gen, LocalSnapshot,
};
use tokio::{sync::mpsc as async_mpsc, task::JoinHandle as TaskHandle};

use crate::framed::{FramedRecv, FramedSend};

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const QUEUE: usize = 256;
const OUTBOUND_QUEUE: usize = 32;
/// Longest a provider call may block on the agent. The fetch itself gives up after
/// 10 s without progress (§8.2); this also covers a large transfer that keeps moving.
const RESOLVE_LIMIT: Duration = Duration::from_secs(60);
/// §4.9 wants bulk reliable writes under the rate controller; until that is wired to
/// streams, this keeps a clipboard transfer from starving video.
const BULK_BYTES_PER_SEC: u64 = 12_500_000;
const BULK_MIN_FRAME: usize = 4096;

/// Answers a promise on whichever thread AppKit calls it (same shape as
/// `nexa_clip::platform::macos::PromiseResolver`).
pub type PromiseResolver = Arc<dyn Fn(Gen, ClipKind) -> Option<Vec<u8>> + Send + Sync>;

/// A pasteboard the driver can poll.
pub trait ClipPlatform: ClipboardBackend {
    /// A description of the clipboard if it changed since the last call.
    fn poll(&mut self) -> nexa_clip::Result<Option<LocalSnapshot>>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromiseMode {
    Lazy,
    Eager,
}

enum DriverMsg {
    Received(ClipMessage),
    StreamEnded,
    Promise {
        generation: Gen,
        kind: ClipKind,
        reply: SyncSender<Option<Vec<u8>>>,
    },
    PasteQuery(SyncSender<Option<Gen>>),
    Policy(ClipPolicy),
    Stop,
}

/// Creates the channel first, so a backend can be built around its resolver.
pub struct ClipChannel {
    tx: SyncSender<DriverMsg>,
    rx: Receiver<DriverMsg>,
}

impl Default for ClipChannel {
    fn default() -> Self {
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        Self { tx, rx }
    }
}

impl ClipChannel {
    /// Blocks its caller while the content is fetched; see the module notes.
    pub fn resolver(&self) -> PromiseResolver {
        let tx = self.tx.clone();
        Arc::new(move |generation, kind| {
            let (reply, answer) = mpsc::sync_channel(1);
            tx.send(DriverMsg::Promise {
                generation,
                kind,
                reply,
            })
            .ok()?;
            answer.recv_timeout(RESOLVE_LIMIT).ok().flatten()
        })
    }

    /// Starts the driver and the two stream tasks. Must run inside a tokio runtime.
    pub fn spawn(
        self,
        backend: Box<dyn ClipPlatform>,
        device: DeviceId,
        mode: PromiseMode,
        streams: (FramedSend, FramedRecv),
        callbacks: ClipCallbacks,
    ) -> ClipSession {
        let (send, recv) = streams;
        let (out_tx, out_rx) = async_mpsc::channel(OUTBOUND_QUEUE);
        let ClipChannel { tx, rx } = self;
        let writer = tokio::spawn(write_loop(send, out_rx));
        let reader = tokio::spawn(read_loop(recv, tx.clone()));
        let config = ClipConfig {
            // §8.6: a restart must not start below values the peer already saw.
            initial_lamport: wall_clock_millis(),
            ..ClipConfig::default()
        };
        let driver = Driver {
            backend,
            sync: ClipSync::new(device, config),
            out: out_tx,
            mode,
            callbacks,
            prefetch: None,
            waiters: HashMap::new(),
        };
        let thread = thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || driver.run(rx))
            .expect("spawn thread");
        ClipSession {
            handle: ClipHandle { tx },
            thread,
            writer,
            reader,
        }
    }
}

pub struct ClipCallbacks {
    /// `ClipCommand::GateRelease`, for `InputGate::on_applied`.
    pub on_gate_release: Box<dyn Fn(Gen) + Send>,
    pub on_notice: Box<dyn Fn(ClipNotice) + Send>,
}

fn wall_clock_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[derive(Clone)]
pub struct ClipHandle {
    tx: SyncSender<DriverMsg>,
}

impl ClipHandle {
    /// What a paste must wait for. Polls the pasteboard first, so a copy made moments
    /// ago is seen; `limit` bounds the wait if the clipboard thread is busy.
    pub fn paste_requirement(&self, limit: Duration) -> Option<Gen> {
        let (reply, answer) = mpsc::sync_channel(1);
        self.tx.send(DriverMsg::PasteQuery(reply)).ok()?;
        answer.recv_timeout(limit).ok().flatten()
    }

    pub fn set_policy(&self, policy: ClipPolicy) {
        let _ = self.tx.try_send(DriverMsg::Policy(policy));
    }
}

pub struct ClipSession {
    pub handle: ClipHandle,
    thread: JoinHandle<()>,
    writer: TaskHandle<()>,
    reader: TaskHandle<()>,
}

impl ClipSession {
    /// Ends the driver and fails any promise still being waited for.
    pub fn stop(self) {
        // The writer goes first: if the peer stalled, the driver may be blocked on it.
        self.writer.abort();
        self.reader.abort();
        let _ = self.handle.tx.send(DriverMsg::Stop);
        let _ = self.thread.join();
    }
}

async fn read_loop(mut recv: FramedRecv, tx: SyncSender<DriverMsg>) {
    loop {
        let message = match recv.next_body().await {
            Ok(Some(body)) => ClipMessage::decode(&body),
            _ => break,
        };
        // A malformed message is fatal for the stream (§9.4), like any framing error.
        let Ok(message) = message else { break };
        let mut pending = DriverMsg::Received(message);
        loop {
            match tx.try_send(pending) {
                Ok(()) => break,
                Err(mpsc::TrySendError::Full(back)) => {
                    pending = back;
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                Err(mpsc::TrySendError::Disconnected(_)) => return,
            }
        }
    }
    let _ = tx.try_send(DriverMsg::StreamEnded);
}

async fn write_loop(mut send: FramedSend, mut out: async_mpsc::Receiver<ClipMessage>) {
    let mut next_at = Instant::now();
    while let Some(message) = out.recv().await {
        let body = message.encode();
        if body.len() >= BULK_MIN_FRAME {
            let wait = next_at.saturating_duration_since(Instant::now());
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
            let cost = Duration::from_secs_f64(body.len() as f64 / BULK_BYTES_PER_SEC as f64);
            next_at = next_at.max(Instant::now()) + cost;
        }
        if send.send_body(&body).await.is_err() {
            return;
        }
    }
}

type Waiters = HashMap<(Gen, ClipKind), Vec<SyncSender<Option<Vec<u8>>>>>;

struct Prefetch {
    generation: Gen,
    formats: Vec<FormatInfo>,
    inline: Vec<(ClipKind, Bytes)>,
    waiting: BTreeSet<ClipKind>,
}

struct Driver {
    backend: Box<dyn ClipPlatform>,
    sync: ClipSync<Instant>,
    out: async_mpsc::Sender<ClipMessage>,
    mode: PromiseMode,
    callbacks: ClipCallbacks,
    prefetch: Option<Prefetch>,
    waiters: Waiters,
}

impl Driver {
    fn run(mut self, rx: Receiver<DriverMsg>) {
        self.feed(ClipEvent::Connected);
        let mut next_poll = Instant::now();
        loop {
            let now = Instant::now();
            let wake = self
                .sync
                .next_deadline()
                .map_or(next_poll, |d| d.min(next_poll));
            match rx.recv_timeout(wake.saturating_duration_since(now)) {
                Ok(DriverMsg::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(DriverMsg::Received(m)) => self.feed(ClipEvent::Received(m)),
                Ok(DriverMsg::StreamEnded) => {
                    self.feed(ClipEvent::Disconnected);
                    self.fail_waiters();
                }
                Ok(DriverMsg::Promise {
                    generation,
                    kind,
                    reply,
                }) => {
                    self.waiters
                        .entry((generation, kind))
                        .or_default()
                        .push(reply);
                    self.feed(ClipEvent::FetchRequested { generation, kind });
                }
                Ok(DriverMsg::PasteQuery(reply)) => {
                    self.poll_backend();
                    let _ = reply.try_send(self.sync.paste_requirement());
                }
                Ok(DriverMsg::Policy(policy)) => self.feed(ClipEvent::PolicyChanged(policy)),
                Err(RecvTimeoutError::Timeout) => {}
            }
            let now = Instant::now();
            if self.sync.next_deadline().is_some_and(|d| d <= now) {
                self.feed(ClipEvent::Tick);
            }
            if now >= next_poll {
                next_poll = now + POLL_INTERVAL;
                self.poll_backend();
            }
        }
        self.feed(ClipEvent::Disconnected);
        self.fail_waiters();
    }

    fn fail_waiters(&mut self) {
        for (_, waiters) in self.waiters.drain() {
            for w in waiters {
                let _ = w.try_send(None);
            }
        }
    }

    fn poll_backend(&mut self) {
        if let Ok(Some(snapshot)) = self.backend.poll() {
            self.feed(ClipEvent::LocalChanged(snapshot));
        }
    }

    fn feed(&mut self, event: ClipEvent) {
        let commands = self.sync.handle(event, Instant::now());
        self.apply(commands);
    }

    fn reply_to_waiters(&mut self, generation: Gen, kind: ClipKind, data: Option<&Bytes>) {
        if let Some(waiters) = self.waiters.remove(&(generation, kind)) {
            for w in waiters {
                let _ = w.try_send(data.map(|d| d.to_vec()));
            }
        }
    }

    fn apply(&mut self, commands: Vec<ClipCommand>) {
        let mut queue: VecDeque<ClipCommand> = commands.into();
        while let Some(command) = queue.pop_front() {
            let more = match command {
                ClipCommand::Send(m) => {
                    if self.out.blocking_send(m).is_err() {
                        self.sync.handle(ClipEvent::Disconnected, Instant::now())
                    } else {
                        Vec::new()
                    }
                }
                ClipCommand::WritePromise {
                    generation,
                    formats,
                    inline,
                } => self.on_write(generation, formats, inline),
                ClipCommand::ReadLocal { generation, kind } => {
                    let result = self.backend.read(kind).map(Bytes::from);
                    self.sync.handle(
                        ClipEvent::LocalReadDone {
                            generation,
                            kind,
                            result,
                        },
                        Instant::now(),
                    )
                }
                ClipCommand::FulfillDeferred {
                    generation,
                    kind,
                    data,
                } => {
                    self.reply_to_waiters(generation, kind, Some(&data));
                    self.on_prefetched(generation, kind, Some(data))
                }
                ClipCommand::FailDeferred {
                    generation, kind, ..
                } => {
                    self.reply_to_waiters(generation, kind, None);
                    self.on_prefetched(generation, kind, None)
                }
                ClipCommand::GateRelease { generation } => {
                    (self.callbacks.on_gate_release)(generation);
                    Vec::new()
                }
                ClipCommand::Notify(notice) => {
                    (self.callbacks.on_notice)(notice);
                    Vec::new()
                }
            };
            queue.extend(more);
        }
    }

    fn on_write(
        &mut self,
        generation: Gen,
        formats: Vec<FormatInfo>,
        inline: Vec<(ClipKind, Bytes)>,
    ) -> Vec<ClipCommand> {
        let missing: BTreeSet<ClipKind> = formats
            .iter()
            .map(|f| f.kind)
            .filter(|k| !inline.iter().any(|(kind, _)| kind == k))
            .collect();
        if self.mode == PromiseMode::Lazy || missing.is_empty() {
            let result = self.backend.write_promise(generation, &formats, &inline);
            return self
                .sync
                .handle(ClipEvent::WriteDone { generation, result }, Instant::now());
        }
        // An older prefetch still running has been superseded.
        self.prefetch = Some(Prefetch {
            generation,
            formats,
            inline,
            waiting: missing.clone(),
        });
        let mut out = Vec::new();
        for kind in missing {
            out.extend(self.sync.handle(
                ClipEvent::FetchRequested { generation, kind },
                Instant::now(),
            ));
        }
        out
    }

    fn on_prefetched(
        &mut self,
        generation: Gen,
        kind: ClipKind,
        data: Option<Bytes>,
    ) -> Vec<ClipCommand> {
        let Some(prefetch) = self.prefetch.as_mut() else {
            return Vec::new();
        };
        if prefetch.generation != generation || !prefetch.waiting.remove(&kind) {
            return Vec::new();
        }
        match data {
            Some(bytes) => prefetch.inline.push((kind, bytes)),
            None => prefetch.formats.retain(|f| f.kind != kind),
        }
        if !prefetch.waiting.is_empty() {
            return Vec::new();
        }
        let Some(done) = self.prefetch.take() else {
            return Vec::new();
        };
        // A local copy made during the download must win, not be overwritten.
        self.poll_backend();
        if self.sync.latest() != Some(done.generation) {
            return Vec::new();
        }
        let result = if done.formats.is_empty() {
            Err(nexa_clip::ClipError::Busy)
        } else {
            self.backend
                .write_promise(done.generation, &done.formats, &done.inline)
        };
        self.sync.handle(
            ClipEvent::WriteDone {
                generation: done.generation,
                result,
            },
            Instant::now(),
        )
    }
}
