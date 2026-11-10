//! The relay over WebSocket (§4.6 WSS fallback), for networks that block UDP.
//!
//! One connection to `/relay` on the signaling server carries relay frames,
//! one per binary message, exactly as they would go over UDP. The virtual
//! socket sees it as the relay at [`STREAM_RELAY_ADDR`]: binding, checks and
//! QUIC run over it unchanged, with the same token. Like UDP, nothing here
//! waits: a full queue drops.
//!
//! TCP brings head-of-line blocking and retransmission under QUIC's own, so
//! this path is scored last (`PENALTY_WSS_RELAY`) and game mode warns about it.
//!
//! [`STREAM_RELAY_ADDR`]: nexa_net::vsock::STREAM_RELAY_ADDR

use std::{sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use nexa_net::vsock::{StreamPipe, VirtualSocket};
use nexa_wire::signal::{RELAY_HEADER_LEN, is_relay_frame};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_tungstenite::tungstenite::{Message, protocol::WebSocketConfig};

use super::signal::{CONNECT_TIMEOUT, SignalConfig, SignalError, connect_ws};

/// The server's path for relay WebSockets.
pub const RELAY_PATH: &str = "/relay";
/// Frames waiting for the socket; more are dropped, as UDP would.
const QUEUE: usize = 512;
const PING_EVERY: Duration = Duration::from_secs(20);
/// A relay frame is a QUIC packet (≤ 1200 bytes here) plus the header.
const MAX_FRAME: usize = 2048;

/// The open WebSocket. Dropping it closes the connection and detaches it
/// from the virtual socket.
pub struct StreamRelay {
    vsock: Arc<VirtualSocket>,
    tasks: [JoinHandle<()>; 2],
}

impl std::fmt::Debug for StreamRelay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamRelay").finish_non_exhaustive()
    }
}

impl Drop for StreamRelay {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
        self.vsock.set_stream_relay(None);
    }
}

fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME))
        .max_frame_size(Some(MAX_FRAME))
}

/// Connects to the server's relay endpoint and attaches it to `vsock`.
pub async fn open(
    cfg: &SignalConfig,
    vsock: Arc<VirtualSocket>,
) -> Result<StreamRelay, SignalError> {
    let (ws, _) = tokio::time::timeout(CONNECT_TIMEOUT, connect_ws(cfg, RELAY_PATH, ws_config()))
        .await
        .map_err(|_| SignalError::Timeout)??;
    let (mut sink, mut stream) = ws.split();
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(QUEUE);
    let pipe = StreamPipe::new(Box::new(move |frame| tx.try_send(frame).is_ok()));
    let writer = tokio::spawn(async move {
        let mut ping = tokio::time::interval(PING_EVERY);
        ping.tick().await;
        loop {
            tokio::select! {
                f = rx.recv() => match f {
                    Some(f) => {
                        if sink.send(Message::binary(f)).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
                _ = ping.tick() => {
                    if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = sink.close().await;
    });
    let inbound = pipe.clone();
    let reader = tokio::spawn(async move {
        while let Some(Ok(m)) = stream.next().await {
            match m {
                // The server is untrusted: only well-formed relay frames go on.
                Message::Binary(b) if b.len() >= RELAY_HEADER_LEN && is_relay_frame(&b) => {
                    inbound.deliver(b.to_vec());
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });
    vsock.set_stream_relay(Some(pipe));
    Ok(StreamRelay {
        vsock,
        tasks: [writer, reader],
    })
}
