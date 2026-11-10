//! Connectivity checks (§4.4), sans-IO.
//!
//! One local socket per session, so a pair is simply a remote target: a peer
//! candidate, a predicted port, a peer-reflexive address learned from a request,
//! or our relay session. Both sides check, since both NATs need outbound traffic
//! before they let the other side in.
//!
//! Every request carries `USERNAME = remote_ufrag:local_ufrag` and
//! MESSAGE-INTEGRITY-SHA256 under the *receiver's* password; the response is
//! signed with the same key (RFC 8445 §7.2.2). Unauthenticated messages get no
//! answer, so the checker cannot be used for reflection.
//!
//! A pair is `Validated` for the viewer when its own request got an
//! authenticated answer: the request crossed both NATs and the answer came back,
//! which is §4.4's "both directions". The host also counts a path as validated
//! when an authenticated request arrived from it: the viewer, who holds the
//! password, demonstrably sends from there, and the viewer only starts QUIC on a
//! path whose round trip it saw. Without this the host would drop the viewer's
//! first QUIC packet whenever its own answer was still in flight.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};

use crate::{
    path::{CandKind, Role},
    stun::{
        StunClass, StunMessage, TransactionId, check_username, encode_binding_request,
        encode_binding_success_tagged,
    },
    vsock::PathTarget,
};

/// §4.4 【初值】.
pub const MAX_PAIRS: usize = 64;
pub const PACE: Duration = Duration::from_millis(20);
pub const RETRANSMITS: [Duration; 3] = [
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
];
pub const CHECK_DEADLINE: Duration = Duration::from_secs(3);
/// The relay pair keeps trying a little past the deadline: it is the fallback, and
/// the peer may bind its relay seat late.
const RELAY_RETRY: Duration = Duration::from_millis(200);
const RELAY_GRACE: Duration = Duration::from_secs(2);
/// Predicted ports tried past the peer's last observed mapping (§5.3 "端口增量预测").
pub const PREDICT_WINDOW: i32 = 8;
/// Deltas larger than this are taken as random allocation, not worth predicting.
pub const PREDICT_MAX_DELTA: i32 = 16;
/// Outstanding probe transactions kept for RTT matching.
const MAX_PENDING_TXIDS: usize = 512;

/// A CSPRNG fill function: it draws transaction ids.
pub type Rng = Box<dyn FnMut(&mut [u8]) + Send>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credentials {
    pub ufrag: String,
    pub pwd: String,
}

const CRED_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

impl Credentials {
    /// 8-character ufrag (48 bits) and 32-character password (192 bits).
    pub fn generate(rng: &mut dyn FnMut(&mut [u8])) -> Self {
        let mut pick = |n: usize| {
            let mut raw = vec![0u8; n];
            rng(&mut raw);
            raw.iter()
                .map(|b| CRED_ALPHABET[usize::from(*b & 63)] as char)
                .collect::<String>()
        };
        Self {
            ufrag: pick(8),
            pwd: pick(32),
        }
    }
}

/// What a peer told us about its NAT (§4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerNat {
    Unknown,
    Easy,
    Hard { port_delta: i32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteCandidate {
    pub addr: SocketAddr,
    pub kind: CandKind,
}

/// How a path is shown to people (§11 diagnostics).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathClass {
    /// Straight to the peer's own address (LAN, public IP, IPv6).
    Direct,
    /// Through the NATs, by hole punching.
    Punched,
    Relay,
    Tailscale,
}

impl PathClass {
    pub fn of(kind: CandKind) -> Self {
        match kind {
            CandKind::Host | CandKind::Ipv6 => PathClass::Direct,
            CandKind::Srflx | CandKind::Portmap => PathClass::Punched,
            CandKind::TailnetDirect | CandKind::TailnetDerp => PathClass::Tailscale,
            CandKind::Relay | CandKind::WssRelay => PathClass::Relay,
        }
    }
}

/// Tailscale's CGNAT and ULA ranges (§4.2).
pub fn is_tailnet(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (64..128).contains(&o[1])
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

/// The kind of an address learned from a request rather than from the peer's list.
pub fn kind_of_unlisted(addr: SocketAddr) -> CandKind {
    let ip = addr.ip();
    if is_tailnet(ip) {
        return CandKind::TailnetDirect;
    }
    let private = match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00,
    };
    if private {
        CandKind::Host
    } else {
        CandKind::Srflx
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PairState {
    Waiting,
    InProgress,
    Succeeded,
    Failed,
}

#[derive(Debug)]
struct Pair {
    target: PathTarget,
    kind: CandKind,
    state: PairState,
    /// Sends so far, including the first.
    sends: usize,
    next_retx: Option<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CheckEvent {
    /// A path that passed. `remote_tag` is the peer's label for it, to name it in
    /// `PathSwitch`; `mapped` is how the peer sees us on it.
    Validated {
        target: PathTarget,
        kind: CandKind,
        rtt: Option<Duration>,
        remote_tag: Option<u32>,
        mapped: Option<SocketAddr>,
    },
    /// An answer to [`Checker::probe`].
    ProbeAnswered {
        target: PathTarget,
        rtt: Duration,
        remote_tag: Option<u32>,
    },
    /// Any authenticated request from the peer arrived on this path.
    PeerSpoke { target: PathTarget },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outgoing {
    pub to: PathTarget,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
struct Pending {
    target: PathTarget,
    sent_at: Instant,
    probe: bool,
}

pub struct Checker {
    role: Role,
    local: Credentials,
    remote: Option<Credentials>,
    /// Address family of the local socket; other families are never paired.
    family_v4: bool,
    pairs: Vec<Pair>,
    /// Indexes into `pairs` checked out of turn (triggered checks).
    triggered: VecDeque<usize>,
    pending: HashMap<TransactionId, Pending>,
    /// Our labels for the paths the peer reaches us on.
    tags: HashMap<PathTarget, u32>,
    next_tag: u32,
    started: Option<Instant>,
    next_send: Option<Instant>,
    validated: HashSet<PathTarget>,
    /// Requests that arrived before we knew the peer's credentials.
    early: Vec<PathTarget>,
    events: VecDeque<CheckEvent>,
    rng: Rng,
    peer_nat: PeerNat,
}

impl std::fmt::Debug for Checker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Checker")
            .field("role", &self.role)
            .field("pairs", &self.pairs.len())
            .field("validated", &self.validated)
            .finish_non_exhaustive()
    }
}

impl Checker {
    /// `rng` must be a CSPRNG: it draws transaction ids.
    pub fn new(role: Role, local: Credentials, family_v4: bool, rng: Rng) -> Self {
        Self {
            role,
            local,
            remote: None,
            family_v4,
            pairs: Vec::new(),
            triggered: VecDeque::new(),
            pending: HashMap::new(),
            tags: HashMap::new(),
            next_tag: 1,
            started: None,
            next_send: None,
            validated: HashSet::new(),
            early: Vec::new(),
            events: VecDeque::new(),
            rng,
            peer_nat: PeerNat::Unknown,
        }
    }

    pub fn local_credentials(&self) -> &Credentials {
        &self.local
    }

    /// The peer's credentials and first candidates: checking starts now.
    pub fn set_remote(
        &mut self,
        creds: Credentials,
        candidates: &[RemoteCandidate],
        nat: PeerNat,
        now: Instant,
    ) {
        if self.remote.is_some() {
            return;
        }
        self.remote = Some(creds);
        self.peer_nat = nat;
        self.started = Some(now);
        self.next_send = Some(now);
        self.add_candidates(candidates);
        for target in std::mem::take(&mut self.early) {
            self.trigger(target, None);
        }
    }

    /// Trickled candidates, or the 30 s re-punch's fresh look (§5.3).
    pub fn add_candidates(&mut self, candidates: &[RemoteCandidate]) {
        let mut sorted: Vec<RemoteCandidate> = candidates
            .iter()
            .filter(|c| c.addr.is_ipv4() == self.family_v4)
            .copied()
            .collect();
        sorted.sort_by_key(|c| priority(c.kind));
        for c in &sorted {
            self.add_pair(PathTarget::Direct(c.addr), c.kind);
        }
        if let PeerNat::Hard { port_delta } = self.peer_nat
            && port_delta != 0
            && port_delta.abs() <= PREDICT_MAX_DELTA
            && let Some(base) = sorted.iter().rev().find(|c| c.kind == CandKind::Srflx)
        {
            for k in 1..=PREDICT_WINDOW {
                let port = i32::from(base.addr.port()) + port_delta * k;
                if let Ok(port) = u16::try_from(port)
                    && port != 0
                {
                    self.add_pair(
                        PathTarget::Direct(SocketAddr::new(base.addr.ip(), port)),
                        CandKind::Srflx,
                    );
                }
            }
        }
    }

    /// Our relay session as a path: checks go through the relay to the peer.
    pub fn add_relay(&mut self, target: PathTarget) {
        self.add_pair(target, CandKind::Relay);
    }

    fn add_pair(&mut self, target: PathTarget, kind: CandKind) -> Option<usize> {
        if let Some(i) = self.pairs.iter().position(|p| p.target == target) {
            return Some(i);
        }
        if self.pairs.len() >= MAX_PAIRS {
            return None;
        }
        self.pairs.push(Pair {
            target,
            kind,
            state: PairState::Waiting,
            sends: 0,
            next_retx: None,
        });
        Some(self.pairs.len() - 1)
    }

    /// Checks `target` next, adding it as peer-reflexive if it is new.
    fn trigger(&mut self, target: PathTarget, kind: Option<CandKind>) {
        let kind = kind.unwrap_or_else(|| match target {
            PathTarget::Direct(a) => kind_of_unlisted(a),
            PathTarget::Relay { .. } => CandKind::Relay,
        });
        let Some(i) = self.add_pair(target, kind) else {
            return;
        };
        let p = &mut self.pairs[i];
        if matches!(p.state, PairState::Waiting | PairState::Failed) {
            p.state = PairState::Waiting;
            if !self.triggered.contains(&i) {
                self.triggered.push_back(i);
            }
        }
    }

    pub fn is_validated(&self, target: &PathTarget) -> bool {
        self.validated.contains(target)
    }

    pub fn validated(&self) -> impl Iterator<Item = &PathTarget> {
        self.validated.iter()
    }

    /// Our label for a path, as the peer will name it in `PathSwitch`.
    pub fn tag_of(&self, target: &PathTarget) -> Option<u32> {
        self.tags.get(target).copied()
    }

    /// The path we labelled `tag`, if we did.
    pub fn target_of_tag(&self, tag: u32) -> Option<PathTarget> {
        self.tags
            .iter()
            .find_map(|(t, &v)| (v == tag).then_some(*t))
    }

    pub fn kind_of(&self, target: &PathTarget) -> Option<CandKind> {
        self.pairs
            .iter()
            .find(|p| p.target == *target)
            .map(|p| p.kind)
    }

    fn tag_for(&mut self, target: PathTarget) -> u32 {
        if let Some(&t) = self.tags.get(&target) {
            return t;
        }
        let t = self.next_tag;
        self.next_tag = self.next_tag.wrapping_add(1).max(1);
        self.tags.insert(target, t);
        t
    }

    pub fn drain_events(&mut self) -> impl Iterator<Item = CheckEvent> + '_ {
        self.events.drain(..)
    }

    /// The deadline has passed: anything not validated by now is given up on,
    /// except the relay, which gets a short grace.
    pub fn deadline_passed(&self, now: Instant) -> bool {
        self.started
            .is_some_and(|s| now.saturating_duration_since(s) >= CHECK_DEADLINE)
    }

    /// Nothing left to try. With no pairs yet (a relay-only peer whose relay
    /// seat is still binding) only the deadline ends it.
    pub fn finished(&self, now: Instant) -> bool {
        self.started.is_some_and(|s| {
            now.saturating_duration_since(s) >= CHECK_DEADLINE + RELAY_GRACE
                || (!self.pairs.is_empty()
                    && self
                        .pairs
                        .iter()
                        .all(|p| matches!(p.state, PairState::Succeeded | PairState::Failed)))
        })
    }

    /// §5.3: on relay, punching is retried every 30 s. Failed direct pairs start over.
    pub fn retry_failed(&mut self, now: Instant) {
        if self.remote.is_none() {
            return;
        }
        let mut any = false;
        for p in &mut self.pairs {
            if p.state == PairState::Failed && !p.target.is_relay() {
                p.state = PairState::Waiting;
                p.sends = 0;
                p.next_retx = None;
                any = true;
            }
        }
        if any {
            self.started = Some(now);
            self.next_send = Some(now);
        }
    }

    fn request(&mut self, target: PathTarget, probe: bool, now: Instant) -> Option<Outgoing> {
        let remote = self.remote.as_ref()?;
        let mut id = [0u8; 12];
        (self.rng)(&mut id);
        let txid = TransactionId(id);
        let name = check_username(&remote.ufrag, &self.local.ufrag);
        let bytes = encode_binding_request(&txid, Some(&name), Some(remote.pwd.as_bytes())).ok()?;
        if self.pending.len() >= MAX_PENDING_TXIDS {
            let horizon = now.checked_sub(Duration::from_secs(5)).unwrap_or(now);
            self.pending.retain(|_, p| p.sent_at > horizon);
            if self.pending.len() >= MAX_PENDING_TXIDS {
                return None;
            }
        }
        self.pending.insert(
            txid,
            Pending {
                target,
                sent_at: now,
                probe,
            },
        );
        Some(Outgoing { to: target, bytes })
    }

    /// An RTT probe or keepalive on a path (§4.5). Its answer becomes
    /// [`CheckEvent::ProbeAnswered`].
    pub fn probe(&mut self, target: PathTarget, now: Instant) -> Option<Outgoing> {
        self.request(target, true, now)
    }

    /// Sends what is due: one new check per [`PACE`], plus retransmissions.
    pub fn poll(&mut self, now: Instant) -> Vec<Outgoing> {
        let mut out = Vec::new();
        let Some(started) = self.started else {
            return out;
        };
        let elapsed = now.saturating_duration_since(started);
        let past_deadline = elapsed >= CHECK_DEADLINE;

        // Retransmissions and timeouts.
        for i in 0..self.pairs.len() {
            let p = &self.pairs[i];
            if p.state != PairState::InProgress || p.next_retx.is_none_or(|t| now < t) {
                continue;
            }
            let relay = p.target.is_relay();
            let exhausted = if relay {
                elapsed >= CHECK_DEADLINE + RELAY_GRACE
            } else {
                p.sends > RETRANSMITS.len() || past_deadline
            };
            if exhausted {
                self.pairs[i].state = PairState::Failed;
                self.pairs[i].next_retx = None;
                continue;
            }
            let target = p.target;
            let gap = if relay {
                RELAY_RETRY
            } else {
                RETRANSMITS.get(p.sends).copied().unwrap_or(RETRANSMITS[2])
            };
            if let Some(o) = self.request(target, false, now) {
                out.push(o);
            }
            let p = &mut self.pairs[i];
            p.sends += 1;
            p.next_retx = Some(now + gap);
        }

        // One new (or triggered) check per pacing slot.
        while self.next_send.is_some_and(|t| now >= t) {
            let (i, triggered) = match self.triggered.pop_front() {
                Some(i) => (i, true),
                None => match self
                    .pairs
                    .iter()
                    .position(|p| p.state == PairState::Waiting)
                {
                    Some(i) => (i, false),
                    None => break,
                },
            };
            if self.pairs[i].state != PairState::Waiting {
                continue;
            }
            if past_deadline && !triggered && !self.pairs[i].target.is_relay() {
                // Past the deadline only the relay and triggered checks still start.
                self.pairs[i].state = PairState::Failed;
                continue;
            }
            let target = self.pairs[i].target;
            if let Some(o) = self.request(target, false, now) {
                out.push(o);
            }
            let first_gap = if target.is_relay() {
                RELAY_RETRY
            } else {
                RETRANSMITS[0]
            };
            let p = &mut self.pairs[i];
            p.state = PairState::InProgress;
            p.sends = 1;
            p.next_retx = Some(now + first_gap);
            self.next_send = self.next_send.map(|t| (t + PACE).max(now));
            if self.next_send.is_some_and(|t| t > now) {
                break;
            }
        }
        out
    }

    /// Handles a STUN message that arrived on `from`. Returns the answer to send,
    /// if it was an authenticated request.
    pub fn on_stun(&mut self, packet: &[u8], from: PathTarget, now: Instant) -> Option<Outgoing> {
        let msg = StunMessage::parse(packet).ok()?;
        if msg.unknown_required.is_some() {
            return None;
        }
        match msg.class {
            StunClass::Request => self.on_request(&msg, from, now),
            StunClass::Success => {
                self.on_success(&msg, from, now);
                None
            }
            StunClass::Indication | StunClass::Error => None,
        }
    }

    fn on_request(
        &mut self,
        msg: &StunMessage<'_>,
        from: PathTarget,
        now: Instant,
    ) -> Option<Outgoing> {
        let name = msg.username?;
        let (ours, theirs) = name.split_once(':')?;
        if ours != self.local.ufrag {
            return None;
        }
        if let Some(r) = &self.remote
            && theirs != r.ufrag
        {
            return None;
        }
        msg.verify_integrity(self.local.pwd.as_bytes()).ok()?;
        if let PathTarget::Direct(a) = from
            && a.is_ipv4() != self.family_v4
        {
            return None;
        }

        let tag = self.tag_for(from);
        self.events
            .push_back(CheckEvent::PeerSpoke { target: from });
        if self.role == Role::Host && self.validated.insert(from) {
            let kind = self.kind_of(&from).unwrap_or_else(|| match from {
                PathTarget::Direct(a) => kind_of_unlisted(a),
                PathTarget::Relay { .. } => CandKind::Relay,
            });
            self.events.push_back(CheckEvent::Validated {
                target: from,
                kind,
                rtt: None,
                remote_tag: None,
                mapped: None,
            });
        }
        if self.remote.is_some() {
            if !self.validated.contains(&from) || self.role == Role::Host {
                // RFC 8445 §7.3.1.4: a request on a pair being checked triggers a check
                // now, since the peer's NAT has just shown it lets us in.
                let known = self.pairs.iter().position(|p| p.target == from);
                if let Some(i) = known
                    && self.pairs[i].state == PairState::InProgress
                {
                    self.pairs[i].state = PairState::Waiting;
                }
                if !matches!(
                    known.map(|i| self.pairs[i].state),
                    Some(PairState::Succeeded)
                ) {
                    self.trigger(from, None);
                    // A triggered check goes out at once rather than waiting its turn.
                    if self.next_send.is_some_and(|t| t > now) {
                        self.next_send = Some(now);
                    }
                }
            }
        } else if self.early.len() < MAX_PAIRS && !self.early.contains(&from) {
            self.early.push(from);
        }
        let bytes = encode_binding_success_tagged(
            &msg.txid,
            from.wire_addr(),
            Some(tag),
            Some(self.local.pwd.as_bytes()),
        )
        .ok()?;
        Some(Outgoing { to: from, bytes })
    }

    fn on_success(&mut self, msg: &StunMessage<'_>, from: PathTarget, now: Instant) {
        let Some(pending) = self.pending.get(&msg.txid).copied() else {
            return;
        };
        // An answer must come back on the path it was sent on.
        if pending.target != from {
            return;
        }
        let Some(remote) = &self.remote else { return };
        if msg.verify_integrity(remote.pwd.as_bytes()).is_err() {
            return;
        }
        self.pending.remove(&msg.txid);
        let rtt = now.saturating_duration_since(pending.sent_at);
        if pending.probe {
            self.events.push_back(CheckEvent::ProbeAnswered {
                target: from,
                rtt,
                remote_tag: msg.path_tag,
            });
            return;
        }
        let Some(i) = self.pairs.iter().position(|p| p.target == from) else {
            return;
        };
        let pair = &mut self.pairs[i];
        pair.state = PairState::Succeeded;
        pair.next_retx = None;
        let kind = pair.kind;
        self.tag_for(from);
        if self.validated.insert(from) || self.role == Role::Viewer {
            self.events.push_back(CheckEvent::Validated {
                target: from,
                kind,
                rtt: Some(rtt),
                remote_tag: msg.path_tag,
                mapped: msg.mapped,
            });
        } else {
            // Host: already validated by the peer's request; this is an RTT sample.
            self.events.push_back(CheckEvent::ProbeAnswered {
                target: from,
                rtt,
                remote_tag: msg.path_tag,
            });
        }
    }
}

/// Lower goes first: the cheapest, most direct paths.
fn priority(kind: CandKind) -> u8 {
    match kind {
        CandKind::Host => 0,
        CandKind::Ipv6 => 1,
        CandKind::TailnetDirect => 2,
        CandKind::Portmap => 3,
        CandKind::Srflx => 4,
        CandKind::TailnetDerp => 5,
        CandKind::Relay => 6,
        CandKind::WssRelay => 7,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng(seed: u64) -> Rng {
        let mut x = seed | 1;
        Box::new(move |buf| {
            for b in buf {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                *b = x as u8;
            }
        })
    }

    fn creds(r: &mut Rng) -> Credentials {
        Credentials::generate(&mut **r)
    }

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn pair(t: Instant) -> (Checker, Checker) {
        let mut r = rng(7);
        let (vc, hc) = (creds(&mut r), creds(&mut r));
        let mut v = Checker::new(Role::Viewer, vc.clone(), true, rng(1));
        let mut h = Checker::new(Role::Host, hc.clone(), true, rng(2));
        v.set_remote(
            hc,
            &[RemoteCandidate {
                addr: sa("10.0.0.2:2000"),
                kind: CandKind::Host,
            }],
            PeerNat::Unknown,
            t,
        );
        h.set_remote(
            vc,
            &[RemoteCandidate {
                addr: sa("10.0.0.1:1000"),
                kind: CandKind::Host,
            }],
            PeerNat::Unknown,
            t,
        );
        (v, h)
    }

    #[test]
    fn credentials_are_valid_and_distinct() {
        let mut r = rng(3);
        let a = creds(&mut r);
        let b = creds(&mut r);
        assert_ne!(a, b);
        assert!(nexa_wire::signal::valid_credential(&a.ufrag, 4, 64));
        assert!(nexa_wire::signal::valid_credential(&a.pwd, 22, 128));
    }

    #[test]
    fn checks_validate_both_ways_and_exchange_tags() {
        let t = Instant::now();
        let (mut v, mut h) = pair(t);
        let to_host = v.poll(t);
        assert_eq!(to_host.len(), 1);
        assert_eq!(to_host[0].to, PathTarget::Direct(sa("10.0.0.2:2000")));
        // The host sees it from the viewer's address and answers.
        let from_v = PathTarget::Direct(sa("10.0.0.1:1000"));
        let answer = h.on_stun(&to_host[0].bytes, from_v, t).unwrap();
        assert_eq!(answer.to, from_v);
        let host_events: Vec<_> = h.drain_events().collect();
        assert!(
            host_events
                .iter()
                .any(|e| matches!(e, CheckEvent::Validated { target, .. } if *target == from_v))
        );
        let tag = h.tag_of(&from_v).unwrap();

        let later = t + Duration::from_millis(30);
        assert!(v.on_stun(&answer.bytes, to_host[0].to, later).is_none());
        let ev: Vec<_> = v.drain_events().collect();
        assert_eq!(
            ev,
            vec![CheckEvent::Validated {
                target: to_host[0].to,
                kind: CandKind::Host,
                rtt: Some(Duration::from_millis(30)),
                remote_tag: Some(tag),
                mapped: Some(sa("10.0.0.1:1000")),
            }]
        );
        assert!(v.is_validated(&to_host[0].to));
        assert_eq!(h.target_of_tag(tag), Some(from_v));
    }

    #[test]
    fn forged_or_misrouted_answers_are_ignored() {
        let t = Instant::now();
        let (mut v, mut h) = pair(t);
        let req = v.poll(t).remove(0);
        let ans = h
            .on_stun(&req.bytes, PathTarget::Direct(sa("10.0.0.1:1000")), t)
            .unwrap();
        // Arriving from somewhere else than where the request went.
        v.on_stun(&ans.bytes, PathTarget::Direct(sa("10.0.0.9:2000")), t);
        assert!(!v.is_validated(&req.to));
        // A flipped bit.
        let mut bad = ans.bytes.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        v.on_stun(&bad, req.to, t);
        assert!(!v.is_validated(&req.to));
        v.on_stun(&ans.bytes, req.to, t);
        assert!(v.is_validated(&req.to));
    }

    #[test]
    fn unauthenticated_requests_get_no_answer() {
        let t = Instant::now();
        let (_, mut h) = pair(t);
        let from = PathTarget::Direct(sa("198.51.100.66:1"));
        let txid = TransactionId([1; 12]);
        let plain = encode_binding_request(&txid, None, None).unwrap();
        assert!(h.on_stun(&plain, from, t).is_none());
        let wrong_key = encode_binding_request(
            &txid,
            Some(&check_username(&h.local.ufrag, "zzzz")),
            Some(b"not the password"),
        )
        .unwrap();
        assert!(h.on_stun(&wrong_key, from, t).is_none());
        assert!(h.drain_events().next().is_none());
        assert!(h.tag_of(&from).is_none());
    }

    #[test]
    fn retransmits_then_fails_on_schedule() {
        let t = Instant::now();
        let (mut v, _) = pair(t);
        let mut sends = Vec::new();
        for ms in (0..1500).step_by(5) {
            let now = t + Duration::from_millis(ms);
            if !v.poll(now).is_empty() {
                sends.push(ms);
            }
        }
        assert_eq!(sends, vec![0, 100, 300, 700]);
        assert!(v.finished(t + Duration::from_millis(1200)));
    }

    #[test]
    fn checks_are_paced_and_capped() {
        let t = Instant::now();
        let mut r = rng(9);
        let mut v = Checker::new(Role::Viewer, creds(&mut r), true, rng(4));
        let many: Vec<RemoteCandidate> = (0..100)
            .map(|i| RemoteCandidate {
                addr: SocketAddr::new([203, 0, 113, 1].into(), 1000 + i),
                kind: CandKind::Srflx,
            })
            .collect();
        v.set_remote(creds(&mut r), &many, PeerNat::Unknown, t);
        assert_eq!(v.pairs.len(), MAX_PAIRS);
        let mut sent = 0;
        for ms in 0..=100 {
            sent += v.poll(t + Duration::from_millis(ms)).len();
        }
        // Pacing slots at 0, 20, ..., 100 ms, plus the first pair's retransmission at 100.
        assert_eq!(sent, 7);
    }

    #[test]
    fn hard_peer_gets_predicted_ports() {
        let t = Instant::now();
        let mut r = rng(5);
        let mut v = Checker::new(Role::Viewer, creds(&mut r), true, rng(6));
        v.set_remote(
            creds(&mut r),
            &[
                RemoteCandidate {
                    addr: sa("198.51.100.1:30000"),
                    kind: CandKind::Srflx,
                },
                RemoteCandidate {
                    addr: sa("198.51.100.1:30001"),
                    kind: CandKind::Srflx,
                },
                RemoteCandidate {
                    addr: sa("[2001:db8::1]:5"),
                    kind: CandKind::Ipv6,
                },
            ],
            PeerNat::Hard { port_delta: 1 },
            t,
        );
        let targets: Vec<u16> = v
            .pairs
            .iter()
            .map(|p| p.target.wire_addr().port())
            .collect();
        assert_eq!(
            targets,
            vec![
                30000, 30001, 30002, 30003, 30004, 30005, 30006, 30007, 30008, 30009
            ],
            "the IPv6 candidate is not paired with an IPv4 socket"
        );
        let mut w = Checker::new(Role::Viewer, creds(&mut r), true, rng(6));
        w.set_remote(
            creds(&mut r),
            &[RemoteCandidate {
                addr: sa("198.51.100.1:30000"),
                kind: CandKind::Srflx,
            }],
            PeerNat::Hard { port_delta: 977 },
            t,
        );
        assert_eq!(w.pairs.len(), 1, "random allocation is not predicted");
    }

    #[test]
    fn a_request_triggers_a_check_back_to_an_unknown_source() {
        let t = Instant::now();
        let (mut v, mut h) = pair(t);
        // The viewer's request reaches the host from a NAT mapping it was never told about.
        let req = v.poll(t).remove(0);
        let prflx = PathTarget::Direct(sa("198.51.100.7:31337"));
        assert!(h.on_stun(&req.bytes, prflx, t).is_some());
        let out = h.poll(t);
        assert!(out.iter().any(|o| o.to == prflx), "{out:?}");
        assert_eq!(h.kind_of(&prflx), Some(CandKind::Srflx));
    }

    #[test]
    fn early_requests_are_answered_and_checked_back_later() {
        let t = Instant::now();
        let mut r = rng(11);
        let (vc, hc) = (creds(&mut r), creds(&mut r));
        let mut v = Checker::new(Role::Viewer, vc.clone(), true, rng(12));
        let mut h = Checker::new(Role::Host, hc.clone(), true, rng(13));
        h.set_remote(vc, &[], PeerNat::Unknown, t);
        h.add_candidates(&[RemoteCandidate {
            addr: sa("10.0.0.1:1000"),
            kind: CandKind::Host,
        }]);
        let req = h.poll(t).remove(0);
        // The viewer has not seen the answer yet, but can still authenticate and reply.
        let from = PathTarget::Direct(sa("10.0.0.2:2000"));
        assert!(v.on_stun(&req.bytes, from, t).is_some());
        assert!(v.poll(t).is_empty());
        v.set_remote(hc, &[], PeerNat::Unknown, t);
        assert!(v.poll(t).iter().any(|o| o.to == from));
    }

    #[test]
    fn relay_pair_keeps_trying_past_the_direct_deadline() {
        let t = Instant::now();
        let (mut v, _) = pair(t);
        let relay = PathTarget::Relay {
            server: sa("203.0.113.1:3480"),
            session: [1; 8],
        };
        v.add_relay(relay);
        let mut relay_sends = 0;
        for ms in (0..6000).step_by(10) {
            relay_sends += v
                .poll(t + Duration::from_millis(ms))
                .iter()
                .filter(|o| o.to == relay)
                .count();
        }
        assert!(relay_sends >= 20, "{relay_sends}");
        assert!(v.finished(t + Duration::from_secs(6)));
    }

    #[test]
    fn probes_measure_rtt_without_revalidating() {
        let t = Instant::now();
        let (mut v, mut h) = pair(t);
        let target = PathTarget::Direct(sa("10.0.0.2:2000"));
        let probe = v.probe(target, t).unwrap();
        let ans = h
            .on_stun(&probe.bytes, PathTarget::Direct(sa("10.0.0.1:1000")), t)
            .unwrap();
        v.on_stun(&ans.bytes, target, t + Duration::from_millis(12));
        let ev: Vec<_> = v.drain_events().collect();
        assert!(
            matches!(ev[..], [CheckEvent::ProbeAnswered { rtt, .. }] if rtt == Duration::from_millis(12))
        );
    }

    #[test]
    fn checker_survives_garbage() {
        let t = Instant::now();
        let (mut v, mut h) = pair(t);
        let req = v.poll(t).remove(0);
        let mut x = 0xFEED_F00Du64;
        for _ in 0..20_000 {
            let mut b = req.bytes.clone();
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let i = (x as usize) % b.len();
            b[i] ^= (x >> 8) as u8 | 1;
            b.truncate((x as usize >> 16) % (b.len() + 1));
            let _ = h.on_stun(&b, PathTarget::Direct(sa("10.0.0.1:1000")), t);
            let _ = v.on_stun(&b, req.to, t);
        }
    }
}
