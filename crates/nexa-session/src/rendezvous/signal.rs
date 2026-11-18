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
    /// Mixed into the device ID the server gives this device; a new seed, a new ID.
    pub id_seed: Vec<u8>,
}

impl SignalConfig {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            extra_roots: Vec::new(),
            only_extra_roots: false,
            id_seed: Vec::new(),
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

/// What a person typed or pasted for the server, as [`parse_url`] takes it and in one
/// form: `wss://host:port`, or `ws://host:port` to this machine. A browser's address
/// works too (the server serves a page on its signaling port): `https://` is taken as
/// `wss://` and `http://` as `ws://`.
pub fn canonical_url(input: &str) -> Result<String, SignalError> {
    let t = input.trim();
    let lower = t.to_ascii_lowercase();
    let url = if lower.starts_with("https://") {
        format!("wss://{}", &t[8..])
    } else if lower.starts_with("http://") {
        format!("ws://{}", &t[7..])
    } else if lower.starts_with("wss://") || lower.starts_with("ws://") {
        // The scheme in lower case, as parse_url compares it.
        let (scheme, rest) = t.split_once("://").unwrap_or(("", t));
        format!("{}://{rest}", scheme.to_ascii_lowercase())
    } else {
        t.to_owned()
    };
    let e = parse_url(&url)?;
    let host = if e.host.contains(':') {
        format!("[{}]", e.host)
    } else {
        e.host.to_ascii_lowercase()
    };
    Ok(format!(
        "{}://{host}:{}",
        if e.tls { "wss" } else { "ws" },
        e.port
    ))
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

/// A server's certificate, for trusting a self-hosted server on first use: shown to
/// the person as a fingerprint, saved as a root once they accept it.
#[derive(Clone, Debug)]
pub struct ServerCertificate {
    /// Already trusted (a public CA, or a root saved earlier): nothing to ask.
    pub trusted: bool,
    /// The certificate to trust: the top of the chain the server sent (its own CA,
    /// or the certificate itself when it is self-signed).
    pub anchor: CertificateDer<'static>,
    /// SHA-256 of `anchor`, as AA:BB:…
    pub fingerprint: String,
}

impl ServerCertificate {
    /// `anchor` as PEM, for signal-ca.pem.
    pub fn pem(&self) -> String {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(self.anchor.as_ref());
        let mut out = String::from("-----BEGIN CERTIFICATE-----\n");
        for line in b64.as_bytes().chunks(64) {
            out.push_str(std::str::from_utf8(line).unwrap_or_default());
            out.push('\n');
        }
        out.push_str("-----END CERTIFICATE-----\n");
        out
    }
}

/// SHA-256, upper-case hex pairs joined by colons (as browsers and openssl show it).
pub fn fingerprint(der: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Connects to the server's TLS port and says whether `cfg` trusts it. When it does
/// not, finds the certificate that would make it trusted, and checks that it really
/// does (the server's name must still match): a certificate that would not help is an
/// error, never something to offer.
pub async fn server_certificate(cfg: &SignalConfig) -> Result<ServerCertificate, SignalError> {
    let ep = parse_url(&cfg.url)?;
    if !ep.tls {
        return Err(SignalError::BadUrl(cfg.url.clone()));
    }
    let first = handshake(&ep, &cfg.url, tls_config(cfg)?).await;
    if let Ok(chain) = &first {
        let top = chain.last().cloned().ok_or(SignalError::Protocol)?;
        return Ok(ServerCertificate {
            trusted: true,
            fingerprint: fingerprint(top.as_ref()),
            anchor: top,
        });
    }
    // Not trusted: see what it presents, without trusting anything.
    let recorder = Arc::new(Recorder::default());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let looking = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| SignalError::Tls(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(recorder.clone())
        .with_no_client_auth();
    handshake(&ep, &cfg.url, Arc::new(looking)).await?;
    let chain = recorder.chain.lock().map(|c| c.clone()).unwrap_or_default();
    let anchor = chain.last().cloned().ok_or(SignalError::Protocol)?;
    // Would trusting it work?
    let mut with = cfg.clone();
    with.extra_roots.push(anchor.clone());
    handshake(&ep, &cfg.url, tls_config(&with)?)
        .await
        .map_err(|e| match e {
            SignalError::Tls(why) => SignalError::Tls(format!(
                "trusting the server's certificate would not help: {why}"
            )),
            other => other,
        })?;
    Ok(ServerCertificate {
        trusted: false,
        fingerprint: fingerprint(anchor.as_ref()),
        anchor,
    })
}

/// A TLS handshake and nothing else; the certificates the server sent.
async fn handshake(
    ep: &Endpoint,
    url: &str,
    config: Arc<rustls::ClientConfig>,
) -> Result<Vec<CertificateDer<'static>>, SignalError> {
    let run = async {
        let tcp = TcpStream::connect((ep.host.as_str(), ep.port))
            .await
            .map_err(|e| SignalError::Unreachable(e.to_string()))?;
        let name = ServerName::try_from(ep.host.clone())
            .map_err(|_| SignalError::BadUrl(url.to_owned()))?;
        let tls = tokio_rustls::TlsConnector::from(config)
            .connect(name, tcp)
            .await
            .map_err(|e| SignalError::Tls(e.to_string()))?;
        Ok(tls
            .get_ref()
            .1
            .peer_certificates()
            .map(|c| c.iter().map(|d| d.clone().into_owned()).collect())
            .unwrap_or_default())
    };
    tokio::time::timeout(PROBE_TIMEOUT, run)
        .await
        .map_err(|_| SignalError::Timeout)?
}

/// Accepts any certificate and keeps the chain: only for looking at what a server
/// presents. Signatures are still checked, so the chain is the server's own.
#[derive(Debug, Default)]
struct Recorder {
    chain: std::sync::Mutex<Vec<CertificateDer<'static>>>,
}

impl rustls::client::danger::ServerCertVerifier for Recorder {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if let Ok(mut chain) = self.chain.lock() {
            *chain = std::iter::once(end_entity)
                .chain(intermediates)
                .map(|c| c.clone().into_owned())
                .collect();
        }
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
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
            &register_message(&nonce, keys.x25519(), &ep.host, &cfg.id_seed),
        );
        let register = SignalClientMsg::from(ClientBody::Register(Register {
            proto: SIGNAL_PROTOCOL,
            pubkey: keys.public_key().as_bytes().to_vec(),
            x25519: keys.x25519().to_vec(),
            x25519_sig: keys.x25519_sig().to_vec(),
            sig: sig.to_vec(),
            server_name: ep.host.clone(),
            id_seed: cfg.id_seed.clone(),
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

    #[test]
    fn what_people_paste_becomes_one_server_address() {
        let ok =
            |input: &str, want: &str| assert_eq!(canonical_url(input).unwrap(), want, "{input}");
        ok("signal.example.com", "wss://signal.example.com:443");
        ok(" Signal.Example.com:8443 ", "wss://signal.example.com:8443");
        ok(
            "wss://signal.example.com:8443/",
            "wss://signal.example.com:8443",
        );
        ok("WSS://signal.example.com", "wss://signal.example.com:443");
        // The page the server serves on its signaling port, pasted from a browser.
        ok(
            "https://signal.example.com:8443/",
            "wss://signal.example.com:8443",
        );
        ok("http://127.0.0.1:8080", "ws://127.0.0.1:8080");
        ok("ws://localhost:47943", "ws://localhost:47943");
        ok("[2001:db8::1]:8443", "wss://[2001:db8::1]:8443");
        for bad in [
            "",
            "ftp://x",
            "http://signal.example.com",
            "user@host",
            "wss://",
            "host:0",
        ] {
            assert!(canonical_url(bad).is_err(), "{bad}");
        }
    }
    use super::*;

    /// A TLS server on 127.0.0.1 whose certificate for `name` is signed by its own CA;
    /// it sends the leaf and the CA, as a self-hosted server does.
    async fn self_signed_server(name: &str) -> (u16, CertificateDer<'static>) {
        use rustls::pki_types::PrivateKeyDer;
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "test CA");
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        let issuer = rcgen::Issuer::new(ca_params, ca_key);
        let leaf_key = rcgen::KeyPair::generate().unwrap();
        let leaf = rcgen::CertificateParams::new(vec![name.to_owned()])
            .unwrap()
            .signed_by(&leaf_key, &issuer)
            .unwrap();
        let ca_der = CertificateDer::from(ca_cert.der().to_vec());
        let chain = vec![CertificateDer::from(leaf.der().to_vec()), ca_der.clone()];
        let key = PrivateKeyDer::try_from(leaf_key.serialize_der()).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let _ = acceptor.accept(tcp).await;
                });
            }
        });
        (port, ca_der)
    }

    #[tokio::test]
    async fn an_untrusted_server_offers_its_ca_and_trusting_it_works() {
        let (port, ca) = self_signed_server("127.0.0.1").await;
        let cfg = SignalConfig::new(format!("wss://127.0.0.1:{port}"));
        let found = server_certificate(&cfg).await.unwrap();
        assert!(!found.trusted);
        assert_eq!(found.anchor, ca, "the top of the chain: the server's CA");
        assert_eq!(found.fingerprint, fingerprint(ca.as_ref()));
        assert_eq!(found.fingerprint.len(), 32 * 3 - 1);

        // Saved as PEM and read back the way the apps do, it is trusted.
        let dir = std::env::temp_dir().join(format!("nexa-trust-{}-{port}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pem = dir.join("signal-ca.pem");
        std::fs::write(&pem, found.pem()).unwrap();
        let cfg = cfg.with_ca_file(&pem).unwrap();
        let again = server_certificate(&cfg).await.unwrap();
        assert!(again.trusted);
        assert_eq!(again.fingerprint, found.fingerprint);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_certificate_for_another_name_is_never_offered() {
        let (port, _) = self_signed_server("signal.example.com").await;
        let cfg = SignalConfig::new(format!("wss://127.0.0.1:{port}"));
        match server_certificate(&cfg).await {
            Err(SignalError::Tls(why)) => assert!(why.contains("would not help"), "{why}"),
            other => panic!("expected a TLS error, got {other:?}"),
        }
    }

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
