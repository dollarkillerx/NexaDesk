//! Virtual path socket (§4.1): QUIC sees one fixed peer address; real paths live below it.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt,
    io::{self, IoSliceMut},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket},
    pin::Pin,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
    },
    task::{Context, Poll, Waker},
};

use arc_swap::ArcSwap;
use nexa_wire::signal::{
    RELAY_HEADER_LEN, RelayKind, RelaySession, is_relay_frame, parse_relay_frame, relay_frame,
    write_relay_header,
};
use quinn::{
    AsyncUdpSocket, UdpPoller,
    udp::{RecvMeta, Transmit},
};

pub const VIRTUAL_PEER: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 255, 255, 1)), 1);

pub const SOCKET_BUFFER: usize = 4 << 20;

/// The "address" of the stream relay (§4.6 WSS fallback). No UDP packet can
/// come from it, so it tells frames read from the WebSocket apart.
pub const STREAM_RELAY_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);

/// Creates a UDP socket with the enlarged buffers from §4.1. An unspecified
/// IPv6 address (`[::]`) makes a dual-stack socket that also carries IPv4.
pub fn bind_media_socket(addr: SocketAddr) -> io::Result<UdpSocket> {
    let s = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    if let SocketAddr::V6(v6) = addr {
        s.set_only_v6(!v6.ip().is_unspecified())?;
    }
    s.set_send_buffer_size(SOCKET_BUFFER)?;
    s.set_recv_buffer_size(SOCKET_BUFFER)?;
    s.bind(&addr.into())?;
    Ok(s.into())
}

/// A dual-stack socket on `port` when this machine has IPv6, else IPv4 only.
pub fn bind_dual_stack(port: u16) -> io::Result<UdpSocket> {
    bind_media_socket(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port))
        .or_else(|_| bind_media_socket(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port)))
}

/// IPv4-mapped IPv6 (how a dual-stack socket reports IPv4 peers) back to IPv4.
pub fn canonical(a: SocketAddr) -> SocketAddr {
    SocketAddr::new(a.ip().to_canonical(), a.port())
}

fn mapped(a: SocketAddr) -> SocketAddr {
    match a {
        SocketAddr::V4(v4) => SocketAddr::new(IpAddr::V6(v4.ip().to_ipv6_mapped()), v4.port()),
        v6 => v6,
    }
}

/// A relay reached over a byte stream instead of UDP (§4.6: the WSS fallback).
/// Frames are relay frames exactly as on UDP; whoever owns the stream calls
/// [`deliver`](Self::deliver) for each one read and drains what `send` is given.
pub struct StreamPipe {
    inbox: Mutex<(VecDeque<Vec<u8>>, Option<Waker>)>,
    /// Queues one frame for the stream. False when full or closed: dropped, as UDP would.
    send: Box<dyn Fn(Vec<u8>) -> bool + Send + Sync>,
}

impl fmt::Debug for StreamPipe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamPipe").finish_non_exhaustive()
    }
}

/// Frames read from the stream but not yet taken by QUIC; more are dropped.
const STREAM_INBOX: usize = 2048;

impl StreamPipe {
    pub fn new(send: Box<dyn Fn(Vec<u8>) -> bool + Send + Sync>) -> Arc<Self> {
        Arc::new(Self {
            inbox: Mutex::new((VecDeque::new(), None)),
            send,
        })
    }

    /// One frame read from the stream.
    pub fn deliver(&self, frame: Vec<u8>) {
        let mut st = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
        if st.0.len() >= STREAM_INBOX {
            return;
        }
        st.0.push_back(frame);
        if let Some(w) = st.1.take() {
            w.wake();
        }
    }
}

const STUN_MAGIC_COOKIE: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];
const STUN_HEADER_LEN: usize = 20;

/// Recognizes STUN by its first bits *and* magic cookie. The first byte alone is not
/// enough: quinn greases the QUIC fixed bit (RFC 9287), so about 3% of short-header
/// packets also start with 0x00-0x03.
pub fn is_stun(packet: &[u8]) -> bool {
    packet.len() >= STUN_HEADER_LEN && packet[0] < 0x04 && packet[4..8] == STUN_MAGIC_COOKIE
}

/// Receives STUN packets for the path manager. Must not block.
pub type StunSink = Box<dyn Fn(&[u8], SocketAddr) + Send + Sync>;

/// Where a path's packets really go: straight to an address, or through a relay
/// session (§4.6), whose frames this socket wraps and unwraps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PathTarget {
    Direct(SocketAddr),
    Relay {
        server: SocketAddr,
        session: RelaySession,
    },
}

impl PathTarget {
    /// The UDP address packets for this path are sent to.
    pub fn wire_addr(&self) -> SocketAddr {
        match *self {
            PathTarget::Direct(a) => a,
            PathTarget::Relay { server, .. } => server,
        }
    }

    pub fn is_relay(&self) -> bool {
        matches!(self, PathTarget::Relay { .. })
    }

    /// A relay reached over the WebSocket fallback rather than UDP.
    pub fn is_stream_relay(&self) -> bool {
        matches!(self, PathTarget::Relay { server, .. } if *server == STREAM_RELAY_ADDR)
    }
}

impl From<SocketAddr> for PathTarget {
    fn from(a: SocketAddr) -> Self {
        PathTarget::Direct(a)
    }
}

impl fmt::Display for PathTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathTarget::Direct(a) => write!(f, "{a}"),
            PathTarget::Relay { server, .. } if *server == STREAM_RELAY_ADDR => {
                write!(f, "relay (WSS)")
            }
            PathTarget::Relay { server, .. } => write!(f, "relay {server}"),
        }
    }
}

/// What the socket hands to the path agent instead of QUIC.
#[derive(Debug)]
pub enum Inbound<'a> {
    /// A STUN message that arrived on `from`.
    Stun(&'a [u8], PathTarget),
    /// A relay control frame (bind answer) from a registered relay server.
    RelayControl {
        server: SocketAddr,
        kind: RelayKind,
        session: RelaySession,
        payload: &'a [u8],
    },
}

/// Receives everything that is not QUIC. Runs on the endpoint driver: must not block.
pub type PathSink = Box<dyn Fn(Inbound<'_>) + Send + Sync>;

pub struct VirtualSocket {
    inner: Arc<dyn AsyncUdpSocket>,
    /// `None` until the first path is chosen; until then QUIC output is dropped.
    active: ArcSwap<Option<PathTarget>>,
    /// Validated paths and, per path, the clock reading of its last QUIC packet.
    validated: RwLock<HashMap<PathTarget, Arc<AtomicU64>>>,
    /// Relay servers whose frames this socket unwraps.
    relays: RwLock<HashSet<SocketAddr>>,
    sink: PathSink,
    clock: Box<dyn Fn() -> u64 + Send + Sync>,
    last_rx_us: AtomicU64,
    dropped_unvalidated: AtomicU64,
    /// Until the first authenticated decision, answer on the validated path the
    /// peer's QUIC last arrived on (the host before its first `PathSwitch`).
    follow_source: AtomicBool,
    /// Also follow when the active path has carried no QUIC for this long
    /// (microseconds; 0 is off). The host's acknowledgements must reach the
    /// viewer for the viewer's `PathSwitch` to get through at all: after the
    /// viewer's address changes, the old path is dead, and QUIC cannot resend
    /// the stream data in front of the `PathSwitch` until something is acked.
    follow_after_silence_us: AtomicU64,
    /// The inner socket is IPv6 (dual-stack when unspecified): IPv4 destinations
    /// go out IPv4-mapped, and QUIC's fixed peer is the mapped virtual address,
    /// which is what quinn uses on an IPv6 endpoint.
    v6_socket: bool,
    quic_peer: SocketAddr,
    stream: RwLock<Option<Arc<StreamPipe>>>,
    /// The endpoint driver's waker from its last empty receive, so that a pipe
    /// attached later gets polled.
    recv_waker: Mutex<Option<Waker>>,
}

impl fmt::Debug for VirtualSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VirtualSocket")
            .field("active", &**self.active.load())
            .finish_non_exhaustive()
    }
}

impl VirtualSocket {
    /// A socket with one direct path, already validated and active.
    pub fn new(
        inner: Arc<dyn AsyncUdpSocket>,
        first_path: SocketAddr,
        stun: StunSink,
        clock: Box<dyn Fn() -> u64 + Send + Sync>,
    ) -> Self {
        let sink: PathSink = Box::new(move |inbound| {
            if let Inbound::Stun(p, from) = inbound {
                stun(p, from.wire_addr());
            }
        });
        let s = Self::unrouted(inner, sink, clock);
        s.switch_to(first_path);
        s
    }

    /// A socket with no path yet: QUIC is neither sent nor accepted until
    /// [`validate_path`](Self::validate_path) and [`switch_to`](Self::switch_to).
    /// Used when connectivity checks run on the same socket first (§4.4).
    pub fn unrouted(
        inner: Arc<dyn AsyncUdpSocket>,
        sink: PathSink,
        clock: Box<dyn Fn() -> u64 + Send + Sync>,
    ) -> Self {
        let now = clock();
        let v6_socket = inner.local_addr().is_ok_and(|a| a.is_ipv6());
        Self {
            inner,
            active: ArcSwap::from_pointee(None),
            validated: RwLock::new(HashMap::new()),
            relays: RwLock::new(HashSet::new()),
            sink,
            clock,
            last_rx_us: AtomicU64::new(now),
            dropped_unvalidated: AtomicU64::new(0),
            follow_source: AtomicBool::new(false),
            follow_after_silence_us: AtomicU64::new(0),
            v6_socket,
            quic_peer: if v6_socket {
                mapped(VIRTUAL_PEER)
            } else {
                VIRTUAL_PEER
            },
            stream: RwLock::new(None),
            recv_waker: Mutex::new(None),
        }
    }

    /// The address QUIC must dial: [`VIRTUAL_PEER`], IPv4-mapped on an IPv6 socket.
    pub fn quic_peer(&self) -> SocketAddr {
        self.quic_peer
    }

    /// Frames for [`STREAM_RELAY_ADDR`] go to this pipe, and its frames are
    /// relay frames from that address. `None` detaches it.
    pub fn set_stream_relay(&self, pipe: Option<Arc<StreamPipe>>) {
        if pipe.is_some() {
            self.add_relay_server(STREAM_RELAY_ADDR);
        }
        *self.stream.write().unwrap() = pipe;
        if let Some(w) = self.recv_waker.lock().unwrap().take() {
            w.wake();
        }
    }

    pub fn has_stream_relay(&self) -> bool {
        self.stream.read().unwrap().is_some()
    }

    /// Sends one datagram (or GSO batch) to a real address, or into the stream relay.
    fn raw_send(&self, t: &Transmit<'_>) -> io::Result<()> {
        if t.destination == STREAM_RELAY_ADDR {
            let pipe = self.stream.read().unwrap().clone();
            if let Some(pipe) = pipe {
                let seg = t.segment_size.unwrap_or(t.contents.len()).max(1);
                for chunk in t.contents.chunks(seg) {
                    (pipe.send)(chunk.to_vec());
                }
            }
            return Ok(());
        }
        if self.v6_socket && t.destination.is_ipv4() {
            return self.inner.try_send(&Transmit {
                destination: mapped(t.destination),
                ..*t
            });
        }
        if !self.v6_socket && t.destination.is_ipv6() {
            // An IPv4 socket has no route there: lost, like any datagram.
            return Ok(());
        }
        self.inner.try_send(t)
    }

    /// Takes queued stream-relay frames into `bufs`. Registers the waker when empty.
    fn take_stream(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> usize {
        let pipe = self.stream.read().unwrap().clone();
        let Some(pipe) = pipe else { return 0 };
        let mut st = pipe.inbox.lock().unwrap_or_else(|e| e.into_inner());
        let mut n = 0;
        while n < bufs.len().min(meta.len()) {
            let Some(frame) = st.0.pop_front() else { break };
            if frame.len() > bufs[n].len() {
                continue;
            }
            bufs[n][..frame.len()].copy_from_slice(&frame);
            meta[n] = RecvMeta {
                addr: STREAM_RELAY_ADDR,
                len: frame.len(),
                stride: frame.len(),
                ..Default::default()
            };
            n += 1;
        }
        if n == 0 {
            st.1 = Some(cx.waker().clone());
        }
        n
    }

    /// See the field. Only ever moves between already validated paths.
    pub fn set_follow_source(&self, on: bool) {
        self.follow_source.store(on, Relaxed);
    }

    /// See the field. Only ever moves between already validated paths, and
    /// only when the active one has gone quiet.
    pub fn set_follow_after_silence(&self, after: Option<std::time::Duration>) {
        let us = after.map_or(0, |d| (d.as_micros() as u64).max(1));
        self.follow_after_silence_us.store(us, Relaxed);
    }

    /// Frames from this address are relay frames and are unwrapped before anything else.
    pub fn add_relay_server(&self, server: SocketAddr) {
        self.relays.write().unwrap().insert(server);
    }

    /// Marks a path as passing connectivity checks. QUIC is only accepted from such sources.
    pub fn validate_path(&self, path: impl Into<PathTarget>) {
        self.validated
            .write()
            .unwrap()
            .entry(path.into())
            .or_insert_with(|| Arc::new(AtomicU64::new(0)));
    }

    pub fn forget_path(&self, path: impl Into<PathTarget>) {
        self.validated.write().unwrap().remove(&path.into());
    }

    pub fn is_validated(&self, path: &PathTarget) -> bool {
        self.validated.read().unwrap().contains_key(path)
    }

    /// Only call after an authenticated decision (`PathSwitch` over QUIC, §4.5), or
    /// for the very first path. The target becomes a validated path, since we now
    /// expect its replies.
    pub fn switch_to(&self, path: impl Into<PathTarget>) {
        let path = path.into();
        self.validate_path(path);
        self.active.store(Arc::new(Some(path)));
    }

    /// The UDP address QUIC packets go to now (a relay server for a relayed path).
    pub fn active(&self) -> SocketAddr {
        self.active_path()
            .map_or(SocketAddr::from(([0, 0, 0, 0], 0)), |p| p.wire_addr())
    }

    pub fn active_path(&self) -> Option<PathTarget> {
        **self.active.load()
    }

    pub fn last_rx_us(&self) -> u64 {
        self.last_rx_us.load(Relaxed)
    }

    /// Clock reading of the last QUIC packet accepted on `path`; 0 before any.
    pub fn path_last_rx_us(&self, path: &PathTarget) -> Option<u64> {
        self.validated
            .read()
            .unwrap()
            .get(path)
            .map(|t| t.load(Relaxed))
    }

    pub fn dropped_unvalidated(&self) -> u64 {
        self.dropped_unvalidated.load(Relaxed)
    }

    /// Sends a STUN message (or anything else that is not QUIC) on a path,
    /// framing it for a relay when the path is relayed. Best effort, like UDP.
    pub fn send_raw(&self, path: &PathTarget, payload: &[u8]) -> io::Result<()> {
        match *path {
            PathTarget::Direct(addr) => self.send_to(addr, payload),
            PathTarget::Relay { server, session } => {
                self.send_to(server, &relay_frame(RelayKind::Data, &session, payload))
            }
        }
    }

    /// Sends bytes as they are to `addr`: relay binds and gathering requests.
    pub fn send_to(&self, addr: SocketAddr, payload: &[u8]) -> io::Result<()> {
        let t = Transmit {
            destination: addr,
            ecn: None,
            contents: payload,
            segment_size: None,
            src_ip: None,
        };
        match self.raw_send(&t) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(()),
            r => r,
        }
    }

    /// Applies §4.1's receive rules in place, keeping accepted packets at the front.
    /// Returns how many remain.
    fn filter(&self, bufs: &mut [IoSliceMut<'_>], meta: &mut [RecvMeta], n: usize) -> usize {
        let mut kept = 0;
        let now = (self.clock)();
        let relays = self.relays.read().unwrap();
        for i in 0..n {
            // A GRO batch (Linux only; macOS has none) is treated as one datagram,
            // as before relays existed. Relayed paths never coalesce: each frame
            // arrives as its own datagram from the relay.
            let total = meta[i].len;
            let from = canonical(meta[i].addr);
            if from == STREAM_RELAY_ADDR && !is_relay_frame(&bufs[i][..total]) {
                continue;
            }
            let mut start = 0;
            let mut len = total;
            let mut path = PathTarget::Direct(from);
            if relays.contains(&from) && is_relay_frame(&bufs[i][..len]) {
                let Ok((kind, session, payload)) = parse_relay_frame(&bufs[i][..len]) else {
                    continue;
                };
                if kind != RelayKind::Data {
                    (self.sink)(Inbound::RelayControl {
                        server: from,
                        kind,
                        session,
                        payload,
                    });
                    continue;
                }
                path = PathTarget::Relay {
                    server: from,
                    session,
                };
                start = RELAY_HEADER_LEN;
                len = total - RELAY_HEADER_LEN;
            }
            if is_stun(&bufs[i][start..start + len]) {
                (self.sink)(Inbound::Stun(&bufs[i][start..start + len], path));
                continue;
            }
            let validated = self.validated.read().unwrap();
            match validated.get(&path) {
                Some(last) => {
                    last.store(now, Relaxed);
                    let active = **self.active.load();
                    if active != Some(path) {
                        let silence = self.follow_after_silence_us.load(Relaxed);
                        let active_quiet = silence > 0
                            && active
                                .and_then(|a| validated.get(&a))
                                .is_none_or(|t| now.saturating_sub(t.load(Relaxed)) >= silence);
                        if self.follow_source.load(Relaxed) || active_quiet {
                            self.active.store(Arc::new(Some(path)));
                        }
                    }
                }
                None => {
                    self.dropped_unvalidated.fetch_add(1, Relaxed);
                    continue;
                }
            }
            drop(validated);
            if kept != i {
                let (dst, src) = bufs.split_at_mut(i);
                dst[kept][..len].copy_from_slice(&src[0][start..start + len]);
                meta[kept] = meta[i];
            } else if start != 0 {
                bufs[i].copy_within(start..start + len, 0);
            }
            meta[kept].addr = self.quic_peer;
            meta[kept].dst_ip = None;
            meta[kept].len = len;
            if start != 0 {
                meta[kept].stride = len;
            }
            kept += 1;
        }
        kept
    }
}

impl AsyncUdpSocket for VirtualSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        self.inner.clone().create_io_poller()
    }

    fn try_send(&self, t: &Transmit<'_>) -> io::Result<()> {
        match **self.active.load() {
            // No path yet: like a packet lost on the wire. QUIC retransmits.
            None => Ok(()),
            Some(PathTarget::Direct(destination)) => {
                let real = Transmit {
                    destination,
                    ecn: t.ecn,
                    contents: t.contents,
                    segment_size: t.segment_size,
                    src_ip: None,
                };
                self.raw_send(&real)
            }
            Some(PathTarget::Relay { server, session }) => {
                // §4.1: no GSO through a relay; each segment gets its own frame.
                let seg = t.segment_size.unwrap_or(t.contents.len()).max(1);
                let mut frame = Vec::with_capacity(RELAY_HEADER_LEN + seg);
                for chunk in t.contents.chunks(seg) {
                    frame.clear();
                    write_relay_header(RelayKind::Data, &session, &mut frame);
                    frame.extend_from_slice(chunk);
                    let real = Transmit {
                        destination: server,
                        ecn: t.ecn,
                        contents: &frame,
                        segment_size: None,
                        src_ip: None,
                    };
                    self.raw_send(&real)?;
                }
                Ok(())
            }
        }
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        loop {
            // Stream-relay frames first; the pipe's waker is registered when it is empty.
            let n = self.take_stream(cx, bufs, meta);
            if n > 0 {
                self.last_rx_us.store((self.clock)(), Relaxed);
                let kept = self.filter(bufs, meta, n);
                if kept > 0 {
                    return Poll::Ready(Ok(kept));
                }
                continue;
            }
            match self.inner.poll_recv(cx, bufs, meta) {
                Poll::Ready(Ok(n)) => {
                    self.last_rx_us.store((self.clock)(), Relaxed);
                    let kept = self.filter(bufs, meta, n);
                    if kept > 0 {
                        return Poll::Ready(Ok(kept));
                    }
                    // Everything was STUN or rejected: poll again so a waker stays registered.
                }
                Poll::Pending => {
                    let mut w = self.recv_waker.lock().unwrap();
                    if !w.as_ref().is_some_and(|w| w.will_wake(cx.waker())) {
                        *w = Some(cx.waker().clone());
                    }
                    return Poll::Pending;
                }
                other => return other,
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        self.inner.max_transmit_segments()
    }

    fn max_receive_segments(&self) -> usize {
        self.inner.max_receive_segments()
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stun_packet() -> [u8; 20] {
        let mut p = [0u8; 20];
        p[1] = 0x01;
        p[4..8].copy_from_slice(&STUN_MAGIC_COOKIE);
        p
    }

    #[test]
    fn recognizes_stun() {
        assert!(is_stun(&stun_packet()));
    }

    #[test]
    fn greased_quic_short_header_is_not_stun() {
        // Fixed bit, spin bit and masked bits all zero: the first byte alone looks like STUN.
        let mut p = [0xABu8; 1200];
        p[0] = 0x01;
        assert!(!is_stun(&p));
    }

    #[test]
    fn short_or_ordinary_packets_are_not_stun() {
        assert!(!is_stun(&stun_packet()[..19]));
        let mut quic = stun_packet();
        quic[0] = 0x40;
        assert!(!is_stun(&quic));
        assert!(!is_stun(&[]));
    }
}
