//! Shared helpers. Everything uses real QUIC over `VirtualSocket`, in-memory identities
//! and temp directories. Nothing here touches the Keychain.

#![allow(dead_code)]

use std::{
    net::SocketAddr,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use bytes::BytesMut;
use nexa_auth::{
    AuthMethod, DeviceIdentity, HelloAuth, HostPairing, HostPassword, HostVerified, OneTimeCode,
    PairError, PairingContext, PasswordGuard, Source, ViewerEvent, ViewerPairing, ViewerVerified,
    hello_auth, pair_exporter, peer_public_key,
};
use nexa_net::vsock::{VIRTUAL_PEER, VirtualSocket, bind_media_socket};
use nexa_wire::{
    control::{Auth, AuthStep, ClientHello},
    framing::{DEFAULT_MAX_FRAME, FrameDecoder, StreamType, encode_frame},
};
use prost::Message;
use quinn::{
    ClientConfig, Connection, Endpoint, EndpointConfig, RecvStream, Runtime, SendStream,
    ServerConfig, TokioRuntime,
};

pub const LIMIT: Duration = Duration::from_secs(20);

/// A different source on every call, so tests about the shared budget are not
/// stopped by the per-source share.
#[allow(dead_code)]
pub fn test_source() -> Source {
    use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
    static NEXT: AtomicU32 = AtomicU32::new(1);
    let n = NEXT.fetch_add(1, Relaxed).to_be_bytes();
    Source::of(std::net::IpAddr::from([10, n[1], n[2], n[3]]))
}

fn clock() -> Box<dyn Fn() -> u64 + Send + Sync> {
    static START: OnceLock<Instant> = OnceLock::new();
    let s = *START.get_or_init(Instant::now);
    Box::new(move || s.elapsed().as_micros() as u64)
}

/// One viewer endpoint and one host endpoint, joined by loopback UDP.
pub struct Link {
    pub viewer: Result<Connection, String>,
    pub host: Result<Connection, String>,
    _endpoints: (Endpoint, Endpoint),
}

/// Runs both halves of a QUIC handshake and reports how each half ended.
pub async fn link(client: ClientConfig, server: ServerConfig) -> Link {
    let any: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let viewer_udp = bind_media_socket(any).unwrap();
    let host_udp = bind_media_socket(any).unwrap();
    let (viewer_addr, host_addr) = (
        viewer_udp.local_addr().unwrap(),
        host_udp.local_addr().unwrap(),
    );
    let rt = Arc::new(TokioRuntime);
    let socket = |udp, peer| {
        Arc::new(VirtualSocket::new(
            rt.wrap_udp_socket(udp).unwrap(),
            peer,
            Box::new(|_, _| {}),
            clock(),
        ))
    };
    let host_ep = Endpoint::new_with_abstract_socket(
        EndpointConfig::default(),
        Some(server),
        socket(host_udp, viewer_addr),
        rt.clone(),
    )
    .unwrap();
    let mut viewer_ep = Endpoint::new_with_abstract_socket(
        EndpointConfig::default(),
        None,
        socket(viewer_udp, host_addr),
        rt.clone(),
    )
    .unwrap();
    viewer_ep.set_default_client_config(client);

    let accept = tokio::spawn({
        let ep = host_ep.clone();
        async move {
            let incoming = ep.accept().await.ok_or("endpoint closed")?;
            incoming.await.map_err(|e| e.to_string())
        }
    });
    let viewer = tokio::time::timeout(
        LIMIT,
        viewer_ep
            .connect(VIRTUAL_PEER, nexa_auth::tls::SERVER_NAME)
            .unwrap(),
    )
    .await
    .expect("viewer handshake timed out")
    .map_err(|e| e.to_string());
    let host = tokio::time::timeout(LIMIT, accept)
        .await
        .expect("host handshake timed out")
        .expect("host task");
    Link {
        viewer,
        host,
        _endpoints: (viewer_ep, host_ep),
    }
}

/// A Control stream: type byte first, then framed protobuf (§3.1).
pub struct Control {
    send: SendStream,
    recv: RecvStream,
    decoder: FrameDecoder,
}

impl Control {
    pub async fn open(conn: &Connection) -> Result<Self, String> {
        let (mut send, recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
        send.write_all(&[StreamType::Control as u8])
            .await
            .map_err(|e| e.to_string())?;
        Ok(Self {
            send,
            recv,
            decoder: FrameDecoder::new(DEFAULT_MAX_FRAME),
        })
    }

    pub async fn accept(conn: &Connection) -> Result<Self, String> {
        let (send, mut recv) = conn.accept_bi().await.map_err(|e| e.to_string())?;
        let mut kind = [0u8; 1];
        recv.read_exact(&mut kind)
            .await
            .map_err(|e| e.to_string())?;
        assert_eq!(kind[0], StreamType::Control as u8);
        Ok(Self {
            send,
            recv,
            decoder: FrameDecoder::new(DEFAULT_MAX_FRAME),
        })
    }

    pub async fn send(&mut self, message: &impl Message) -> Result<(), String> {
        let mut frame = BytesMut::new();
        encode_frame(&message.encode_to_vec(), &mut frame);
        self.send.write_all(&frame).await.map_err(|e| e.to_string())
    }

    pub async fn recv<M: Message + Default>(&mut self) -> Result<M, String> {
        loop {
            if let Some(frame) = self.decoder.next_frame().map_err(|e| e.to_string())? {
                return M::decode(frame).map_err(|e| e.to_string());
            }
            let mut buf = [0u8; 4096];
            match self.recv.read(&mut buf).await.map_err(|e| e.to_string())? {
                Some(n) => self.decoder.push(&buf[..n]),
                None => return Err("stream finished".into()),
            }
        }
    }
}

pub fn hello(auth: Auth) -> ClientHello {
    ClientHello {
        proto: 1,
        device_name: "test viewer".into(),
        os: 0,
        caps: None,
        auth: Some(auth),
    }
}

#[derive(Debug, PartialEq)]
pub enum Failure {
    Pair(PairError),
    Transport(String),
}

impl From<PairError> for Failure {
    fn from(e: PairError) -> Self {
        Failure::Pair(e)
    }
}

impl From<String> for Failure {
    fn from(e: String) -> Self {
        Failure::Transport(e)
    }
}

pub fn context_as_viewer(me: &DeviceIdentity, conn: &Connection) -> PairingContext {
    PairingContext::new(
        me.public_key(),
        peer_public_key(conn).unwrap(),
        pair_exporter(conn).unwrap(),
    )
    .unwrap()
}

pub fn context_as_host(me: &DeviceIdentity, conn: &Connection) -> PairingContext {
    PairingContext::new(
        peer_public_key(conn).unwrap(),
        me.public_key(),
        pair_exporter(conn).unwrap(),
    )
    .unwrap()
}

/// What the viewer knows: a typed code or a password.
pub enum Typed<'a> {
    Code(&'a str),
    Password(&'a str),
}

pub async fn run_viewer(
    conn: &Connection,
    me: &DeviceIdentity,
    secret: Typed<'_>,
) -> Result<ViewerVerified, Failure> {
    let ctx = context_as_viewer(me, conn);
    let (mut machine, auth) = match secret {
        Typed::Code(c) => (ViewerPairing::with_code(ctx, c)?, Auth::Code(true)),
        Typed::Password(p) => (ViewerPairing::with_password(ctx, p)?, Auth::Password(true)),
    };
    let mut control = Control::open(conn).await?;
    control.send(&hello(auth)).await?;
    loop {
        let step: AuthStep = control.recv().await?;
        match machine.on_step(&step)? {
            ViewerEvent::Send(reply) => control.send(&reply).await?,
            ViewerEvent::Complete(verified) => return Ok(verified),
        }
    }
}

/// What the host accepts.
pub enum Accepts<'a> {
    Code(&'a OneTimeCode),
    Password(&'a HostPassword, &'a PasswordGuard),
}

pub async fn run_host(
    conn: &Connection,
    me: &DeviceIdentity,
    accepts: Accepts<'_>,
) -> Result<HostVerified, Failure> {
    let ctx = context_as_host(me, conn);
    let mut control = Control::accept(conn).await?;
    let hello: ClientHello = control.recv().await?;
    let (mut machine, offer) = match (hello_auth(&hello)?, accepts) {
        (HelloAuth::Code, Accepts::Code(code)) => HostPairing::with_code(ctx, code, test_source())?,
        (HelloAuth::Password, Accepts::Password(stored, guard)) => {
            HostPairing::with_password(ctx, stored, guard, test_source())?
        }
        _ => {
            return Err(Failure::Pair(PairError::Malformed(
                "auth method not offered",
            )));
        }
    };
    control.send(&offer).await?;
    let step: AuthStep = control.recv().await?;
    match machine.on_step(&step) {
        Ok(outcome) => {
            control.send(&outcome.reply).await?;
            Ok(outcome.verified)
        }
        Err(e) => {
            conn.close(1u32.into(), b"pairing failed");
            Err(e.into())
        }
    }
}

/// A completed in-memory code exchange, for tests that need real proof objects.
pub fn pair_in_memory(
    viewer: &DeviceIdentity,
    host: &DeviceIdentity,
) -> (ViewerVerified, HostVerified) {
    let code = OneTimeCode::generate();
    let shown = code.current();
    let exporter = [42u8; 32];
    let ctx = || PairingContext::new(viewer.public_key(), host.public_key(), exporter).unwrap();
    let (mut h, offer) = HostPairing::with_code(ctx(), &code, test_source()).unwrap();
    let mut v = ViewerPairing::with_code(ctx(), &shown).unwrap();
    let ViewerEvent::Send(reply) = v.on_step(&offer).unwrap() else {
        panic!("viewer should reply");
    };
    let outcome = h.on_step(&reply).unwrap();
    let ViewerEvent::Complete(viewer_proof) = v.on_step(&outcome.reply).unwrap() else {
        panic!("viewer should complete");
    };
    assert_eq!(viewer_proof.method(), AuthMethod::Code);
    (viewer_proof, outcome.verified)
}
