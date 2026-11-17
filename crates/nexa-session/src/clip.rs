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
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{
        Arc, Condvar, Mutex,
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
/// A copy that is across sooner than this is never reported as on its way: the
/// toolbar would only flicker.
const PROGRESS_AFTER: Duration = Duration::from_millis(300);
/// How often a copy on its way is reported.
const PROGRESS_EVERY: Duration = Duration::from_millis(200);
/// How often the writer tells the driver how far a format has been written.
const SENT_EVERY: Duration = Duration::from_millis(50);

/// A pasteboard the driver can poll.
pub trait ClipPlatform: ClipboardBackend {
    /// A description of the clipboard if it changed since the last call.
    fn poll(&mut self) -> nexa_clip::Result<Option<LocalSnapshot>>;
    /// The clipboard as it is now, changed or not: what was copied before the session
    /// began, for its first paste. `None` where that cannot be told.
    fn current(&mut self) -> nexa_clip::Result<Option<LocalSnapshot>> {
        Ok(None)
    }
}

/// What [`ClipHandle::paste_requirement`] found out in the time it had.
#[derive(Debug, PartialEq, Eq)]
pub enum PasteNeed {
    /// What the paste must wait for, if anything.
    Known(Option<Gen>),
    /// The clipboard thread is busy; the answer goes to the `late` callback.
    Later,
}

type LateAnswer = Box<dyn FnOnce(Option<Gen>) + Send>;

enum Answer {
    Waiting,
    Given(Option<Gen>),
    /// The asker stopped waiting and left this to call.
    Late(LateAnswer),
    Taken,
}

/// One paste query's answer, on its way from the clipboard thread to the input thread:
/// in the time the input thread may wait, or to its callback after that.
struct PasteReply {
    answer: Mutex<Answer>,
    given: Condvar,
}

impl PasteReply {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            answer: Mutex::new(Answer::Waiting),
            given: Condvar::new(),
        })
    }

    fn give(&self, required: Option<Gen>) {
        let mut answer = self.answer.lock().unwrap_or_else(|e| e.into_inner());
        match std::mem::replace(&mut *answer, Answer::Taken) {
            Answer::Waiting => {
                *answer = Answer::Given(required);
                self.given.notify_all();
            }
            Answer::Late(late) => {
                drop(answer);
                late(required);
            }
            Answer::Given(_) | Answer::Taken => {}
        }
    }

    fn wait(&self, limit: Duration, late: LateAnswer) -> PasteNeed {
        let answer = self.answer.lock().unwrap_or_else(|e| e.into_inner());
        let (mut answer, _) = self
            .given
            .wait_timeout_while(answer, limit, |a| matches!(a, Answer::Waiting))
            .unwrap_or_else(|e| e.into_inner());
        match std::mem::replace(&mut *answer, Answer::Taken) {
            Answer::Given(required) => PasteNeed::Known(required),
            _ => {
                *answer = Answer::Late(late);
                PasteNeed::Later
            }
        }
    }
}

/// Which way a large copy is going.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipDirection {
    /// From the peer's clipboard to this one.
    Receiving,
    /// From this clipboard to the peer's.
    Sending,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipStage {
    /// On its way: `done` of `size` bytes so far.
    Moving,
    /// Receiving: it is on this clipboard now and can be pasted. Sending: the peer has
    /// it on its own.
    Done,
    /// Given up: a newer copy replaced it, it failed (which has its own notice), or the
    /// session ended.
    Dropped,
}

/// A copy too large to travel with its announcement (a picture, a long text), for the
/// toolbar: until it is across, a paste on the receiving side still gets what was there
/// before, and nothing else said so. Reported only once it has taken
/// [`PROGRESS_AFTER`]; `Done` and `Dropped` only follow a `Moving`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipProgress {
    pub direction: ClipDirection,
    pub stage: ClipStage,
    /// The largest of the formats being fetched.
    pub kind: ClipKind,
    pub done: u64,
    /// All fetched formats together.
    pub size: u64,
}

enum DriverMsg {
    Received(ClipMessage),
    /// The writer has put `kind` of `generation` on the stream up to `end`.
    DataSent {
        generation: Gen,
        kind: ClipKind,
        end: u64,
    },
    StreamEnded,
    PasteQuery(Arc<PasteReply>),
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
        let writer = tokio::spawn(write_loop(send, control_rx, data_rx, bulk, tx.clone()));
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
            offered: false,
            receiving: None,
            sending: None,
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
    /// A large copy on its way, across or given up, see [`ClipProgress`].
    pub on_progress: Box<dyn Fn(ClipProgress) + Send>,
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
    /// ago is seen, and on the session's first paste offers the peer what was copied
    /// before the session began.
    ///
    /// Never blocks longer than `limit`: it runs on the input thread, where a wait stops
    /// every key and click. If the clipboard thread has not answered by then (it is
    /// reading or writing a large copy), this returns [`PasteNeed::Later`] and `late` is
    /// called with the answer once there is one, on the clipboard thread; a session that
    /// ends first never calls it. A full queue answers `Known(None)`, which lets the
    /// paste go as if nothing were pending.
    pub fn paste_requirement(
        &self,
        limit: Duration,
        late: impl FnOnce(Option<Gen>) + Send + 'static,
    ) -> PasteNeed {
        let reply = PasteReply::new();
        if self
            .tx
            .try_send(DriverMsg::PasteQuery(reply.clone()))
            .is_err()
        {
            return PasteNeed::Known(None);
        }
        reply.wait(limit, Box::new(late))
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
    driver: SyncSender<DriverMsg>,
) {
    let mut told = Instant::now();
    while let Some((message, is_data)) = next_message(&mut control, &mut data).await {
        // How far this chunk takes its format, and whether it is the last of it.
        let chunk = match &message {
            ClipMessage::Data(d) if !d.bytes.is_empty() => {
                Some((d.generation, d.kind, d.offset + d.bytes.len() as u64, d.eof))
            }
            _ => None,
        };
        let body = message.encode();
        if is_data && body.len() >= BULK_MIN_FRAME {
            bulk.acquire(body.len()).await;
        }
        if send.send_body(&body).await.is_err() {
            return;
        }
        // For the toolbar only: now and then, and never waiting for the driver.
        if let Some((generation, kind, end, last)) = chunk
            && (last || told.elapsed() >= SENT_EVERY)
        {
            told = Instant::now();
            let _ = driver.try_send(DriverMsg::DataSent {
                generation,
                kind,
                end,
            });
        }
    }
}

struct Prefetch {
    generation: Gen,
    formats: Vec<FormatInfo>,
    inline: Vec<(ClipKind, Bytes)>,
    waiting: BTreeSet<ClipKind>,
    /// Bytes of the formats fetched so far.
    fetched: u64,
}

/// A large copy on its way, as last told to the toolbar.
struct Moving {
    generation: Gen,
    kind: ClipKind,
    size: u64,
    started: Instant,
    /// When it was last reported and at how many bytes; `None`: not yet, it may still
    /// be across before anyone would notice.
    reported: Option<(Instant, u64)>,
}

impl Moving {
    fn new(generation: Gen, kind: ClipKind, size: u64) -> Self {
        Self {
            generation,
            kind,
            size,
            started: Instant::now(),
            reported: None,
        }
    }

    /// The report due now, if one is.
    fn due(&mut self, direction: ClipDirection, done: u64, now: Instant) -> Option<ClipProgress> {
        let done = done.min(self.size);
        let due = match self.reported {
            None => now.saturating_duration_since(self.started) >= PROGRESS_AFTER,
            Some((at, last)) => done != last && now.saturating_duration_since(at) >= PROGRESS_EVERY,
        };
        due.then(|| {
            self.reported = Some((now, done));
            self.progress(direction, ClipStage::Moving, done)
        })
    }

    /// How it ended, if anyone was told it had begun.
    fn end(&self, direction: ClipDirection, stage: ClipStage) -> Option<ClipProgress> {
        let done = match stage {
            ClipStage::Done => self.size,
            _ => self.reported.map_or(0, |(_, done)| done),
        };
        self.reported.map(|_| self.progress(direction, stage, done))
    }

    fn progress(&self, direction: ClipDirection, stage: ClipStage, done: u64) -> ClipProgress {
        ClipProgress {
            direction,
            stage,
            kind: self.kind,
            done,
            size: self.size,
        }
    }
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
    /// The session's first paste has been seen, see [`Driver::offer_current`].
    offered: bool,
    /// The copy being fetched from the peer, for the toolbar.
    receiving: Option<Moving>,
    /// The copy the peer is fetching, and how far each format has been written.
    sending: Option<(Moving, BTreeMap<ClipKind, u64>)>,
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
                Ok(DriverMsg::DataSent {
                    generation,
                    kind,
                    end,
                }) => self.on_data_sent(generation, kind, end),
                Ok(DriverMsg::StreamEnded) => self.feed(ClipEvent::Disconnected),
                Ok(DriverMsg::PasteQuery(reply)) => {
                    self.poll_backend();
                    self.offer_current();
                    reply.give(self.sync.paste_requirement());
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
            self.report_progress(Instant::now());
        }
        self.feed(ClipEvent::Disconnected);
        self.end_receiving(ClipStage::Dropped);
        self.end_sending(ClipStage::Dropped);
    }

    fn end_receiving(&mut self, stage: ClipStage) {
        if let Some(report) = self
            .receiving
            .take()
            .and_then(|moving| moving.end(ClipDirection::Receiving, stage))
        {
            (self.callbacks.on_progress)(report);
        }
    }

    fn end_sending(&mut self, stage: ClipStage) {
        if let Some(report) = self
            .sending
            .take()
            .and_then(|(moving, _)| moving.end(ClipDirection::Sending, stage))
        {
            (self.callbacks.on_progress)(report);
        }
    }

    /// The writer got further with a format the peer is fetching.
    fn on_data_sent(&mut self, generation: Gen, kind: ClipKind, end: u64) {
        let Some((current, largest, size)) = self.sync.local_to_fetch() else {
            return;
        };
        if current != generation {
            return; // chunks of a copy that has been replaced since
        }
        if self
            .sending
            .as_ref()
            .is_none_or(|(moving, _)| moving.generation != generation)
        {
            self.end_sending(ClipStage::Dropped);
            self.sending = Some((Moving::new(generation, largest, size), BTreeMap::new()));
        }
        if let Some((_, sent)) = &mut self.sending {
            let so_far = sent.entry(kind).or_insert(0);
            *so_far = (*so_far).max(end);
        }
    }

    /// Tells the toolbar how the large copies under way are getting on, and ends the
    /// one being sent once the peer has it or no longer wants it.
    fn report_progress(&mut self, now: Instant) {
        let fetched = self
            .prefetch
            .as_ref()
            .map(|prefetch| (prefetch.generation, prefetch.fetched));
        if let Some(moving) = &mut self.receiving
            && let Some((generation, fetched)) = fetched
            && generation == moving.generation
        {
            let done = fetched + self.sync.fetch_received(generation);
            if let Some(report) = moving.due(ClipDirection::Receiving, done, now) {
                (self.callbacks.on_progress)(report);
            }
        }

        let Some((moving, sent)) = &mut self.sending else {
            return;
        };
        let generation = moving.generation;
        if self.sync.acked() >= Some(generation) {
            self.end_sending(ClipStage::Done);
        } else if self.sync.paste_requirement() != Some(generation) {
            // Replaced by a newer copy, refused by the peer, or the stream is down.
            self.end_sending(ClipStage::Dropped);
        } else if let Some(report) = moving.due(ClipDirection::Sending, sent.values().sum(), now) {
            (self.callbacks.on_progress)(report);
        }
    }

    fn poll_backend(&mut self) {
        if let Ok(Some(snapshot)) = self.backend.poll() {
            self.polled = snapshot.data.clone();
            self.feed(ClipEvent::LocalChanged(snapshot));
        }
    }

    /// The session's first paste: if neither side has copied anything since it began,
    /// what this side copied before is what the person means to paste, so the peer gets
    /// it now, and the paste waits for it like for any other copy. Not sent when the
    /// session starts: a clipboard is only the peer's business once it is pasted there.
    /// What this app put on the clipboard itself (the peer's own copy from an earlier
    /// session) carries the owner marker and is not offered back.
    fn offer_current(&mut self) {
        if std::mem::replace(&mut self.offered, true) || self.sync.latest().is_some() {
            return;
        }
        if let Ok(Some(snapshot)) = self.backend.current() {
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
        self.end_receiving(ClipStage::Dropped);
        let fetching = || formats.iter().filter(|f| missing.contains(&f.kind));
        if let Some(largest) = fetching().max_by_key(|f| f.size) {
            let size = fetching().map(|f| f.size).sum();
            self.receiving = Some(Moving::new(generation, largest.kind, size));
        }
        self.prefetch = Some(Prefetch {
            generation,
            formats,
            inline,
            waiting: missing.clone(),
            fetched: 0,
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
            Some(bytes) => {
                prefetch.fetched += bytes.len() as u64;
                prefetch.inline.push((kind, bytes));
            }
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
            self.end_receiving(ClipStage::Dropped);
            return Vec::new();
        }
        let result = if done.formats.is_empty() {
            Err(nexa_clip::ClipError::Busy)
        } else {
            self.backend
                .write_promise(done.generation, &done.formats, &done.inline)
        };
        self.end_receiving(match result {
            Ok(()) => ClipStage::Done,
            Err(_) => ClipStage::Dropped,
        });
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

    use nexa_clip::{ClipAnnounce, ClipApplied, ClipData, ClipStatus};

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
    #[derive(Default)]
    struct Counting {
        reads: Arc<AtomicU32>,
        next: Option<LocalSnapshot>,
        /// What was copied before the session began.
        before: Option<LocalSnapshot>,
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
        fn current(&mut self) -> nexa_clip::Result<Option<LocalSnapshot>> {
            Ok(self.before.clone())
        }
    }

    fn text_snapshot(text: &'static [u8], owner_marker: bool) -> LocalSnapshot {
        LocalSnapshot {
            formats: vec![FormatInfo::of(ClipKind::Text, text)],
            data: vec![(ClipKind::Text, Bytes::from_static(text))],
            owner_marker,
            sensitive: false,
        }
    }

    /// A driver on `backend`, connected, with what it sends to the peer.
    fn connected_driver(
        backend: Counting,
    ) -> (
        Driver,
        async_mpsc::UnboundedReceiver<ClipMessage>,
        async_mpsc::UnboundedReceiver<ClipMessage>,
    ) {
        let (control, control_rx) = async_mpsc::unbounded_channel();
        let (data, data_rx) = async_mpsc::unbounded_channel();
        let mut driver = Driver {
            backend: Box::new(backend),
            sync: ClipSync::new(ME, ClipConfig::default()),
            out: Outbound { control, data },
            callbacks: ClipCallbacks {
                on_gate_release: Box::new(|_| {}),
                on_gate_progress: Box::new(|_| {}),
                on_notice: Box::new(|_| {}),
                on_progress: Box::new(|_| {}),
            },
            prefetch: None,
            polled: Vec::new(),
            offered: false,
            receiving: None,
            sending: None,
        };
        driver.feed(ClipEvent::Connected);
        (driver, control_rx, data_rx)
    }

    /// Copy, connect, paste: the paste used to land on whatever the host's clipboard
    /// held, because only copies made during the session were ever sent.
    #[test]
    fn the_first_paste_offers_what_was_copied_before_the_session() {
        let (mut driver, mut sent, _data) = connected_driver(Counting {
            before: Some(text_snapshot(b"copied before connecting", false)),
            ..Counting::default()
        });
        driver.poll_backend();
        assert!(sent.try_recv().is_err(), "nothing leaves before a paste");

        driver.offer_current();
        let Ok(ClipMessage::Announce(announce)) = sent.try_recv() else {
            panic!("the copy is announced at the first paste");
        };
        assert_eq!(
            announce.inline,
            vec![(
                ClipKind::Text,
                Bytes::from_static(b"copied before connecting")
            )]
        );
        assert_eq!(
            driver.sync.paste_requirement(),
            Some(announce.generation),
            "and the paste waits for it"
        );
        driver.offer_current();
        assert!(sent.try_recv().is_err(), "once");
    }

    #[test]
    fn a_copy_made_during_the_session_is_not_replaced_by_an_older_one() {
        // The peer copied since the session began: that is what a paste means.
        let (mut driver, mut sent, _data) = connected_driver(Counting {
            before: Some(text_snapshot(b"old", false)),
            ..Counting::default()
        });
        let theirs = b"copied on the host";
        driver.feed(ClipEvent::Received(ClipMessage::Announce(ClipAnnounce {
            generation: (7, DeviceId(*b"the-other-device")),
            origin: DeviceId(*b"the-other-device"),
            formats: vec![FormatInfo::of(ClipKind::Text, theirs)],
            inline: vec![(ClipKind::Text, Bytes::from_static(theirs))],
        })));
        while sent.try_recv().is_ok() {}
        driver.offer_current();
        assert!(sent.try_recv().is_err());
        assert_eq!(driver.sync.paste_requirement(), None);

        // What this app wrote itself in an earlier session is the peer's own copy.
        let (mut driver, mut sent, _data) = connected_driver(Counting {
            before: Some(text_snapshot(b"from the host, last time", true)),
            ..Counting::default()
        });
        driver.offer_current();
        assert!(sent.try_recv().is_err());
    }

    #[test]
    fn a_copy_just_polled_is_served_without_reading_it_again() {
        let picture = Bytes::from_static(b"\x89PNG a large picture");
        let reads = Arc::new(AtomicU32::new(0));
        let (mut driver, _control_rx, _data_rx) = connected_driver(Counting {
            reads: reads.clone(),
            next: Some(LocalSnapshot {
                formats: vec![FormatInfo::of(ClipKind::Png, &picture)],
                data: vec![(ClipKind::Png, picture.clone())],
                owner_marker: false,
                sensitive: false,
            }),
            before: None,
        });
        driver.poll_backend();
        assert_eq!(driver.read_local(ClipKind::Png).unwrap(), picture);
        assert_eq!(reads.load(Relaxed), 0, "not read a second time");
        // What the poll did not hold is read as before.
        assert!(driver.read_local(ClipKind::Html).is_err());
        assert_eq!(reads.load(Relaxed), 1);
    }

    /// What the toolbar is told, collected.
    fn watch_progress(driver: &mut Driver) -> Arc<Mutex<Vec<ClipProgress>>> {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        driver.callbacks.on_progress = Box::new(move |p| sink.lock().unwrap().push(p));
        seen
    }

    const THEM: DeviceId = DeviceId(*b"the-other-device");

    fn picture(len: usize) -> Bytes {
        Bytes::from((0..len).map(|i| (i * 31 % 251) as u8).collect::<Vec<u8>>())
    }

    fn announce_picture(driver: &mut Driver, generation: Gen, picture: &Bytes) {
        driver.feed(ClipEvent::Received(ClipMessage::Announce(ClipAnnounce {
            generation,
            origin: generation.1,
            formats: vec![FormatInfo::of(ClipKind::Png, picture)],
            inline: Vec::new(),
        })));
    }

    fn deliver(driver: &mut Driver, generation: Gen, picture: &Bytes, from: usize, to: usize) {
        driver.feed(ClipEvent::Received(ClipMessage::Data(ClipData {
            generation,
            kind: ClipKind::Png,
            offset: from as u64,
            bytes: picture.slice(from..to),
            eof: to == picture.len(),
            status: ClipStatus::Ok,
        })));
    }

    /// A picture from the peer took seconds to arrive and nothing said so: a paste in
    /// the meantime got the old content. The toolbar now hears how far it is, and when
    /// it can be pasted.
    #[test]
    fn a_large_copy_from_the_peer_is_reported_on_its_way_and_when_it_is_here() {
        let (mut driver, _sent, _data) = connected_driver(Counting::default());
        let seen = watch_progress(&mut driver);
        let picture = picture(300_000);
        let generation = (7, THEM);
        let start = Instant::now();
        announce_picture(&mut driver, generation, &picture);
        deliver(&mut driver, generation, &picture, 0, 100_000);

        driver.report_progress(start + Duration::from_millis(100));
        assert!(
            seen.lock().unwrap().is_empty(),
            "too soon to be worth saying"
        );
        let later = start + PROGRESS_AFTER + Duration::from_millis(100);
        driver.report_progress(later);
        driver.report_progress(later + Duration::from_millis(10));
        let moving = ClipProgress {
            direction: ClipDirection::Receiving,
            stage: ClipStage::Moving,
            kind: ClipKind::Png,
            done: 100_000,
            size: 300_000,
        };
        assert_eq!(*seen.lock().unwrap(), [moving], "said once per step");

        deliver(&mut driver, generation, &picture, 100_000, 300_000);
        assert_eq!(
            seen.lock().unwrap().last(),
            Some(&ClipProgress {
                stage: ClipStage::Done,
                done: 300_000,
                ..moving
            }),
            "written to this clipboard: it can be pasted"
        );
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_copy_that_is_across_at_once_is_not_reported() {
        let (mut driver, _sent, _data) = connected_driver(Counting::default());
        let seen = watch_progress(&mut driver);
        let picture = picture(100_000);
        announce_picture(&mut driver, (7, THEM), &picture);
        deliver(&mut driver, (7, THEM), &picture, 0, 100_000);
        driver.report_progress(Instant::now() + Duration::from_secs(5));
        assert!(seen.lock().unwrap().is_empty());
    }

    /// A newer copy, here a short text, replaces the picture still on its way: the
    /// toolbar must not go on showing it at 33%.
    #[test]
    fn a_large_copy_replaced_on_its_way_is_reported_as_dropped() {
        let (mut driver, _sent, _data) = connected_driver(Counting::default());
        let seen = watch_progress(&mut driver);
        let picture = picture(300_000);
        let start = Instant::now();
        announce_picture(&mut driver, (7, THEM), &picture);
        deliver(&mut driver, (7, THEM), &picture, 0, 100_000);
        driver.report_progress(start + Duration::from_secs(1));

        let text = b"copied after the picture";
        driver.feed(ClipEvent::Received(ClipMessage::Announce(ClipAnnounce {
            generation: (8, THEM),
            origin: THEM,
            formats: vec![FormatInfo::of(ClipKind::Text, text)],
            inline: vec![(ClipKind::Text, Bytes::from_static(text))],
        })));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(
            (seen[1].direction, seen[1].stage, seen[1].done),
            (ClipDirection::Receiving, ClipStage::Dropped, 100_000)
        );
    }

    /// The same for a copy made here: while the peer fetches it a paste is held, with
    /// every key after it, and the person should see why.
    #[test]
    fn a_large_copy_to_the_peer_is_reported_until_the_peer_has_it() {
        let picture = picture(300_000);
        let (mut driver, mut sent, _data) = connected_driver(Counting {
            next: Some(LocalSnapshot {
                formats: vec![FormatInfo::of(ClipKind::Png, &picture)],
                data: vec![(ClipKind::Png, picture.clone())],
                owner_marker: false,
                sensitive: false,
            }),
            ..Counting::default()
        });
        let seen = watch_progress(&mut driver);
        driver.poll_backend();
        let Ok(ClipMessage::Announce(announce)) = sent.try_recv() else {
            panic!("the copy is announced");
        };
        let generation = announce.generation;
        let start = Instant::now();
        // Chunks of something older, then the writer getting on with this copy.
        driver.on_data_sent((1, ME), ClipKind::Png, 5);
        driver.on_data_sent(generation, ClipKind::Png, 64_000);
        driver.on_data_sent(generation, ClipKind::Png, 128_000);
        driver.report_progress(start + Duration::from_secs(1));
        let moving = ClipProgress {
            direction: ClipDirection::Sending,
            stage: ClipStage::Moving,
            kind: ClipKind::Png,
            done: 128_000,
            size: 300_000,
        };
        assert_eq!(*seen.lock().unwrap(), [moving]);

        driver.on_data_sent(generation, ClipKind::Png, 300_000);
        driver.feed(ClipEvent::Received(ClipMessage::Applied(ClipApplied {
            generation,
            status: ClipStatus::Ok,
        })));
        driver.report_progress(start + Duration::from_secs(1));
        assert_eq!(
            seen.lock().unwrap().last(),
            Some(&ClipProgress {
                stage: ClipStage::Done,
                done: 300_000,
                ..moving
            })
        );
        assert!(driver.sending.is_none());
    }

    #[test]
    fn a_large_copy_the_peer_refused_is_reported_as_dropped() {
        let picture = picture(300_000);
        let (mut driver, mut sent, _data) = connected_driver(Counting {
            next: Some(LocalSnapshot {
                formats: vec![FormatInfo::of(ClipKind::Png, &picture)],
                data: vec![(ClipKind::Png, picture.clone())],
                owner_marker: false,
                sensitive: false,
            }),
            ..Counting::default()
        });
        let seen = watch_progress(&mut driver);
        driver.poll_backend();
        let Ok(ClipMessage::Announce(announce)) = sent.try_recv() else {
            panic!("the copy is announced");
        };
        let start = Instant::now();
        driver.on_data_sent(announce.generation, ClipKind::Png, 64_000);
        driver.report_progress(start + Duration::from_secs(1));
        driver.feed(ClipEvent::Received(ClipMessage::Applied(ClipApplied {
            generation: announce.generation,
            status: ClipStatus::Busy,
        })));
        driver.report_progress(start + Duration::from_secs(2));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(
            (seen[1].direction, seen[1].stage),
            (ClipDirection::Sending, ClipStage::Dropped)
        );
    }

    #[test]
    fn a_paste_query_does_not_wait_on_a_full_queue() {
        let (tx, _rx) = mpsc::sync_channel(1);
        tx.try_send(DriverMsg::StreamEnded).unwrap();
        let handle = ClipHandle { tx };
        let started = Instant::now();
        assert_eq!(
            handle.paste_requirement(Duration::from_millis(30), |_| {}),
            PasteNeed::Known(None)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// The clipboard thread busy (a large picture being read or written) used to mean
    /// "nothing to wait for", and the paste went ahead of the copy it was meant for.
    #[test]
    fn a_paste_query_waits_at_most_its_limit_and_is_answered_later() {
        let (tx, rx) = mpsc::sync_channel(4);
        let handle = ClipHandle { tx };
        let (late_tx, late_rx) = mpsc::channel();
        let started = Instant::now();
        let need = handle.paste_requirement(Duration::from_millis(30), move |required| {
            let _ = late_tx.send(required);
        });
        assert_eq!(need, PasteNeed::Later);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(late_rx.try_recv().is_err(), "not before there is an answer");

        // The clipboard thread gets to the query.
        let Ok(DriverMsg::PasteQuery(reply)) = rx.try_recv() else {
            panic!("the query is still queued");
        };
        let generation = (3, ME);
        reply.give(Some(generation));
        assert_eq!(late_rx.try_recv(), Ok(Some(generation)));
    }

    #[test]
    fn a_paste_query_answered_in_time_is_known_at_once() {
        let (tx, rx) = mpsc::sync_channel(4);
        let handle = ClipHandle { tx };
        let answering = thread::spawn(move || {
            if let Ok(DriverMsg::PasteQuery(reply)) = rx.recv() {
                reply.give(Some((9, ME)));
            }
        });
        let need = handle.paste_requirement(Duration::from_secs(5), |_| {
            panic!("answered in time: no late answer");
        });
        assert_eq!(need, PasteNeed::Known(Some((9, ME))));
        answering.join().unwrap();
    }
}
