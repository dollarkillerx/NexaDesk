//! The clipboard synchronisation state machine (design §8.1, §8.2).

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use bytes::Bytes;

use crate::backend::ClipError;
use crate::time::ClipInstant;
use crate::types::{ClipKind, ContentHash, DeviceId, FormatInfo, Gen};
use crate::wire::{ClipAnnounce, ClipApplied, ClipData, ClipFetch, ClipMessage, ClipStatus};

const MAX_QUEUED_FETCHES: usize = 64;

/// What this side may send and receive (§9.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipPolicy {
    /// Whether local clipboard changes may reach the peer.
    pub outbound: bool,
    /// Whether the peer's clipboard may be applied here.
    pub inbound: bool,
    /// Whether content the OS marks as sensitive may be sent. Off by default.
    pub sync_sensitive: bool,
}

impl Default for ClipPolicy {
    fn default() -> Self {
        Self {
            outbound: true,
            inbound: true,
            sync_sensitive: false,
        }
    }
}

/// Tunables. The defaults are the values from the design.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipConfig {
    /// Total budget for content sent inside one announcement (§8.4).
    pub inline_threshold: u64,
    /// Largest accepted format (§8.4).
    pub max_format_size: u64,
    /// A fetch fails after this long without progress (§8.2).
    pub fetch_timeout: Duration,
    /// Largest [`ClipData`] payload.
    pub chunk_size: u64,
    /// Bytes requested per [`ClipFetch`]; the receiver asks for the next window
    /// when one completes, which keeps a large transfer from queueing up.
    pub fetch_window: u64,
    /// Capacity of the echo-suppression LRU.
    pub hash_lru_capacity: usize,
    /// How long after a write its hash still counts as an echo.
    pub echo_ttl: Duration,
    /// Starting Lamport value. A process that restarts must not start below
    /// values the peer has already seen: seed this from persisted state or the
    /// wall clock.
    pub initial_lamport: u64,
    /// Direction switches and privacy.
    pub policy: ClipPolicy,
}

impl Default for ClipConfig {
    fn default() -> Self {
        Self {
            inline_threshold: 64 * 1024,
            max_format_size: 32 * 1024 * 1024,
            fetch_timeout: Duration::from_secs(10),
            chunk_size: 64 * 1024,
            fetch_window: 1024 * 1024,
            hash_lru_capacity: 64,
            echo_ttl: Duration::from_secs(5),
            initial_lamport: 0,
            policy: ClipPolicy::default(),
        }
    }
}

/// The local clipboard as observed after a change.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalSnapshot {
    /// Formats on the clipboard, described in canonical form. A format that is
    /// still a promise must be listed without rendering it.
    pub formats: Vec<FormatInfo>,
    /// Canonical content the observer already holds. Text formats that should
    /// travel inline must be present; anything absent is served on demand.
    pub data: Vec<(ClipKind, Bytes)>,
    /// Whether the clipboard carries our owner marker.
    pub owner_marker: bool,
    /// Whether the OS marks the content as sensitive.
    pub sensitive: bool,
}

/// Inputs to [`ClipSync::handle`].
#[derive(Clone, Debug)]
pub enum ClipEvent {
    /// The OS clipboard changed.
    LocalChanged(LocalSnapshot),
    /// A message arrived from the peer.
    Received(ClipMessage),
    /// The OS asked for a promised format (`WM_RENDERFORMAT` and friends).
    FetchRequested {
        /// The promise's generation.
        generation: Gen,
        /// The format.
        kind: ClipKind,
    },
    /// Result of [`ClipCommand::WritePromise`].
    WriteDone {
        /// The generation that was written.
        generation: Gen,
        /// The outcome.
        result: Result<(), ClipError>,
    },
    /// Result of [`ClipCommand::ReadLocal`].
    LocalReadDone {
        /// The generation the read was for.
        generation: Gen,
        /// The format.
        kind: ClipKind,
        /// The content, in canonical form.
        result: Result<Bytes, ClipError>,
    },
    /// The Clipboard stream came up.
    Connected,
    /// The Clipboard stream went down.
    Disconnected,
    /// The user changed a switch.
    PolicyChanged(ClipPolicy),
    /// Time passed; deliver this around [`ClipSync::next_deadline`].
    Tick,
}

/// Why a promised format could not be delivered. The driver writes empty data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FetchFailure {
    /// The generation is no longer current.
    Stale,
    /// A direction switch refused it.
    Denied,
    /// No progress within the timeout.
    Timeout,
    /// The received data failed validation.
    Corrupt,
    /// The peer could not read its clipboard.
    Busy,
}

/// Status for the toolbar (§9.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipNotice {
    /// Sensitive content was kept local.
    SensitiveKeptLocal,
    /// A format exceeded the limit; suggest file transfer.
    TooLarge {
        /// The format.
        kind: ClipKind,
        /// Its size.
        size: u64,
    },
    /// The peer could not take our clipboard.
    PeerRejected {
        /// The generation.
        generation: Gen,
        /// The reason.
        status: ClipStatus,
    },
    /// Writing the peer's clipboard here failed.
    WriteFailed {
        /// The generation.
        generation: Gen,
    },
    /// A promised format could not be delivered.
    FetchFailed {
        /// The generation.
        generation: Gen,
        /// The format.
        kind: ClipKind,
        /// The reason.
        reason: FetchFailure,
    },
}

/// Outputs of [`ClipSync::handle`], to be carried out in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipCommand {
    /// Send on the Clipboard stream.
    Send(ClipMessage),
    /// Write to the OS clipboard, then report [`ClipEvent::WriteDone`].
    WritePromise {
        /// The generation, for the owner marker.
        generation: Gen,
        /// All formats to offer.
        formats: Vec<FormatInfo>,
        /// Content available now; the other formats become promises.
        inline: Vec<(ClipKind, Bytes)>,
    },
    /// Read a format and report [`ClipEvent::LocalReadDone`].
    ReadLocal {
        /// The generation being served.
        generation: Gen,
        /// The format.
        kind: ClipKind,
    },
    /// Deliver a promised format to the OS.
    FulfillDeferred {
        /// The promise's generation.
        generation: Gen,
        /// The format.
        kind: ClipKind,
        /// The content, in canonical form.
        data: Bytes,
    },
    /// Give up on a promised format (write empty).
    FailDeferred {
        /// The promise's generation.
        generation: Gen,
        /// The format.
        kind: ClipKind,
        /// The reason.
        reason: FetchFailure,
    },
    /// Forward to `InputGate::on_applied`.
    GateRelease {
        /// The confirmed generation.
        generation: Gen,
    },
    /// Show on the toolbar.
    Notify(ClipNotice),
}

#[derive(Debug)]
struct LocalEntry {
    generation: Gen,
    formats: Vec<FormatInfo>,
    inline: Vec<(ClipKind, Bytes)>,
    /// The OS clipboard no longer holds this entry, so it must not be read to
    /// serve it. Set when a change was filtered out by policy.
    invalidated: bool,
    /// The peer's last non-Ok answer for this generation.
    rejected: Option<ClipStatus>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApplyState {
    Writing,
    Done(ClipStatus),
    /// A promised format could not be delivered and was written empty, so the
    /// OS clipboard no longer offers it. A retransmission restores the promise.
    PromiseLost,
}

#[derive(Debug)]
struct RemoteEntry {
    generation: Gen,
    formats: Vec<FormatInfo>,
    inline: Vec<(ClipKind, Bytes)>,
    state: ApplyState,
}

#[derive(Debug)]
struct Inflight<T> {
    generation: Gen,
    kind: ClipKind,
    size: u64,
    hash: ContentHash,
    received: Vec<u8>,
    out_of_order: BTreeMap<u64, Bytes>,
    requested_end: u64,
    deadline: T,
}

#[derive(Debug)]
struct HashEntry<T> {
    hash: ContentHash,
    written_at: T,
}

/// A clipboard sync endpoint. See the crate docs for how it is driven.
#[derive(Debug)]
pub struct ClipSync<T> {
    device: DeviceId,
    config: ClipConfig,
    connected: bool,
    lamport: u64,
    local: Option<LocalEntry>,
    remote: Option<RemoteEntry>,
    acked: Option<Gen>,
    self_hashes: VecDeque<HashEntry<T>>,
    inflight: Vec<Inflight<T>>,
    queued_fetches: Vec<ClipFetch>,
    serve_cache: Option<(Gen, ClipKind, Bytes)>,
}

impl<T: ClipInstant> ClipSync<T> {
    /// Creates a disconnected endpoint; deliver [`ClipEvent::Connected`] when
    /// the stream is up.
    pub fn new(device: DeviceId, config: ClipConfig) -> Self {
        Self {
            device,
            lamport: config.initial_lamport,
            config,
            connected: false,
            local: None,
            remote: None,
            acked: None,
            self_hashes: VecDeque::new(),
            inflight: Vec::new(),
            queued_fetches: Vec::new(),
            serve_cache: None,
        }
    }

    /// Processes one event and returns the resulting commands.
    pub fn handle(&mut self, event: ClipEvent, now: T) -> Vec<ClipCommand> {
        let mut out = Vec::new();
        match event {
            ClipEvent::LocalChanged(snapshot) => self.on_local_changed(snapshot, now, &mut out),
            ClipEvent::Received(ClipMessage::Announce(m)) => self.on_announce(m, now, &mut out),
            ClipEvent::Received(ClipMessage::Applied(m)) => self.on_applied(m, &mut out),
            ClipEvent::Received(ClipMessage::Fetch(m)) => self.on_fetch(m, &mut out),
            ClipEvent::Received(ClipMessage::Data(m)) => self.on_data(m, now, &mut out),
            ClipEvent::FetchRequested { generation, kind } => {
                self.on_fetch_requested(generation, kind, now, &mut out)
            }
            ClipEvent::WriteDone { generation, result } => {
                self.on_write_done(generation, result, &mut out)
            }
            ClipEvent::LocalReadDone {
                generation,
                kind,
                result,
            } => self.on_local_read_done(generation, kind, result, &mut out),
            ClipEvent::Connected => self.on_connected(&mut out),
            ClipEvent::Disconnected => {
                self.connected = false;
                self.queued_fetches.clear();
            }
            ClipEvent::PolicyChanged(policy) => self.on_policy_changed(policy, &mut out),
            ClipEvent::Tick => self.on_tick(now, &mut out),
        }
        out
    }

    /// The generation a paste must wait for, or `None` if it can go ahead.
    ///
    /// Pass the result to `InputGate::on_paste`.
    pub fn paste_requirement(&self) -> Option<Gen> {
        if !self.connected {
            return None;
        }
        let local = self.local.as_ref()?;
        if self.latest() != Some(local.generation)
            || local.invalidated
            || local.rejected.is_some()
            || self.acked >= Some(local.generation)
        {
            return None;
        }
        Some(local.generation)
    }

    /// When [`ClipEvent::Tick`] next needs to be delivered.
    pub fn next_deadline(&self) -> Option<T> {
        self.inflight.iter().map(|f| f.deadline).min()
    }

    /// The newest generation known here, local or remote.
    pub fn latest(&self) -> Option<Gen> {
        let local = self.local.as_ref().map(|e| e.generation);
        let remote = self.remote.as_ref().map(|e| e.generation);
        local.max(remote)
    }

    /// The newest generation originated here.
    pub fn local_generation(&self) -> Option<Gen> {
        self.local.as_ref().map(|e| e.generation)
    }

    /// The newest generation accepted from the peer.
    pub fn remote_generation(&self) -> Option<Gen> {
        self.remote.as_ref().map(|e| e.generation)
    }

    /// The newest local generation the peer confirmed.
    pub fn acked(&self) -> Option<Gen> {
        self.acked
    }

    /// The current Lamport counter.
    pub fn lamport(&self) -> u64 {
        self.lamport
    }

    /// The current policy.
    pub fn policy(&self) -> ClipPolicy {
        self.config.policy
    }

    fn send(&self, message: ClipMessage, out: &mut Vec<ClipCommand>) {
        if self.connected {
            out.push(ClipCommand::Send(message));
        }
    }

    fn send_applied(&self, generation: Gen, status: ClipStatus, out: &mut Vec<ClipCommand>) {
        self.send(
            ClipMessage::Applied(ClipApplied { generation, status }),
            out,
        );
    }

    fn on_local_changed(&mut self, snapshot: LocalSnapshot, now: T, out: &mut Vec<ClipCommand>) {
        if snapshot.owner_marker {
            // Our own write came back. Its hashes have served their purpose;
            // leaving them would swallow a genuine copy of the same content.
            // `formats` may be empty if the observer skipped listing them.
            let written = self.remote.as_ref().map(|r| r.formats.clone());
            self.forget_hashes(&snapshot.formats);
            self.forget_hashes(written.as_deref().unwrap_or_default());
            return;
        }
        if snapshot.formats.is_empty() {
            self.invalidate_local();
            return;
        }
        let policy = self.config.policy;
        if snapshot.sensitive && !policy.sync_sensitive {
            self.invalidate_local();
            out.push(ClipCommand::Notify(ClipNotice::SensitiveKeptLocal));
            return;
        }
        if self.consume_echo(&snapshot.formats, now) {
            return;
        }
        if !policy.outbound {
            self.invalidate_local();
            return;
        }
        if self.matches_latest(&snapshot.formats) {
            // The peer already has this content. If it is our entry, the OS
            // clipboard holds it again and it can be served.
            if let Some(local) = &mut self.local
                && Some(local.generation) >= self.remote.as_ref().map(|r| r.generation)
            {
                local.invalidated = false;
            }
            return;
        }

        let max_size = self.config.max_format_size;
        let mut formats = Vec::with_capacity(snapshot.formats.len());
        for format in snapshot.formats {
            if format.size > max_size {
                out.push(ClipCommand::Notify(ClipNotice::TooLarge {
                    kind: format.kind,
                    size: format.size,
                }));
            } else {
                formats.push(format);
            }
        }
        if formats.is_empty() {
            self.invalidate_local();
            return;
        }

        self.lamport += 1;
        let generation = (self.lamport, self.device);
        let inline = self.select_inline(&formats, &snapshot.data);
        self.local = Some(LocalEntry {
            generation,
            formats,
            inline,
            invalidated: false,
            rejected: None,
        });
        self.serve_cache = None;
        self.fail_older_fetches(generation, out);
        self.announce_local(out);
    }

    /// Picks what travels with the announcement: text formats only (§9.2), in a
    /// fixed order, within one budget for the whole message.
    fn select_inline(
        &self,
        formats: &[FormatInfo],
        data: &[(ClipKind, Bytes)],
    ) -> Vec<(ClipKind, Bytes)> {
        let mut budget = self.config.inline_threshold;
        let mut inline = Vec::new();
        for kind in [ClipKind::Text, ClipKind::Html, ClipKind::Rtf] {
            let Some(format) = formats.iter().find(|f| f.kind == kind) else {
                continue;
            };
            let bytes = data.iter().find(|(k, _)| *k == kind).map(|(_, b)| b);
            if let Some(bytes) = bytes
                && format.size <= budget
                && bytes.len() as u64 == format.size
            {
                budget -= format.size;
                inline.push((kind, bytes.clone()));
            }
        }
        inline
    }

    fn announce_local(&self, out: &mut Vec<ClipCommand>) {
        if let Some(local) = &self.local {
            self.send(
                ClipMessage::Announce(ClipAnnounce {
                    generation: local.generation,
                    origin: local.generation.1,
                    formats: local.formats.clone(),
                    inline: local.inline.clone(),
                }),
                out,
            );
        }
    }

    fn invalidate_local(&mut self) {
        if let Some(local) = &mut self.local {
            local.invalidated = true;
        }
        self.serve_cache = None;
    }

    /// Checks the hash LRU. An entry is good for one echo only: otherwise a user
    /// copying the same text again later would be silently ignored.
    fn consume_echo(&mut self, formats: &[FormatInfo], now: T) -> bool {
        let ttl = self.config.echo_ttl;
        let fresh = |entry: &HashEntry<T>, hash: ContentHash| {
            entry.hash == hash && entry.written_at + ttl >= now
        };
        let all_known = formats
            .iter()
            .all(|f| self.self_hashes.iter().any(|e| fresh(e, f.hash)));
        if !all_known {
            return false;
        }
        self.self_hashes
            .retain(|e| !formats.iter().any(|f| f.hash == e.hash));
        true
    }

    fn remember_hashes(&mut self, formats: &[FormatInfo], now: T) {
        for format in formats {
            self.self_hashes.retain(|e| e.hash != format.hash);
            if self.self_hashes.len() == self.config.hash_lru_capacity {
                self.self_hashes.pop_front();
            }
            self.self_hashes.push_back(HashEntry {
                hash: format.hash,
                written_at: now,
            });
        }
    }

    fn forget_hashes(&mut self, formats: &[FormatInfo]) {
        self.self_hashes
            .retain(|e| !formats.iter().any(|f| f.hash == e.hash));
    }

    fn matches_latest(&self, formats: &[FormatInfo]) -> bool {
        let latest = self.latest();
        let current = match (&self.local, &self.remote) {
            (Some(local), _) if Some(local.generation) == latest => &local.formats,
            (_, Some(remote)) if Some(remote.generation) == latest => &remote.formats,
            _ => return false,
        };
        formats.iter().all(|f| current.contains(f))
    }

    fn on_announce(&mut self, announce: ClipAnnounce, now: T, out: &mut Vec<ClipCommand>) {
        let generation = announce.generation;
        self.lamport = self.lamport.max(generation.0);
        if generation.1 == self.device {
            return;
        }
        if announce.origin != generation.1 {
            self.send_applied(generation, ClipStatus::Invalid, out);
            return;
        }
        if !self.config.policy.inbound {
            self.send_applied(generation, ClipStatus::Denied, out);
            return;
        }
        let latest = self.latest();
        if Some(generation) < latest {
            self.send_applied(generation, ClipStatus::Stale, out);
            return;
        }
        if Some(generation) == latest {
            self.on_duplicate_announce(generation, now, out);
            return;
        }
        if !announce_is_valid(&announce) {
            self.send_applied(generation, ClipStatus::Invalid, out);
            return;
        }

        let max_size = self.config.max_format_size;
        let formats: Vec<FormatInfo> = announce
            .formats
            .into_iter()
            .filter(|f| f.size <= max_size)
            .collect();
        if formats.is_empty() {
            self.send_applied(generation, ClipStatus::TooLarge, out);
            return;
        }
        let inline: Vec<(ClipKind, Bytes)> = announce
            .inline
            .into_iter()
            .filter(|(kind, _)| formats.iter().any(|f| f.kind == *kind))
            .collect();

        self.fail_older_fetches(generation, out);
        self.remember_hashes(&formats, now);
        out.push(ClipCommand::GateRelease { generation });
        out.push(ClipCommand::WritePromise {
            generation,
            formats: formats.clone(),
            inline: inline.clone(),
        });
        self.remote = Some(RemoteEntry {
            generation,
            formats,
            inline,
            state: ApplyState::Writing,
        });
    }

    /// A retransmission of the entry we already hold: answer it again, since
    /// the first answer may have been lost, and retry a write that failed.
    fn on_duplicate_announce(&mut self, generation: Gen, now: T, out: &mut Vec<ClipCommand>) {
        let Some(remote) = self.remote.as_mut() else {
            return;
        };
        match remote.state {
            ApplyState::Writing => {}
            ApplyState::Done(ClipStatus::Ok) => {
                self.send_applied(generation, ClipStatus::Ok, out);
            }
            ApplyState::Done(_) | ApplyState::PromiseLost => {
                remote.state = ApplyState::Writing;
                let formats = remote.formats.clone();
                let inline = remote.inline.clone();
                self.remember_hashes(&formats, now);
                out.push(ClipCommand::WritePromise {
                    generation,
                    formats,
                    inline,
                });
            }
        }
    }

    fn on_write_done(
        &mut self,
        generation: Gen,
        result: Result<(), ClipError>,
        out: &mut Vec<ClipCommand>,
    ) {
        let status = match result {
            Ok(()) => ClipStatus::Ok,
            Err(_) => ClipStatus::Busy,
        };
        let failed_formats = match self.remote.as_mut() {
            Some(remote) if remote.generation == generation => {
                remote.state = ApplyState::Done(status);
                (status != ClipStatus::Ok).then(|| remote.formats.clone())
            }
            _ => None,
        };
        if let Some(formats) = failed_formats {
            self.forget_hashes(&formats);
            out.push(ClipCommand::Notify(ClipNotice::WriteFailed { generation }));
        }
        self.send_applied(generation, status, out);
    }

    fn on_applied(&mut self, applied: ClipApplied, out: &mut Vec<ClipCommand>) {
        let generation = applied.generation;
        let Some(local) = self.local.as_mut() else {
            return;
        };
        if generation.1 != self.device || generation > local.generation {
            return;
        }
        if applied.status == ClipStatus::Ok {
            if self.acked < Some(generation) {
                self.acked = Some(generation);
            }
        } else if generation == local.generation {
            local.rejected = Some(applied.status);
            if applied.status != ClipStatus::Stale {
                out.push(ClipCommand::Notify(ClipNotice::PeerRejected {
                    generation,
                    status: applied.status,
                }));
            }
        }
        out.push(ClipCommand::GateRelease { generation });
    }

    fn on_connected(&mut self, out: &mut Vec<ClipCommand>) {
        self.connected = true;
        let latest = self.latest();
        let outbound = self.config.policy.outbound;
        if let Some(local) = &mut self.local
            && Some(local.generation) == latest
            && !local.invalidated
            && outbound
        {
            local.rejected = None;
            self.announce_local(out);
        }
        // Resume fetches from what already arrived.
        let window = self.config.fetch_window;
        let mut requests = Vec::new();
        for fetch in &mut self.inflight {
            fetch.out_of_order.clear();
            let offset = fetch.received.len() as u64;
            fetch.requested_end = fetch.size.min(offset + window);
            requests.push(ClipFetch {
                generation: fetch.generation,
                kind: fetch.kind,
                offset,
                len: fetch.requested_end - offset,
            });
        }
        for request in requests {
            self.send(ClipMessage::Fetch(request), out);
        }
    }

    fn on_policy_changed(&mut self, policy: ClipPolicy, out: &mut Vec<ClipCommand>) {
        self.config.policy = policy;
        if !policy.inbound {
            for fetch in std::mem::take(&mut self.inflight) {
                push_fetch_failure(fetch.generation, fetch.kind, FetchFailure::Denied, out);
            }
        }
        if !policy.outbound {
            self.serve_cache = None;
        }
    }

    fn on_tick(&mut self, now: T, out: &mut Vec<ClipCommand>) {
        let (expired, active): (Vec<_>, Vec<_>) = std::mem::take(&mut self.inflight)
            .into_iter()
            .partition(|f| now >= f.deadline);
        self.inflight = active;
        for fetch in expired {
            self.mark_promise_lost(fetch.generation);
            push_fetch_failure(fetch.generation, fetch.kind, FetchFailure::Timeout, out);
        }
    }

    fn mark_promise_lost(&mut self, generation: Gen) {
        if let Some(remote) = &mut self.remote
            && remote.generation == generation
        {
            remote.state = ApplyState::PromiseLost;
        }
    }

    fn fail_older_fetches(&mut self, newest: Gen, out: &mut Vec<ClipCommand>) {
        let (older, current): (Vec<_>, Vec<_>) = std::mem::take(&mut self.inflight)
            .into_iter()
            .partition(|f| f.generation < newest);
        self.inflight = current;
        for fetch in older {
            push_fetch_failure(fetch.generation, fetch.kind, FetchFailure::Stale, out);
        }
    }

    fn on_fetch_requested(
        &mut self,
        generation: Gen,
        kind: ClipKind,
        now: T,
        out: &mut Vec<ClipCommand>,
    ) {
        if self
            .inflight
            .iter()
            .any(|f| f.generation == generation && f.kind == kind)
        {
            return;
        }
        let entry = self
            .remote
            .as_ref()
            .filter(|r| r.generation == generation && self.latest() == Some(generation));
        let Some(entry) = entry else {
            push_fetch_failure(generation, kind, FetchFailure::Stale, out);
            return;
        };
        let Some(format) = entry.formats.iter().find(|f| f.kind == kind).copied() else {
            push_fetch_failure(generation, kind, FetchFailure::Stale, out);
            return;
        };
        if let Some((_, data)) = entry.inline.iter().find(|(k, _)| *k == kind) {
            out.push(ClipCommand::FulfillDeferred {
                generation,
                kind,
                data: data.clone(),
            });
            return;
        }
        if format.size == 0 {
            out.push(ClipCommand::FulfillDeferred {
                generation,
                kind,
                data: Bytes::new(),
            });
            return;
        }
        let requested_end = format.size.min(self.config.fetch_window);
        self.inflight.push(Inflight {
            generation,
            kind,
            size: format.size,
            hash: format.hash,
            received: Vec::new(),
            out_of_order: BTreeMap::new(),
            requested_end,
            deadline: now + self.config.fetch_timeout,
        });
        self.send(
            ClipMessage::Fetch(ClipFetch {
                generation,
                kind,
                offset: 0,
                len: requested_end,
            }),
            out,
        );
    }

    fn on_data(&mut self, data: ClipData, now: T, out: &mut Vec<ClipCommand>) {
        let Some(index) = self
            .inflight
            .iter()
            .position(|f| f.generation == data.generation && f.kind == data.kind)
        else {
            return;
        };
        let failure = match data.status {
            ClipStatus::Ok => None,
            ClipStatus::Stale => Some(FetchFailure::Stale),
            ClipStatus::Denied => Some(FetchFailure::Denied),
            ClipStatus::Busy => Some(FetchFailure::Busy),
            ClipStatus::TooLarge | ClipStatus::Invalid => Some(FetchFailure::Corrupt),
        };
        if let Some(reason) = failure {
            self.inflight.swap_remove(index);
            self.mark_promise_lost(data.generation);
            push_fetch_failure(data.generation, data.kind, reason, out);
            return;
        }

        let fetch = &mut self.inflight[index];
        let offset = data.offset;
        let end = offset.saturating_add(data.bytes.len() as u64);
        if end > fetch.size || (data.eof && end != fetch.size) {
            self.inflight.swap_remove(index);
            self.mark_promise_lost(data.generation);
            push_fetch_failure(data.generation, data.kind, FetchFailure::Corrupt, out);
            return;
        }

        let before = fetch.received.len();
        if offset > before as u64 {
            if offset < fetch.requested_end {
                fetch.out_of_order.insert(offset, data.bytes);
            }
        } else {
            extend_from_offset(&mut fetch.received, offset, &data.bytes);
        }
        while let Some(entry) = fetch.out_of_order.first_entry() {
            let chunk_offset = *entry.key();
            if chunk_offset > fetch.received.len() as u64 {
                break;
            }
            let chunk = entry.remove();
            extend_from_offset(&mut fetch.received, chunk_offset, &chunk);
        }
        if fetch.received.len() > before {
            fetch.deadline = now + self.config.fetch_timeout;
        }

        let received = fetch.received.len() as u64;
        if received == fetch.size {
            let fetch = self.inflight.swap_remove(index);
            if ContentHash::of(&fetch.received) == fetch.hash {
                out.push(ClipCommand::FulfillDeferred {
                    generation: fetch.generation,
                    kind: fetch.kind,
                    data: Bytes::from(fetch.received),
                });
            } else {
                self.mark_promise_lost(fetch.generation);
                push_fetch_failure(fetch.generation, fetch.kind, FetchFailure::Corrupt, out);
            }
        } else if received >= fetch.requested_end {
            fetch.requested_end = fetch.size.min(received + self.config.fetch_window);
            let request = ClipFetch {
                generation: fetch.generation,
                kind: fetch.kind,
                offset: received,
                len: fetch.requested_end - received,
            };
            self.send(ClipMessage::Fetch(request), out);
        }
    }

    /// Why a fetch for `(generation, kind)` cannot be served, if so.
    fn refuse_fetch(&self, generation: Gen, kind: ClipKind) -> Option<ClipStatus> {
        if !self.config.policy.outbound {
            return Some(ClipStatus::Denied);
        }
        let servable = self.local.as_ref().is_some_and(|local| {
            local.generation == generation
                && self.latest() == Some(generation)
                && !local.invalidated
                && local.formats.iter().any(|f| f.kind == kind)
        });
        (!servable).then_some(ClipStatus::Stale)
    }

    fn on_fetch(&mut self, fetch: ClipFetch, out: &mut Vec<ClipCommand>) {
        let (generation, kind) = (fetch.generation, fetch.kind);
        if let Some(status) = self.refuse_fetch(generation, kind) {
            self.send_fetch_error(&fetch, status, out);
            return;
        }
        if let Some(bytes) = self.lookup_servable(generation, kind) {
            self.serve(&fetch, &bytes, out);
            return;
        }
        if self.queued_fetches.len() >= MAX_QUEUED_FETCHES {
            self.send_fetch_error(&fetch, ClipStatus::Busy, out);
            return;
        }
        let read_pending = self
            .queued_fetches
            .iter()
            .any(|q| q.generation == generation && q.kind == kind);
        self.queued_fetches.push(fetch);
        if !read_pending {
            out.push(ClipCommand::ReadLocal { generation, kind });
        }
    }

    fn lookup_servable(&self, generation: Gen, kind: ClipKind) -> Option<Bytes> {
        let inline = self
            .local
            .as_ref()
            .and_then(|local| local.inline.iter().find(|(k, _)| *k == kind))
            .map(|(_, bytes)| bytes.clone());
        inline.or_else(|| match &self.serve_cache {
            Some((g, k, bytes)) if *g == generation && *k == kind => Some(bytes.clone()),
            _ => None,
        })
    }

    fn on_local_read_done(
        &mut self,
        generation: Gen,
        kind: ClipKind,
        result: Result<Bytes, ClipError>,
        out: &mut Vec<ClipCommand>,
    ) {
        let (requests, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.queued_fetches)
            .into_iter()
            .partition(|q| q.generation == generation && q.kind == kind);
        self.queued_fetches = rest;
        if requests.is_empty() {
            return;
        }
        let outcome = match self.refuse_fetch(generation, kind) {
            Some(status) => Err(status),
            None => result.map_err(|_| ClipStatus::Busy),
        };
        let outcome = outcome.and_then(|bytes| {
            // The OS clipboard may have changed without us having seen the
            // event yet. Never send bytes that are not the announced content.
            if self.announced_hash(kind) == Some(ContentHash::of(&bytes)) {
                Ok(bytes)
            } else {
                Err(ClipStatus::Stale)
            }
        });
        match outcome {
            Ok(bytes) => {
                self.serve_cache = Some((generation, kind, bytes.clone()));
                for request in &requests {
                    self.serve(request, &bytes, out);
                }
            }
            Err(status) => {
                if status == ClipStatus::Stale {
                    self.invalidate_local();
                }
                for request in &requests {
                    self.send_fetch_error(request, status, out);
                }
            }
        }
    }

    fn announced_hash(&self, kind: ClipKind) -> Option<ContentHash> {
        let local = self.local.as_ref()?;
        local
            .formats
            .iter()
            .find(|f| f.kind == kind)
            .map(|f| f.hash)
    }

    fn send_fetch_error(&self, fetch: &ClipFetch, status: ClipStatus, out: &mut Vec<ClipCommand>) {
        self.send(
            ClipMessage::Data(ClipData {
                generation: fetch.generation,
                kind: fetch.kind,
                offset: fetch.offset,
                bytes: Bytes::new(),
                eof: true,
                status,
            }),
            out,
        );
    }

    /// Answers a fetch in chunks, capped at one window however much was asked.
    fn serve(&self, fetch: &ClipFetch, bytes: &Bytes, out: &mut Vec<ClipCommand>) {
        let total = bytes.len() as u64;
        let start = fetch.offset.min(total);
        let end = total.min(start.saturating_add(fetch.len.min(self.config.fetch_window)));
        let chunk_size = self.config.chunk_size.max(1);
        let mut offset = start;
        loop {
            let chunk_end = end.min(offset + chunk_size);
            self.send(
                ClipMessage::Data(ClipData {
                    generation: fetch.generation,
                    kind: fetch.kind,
                    offset,
                    bytes: bytes.slice(offset as usize..chunk_end as usize),
                    eof: chunk_end == total,
                    status: ClipStatus::Ok,
                }),
                out,
            );
            offset = chunk_end;
            if offset >= end {
                break;
            }
        }
    }
}

/// Appends the part of `chunk` (starting at `offset`) beyond what is received.
fn extend_from_offset(received: &mut Vec<u8>, offset: u64, chunk: &[u8]) {
    let skip = received.len() as u64 - offset;
    if skip < chunk.len() as u64 {
        received.extend_from_slice(&chunk[skip as usize..]);
    }
}

fn push_fetch_failure(
    generation: Gen,
    kind: ClipKind,
    reason: FetchFailure,
    out: &mut Vec<ClipCommand>,
) {
    out.push(ClipCommand::FailDeferred {
        generation,
        kind,
        reason,
    });
    out.push(ClipCommand::Notify(ClipNotice::FetchFailed {
        generation,
        kind,
        reason,
    }));
}

/// Checks that the formats are unique and that inline content is what it claims
/// to be, since it goes straight onto the OS clipboard.
fn announce_is_valid(announce: &ClipAnnounce) -> bool {
    let formats = &announce.formats;
    if formats.is_empty() {
        return false;
    }
    let unique = formats
        .iter()
        .enumerate()
        .all(|(i, f)| formats[..i].iter().all(|other| other.kind != f.kind));
    unique
        && announce.inline.iter().all(|(kind, bytes)| {
            formats.iter().any(|f| {
                f.kind == *kind && f.size == bytes.len() as u64 && f.hash == ContentHash::of(bytes)
            })
        })
}
