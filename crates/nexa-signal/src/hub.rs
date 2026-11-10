//! Signaling state (§4.3): who is registered under which short ID, and the
//! rendezvous sessions between them. Memory only.
//!
//! The hub sees public keys, short IDs, IP addresses and the sizes and timing of
//! sealed blobs. It never sees candidates or credentials (they are sealed to the
//! peer) and has no part in pairing or consent, which run end to end inside QUIC.

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    time::Instant,
};

use nexa_auth::DevicePublicKey;
use nexa_wire::signal::{
    Answer, ClientBody, Connect, ErrorCode, Incoming, Lookup, MAX_SEALED, NONCE_LEN,
    PURPOSE_REGISTER, PURPOSE_X25519, PeerInfo, Register, RelayGrant, RelayRole, Routed,
    SESSION_ID_LEN, SHORT_ID_DIGITS, SIGNAL_PROTOCOL, ServerBody, SignalError, SignalServerMsg,
    Trickle, normalize_short_id, register_message,
};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use crate::{
    config::Limits,
    limit::{Keyed, Rate},
    relay::{TokenIssuer, unix_now},
};

const MAX_TRACKED: usize = 65_536;
/// Collision fallbacks per key before the server gives up on it.
const ID_ATTEMPTS: u8 = 8;

pub type Outbox = mpsc::Sender<SignalServerMsg>;

/// The ID a key gets unless another key already holds it: stable across server
/// restarts without storing anything, because it is derived from the key.
pub fn derive_short_id(pubkey: &[u8; 32], attempt: u8) -> String {
    let mut h = Sha256::new();
    h.update(b"nexa/1 short id");
    h.update(pubkey);
    h.update([attempt]);
    let d = h.finalize();
    let n = u64::from_be_bytes(d[..8].try_into().expect("8 bytes"));
    // Nine digits, no leading zero.
    let id = 100_000_000 + n % 900_000_000;
    let s = id.to_string();
    debug_assert_eq!(s.len(), SHORT_ID_DIGITS);
    s
}

struct Peer {
    conn: u64,
    pubkey: [u8; 32],
    x25519: Vec<u8>,
    x25519_sig: Vec<u8>,
    ip: IpAddr,
    out: Outbox,
}

struct Session {
    viewer_conn: u64,
    host_conn: u64,
    expires: Instant,
    trickles: u32,
}

pub struct Hub {
    limits: Limits,
    server_names: Vec<String>,
    stun: Vec<SocketAddr>,
    stun_note: String,
    relay: Option<SocketAddr>,
    relay_ttl: u64,
    issuer: std::sync::Arc<TokenIssuer>,
    peers: HashMap<String, Peer>,
    by_conn: HashMap<u64, String>,
    sessions: HashMap<[u8; SESSION_ID_LEN], Session>,
    conns_per_ip: HashMap<IpAddr, usize>,
    total_conns: usize,
    register_ip: Keyed<IpAddr>,
    lookup_ip: Keyed<IpAddr>,
    connect_ip: Keyed<IpAddr>,
    connect_src: Keyed<String>,
    connect_dst: Keyed<String>,
}

fn rate((per_sec, burst): (f64, f64)) -> Rate {
    Rate::new(per_sec, burst)
}

pub fn error(code: ErrorCode, message: &str) -> SignalServerMsg {
    ServerBody::Error(SignalError {
        code: code as i32,
        session_id: Vec::new(),
        req_id: 0,
        message: message.to_owned(),
    })
    .into()
}

fn session_error(code: ErrorCode, session_id: &[u8], message: &str) -> SignalServerMsg {
    ServerBody::Error(SignalError {
        code: code as i32,
        session_id: session_id.to_vec(),
        req_id: 0,
        message: message.to_owned(),
    })
    .into()
}

pub struct HubSettings {
    pub limits: Limits,
    pub server_names: Vec<String>,
    pub stun: Vec<SocketAddr>,
    pub stun_note: Option<String>,
    pub relay: Option<SocketAddr>,
    pub relay_ttl_secs: u64,
    pub issuer: std::sync::Arc<TokenIssuer>,
}

impl Hub {
    pub fn new(s: HubSettings) -> Self {
        let l = &s.limits;
        Self {
            register_ip: Keyed::new(rate(l.register_per_ip), MAX_TRACKED),
            lookup_ip: Keyed::new(rate(l.lookup_per_ip), MAX_TRACKED),
            connect_ip: Keyed::new(rate(l.connect_per_ip), MAX_TRACKED),
            connect_src: Keyed::new(rate(l.connect_per_source), MAX_TRACKED),
            connect_dst: Keyed::new(rate(l.connect_per_target), MAX_TRACKED),
            limits: s.limits,
            server_names: s.server_names,
            stun: s.stun,
            stun_note: s.stun_note.unwrap_or_default(),
            relay: s.relay,
            relay_ttl: s.relay_ttl_secs,
            issuer: s.issuer,
            peers: HashMap::new(),
            by_conn: HashMap::new(),
            sessions: HashMap::new(),
            conns_per_ip: HashMap::new(),
            total_conns: 0,
        }
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Admission of a new TCP connection.
    pub fn open_conn(&mut self, ip: IpAddr) -> bool {
        let n = self.conns_per_ip.get(&ip).copied().unwrap_or(0);
        if n >= self.limits.max_connections_per_ip
            || self.total_conns >= self.limits.max_connections
        {
            return false;
        }
        self.conns_per_ip.insert(ip, n + 1);
        self.total_conns += 1;
        true
    }

    pub fn close_conn(&mut self, conn: u64, ip: IpAddr) {
        if let Some(n) = self.conns_per_ip.get_mut(&ip) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.conns_per_ip.remove(&ip);
            }
        }
        self.total_conns = self.total_conns.saturating_sub(1);
        if let Some(id) = self.by_conn.remove(&conn)
            && self.peers.get(&id).is_some_and(|p| p.conn == conn)
        {
            self.peers.remove(&id);
        }
        self.sessions
            .retain(|_, s| s.viewer_conn != conn && s.host_conn != conn);
    }

    pub fn registered(&self) -> usize {
        self.peers.len()
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Checks a registration and assigns the ID. `Err` is the reply to send
    /// before closing.
    pub fn register(
        &mut self,
        conn: u64,
        ip: IpAddr,
        nonce: &[u8; NONCE_LEN],
        r: &Register,
        out: Outbox,
        now: Instant,
    ) -> Result<SignalServerMsg, Box<SignalServerMsg>> {
        self.register_inner(conn, ip, nonce, r, out, now)
            .map_err(Box::new)
    }

    // Boxed at the public boundary; here the plain type keeps the many early returns short.
    #[allow(clippy::result_large_err)]
    fn register_inner(
        &mut self,
        conn: u64,
        ip: IpAddr,
        nonce: &[u8; NONCE_LEN],
        r: &Register,
        out: Outbox,
        now: Instant,
    ) -> Result<SignalServerMsg, SignalServerMsg> {
        if !self.register_ip.allow(&ip, now) {
            return Err(error(ErrorCode::RateLimited, "too many registrations"));
        }
        if r.proto != SIGNAL_PROTOCOL {
            return Err(error(ErrorCode::BadRequest, "unsupported protocol version"));
        }
        let pubkey: [u8; 32] = r
            .pubkey
            .as_slice()
            .try_into()
            .map_err(|_| error(ErrorCode::BadRequest, "bad key"))?;
        let key = DevicePublicKey::from_bytes(pubkey)
            .map_err(|_| error(ErrorCode::BadRequest, "bad key"))?;
        let x_sig: [u8; 64] = r
            .x25519_sig
            .as_slice()
            .try_into()
            .map_err(|_| error(ErrorCode::BadRequest, "bad signature"))?;
        let sig: [u8; 64] = r
            .sig
            .as_slice()
            .try_into()
            .map_err(|_| error(ErrorCode::BadRequest, "bad signature"))?;
        if r.x25519.len() != 32
            || r.server_name.len() > 253
            || !key.verify_for(PURPOSE_X25519, &r.x25519, &x_sig)
            || !key.verify_for(
                PURPOSE_REGISTER,
                &register_message(nonce, &r.x25519, &r.server_name),
                &sig,
            )
        {
            return Err(error(ErrorCode::BadRequest, "bad signature"));
        }
        if !self.server_names.is_empty()
            && !self
                .server_names
                .iter()
                .any(|n| n.eq_ignore_ascii_case(&r.server_name))
        {
            return Err(error(
                ErrorCode::BadRequest,
                "registration is for another server",
            ));
        }

        // The same key again (a reconnect): the newer connection wins.
        let mut chosen = None;
        for attempt in 0..ID_ATTEMPTS {
            let id = derive_short_id(&pubkey, attempt);
            match self.peers.get(&id) {
                None => {
                    chosen = Some(id);
                    break;
                }
                Some(p) if p.pubkey == pubkey => {
                    chosen = Some(id);
                    break;
                }
                Some(_) => {}
            }
        }
        let Some(id) = chosen else {
            return Err(error(ErrorCode::Unavailable, "no free ID"));
        };
        if let Some(old) = self.peers.remove(&id) {
            self.by_conn.remove(&old.conn);
            let _ = old
                .out
                .try_send(error(ErrorCode::Unspecified, "registered again elsewhere"));
            let old_conn = old.conn;
            self.sessions
                .retain(|_, s| s.viewer_conn != old_conn && s.host_conn != old_conn);
        }
        self.by_conn.insert(conn, id.clone());
        self.peers.insert(
            id.clone(),
            Peer {
                conn,
                pubkey,
                x25519: r.x25519.clone(),
                x25519_sig: r.x25519_sig.clone(),
                ip,
                out,
            },
        );
        Ok(ServerBody::Registered(nexa_wire::signal::Registered {
            short_id: id,
            stun_servers: self.stun.iter().map(|a| a.to_string()).collect(),
            relays: self.relay.iter().map(|a| a.to_string()).collect(),
            stun_note: self.stun_note.clone(),
        })
        .into())
    }

    /// Whether `conn` is still the registered holder of its ID.
    pub fn is_current(&self, conn: u64) -> bool {
        self.by_conn.contains_key(&conn)
    }

    pub fn sweep(&mut self, now: Instant) {
        self.sessions.retain(|_, s| s.expires > now);
    }

    /// One message from a registered connection. Replies go straight to the
    /// relevant outboxes; nothing here blocks.
    pub fn handle(&mut self, conn: u64, body: ClientBody, now: Instant) {
        let Some(me) = self.by_conn.get(&conn).cloned() else {
            return;
        };
        let Some(my_out) = self.peers.get(&me).map(|p| p.out.clone()) else {
            return;
        };
        let reply = match body {
            ClientBody::Register(_) => Some(error(ErrorCode::BadRequest, "already registered")),
            ClientBody::Lookup(l) => Some(self.lookup(&me, l, now)),
            ClientBody::Connect(c) => self.connect(conn, &me, c, now),
            ClientBody::Answer(a) => self.answer(conn, a),
            ClientBody::Trickle(t) => self.trickle(conn, t),
        };
        if let Some(r) = reply {
            let _ = my_out.try_send(r);
        }
    }

    fn lookup(&mut self, me: &str, l: Lookup, now: Instant) -> SignalServerMsg {
        let ip = self.peers.get(me).map(|p| p.ip);
        if ip.is_some_and(|ip| !self.lookup_ip.allow(&ip, now)) {
            return ServerBody::Error(SignalError {
                code: ErrorCode::RateLimited as i32,
                session_id: Vec::new(),
                req_id: l.req_id,
                message: "too many lookups".into(),
            })
            .into();
        }
        let found =
            normalize_short_id(&l.short_id).and_then(|id| self.peers.get(&id).map(|p| (id, p)));
        match found {
            Some((id, p)) => ServerBody::Peer(PeerInfo {
                req_id: l.req_id,
                short_id: id,
                pubkey: p.pubkey.to_vec(),
                x25519: p.x25519.clone(),
                x25519_sig: p.x25519_sig.clone(),
            })
            .into(),
            None => ServerBody::Error(SignalError {
                code: ErrorCode::Offline as i32,
                session_id: Vec::new(),
                req_id: l.req_id,
                message: "that device is not online".into(),
            })
            .into(),
        }
    }

    fn connect(
        &mut self,
        conn: u64,
        me: &str,
        c: Connect,
        now: Instant,
    ) -> Option<SignalServerMsg> {
        let sid: [u8; SESSION_ID_LEN] = match c.session_id.as_slice().try_into() {
            Ok(s) => s,
            Err(_) => return Some(error(ErrorCode::BadRequest, "bad session id")),
        };
        if c.sealed.is_empty() || c.sealed.len() > MAX_SEALED {
            return Some(session_error(ErrorCode::BadRequest, &sid, "bad offer"));
        }
        let ip = self.peers.get(me).map(|p| p.ip)?;
        let Some(target) = normalize_short_id(&c.target_short_id) else {
            return Some(session_error(ErrorCode::Unknown, &sid, "not an ID"));
        };
        if !self.connect_ip.allow(&ip, now)
            || !self.connect_src.allow(&me.to_owned(), now)
            || !self.connect_dst.allow(&target, now)
        {
            return Some(session_error(
                ErrorCode::RateLimited,
                &sid,
                "too many connection requests",
            ));
        }
        if target == me {
            return Some(session_error(
                ErrorCode::BadRequest,
                &sid,
                "that is this device",
            ));
        }
        if self.sessions.contains_key(&sid) {
            return Some(session_error(
                ErrorCode::BadRequest,
                &sid,
                "session id in use",
            ));
        }
        let open = self
            .sessions
            .values()
            .filter(|s| s.viewer_conn == conn)
            .count();
        if open >= self.limits.max_sessions_per_conn {
            return Some(session_error(
                ErrorCode::RateLimited,
                &sid,
                "too many open requests",
            ));
        }
        let Some(host) = self.peers.get(&target) else {
            return Some(session_error(
                ErrorCode::Offline,
                &sid,
                "that device is not online",
            ));
        };
        let viewer = self.peers.get(me)?;

        let (viewer_grant, host_grant) = match self.relay {
            Some(addr) => {
                let rs = TokenIssuer::new_session();
                let expiry = unix_now() + self.relay_ttl;
                let grant = |role| RelayGrant {
                    relay: addr.to_string(),
                    token: self.issuer.issue(rs, role, expiry).encode(),
                };
                (Some(grant(RelayRole::Viewer)), Some(grant(RelayRole::Host)))
            }
            None => (None, None),
        };
        let incoming: SignalServerMsg = ServerBody::Incoming(Incoming {
            session_id: sid.to_vec(),
            from_pubkey: viewer.pubkey.to_vec(),
            from_x25519: viewer.x25519.clone(),
            from_x25519_sig: viewer.x25519_sig.clone(),
            sealed: c.sealed,
            relay: host_grant,
            from_ip: nexa_wire::signal::encode_ip(viewer.ip),
        })
        .into();
        if host.out.try_send(incoming).is_err() {
            return Some(session_error(
                ErrorCode::Unavailable,
                &sid,
                "that device is busy",
            ));
        }
        let host_conn = host.conn;
        self.sessions.insert(
            sid,
            Session {
                viewer_conn: conn,
                host_conn,
                expires: now + self.limits.session_ttl(),
                trickles: 0,
            },
        );
        Some(
            ServerBody::Routed(Routed {
                session_id: sid.to_vec(),
                relay: viewer_grant,
            })
            .into(),
        )
    }

    fn out_of(&self, conn: u64) -> Option<Outbox> {
        let id = self.by_conn.get(&conn)?;
        self.peers.get(id).map(|p| p.out.clone())
    }

    fn answer(&mut self, conn: u64, a: Answer) -> Option<SignalServerMsg> {
        let sid: [u8; SESSION_ID_LEN] = a.session_id.as_slice().try_into().ok()?;
        if a.sealed.is_empty() || a.sealed.len() > MAX_SEALED {
            return Some(session_error(ErrorCode::BadRequest, &sid, "bad answer"));
        }
        // Only the target of a session may answer it.
        let viewer = match self.sessions.get(&sid) {
            Some(s) if s.host_conn == conn => s.viewer_conn,
            _ => return Some(session_error(ErrorCode::Unknown, &sid, "no such session")),
        };
        let out = self.out_of(viewer)?;
        let _ = out.try_send(ServerBody::Answer(a).into());
        None
    }

    fn trickle(&mut self, conn: u64, t: Trickle) -> Option<SignalServerMsg> {
        let sid: [u8; SESSION_ID_LEN] = t.session_id.as_slice().try_into().ok()?;
        if t.sealed.is_empty() || t.sealed.len() > MAX_SEALED {
            return Some(session_error(ErrorCode::BadRequest, &sid, "bad trickle"));
        }
        let cap = self.limits.trickles_per_session;
        let other = match self.sessions.get_mut(&sid) {
            Some(s) if s.trickles >= cap && (s.viewer_conn == conn || s.host_conn == conn) => {
                return Some(session_error(
                    ErrorCode::RateLimited,
                    &sid,
                    "too many trickles",
                ));
            }
            Some(s) if s.viewer_conn == conn => {
                s.trickles += 1;
                s.host_conn
            }
            Some(s) if s.host_conn == conn => {
                s.trickles += 1;
                s.viewer_conn
            }
            _ => return Some(session_error(ErrorCode::Unknown, &sid, "no such session")),
        };
        let out = self.out_of(other)?;
        let _ = out.try_send(ServerBody::Trickle(t).into());
        None
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use nexa_auth::DeviceIdentity;
    use nexa_wire::signal::Registered;

    use super::*;

    fn hub() -> Hub {
        Hub::new(HubSettings {
            limits: Limits::default(),
            server_names: vec!["signal.test".into()],
            stun: vec!["203.0.113.1:3478".parse().unwrap()],
            stun_note: Some("one".into()),
            relay: Some("203.0.113.1:3480".parse().unwrap()),
            relay_ttl_secs: 60,
            issuer: Arc::new(TokenIssuer::new()),
        })
    }

    fn reg(id: &DeviceIdentity, nonce: &[u8; 32], name: &str) -> Register {
        let x = vec![9u8; 32];
        Register {
            proto: SIGNAL_PROTOCOL,
            pubkey: id.public_key().as_bytes().to_vec(),
            x25519_sig: id.sign_for(PURPOSE_X25519, &x).to_vec(),
            sig: id
                .sign_for(PURPOSE_REGISTER, &register_message(nonce, &x, name))
                .to_vec(),
            x25519: x,
            server_name: name.into(),
        }
    }

    fn join(
        h: &mut Hub,
        conn: u64,
        id: &DeviceIdentity,
    ) -> (String, mpsc::Receiver<SignalServerMsg>) {
        let (tx, rx) = mpsc::channel(64);
        let nonce = [conn as u8; 32];
        let ip: IpAddr = [198, 51, 100, conn as u8].into();
        assert!(h.open_conn(ip));
        match h.register(
            conn,
            ip,
            &nonce,
            &reg(id, &nonce, "signal.test"),
            tx,
            Instant::now(),
        ) {
            Ok(SignalServerMsg {
                body: Some(ServerBody::Registered(Registered { short_id, .. })),
            }) => (short_id, rx),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn ids_are_stable_and_nine_digits() {
        let k = [7u8; 32];
        assert_eq!(derive_short_id(&k, 0), derive_short_id(&k, 0));
        assert_ne!(derive_short_id(&k, 0), derive_short_id(&k, 1));
        assert_eq!(derive_short_id(&k, 0).len(), 9);
    }

    #[test]
    fn registration_requires_a_fresh_signature_for_this_server() {
        let mut h = hub();
        let id = DeviceIdentity::generate();
        let (tx, _rx) = mpsc::channel(4);
        let ip: IpAddr = [198, 51, 100, 1].into();
        let nonce = [1u8; 32];
        // Signed for another nonce: a replay.
        let replay = reg(&id, &[2u8; 32], "signal.test");
        assert!(
            h.register(1, ip, &nonce, &replay, tx.clone(), Instant::now())
                .is_err()
        );
        // Signed for another server.
        let elsewhere = reg(&id, &nonce, "evil.test");
        assert!(
            h.register(1, ip, &nonce, &elsewhere, tx.clone(), Instant::now())
                .is_err()
        );
        // Someone else's key with our signature.
        let mut stolen = reg(&id, &nonce, "signal.test");
        stolen.pubkey = DeviceIdentity::generate().public_key().as_bytes().to_vec();
        assert!(
            h.register(1, ip, &nonce, &stolen, tx.clone(), Instant::now())
                .is_err()
        );
        // The X25519 key swapped after signing.
        let mut swapped = reg(&id, &nonce, "signal.test");
        swapped.x25519 = vec![1u8; 32];
        assert!(
            h.register(1, ip, &nonce, &swapped, tx.clone(), Instant::now())
                .is_err()
        );
        assert!(
            h.register(
                1,
                ip,
                &nonce,
                &reg(&id, &nonce, "signal.test"),
                tx,
                Instant::now()
            )
            .is_ok()
        );
        assert_eq!(h.registered(), 1);
    }

    #[test]
    fn forwards_only_within_a_session_and_only_to_its_parties() {
        let mut h = hub();
        let (v, host, eve) = (
            DeviceIdentity::generate(),
            DeviceIdentity::generate(),
            DeviceIdentity::generate(),
        );
        let (_vid, mut vrx) = join(&mut h, 1, &v);
        let (hid, mut hrx) = join(&mut h, 2, &host);
        let (_eid, mut erx) = join(&mut h, 3, &eve);
        let sid = vec![5u8; 16];
        h.handle(
            1,
            ClientBody::Connect(Connect {
                target_short_id: hid.clone(),
                session_id: sid.clone(),
                sealed: vec![1; 10],
            }),
            Instant::now(),
        );
        let routed = vrx.try_recv().unwrap();
        assert!(matches!(
            routed.body,
            Some(ServerBody::Routed(Routed { relay: Some(_), .. }))
        ));
        match hrx.try_recv().unwrap().body {
            Some(ServerBody::Incoming(i)) => {
                assert_eq!(i.from_pubkey, v.public_key().as_bytes().to_vec());
                assert_eq!(i.sealed, vec![1; 10]);
                assert!(i.relay.is_some());
                // The viewer joined as conn 1, from 198.51.100.1.
                assert_eq!(i.from_ip, vec![198, 51, 100, 1]);
            }
            other => panic!("{other:?}"),
        }
        // Eve cannot answer or trickle into someone else's session.
        h.handle(
            3,
            ClientBody::Answer(Answer {
                session_id: sid.clone(),
                sealed: vec![2],
            }),
            Instant::now(),
        );
        h.handle(
            3,
            ClientBody::Trickle(Trickle {
                session_id: sid.clone(),
                sealed: vec![2],
            }),
            Instant::now(),
        );
        assert!(vrx.try_recv().is_err());
        assert!(matches!(
            erx.try_recv().unwrap().body,
            Some(ServerBody::Error(_))
        ));
        // Nor can the viewer answer its own request.
        h.handle(
            1,
            ClientBody::Answer(Answer {
                session_id: sid.clone(),
                sealed: vec![2],
            }),
            Instant::now(),
        );
        assert!(matches!(
            vrx.try_recv().unwrap().body,
            Some(ServerBody::Error(_))
        ));
        assert!(hrx.try_recv().is_err());
        // The host can.
        h.handle(
            2,
            ClientBody::Answer(Answer {
                session_id: sid.clone(),
                sealed: vec![3],
            }),
            Instant::now(),
        );
        assert!(
            matches!(vrx.try_recv().unwrap().body, Some(ServerBody::Answer(Answer { sealed, .. })) if sealed == vec![3])
        );
        h.handle(
            2,
            ClientBody::Trickle(Trickle {
                session_id: sid.clone(),
                sealed: vec![4],
            }),
            Instant::now(),
        );
        assert!(matches!(
            vrx.try_recv().unwrap().body,
            Some(ServerBody::Trickle(_))
        ));
        // Re-using a session id is refused.
        h.handle(
            3,
            ClientBody::Connect(Connect {
                target_short_id: hid,
                session_id: sid,
                sealed: vec![1],
            }),
            Instant::now(),
        );
        assert!(matches!(
            erx.try_recv().unwrap().body,
            Some(ServerBody::Error(_))
        ));
    }

    #[test]
    fn connect_requests_to_one_target_are_rate_limited() {
        let mut h = hub();
        let host = DeviceIdentity::generate();
        let (hid, _hrx) = join(&mut h, 1, &host);
        let mut limited = 0;
        for c in 2..20u64 {
            let v = DeviceIdentity::generate();
            let (_, mut vrx) = join(&mut h, c, &v);
            h.handle(
                c,
                ClientBody::Connect(Connect {
                    target_short_id: hid.clone(),
                    session_id: vec![c as u8; 16],
                    sealed: vec![1],
                }),
                Instant::now(),
            );
            if let Some(ServerBody::Error(e)) = vrx.try_recv().unwrap().body
                && e.code == ErrorCode::RateLimited as i32
            {
                limited += 1;
            }
        }
        assert_eq!(limited, 18 - 6, "burst of 6 per target");
    }

    #[test]
    fn closing_forgets_the_peer_and_its_sessions() {
        let mut h = hub();
        let (v, host) = (DeviceIdentity::generate(), DeviceIdentity::generate());
        let (_, _vrx) = join(&mut h, 1, &v);
        let (hid, _hrx) = join(&mut h, 2, &host);
        h.handle(
            1,
            ClientBody::Connect(Connect {
                target_short_id: hid.clone(),
                session_id: vec![1; 16],
                sealed: vec![1],
            }),
            Instant::now(),
        );
        assert_eq!(h.session_count(), 1);
        h.close_conn(2, [198, 51, 100, 2].into());
        assert_eq!(h.session_count(), 0);
        assert_eq!(h.registered(), 1);
    }

    #[test]
    fn lookups_return_signed_keys_or_offline() {
        let mut h = hub();
        let (v, host) = (DeviceIdentity::generate(), DeviceIdentity::generate());
        let (_, mut vrx) = join(&mut h, 1, &v);
        let (hid, _hrx) = join(&mut h, 2, &host);
        h.handle(
            1,
            ClientBody::Lookup(Lookup {
                req_id: 9,
                short_id: hid,
            }),
            Instant::now(),
        );
        match vrx.try_recv().unwrap().body {
            Some(ServerBody::Peer(p)) => {
                assert_eq!(p.req_id, 9);
                assert_eq!(p.pubkey, host.public_key().as_bytes().to_vec());
            }
            other => panic!("{other:?}"),
        }
        h.handle(
            1,
            ClientBody::Lookup(Lookup {
                req_id: 10,
                short_id: "999999999".into(),
            }),
            Instant::now(),
        );
        assert!(matches!(
            vrx.try_recv().unwrap().body,
            Some(ServerBody::Error(SignalError { req_id: 10, .. }))
        ));
    }
}
