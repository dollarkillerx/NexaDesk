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
//!
//! Also modelled: dual-stack sockets (IPv4 peers appear IPv4-mapped, as from a
//! real `[::]` socket), public hosts behind a stateful firewall (IPv6 without
//! NAT), static NAT entries (what PCP / NAT-PMP / UPnP create), and a machine
//! changing address under a live socket.

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
    pub dropped_no_route: u64,
}

struct Nat {
    cfg: NatConfig,
    /// Internal addresses (any port) behind this NAT.
    lan: HashSet<IpAddr>,
    /// (internal, destination or None for EIM) → public port.
    by_key: HashMap<(SocketAddr, Option<SocketAddr>), u16>,
    /// public port → (internal, permitted remotes).
    by_port: HashMap<u16, (SocketAddr, HashSet<SocketAddr>)>,
    /// Port-mapping protocol entries: internal → public port, open to anyone.
    statics: HashMap<SocketAddr, u16>,
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
        // A static entry is endpoint-independent for its internal port (RFC 6887 §11).
        let port = match self.statics.get(&from).or_else(|| self.by_key.get(&key)) {
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
        let open = self.statics.get(internal) == Some(&to_port);
        let ok = open
            || match self.cfg.filtering {
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
    /// Public hosts behind a stateful firewall: inbound only from where they sent.
    firewalled: HashSet<IpAddr>,
    /// (local, remote) pairs a firewalled host has sent on.
    permits: HashSet<(SocketAddr, SocketAddr)>,
    /// Public hosts whose outbound traffic is all dropped (an outage).
    blocked: HashSet<IpAddr>,
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
                firewalled: HashSet::new(),
                permits: HashSet::new(),
                blocked: HashSet::new(),
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

    /// A public host behind a stateful firewall, like most IPv6 home routers:
    /// inbound UDP only from addresses it has sent to (address and port).
    pub fn add_firewalled_host(&self, ip: IpAddr, uplink: Duration) {
        let mut st = self.lock();
        st.public.insert(ip, uplink);
        st.firewalled.insert(ip);
    }

    /// Drops everything a public host sends (`true`) or lets it through again.
    pub fn set_host_blocked(&self, ip: IpAddr, blocked: bool) {
        let mut st = self.lock();
        if blocked {
            st.blocked.insert(ip);
        } else {
            st.blocked.remove(&ip);
        }
    }

    /// What a port-mapping protocol does: `internal` is reachable from anyone at
    /// `public_ip:external`. False if the port is taken or there is no such NAT.
    pub fn add_static_mapping(
        &self,
        public_ip: IpAddr,
        internal: SocketAddr,
        external: u16,
    ) -> bool {
        let mut st = self.lock();
        let Some(n) = st.nats.iter_mut().find(|n| n.cfg.public_ip == public_ip) else {
            return false;
        };
        if n.by_port
            .get(&external)
            .is_some_and(|(i, _)| *i != internal)
        {
            return false;
        }
        n.by_port.insert(external, (internal, HashSet::new()));
        n.statics.insert(internal, external);
        true
    }

    pub fn remove_static_mapping(&self, public_ip: IpAddr, external: u16) {
        let mut st = self.lock();
        if let Some(n) = st.nats.iter_mut().find(|n| n.cfg.public_ip == public_ip)
            && let Some((internal, _)) = n.by_port.remove(&external)
        {
            n.statics.remove(&internal);
            n.by_key.retain(|_, p| *p != external);
        }
    }

    pub fn has_static_mapping(&self, public_ip: IpAddr, external: u16) -> bool {
        self.lock()
            .nats
            .iter()
            .any(|n| n.cfg.public_ip == public_ip && n.statics.values().any(|p| *p == external))
    }

    /// The machine at `old` now has address `new` (moved to another network):
    /// its sockets keep their ports and now send from, and receive at, `new`.
    /// Put `new` behind a NAT or on the core first.
    pub fn change_address(&self, old: IpAddr, new: IpAddr) -> usize {
        let mut st = self.lock();
        let moving: Vec<(SocketAddr, Weak<SimSocketShared>)> = st
            .sockets
            .iter()
            .filter(|(a, _)| a.ip() == old)
            .map(|(a, w)| (*a, w.clone()))
            .collect();
        for (a, w) in &moving {
            st.sockets.remove(a);
            let to = SocketAddr::new(new, a.port());
            if let Some(sock) = w.upgrade() {
                let mut addrs = sock.addrs.lock().unwrap_or_else(|e| e.into_inner());
                for x in addrs.iter_mut().filter(|x| **x == *a) {
                    *x = to;
                }
            }
            st.sockets.insert(to, w.clone());
        }
        moving.len()
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
            statics: HashMap::new(),
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
    pub fn bind(&self, addr: SocketAddr) -> io::Result<Arc<SimSocket>> {
        self.bind_ips(&[addr.ip()], addr.port(), addr)
    }

    /// A dual-stack socket (`[::]:port`) on a machine with these addresses.
    /// IPv4 peers show up IPv4-mapped, as on a real dual-stack socket.
    pub fn bind_dual(&self, ips: &[IpAddr], port: u16) -> io::Result<Arc<SimSocket>> {
        let local = SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), port);
        self.bind_ips(ips, port, local)
    }

    fn bind_ips(
        &self,
        ips: &[IpAddr],
        mut port: u16,
        local: SocketAddr,
    ) -> io::Result<Arc<SimSocket>> {
        let mut st = self.lock();
        let free = |st: &NetState, p: u16| {
            ips.iter().all(|ip| {
                st.sockets
                    .get(&SocketAddr::new(*ip, p))
                    .is_none_or(|w| w.strong_count() == 0)
            })
        };
        if port == 0 {
            loop {
                let p = st.next_ephemeral;
                st.next_ephemeral = if p >= 60_000 { 40_000 } else { p + 1 };
                if free(&st, p) {
                    port = p;
                    break;
                }
            }
        }
        if !free(&st, port) {
            return Err(io::Error::new(io::ErrorKind::AddrInUse, "address in use"));
        }
        let addrs: Vec<SocketAddr> = ips.iter().map(|ip| SocketAddr::new(*ip, port)).collect();
        let local = SocketAddr::new(local.ip(), port);
        let dual = local.is_ipv6() && local.ip().is_unspecified();
        let shared = Arc::new(SimSocketShared {
            addrs: Mutex::new(addrs.clone()),
            local,
            dual,
            state: Mutex::new(SocketState {
                queue: VecDeque::new(),
                waker: None,
            }),
        });
        for a in addrs {
            st.sockets.insert(a, Arc::downgrade(&shared));
        }
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

        if st.blocked.contains(&from.ip()) {
            stats.dropped_firewall += 1;
            st.stats = stats;
            return;
        }
        if st.firewalled.contains(&from.ip()) {
            st.permits.insert((from, to));
        }
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
                if st.firewalled.contains(&to.ip()) && !st.permits.contains(&(to, src)) {
                    stats.dropped_filtered += 1;
                    None
                } else {
                    Some(to)
                }
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
    /// One per address family the machine has; they move with `change_address`.
    addrs: Mutex<Vec<SocketAddr>>,
    /// What `local_addr` reports.
    local: SocketAddr,
    dual: bool,
    state: Mutex<SocketState>,
}

impl SimSocketShared {
    fn source_for(&self, to: SocketAddr) -> Option<SocketAddr> {
        let addrs = self.addrs.lock().unwrap_or_else(|e| e.into_inner());
        addrs.iter().find(|a| a.is_ipv4() == to.is_ipv4()).copied()
    }

    fn push(&self, from: SocketAddr, payload: Vec<u8>) {
        let from = match from {
            SocketAddr::V4(v4) if self.dual => {
                SocketAddr::new(IpAddr::V6(v4.ip().to_ipv6_mapped()), v4.port())
            }
            other => other,
        };
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
        write!(f, "SimSocket({})", self.shared.local)
    }
}

impl SimSocket {
    /// The address it was bound to (`[::]:port` for a dual-stack socket).
    pub fn addr(&self) -> SocketAddr {
        self.shared.local
    }

    /// Its current real addresses.
    pub fn addrs(&self) -> Vec<SocketAddr> {
        self.shared
            .addrs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
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
        if t.destination.is_ipv4() && self.shared.dual {
            // A real dual-stack socket wants IPv4 destinations IPv4-mapped.
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "IPv4 address on an IPv6 socket",
            ));
        }
        let to = crate::vsock::canonical(t.destination);
        let Some(from) = self.shared.source_for(to) else {
            self.net.lock().stats.dropped_no_route += 1;
            return Ok(());
        };
        let seg = t.segment_size.unwrap_or(t.contents.len()).max(1);
        for chunk in t.contents.chunks(seg) {
            self.net.route(from, to, chunk.to_vec());
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
        Ok(self.shared.local)
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
