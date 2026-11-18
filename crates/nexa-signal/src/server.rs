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
    relay::{Relay, RelayIo, RelayLimits, TokenIssuer},
};

/// The WebSocket path of the relay fallback (§4.6); anything else is signaling.
pub const RELAY_PATH: &str = "/relay";

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
    /// Empty: anyone may register. See `config::SignalSection::access_secrets`.
    pub access_secrets: Vec<String>,
    /// What clients are told; one entry per STUN socket.
    pub stun_advertise: Vec<SocketAddr>,
    pub relay_advertise: Option<SocketAddr>,
    pub relay: RelayLimits,
    pub relay_token_ttl_secs: u64,
    pub limits: Limits,
    /// Browsers anywhere: the page here, TURN on the first STUN socket.
    pub web: Option<crate::config::WebSection>,
}

pub struct Server {
    pub ws_addr: SocketAddr,
    hub: Arc<Mutex<Hub>>,
    relay: Arc<Mutex<Relay>>,
    relay_io: Arc<RelayIo>,
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

    /// Relay ends connected over WebSocket right now.
    pub fn relay_streams(&self) -> usize {
        self.relay_io.stream_count()
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
    // TURN takes the first STUN socket, and answers its Binding requests as before.
    let turn = match (&settings.web, settings.stun_advertise.first()) {
        (Some(w), Some(advertised)) if !sockets.stun.is_empty() => {
            let mut secret = vec![0u8; 32];
            OsRng.fill_bytes(&mut secret);
            let mut nonce_key = [0u8; 32];
            OsRng.fill_bytes(&mut nonce_key);
            let server = nexa_turn::Server::new(
                nexa_turn::ServerConfig {
                    realm: "nexadesk".into(),
                    secret: secret.clone(),
                    relay_ip: advertised.ip(),
                    max_allocations: w.max_allocations,
                    max_per_ip: w.max_per_ip,
                },
                nonce_key,
            );
            Some((
                Arc::new(Mutex::new(server)),
                crate::hub::WebTurn {
                    secret,
                    server: *advertised,
                    password: w.password.clone(),
                },
            ))
        }
        _ => None,
    };
    let hub = Arc::new(Mutex::new(Hub::new(HubSettings {
        limits: settings.limits.clone(),
        server_names: settings.server_names.clone(),
        access_secrets: settings.access_secrets.clone(),
        stun: settings.stun_advertise.clone(),
        stun_note,
        relay: settings.relay_advertise.filter(|_| sockets.relay.is_some()),
        relay_ttl_secs: settings.relay_token_ttl_secs,
        issuer: issuer.clone(),
        web: turn.as_ref().map(|(_, w)| w.clone()),
    })));
    let relay = Arc::new(Mutex::new(Relay::new(
        issuer.verifying_key(),
        settings.relay,
    )));
    let mut tasks = Vec::new();
    let stun_rate = Rate::new(settings.limits.stun_per_ip.0, settings.limits.stun_per_ip.1);
    for (i, s) in sockets.stun.into_iter().enumerate() {
        match (&turn, i) {
            (Some((t, _)), 0) => {
                tasks.push(tokio::spawn(crate::turn::serve(s, t.clone(), stun_rate)))
            }
            _ => tasks.push(tokio::spawn(crate::stun::serve(s, stun_rate))),
        }
    }
    let relay_io = Arc::new(RelayIo::new(sockets.relay.clone()));
    if let Some(s) = sockets.relay {
        tasks.push(tokio::spawn(crate::relay::serve(
            s,
            relay.clone(),
            relay_io.clone(),
        )));
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
    let ends = Ends {
        hub: hub.clone(),
        relay: relay.clone(),
        relay_io: relay_io.clone(),
        web: turn.is_some(),
        web_password: turn.as_ref().is_some_and(|(_, w)| w.password.is_some()),
    };
    tasks.push(tokio::spawn(accept_loop(sockets.ws, acceptor, ends)));
    Ok(Server {
        ws_addr,
        hub,
        relay,
        relay_io,
        tasks,
    })
}

/// What a WebSocket connection can be served by.
#[derive(Clone)]
struct Ends {
    hub: Arc<Mutex<Hub>>,
    relay: Arc<Mutex<Relay>>,
    relay_io: Arc<RelayIo>,
    /// Browser access is on: the page and `/web` are served.
    web: bool,
    /// ...and browsers are asked for a password.
    web_password: bool,
}

async fn accept_loop(listener: TcpListener, tls: Option<TlsAcceptor>, ends: Ends) {
    let hub = ends.hub.clone();
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
        let (hub, tls, ends) = (hub.clone(), tls.clone(), ends.clone());
        tokio::spawn(async move {
            match tls {
                Some(acceptor) => {
                    if let Ok(Ok(stream)) =
                        tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await
                    {
                        route_conn(stream, conn, ip, &ends, true).await;
                    }
                }
                None => route_conn(tcp, conn, ip, &ends, false).await,
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

/// Reads the request: the page for a plain GET (with browser access on), else a
/// WebSocket for signaling, the relay on [`RELAY_PATH`], or a browser on
/// [`crate::web::WEB_PATH`].
async fn route_conn<S>(mut stream: S, conn: u64, ip: IpAddr, ends: &Ends, secure: bool)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    use nexa_web_page::http::{
        Io, MAX_HEAD, Rewind, parse_head, respond, respond_len, same_origin,
    };
    use tokio::io::AsyncReadExt;

    let deadline = tokio::time::Instant::now() + HANDSHAKE_TIMEOUT;
    let mut buf = vec![0u8; MAX_HEAD];
    let mut n = 0;
    let head = loop {
        match parse_head(&buf[..n]) {
            Some(Ok(h)) => break h,
            Some(Err(())) => return respond(&mut stream, 400, "text/plain", b"bad request").await,
            None if n == MAX_HEAD => return respond(&mut stream, 431, "text/plain", b"").await,
            None => {}
        }
        match tokio::time::timeout_at(deadline, stream.read(&mut buf[n..])).await {
            Ok(Ok(read)) if read > 0 => n += read,
            _ => return,
        }
    };

    if !head.upgrade_ws {
        // The page, for browsers anywhere; nothing else speaks plain HTTP here.
        if ends.web && (head.method == "GET" || head.method == "HEAD") {
            if head.path == "/mode" {
                return respond(
                    &mut stream,
                    200,
                    "application/json",
                    if ends.web_password {
                        nexa_web_page::MODE_SERVER_PASSWORD
                    } else {
                        nexa_web_page::MODE_SERVER
                    },
                )
                .await;
            }
            if let Some(a) = nexa_web_page::assets::find(&head.path) {
                let body = if head.method == "HEAD" {
                    &[][..]
                } else {
                    a.body
                };
                return respond_len(&mut stream, 200, a.content_type, body, a.body.len(), secure)
                    .await;
            }
        }
        return respond(&mut stream, 404, "text/plain", b"not found").await;
    }

    let path = head.path.clone();
    let browser = path == crate::web::WEB_PATH;
    if browser && (!ends.web || !same_origin(&head)) {
        return respond(&mut stream, 403, "text/plain", b"forbidden").await;
    }
    let replay = Rewind::new(buf[..n].to_vec(), Box::new(stream) as Box<dyn Io>);
    let config = if browser {
        WebSocketConfig::default()
            .max_message_size(Some(crate::web::MAX_BROWSER_MESSAGE))
            .max_frame_size(Some(crate::web::MAX_BROWSER_MESSAGE))
    } else {
        ws_config()
    };
    let ws = match tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        tokio_tungstenite::accept_async_with_config(replay, Some(config)),
    )
    .await
    {
        Ok(Ok(ws)) => ws,
        _ => return,
    };
    if path == RELAY_PATH {
        crate::relay::serve_stream(ws, conn, ip, ends.relay.clone(), ends.relay_io.clone()).await;
    } else if browser {
        crate::web::serve(ws, ip, ends.hub.clone()).await;
    } else {
        serve_conn(ws, conn, ip, &ends.hub).await;
    }
}

async fn serve_conn<S>(
    ws: tokio_tungstenite::WebSocketStream<S>,
    conn: u64,
    ip: IpAddr,
    hub: &Arc<Mutex<Hub>>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
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
