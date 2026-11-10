//! `recv_from` / `send_to` over a quinn [`AsyncUdpSocket`], so that the same
//! service code runs on a real socket or on the simulator.

use std::{
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
}

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
        }
    }

    /// A real UDP socket on the tokio runtime.
    pub fn bind_tokio(addr: SocketAddr) -> io::Result<Self> {
        use quinn::Runtime;
        let std = crate::vsock::bind_media_socket(addr)?;
        let socket = quinn::TokioRuntime.wrap_udp_socket(std)?;
        Ok(Self::new(socket))
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.socket
            .local_addr()
            .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)))
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        poll_fn(|cx| {
            let mut meta = [RecvMeta::default()];
            let mut bufs = [IoSliceMut::new(buf)];
            match self.socket.poll_recv(cx, &mut bufs, &mut meta) {
                std::task::Poll::Ready(Ok(_)) => {
                    std::task::Poll::Ready(Ok((meta[0].len, meta[0].addr)))
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
