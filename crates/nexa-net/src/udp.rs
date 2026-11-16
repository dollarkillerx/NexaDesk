//! `recv_from` / `send_to` over a quinn [`AsyncUdpSocket`], so that the same
//! service code runs on a real socket or on the simulator.

use std::{
    collections::VecDeque,
    future::poll_fn,
    io::{self, IoSliceMut},
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex},
};

use quinn::{
    AsyncUdpSocket, UdpPoller,
    udp::{RecvMeta, Transmit},
};

pub struct UdpIo {
    socket: Arc<dyn AsyncUdpSocket>,
    poller: Mutex<Pin<Box<dyn UdpPoller>>>,
    rx: Mutex<Rx>,
}

/// Room for one receive, which may hold several coalesced datagrams, and the
/// datagrams of the last one not handed out yet.
struct Rx {
    buf: Vec<u8>,
    queued: VecDeque<(Vec<u8>, SocketAddr)>,
}

/// The most one receive returns: a UDP datagram, or a coalesced batch of them.
const RECV_BUF: usize = 64 * 1024;

impl std::fmt::Debug for UdpIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UdpIo").finish_non_exhaustive()
    }
}

impl UdpIo {
    pub fn new(socket: Arc<dyn AsyncUdpSocket>) -> Self {
        let poller = socket.clone().create_io_poller();
        Self {
            socket,
            poller: Mutex::new(poller),
            rx: Mutex::new(Rx {
                buf: vec![0; RECV_BUF],
                queued: VecDeque::new(),
            }),
        }
    }

    /// A real UDP socket on the tokio runtime.
    pub fn bind_tokio(addr: SocketAddr) -> io::Result<Self> {
        let std = crate::vsock::bind_media_socket(addr)?;
        Ok(Self::new(crate::vsock::wrap_without_gro(std)?))
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.socket
            .local_addr()
            .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)))
    }

    /// One datagram per call, as from a plain socket. A receive the kernel
    /// coalesced (GRO, `stride` bytes apart) is handed out a datagram at a time.
    /// A datagram longer than `buf` is truncated.
    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        poll_fn(|cx| {
            let mut rx = self.rx.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((d, from)) = rx.queued.pop_front() {
                return std::task::Poll::Ready(Ok((copy_into(buf, &d), from)));
            }
            let Rx { buf: big, queued } = &mut *rx;
            let mut meta = [RecvMeta::default()];
            let mut bufs = [IoSliceMut::new(big)];
            match self.socket.poll_recv(cx, &mut bufs, &mut meta) {
                std::task::Poll::Ready(Ok(_)) => {
                    let m = meta[0];
                    let stride = if m.stride == 0 { m.len } else { m.stride };
                    let mut chunks = big[..m.len].chunks(stride.max(1));
                    let first = chunks.next().unwrap_or(&[]);
                    queued.extend(chunks.map(|c| (c.to_vec(), m.addr)));
                    std::task::Poll::Ready(Ok((copy_into(buf, first), m.addr)))
                }
                std::task::Poll::Ready(Err(e)) => std::task::Poll::Ready(Err(e)),
                std::task::Poll::Pending => std::task::Poll::Pending,
            }
        })
        .await
    }

    /// Waits for room when the socket is full; datagram semantics otherwise.
    pub async fn send_to(&self, payload: &[u8], to: SocketAddr) -> io::Result<()> {
        loop {
            let t = Transmit {
                destination: to,
                ecn: None,
                contents: payload,
                segment_size: None,
                src_ip: None,
            };
            match self.socket.try_send(&t) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    poll_fn(|cx| {
                        self.poller
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .as_mut()
                            .poll_writable(cx)
                    })
                    .await?;
                }
                r => return r,
            }
        }
    }

    /// Sends without waiting; a full socket drops the datagram.
    pub fn try_send_to(&self, payload: &[u8], to: SocketAddr) -> io::Result<()> {
        let t = Transmit {
            destination: to,
            ecn: None,
            contents: payload,
            segment_size: None,
            src_ip: None,
        };
        match self.socket.try_send(&t) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(()),
            r => r,
        }
    }
}

fn copy_into(buf: &mut [u8], datagram: &[u8]) -> usize {
    let n = datagram.len().min(buf.len());
    buf[..n].copy_from_slice(&datagram[..n]);
    n
}

#[cfg(test)]
mod tests {
    use std::task::{Context, Poll};

    use super::*;

    /// Returns one coalesced receive: three datagrams `stride` apart, the last short.
    #[derive(Debug)]
    struct Coalesced(Mutex<Option<Vec<u8>>>);

    #[derive(Debug)]
    struct Writable;

    impl UdpPoller for Writable {
        fn poll_writable(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    const FROM: SocketAddr =
        SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 9);

    impl AsyncUdpSocket for Coalesced {
        fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
            Box::pin(Writable)
        }
        fn try_send(&self, _: &Transmit) -> io::Result<()> {
            Ok(())
        }
        fn poll_recv(
            &self,
            _: &mut Context<'_>,
            bufs: &mut [IoSliceMut<'_>],
            meta: &mut [RecvMeta],
        ) -> Poll<io::Result<usize>> {
            let Some(batch) = self.0.lock().unwrap().take() else {
                return Poll::Pending;
            };
            bufs[0][..batch.len()].copy_from_slice(&batch);
            meta[0] = RecvMeta {
                addr: FROM,
                len: batch.len(),
                stride: 4,
                ..RecvMeta::default()
            };
            Poll::Ready(Ok(1))
        }
        fn local_addr(&self) -> io::Result<SocketAddr> {
            Ok(FROM)
        }
    }

    #[tokio::test]
    async fn a_coalesced_receive_comes_out_one_datagram_at_a_time() {
        let batch = b"aaaabbbbcc".to_vec();
        let io = UdpIo::new(Arc::new(Coalesced(Mutex::new(Some(batch)))));
        let mut buf = [0u8; 64];
        for want in [&b"aaaa"[..], b"bbbb", b"cc"] {
            let (n, from) = io.recv_from(&mut buf).await.unwrap();
            assert_eq!((&buf[..n], from), (want, FROM));
        }
    }
}
