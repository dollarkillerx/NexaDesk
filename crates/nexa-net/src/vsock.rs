//! Virtual path socket (§4.1): QUIC sees one fixed peer address; real paths live below it.

use std::{
    collections::HashSet,
    fmt,
    io::{self, IoSliceMut},
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    pin::Pin,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
    task::{Context, Poll},
};

use arc_swap::ArcSwap;
use quinn::{
    AsyncUdpSocket, UdpPoller,
    udp::{RecvMeta, Transmit},
};

pub const VIRTUAL_PEER: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 255, 255, 1)), 1);

pub const SOCKET_BUFFER: usize = 4 << 20;

/// Creates a UDP socket with the enlarged buffers from §4.1.
pub fn bind_media_socket(addr: SocketAddr) -> io::Result<UdpSocket> {
    let s = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    s.set_send_buffer_size(SOCKET_BUFFER)?;
    s.set_recv_buffer_size(SOCKET_BUFFER)?;
    s.bind(&addr.into())?;
    Ok(s.into())
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

pub struct VirtualSocket {
    inner: Arc<dyn AsyncUdpSocket>,
    active: ArcSwap<SocketAddr>,
    validated: RwLock<HashSet<SocketAddr>>,
    stun: StunSink,
    clock: Box<dyn Fn() -> u64 + Send + Sync>,
    last_rx_us: AtomicU64,
    dropped_unvalidated: AtomicU64,
}

impl fmt::Debug for VirtualSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VirtualSocket")
            .field("active", &**self.active.load())
            .finish_non_exhaustive()
    }
}

impl VirtualSocket {
    pub fn new(
        inner: Arc<dyn AsyncUdpSocket>,
        first_path: SocketAddr,
        stun: StunSink,
        clock: Box<dyn Fn() -> u64 + Send + Sync>,
    ) -> Self {
        let now = clock();
        Self {
            inner,
            active: ArcSwap::from_pointee(first_path),
            validated: RwLock::new(HashSet::from([first_path])),
            stun,
            clock,
            last_rx_us: AtomicU64::new(now),
            dropped_unvalidated: AtomicU64::new(0),
        }
    }

    /// Marks a path as passing connectivity checks. QUIC is only accepted from such sources.
    pub fn validate_path(&self, addr: SocketAddr) {
        self.validated.write().unwrap().insert(addr);
    }

    pub fn forget_path(&self, addr: SocketAddr) {
        self.validated.write().unwrap().remove(&addr);
    }

    /// Only call after an authenticated decision (`PathSwitch` over QUIC, §4.5).
    /// The target becomes a validated path, since we now expect its replies.
    pub fn switch_to(&self, addr: SocketAddr) {
        self.validate_path(addr);
        self.active.store(Arc::new(addr));
    }

    pub fn active(&self) -> SocketAddr {
        **self.active.load()
    }

    pub fn last_rx_us(&self) -> u64 {
        self.last_rx_us.load(Relaxed)
    }

    pub fn dropped_unvalidated(&self) -> u64 {
        self.dropped_unvalidated.load(Relaxed)
    }

    /// Applies §4.1's receive rules in place, keeping accepted packets at the front.
    /// Returns how many remain.
    fn filter(&self, bufs: &mut [IoSliceMut<'_>], meta: &mut [RecvMeta], n: usize) -> usize {
        let mut kept = 0;
        for i in 0..n {
            let len = meta[i].len;
            let from = meta[i].addr;
            if is_stun(&bufs[i][..len]) {
                (self.stun)(&bufs[i][..len], from);
                continue;
            }
            if !self.validated.read().unwrap().contains(&from) {
                self.dropped_unvalidated.fetch_add(1, Relaxed);
                continue;
            }
            if kept != i {
                let (dst, src) = bufs.split_at_mut(i);
                dst[kept][..len].copy_from_slice(&src[0][..len]);
                meta[kept] = meta[i];
            }
            meta[kept].addr = VIRTUAL_PEER;
            meta[kept].dst_ip = None;
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
        let real = Transmit {
            destination: **self.active.load(),
            ecn: t.ecn,
            contents: t.contents,
            segment_size: t.segment_size,
            src_ip: None,
        };
        self.inner.try_send(&real)
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        loop {
            match self.inner.poll_recv(cx, bufs, meta) {
                Poll::Ready(Ok(n)) => {
                    self.last_rx_us.store((self.clock)(), Relaxed);
                    let kept = self.filter(bufs, meta, n);
                    if kept > 0 {
                        return Poll::Ready(Ok(kept));
                    }
                    // Everything was STUN or rejected: poll again so a waker stays registered.
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
