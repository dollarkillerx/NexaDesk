//! Ties the hub, STUN and relay to sockets.

use std::{
    net::{IpAddr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};

use futures_util::{SinkExt, StreamExt};
use nexa_net::udp::UdpIo;
use nexa_wire::signal::{
    Challenge, ClientBody, ErrorCode, MAX_SIGNAL_MESSAGE, NONCE_LEN, SIGNAL_PROTOCOL, ServerBody,
    SignalClientMsg, SignalServerMsg,
};
use prost::Message as _;
use rand_core::{OsRng, RngCore};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpListener,
    sync::mpsc,
    task::JoinHandle,
};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::{Message, protocol::WebSocketConfig};

use crate::{
    config::Limits,
    hub::{Hub, HubSettings, error},
    limit::{Bucket, Rate},
    relay::{Relay, RelayLimits, TokenIssuer},
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const REGISTER_TIMEOUT: Duration = Duration::from_secs(10);
/// Clients ping every 20 s; silence for this long ends the connection.
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const PING_EVERY: Duration = Duration::from_secs(30);
const OUTBOX: usize = 64;

/// Everything the server listens on. The binary binds real sockets; tests can
/// pass simulated UDP sockets.
pub struct Sockets {
    pub ws: TcpListener,
    pub stun: Vec<Arc<UdpIo>>,
    pub relay: Option<Arc<UdpIo>>,
}

pub struct Settings {
    pub tls: Option<Arc<rustls::ServerConfig>>,
    pub server_names: Vec<String>,
    /// What clients are told; one entry per STUN socket.
    pub stun_advertise: Vec<SocketAddr>,
    pub relay_advertise: Option<SocketAddr>,
    pub relay: RelayLimits,
    pub relay_token_ttl_secs: u64,
    pub limits: Limits,
}

pub struct Server {
    pub ws_addr: SocketAddr,
    hub: Arc<Mutex<Hub>>,
    relay: Arc<Mutex<Relay>>,
    tasks: Vec<JoinHandle<()>>,
}

impl Server {
    pub fn registered(&self) -> usize {
        self.hub
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .registered()
    }

    pub fn relay_stats(&self) -> crate::relay::RelayStats {
        self.relay.lock().unwrap_or_else(|e| e.into_inner()).stats
    }

    pub fn shutdown(&self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub async fn start(settings: Settings, sockets: Sockets) -> std::io::Result<Server> {
    let ws_addr = sockets.ws.local_addr()?;
    let issuer = Arc::new(TokenIssuer::new());
    let stun_note = crate::config::stun_note(&settings.stun_advertise);
    let hub = Arc::new(Mutex::new(Hub::new(HubSettings {
        limits: settings.limits.clone(),
        server_names: settings.server_names.clone(),
        stun: settings.stun_advertise.clone(),
        stun_note,
        relay: settings.relay_advertise.filter(|_| sockets.relay.is_some()),
        relay_ttl_secs: settings.relay_token_ttl_secs,
        issuer: issuer.clone(),
    })));
    let relay = Arc::new(Mutex::new(Relay::new(
        issuer.verifying_key(),
        settings.relay,
    )));
    let mut tasks = Vec::new();
    let stun_rate = Rate::new(settings.limits.stun_per_ip.0, settings.limits.stun_per_ip.1);
    for s in sockets.stun {
        tasks.push(tokio::spawn(crate::stun::serve(s, stun_rate)));
    }
    if let Some(s) = sockets.relay {
        tasks.push(tokio::spawn(crate::relay::serve(s, relay.clone())));
    }
    {
        let hub = hub.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                hub.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .sweep(Instant::now());
            }
        }));
    }
    let acceptor = settings.tls.clone().map(TlsAcceptor::from);
    tasks.push(tokio::spawn(accept_loop(sockets.ws, acceptor, hub.clone())));
    Ok(Server {
        ws_addr,
        hub,
        relay,
        tasks,
    })
}

async fn accept_loop(listener: TcpListener, tls: Option<TlsAcceptor>, hub: Arc<Mutex<Hub>>) {
    let next_conn = Arc::new(AtomicU64::new(1));
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let ip = peer.ip().to_canonical();
        if !hub.lock().unwrap_or_else(|e| e.into_inner()).open_conn(ip) {
            drop(tcp);
            continue;
        }
        let _ = tcp.set_nodelay(true);
        let conn = next_conn.fetch_add(1, Relaxed);
        let (hub, tls) = (hub.clone(), tls.clone());
        tokio::spawn(async move {
            match tls {
                Some(acceptor) => {
                    if let Ok(Ok(stream)) =
                        tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await
                    {
                        serve_conn(stream, conn, ip, &hub).await;
                    }
                }
                None => serve_conn(tcp, conn, ip, &hub).await,
            }
            hub.lock()
                .unwrap_or_else(|e| e.into_inner())
                .close_conn(conn, ip);
        });
    }
}

pub fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_SIGNAL_MESSAGE))
        .max_frame_size(Some(MAX_SIGNAL_MESSAGE))
}

fn encode(msg: &SignalServerMsg) -> Message {
    Message::binary(msg.encode_to_vec())
}

async fn serve_conn<S>(stream: S, conn: u64, ip: IpAddr, hub: &Arc<Mutex<Hub>>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let ws = match tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        tokio_tungstenite::accept_async_with_config(stream, Some(ws_config())),
    )
    .await
    {
        Ok(Ok(ws)) => ws,
        _ => return,
    };
    let (mut sink, mut stream) = ws.split();

    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let challenge: SignalServerMsg = ServerBody::Challenge(Challenge {
        proto: SIGNAL_PROTOCOL,
        nonce: nonce.to_vec(),
    })
    .into();
    if sink.send(encode(&challenge)).await.is_err() {
        return;
    }

    // The first message must be a valid Register.
    let first = tokio::time::timeout(REGISTER_TIMEOUT, next_binary(&mut stream)).await;
    let register = match first {
        Ok(Some(bytes)) => match SignalClientMsg::decode(bytes.as_slice()) {
            Ok(SignalClientMsg {
                body: Some(ClientBody::Register(r)),
            }) => r,
            _ => {
                let _ = sink
                    .send(encode(&error(ErrorCode::NotRegistered, "register first")))
                    .await;
                return;
            }
        },
        _ => return,
    };
    let (tx, mut rx) = mpsc::channel::<SignalServerMsg>(OUTBOX);
    let registered = hub.lock().unwrap_or_else(|e| e.into_inner()).register(
        conn,
        ip,
        &nonce,
        &register,
        tx,
        Instant::now(),
    );
    match registered {
        Ok(reply) => {
            if sink.send(encode(&reply)).await.is_err() {
                return;
            }
        }
        Err(reply) => {
            let _ = sink.send(encode(&reply)).await;
            let _ = sink.close().await;
            return;
        }
    }

    let writer = tokio::spawn(async move {
        let mut ping = tokio::time::interval(PING_EVERY);
        ping.tick().await;
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(m) => {
                        // Being replaced ends this connection after the notice.
                        let last = matches!(&m.body, Some(ServerBody::Error(e)) if e.code == ErrorCode::Unspecified as i32);
                        if sink.send(encode(&m)).await.is_err() || last {
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

    let rate = {
        let l = hub.lock().unwrap_or_else(|e| e.into_inner());
        let (r, b) = l.limits().messages_per_conn;
        Rate::new(r, b)
    };
    let mut bucket = Bucket::full(rate, Instant::now());
    let mut strikes = 0u32;
    loop {
        let next = tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await;
        let msg = match next {
            Ok(Some(Ok(m))) => m,
            _ => break,
        };
        let bytes = match msg {
            Message::Binary(b) => b,
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => break,
            Message::Text(_) | Message::Frame(_) => break,
        };
        let now = Instant::now();
        if !bucket.take(rate, 1.0, now) {
            strikes += 1;
            if strikes > 100 {
                break;
            }
            continue;
        }
        let body = match SignalClientMsg::decode(bytes.as_ref()) {
            Ok(SignalClientMsg { body: Some(b) }) => b,
            // Unknown messages from a newer client are ignored; garbage ends it.
            Ok(SignalClientMsg { body: None }) => continue,
            Err(_) => break,
        };
        let mut h = hub.lock().unwrap_or_else(|e| e.into_inner());
        if !h.is_current(conn) {
            break;
        }
        h.handle(conn, body, now);
    }
    writer.abort();
}

async fn next_binary<S>(stream: &mut S) -> Option<Vec<u8>>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match stream.next().await? {
            Ok(Message::Binary(b)) => return Some(b.to_vec()),
            Ok(Message::Ping(_) | Message::Pong(_)) => continue,
            _ => return None,
        }
    }
}
