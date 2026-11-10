//! Virtual path socket (DETAILED_DESIGN §4.1): QUIC only ever sees one fixed
//! peer address; the real destination is chosen below it.

use std::{
    fmt,
    io::{self, IoSliceMut},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed},
    },
    task::{Context, Poll},
};

use quinn::{
    AsyncUdpSocket, UdpPoller,
    udp::{RecvMeta, Transmit},
};

use crate::now_us;

pub const VIRTUAL_PEER: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 255, 255, 1)), 1);

pub struct VirtualSocket {
    inner: Arc<dyn AsyncUdpSocket>,
    targets: Vec<SocketAddr>,
    active: AtomicUsize,
    pub last_rx_us: AtomicU64,
}

impl fmt::Debug for VirtualSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VirtualSocket")
            .field("targets", &self.targets)
            .finish()
    }
}

impl VirtualSocket {
    pub fn new(inner: Arc<dyn AsyncUdpSocket>, targets: Vec<SocketAddr>) -> Self {
        Self {
            inner,
            targets,
            active: AtomicUsize::new(0),
            last_rx_us: AtomicU64::new(now_us()),
        }
    }

    pub fn set_active(&self, idx: usize) {
        self.active.store(idx, Relaxed);
    }
}

impl AsyncUdpSocket for VirtualSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        self.inner.clone().create_io_poller()
    }

    fn try_send(&self, t: &Transmit<'_>) -> io::Result<()> {
        let real = Transmit {
            destination: self.targets[self.active.load(Relaxed)],
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
        let r = self.inner.poll_recv(cx, bufs, meta);
        if let Poll::Ready(Ok(n)) = r {
            for m in meta.iter_mut().take(n) {
                m.addr = VIRTUAL_PEER;
                m.dst_ip = None;
            }
            self.last_rx_us.store(now_us(), Relaxed);
        }
        r
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
