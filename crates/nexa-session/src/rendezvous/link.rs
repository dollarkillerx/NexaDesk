//! One session's network link (§4.1–§4.5): a UDP socket under a [`VirtualSocket`],
//! gathering, connectivity checks, relay binding, and the path manager.
//!
//! A driver task owns all of it. The QUIC endpoint is created at once on the
//! virtual socket, so its receive loop also delivers STUN and relay control
//! traffic to the driver before any path exists; QUIC is neither sent nor
//! accepted until a path is validated.
//!
//! Path changes: the viewer's [`PathManager`] decides and the session sends
//! `PathSwitch` over the authenticated control stream (see
//! [`Link::take_switches`]); the host follows only [`Link::apply_remote_switch`].
//! Before the first `PathSwitch` the host answers on whichever validated path
//! the viewer's QUIC packets last came from; STUN never moves the active path.
//!
//! Also here (§4.2, §4.5, §4.6, §4.7):
//! - one dual-stack socket, so IPv4 and IPv6 candidates pair in their own family;
//! - a port mapping (PCP / NAT-PMP / UPnP) as one more candidate, released on drop;
//! - the WSS relay: when the UDP relay never answers our bind, the same frames
//!   and token go over a WebSocket to the signaling server, as another path;
//! - network changes: rebind the relay, check every pair again, gather again,
//!   and hand the new candidates to the session for a `CandidateUpdate` on the
//!   control stream ([`Link::take_candidate_updates`], [`Link::apply_candidate_update`]);
//! - Tailscale paths are marked DERP when the LocalAPI says the peer is relayed.

use std::{
    collections::HashMap,
    io,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use nexa_net::{
    check::{
        CheckEvent, Checker, Credentials, Families, PathClass, PeerNat, RemoteCandidate,
        kind_of_unlisted,
    },
    gather::{local_candidates_for, nat_from_mappings, system_addresses},
    path::{CandKind, PathDecision, PathId, PathManager, Role},
    portmap::{PortMapConfig, PortMapping},
    stun::{StunClass, StunMessage, TransactionId, encode_binding_request},
    tailscale::TailnetMonitor,
    vsock::{Inbound, PathSink, PathTarget, STREAM_RELAY_ADDR, VirtualSocket, bind_media_socket},
};
use nexa_wire::signal::{
    BindRefusal, MAX_CANDIDATES, MAX_PWD, MAX_UFRAG, MIN_PWD, MIN_UFRAG, NatKind, RelayGrant,
    RelayKind, RelaySession, RelayToken, Rendezvous, WireCandKind, WireCandidate, relay_frame,
    valid_credential,
};
use quinn::{AsyncUdpSocket, Endpoint, EndpointConfig, Runtime, TokioRuntime};
use tokio::sync::{mpsc, oneshot, watch};

use super::{signal::SignalConfig, wss::StreamRelay};

/// §4.4 Gathering: 1 s, then go with what we have; later answers are trickled.
pub const GATHER_TIMEOUT: Duration = Duration::from_secs(1);
const GATHER_RETX: [Duration; 3] = [
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
];
const TICK: Duration = Duration::from_millis(10);
const RELAY_BIND_RETX: Duration = Duration::from_millis(250);
const RELAY_BIND_REFRESH: Duration = Duration::from_secs(20);
/// The relay address comes from the (untrusted) server: stop sending binds to it
/// if it never answers.
const RELAY_BIND_GIVE_UP: Duration = Duration::from_secs(10);
/// §5.3: on the relay, try punching again this often.
pub const REPUNCH_EVERY: Duration = Duration::from_secs(30);
/// Probe the active path when nothing has arrived on it for this long, so the
/// path manager's 1 s failover never fires on a quiet but healthy path (a
/// consent prompt can keep QUIC quiet for a minute).
const LIVENESS_PROBE_AFTER: Duration = Duration::from_millis(250);
const STUN_QUEUE: usize = 1024;
/// §4.6: our UDP relay bind went unanswered this long, so try the WebSocket.
pub const WSS_FALLBACK_AFTER: Duration = Duration::from_secs(1);
/// Network events come in bursts (an interface goes down, another comes up).
const NETWORK_SETTLE: Duration = Duration::from_millis(300);
/// How often Tailscale paths are checked for a move between direct and DERP.
const TAILNET_RECHECK: Duration = Duration::from_secs(2);
/// Each `CandidateUpdate` sets off up to 64 pairs of checks to addresses the
/// peer names: even an authenticated peer gets that at most once a second.
const PEER_UPDATE_EVERY: Duration = Duration::from_secs(1);

pub type SocketFactory =
    Arc<dyn Fn(SocketAddr) -> io::Result<Arc<dyn AsyncUdpSocket>> + Send + Sync>;

/// Which local addresses to offer.
#[derive(Clone)]
pub enum Interfaces {
    /// Enumerate this machine's interfaces (§4.2).
    System,
    /// Exactly these, e.g. in tests.
    Fixed(Vec<IpAddr>),
    /// Asked again at every gathering, e.g. a simulated machine that moves.
    Dynamic(Arc<dyn Fn() -> Vec<IpAddr> + Send + Sync>),
}

impl std::fmt::Debug for Interfaces {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Interfaces::System => write!(f, "System"),
            Interfaces::Fixed(ips) => f.debug_tuple("Fixed").field(ips).finish(),
            Interfaces::Dynamic(_) => write!(f, "Dynamic"),
        }
    }
}

/// Where port-mapping requests go (§4.2 Portmap).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PortMap {
    /// This machine's default gateways, looked up again after a network change.
    System,
    /// Exactly this (fake gateways in tests).
    Fixed(PortMapConfig),
}

impl PortMap {
    fn config(&self) -> PortMapConfig {
        match self {
            PortMap::System => PortMapConfig::system(),
            PortMap::Fixed(c) => c.clone(),
        }
    }
}

/// Bumped by whoever watches the network ([`system_network_changes`] for the real one).
pub type NetworkChanges = watch::Receiver<u64>;

#[derive(Clone)]
pub struct LinkOptions {
    /// Local address for the session socket. An unspecified IP means all
    /// interfaces (`[::]` is dual-stack); a specific one is also the only host
    /// candidate.
    pub bind: SocketAddr,
    pub interfaces: Interfaces,
    /// Offer no direct candidates and answer no direct checks: the peer only
    /// ever learns the relay's address (§9.4 "仅中继").
    pub relay_only: bool,
    /// Sockets come from here instead of the OS (the NAT simulator).
    pub socket_factory: Option<SocketFactory>,
    /// §5.3's 30 s; shorter in tests.
    pub repunch_every: Duration,
    /// The signaling server, for the WSS relay fallback (§4.6). `dial` fills it in.
    pub wss_relay: Option<SignalConfig>,
    /// Off unless set: a mapping changes the user's router.
    pub portmap: Option<PortMap>,
    /// Off unless set.
    pub network_changes: Option<NetworkChanges>,
    /// Tailscale's view of its peers, for direct vs DERP (§4.7). Off unless set.
    pub tailnet: Option<Arc<TailnetMonitor>>,
}

impl std::fmt::Debug for LinkOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkOptions")
            .field("bind", &self.bind)
            .field("interfaces", &self.interfaces)
            .field("relay_only", &self.relay_only)
            .field("repunch_every", &self.repunch_every)
            .field("wss_relay", &self.wss_relay.as_ref().map(|c| &c.url))
            .field("portmap", &self.portmap)
            .field("network_changes", &self.network_changes.is_some())
            .field("tailnet", &self.tailnet.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for LinkOptions {
    /// All interfaces, dual-stack, and nothing that reaches beyond the peer and
    /// the signaling server: no port mapping, no network or Tailscale watching.
    fn default() -> Self {
        Self {
            bind: SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0)),
            interfaces: Interfaces::System,
            relay_only: false,
            socket_factory: None,
            repunch_every: REPUNCH_EVERY,
            wss_relay: None,
            portmap: None,
            network_changes: None,
            tailnet: None,
        }
    }
}

impl LinkOptions {
    /// What the shipped apps use: the defaults plus port mapping on this
    /// machine's gateway, network-change watching and the Tailscale LocalAPI.
    pub fn system() -> Self {
        Self {
            portmap: Some(PortMap::System),
            network_changes: Some(system_network_changes()),
            tailnet: Some(system_tailnet()),
            ..Self::default()
        }
    }
}

/// This machine's network changes, watched once per process.
pub fn system_network_changes() -> NetworkChanges {
    use std::sync::OnceLock;
    static WATCH: OnceLock<(watch::Sender<u64>, nexa_net::netwatch::NetWatcher)> = OnceLock::new();
    WATCH
        .get_or_init(|| {
            let (tx, _) = watch::channel(0u64);
            let bump = tx.clone();
            let watcher = nexa_net::netwatch::NetWatcher::start(Arc::new(move || {
                bump.send_modify(|v| *v += 1);
            }));
            (tx, watcher)
        })
        .0
        .subscribe()
}

/// tailscaled's status, polled once per process. Fails silently (§4.7).
pub fn system_tailnet() -> Arc<TailnetMonitor> {
    use std::sync::OnceLock;
    static MON: OnceLock<Arc<TailnetMonitor>> = OnceLock::new();
    MON.get_or_init(|| {
        TailnetMonitor::spawn(
            nexa_net::tailscale::LocalApi::System,
            Duration::from_secs(10),
        )
    })
    .clone()
}

/// A validated path as reported to the session.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PathInfo {
    pub id: PathId,
    pub target: PathTarget,
    pub kind: CandKind,
    pub class: PathClass,
    pub rtt: Option<Duration>,
    /// The peer's label for this path (`PathSwitch.path_id`).
    pub remote_tag: Option<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct LinkState {
    pub validated: Vec<PathInfo>,
    pub active: Option<PathInfo>,
    pub relay: RelayState,
    /// Checks are over and nothing was validated.
    pub failed: bool,
    pub switches: u32,
    /// Our relay seat is on the WebSocket fallback, not UDP.
    pub relay_over_wss: bool,
    /// Where the gateway forwards to us, if a port mapping was granted.
    pub port_mapping: Option<SocketAddr>,
    /// Local network changes handled so far.
    pub network_changes: u32,
    /// `CandidateUpdate`s received from the peer.
    pub peer_updates: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RelayState {
    #[default]
    None,
    Binding,
    Bound,
    Refused,
}

/// Our half of the rendezvous: what goes, sealed, to the peer.
#[derive(Clone, Debug)]
pub struct LocalOffer {
    pub creds: Credentials,
    pub candidates: Vec<WireCandidate>,
    pub nat: PeerNat,
    pub stun_note: Option<String>,
}

impl LocalOffer {
    pub fn to_wire(&self) -> Rendezvous {
        let (nat, port_delta) = match self.nat {
            PeerNat::Unknown => (NatKind::Unknown, 0),
            PeerNat::Easy => (NatKind::Easy, 0),
            PeerNat::Hard { port_delta } => (NatKind::Hard, port_delta),
        };
        Rendezvous {
            ufrag: self.creds.ufrag.clone(),
            pwd: self.creds.pwd.clone(),
            candidates: self.candidates.clone(),
            nat: nat as i32,
            port_delta,
        }
    }
}

/// The peer's half, validated: bad candidates are dropped, bad credentials
/// reject the whole thing.
#[derive(Clone, Debug)]
pub struct RemoteOffer {
    pub creds: Credentials,
    pub candidates: Vec<RemoteCandidate>,
    pub nat: PeerNat,
}

pub fn wire_kind(kind: CandKind) -> WireCandKind {
    match kind {
        CandKind::Host => WireCandKind::Host,
        CandKind::Ipv6 => WireCandKind::Ipv6,
        CandKind::Srflx => WireCandKind::Srflx,
        CandKind::Portmap => WireCandKind::Portmap,
        _ => WireCandKind::Tailnet,
    }
}

fn cand_kind(kind: WireCandKind) -> CandKind {
    match kind {
        WireCandKind::Host => CandKind::Host,
        WireCandKind::Ipv6 => CandKind::Ipv6,
        WireCandKind::Srflx => CandKind::Srflx,
        WireCandKind::Portmap => CandKind::Portmap,
        WireCandKind::Tailnet => CandKind::TailnetDirect,
    }
}

pub fn remote_candidates(list: &[WireCandidate]) -> Vec<RemoteCandidate> {
    list.iter()
        .take(MAX_CANDIDATES)
        .filter_map(|c| {
            let addr = c.addr().ok()?;
            let kind = cand_kind(c.checked_kind().ok()?);
            // The peer cannot vouch for the kind of an address: a Tailscale range is
            // Tailscale whatever it says, and the penalty follows the address.
            let kind = if nexa_net::check::is_tailnet(addr.ip()) {
                CandKind::TailnetDirect
            } else {
                kind
            };
            Some(RemoteCandidate { addr, kind })
        })
        .collect()
}

impl RemoteOffer {
    pub fn from_wire(r: &Rendezvous) -> Option<Self> {
        if !valid_credential(&r.ufrag, MIN_UFRAG, MAX_UFRAG)
            || !valid_credential(&r.pwd, MIN_PWD, MAX_PWD)
        {
            return None;
        }
        let nat = match NatKind::try_from(r.nat).unwrap_or(NatKind::Unknown) {
            NatKind::Unknown => PeerNat::Unknown,
            NatKind::Easy => PeerNat::Easy,
            NatKind::Hard => PeerNat::Hard {
                port_delta: r.port_delta.clamp(-65_535, 65_535),
            },
        };
        Some(Self {
            creds: Credentials {
                ufrag: r.ufrag.clone(),
                pwd: r.pwd.clone(),
            },
            candidates: remote_candidates(&r.candidates),
            nat,
        })
    }
}

enum Cmd {
    Gather {
        stun: Vec<SocketAddr>,
        reply: oneshot::Sender<LocalOffer>,
    },
    Relay(SocketAddr, RelaySession, Vec<u8>),
    Remote(RemoteOffer),
    Trickle(Vec<RemoteCandidate>),
    RemoteSwitch(u32),
    /// Viewer: the QUIC handshake is stuck; try the next validated path.
    Rotate,
    /// The peer's `CandidateUpdate`, from the authenticated control stream.
    PeerUpdate(Vec<RemoteCandidate>),
    /// The WebSocket relay came up (or did not).
    StreamRelay(Option<StreamRelay>),
    /// A port-mapping attempt finished.
    Mapped(Option<PortMapping>),
}

/// See the module docs. Dropping the last handle stops the driver.
pub struct Link {
    pub vsock: Arc<VirtualSocket>,
    pub endpoint: Endpoint,
    role: Role,
    families: Families,
    cmd: mpsc::UnboundedSender<Cmd>,
    state: watch::Receiver<LinkState>,
    switches: Mutex<Option<mpsc::UnboundedReceiver<u32>>>,
    late: Mutex<Option<mpsc::UnboundedReceiver<Vec<WireCandidate>>>>,
    updates: Mutex<Option<mpsc::UnboundedReceiver<Vec<WireCandidate>>>>,
    /// Shared with the driver so that dropping the link releases it.
    mapping: Arc<Mutex<Option<PortMapping>>>,
    driver: tokio::task::JoinHandle<()>,
}

impl Drop for Link {
    fn drop(&mut self) {
        self.driver.abort();
        self.endpoint.close(0u32.into(), b"link closed");
        if let Some(m) = self
            .mapping
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            release_mapping(m);
        }
    }
}

/// Releases on a thread of its own: dropping a link must not block.
fn release_mapping(m: PortMapping) {
    let _ = std::thread::Builder::new()
        .name("portmap-release".into())
        .spawn(move || nexa_net::portmap::release(&m));
}

impl std::fmt::Debug for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Link")
            .field("role", &self.role)
            .field("state", &*self.state.borrow())
            .finish_non_exhaustive()
    }
}

fn os_random(buf: &mut [u8]) {
    use rand_core09::TryRngCore;
    rand_core09::OsRng
        .try_fill_bytes(buf)
        .expect("the OS random source failed");
}

impl Link {
    /// Binds the socket and starts the driver. `server` makes this the host's
    /// (QUIC server) side.
    pub fn bind(
        role: Role,
        opts: LinkOptions,
        server: Option<quinn::ServerConfig>,
    ) -> io::Result<Self> {
        let inner: Arc<dyn AsyncUdpSocket> = match &opts.socket_factory {
            Some(f) => f(opts.bind)?,
            None => {
                let std = match bind_media_socket(opts.bind) {
                    // No IPv6 on this machine: IPv4 only.
                    Err(_) if opts.bind.is_ipv6() && opts.bind.ip().is_unspecified() => {
                        bind_media_socket(SocketAddr::from(([0, 0, 0, 0], opts.bind.port())))?
                    }
                    r => r?,
                };
                TokioRuntime.wrap_udp_socket(std)?
            }
        };
        let local = inner.local_addr()?;
        let families = Families::of(local);
        let (stun_tx, stun_rx) = mpsc::channel::<(Inbound2, Vec<u8>)>(STUN_QUEUE);
        let sink: PathSink = Box::new(move |inbound| {
            let item = match inbound {
                Inbound::Stun(p, from) => (Inbound2::Stun(from), p.to_vec()),
                Inbound::RelayControl {
                    server,
                    kind,
                    session,
                    payload,
                } => (Inbound2::Relay(server, kind, session), payload.to_vec()),
            };
            // A full queue drops: checks retransmit.
            let _ = stun_tx.try_send(item);
        });
        let start = Instant::now();
        let clock: Box<dyn Fn() -> u64 + Send + Sync> =
            Box::new(move || start.elapsed().as_micros() as u64);
        let vsock = Arc::new(VirtualSocket::unrouted(inner, sink, clock));
        vsock.set_follow_source(role == Role::Host);
        if role == Role::Host {
            // §4.5's failover silence: the viewer has moved on, so answer where it is.
            vsock.set_follow_after_silence(Some(nexa_net::path::FAILOVER_SILENCE));
        }
        let endpoint = Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            server,
            vsock.clone(),
            Arc::new(TokioRuntime),
        )?;
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (state_tx, state_rx) = watch::channel(LinkState::default());
        let (switch_tx, switch_rx) = mpsc::unbounded_channel();
        let (late_tx, late_rx) = mpsc::unbounded_channel();
        let (update_tx, update_rx) = mpsc::unbounded_channel();
        let mapping: Arc<Mutex<Option<PortMapping>>> = Arc::default();
        let creds = Credentials::generate(&mut os_random);
        let network = opts.network_changes.clone();
        let driver = Driver {
            role,
            local,
            families,
            start,
            vsock: vsock.clone(),
            checker: Checker::new(role, creds, families, Box::new(os_random)),
            cmd: cmd_tx.downgrade(),
            updates: update_tx,
            mapping: mapping.clone(),
            mapping_pending: false,
            portmap_nonce: {
                let mut n = [0u8; 12];
                os_random(&mut n);
                n
            },
            mapped_external: None,
            stream_relay: None,
            pending_change: None,
            network_changes: 0,
            peer_updates: 0,
            pending_peer_update: None,
            last_peer_update: None,
            last_tailnet_check: None,
            last_offer: Vec::new(),
            opts,
            pm: PathManager::new(role),
            paths: HashMap::new(),
            by_target: HashMap::new(),
            next_id: 1,
            gathering: None,
            relay: None,
            state: state_tx,
            switches: switch_tx,
            late: late_tx,
            remote_switched: false,
            last_repunch: None,
            last_rx_seen: HashMap::new(),
            last_liveness: None,
            failed: false,
            remote_set: false,
            switches_done: 0,
        };
        let task = tokio::spawn(driver.run(stun_rx, cmd_rx, network));
        Ok(Self {
            vsock,
            endpoint,
            role,
            families,
            cmd: cmd_tx,
            state: state_rx,
            switches: Mutex::new(Some(switch_rx)),
            late: Mutex::new(Some(late_rx)),
            updates: Mutex::new(Some(update_rx)),
            mapping,
            driver: task,
        })
    }

    /// What QUIC must dial on [`endpoint`](Self::endpoint): the virtual peer,
    /// IPv4-mapped on a dual-stack socket. quinn maps it itself, so
    /// `VIRTUAL_PEER` works too.
    pub fn quic_peer(&self) -> SocketAddr {
        self.vsock.quic_peer()
    }

    /// Which address families the session socket carries.
    pub fn families(&self) -> Families {
        self.families
    }

    /// Candidates gathered again after a network change, for a
    /// `CandidateUpdate` on the authenticated control stream (§4.5).
    pub fn take_candidate_updates(&self) -> Option<mpsc::UnboundedReceiver<Vec<WireCandidate>>> {
        self.updates
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// The peer's `CandidateUpdate`. Only call with what arrived on the
    /// authenticated control stream; the candidates are still only checked.
    pub fn apply_candidate_update(&self, candidates: &[WireCandidate]) {
        let c = remote_candidates(candidates);
        let _ = self.cmd.send(Cmd::PeerUpdate(c));
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.vsock.local_addr()
    }

    /// Gathers candidates for at most [`GATHER_TIMEOUT`].
    pub async fn gather(&self, stun: &[SocketAddr]) -> io::Result<LocalOffer> {
        let (tx, rx) = oneshot::channel();
        let _ = self.cmd.send(Cmd::Gather {
            stun: stun.to_vec(),
            reply: tx,
        });
        rx.await
            .map_err(|_| io::Error::other("link stopped while gathering"))
    }

    /// Binds our seat on the relay. Invalid grants are ignored: no relay path.
    pub fn set_relay(&self, grant: &RelayGrant) -> bool {
        let Ok(server) = grant.relay.parse::<SocketAddr>() else {
            return false;
        };
        let Ok(token) = RelayToken::decode(&grant.token) else {
            return false;
        };
        // A relay we cannot reach over UDP is still worth binding: the
        // WebSocket fallback uses the same grant.
        if server.port() == 0 || server.ip().is_unspecified() || server.ip().is_multicast() {
            return false;
        }
        let _ = self
            .cmd
            .send(Cmd::Relay(server, token.session, grant.token.clone()));
        true
    }

    /// The peer's offer or answer: checks start.
    pub fn start_checks(&self, remote: RemoteOffer) {
        let _ = self.cmd.send(Cmd::Remote(remote));
    }

    pub fn add_remote_candidates(&self, c: Vec<RemoteCandidate>) {
        if !c.is_empty() {
            let _ = self.cmd.send(Cmd::Trickle(c));
        }
    }

    pub fn state(&self) -> LinkState {
        self.state.borrow().clone()
    }

    pub fn watch(&self) -> watch::Receiver<LinkState> {
        self.state.clone()
    }

    /// Waits for the first validated path, or until checking has failed.
    pub async fn first_validated(&self, limit: Duration) -> Option<PathInfo> {
        let mut w = self.state.clone();
        let wait = w.wait_for(|s| s.active.is_some() || s.failed);
        match tokio::time::timeout(limit, wait).await {
            Ok(Ok(s)) => s.active,
            _ => None,
        }
    }

    /// Viewer: `PathSwitch.path_id`s to send on the control stream, in order.
    pub fn take_switches(&self) -> Option<mpsc::UnboundedReceiver<u32>> {
        self.switches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// Server-reflexive candidates that arrived after gathering ended.
    pub fn take_late_candidates(&self) -> Option<mpsc::UnboundedReceiver<Vec<WireCandidate>>> {
        self.late.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Host: a `PathSwitch` that arrived on the authenticated control stream.
    pub fn apply_remote_switch(&self, path_id: u32) {
        if self.role == Role::Host {
            let _ = self.cmd.send(Cmd::RemoteSwitch(path_id));
        }
    }

    /// Viewer: the QUIC handshake made no progress; move to the next path.
    pub fn rotate(&self) {
        if self.role == Role::Viewer {
            let _ = self.cmd.send(Cmd::Rotate);
        }
    }
}

enum Inbound2 {
    Stun(PathTarget),
    Relay(SocketAddr, RelayKind, RelaySession),
}

struct Gathering {
    servers: Vec<SocketAddr>,
    txids: Vec<TransactionId>,
    mapped: Vec<Option<SocketAddr>>,
    sends: usize,
    next: Instant,
    deadline: Instant,
    reply: Option<oneshot::Sender<LocalOffer>>,
    /// Gathering again after a network change: the result is a `CandidateUpdate`.
    update: bool,
    done: bool,
    host: Vec<(SocketAddr, CandKind)>,
    note: Option<String>,
}

struct RelayBinding {
    target: PathTarget,
    frame: Vec<u8>,
    state: RelayState,
    next: Instant,
    since: Instant,
    /// The WebSocket fallback was tried for this seat.
    tried_wss: bool,
}

struct PathEntry {
    info: PathInfo,
}

struct Driver {
    role: Role,
    opts: LinkOptions,
    local: SocketAddr,
    families: Families,
    start: Instant,
    vsock: Arc<VirtualSocket>,
    checker: Checker,
    pm: PathManager,
    paths: HashMap<PathId, PathEntry>,
    by_target: HashMap<PathTarget, PathId>,
    next_id: PathId,
    gathering: Option<Gathering>,
    relay: Option<RelayBinding>,
    state: watch::Sender<LinkState>,
    switches: mpsc::UnboundedSender<u32>,
    late: mpsc::UnboundedSender<Vec<WireCandidate>>,
    updates: mpsc::UnboundedSender<Vec<WireCandidate>>,
    /// For work that finishes off the driver (WebSocket, port mapping). Weak,
    /// so the driver does not keep its own command channel open.
    cmd: mpsc::WeakUnboundedSender<Cmd>,
    mapping: Arc<Mutex<Option<PortMapping>>>,
    mapping_pending: bool,
    /// One PCP mapping per socket (RFC 6887 §11.1): the same nonce every time.
    portmap_nonce: [u8; 12],
    mapped_external: Option<SocketAddr>,
    stream_relay: Option<StreamRelay>,
    pending_change: Option<Instant>,
    network_changes: u32,
    peer_updates: u32,
    pending_peer_update: Option<Vec<RemoteCandidate>>,
    last_peer_update: Option<Instant>,
    last_tailnet_check: Option<Instant>,
    /// What the peer has from us, to send an update only when it changed.
    last_offer: Vec<SocketAddr>,
    remote_switched: bool,
    last_repunch: Option<Instant>,
    last_rx_seen: HashMap<PathId, u64>,
    last_liveness: Option<Instant>,
    failed: bool,
    remote_set: bool,
    switches_done: u32,
}

async fn changed(rx: &mut Option<NetworkChanges>) {
    match rx {
        Some(r) => {
            if r.changed().await.is_err() {
                *rx = None;
            }
        }
        None => std::future::pending().await,
    }
}

impl Driver {
    async fn run(
        mut self,
        mut stun: mpsc::Receiver<(Inbound2, Vec<u8>)>,
        mut cmds: mpsc::UnboundedReceiver<Cmd>,
        mut network: Option<NetworkChanges>,
    ) {
        if let Some(n) = &mut network {
            n.mark_unchanged();
        }
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                Some((from, bytes)) = stun.recv() => {
                    let now = Instant::now();
                    match from {
                        Inbound2::Stun(target) => self.on_stun(&bytes, target, now),
                        Inbound2::Relay(server, kind, session) => self.on_relay(server, kind, session, &bytes, now),
                    }
                    self.after_events(now);
                }
                cmd = cmds.recv() => match cmd {
                    Some(cmd) => {
                        let now = Instant::now();
                        self.on_cmd(cmd, now);
                        self.after_events(now);
                    }
                    None => return,
                },
                _ = changed(&mut network) => {
                    let now = Instant::now();
                    self.pending_change.get_or_insert(now + NETWORK_SETTLE);
                }
                _ = tick.tick() => {
                    let now = Instant::now();
                    self.on_tick(now);
                    self.after_events(now);
                }
            }
        }
    }

    fn send(&self, to: &PathTarget, bytes: &[u8]) {
        let _ = self.vsock.send_raw(to, bytes);
    }

    fn on_cmd(&mut self, cmd: Cmd, now: Instant) {
        match cmd {
            Cmd::Gather { stun, reply } => self.start_gathering(stun, Some(reply), now),
            Cmd::Relay(server, session, token) => {
                self.vsock.add_relay_server(server);
                let target = PathTarget::Relay { server, session };
                self.relay = Some(RelayBinding {
                    target,
                    frame: relay_frame(RelayKind::Bind, &session, &token),
                    state: RelayState::Binding,
                    next: now,
                    since: now,
                    tried_wss: false,
                });
            }
            Cmd::Remote(r) => {
                if self.remote_set {
                    return;
                }
                self.remote_set = true;
                let cands = if self.opts.relay_only {
                    Vec::new()
                } else {
                    r.candidates
                };
                self.checker.set_remote(r.creds, &cands, r.nat, now);
                if let Some(rb) = &self.relay
                    && rb.state == RelayState::Bound
                {
                    self.checker.add_relay(rb.target);
                }
            }
            Cmd::Trickle(c) => {
                if !self.opts.relay_only {
                    self.checker.add_candidates(&c);
                }
            }
            Cmd::PeerUpdate(c) => {
                if !self.opts.relay_only && !c.is_empty() {
                    // The latest one wins; applied at most once per PEER_UPDATE_EVERY.
                    self.pending_peer_update = Some(c);
                    self.apply_peer_update(now);
                }
            }
            Cmd::StreamRelay(relay) => self.on_stream_relay(relay, now),
            Cmd::Mapped(m) => self.on_mapped(m, now),
            Cmd::RemoteSwitch(tag) => {
                let Some(target) = self.checker.target_of_tag(tag) else {
                    return;
                };
                let Some(&id) = self.by_target.get(&target) else {
                    return;
                };
                if self.pm.apply_remote_switch(id, now) {
                    self.remote_switched = true;
                    self.vsock.set_follow_source(false);
                    self.activate(id);
                }
            }
            Cmd::Rotate => {
                // The stuck path is dropped from consideration; the manager fails
                // over to the best remaining one, as if it had gone silent.
                let Some(current) = self.pm.active() else {
                    return;
                };
                if self.paths.len() < 2 {
                    return;
                }
                self.pm.remove_path(current);
                if let Some(p) = self.paths.remove(&current) {
                    self.by_target.remove(&p.info.target);
                }
                if let PathDecision::Switch { to, .. } = self.pm.poll(now).decision {
                    self.switch_viewer(to);
                }
            }
        }
    }

    /// Host candidates: the bound address, or every interface address.
    fn host_candidates(&self) -> Vec<(SocketAddr, CandKind)> {
        if self.opts.relay_only {
            return Vec::new();
        }
        if !self.opts.bind.ip().is_unspecified() && !self.local.ip().is_unspecified() {
            let kind =
                nexa_net::gather::classify_local("", self.local.ip()).unwrap_or(CandKind::Host);
            return vec![(self.local, kind)];
        }
        let addrs: Vec<(String, IpAddr)> = match &self.opts.interfaces {
            Interfaces::System => system_addresses(),
            Interfaces::Fixed(ips) => ips.iter().map(|ip| (String::new(), *ip)).collect(),
            Interfaces::Dynamic(f) => f().into_iter().map(|ip| (String::new(), ip)).collect(),
        };
        local_candidates_for(&addrs, self.local.port(), self.families)
    }

    fn start_gathering(
        &mut self,
        stun: Vec<SocketAddr>,
        reply: Option<oneshot::Sender<LocalOffer>>,
        now: Instant,
    ) {
        let host = self.host_candidates();
        let servers: Vec<SocketAddr> = if self.opts.relay_only {
            Vec::new()
        } else {
            // Two per family: §4.2 needs two answers to tell the NAT's mapping.
            let v4 = stun
                .iter()
                .filter(|s| s.is_ipv4() && self.families.v4)
                .take(2);
            let v6 = stun
                .iter()
                .filter(|s| s.is_ipv6() && self.families.v6)
                .take(2);
            v4.chain(v6).copied().collect()
        };
        let note = match servers.iter().filter(|s| s.is_ipv4()).count() {
            0 if servers.is_empty() => Some("no STUN server".to_owned()),
            0 | 1 => Some("one STUN server: NAT type not measured".to_owned()),
            _ => None,
        };
        let mut txids = Vec::new();
        for _ in &servers {
            let mut id = [0u8; 12];
            os_random(&mut id);
            txids.push(TransactionId(id));
        }
        let update = reply.is_none();
        self.gathering = Some(Gathering {
            mapped: vec![None; servers.len()],
            servers,
            txids,
            sends: 0,
            next: now,
            deadline: now + GATHER_TIMEOUT,
            reply,
            update,
            done: false,
            host,
            note,
        });
        self.start_mapping();
        self.gather_tick(now);
    }

    /// §4.2 Portmap, off the driver: at most 800 ms of blocking I/O.
    fn start_mapping(&mut self) {
        let Some(pm) = self.opts.portmap.clone() else {
            return;
        };
        if self.opts.relay_only || self.mapping_pending || !self.families.v4 {
            return;
        }
        let Some(cmd) = self.cmd.upgrade() else {
            return;
        };
        self.mapping_pending = true;
        let port = self.local.port();
        let nonce = self.portmap_nonce;
        tokio::task::spawn_blocking(move || {
            let m = nexa_net::portmap::map_udp(&pm.config(), port, nonce);
            let _ = cmd.send(Cmd::Mapped(m));
        });
    }

    fn on_mapped(&mut self, m: Option<PortMapping>, now: Instant) {
        self.mapping_pending = false;
        let old = {
            let mut slot = self.mapping.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::replace(&mut *slot, m.clone())
        };
        // Asking the same gateway again renewed the same mapping; only one on
        // a gateway we have left (or a different method) is released.
        if let Some(old) = old
            && m.as_ref()
                .is_none_or(|n| n.protocol != old.protocol || n.server() != old.server())
        {
            release_mapping(old);
        }
        self.mapped_external = m.map(|m| m.external);
        if self.gathering.as_ref().is_some_and(|g| g.done) {
            // Gathering is over: the mapping goes out late, like a slow STUN answer.
            self.send_late(now);
        }
        self.publish();
    }

    fn gather_tick(&mut self, now: Instant) {
        let Some(g) = &mut self.gathering else { return };
        let all = g.mapped.iter().all(Option::is_some) && !self.mapping_pending;
        if !g.done && (all || now >= g.deadline) {
            g.done = true;
            let offer = Self::offer_of(g, &self.checker, self.mapped_external);
            if let Some(r) = g.reply.take() {
                let _ = r.send(offer.clone());
            }
            let update = g.update;
            self.last_offer = offer_addrs(&offer.candidates);
            if update && !self.opts.relay_only {
                let _ = self.updates.send(offer.candidates);
            }
        }
        let Some(g) = &mut self.gathering else { return };
        let all = g.mapped.iter().all(Option::is_some);
        if all || g.sends > GATHER_RETX.len() || now < g.next {
            return;
        }
        for (i, server) in g.servers.iter().enumerate() {
            if g.mapped[i].is_none()
                && let Ok(req) = encode_binding_request(&g.txids[i], None, None)
            {
                let _ = self.vsock.send_to(*server, &req);
            }
        }
        g.next = now + GATHER_RETX.get(g.sends).copied().unwrap_or(GATHER_RETX[2]);
        g.sends += 1;
    }

    /// Something arrived after gathering ended: a trickle during the rendezvous,
    /// a `CandidateUpdate` after a network change.
    fn send_late(&mut self, _now: Instant) {
        let Some(g) = &self.gathering else { return };
        let offer = Self::offer_of(g, &self.checker, self.mapped_external);
        let addrs = offer_addrs(&offer.candidates);
        if addrs == self.last_offer || self.opts.relay_only {
            return;
        }
        let new: Vec<WireCandidate> = offer
            .candidates
            .iter()
            .filter(|c| c.addr().is_ok_and(|a| !self.last_offer.contains(&a)))
            .cloned()
            .collect();
        self.last_offer = addrs;
        if g.update {
            let _ = self.updates.send(offer.candidates);
        } else if !new.is_empty() {
            let _ = self.late.send(new);
        }
    }

    fn offer_of(g: &Gathering, checker: &Checker, mapped_port: Option<SocketAddr>) -> LocalOffer {
        let mut candidates: Vec<WireCandidate> = Vec::new();
        let mut foundation = 1;
        for (a, kind) in &g.host {
            candidates.push(WireCandidate::new(*a, wire_kind(*kind), foundation));
            foundation += 1;
        }
        if let Some(m) = mapped_port
            && !candidates.iter().any(|c| c.addr().ok() == Some(m))
        {
            candidates.push(WireCandidate::new(m, WireCandKind::Portmap, foundation));
            foundation += 1;
        }
        for m in g.mapped.iter().flatten() {
            if !candidates.iter().any(|c| c.addr().ok() == Some(*m)) {
                candidates.push(WireCandidate::new(*m, WireCandKind::Srflx, foundation));
                foundation += 1;
            }
        }
        candidates.truncate(MAX_CANDIDATES);
        let mapped: Vec<SocketAddr> = g
            .mapped
            .iter()
            .zip(&g.servers)
            .filter(|(_, s)| s.is_ipv4())
            .filter_map(|(m, _)| *m)
            .collect();
        let v4_servers = g.servers.iter().filter(|s| s.is_ipv4()).count();
        let nat = if v4_servers >= 2 && mapped.len() == v4_servers {
            nat_from_mappings(&mapped)
        } else {
            PeerNat::Unknown
        };
        LocalOffer {
            creds: checker.local_credentials().clone(),
            candidates,
            nat,
            stun_note: g.note.clone(),
        }
    }

    fn on_stun(&mut self, bytes: &[u8], from: PathTarget, now: Instant) {
        // Gathering answers come straight from a STUN server.
        if let (Some(g), PathTarget::Direct(addr)) = (&mut self.gathering, from)
            && let Some(i) = g.servers.iter().position(|s| *s == addr)
            && let Ok(m) = StunMessage::parse(bytes)
            && m.class == StunClass::Success
            && m.txid == g.txids[i]
        {
            if g.mapped[i].is_none()
                && let Some(mapped) = m.mapped.filter(|a| a.is_ipv4() == addr.is_ipv4())
            {
                g.mapped[i] = Some(mapped);
                if g.done {
                    self.send_late(now);
                }
            }
            return;
        }
        if self.opts.relay_only && !from.is_relay() {
            return;
        }
        if let Some(reply) = self.checker.on_stun(bytes, from, now) {
            self.send(&reply.to, &reply.bytes);
        }
    }

    /// §4.6: our UDP relay bind is going unanswered; try the WebSocket.
    fn maybe_open_stream_relay(&mut self, now: Instant) {
        let Some(cfg) = self.opts.wss_relay.clone() else {
            return;
        };
        let Some(rb) = &mut self.relay else { return };
        if rb.tried_wss
            || rb.state != RelayState::Binding
            || rb.target.is_stream_relay()
            || now.saturating_duration_since(rb.since) < WSS_FALLBACK_AFTER
        {
            return;
        }
        let Some(cmd) = self.cmd.upgrade() else {
            return;
        };
        rb.tried_wss = true;
        let vsock = self.vsock.clone();
        tokio::spawn(async move {
            let relay = super::wss::open(&cfg, vsock).await.ok();
            let _ = cmd.send(Cmd::StreamRelay(relay));
        });
    }

    fn on_stream_relay(&mut self, relay: Option<StreamRelay>, now: Instant) {
        let Some(relay) = relay else { return };
        let Some(rb) = &mut self.relay else { return };
        if rb.state == RelayState::Bound {
            // UDP made it after all; keep it.
            return;
        }
        let PathTarget::Relay { session, .. } = rb.target else {
            return;
        };
        rb.target = PathTarget::Relay {
            server: STREAM_RELAY_ADDR,
            session,
        };
        rb.state = RelayState::Binding;
        rb.next = now;
        rb.since = now;
        self.stream_relay = Some(relay);
        self.publish();
    }

    /// §4.5: the local network changed.
    fn on_network_change(&mut self, now: Instant) {
        self.network_changes += 1;
        // The relay must learn our new address before it can forward to us.
        if let Some(rb) = &mut self.relay
            && matches!(rb.state, RelayState::Binding | RelayState::Bound)
        {
            rb.next = now;
        }
        self.checker.restart(now);
        if self.remote_set {
            let stun: Vec<SocketAddr> = self
                .gathering
                .as_ref()
                .map(|g| g.servers.clone())
                .unwrap_or_default();
            self.start_gathering(stun, None, now);
        }
        self.publish();
    }

    fn apply_peer_update(&mut self, now: Instant) {
        if self
            .last_peer_update
            .is_some_and(|t| now.saturating_duration_since(t) < PEER_UPDATE_EVERY)
        {
            return;
        }
        let Some(c) = self.pending_peer_update.take() else {
            return;
        };
        self.last_peer_update = Some(now);
        self.peer_updates += 1;
        self.checker.add_update(&c, now);
        self.publish();
    }

    /// §4.7: a Tailscale path is DERP when the LocalAPI says the peer is relayed.
    fn tailnet_kind(&self, ip: IpAddr) -> CandKind {
        match self.opts.tailnet.as_ref().and_then(|t| t.direct(ip)) {
            Some(false) => CandKind::TailnetDerp,
            _ => CandKind::TailnetDirect,
        }
    }

    fn recheck_tailnet(&mut self, now: Instant) {
        if self.opts.tailnet.is_none()
            || self
                .last_tailnet_check
                .is_some_and(|t| now.saturating_duration_since(t) < TAILNET_RECHECK)
        {
            return;
        }
        self.last_tailnet_check = Some(now);
        let mut changed = false;
        let ids: Vec<PathId> = self.paths.keys().copied().collect();
        for id in ids {
            let Some(p) = self.paths.get(&id) else {
                continue;
            };
            let PathTarget::Direct(a) = p.info.target else {
                continue;
            };
            if !matches!(p.info.kind, CandKind::TailnetDirect | CandKind::TailnetDerp) {
                continue;
            }
            let kind = self.tailnet_kind(a.ip());
            if let Some(p) = self.paths.get_mut(&id)
                && p.info.kind != kind
            {
                p.info.kind = kind;
                p.info.class = PathClass::of(kind);
                self.pm.set_kind(id, kind);
                changed = true;
            }
        }
        if changed {
            self.publish();
        }
    }

    fn on_relay(
        &mut self,
        server: SocketAddr,
        kind: RelayKind,
        session: RelaySession,
        payload: &[u8],
        now: Instant,
    ) {
        let Some(rb) = &mut self.relay else { return };
        if rb.target != (PathTarget::Relay { server, session }) {
            return;
        }
        match kind {
            RelayKind::BindOk => {
                let first = rb.state != RelayState::Bound;
                rb.state = RelayState::Bound;
                rb.next = now + RELAY_BIND_REFRESH;
                if first && self.remote_set {
                    let t = rb.target;
                    self.checker.add_relay(t);
                }
            }
            RelayKind::BindRefused => {
                // Refusals are unauthenticated: anyone can spoof the relay's address.
                // Keep trying until the token itself is the problem.
                let why = payload.first().copied().and_then(BindRefusal::from_byte);
                if matches!(why, Some(BindRefusal::BadToken | BindRefusal::Expired))
                    && rb.state != RelayState::Bound
                {
                    rb.state = RelayState::Refused;
                }
            }
            _ => {}
        }
    }

    fn on_tick(&mut self, now: Instant) {
        if self.pending_change.is_some_and(|t| now >= t) {
            self.pending_change = None;
            self.on_network_change(now);
        }
        self.gather_tick(now);
        self.maybe_open_stream_relay(now);
        self.recheck_tailnet(now);
        self.apply_peer_update(now);
        if let Some(rb) = &mut self.relay
            && rb.state == RelayState::Binding
            && now.saturating_duration_since(rb.since) >= RELAY_BIND_GIVE_UP
        {
            rb.state = RelayState::Refused;
        }
        if let Some(rb) = &mut self.relay
            && matches!(rb.state, RelayState::Binding | RelayState::Bound)
            && now >= rb.next
        {
            let _ = self.vsock.send_to(rb.target.wire_addr(), &rb.frame);
            rb.next = now
                + if rb.state == RelayState::Bound {
                    RELAY_BIND_REFRESH
                } else {
                    RELAY_BIND_RETX
                };
        }
        for o in self.checker.poll(now) {
            self.send(&o.to, &o.bytes);
        }

        // QUIC traffic per path is liveness for the path manager.
        for (&id, p) in &self.paths {
            if let Some(t) = self.vsock.path_last_rx_us(&p.info.target) {
                let seen = self.last_rx_seen.entry(id).or_insert(0);
                // 0 means no QUIC yet on this path.
                if t > *seen {
                    *seen = t;
                    let at = self.start + Duration::from_micros(t);
                    self.pm.on_inbound(id, at.min(now));
                }
            }
        }

        if !self.paths.is_empty() {
            let out = self.pm.poll(now);
            for p in out.probes {
                if let Some(e) = self.paths.get(&p.path) {
                    // Probes measure, keepalives hold the NAT binding; both are a Binding request.
                    if let Some(o) = self.checker.probe(e.info.target, now) {
                        self.send(&o.to, &o.bytes);
                    }
                }
            }
            match out.decision {
                PathDecision::Switch { to, .. } if self.role == Role::Viewer => {
                    self.switch_viewer(to)
                }
                _ => {}
            }
            self.liveness_probe(now);
        }

        // The host's socket may have followed the viewer's QUIC onto another path.
        if self.vsock.active_path() != self.state.borrow().active.map(|a| a.target) {
            self.publish();
        }

        // §5.3: on the relay, retry punching every 30 s.
        let on_relay = self
            .pm
            .active()
            .and_then(|id| self.paths.get(&id))
            .is_some_and(|p| p.info.class == PathClass::Relay);
        if on_relay && !self.opts.relay_only {
            let every = self.opts.repunch_every;
            let due = self
                .last_repunch
                .is_none_or(|t| now.saturating_duration_since(t) >= every);
            if due {
                if self.last_repunch.is_some() {
                    self.checker.retry_failed(now);
                }
                self.last_repunch = Some(now);
            }
        }

        // A relay seat still being bound (over UDP, or the WebSocket still to come)
        // is a path that may yet work: not failed until it gives up too.
        let relay_pending = self
            .relay
            .as_ref()
            .is_some_and(|r| r.state == RelayState::Binding);
        if !self.failed && self.paths.is_empty() && !relay_pending && self.checker.finished(now) {
            self.failed = true;
            self.publish();
        }
    }

    fn liveness_probe(&mut self, now: Instant) {
        let Some(id) = self.pm.active() else { return };
        let Some(p) = self.paths.get(&id) else { return };
        let target = p.info.target;
        let last = self
            .vsock
            .path_last_rx_us(&target)
            .map(|t| self.start + Duration::from_micros(t));
        let quiet = last.is_none_or(|t| now.saturating_duration_since(t) >= LIVENESS_PROBE_AFTER);
        let due = self
            .last_liveness
            .is_none_or(|t| now.saturating_duration_since(t) >= LIVENESS_PROBE_AFTER);
        if quiet && due {
            self.last_liveness = Some(now);
            if let Some(o) = self.checker.probe(target, now) {
                self.send(&o.to, &o.bytes);
            }
        }
    }

    fn after_events(&mut self, now: Instant) {
        let events: Vec<CheckEvent> = self.checker.drain_events().collect();
        let mut changed = false;
        for e in events {
            match e {
                CheckEvent::Validated {
                    target,
                    kind,
                    rtt,
                    remote_tag,
                    ..
                } => {
                    changed |= self.on_validated(target, kind, rtt, remote_tag, now);
                }
                CheckEvent::ProbeAnswered {
                    target,
                    rtt,
                    remote_tag,
                } => {
                    if let Some(&id) = self.by_target.get(&target) {
                        self.pm.on_rtt(id, rtt, now);
                        if let Some(p) = self.paths.get_mut(&id) {
                            p.info.rtt = Some(rtt);
                        }
                        self.update_remote_tag(id, remote_tag);
                        changed = true;
                    }
                }
                CheckEvent::PeerSpoke { target } => {
                    if let Some(&id) = self.by_target.get(&target) {
                        self.pm.on_inbound(id, now);
                    }
                }
            }
        }
        if changed {
            self.publish();
        }
    }

    fn on_validated(
        &mut self,
        target: PathTarget,
        kind: CandKind,
        rtt: Option<Duration>,
        remote_tag: Option<u32>,
        now: Instant,
    ) -> bool {
        if self.opts.relay_only && !target.is_relay() {
            return false;
        }
        self.vsock.validate_path(target);
        if let Some(&id) = self.by_target.get(&target) {
            if let Some(p) = self.paths.get_mut(&id)
                && rtt.is_some()
            {
                p.info.rtt = rtt;
            }
            self.update_remote_tag(id, remote_tag);
            if let Some(r) = rtt {
                self.pm.on_rtt(id, r, now);
            }
            return true;
        }
        let kind = match target {
            PathTarget::Relay { server, .. } if server == STREAM_RELAY_ADDR => CandKind::WssRelay,
            PathTarget::Relay { .. } => CandKind::Relay,
            PathTarget::Direct(a) if nexa_net::check::is_tailnet(a.ip()) => {
                self.tailnet_kind(a.ip())
            }
            PathTarget::Direct(_) => kind,
        };
        // The host names paths by its own tag, so a PathSwitch maps straight to an id.
        let id = match self.role {
            Role::Host => self.checker.tag_of(&target).unwrap_or_else(|| {
                let id = self.next_id | 0x8000_0000;
                self.next_id += 1;
                id
            }),
            Role::Viewer => {
                let id = self.next_id;
                self.next_id += 1;
                id
            }
        };
        let info = PathInfo {
            id,
            target,
            kind,
            class: PathClass::of(kind),
            rtt,
            remote_tag,
        };
        self.paths.insert(id, PathEntry { info });
        self.by_target.insert(target, id);
        self.pm.add_path(id, kind, now);
        if let Some(r) = rtt {
            self.pm.on_rtt(id, r, now);
        }
        if self.vsock.active_path().is_none() {
            // §4.4: the first validated path starts QUIC at once.
            match self.role {
                Role::Viewer => {
                    if let PathDecision::Switch { to, .. } = self.pm.poll(now).decision {
                        self.switch_viewer(to);
                    }
                }
                Role::Host => self.activate(id),
            }
        }
        true
    }

    /// The peer labels a path by where our packets come from, so after our
    /// address changes the same target gets a new label: the host must hear it
    /// again or it keeps answering on the old path.
    fn update_remote_tag(&mut self, id: PathId, tag: Option<u32>) {
        let Some(tag) = tag else { return };
        let Some(p) = self.paths.get_mut(&id) else {
            return;
        };
        let changed = p.info.remote_tag.is_some_and(|old| old != tag);
        p.info.remote_tag = Some(tag);
        if changed && self.role == Role::Viewer && self.pm.active() == Some(id) {
            let _ = self.switches.send(tag);
        }
    }

    fn activate(&mut self, id: PathId) {
        if let Some(p) = self.paths.get(&id) {
            self.vsock.switch_to(p.info.target);
            self.switches_done += 1;
            self.publish();
        }
    }

    /// Viewer: switch locally and tell the host over the control stream.
    fn switch_viewer(&mut self, id: PathId) {
        let Some(p) = self.paths.get(&id) else { return };
        let (target, tag) = (p.info.target, p.info.remote_tag);
        if self.vsock.active_path() != Some(target) {
            self.vsock.switch_to(target);
            self.switches_done += 1;
        }
        if let Some(tag) = tag {
            let _ = self.switches.send(tag);
        }
        self.publish();
    }

    fn publish(&self) {
        let active_target = self.vsock.active_path();
        let mut validated: Vec<PathInfo> = self.paths.values().map(|p| p.info).collect();
        validated.sort_by_key(|p| p.id);
        let active = validated
            .iter()
            .find(|p| Some(p.target) == active_target)
            .copied();
        let relay = self.relay.as_ref().map_or(RelayState::None, |r| r.state);
        let relay_over_wss = self
            .relay
            .as_ref()
            .is_some_and(|r| r.target.is_stream_relay());
        self.state.send_replace(LinkState {
            validated,
            active,
            relay,
            failed: self.failed,
            switches: self.switches_done,
            relay_over_wss,
            port_mapping: self.mapped_external,
            network_changes: self.network_changes,
            peer_updates: self.peer_updates,
        });
    }
}

fn offer_addrs(c: &[WireCandidate]) -> Vec<SocketAddr> {
    c.iter().filter_map(|c| c.addr().ok()).collect()
}

/// How a path is described for diagnostics.
pub fn describe(info: &PathInfo) -> String {
    match (info.target, info.kind) {
        (PathTarget::Direct(a), CandKind::TailnetDerp) => format!("{a} (DERP)"),
        (PathTarget::Direct(a), _) => a.to_string(),
        (t, _) => t.to_string(),
    }
}

/// Kind of an address seen without the peer's word for it.
pub fn kind_of(addr: SocketAddr) -> CandKind {
    kind_of_unlisted(addr)
}
