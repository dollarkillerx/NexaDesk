//! The TURN server, as a state machine: [`Server::handle`] takes a datagram from a
//! client and returns the datagrams to send.
//!
//! Supported: Binding (it answers as a STUN server too), Allocate over UDP, Refresh,
//! CreatePermission, ChannelBind, Send and Data indications, ChannelData. Credentials
//! are long-term ones derived from the configured secret (`crate::auth`).
//!
//! Relayed addresses are the server's public IP with a virtual port: no socket is
//! opened for them. Data sent to a relayed address of this server is handed straight to
//! that allocation's client, if that allocation has given the sender permission; data
//! for anywhere else is dropped. So the server only ever connects its own clients to
//! each other, which is all browser access needs, and cannot be used to reach anything
//! else.

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};

use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;

use crate::{auth, msg::*};

const DEFAULT_LIFETIME: u32 = 600;
const MAX_LIFETIME: u32 = 3600;
const PERMISSION_LIFETIME: Duration = Duration::from_secs(300);
const CHANNEL_LIFETIME: Duration = Duration::from_secs(600);
const NONCE_LIFETIME: u64 = 3600;
const FIRST_PORT: u16 = 49152;
const UNKNOWN_ATTRIBUTES: u16 = 0x000A;

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub realm: String,
    /// Derives every password (`crate::auth`).
    pub secret: Vec<u8>,
    /// The server's public address, in every relayed address it hands out.
    pub relay_ip: IpAddr,
    pub max_allocations: usize,
    pub max_per_ip: usize,
}

struct Alloc {
    relay: SocketAddr,
    username: String,
    expires: Instant,
    perms: HashMap<IpAddr, Instant>,
    /// Channel → peer, and back.
    channels: HashMap<u16, (SocketAddr, Instant)>,
    by_peer: HashMap<SocketAddr, u16>,
    /// The Allocate that made it, answered again if it is sent again.
    made_by: [u8; 12],
    made_reply: Vec<u8>,
}

impl Alloc {
    fn permits(&self, ip: IpAddr, now: Instant) -> bool {
        self.perms
            .get(&ip.to_canonical())
            .is_some_and(|until| *until > now)
    }
}

/// A datagram to send, and where.
pub type Out = (SocketAddr, Vec<u8>);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ServerStats {
    pub allocations: usize,
    pub relayed_packets: u64,
    pub relayed_bytes: u64,
}

pub struct Server {
    cfg: ServerConfig,
    nonce_key: [u8; 32],
    allocs: HashMap<SocketAddr, Alloc>,
    by_port: HashMap<u16, SocketAddr>,
    per_ip: HashMap<IpAddr, usize>,
    next_port: u16,
    next_txid: u64,
    stats: ServerStats,
}

impl Server {
    /// `nonce_key` signs nonces; draw it from a CSPRNG.
    pub fn new(mut cfg: ServerConfig, nonce_key: [u8; 32]) -> Self {
        cfg.relay_ip = cfg.relay_ip.to_canonical();
        Self {
            cfg,
            nonce_key,
            allocs: HashMap::new(),
            by_port: HashMap::new(),
            per_ip: HashMap::new(),
            next_port: FIRST_PORT,
            next_txid: 1,
            stats: ServerStats::default(),
        }
    }

    pub fn stats(&self) -> ServerStats {
        ServerStats {
            allocations: self.allocs.len(),
            ..self.stats
        }
    }

    /// One datagram from `from`. `unix` is the wall clock, for credential expiry.
    pub fn handle(&mut self, from: SocketAddr, data: &[u8], now: Instant, unix: u64) -> Vec<Out> {
        let from = canonical(from);
        if let Some((ch, payload)) = parse_channel_data(data) {
            return self
                .channel_data_in(from, ch, payload, now)
                .into_iter()
                .collect();
        }
        let Some(m) = Msg::parse(data) else {
            return Vec::new();
        };
        let reply = match (m.method, m.class) {
            (BINDING, Class::Request) => Some(
                Builder::new(BINDING, Class::Success, m.txid)
                    .addr(XOR_MAPPED_ADDRESS, from)
                    .fingerprint()
                    .build(),
            ),
            (SEND, Class::Indication) => {
                return self.send_indication(from, &m, now).into_iter().collect();
            }
            (ALLOCATE, Class::Request) => Some(self.allocate(from, &m, now, unix)),
            (REFRESH, Class::Request) => Some(self.refresh(from, &m, now, unix)),
            (CREATE_PERMISSION, Class::Request) => Some(self.permission(from, &m, now, unix)),
            (CHANNEL_BIND, Class::Request) => Some(self.bind(from, &m, now, unix)),
            _ => None,
        };
        reply.map(|r| vec![(from, r)]).unwrap_or_default()
    }

    /// Forgets expired allocations, permissions and channels.
    pub fn sweep(&mut self, now: Instant) {
        let dead: Vec<SocketAddr> = self
            .allocs
            .iter()
            .filter(|(_, a)| a.expires <= now)
            .map(|(c, _)| *c)
            .collect();
        for client in dead {
            self.remove(client);
        }
        for a in self.allocs.values_mut() {
            a.perms.retain(|_, until| *until > now);
            a.channels.retain(|_, (_, until)| *until > now);
            let live: Vec<SocketAddr> = a.channels.values().map(|(p, _)| *p).collect();
            a.by_peer.retain(|p, _| live.contains(p));
        }
    }

    fn remove(&mut self, client: SocketAddr) {
        if let Some(a) = self.allocs.remove(&client) {
            self.by_port.remove(&a.relay.port());
            if let Some(n) = self.per_ip.get_mut(&client.ip()) {
                *n -= 1;
                if *n == 0 {
                    self.per_ip.remove(&client.ip());
                }
            }
        }
    }

    fn live(&self, client: SocketAddr, now: Instant) -> Option<&Alloc> {
        self.allocs.get(&client).filter(|a| a.expires > now)
    }

    fn nonce(&self, unix: u64) -> String {
        format!("{unix:x}.{}", self.nonce_tag(unix))
    }

    fn nonce_tag(&self, unix: u64) -> String {
        let mut mac = <Hmac<Sha1> as KeyInit>::new_from_slice(&self.nonce_key).expect("key");
        mac.update(&unix.to_be_bytes());
        data_encoding::HEXLOWER.encode(&mac.finalize().into_bytes()[..8])
    }

    fn nonce_fresh(&self, nonce: &str, unix: u64) -> bool {
        let Some((ts, tag)) = nonce.split_once('.') else {
            return false;
        };
        let Ok(ts) = u64::from_str_radix(ts, 16) else {
            return false;
        };
        unix.saturating_sub(ts) < NONCE_LIFETIME && ts <= unix + 60 && self.nonce_tag(ts) == tag
    }

    fn error(&self, m: &Msg, code: u16, reason: &str, unix: u64, key: Option<&[u8]>) -> Vec<u8> {
        let mut b = Builder::new(m.method, Class::Error, m.txid).error(code, reason);
        if code == 401 || code == 438 {
            b = b
                .attr(REALM, self.cfg.realm.as_bytes())
                .attr(NONCE, self.nonce(unix).as_bytes());
        }
        if let Some(k) = key {
            b = b.integrity(k);
        }
        b.fingerprint().build()
    }

    /// The username and key a request is signed with, or the error to answer.
    fn authenticate(&self, m: &Msg, unix: u64) -> Result<(String, [u8; 16]), Vec<u8>> {
        if !m.has_integrity() {
            return Err(self.error(m, 401, "Unauthorized", unix, None));
        }
        let (Some(username), Some(realm), Some(nonce)) =
            (m.text(USERNAME), m.text(REALM), m.text(NONCE))
        else {
            return Err(self.error(m, 400, "Bad Request", unix, None));
        };
        if !self.nonce_fresh(nonce, unix) {
            return Err(self.error(m, 438, "Stale Nonce", unix, None));
        }
        if realm != self.cfg.realm || auth::expiry(username).is_none_or(|e| e <= unix) {
            return Err(self.error(m, 401, "Unauthorized", unix, None));
        }
        let password = auth::password_for(&self.cfg.secret, username);
        let key = auth::long_term_key(username, realm, &password);
        if !m.verify(&key) {
            return Err(self.error(m, 401, "Unauthorized", unix, None));
        }
        Ok((username.to_string(), key))
    }

    fn allocate(&mut self, from: SocketAddr, m: &Msg, now: Instant, unix: u64) -> Vec<u8> {
        let unknown = m.unknown_required();
        if !unknown.is_empty() {
            let list: Vec<u8> = unknown.iter().flat_map(|t| t.to_be_bytes()).collect();
            return Builder::new(ALLOCATE, Class::Error, m.txid)
                .error(420, "Unknown Attribute")
                .attr(UNKNOWN_ATTRIBUTES, &list)
                .fingerprint()
                .build();
        }
        if let Some(a) = self.live(from, now) {
            // A retransmission gets the same answer; anything else is a mismatch.
            return if a.made_by == m.txid {
                a.made_reply.clone()
            } else {
                self.error(m, 437, "Allocation Mismatch", unix, None)
            };
        }
        let (username, key) = match self.authenticate(m, unix) {
            Ok(v) => v,
            Err(e) => return e,
        };
        match m.get(REQUESTED_TRANSPORT) {
            Some(v) if v.first() == Some(&TRANSPORT_UDP) => {}
            Some(_) => {
                return self.error(m, 442, "Unsupported Transport Protocol", unix, Some(&key));
            }
            None => return self.error(m, 400, "Bad Request", unix, Some(&key)),
        }
        if m.get(RESERVATION_TOKEN).is_some() {
            return self.error(m, 400, "Bad Request", unix, Some(&key));
        }
        let wants_v6 = m
            .get(REQUESTED_ADDRESS_FAMILY)
            .is_some_and(|v| v.first() == Some(&0x02));
        if wants_v6 != self.cfg.relay_ip.is_ipv6() {
            return self.error(m, 440, "Address Family not Supported", unix, Some(&key));
        }
        let used = self.per_ip.get(&from.ip()).copied().unwrap_or(0);
        if self.allocs.len() >= self.cfg.max_allocations || used >= self.cfg.max_per_ip {
            return self.error(m, 486, "Allocation Quota Reached", unix, Some(&key));
        }
        let Some(port) = self.free_port() else {
            return self.error(m, 508, "Insufficient Capacity", unix, Some(&key));
        };
        // An expired allocation of this client goes before the new one.
        self.remove(from);
        let lifetime = m
            .u32(LIFETIME)
            .unwrap_or(DEFAULT_LIFETIME)
            .clamp(60, MAX_LIFETIME);
        let relay = SocketAddr::new(self.cfg.relay_ip, port);
        let reply = Builder::new(ALLOCATE, Class::Success, m.txid)
            .addr(XOR_RELAYED_ADDRESS, relay)
            .addr(XOR_MAPPED_ADDRESS, from)
            .u32(LIFETIME, lifetime)
            .integrity(&key)
            .fingerprint()
            .build();
        self.allocs.insert(
            from,
            Alloc {
                relay,
                username,
                expires: now + Duration::from_secs(u64::from(lifetime)),
                perms: HashMap::new(),
                channels: HashMap::new(),
                by_peer: HashMap::new(),
                made_by: m.txid,
                made_reply: reply.clone(),
            },
        );
        self.by_port.insert(port, from);
        *self.per_ip.entry(from.ip()).or_default() += 1;
        reply
    }

    fn free_port(&mut self) -> Option<u16> {
        let span = usize::from(u16::MAX - FIRST_PORT) + 1;
        for _ in 0..span {
            let p = self.next_port;
            self.next_port = if p == u16::MAX { FIRST_PORT } else { p + 1 };
            if !self.by_port.contains_key(&p) {
                return Some(p);
            }
        }
        None
    }

    /// The allocation a request is about, authenticated as its owner.
    fn owned(
        &self,
        from: SocketAddr,
        m: &Msg,
        now: Instant,
        unix: u64,
    ) -> Result<[u8; 16], Vec<u8>> {
        let (username, key) = self.authenticate(m, unix)?;
        match self.live(from, now) {
            None => Err(self.error(m, 437, "Allocation Mismatch", unix, Some(&key))),
            Some(a) if a.username != username => {
                Err(self.error(m, 441, "Wrong Credentials", unix, Some(&key)))
            }
            Some(_) => Ok(key),
        }
    }

    fn refresh(&mut self, from: SocketAddr, m: &Msg, now: Instant, unix: u64) -> Vec<u8> {
        let key = match self.owned(from, m, now, unix) {
            Ok(k) => k,
            Err(e) => return e,
        };
        let lifetime = m.u32(LIFETIME).unwrap_or(DEFAULT_LIFETIME);
        if lifetime == 0 {
            self.remove(from);
        } else if let Some(a) = self.allocs.get_mut(&from) {
            let lifetime = lifetime.clamp(60, MAX_LIFETIME);
            a.expires = now + Duration::from_secs(u64::from(lifetime));
        }
        Builder::new(REFRESH, Class::Success, m.txid)
            .u32(LIFETIME, lifetime.min(MAX_LIFETIME))
            .integrity(&key)
            .fingerprint()
            .build()
    }

    fn permission(&mut self, from: SocketAddr, m: &Msg, now: Instant, unix: u64) -> Vec<u8> {
        let key = match self.owned(from, m, now, unix) {
            Ok(k) => k,
            Err(e) => return e,
        };
        let peers = m.addrs(XOR_PEER_ADDRESS);
        if peers.is_empty() {
            return self.error(m, 400, "Bad Request", unix, Some(&key));
        }
        if let Some(a) = self.allocs.get_mut(&from) {
            for p in peers {
                a.perms
                    .insert(p.ip().to_canonical(), now + PERMISSION_LIFETIME);
            }
        }
        Builder::new(CREATE_PERMISSION, Class::Success, m.txid)
            .integrity(&key)
            .fingerprint()
            .build()
    }

    fn bind(&mut self, from: SocketAddr, m: &Msg, now: Instant, unix: u64) -> Vec<u8> {
        let key = match self.owned(from, m, now, unix) {
            Ok(k) => k,
            Err(e) => return e,
        };
        let channel = m
            .get(CHANNEL_NUMBER)
            .filter(|v| v.len() == 4)
            .map(|v| u16::from_be_bytes([v[0], v[1]]))
            .filter(|c| (0x4000..=0x4FFF).contains(c));
        let (Some(channel), Some(peer)) = (channel, m.addr(XOR_PEER_ADDRESS)) else {
            return self.error(m, 400, "Bad Request", unix, Some(&key));
        };
        let peer = canonical(peer);
        let Some(a) = self.allocs.get_mut(&from) else {
            return self.error(m, 437, "Allocation Mismatch", unix, Some(&key));
        };
        let taken_by_other = a.channels.get(&channel).is_some_and(|(p, _)| *p != peer)
            || a.by_peer.get(&peer).is_some_and(|c| *c != channel);
        if taken_by_other {
            return self.error(m, 400, "Bad Request", unix, Some(&key));
        }
        a.channels.insert(channel, (peer, now + CHANNEL_LIFETIME));
        a.by_peer.insert(peer, channel);
        a.perms.insert(peer.ip(), now + PERMISSION_LIFETIME);
        Builder::new(CHANNEL_BIND, Class::Success, m.txid)
            .integrity(&key)
            .fingerprint()
            .build()
    }

    fn send_indication(&mut self, from: SocketAddr, m: &Msg, now: Instant) -> Option<Out> {
        let a = self.live(from, now)?;
        let peer = canonical(m.addr(XOR_PEER_ADDRESS)?);
        let data = m.get(DATA_ATTR)?;
        if !a.permits(peer.ip(), now) {
            return None;
        }
        let src = a.relay;
        self.deliver(src, peer, data, now)
    }

    fn channel_data_in(
        &mut self,
        from: SocketAddr,
        ch: u16,
        data: &[u8],
        now: Instant,
    ) -> Option<Out> {
        let a = self.live(from, now)?;
        let (peer, until) = *a.channels.get(&ch)?;
        if until <= now {
            return None;
        }
        let src = a.relay;
        self.deliver(src, peer, data, now)
    }

    /// From the allocation at `src` to `peer`: only another allocation of ours, which
    /// must have given `src` permission.
    fn deliver(
        &mut self,
        src: SocketAddr,
        peer: SocketAddr,
        data: &[u8],
        now: Instant,
    ) -> Option<Out> {
        if peer.ip() != self.cfg.relay_ip {
            return None;
        }
        let client = *self.by_port.get(&peer.port())?;
        let target = self.live(client, now)?;
        if !target.permits(src.ip(), now) {
            return None;
        }
        let frame = match target.by_peer.get(&src) {
            Some(ch)
                if target
                    .channels
                    .get(ch)
                    .is_some_and(|(_, until)| *until > now) =>
            {
                channel_data(*ch, data)
            }
            _ => {
                let txid = self.txid();
                Builder::new(DATA, Class::Indication, txid)
                    .addr(XOR_PEER_ADDRESS, src)
                    .attr(DATA_ATTR, data)
                    .build()
            }
        };
        self.stats.relayed_packets += 1;
        self.stats.relayed_bytes += data.len() as u64;
        Some((client, frame))
    }

    /// Indications need a transaction id, not an unguessable one.
    fn txid(&mut self) -> [u8; 12] {
        self.next_txid += 1;
        let mut t = [0u8; 12];
        t[..4].copy_from_slice(b"nxtn");
        t[4..].copy_from_slice(&self.next_txid.to_be_bytes());
        t
    }
}

fn canonical(a: SocketAddr) -> SocketAddr {
    SocketAddr::new(a.ip().to_canonical(), a.port())
}
