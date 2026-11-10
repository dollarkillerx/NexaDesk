//! The UDP relay (§4.6). It forwards opaque payloads between the two addresses
//! that bound a session with server-signed tokens, and nothing else: no answer
//! to anything unbound, so it cannot be used to reflect traffic, and a
//! per-session byte budget.

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use nexa_net::udp::UdpIo;
use nexa_wire::signal::{
    BindRefusal, RelayKind, RelayRole, RelaySession, RelayToken, parse_relay_frame, relay_frame,
};
use rand_core::{OsRng, RngCore};

use crate::limit::{Bucket, Keyed, Rate};

/// A session nobody sent anything on for this long is forgotten.
const IDLE: Duration = Duration::from_secs(60);
const SWEEP_EVERY: Duration = Duration::from_secs(5);

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Signs and checks relay tokens. The key lives only in memory: a restart
/// invalidates every token, like every other piece of server state.
pub struct TokenIssuer {
    key: SigningKey,
}

impl Default for TokenIssuer {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenIssuer {
    pub fn new() -> Self {
        Self {
            key: SigningKey::generate(&mut OsRng),
        }
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.key.verifying_key()
    }

    pub fn new_session() -> RelaySession {
        let mut s = [0u8; 8];
        OsRng.fill_bytes(&mut s);
        s
    }

    pub fn issue(&self, session: RelaySession, role: RelayRole, expiry: u64) -> RelayToken {
        let sig = self
            .key
            .sign(&RelayToken::signed_part(&session, role, expiry))
            .to_bytes();
        RelayToken {
            session,
            role,
            expiry,
            sig,
        }
    }
}

pub fn verify_token(key: &VerifyingKey, t: &RelayToken, now_unix: u64) -> Result<(), BindRefusal> {
    key.verify(
        &RelayToken::signed_part(&t.session, t.role, t.expiry),
        &Signature::from_bytes(&t.sig),
    )
    .map_err(|_| BindRefusal::BadToken)?;
    if t.expiry < now_unix {
        return Err(BindRefusal::Expired);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
pub struct RelayLimits {
    pub bandwidth_bps: u64,
    pub max_sessions: usize,
}

struct Session {
    viewer: Option<SocketAddr>,
    host: Option<SocketAddr>,
    bucket: Bucket,
    last_active: Instant,
}

/// Counters, for tests and logs.
#[derive(Default, Debug, Clone, Copy)]
pub struct RelayStats {
    pub forwarded: u64,
    pub forwarded_bytes: u64,
    pub dropped: u64,
    pub binds: u64,
    pub refused: u64,
}

pub struct Relay {
    key: VerifyingKey,
    limits: RelayLimits,
    sessions: HashMap<RelaySession, Session>,
    bind_limiter: Keyed<IpAddr>,
    pub stats: RelayStats,
}

/// What to send after handling one datagram.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Send(SocketAddr, Vec<u8>),
}

impl Relay {
    pub fn new(key: VerifyingKey, limits: RelayLimits) -> Self {
        Self {
            key,
            limits,
            sessions: HashMap::new(),
            bind_limiter: Keyed::new(Rate::new(5.0, 20.0), 65_536),
            stats: RelayStats::default(),
        }
    }

    fn byte_rate(&self) -> Rate {
        let bytes = self.limits.bandwidth_bps as f64 / 8.0;
        // Up to a quarter second of burst: enough for a keyframe.
        Rate::new(bytes, (bytes / 4.0).max(64.0 * 1024.0))
    }

    pub fn handle(
        &mut self,
        packet: &[u8],
        from: SocketAddr,
        now: Instant,
        now_unix: u64,
    ) -> Action {
        let Ok((kind, sid, payload)) = parse_relay_frame(packet) else {
            self.stats.dropped += 1;
            return Action::None;
        };
        match kind {
            RelayKind::Bind => self.bind(sid, payload, from, now, now_unix),
            RelayKind::Data => self.forward(sid, packet, from, now),
            RelayKind::BindOk | RelayKind::BindRefused => {
                self.stats.dropped += 1;
                Action::None
            }
        }
    }

    fn bind(
        &mut self,
        sid: RelaySession,
        payload: &[u8],
        from: SocketAddr,
        now: Instant,
        now_unix: u64,
    ) -> Action {
        if !self.bind_limiter.allow(&from.ip().to_canonical(), now) {
            self.stats.refused += 1;
            return Action::None;
        }
        let refuse = |stats: &mut RelayStats, why: BindRefusal| {
            stats.refused += 1;
            Action::Send(
                from,
                relay_frame(RelayKind::BindRefused, &sid, &[why as u8]),
            )
        };
        let token = match RelayToken::decode(payload) {
            Ok(t) if t.session == sid => t,
            _ => return refuse(&mut self.stats, BindRefusal::BadToken),
        };
        if let Err(why) = verify_token(&self.key, &token, now_unix) {
            return refuse(&mut self.stats, why);
        }
        if !self.sessions.contains_key(&sid) && self.sessions.len() >= self.limits.max_sessions {
            self.sweep(now);
            if self.sessions.len() >= self.limits.max_sessions {
                return refuse(&mut self.stats, BindRefusal::Full);
            }
        }
        let rate = self.byte_rate();
        let s = self.sessions.entry(sid).or_insert_with(|| Session {
            viewer: None,
            host: None,
            bucket: Bucket::full(rate, now),
            last_active: now,
        });
        // A later bind from a new address replaces the old one: NATs rebind.
        match token.role {
            RelayRole::Viewer => s.viewer = Some(from),
            RelayRole::Host => s.host = Some(from),
        }
        s.last_active = now;
        self.stats.binds += 1;
        Action::Send(from, relay_frame(RelayKind::BindOk, &sid, &[]))
    }

    fn forward(
        &mut self,
        sid: RelaySession,
        packet: &[u8],
        from: SocketAddr,
        now: Instant,
    ) -> Action {
        let rate = self.byte_rate();
        let Some(s) = self.sessions.get_mut(&sid) else {
            self.stats.dropped += 1;
            return Action::None;
        };
        let to = if s.viewer == Some(from) {
            s.host
        } else if s.host == Some(from) {
            s.viewer
        } else {
            None
        };
        let Some(to) = to else {
            self.stats.dropped += 1;
            return Action::None;
        };
        if !s.bucket.take(rate, packet.len() as f64, now) {
            self.stats.dropped += 1;
            return Action::None;
        }
        s.last_active = now;
        self.stats.forwarded += 1;
        self.stats.forwarded_bytes += packet.len() as u64;
        // Forwarded with its header: the receiver needs the session id to tell
        // relayed packets apart, and the payload is never looked at.
        Action::Send(to, packet.to_vec())
    }

    pub fn sweep(&mut self, now: Instant) {
        self.sessions
            .retain(|_, s| now.saturating_duration_since(s.last_active) < IDLE);
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }
}

pub async fn serve(socket: Arc<UdpIo>, relay: Arc<Mutex<Relay>>) {
    let mut buf = vec![0u8; 2048];
    let mut last_sweep = Instant::now();
    loop {
        let (n, from) = match socket.recv_from(&mut buf).await {
            Ok(r) => r,
            Err(_) => continue,
        };
        let now = Instant::now();
        let action = {
            let mut r = relay.lock().unwrap_or_else(|e| e.into_inner());
            if now.saturating_duration_since(last_sweep) >= SWEEP_EVERY {
                r.sweep(now);
                last_sweep = now;
            }
            r.handle(&buf[..n], from, now, unix_now())
        };
        if let Action::Send(to, bytes) = action {
            let _ = socket.try_send_to(&bytes, to);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn setup() -> (TokenIssuer, Relay, RelaySession) {
        let issuer = TokenIssuer::new();
        let relay = Relay::new(
            issuer.verifying_key(),
            RelayLimits {
                bandwidth_bps: 8_000_000,
                max_sessions: 2,
            },
        );
        (issuer, relay, [7; 8])
    }

    fn bind(
        r: &mut Relay,
        issuer: &TokenIssuer,
        sid: RelaySession,
        role: RelayRole,
        from: &str,
    ) -> Action {
        let token = issuer.issue(sid, role, unix_now() + 60);
        r.handle(
            &relay_frame(RelayKind::Bind, &sid, &token.encode()),
            sa(from),
            Instant::now(),
            unix_now(),
        )
    }

    #[test]
    fn forwards_between_bound_ends_only() {
        let (issuer, mut r, sid) = setup();
        let ok = relay_frame(RelayKind::BindOk, &sid, &[]);
        assert_eq!(
            bind(&mut r, &issuer, sid, RelayRole::Viewer, "198.51.100.1:1"),
            Action::Send(sa("198.51.100.1:1"), ok.clone())
        );
        let data = relay_frame(RelayKind::Data, &sid, b"quic");
        // The other side is not bound yet.
        assert_eq!(
            r.handle(&data, sa("198.51.100.1:1"), Instant::now(), unix_now()),
            Action::None
        );
        bind(&mut r, &issuer, sid, RelayRole::Host, "198.51.100.2:2");
        assert_eq!(
            r.handle(&data, sa("198.51.100.1:1"), Instant::now(), unix_now()),
            Action::Send(sa("198.51.100.2:2"), data.clone())
        );
        assert_eq!(
            r.handle(&data, sa("198.51.100.2:2"), Instant::now(), unix_now()),
            Action::Send(sa("198.51.100.1:1"), data.clone())
        );
        // A stranger who knows the session id gets nothing through, and no answer.
        assert_eq!(
            r.handle(&data, sa("203.0.113.66:6"), Instant::now(), unix_now()),
            Action::None
        );
    }

    #[test]
    fn refuses_forged_expired_and_mismatched_tokens() {
        let (issuer, mut r, sid) = setup();
        let other = TokenIssuer::new();
        let forged = other.issue(sid, RelayRole::Viewer, unix_now() + 60);
        let refused = |why: BindRefusal| relay_frame(RelayKind::BindRefused, &sid, &[why as u8]);
        let from = sa("198.51.100.1:1");
        let now = Instant::now();
        assert_eq!(
            r.handle(
                &relay_frame(RelayKind::Bind, &sid, &forged.encode()),
                from,
                now,
                unix_now()
            ),
            Action::Send(from, refused(BindRefusal::BadToken))
        );
        let expired = issuer.issue(sid, RelayRole::Viewer, unix_now() - 1);
        assert_eq!(
            r.handle(
                &relay_frame(RelayKind::Bind, &sid, &expired.encode()),
                from,
                now,
                unix_now()
            ),
            Action::Send(from, refused(BindRefusal::Expired))
        );
        let elsewhere = issuer.issue([8; 8], RelayRole::Viewer, unix_now() + 60);
        assert_eq!(
            r.handle(
                &relay_frame(RelayKind::Bind, &sid, &elsewhere.encode()),
                from,
                now,
                unix_now()
            ),
            Action::Send(from, refused(BindRefusal::BadToken))
        );
        let mut tampered = issuer.issue(sid, RelayRole::Viewer, unix_now() + 60);
        tampered.role = RelayRole::Host;
        assert_eq!(
            r.handle(
                &relay_frame(RelayKind::Bind, &sid, &tampered.encode()),
                from,
                now,
                unix_now()
            ),
            Action::Send(from, refused(BindRefusal::BadToken))
        );
        assert_eq!(r.session_count(), 0);
    }

    #[test]
    fn caps_sessions_and_bandwidth() {
        let (issuer, mut r, _) = setup();
        bind(&mut r, &issuer, [1; 8], RelayRole::Viewer, "198.51.100.1:1");
        bind(&mut r, &issuer, [2; 8], RelayRole::Viewer, "198.51.100.1:2");
        let third = bind(&mut r, &issuer, [3; 8], RelayRole::Viewer, "198.51.100.1:3");
        assert_eq!(
            third,
            Action::Send(
                sa("198.51.100.1:3"),
                relay_frame(RelayKind::BindRefused, &[3; 8], &[BindRefusal::Full as u8])
            )
        );
        bind(&mut r, &issuer, [1; 8], RelayRole::Host, "198.51.100.2:1");
        let big = relay_frame(RelayKind::Data, &[1; 8], &[0u8; 1200]);
        let now = Instant::now();
        let mut sent = 0;
        for _ in 0..1000 {
            if matches!(
                r.handle(&big, sa("198.51.100.1:1"), now, unix_now()),
                Action::Send(..)
            ) {
                sent += 1;
            }
        }
        // 1 MB/s with a 256 KB burst: roughly 210 frames at once, not 1000.
        assert!((150..300).contains(&sent), "{sent}");
    }

    #[test]
    fn relay_survives_garbage() {
        let (_, mut r, _) = setup();
        let mut x = 0x5151u64;
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let len = (x % 120) as usize;
            let mut b: Vec<u8> = (0..len).map(|i| (x >> (i % 56)) as u8).collect();
            if b.len() > 4 {
                b[0] = 0x7E;
                b[1] = 1;
                b[2] = (x % 4) as u8;
                b[3] = 0;
            }
            let _ = r.handle(&b, sa("198.51.100.9:9"), Instant::now(), unix_now());
        }
    }
}
