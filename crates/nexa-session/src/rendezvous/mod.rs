//! Connecting by device ID (§4.2–§4.6, §5.3).
//!
//! ```text
//! viewer                         nexa-signal                       host
//!   Register ───────────────────▶ (both registered) ◀──────────── Register
//!   Lookup(id) ─────────────────▶ PeerInfo{pubkey, x25519, sig}
//!   gather; Connect{sealed offer} ▶ Incoming{from, sealed, relay} ─▶ open, gather
//!   ◀──────────────── Routed{relay}       Answer{sealed} ◀────────── seal answer
//!   ◀────────────── Answer ◀─────────────┘
//!   checks ◀═══════════════ STUN over UDP / relay ═══════════════▶ checks
//!   QUIC over VirtualSocket on the first validated path ══════════▶ accept
//!   then §9.2 exactly as for ip:port: TLS identity, pairing, consent.
//! ```
//!
//! The server only routes; its messages are parsed defensively and its claims
//! are checked: the peer's X25519 key must carry the peer's signature, the
//! sealed blobs are authenticated end to end, and after QUIC is up the TLS
//! identity must be the key the rendezvous was made with.

pub mod link;
pub mod seal;
pub mod signal;
pub mod wss;

use std::{net::SocketAddr, sync::Arc, time::Duration};

use nexa_auth::DevicePublicKey;
use nexa_wire::signal::{
    Answer, ClientBody, Connect, ErrorCode, Incoming, Lookup, Rendezvous, SESSION_ID_LEN,
    SealedKind, ServerBody, Trickle,
};
use prost::Message as _;
use thiserror::Error;
use tokio::sync::mpsc;

use self::{
    link::{Link, LinkOptions, LocalOffer, PathInfo, RemoteOffer, remote_candidates},
    seal::{PeerKeys, SealError, SignalKeys, open, seal},
    signal::{SignalClient, SignalConfig, SignalError, SignalSender},
};
use nexa_net::path::Role;

/// §4.4 state timeouts.
pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the host may take to answer: it gathers for up to a second.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(8);
/// Checking (3 s) plus the relay's grace.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(5);
/// How long trickles keep flowing after the first path validated.
const TRICKLE_WINDOW: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Resolving,
    Gathering,
    Checking,
}

#[derive(Debug, Error)]
pub enum DialError {
    #[error("{0}")]
    Signal(#[from] SignalError),
    #[error("the device is not online")]
    Offline,
    #[error("the server is limiting requests; try again shortly")]
    RateLimited,
    #[error("the server refused: {0}")]
    Refused(String),
    #[error("the device answered with keys that do not check out: {0}")]
    BadPeer(SealError),
    #[error("the device at this ID is not the paired device")]
    WrongDevice,
    #[error("the device did not answer")]
    NoAnswer,
    #[error("no network path to the device worked")]
    NoPath,
    #[error("network: {0}")]
    Io(#[from] std::io::Error),
}

fn new_session_id() -> [u8; SESSION_ID_LEN] {
    use rand_core09::TryRngCore;
    let mut s = [0u8; SESSION_ID_LEN];
    rand_core09::OsRng
        .try_fill_bytes(&mut s)
        .expect("the OS random source failed");
    s
}

fn error_of(e: &nexa_wire::signal::SignalError) -> DialError {
    match ErrorCode::try_from(e.code).unwrap_or(ErrorCode::Unspecified) {
        ErrorCode::Offline | ErrorCode::Unknown => DialError::Offline,
        ErrorCode::RateLimited => DialError::RateLimited,
        other => DialError::Refused(format!("{other:?}")),
    }
}

/// The viewer's side after rendezvous: a link with a validated path, ready for
/// `link.endpoint.connect_with(.., VIRTUAL_PEER, ..)`.
pub struct Dialed {
    pub link: Arc<Link>,
    /// The key the host registered and sealed its answer with. The QUIC peer
    /// must present this same key.
    pub host_key: DevicePublicKey,
    pub first_path: PathInfo,
    /// Keeps trickling candidates for a while; drop to stop.
    pub trickle: tokio::task::JoinHandle<()>,
}

/// Connects to `target_id` through the signaling server. `expected` is the
/// pinned key when connecting to a paired device: anything else at that ID is
/// refused before a single packet is sent to it.
pub async fn dial(
    config: &SignalConfig,
    keys: &Arc<SignalKeys>,
    target_id: &str,
    expected: Option<DevicePublicKey>,
    mut opts: LinkOptions,
    progress: &(dyn Fn(Stage) + Send + Sync),
) -> Result<Dialed, DialError> {
    progress(Stage::Resolving);
    // §4.6: the relay over WebSocket goes to the server we already talk to.
    opts.wss_relay.get_or_insert_with(|| config.clone());
    let resolve = async {
        let mut client = SignalClient::connect(config, keys).await?;
        let req_id = 1;
        client.send(ClientBody::Lookup(Lookup {
            req_id,
            short_id: target_id.to_owned(),
        }))?;
        loop {
            match client.recv().await {
                Some(ServerBody::Peer(p)) if p.req_id == req_id => {
                    let peer = PeerKeys::verify(&p.pubkey, &p.x25519, &p.x25519_sig)
                        .map_err(DialError::BadPeer)?;
                    return Ok::<_, DialError>((client, peer));
                }
                Some(ServerBody::Error(e)) if e.req_id == req_id => return Err(error_of(&e)),
                Some(_) => continue,
                None => return Err(SignalError::Closed.into()),
            }
        }
    };
    let (mut client, peer) = tokio::time::timeout(RESOLVE_TIMEOUT, resolve)
        .await
        .map_err(|_| DialError::Signal(SignalError::Timeout))??;
    if expected.is_some_and(|k| k != peer.identity) {
        return Err(DialError::WrongDevice);
    }
    if peer.identity == keys.public_key() {
        return Err(DialError::Refused("that ID is this device".into()));
    }

    progress(Stage::Gathering);
    let link = Arc::new(Link::bind(Role::Viewer, opts, None)?);
    let offer = link.gather(&client.registration.stun_servers).await?;
    let session = new_session_id();
    let sealed = seal(
        keys,
        &peer,
        SealedKind::Offer,
        &session,
        &offer.to_wire().encode_to_vec(),
    )
    .map_err(DialError::BadPeer)?;
    client.send(ClientBody::Connect(Connect {
        target_short_id: target_id.to_owned(),
        session_id: session.to_vec(),
        sealed,
    }))?;

    // Routed, then the sealed Answer; trickles may come in between.
    let mut early_trickles = Vec::new();
    let answer = async {
        loop {
            match client.recv().await {
                Some(ServerBody::Routed(r)) if r.session_id == session => {
                    if let Some(g) = &r.relay {
                        link.set_relay(g);
                    }
                }
                Some(ServerBody::Answer(a)) if a.session_id == session => {
                    let plain = open(keys, &peer, SealedKind::Answer, &session, &a.sealed)
                        .map_err(DialError::BadPeer)?;
                    let r =
                        Rendezvous::decode(plain.as_slice()).map_err(|_| DialError::NoAnswer)?;
                    return RemoteOffer::from_wire(&r).ok_or(DialError::NoAnswer);
                }
                Some(ServerBody::Trickle(t)) if t.session_id == session => early_trickles.push(t),
                Some(ServerBody::Error(e)) if e.session_id == session => return Err(error_of(&e)),
                Some(_) => continue,
                None => return Err(SignalError::Closed.into()),
            }
        }
    };
    let remote = tokio::time::timeout(ANSWER_TIMEOUT, answer)
        .await
        .map_err(|_| DialError::NoAnswer)??;

    progress(Stage::Checking);
    link.start_checks(remote);
    for t in early_trickles {
        apply_trickle(&link, keys, &peer, &session, &t);
    }
    let trickle = spawn_trickle_pump(client, link.clone(), keys.clone(), peer.clone(), session);
    let Some(first_path) = link.first_validated(CHECK_TIMEOUT).await else {
        trickle.abort();
        return Err(DialError::NoPath);
    };
    Ok(Dialed {
        link,
        host_key: peer.identity,
        first_path,
        trickle,
    })
}

fn apply_trickle(
    link: &Link,
    keys: &SignalKeys,
    peer: &PeerKeys,
    session: &[u8; SESSION_ID_LEN],
    t: &Trickle,
) {
    if let Ok(plain) = open(keys, peer, SealedKind::Trickle, session, &t.sealed)
        && let Ok(r) = Rendezvous::decode(plain.as_slice())
    {
        link.add_remote_candidates(remote_candidates(&r.candidates));
    }
}

fn send_trickle(
    out: &SignalSender,
    keys: &SignalKeys,
    peer: &PeerKeys,
    session: &[u8; SESSION_ID_LEN],
    candidates: Vec<nexa_wire::signal::WireCandidate>,
) {
    let body = Rendezvous {
        candidates,
        ..Default::default()
    };
    if let Ok(sealed) = seal(
        keys,
        peer,
        SealedKind::Trickle,
        session,
        &body.encode_to_vec(),
    ) {
        let _ = out.send(ClientBody::Trickle(Trickle {
            session_id: session.to_vec(),
            sealed,
        }));
    }
}

/// Viewer: relays trickles both ways for a while, then closes the signaling
/// connection, which is not needed once QUIC runs.
fn spawn_trickle_pump(
    mut client: SignalClient,
    link: Arc<Link>,
    keys: Arc<SignalKeys>,
    peer: PeerKeys,
    session: [u8; SESSION_ID_LEN],
) -> tokio::task::JoinHandle<()> {
    let mut late = link.take_late_candidates();
    tokio::spawn(async move {
        let out = client.sender();
        let stop = tokio::time::sleep(TRICKLE_WINDOW);
        tokio::pin!(stop);
        loop {
            tokio::select! {
                _ = &mut stop => break,
                m = client.recv() => match m {
                    Some(ServerBody::Trickle(t)) if t.session_id == session => {
                        apply_trickle(&link, &keys, &peer, &session, &t);
                    }
                    Some(_) => {}
                    None => break,
                },
                Some(c) = async { match late.as_mut() { Some(l) => l.recv().await, None => None } } => {
                    send_trickle(&out, &keys, &peer, &session, c);
                }
            }
        }
    })
}

/// What the host learned from an `Incoming` before answering it.
pub struct Offered {
    pub session: [u8; SESSION_ID_LEN],
    pub viewer: PeerKeys,
    pub remote: RemoteOffer,
    pub relay: Option<nexa_wire::signal::RelayGrant>,
}

/// Host: checks and opens an `Incoming`. Anything malformed or unauthenticated
/// is dropped without an answer.
pub fn open_incoming(keys: &SignalKeys, i: &Incoming) -> Result<Offered, SealError> {
    let session: [u8; SESSION_ID_LEN] = i
        .session_id
        .as_slice()
        .try_into()
        .map_err(|_| SealError::Open)?;
    let viewer = PeerKeys::verify(&i.from_pubkey, &i.from_x25519, &i.from_x25519_sig)?;
    let plain = open(keys, &viewer, SealedKind::Offer, &session, &i.sealed)?;
    let r = Rendezvous::decode(plain.as_slice()).map_err(|_| SealError::Open)?;
    let remote = RemoteOffer::from_wire(&r).ok_or(SealError::Open)?;
    Ok(Offered {
        session,
        viewer,
        remote,
        relay: i.relay.clone(),
    })
}

/// Host: answers an opened offer and starts checking. The returned link's
/// endpoint accepts QUIC only from validated paths; trickles for this session
/// go to [`HostRendezvous::trickles`].
pub struct HostRendezvous {
    pub link: Arc<Link>,
    pub viewer: DevicePublicKey,
    pub session: [u8; SESSION_ID_LEN],
    pub trickles: mpsc::UnboundedSender<Trickle>,
    pub pump: tokio::task::JoinHandle<()>,
}

pub async fn answer(
    out: SignalSender,
    keys: Arc<SignalKeys>,
    offered: Offered,
    stun: &[SocketAddr],
    opts: LinkOptions,
    server: quinn::ServerConfig,
) -> Result<HostRendezvous, DialError> {
    let link = Arc::new(Link::bind(Role::Host, opts, Some(server))?);
    if let Some(g) = &offered.relay {
        link.set_relay(g);
    }
    let local: LocalOffer = link.gather(stun).await?;
    let sealed = seal(
        &keys,
        &offered.viewer,
        SealedKind::Answer,
        &offered.session,
        &local.to_wire().encode_to_vec(),
    )
    .map_err(DialError::BadPeer)?;
    out.send(ClientBody::Answer(Answer {
        session_id: offered.session.to_vec(),
        sealed,
    }))?;
    link.start_checks(offered.remote);

    let (tx, mut rx) = mpsc::unbounded_channel::<Trickle>();
    let mut late = link.take_late_candidates();
    let pump = {
        let (link, keys, peer, session) = (
            link.clone(),
            keys.clone(),
            offered.viewer.clone(),
            offered.session,
        );
        tokio::spawn(async move {
            let stop = tokio::time::sleep(TRICKLE_WINDOW);
            tokio::pin!(stop);
            loop {
                tokio::select! {
                    _ = &mut stop => break,
                    t = rx.recv() => match t {
                        Some(t) => apply_trickle(&link, &keys, &peer, &session, &t),
                        None => break,
                    },
                    Some(c) = async { match late.as_mut() { Some(l) => l.recv().await, None => None } } => {
                        send_trickle(&out, &keys, &peer, &session, c);
                    }
                }
            }
        })
    };
    Ok(HostRendezvous {
        link,
        viewer: offered.viewer.identity,
        session: offered.session,
        trickles: tx,
        pump,
    })
}
