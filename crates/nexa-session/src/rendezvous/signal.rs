//! The signaling client (§4.3): one WebSocket to `nexa-signal`, registered under
//! this device's key. Everything the server sends is treated as hostile.

use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use nexa_wire::signal::{
    ClientBody, ErrorCode, MAX_SIGNAL_MESSAGE, NONCE_LEN, Register, SIGNAL_PROTOCOL, ServerBody,
    SignalClientMsg, SignalServerMsg, normalize_short_id, register_message,
};
use prost::Message as _;
use rustls::pki_types::{CertificateDer, ServerName};
use thiserror::Error;
use tokio::{net::TcpStream, sync::mpsc, task::JoinHandle};
use tokio_tungstenite::tungstenite::{Message, protocol::WebSocketConfig};

use super::seal::SignalKeys;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const PING_EVERY: Duration = Duration::from_secs(20);
/// The server pings every 30 s; this much silence means it is gone.
const IDLE_TIMEOUT: Duration = Duration::from_secs(75);
const MAX_STUN_SERVERS: usize = 4;

#[derive(Debug, Error)]
pub enum SignalError {
    #[error("bad server address: {0}")]
    BadUrl(String),
    #[error("cannot reach the server: {0}")]
    Unreachable(String),
    #[error("TLS: {0}")]
    Tls(String),
    #[error("the server refused: {0}")]
    Refused(String),
    #[error("the server sent something unexpected")]
    Protocol,
    #[error("timed out talking to the server")]
    Timeout,
    #[error("the server is gone")]
    Closed,
}

/// Where the server is and how to trust it.
#[derive(Clone, Debug)]
pub struct SignalConfig {
    pub url: String,
    /// Extra roots, e.g. a self-hosted server's own CA. The built-in Mozilla
    /// roots are always trusted as well.
    pub extra_roots: Vec<CertificateDer<'static>>,
    /// Trust only `extra_roots` (tests; a private deployment).
    pub only_extra_roots: bool,
}

impl SignalConfig {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            extra_roots: Vec::new(),
            only_extra_roots: false,
        }
    }

    /// Also trusts the certificates in a PEM file (a self-hosted server's CA).
    pub fn with_ca_file(mut self, path: &std::path::Path) -> Result<Self, String> {
        use rustls::pki_types::pem::PemObject;
        let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(path)
            .map_err(|e| format!("{}: {e}", path.display()))?
            .collect::<Result<_, _>>()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if certs.is_empty() {
            return Err(format!("{}: no certificate", path.display()));
        }
        self.extra_roots.extend(certs);
        Ok(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub tls: bool,
    pub host: String,
    pub port: u16,
}

/// `wss://host[:port][/path]`, `ws://host:port` (loopback only) or `host[:port]`
/// (taken as wss). The path is ignored.
pub fn parse_url(url: &str) -> Result<Endpoint, SignalError> {
    let url = url.trim();
    let bad = || SignalError::BadUrl(url.to_owned());
    let (tls, rest) = if let Some(r) = url.strip_prefix("wss://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("ws://") {
        (false, r)
    } else if url.contains("://") {
        return Err(bad());
    } else {
        (true, url)
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return Err(bad());
    }
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (h, after) = v6.split_once(']').ok_or_else(bad)?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().map_err(|_| bad())?,
            None if after.is_empty() => 443,
            None => return Err(bad()),
        };
        (h.to_owned(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_owned(), p.parse::<u16>().map_err(|_| bad())?),
            None => (authority.to_owned(), if tls { 443 } else { 80 }),
        }
    };
    if host.is_empty()
        || port == 0
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-.:".contains(&b))
    {
        return Err(bad());
    }
    if !tls {
        let loopback =
            host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
        if !loopback {
            return Err(SignalError::BadUrl(format!(
                "{url}: plain ws:// is only allowed to this machine; use wss://"
            )));
        }
    }
    Ok(Endpoint { tls, host, port })
}

/// What the server told us at registration. Addresses are validated here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registration {
    pub short_id: String,
    pub stun_servers: Vec<SocketAddr>,
    pub stun_note: Option<String>,
}

/// A registered connection. Dropping it closes the socket.
pub struct SignalClient {
    out: mpsc::Sender<SignalClientMsg>,
    inbox: mpsc::Receiver<ServerBody>,
    pub registration: Registration,
    task: JoinHandle<()>,
}

impl Drop for SignalClient {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A cloneable way to send on the connection.
#[derive(Clone)]
pub struct SignalSender(mpsc::Sender<SignalClientMsg>);

impl SignalSender {
    /// Never blocks; a full queue drops, as UDP-like signaling allows.
    pub fn send(&self, body: ClientBody) -> Result<(), SignalError> {
        self.0
            .try_send(body.into())
            .map_err(|_| SignalError::Closed)
    }
}

pub(crate) trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Io for T {}

pub(crate) type WsStream = tokio_tungstenite::WebSocketStream<Box<dyn Io>>;

/// TCP, TLS when the URL says so, and the WebSocket upgrade on `path`.
pub(crate) async fn connect_ws(
    cfg: &SignalConfig,
    path: &str,
    ws: WebSocketConfig,
) -> Result<(WsStream, Endpoint), SignalError> {
    let ep = parse_url(&cfg.url)?;
    let tcp = TcpStream::connect((ep.host.as_str(), ep.port))
        .await
        .map_err(|e| SignalError::Unreachable(e.to_string()))?;
    let _ = tcp.set_nodelay(true);
    let stream: Box<dyn Io> = if ep.tls {
        let name = ServerName::try_from(ep.host.clone())
            .map_err(|_| SignalError::BadUrl(cfg.url.clone()))?;
        let tls = tokio_rustls::TlsConnector::from(tls_config(cfg)?)
            .connect(name, tcp)
            .await
            .map_err(|e| SignalError::Tls(e.to_string()))?;
        Box::new(tls)
    } else {
        Box::new(tcp)
    };
    let scheme = if ep.tls { "wss" } else { "ws" };
    let host = if ep.host.contains(':') {
        format!("[{}]", ep.host)
    } else {
        ep.host.clone()
    };
    let url = format!("{scheme}://{host}:{}{path}", ep.port);
    let (ws, _) = tokio_tungstenite::client_async_with_config(url, stream, Some(ws))
        .await
        .map_err(|e| SignalError::Unreachable(e.to_string()))?;
    Ok((ws, ep))
}

fn tls_config(cfg: &SignalConfig) -> Result<Arc<rustls::ClientConfig>, SignalError> {
    let mut roots = rustls::RootCertStore::empty();
    if !cfg.only_extra_roots {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }
    for c in &cfg.extra_roots {
        roots
            .add(c.clone())
            .map_err(|e| SignalError::Tls(e.to_string()))?;
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| SignalError::Tls(e.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_SIGNAL_MESSAGE))
        .max_frame_size(Some(MAX_SIGNAL_MESSAGE))
}

fn decode(m: Message) -> Result<Option<ServerBody>, SignalError> {
    match m {
        Message::Binary(b) => match SignalServerMsg::decode(b.as_ref()) {
            Ok(SignalServerMsg { body }) => Ok(body),
            Err(_) => Err(SignalError::Protocol),
        },
        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => Ok(None),
        Message::Text(_) => Err(SignalError::Protocol),
        Message::Close(_) => Err(SignalError::Closed),
    }
}

fn refusal(e: &nexa_wire::signal::SignalError) -> SignalError {
    let code = ErrorCode::try_from(e.code).unwrap_or(ErrorCode::Unspecified);
    // The message is the server's text: keep it short and printable.
    let text: String = e
        .message
        .chars()
        .filter(|c| !c.is_control())
        .take(120)
        .collect();
    SignalError::Refused(format!("{code:?}: {text}"))
}

impl SignalClient {
    pub async fn connect(cfg: &SignalConfig, keys: &SignalKeys) -> Result<Self, SignalError> {
        tokio::time::timeout(CONNECT_TIMEOUT, Self::connect_inner(cfg, keys))
            .await
            .map_err(|_| SignalError::Timeout)?
    }

    async fn connect_inner(cfg: &SignalConfig, keys: &SignalKeys) -> Result<Self, SignalError> {
        let (ws, ep) = connect_ws(cfg, "/", ws_config()).await?;
        let (mut sink, mut stream) = ws.split();

        let nonce = loop {
            let m = stream
                .next()
                .await
                .ok_or(SignalError::Closed)?
                .map_err(|_| SignalError::Closed)?;
            match decode(m)? {
                Some(ServerBody::Challenge(c)) => {
                    if c.proto != SIGNAL_PROTOCOL {
                        return Err(SignalError::Refused("unsupported server version".into()));
                    }
                    let n: [u8; NONCE_LEN] = c
                        .nonce
                        .as_slice()
                        .try_into()
                        .map_err(|_| SignalError::Protocol)?;
                    break n;
                }
                None => continue,
                Some(_) => return Err(SignalError::Protocol),
            }
        };
        let sig = keys.identity().sign_for(
            nexa_wire::signal::PURPOSE_REGISTER,
            &register_message(&nonce, keys.x25519(), &ep.host),
        );
        let register = SignalClientMsg::from(ClientBody::Register(Register {
            proto: SIGNAL_PROTOCOL,
            pubkey: keys.public_key().as_bytes().to_vec(),
            x25519: keys.x25519().to_vec(),
            x25519_sig: keys.x25519_sig().to_vec(),
            sig: sig.to_vec(),
            server_name: ep.host.clone(),
        }));
        sink.send(Message::binary(register.encode_to_vec()))
            .await
            .map_err(|_| SignalError::Closed)?;
        let registration = loop {
            let m = stream
                .next()
                .await
                .ok_or(SignalError::Closed)?
                .map_err(|_| SignalError::Closed)?;
            match decode(m)? {
                Some(ServerBody::Registered(r)) => {
                    let short_id = normalize_short_id(&r.short_id).ok_or(SignalError::Protocol)?;
                    let stun_servers = r
                        .stun_servers
                        .iter()
                        .take(MAX_STUN_SERVERS)
                        .filter_map(|s| s.parse::<SocketAddr>().ok())
                        .filter(|a| {
                            a.port() != 0 && !a.ip().is_unspecified() && !a.ip().is_multicast()
                        })
                        .collect();
                    let note: String = r
                        .stun_note
                        .chars()
                        .filter(|c| !c.is_control())
                        .take(200)
                        .collect();
                    break Registration {
                        short_id,
                        stun_servers,
                        stun_note: (!note.is_empty()).then_some(note),
                    };
                }
                Some(ServerBody::Error(e)) => return Err(refusal(&e)),
                None => continue,
                Some(_) => return Err(SignalError::Protocol),
            }
        };

        let (out_tx, mut out_rx) = mpsc::channel::<SignalClientMsg>(64);
        let (in_tx, inbox) = mpsc::channel::<ServerBody>(64);
        let task = tokio::spawn(async move {
            let mut ping = tokio::time::interval(PING_EVERY);
            ping.tick().await;
            loop {
                tokio::select! {
                    m = out_rx.recv() => match m {
                        Some(m) => {
                            if sink.send(Message::binary(m.encode_to_vec())).await.is_err() {
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
                    m = tokio::time::timeout(IDLE_TIMEOUT, stream.next()) => match m {
                        Ok(Some(Ok(m))) => match decode(m) {
                            Ok(Some(body)) => {
                                // A flood from the server is dropped rather than queued.
                                let _ = in_tx.try_send(body);
                            }
                            Ok(None) => {}
                            Err(_) => break,
                        },
                        _ => break,
                    },
                }
            }
            let _ = sink.close().await;
        });
        Ok(Self {
            out: out_tx,
            inbox,
            registration,
            task,
        })
    }

    pub fn sender(&self) -> SignalSender {
        SignalSender(self.out.clone())
    }

    pub fn send(&self, body: ClientBody) -> Result<(), SignalError> {
        self.sender().send(body)
    }

    /// The next message from the server; `None` once the connection is gone.
    pub async fn recv(&mut self) -> Option<ServerBody> {
        self.inbox.recv().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_server_addresses() {
        let e = |tls, host: &str, port| Endpoint {
            tls,
            host: host.into(),
            port,
        };
        assert_eq!(
            parse_url("wss://signal.example.com").unwrap(),
            e(true, "signal.example.com", 443)
        );
        assert_eq!(
            parse_url("signal.example.com:8443").unwrap(),
            e(true, "signal.example.com", 8443)
        );
        assert_eq!(
            parse_url("wss://signal.example.com:8443/ws").unwrap(),
            e(true, "signal.example.com", 8443)
        );
        assert_eq!(
            parse_url("ws://127.0.0.1:9000").unwrap(),
            e(false, "127.0.0.1", 9000)
        );
        assert_eq!(parse_url("wss://[::1]:9000").unwrap(), e(true, "::1", 9000));
        assert!(
            parse_url("ws://signal.example.com:80").is_err(),
            "plain ws off this machine"
        );
        assert!(parse_url("https://signal.example.com").is_err());
        assert!(parse_url("wss://user@signal.example.com").is_err());
        assert!(parse_url("wss://").is_err());
        assert!(parse_url("wss://host:0").is_err());
        assert!(parse_url("wss://ho st").is_err());
    }
}
