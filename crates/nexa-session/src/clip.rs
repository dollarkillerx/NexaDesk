//! Drives [`ClipSync`] over the Clipboard stream on the `clipboard` thread (§1, §8).
//!
//! Promised formats, §8.6's open note: AppKit calls a promise provider on the owner's
//! main thread, and a synchronous fetch there blocks that thread, and the app that is
//! pasting, until the content has arrived. So no promise ever reaches the pasteboard,
//! on the viewer or the agent: every non-inline format is fetched first, then
//! everything is written as ordinary data. The pasteboard entry appears when the data
//! is here; until then the previous contents stay. The peer's paste gate keeps a paste
//! waiting while this side is still fetching (`ClipCommand::GateProgress`), so the paste
//! lands on the new content.
//!
//! (Up to 0.1.3 the agent wrote promises and answered them from the main thread, which
//! then waited out the whole download of a large image, and crashed when the clipboard
//! changed meanwhile.)

use std::{
    collections::{BTreeSet, VecDeque},
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

use crate::{
    bulk::BulkLimiter,
    framed::{FramedRecv, FramedSend},
};

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const QUEUE: usize = 256;
/// Data chunks this large are bulk (§4.9) and wait for the session's [`BulkLimiter`].
/// Announcements, acknowledgements and fetches never do.
const BULK_MIN_FRAME: usize = 4096;

/// A pasteboard the driver can poll.
pub trait ClipPlatform: ClipboardBackend {
    /// A description of the clipboard if it changed since the last call.
    fn poll(&mut self) -> nexa_clip::Result<Option<LocalSnapshot>>;
}

enum DriverMsg {
    Received(ClipMessage),
    StreamEnded,
    PasteQuery(SyncSender<Option<Gen>>),
    Policy(ClipPolicy),
    Stop,
}

/// The driver's queue, made before the backend.
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
    /// Starts the driver and the two stream tasks. Must run inside a tokio runtime.
    pub fn spawn(
        self,
        backend: Box<dyn ClipPlatform>,
        device: DeviceId,
        streams: (FramedSend, FramedRecv),
        callbacks: ClipCallbacks,
        bulk: Arc<BulkLimiter>,
    ) -> ClipSession {
        let (send, recv) = streams;
        // Unbounded, but not without limit: the peer asks for one fetch window (1 MiB) at
        // a time. A bounded queue made the driver wait while a window drained at the bulk
        // rate, seconds at the floor, answering no paste query meanwhile.
        let (control_tx, control_rx) = async_mpsc::unbounded_channel();
        let (data_tx, data_rx) = async_mpsc::unbounded_channel();
        let ClipChannel { tx, rx } = self;
        let writer = tokio::spawn(write_loop(send, control_rx, data_rx, bulk));
        let reader = tokio::spawn(read_loop(recv, tx.clone()));
        let config = ClipConfig {
            // §8.6: a restart must not start below values the peer already saw.
            initial_lamport: wall_clock_millis(),
            // Bulk writes are paced per message (§4.9); see `transfer::CHUNK`.
            chunk_size: crate::transfer::CHUNK as u64,
            ..ClipConfig::default()
        };
        let driver = Driver {
            backend,
            sync: ClipSync::new(device, config),
            out: Outbound {
                control: control_tx,
                data: data_tx,
            },
            callbacks,
            prefetch: None,
            polled: Vec::new(),
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
    /// `ClipCommand::GateProgress`, for `InputGate::on_progress`.
    pub on_gate_progress: Box<dyn Fn(Gen) + Send>,
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
    ///
    /// Never blocks longer than `limit`: it runs on the input thread, where a wait stops
    /// every key and click. A full queue (the driver busy with a large transfer) answers
    /// `None`, which lets the paste go as if nothing were pending.
    pub fn paste_requirement(&self, limit: Duration) -> Option<Gen> {
        let (reply, answer) = mpsc::sync_channel(1);
        self.tx.try_send(DriverMsg::PasteQuery(reply)).ok()?;
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

/// Where the driver's messages go: data chunks in one queue, everything else in another
/// that the writer always empties first. A newer copy's announcement, an
/// acknowledgement or a fetch then never waits behind a megabyte of an older picture.
/// Chunks stay in order among themselves, and the receiver drops chunks of a generation
/// it is not fetching, so nothing depends on the order between the two.
struct Outbound {
    control: async_mpsc::UnboundedSender<ClipMessage>,
    data: async_mpsc::UnboundedSender<ClipMessage>,
}

impl Outbound {
    fn send(&self, message: ClipMessage) -> Result<(), ()> {
        let queue = match message {
            ClipMessage::Data(_) => &self.data,
            _ => &self.control,
        };
        queue.send(message).map_err(|_| ())
    }
}

/// The next message to write, small ones before chunks; whether it is a chunk. `None`
/// once both queues are closed and empty: a closed one only drops out.
async fn next_message(
    control: &mut async_mpsc::UnboundedReceiver<ClipMessage>,
    data: &mut async_mpsc::UnboundedReceiver<ClipMessage>,
) -> Option<(ClipMessage, bool)> {
    tokio::select! {
        biased;
        Some(m) = control.recv() => Some((m, false)),
        Some(m) = data.recv() => Some((m, true)),
        else => None,
    }
}

async fn write_loop(
    mut send: FramedSend,
    mut control: async_mpsc::UnboundedReceiver<ClipMessage>,
    mut data: async_mpsc::UnboundedReceiver<ClipMessage>,
    bulk: Arc<BulkLimiter>,
) {
    while let Some((message, is_data)) = next_message(&mut control, &mut data).await {
        let body = message.encode();
        if is_data && body.len() >= BULK_MIN_FRAME {
            bulk.acquire(body.len()).await;
        }
        if send.send_body(&body).await.is_err() {
            return;
        }
    }
}

struct Prefetch {
    generation: Gen,
    formats: Vec<FormatInfo>,
    inline: Vec<(ClipKind, Bytes)>,
    waiting: BTreeSet<ClipKind>,
}

struct Driver {
    backend: Box<dyn ClipPlatform>,
    sync: ClipSync<Instant>,
    out: Outbound,
    callbacks: ClipCallbacks,
    prefetch: Option<Prefetch>,
    /// The content of the last change polled. The peer fetches a new copy at once, and
    /// reading it again converted a large picture a second time (TIFF to PNG on a Mac, a
    /// bitmap to PNG on Windows); the engine checks it against what was announced.
    polled: Vec<(ClipKind, Bytes)>,
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
                Ok(DriverMsg::StreamEnded) => self.feed(ClipEvent::Disconnected),
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
    }

    fn poll_backend(&mut self) {
        if let Ok(Some(snapshot)) = self.backend.poll() {
            self.polled = snapshot.data.clone();
            self.feed(ClipEvent::LocalChanged(snapshot));
        }
    }

    /// `kind` as last polled, or read now if the poll did not hold it.
    fn read_local(&mut self, kind: ClipKind) -> nexa_clip::Result<Bytes> {
        match self.polled.iter().find(|(k, _)| *k == kind) {
            Some((_, bytes)) => Ok(bytes.clone()),
            None => self.backend.read(kind).map(Bytes::from),
        }
    }

    fn feed(&mut self, event: ClipEvent) {
        let commands = self.sync.handle(event, Instant::now());
        self.apply(commands);
    }

    fn apply(&mut self, commands: Vec<ClipCommand>) {
        let mut queue: VecDeque<ClipCommand> = commands.into();
        while let Some(command) = queue.pop_front() {
            let more = match command {
                ClipCommand::Send(m) => {
                    if self.out.send(m).is_err() {
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
                    let result = self.read_local(kind);
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
                } => self.on_prefetched(generation, kind, Some(data)),
                ClipCommand::FailDeferred {
                    generation, kind, ..
                } => self.on_prefetched(generation, kind, None),
                ClipCommand::GateRelease { generation } => {
                    (self.callbacks.on_gate_release)(generation);
                    Vec::new()
                }
                ClipCommand::GateProgress { generation } => {
                    (self.callbacks.on_gate_progress)(generation);
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
        if missing.is_empty() {
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

    use nexa_clip::{ClipAnnounce, ClipData, ClipStatus};

    use super::*;

    const ME: DeviceId = DeviceId(*b"clip-driver-test");

    fn chunk(offset: u64) -> ClipMessage {
        ClipMessage::Data(ClipData {
            generation: (1, ME),
            kind: ClipKind::Png,
            offset,
            bytes: Bytes::from_static(&[0; 16]),
            eof: false,
            status: ClipStatus::Ok,
        })
    }

    #[tokio::test]
    async fn a_new_copy_is_announced_before_the_chunks_of_an_old_one() {
        let (control_tx, mut control) = async_mpsc::unbounded_channel();
        let (data_tx, mut data) = async_mpsc::unbounded_channel();
        let out = Outbound {
            control: control_tx,
            data: data_tx,
        };
        for offset in [0, 16, 32] {
            out.send(chunk(offset)).unwrap();
        }
        out.send(ClipMessage::Announce(ClipAnnounce {
            generation: (2, ME),
            origin: ME,
            formats: Vec::new(),
            inline: Vec::new(),
        }))
        .unwrap();
        let mut order = Vec::new();
        drop(out);
        while let Some((m, is_data)) = next_message(&mut control, &mut data).await {
            order.push(match m {
                ClipMessage::Data(d) => {
                    assert!(is_data);
                    format!("chunk {}", d.offset)
                }
                _ => "announce".into(),
            });
        }
        assert_eq!(order, ["announce", "chunk 0", "chunk 16", "chunk 32"]);
    }

    /// A clipboard whose reads are counted: a picture on it costs a conversion each.
    struct Counting {
        reads: Arc<AtomicU32>,
        next: Option<LocalSnapshot>,
    }

    impl ClipboardBackend for Counting {
        fn read_formats(&mut self) -> nexa_clip::Result<Vec<FormatInfo>> {
            Ok(Vec::new())
        }
        fn read(&mut self, kind: ClipKind) -> nexa_clip::Result<Vec<u8>> {
            self.reads.fetch_add(1, Relaxed);
            Err(nexa_clip::ClipError::NotAvailable(kind))
        }
        fn write_promise(
            &mut self,
            _: Gen,
            _: &[FormatInfo],
            _: &[(ClipKind, Bytes)],
        ) -> nexa_clip::Result<()> {
            Ok(())
        }
    }

    impl ClipPlatform for Counting {
        fn poll(&mut self) -> nexa_clip::Result<Option<LocalSnapshot>> {
            Ok(self.next.take())
        }
    }

    #[test]
    fn a_copy_just_polled_is_served_without_reading_it_again() {
        let picture = Bytes::from_static(b"\x89PNG a large picture");
        let reads = Arc::new(AtomicU32::new(0));
        let (control, _control_rx) = async_mpsc::unbounded_channel();
        let (data, _data_rx) = async_mpsc::unbounded_channel();
        let mut driver = Driver {
            backend: Box::new(Counting {
                reads: reads.clone(),
                next: Some(LocalSnapshot {
                    formats: vec![FormatInfo::of(ClipKind::Png, &picture)],
                    data: vec![(ClipKind::Png, picture.clone())],
                    owner_marker: false,
                    sensitive: false,
                }),
            }),
            sync: ClipSync::new(ME, ClipConfig::default()),
            out: Outbound { control, data },
            callbacks: ClipCallbacks {
                on_gate_release: Box::new(|_| {}),
                on_gate_progress: Box::new(|_| {}),
                on_notice: Box::new(|_| {}),
            },
            prefetch: None,
            polled: Vec::new(),
        };
        driver.poll_backend();
        assert_eq!(driver.read_local(ClipKind::Png).unwrap(), picture);
        assert_eq!(reads.load(Relaxed), 0, "not read a second time");
        // What the poll did not hold is read as before.
        assert!(driver.read_local(ClipKind::Html).is_err());
        assert_eq!(reads.load(Relaxed), 1);
    }

    #[test]
    fn a_paste_query_does_not_wait_on_a_full_queue() {
        let (tx, _rx) = mpsc::sync_channel(1);
        tx.try_send(DriverMsg::StreamEnded).unwrap();
        let handle = ClipHandle { tx };
        let started = Instant::now();
        assert_eq!(handle.paste_requirement(Duration::from_millis(30)), None);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_paste_query_waits_at_most_its_limit() {
        let (tx, _rx) = mpsc::sync_channel(4);
        let handle = ClipHandle { tx };
        let started = Instant::now();
        assert_eq!(handle.paste_requirement(Duration::from_millis(30)), None);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
