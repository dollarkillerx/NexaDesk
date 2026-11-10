//! File transfer over stream type 4 (§3.1, DESIGN §9.4 "传输面板").
//!
//! Only the side that has a file can start a transfer: there is no message to list
//! or request the other side's files. One transfer is one bidirectional stream
//! opened by the sender:
//!
//! ```text
//! sender                                   receiver
//!   FileOffer { name, size }        ──→    checks name, size, permission, space;
//!                                          above the confirm threshold asks its UI
//!                                   ←──    FileAnswer { status }   (Ok = go ahead)
//!   FileChunk { offset, data } ×n   ──→    written to a hidden temporary file
//!   FileEnd { sha256 }              ──→    compared with the hash of what arrived;
//!                                          only then moved into place, never
//!                                          over an existing file
//!                                   ←──    FileResult { status }
//! ```
//!
//! ```protobuf
//! message FileOffer  { string name = 1; uint64 size = 2; }
//! message FileAnswer { FileStatus status = 1; }
//! message FileChunk  { uint64 offset = 1; bytes data = 2; }
//! message FileEnd    { bytes sha256 = 1; }
//! message FileResult { FileStatus status = 1; }
//! message FileMsg { oneof body { FileOffer offer = 1; FileAnswer answer = 2;
//!                   FileChunk chunk = 3; FileEnd end = 4; FileResult result = 5; } }
//! ```
//!
//! Every message from the network is checked: one offer and only first, a name
//! without path components, a size within the limit, chunks in order with no gap,
//! overlap or excess, an end exactly at the offered size, a 32-byte hash. Anything
//! else abandons the transfer and deletes what was written. Cancelling is a stream
//! reset (sender) or a `FileResult{Cancelled}` and stop (receiver), code
//! [`code::CANCELLED`].
//!
//! The bytes are paced by the session's [`BulkLimiter`] (§4.9), the lowest priority
//! of all traffic, on a stream with quinn priority -10.

use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};

use bytes::Bytes;
use nexa_wire::framing::StreamType;
use prost::{Enumeration, Message, Oneof};
use quinn::Connection;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Semaphore, oneshot, watch},
};

use crate::{
    SessionError,
    bulk::BulkLimiter,
    framed::{FramedRecv, FramedSend, open_bi},
};

/// Bytes per `FileChunk`. Small, because quinn sends a write at line rate (§4.8):
/// a 64 KiB write on a slow allocation is a burst the delay controller reads as
/// congestion, and the allocation then never grows.
pub const CHUNK: usize = 16 * 1024;
/// How long the first message may take.
const OFFER_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the person at the receiver has to decide on a large file.
pub const CONFIRM_TIMEOUT: Duration = Duration::from_secs(60);
/// No progress for this long ends a transfer.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const PROGRESS_EVERY: Duration = Duration::from_millis(100);
const MAX_INCOMING: usize = 4;
const MAX_OUTGOING: usize = 4;
/// Kept free on the receiving disk beyond the file itself.
const SPACE_MARGIN: u64 = 64 << 20;
const MAX_NAME_BYTES: usize = 240;
const MAX_OFFERED_NAME: usize = 1024;
const MAX_SUFFIX: u32 = 999;
/// Temporary files end in this, and no received name may.
const PART_SUFFIX: &str = ".nexadesk-part";

/// Stream reset and stop codes.
pub mod code {
    pub const CANCELLED: u32 = 1;
    /// The session has no `file` permission, or the receiver is busy.
    pub const REFUSED: u32 = 2;
    pub const INVALID: u32 = 3;
}

// ---------------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Enumeration)]
#[repr(i32)]
pub enum FileStatus {
    Unspecified = 0,
    Ok = 1,
    Declined = 2,
    TooLarge = 3,
    NotPermitted = 4,
    Invalid = 5,
    HashMismatch = 6,
    Cancelled = 7,
    NoSpace = 8,
    WriteFailed = 9,
    Busy = 10,
}

#[derive(Clone, PartialEq, Message)]
pub struct FileOffer {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(uint64, tag = "2")]
    pub size: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct FileAnswer {
    #[prost(enumeration = "FileStatus", tag = "1")]
    pub status: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct FileChunk {
    #[prost(uint64, tag = "1")]
    pub offset: u64,
    #[prost(bytes = "bytes", tag = "2")]
    pub data: Bytes,
}

#[derive(Clone, PartialEq, Message)]
pub struct FileEnd {
    #[prost(bytes = "vec", tag = "1")]
    pub sha256: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct FileResult {
    #[prost(enumeration = "FileStatus", tag = "1")]
    pub status: i32,
}

#[derive(Clone, PartialEq, Oneof)]
pub enum FileBody {
    #[prost(message, tag = "1")]
    Offer(FileOffer),
    #[prost(message, tag = "2")]
    Answer(FileAnswer),
    #[prost(message, tag = "3")]
    Chunk(FileChunk),
    #[prost(message, tag = "4")]
    End(FileEnd),
    #[prost(message, tag = "5")]
    Result(FileResult),
}

#[derive(Clone, PartialEq, Message)]
pub struct FileMsg {
    #[prost(oneof = "FileBody", tags = "1, 2, 3, 4, 5")]
    pub body: Option<FileBody>,
}

impl From<FileBody> for FileMsg {
    fn from(body: FileBody) -> Self {
        Self { body: Some(body) }
    }
}

fn status_of(raw: i32) -> FileStatus {
    FileStatus::try_from(raw).unwrap_or(FileStatus::Unspecified)
}

// ---------------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Outgoing,
    Incoming,
}

/// Why a transfer did not complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailReason {
    /// The receiver (or its person) said no.
    Declined,
    TooLarge,
    /// The session has no `file` permission.
    NotPermitted,
    /// The peer sent something inconsistent.
    Invalid,
    HashMismatch,
    NoSpace,
    /// Reading or writing the file failed here.
    Io,
    ConnectionLost,
    Timeout,
    /// Folders and other non-files cannot be sent.
    NotAFile,
    Busy,
}

impl FailReason {
    fn of_status(s: FileStatus) -> Self {
        match s {
            FileStatus::Declined => Self::Declined,
            FileStatus::TooLarge => Self::TooLarge,
            FileStatus::NotPermitted => Self::NotPermitted,
            FileStatus::HashMismatch => Self::HashMismatch,
            FileStatus::NoSpace => Self::NoSpace,
            FileStatus::WriteFailed => Self::Io,
            FileStatus::Busy => Self::Busy,
            _ => Self::Invalid,
        }
    }

    fn status(self) -> FileStatus {
        match self {
            Self::Declined => FileStatus::Declined,
            Self::TooLarge => FileStatus::TooLarge,
            Self::NotPermitted => FileStatus::NotPermitted,
            Self::Invalid | Self::NotAFile => FileStatus::Invalid,
            Self::HashMismatch => FileStatus::HashMismatch,
            Self::NoSpace => FileStatus::NoSpace,
            Self::Io => FileStatus::WriteFailed,
            Self::Busy => FileStatus::Busy,
            Self::ConnectionLost | Self::Timeout => FileStatus::Cancelled,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransferState {
    /// Outgoing: waiting for a slot or for the receiver's answer.
    Waiting,
    /// Incoming: the person here must accept or decline (`Transfers::answer`).
    NeedsConfirm,
    Running,
    /// Incoming transfers name where the file went.
    Done {
        path: Option<PathBuf>,
    },
    Failed(FailReason),
    Cancelled {
        by_peer: bool,
    },
}

impl TransferState {
    pub fn is_final(&self) -> bool {
        matches!(
            self,
            Self::Done { .. } | Self::Failed(_) | Self::Cancelled { .. }
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferEvent {
    /// Local to this side; the peer numbers its own.
    pub id: u64,
    pub direction: Direction,
    /// For incoming transfers, the sanitized name.
    pub name: String,
    pub size: u64,
    pub done: u64,
    pub state: TransferState,
}

pub type TransferEvents = Arc<dyn Fn(TransferEvent) + Send + Sync>;

// ---------------------------------------------------------------------------------
// Names and placing files
// ---------------------------------------------------------------------------------

fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{061C}' | '\u{180E}' | '\u{200B}'..='\u{200F}'
        | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206F}'
        | '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}')
}

/// The name a received file gets, or `None` if the offered name is unacceptable.
///
/// Path components are refused outright (a sender only ever offers a file name).
/// The rest is cleaned: control, bidi and zero-width characters go, `:` (shown as
/// `/` by the Finder) becomes `_`, leading dots go so nothing arrives hidden,
/// trailing dots and spaces go, and the result is shortened to 240 bytes keeping
/// the extension.
pub fn sanitize_file_name(offered: &str) -> Option<String> {
    if offered.len() > MAX_OFFERED_NAME || offered.contains(['/', '\\', '\0']) {
        return None;
    }
    let cleaned: String = offered
        .chars()
        .filter(|c| !c.is_control() && !is_invisible(*c))
        .map(|c| if c == ':' { '_' } else { c })
        .collect();
    let mut name = cleaned
        .trim()
        .trim_start_matches('.')
        .trim_end_matches(['.', ' '])
        .trim()
        .to_owned();
    if name.is_empty() {
        return None;
    }
    if name.to_lowercase().ends_with(PART_SUFFIX) {
        name.push('_');
    }
    if name.len() > MAX_NAME_BYTES {
        let (stem, ext) = split_ext(&name);
        let ext = if ext.len() <= 16 { ext } else { "" };
        let mut stem: String = stem.to_owned();
        while stem.len() + ext.len() > MAX_NAME_BYTES {
            stem.pop();
        }
        name = format!("{}{ext}", stem.trim_end());
    }
    Some(name)
}

/// `("report", ".pdf")`; no extension for names like `README`.
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    }
}

fn candidate(name: &str, n: u32) -> String {
    if n == 0 {
        return name.to_owned();
    }
    let (stem, ext) = split_ext(name);
    format!("{stem} ({n}){ext}")
}

/// Moves `temp` to `dir/name`, or `name (1)` and so on: never over an existing file.
/// A hard link fails if the name exists, so no check-then-create race.
fn place(temp: &Path, dir: &Path, name: &str) -> io::Result<PathBuf> {
    for n in 0..=MAX_SUFFIX {
        let target = dir.join(candidate(name, n));
        match std::fs::hard_link(temp, &target) {
            Ok(()) => {
                let _ = std::fs::remove_file(temp);
                return Ok(target);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "too many files with this name",
    ))
}

fn random_hex() -> String {
    use rand_core09::TryRngCore;
    let mut b = [0u8; 8];
    if rand_core09::OsRng.try_fill_bytes(&mut b).is_err() {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        b = (t ^ u64::from(std::process::id())).to_le_bytes();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Deletes the temporary file unless the transfer completed.
struct TempFile {
    path: PathBuf,
    keep: bool,
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

async fn create_temp(dir: &Path) -> io::Result<(tokio::fs::File, TempFile)> {
    tokio::fs::create_dir_all(dir).await?;
    for _ in 0..8 {
        let path = dir.join(format!(".{}{PART_SUFFIX}", random_hex()));
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&path).await {
            Ok(f) => return Ok((f, TempFile { path, keep: false })),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("could not create a temporary file"))
}

/// Free bytes for an unprivileged writer at `dir` (or its nearest existing parent).
fn free_space(dir: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut probe = dir;
        while !probe.exists() {
            probe = probe.parent()?;
        }
        let c = std::ffi::CString::new(probe.as_os_str().as_bytes()).ok()?;
        // SAFETY: statvfs fills the struct; `c` is a valid C string.
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
            return None;
        }
        #[allow(clippy::unnecessary_cast)]
        Some((s.f_bavail as u64).saturating_mul(s.f_frsize as u64))
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        None
    }
}

/// Marks a received file as downloaded, so Gatekeeper checks it before it runs.
fn quarantine(path: &Path) {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            return;
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let value = format!("0081;{now:08x};NexaDesk;");
        // SAFETY: valid C strings and a buffer of the stated length.
        unsafe {
            libc::setxattr(
                c.as_ptr(),
                c"com.apple.quarantine".as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
                0,
            );
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = path;
}

// ---------------------------------------------------------------------------------
// The per-session manager
// ---------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct TransferLimits {
    /// Larger offers are refused.
    pub max_size: u64,
    /// Larger offers wait for the person here to accept.
    pub confirm_above: u64,
}

impl Default for TransferLimits {
    fn default() -> Self {
        Self {
            max_size: 8 << 30,
            confirm_above: 256 << 20,
        }
    }
}

pub struct TransferConfig {
    /// Where received files go (`~/Downloads/NexaDesk` outside tests).
    pub downloads: PathBuf,
    pub limits: TransferLimits,
    pub bulk: Arc<BulkLimiter>,
    pub confirm_timeout: Duration,
}

struct Active {
    cancel: watch::Sender<bool>,
    confirm: Option<oneshot::Sender<bool>>,
}

struct Inner {
    conn: Connection,
    config: TransferConfig,
    events: TransferEvents,
    active: Mutex<HashMap<u64, Active>>,
    incoming: AtomicUsize,
    outgoing: Arc<Semaphore>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Every transfer of one session. Only created for sessions granted `file`; a
/// session without it has none, and refuses File streams with [`refuse`].
#[derive(Clone)]
pub struct Transfers {
    inner: Arc<Inner>,
}

/// Turns away a File stream without reading it: nothing is written.
pub fn refuse(mut send: FramedSend, mut recv: FramedRecv) {
    recv.stop_with(code::REFUSED);
    send.reset(code::REFUSED);
}

struct Tracker {
    inner: Arc<Inner>,
    event: TransferEvent,
    last_progress: Instant,
}

impl Tracker {
    fn emit(&mut self, state: TransferState) {
        self.event.state = state;
        (self.inner.events)(self.event.clone());
    }

    fn progress(&mut self, done: u64) {
        self.event.done = done;
        let now = Instant::now();
        if now.saturating_duration_since(self.last_progress) >= PROGRESS_EVERY
            || done == self.event.size
        {
            self.last_progress = now;
            self.emit(TransferState::Running);
        }
    }

    fn finish(mut self, state: TransferState) {
        lock(&self.inner.active).remove(&self.event.id);
        self.emit(state);
    }
}

enum Stop {
    Failed(FailReason),
    Cancelled { by_peer: bool },
}

impl Stop {
    fn state(self) -> TransferState {
        match self {
            Self::Failed(r) => TransferState::Failed(r),
            Self::Cancelled { by_peer } => TransferState::Cancelled { by_peer },
        }
    }
}

impl Transfers {
    pub fn new(conn: Connection, config: TransferConfig, events: TransferEvents) -> Self {
        Self {
            inner: Arc::new(Inner {
                conn,
                config,
                events,
                active: Mutex::new(HashMap::new()),
                incoming: AtomicUsize::new(0),
                outgoing: Arc::new(Semaphore::new(MAX_OUTGOING)),
            }),
        }
    }

    fn register(
        &self,
        direction: Direction,
        name: String,
        size: u64,
    ) -> (Tracker, watch::Receiver<bool>) {
        // Unique in the process, so a reconnected session's transfers never reuse
        // an id the UI still shows.
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, Relaxed);
        let (cancel, cancelled) = watch::channel(false);
        lock(&self.inner.active).insert(
            id,
            Active {
                cancel,
                confirm: None,
            },
        );
        let tracker = Tracker {
            inner: self.inner.clone(),
            event: TransferEvent {
                id,
                direction,
                name,
                size,
                done: 0,
                state: TransferState::Waiting,
            },
            last_progress: Instant::now(),
        };
        (tracker, cancelled)
    }

    /// Sends the file at `path`. Returns its transfer id at once; progress and the
    /// outcome arrive as events. Must run inside a tokio runtime.
    pub fn send_file(&self, path: PathBuf) -> u64 {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (mut tracker, cancelled) = self.register(Direction::Outgoing, name, 0);
        let id = tracker.event.id;
        tracker.emit(TransferState::Waiting);
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let Ok(_slot) = inner.outgoing.clone().acquire_owned().await else {
                return;
            };
            let state = match send(&inner, &path, &mut tracker, cancelled).await {
                Ok(()) => TransferState::Done { path: None },
                Err(stop) => stop.state(),
            };
            tracker.finish(state);
        });
        id
    }

    /// Takes over a File stream the peer opened. The caller has checked that the
    /// session may transfer files. Must run inside a tokio runtime.
    pub fn accept(&self, mut send: FramedSend, mut recv: FramedRecv) {
        if self.inner.incoming.fetch_add(1, Relaxed) >= MAX_INCOMING {
            self.inner.incoming.fetch_sub(1, Relaxed);
            tokio::spawn(async move {
                turn_away(&mut send, &mut recv, FileStatus::Busy).await;
            });
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            receive(&me, send, recv).await;
            me.inner.incoming.fetch_sub(1, Relaxed);
        });
    }

    /// Stops a transfer in either direction. False if there is no such transfer.
    pub fn cancel(&self, id: u64) -> bool {
        match lock(&self.inner.active).get(&id) {
            Some(a) => {
                a.cancel.send_replace(true);
                true
            }
            None => false,
        }
    }

    /// The person's answer to [`TransferState::NeedsConfirm`]. False if nothing is
    /// waiting for one under this id.
    pub fn answer(&self, id: u64, accept: bool) -> bool {
        let reply = lock(&self.inner.active)
            .get_mut(&id)
            .and_then(|a| a.confirm.take());
        match reply {
            Some(r) => r.send(accept).is_ok(),
            None => false,
        }
    }

    /// Cancels everything, for the session's end.
    pub fn shutdown(&self) {
        for a in lock(&self.inner.active).values() {
            a.cancel.send_replace(true);
        }
    }

    /// Transfers not yet finished.
    pub fn running(&self) -> usize {
        lock(&self.inner.active).len()
    }
}

fn lost_or(conn: &Connection, otherwise: Stop) -> Stop {
    if conn.close_reason().is_some() {
        Stop::Failed(FailReason::ConnectionLost)
    } else {
        otherwise
    }
}

async fn next_msg(recv: &mut FramedRecv) -> Result<Option<FileBody>, SessionError> {
    loop {
        match recv.next::<FileMsg>().await? {
            Some(FileMsg { body: Some(b) }) => return Ok(Some(b)),
            Some(FileMsg { body: None }) => continue,
            None => return Ok(None),
        }
    }
}

async fn send_msg(send: &mut FramedSend, body: FileBody) -> Result<(), SessionError> {
    send.send(&FileMsg::from(body)).await
}

async fn cancelled(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|c| *c).await;
}

// ---------------------------------------------------------------------------------
// Sending
// ---------------------------------------------------------------------------------

async fn send(
    inner: &Arc<Inner>,
    path: &Path,
    tracker: &mut Tracker,
    mut cancel: watch::Receiver<bool>,
) -> Result<(), Stop> {
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|_| Stop::Failed(FailReason::Io))?;
    if !meta.is_file() || tracker.event.name.is_empty() {
        return Err(Stop::Failed(FailReason::NotAFile));
    }
    let size = meta.len();
    tracker.event.size = size;
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|_| Stop::Failed(FailReason::Io))?;
    if *cancel.borrow() {
        return Err(Stop::Cancelled { by_peer: false });
    }

    let (mut tx, mut rx) = open_bi(&inner.conn, StreamType::File)
        .await
        .map_err(|_| Stop::Failed(FailReason::ConnectionLost))?;
    let abandon = |tx: &mut FramedSend, rx: &mut FramedRecv| {
        tx.reset(code::CANCELLED);
        rx.stop_with(code::CANCELLED);
    };
    let offer = FileBody::Offer(FileOffer {
        name: tracker.event.name.clone(),
        size,
    });
    if send_msg(&mut tx, offer).await.is_err() {
        return Err(lost_or(&inner.conn, Stop::Failed(FailReason::NotPermitted)));
    }
    tracker.emit(TransferState::Waiting);

    let answer_wait = inner.config.confirm_timeout + OFFER_TIMEOUT;
    let answer = tokio::select! {
        a = tokio::time::timeout(answer_wait, next_msg(&mut rx)) => a,
        _ = cancelled(&mut cancel) => {
            abandon(&mut tx, &mut rx);
            return Err(Stop::Cancelled { by_peer: false });
        }
    };
    match answer {
        Ok(Ok(Some(FileBody::Answer(a)))) if status_of(a.status) == FileStatus::Ok => {}
        Ok(Ok(Some(FileBody::Answer(a)))) => {
            return Err(Stop::Failed(FailReason::of_status(status_of(a.status))));
        }
        Ok(Ok(Some(_))) => {
            abandon(&mut tx, &mut rx);
            return Err(Stop::Failed(FailReason::Invalid));
        }
        // Refused without reading (no permission, busy), or the connection went.
        Ok(Ok(None)) | Ok(Err(_)) => {
            return Err(lost_or(&inner.conn, Stop::Failed(FailReason::NotPermitted)));
        }
        Err(_) => {
            abandon(&mut tx, &mut rx);
            return Err(Stop::Failed(FailReason::Timeout));
        }
    }
    tracker.emit(TransferState::Running);

    let mut hasher = Sha256::new();
    let mut offset = 0u64;
    let mut buf = vec![0u8; CHUNK];
    while offset < size {
        let want = (size - offset).min(CHUNK as u64) as usize;
        let n = match file.read(&mut buf[..want]).await {
            Ok(0) | Err(_) => {
                // The file shrank or became unreadable while it was being sent.
                abandon(&mut tx, &mut rx);
                return Err(Stop::Failed(FailReason::Io));
            }
            Ok(n) => n,
        };
        tokio::select! {
            _ = inner.config.bulk.acquire(n) => {}
            _ = cancelled(&mut cancel) => {
                abandon(&mut tx, &mut rx);
                return Err(Stop::Cancelled { by_peer: false });
            }
        }
        hasher.update(&buf[..n]);
        let chunk = FileBody::Chunk(FileChunk {
            offset,
            data: Bytes::copy_from_slice(&buf[..n]),
        });
        let wrote = tokio::select! {
            w = send_msg(&mut tx, chunk) => w,
            m = next_msg(&mut rx) => {
                // The receiver only speaks mid-transfer to stop it.
                abandon(&mut tx, &mut rx);
                return Err(match m {
                    Ok(Some(FileBody::Result(r))) if status_of(r.status) == FileStatus::Cancelled => {
                        Stop::Cancelled { by_peer: true }
                    }
                    Ok(Some(FileBody::Result(r))) => Stop::Failed(FailReason::of_status(status_of(r.status))),
                    _ => lost_or(&inner.conn, Stop::Cancelled { by_peer: true }),
                });
            }
            _ = cancelled(&mut cancel) => {
                abandon(&mut tx, &mut rx);
                return Err(Stop::Cancelled { by_peer: false });
            }
        };
        if wrote.is_err() {
            return Err(lost_or(&inner.conn, Stop::Cancelled { by_peer: true }));
        }
        offset += n as u64;
        tracker.progress(offset);
    }

    let end = FileBody::End(FileEnd {
        sha256: hasher.finalize().to_vec(),
    });
    if send_msg(&mut tx, end).await.is_err() {
        return Err(lost_or(&inner.conn, Stop::Cancelled { by_peer: true }));
    }
    tx.finish();
    let result = tokio::select! {
        r = tokio::time::timeout(IDLE_TIMEOUT, next_msg(&mut rx)) => r,
        _ = cancelled(&mut cancel) => {
            abandon(&mut tx, &mut rx);
            return Err(Stop::Cancelled { by_peer: false });
        }
    };
    match result {
        Ok(Ok(Some(FileBody::Result(r)))) => match status_of(r.status) {
            FileStatus::Ok => Ok(()),
            FileStatus::Cancelled => Err(Stop::Cancelled { by_peer: true }),
            other => Err(Stop::Failed(FailReason::of_status(other))),
        },
        Ok(Ok(_)) => Err(Stop::Failed(FailReason::Invalid)),
        Ok(Err(_)) => Err(lost_or(&inner.conn, Stop::Cancelled { by_peer: true })),
        Err(_) => Err(Stop::Failed(FailReason::Timeout)),
    }
}

// ---------------------------------------------------------------------------------
// Receiving
// ---------------------------------------------------------------------------------

/// Ends an incoming transfer that will not go ahead: answers with a status and
/// closes both halves.
async fn turn_away(send: &mut FramedSend, recv: &mut FramedRecv, status: FileStatus) {
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        send_msg(
            send,
            FileBody::Answer(FileAnswer {
                status: status as i32,
            }),
        ),
    )
    .await;
    send.finish();
    recv.stop_with(code::REFUSED);
}

async fn receive(me: &Transfers, mut send: FramedSend, mut recv: FramedRecv) {
    let inner = &me.inner;
    let offer = match tokio::time::timeout(OFFER_TIMEOUT, next_msg(&mut recv)).await {
        Ok(Ok(Some(FileBody::Offer(o)))) => o,
        Ok(Ok(Some(_))) => {
            turn_away(&mut send, &mut recv, FileStatus::Invalid).await;
            return;
        }
        _ => {
            recv.stop_with(code::INVALID);
            send.reset(code::INVALID);
            return;
        }
    };
    // Nothing is shown or written for an offer that is wrong on its face.
    let Some(name) = sanitize_file_name(&offer.name) else {
        turn_away(&mut send, &mut recv, FileStatus::Invalid).await;
        return;
    };
    let size = offer.size;
    let (mut tracker, mut cancel) = me.register(Direction::Incoming, name.clone(), size);
    let limits = inner.config.limits;
    let refusal = if size > limits.max_size {
        Some(FailReason::TooLarge)
    } else if free_space(&inner.config.downloads)
        .is_some_and(|free| free < size.saturating_add(SPACE_MARGIN))
    {
        Some(FailReason::NoSpace)
    } else {
        None
    };
    if let Some(reason) = refusal {
        turn_away(&mut send, &mut recv, reason.status()).await;
        tracker.finish(TransferState::Failed(reason));
        return;
    }

    if size > limits.confirm_above {
        let (reply, answer) = oneshot::channel();
        if let Some(a) = lock(&inner.active).get_mut(&tracker.event.id) {
            a.confirm = Some(reply);
        }
        tracker.emit(TransferState::NeedsConfirm);
        let accepted = tokio::select! {
            a = tokio::time::timeout(inner.config.confirm_timeout, answer) => matches!(a, Ok(Ok(true))),
            _ = cancelled(&mut cancel) => false,
            // The sender gave up while the person was deciding.
            _ = next_msg(&mut recv) => {
                send.reset(code::CANCELLED);
                tracker.finish(TransferState::Cancelled { by_peer: true });
                return;
            }
        };
        if !accepted {
            turn_away(&mut send, &mut recv, FileStatus::Declined).await;
            tracker.finish(TransferState::Failed(FailReason::Declined));
            return;
        }
    }

    let state = match receive_body(
        inner,
        &name,
        size,
        &mut send,
        &mut recv,
        &mut tracker,
        cancel,
    )
    .await
    {
        Ok(path) => TransferState::Done { path: Some(path) },
        Err(stop) => stop.state(),
    };
    tracker.finish(state);
}

async fn receive_body(
    inner: &Arc<Inner>,
    name: &str,
    size: u64,
    send: &mut FramedSend,
    recv: &mut FramedRecv,
    tracker: &mut Tracker,
    mut cancel: watch::Receiver<bool>,
) -> Result<PathBuf, Stop> {
    let dir = inner.config.downloads.clone();
    let (mut file, mut temp) = match create_temp(&dir).await {
        Ok(t) => t,
        Err(_) => {
            turn_away(send, recv, FileStatus::WriteFailed).await;
            return Err(Stop::Failed(FailReason::Io));
        }
    };
    let ok = FileBody::Answer(FileAnswer {
        status: FileStatus::Ok as i32,
    });
    if send_msg(send, ok).await.is_err() {
        return Err(lost_or(&inner.conn, Stop::Cancelled { by_peer: true }));
    }
    tracker.emit(TransferState::Running);

    // Says why to the sender, then stops reading.
    async fn fail(send: &mut FramedSend, recv: &mut FramedRecv, status: FileStatus) {
        let result = FileBody::Result(FileResult {
            status: status as i32,
        });
        let _ = tokio::time::timeout(Duration::from_secs(2), send_msg(send, result)).await;
        send.finish();
        recv.stop_with(if status == FileStatus::Cancelled {
            code::CANCELLED
        } else {
            code::INVALID
        });
    }

    let mut hasher = Sha256::new();
    let mut received = 0u64;
    let digest = loop {
        let msg = tokio::select! {
            m = tokio::time::timeout(IDLE_TIMEOUT, next_msg(recv)) => m,
            _ = cancelled(&mut cancel) => {
                fail(send, recv, FileStatus::Cancelled).await;
                return Err(Stop::Cancelled { by_peer: false });
            }
        };
        match msg {
            Ok(Ok(Some(FileBody::Chunk(c)))) => {
                let len = c.data.len() as u64;
                // In order, no gap or overlap, not empty, never past the offer.
                if c.offset != received || len == 0 || received + len > size {
                    fail(send, recv, FileStatus::Invalid).await;
                    return Err(Stop::Failed(FailReason::Invalid));
                }
                if file.write_all(&c.data).await.is_err() {
                    fail(send, recv, FileStatus::WriteFailed).await;
                    return Err(Stop::Failed(FailReason::Io));
                }
                hasher.update(&c.data);
                received += len;
                tracker.progress(received);
            }
            Ok(Ok(Some(FileBody::End(e)))) => {
                if received != size || e.sha256.len() != 32 {
                    fail(send, recv, FileStatus::Invalid).await;
                    return Err(Stop::Failed(FailReason::Invalid));
                }
                break e.sha256;
            }
            Ok(Ok(Some(_))) => {
                fail(send, recv, FileStatus::Invalid).await;
                return Err(Stop::Failed(FailReason::Invalid));
            }
            // The stream ended early or was reset: the sender gave up or went away.
            Ok(Ok(None)) | Ok(Err(_)) => {
                send.reset(code::CANCELLED);
                return Err(lost_or(&inner.conn, Stop::Cancelled { by_peer: true }));
            }
            Err(_) => {
                fail(send, recv, FileStatus::Cancelled).await;
                return Err(Stop::Failed(FailReason::Timeout));
            }
        }
    };

    if hasher.finalize().as_slice() != digest.as_slice() {
        fail(send, recv, FileStatus::HashMismatch).await;
        return Err(Stop::Failed(FailReason::HashMismatch));
    }
    let placed = async {
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        let (temp_path, dir, name) = (temp.path.clone(), dir.clone(), name.to_owned());
        tokio::task::spawn_blocking(move || {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&temp_path, std::fs::Permissions::from_mode(0o644))?;
            }
            quarantine(&temp_path);
            place(&temp_path, &dir, &name)
        })
        .await
        .map_err(io::Error::other)?
    }
    .await;
    match placed {
        Ok(path) => {
            temp.keep = true;
            let ok = FileBody::Result(FileResult {
                status: FileStatus::Ok as i32,
            });
            let _ = tokio::time::timeout(Duration::from_secs(2), send_msg(send, ok)).await;
            send.finish();
            Ok(path)
        }
        Err(_) => {
            fail(send, recv, FileStatus::WriteFailed).await;
            Err(Stop::Failed(FailReason::Io))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_lose_paths_hidden_dots_and_invisibles() {
        assert_eq!(sanitize_file_name("../../etc/passwd"), None);
        assert_eq!(sanitize_file_name("a\\b.txt"), None);
        assert_eq!(sanitize_file_name("nul\0.txt"), None);
        assert_eq!(sanitize_file_name(".."), None);
        assert_eq!(sanitize_file_name("."), None);
        assert_eq!(sanitize_file_name("   "), None);
        assert_eq!(sanitize_file_name(".bashrc").as_deref(), Some("bashrc"));
        assert_eq!(sanitize_file_name("...x").as_deref(), Some("x"));
        assert_eq!(
            sanitize_file_name("re\u{202E}fdp.exe").as_deref(),
            Some("refdp.exe")
        );
        assert_eq!(
            sanitize_file_name("a\x1b[2Jb.txt").as_deref(),
            Some("a[2Jb.txt")
        );
        assert_eq!(
            sanitize_file_name("time 10:30.txt").as_deref(),
            Some("time 10_30.txt")
        );
        assert_eq!(
            sanitize_file_name("trailing. . ").as_deref(),
            Some("trailing")
        );
        assert_eq!(
            sanitize_file_name("x.nexadesk-part").as_deref(),
            Some("x.nexadesk-part_")
        );
        assert_eq!(
            sanitize_file_name("报告 2026.pdf").as_deref(),
            Some("报告 2026.pdf")
        );
        let long = format!("{}.pdf", "长".repeat(200));
        let short = sanitize_file_name(&long).unwrap();
        assert!(
            short.len() <= MAX_NAME_BYTES && short.ends_with(".pdf"),
            "{short}"
        );
        assert_eq!(sanitize_file_name(&"a".repeat(MAX_OFFERED_NAME + 1)), None);
    }

    #[test]
    fn collisions_get_a_suffix_and_never_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"old").unwrap();
        std::fs::write(dir.path().join("a (1).txt"), b"old1").unwrap();
        let temp = dir.path().join(".t.nexadesk-part");
        std::fs::write(&temp, b"new").unwrap();
        let placed = place(&temp, dir.path(), "a.txt").unwrap();
        assert_eq!(placed, dir.path().join("a (2).txt"));
        assert_eq!(std::fs::read(dir.path().join("a.txt")).unwrap(), b"old");
        assert_eq!(std::fs::read(&placed).unwrap(), b"new");
        assert!(!temp.exists());
        assert_eq!(candidate("README", 3), "README (3)");
        assert_eq!(candidate("archive.tar.gz", 1), "archive.tar (1).gz");
    }

    #[test]
    fn messages_round_trip_and_garbage_is_an_error() {
        let m = FileMsg::from(FileBody::Chunk(FileChunk {
            offset: 7,
            data: Bytes::from_static(b"abc"),
        }));
        assert_eq!(FileMsg::decode(m.encode_to_vec().as_slice()).unwrap(), m);
        assert!(FileMsg::decode(&[0xFFu8; 9][..]).is_err());
    }
}
