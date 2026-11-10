//! An in-process, user-space network with NATs, for connectivity tests (§14).
//!
//! Sockets are [`AsyncUdpSocket`]s, so the real QUIC stack, `VirtualSocket`, the
//! checker and the signaling server's STUN and relay services all run on it
//! unchanged. Nothing touches the host's network.
//!
//! The NAT model follows RFC 4787's vocabulary:
//! - **mapping**: endpoint-independent (EIM) reuses one public port per internal
//!   socket; address-and-port-dependent (EDM) allocates one per destination.
//! - **filtering**: endpoint-independent (EIF), address-dependent (ADF) or
//!   address-and-port-dependent (APDF), relative to the destinations the
//!   mapping has sent to.
//! - **port allocation**: preserve the internal port when free, sequential from
//!   a base, or random.
//!
//! Latency is one-way and per link; delivery uses tokio timers, so tests need a
//! tokio runtime (paused time works).

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt, io,
    io::IoSliceMut,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::{Arc, Mutex, Weak},
    task::{Context, Poll, Waker},
    time::Duration,
};

use quinn::{
    AsyncUdpSocket, UdpPoller,
    udp::{RecvMeta, Transmit},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mapping {
    EndpointIndependent,
    AddressAndPortDependent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filtering {
    EndpointIndependent,
    AddressDependent,
    AddressAndPortDependent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortAlloc {
    Preserve,
    /// Each new mapping takes the next port (a "delta 1" symmetric NAT).
    Sequential,
    /// Unpredictable ports, seeded for reproducibility.
    Random(u64),
}

#[derive(Clone, Debug)]
pub struct NatConfig {
    pub public_ip: IpAddr,
    pub mapping: Mapping,
    pub filtering: Filtering,
    pub alloc: PortAlloc,
    pub hairpin: bool,
    /// One-way latency between this NAT and the core.
    pub uplink: Duration,
    /// If set, outbound UDP only reaches these addresses (a restrictive firewall).
    pub allow_only: Option<HashSet<IpAddr>>,
}

impl NatConfig {
    /// EIM + APDF with port preservation: what most home routers do.
    pub fn easy(public_ip: IpAddr) -> Self {
        Self {
            public_ip,
            mapping: Mapping::EndpointIndependent,
            filtering: Filtering::AddressAndPortDependent,
            alloc: PortAlloc::Preserve,
            hairpin: false,
            uplink: Duration::from_millis(10),
            allow_only: None,
        }
    }

    /// EDM + APDF with sequential ports: a predictable symmetric NAT.
    pub fn hard(public_ip: IpAddr) -> Self {
        Self {
            mapping: Mapping::AddressAndPortDependent,
            alloc: PortAlloc::Sequential,
            ..Self::easy(public_ip)
        }
    }

    /// EDM + APDF with random ports: carrier-grade NATs, many firewalls.
    pub fn hard_random(public_ip: IpAddr, seed: u64) -> Self {
        Self {
            alloc: PortAlloc::Random(seed),
            ..Self::hard(public_ip)
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SimStats {
    pub delivered: u64,
    pub dropped_no_mapping: u64,
    pub dropped_filtered: u64,
    pub dropped_unroutable: u64,
    pub dropped_firewall: u64,
    pub mappings_created: u64,
}

struct Nat {
    cfg: NatConfig,
    /// Internal addresses (any port) behind this NAT.
    lan: HashSet<IpAddr>,
    /// (internal, destination or None for EIM) → public port.
    by_key: HashMap<(SocketAddr, Option<SocketAddr>), u16>,
    /// public port → (internal, permitted remotes).
    by_port: HashMap<u16, (SocketAddr, HashSet<SocketAddr>)>,
    next_port: u16,
    rng: u64,
}

impl Nat {
    fn allocate(&mut self, internal: SocketAddr) -> Option<u16> {
        for _ in 0..65_536 {
            let p = match self.cfg.alloc {
                PortAlloc::Preserve if !self.by_port.contains_key(&internal.port()) => {
                    internal.port()
                }
                PortAlloc::Preserve | PortAlloc::Sequential => {
                    let p = self.next_port;
                    self.next_port = if p >= 65_000 { 20_000 } else { p + 1 };
                    p
                }
                PortAlloc::Random(_) => {
                    self.rng ^= self.rng << 13;
                    self.rng ^= self.rng >> 7;
                    self.rng ^= self.rng << 17;
                    20_000 + (self.rng % 40_000) as u16
                }
            };
            if !self.by_port.contains_key(&p) {
                return Some(p);
            }
        }
        None
    }

    fn outbound(
        &mut self,
        from: SocketAddr,
        to: SocketAddr,
        stats: &mut SimStats,
    ) -> Option<SocketAddr> {
        let key = match self.cfg.mapping {
            Mapping::EndpointIndependent => (from, None),
            Mapping::AddressAndPortDependent => (from, Some(to)),
        };
        let port = match self.by_key.get(&key) {
            Some(&p) => p,
            None => {
                let p = self.allocate(from)?;
                self.by_key.insert(key, p);
                self.by_port.insert(p, (from, HashSet::new()));
                stats.mappings_created += 1;
                p
            }
        };
        if let Some((_, permitted)) = self.by_port.get_mut(&port) {
            permitted.insert(to);
        }
        Some(SocketAddr::new(self.cfg.public_ip, port))
    }

    fn inbound(&self, from: SocketAddr, to_port: u16, stats: &mut SimStats) -> Option<SocketAddr> {
        let Some((internal, permitted)) = self.by_port.get(&to_port) else {
            stats.dropped_no_mapping += 1;
            return None;
        };
        let ok = match self.cfg.filtering {
            Filtering::EndpointIndependent => true,
            Filtering::AddressDependent => permitted.iter().any(|p| p.ip() == from.ip()),
            Filtering::AddressAndPortDependent => permitted.contains(&from),
        };
        if !ok {
            stats.dropped_filtered += 1;
            return None;
        }
        Some(*internal)
    }
}

struct SocketState {
    queue: VecDeque<(SocketAddr, Vec<u8>)>,
    waker: Option<Waker>,
}

struct NetState {
    nats: Vec<Nat>,
    /// Public hosts (no NAT) and their uplink latency.
    public: HashMap<IpAddr, Duration>,
    sockets: HashMap<SocketAddr, Weak<SimSocketShared>>,
    lan_latency: Duration,
    stats: SimStats,
    next_ephemeral: u16,
}

/// The network. Cloning shares it.
#[derive(Clone)]
pub struct SimNet {
    state: Arc<Mutex<NetState>>,
}

impl fmt::Debug for SimNet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SimNet").finish_non_exhaustive()
    }
}

impl Default for SimNet {
    fn default() -> Self {
        Self::new()
    }
}

impl SimNet {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(NetState {
                nats: Vec::new(),
                public: HashMap::new(),
                sockets: HashMap::new(),
                lan_latency: Duration::from_millis(1),
                stats: SimStats::default(),
                next_ephemeral: 40_000,
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, NetState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A machine directly on the core network, `uplink` away from it.
    pub fn add_public_host(&self, ip: IpAddr, uplink: Duration) {
        self.lock().public.insert(ip, uplink);
    }

    /// A NAT with the given machines behind it.
    pub fn add_nat(&self, cfg: NatConfig, lan: &[IpAddr]) {
        let seed = match cfg.alloc {
            PortAlloc::Random(s) => s | 1,
            _ => 1,
        };
        self.lock().nats.push(Nat {
            cfg,
            lan: lan.iter().copied().collect(),
            by_key: HashMap::new(),
            by_port: HashMap::new(),
            next_port: 30_000,
            rng: seed,
        });
    }

    /// Changes a NAT's outbound firewall at run time (`None` lifts it).
    pub fn set_firewall(&self, public_ip: IpAddr, allow_only: Option<HashSet<IpAddr>>) {
        let mut st = self.lock();
        if let Some(n) = st.nats.iter_mut().find(|n| n.cfg.public_ip == public_ip) {
            n.cfg.allow_only = allow_only;
        }
    }

    pub fn stats(&self) -> SimStats {
        self.lock().stats
    }

    /// Binds a socket. Port 0 picks a free one.
    pub fn bind(&self, mut addr: SocketAddr) -> io::Result<Arc<SimSocket>> {
        let mut st = self.lock();
        if addr.port() == 0 {
            loop {
                let p = st.next_ephemeral;
                st.next_ephemeral = if p >= 60_000 { 40_000 } else { p + 1 };
                let candidate = SocketAddr::new(addr.ip(), p);
                if !st.sockets.contains_key(&candidate) {
                    addr = candidate;
                    break;
                }
            }
        }
        if st.sockets.get(&addr).is_some_and(|w| w.strong_count() > 0) {
            return Err(io::Error::new(io::ErrorKind::AddrInUse, "address in use"));
        }
        let shared = Arc::new(SimSocketShared {
            addr,
            state: Mutex::new(SocketState {
                queue: VecDeque::new(),
                waker: None,
            }),
        });
        st.sockets.insert(addr, Arc::downgrade(&shared));
        Ok(Arc::new(SimSocket {
            net: self.clone(),
            shared,
        }))
    }

    fn nat_of(st: &NetState, ip: IpAddr) -> Option<usize> {
        st.nats.iter().position(|n| n.lan.contains(&ip))
    }

    fn nat_by_public(st: &NetState, ip: IpAddr) -> Option<usize> {
        st.nats.iter().position(|n| n.cfg.public_ip == ip)
    }

    /// Routes one datagram and schedules its delivery.
    fn route(&self, from: SocketAddr, to: SocketAddr, payload: Vec<u8>) {
        let mut st = self.lock();
        let st = &mut *st;
        let mut stats = st.stats;
        let mut delay = Duration::ZERO;
        let mut src = from;

        let src_nat = Self::nat_of(st, from.ip());
        let same_lan = src_nat.is_some() && src_nat == Self::nat_of(st, to.ip());
        let dst: Option<SocketAddr> = if same_lan || from.ip() == to.ip() {
            delay += st.lan_latency;
            Some(to)
        } else {
            if let Some(i) = src_nat {
                let nat = &mut st.nats[i];
                if nat
                    .cfg
                    .allow_only
                    .as_ref()
                    .is_some_and(|a| !a.contains(&to.ip()))
                {
                    stats.dropped_firewall += 1;
                    st.stats = stats;
                    return;
                }
                let hairpin = nat.cfg.public_ip == to.ip();
                if hairpin && !nat.cfg.hairpin {
                    stats.dropped_unroutable += 1;
                    st.stats = stats;
                    return;
                }
                match nat.outbound(from, to, &mut stats) {
                    Some(mapped) => src = mapped,
                    None => {
                        st.stats = stats;
                        return;
                    }
                }
                delay += nat.cfg.uplink;
            } else if let Some(up) = st.public.get(&from.ip()) {
                delay += *up;
            }
            // The core: deliver to a public host or through the destination's NAT.
            if let Some(up) = st.public.get(&to.ip()) {
                delay += *up;
                Some(to)
            } else if let Some(j) = Self::nat_by_public(st, to.ip()) {
                delay += st.nats[j].cfg.uplink;
                st.nats[j].inbound(src, to.port(), &mut stats)
            } else {
                stats.dropped_unroutable += 1;
                None
            }
        };
        let target = dst.and_then(|d| st.sockets.get(&d).and_then(Weak::upgrade));
        match target {
            Some(sock) => {
                stats.delivered += 1;
                st.stats = stats;
                if delay.is_zero() {
                    sock.push(src, payload);
                } else {
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        sock.push(src, payload);
                    });
                }
            }
            None => {
                if dst.is_some() {
                    stats.dropped_unroutable += 1;
                }
                st.stats = stats;
            }
        }
    }
}

struct SimSocketShared {
    addr: SocketAddr,
    state: Mutex<SocketState>,
}

impl SimSocketShared {
    fn push(&self, from: SocketAddr, payload: Vec<u8>) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.queue.len() >= 4096 {
            return; // a full receive buffer drops, like a real one
        }
        st.queue.push_back((from, payload));
        if let Some(w) = st.waker.take() {
            w.wake();
        }
    }
}

/// A socket on a [`SimNet`].
pub struct SimSocket {
    net: SimNet,
    shared: Arc<SimSocketShared>,
}

impl fmt::Debug for SimSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SimSocket({})", self.shared.addr)
    }
}

impl SimSocket {
    pub fn addr(&self) -> SocketAddr {
        self.shared.addr
    }
}

#[derive(Debug)]
struct AlwaysWritable;

impl UdpPoller for AlwaysWritable {
    fn poll_writable(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncUdpSocket for SimSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(AlwaysWritable)
    }

    fn try_send(&self, t: &Transmit<'_>) -> io::Result<()> {
        let seg = t.segment_size.unwrap_or(t.contents.len()).max(1);
        for chunk in t.contents.chunks(seg) {
            self.net
                .route(self.shared.addr, t.destination, chunk.to_vec());
        }
        Ok(())
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let mut st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut n = 0;
        while n < bufs.len().min(meta.len()) {
            let Some((from, payload)) = st.queue.pop_front() else {
                break;
            };
            let len = payload.len().min(bufs[n].len());
            bufs[n][..len].copy_from_slice(&payload[..len]);
            meta[n] = RecvMeta {
                addr: from,
                len,
                stride: len,
                ..Default::default()
            };
            n += 1;
        }
        if n > 0 {
            return Poll::Ready(Ok(n));
        }
        st.waker = Some(cx.waker().clone());
        Poll::Pending
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.shared.addr)
    }

    fn max_transmit_segments(&self) -> usize {
        1
    }

    fn max_receive_segments(&self) -> usize {
        1
    }

    fn may_fragment(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::udp::UdpIo;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    async fn recv(io: &UdpIo) -> Option<(Vec<u8>, SocketAddr)> {
        let mut buf = [0u8; 64];
        match tokio::time::timeout(Duration::from_millis(200), io.recv_from(&mut buf)).await {
            Ok(Ok((n, from))) => Some((buf[..n].to_vec(), from)),
            _ => None,
        }
    }

    fn net(nat: NatConfig) -> SimNet {
        let net = SimNet::new();
        net.add_public_host(ip("203.0.113.1"), Duration::from_millis(5));
        net.add_public_host(ip("203.0.113.2"), Duration::from_millis(5));
        net.add_nat(nat, &[ip("192.168.1.2")]);
        net
    }

    #[tokio::test(start_paused = true)]
    async fn easy_nat_maps_once_and_filters_by_destination() {
        let net = net(NatConfig::easy(ip("198.51.100.1")));
        let inside = UdpIo::new(net.bind(sa("192.168.1.2:5000")).unwrap());
        let a = UdpIo::new(net.bind(sa("203.0.113.1:3478")).unwrap());
        let b = UdpIo::new(net.bind(sa("203.0.113.2:3478")).unwrap());
        inside.send_to(b"x", a.local_addr()).await.unwrap();
        inside.send_to(b"y", b.local_addr()).await.unwrap();
        let (_, seen_a) = recv(&a).await.unwrap();
        let (_, seen_b) = recv(&b).await.unwrap();
        assert_eq!(seen_a, sa("198.51.100.1:5000"), "port preserved");
        assert_eq!(seen_a, seen_b, "endpoint-independent mapping");
        // A third party that was never contacted is filtered.
        let c = UdpIo::new(net.bind(sa("203.0.113.1:9999")).unwrap());
        c.send_to(b"z", seen_a).await.unwrap();
        assert!(recv(&inside).await.is_none());
        assert_eq!(net.stats().dropped_filtered, 1);
        // A contacted one gets through.
        a.send_to(b"back", seen_a).await.unwrap();
        assert_eq!(
            recv(&inside).await.unwrap(),
            (b"back".to_vec(), a.local_addr())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn hard_nat_maps_per_destination_sequentially() {
        let net = net(NatConfig::hard(ip("198.51.100.1")));
        let inside = UdpIo::new(net.bind(sa("192.168.1.2:5000")).unwrap());
        let a = UdpIo::new(net.bind(sa("203.0.113.1:3478")).unwrap());
        let b = UdpIo::new(net.bind(sa("203.0.113.2:3478")).unwrap());
        inside.send_to(b"x", a.local_addr()).await.unwrap();
        inside.send_to(b"y", b.local_addr()).await.unwrap();
        let (_, seen_a) = recv(&a).await.unwrap();
        let (_, seen_b) = recv(&b).await.unwrap();
        assert_eq!(seen_b.port(), seen_a.port() + 1);
        // Answers from b to a's mapping are not let in.
        b.send_to(b"nope", seen_a).await.unwrap();
        assert!(recv(&inside).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn firewall_and_unroutable_destinations_drop() {
        let mut cfg = NatConfig::easy(ip("198.51.100.1"));
        cfg.allow_only = Some([ip("203.0.113.1")].into());
        let net = net(cfg);
        let inside = UdpIo::new(net.bind(sa("192.168.1.2:5000")).unwrap());
        inside.send_to(b"x", sa("203.0.113.2:1")).await.unwrap();
        inside.send_to(b"x", sa("10.9.9.9:1")).await.unwrap();
        let s = net.stats();
        assert_eq!(s.dropped_firewall, 2);
        assert_eq!(s.delivered, 0);
    }
}
