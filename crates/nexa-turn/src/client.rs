//! A TURN client, as a state machine: the host's side of a relayed browser session.
//!
//! It allocates (answering the server's 401 with the long-term key), keeps the
//! allocation, its permissions and its channels refreshed, and wraps and unwraps
//! data: ChannelData once a channel is bound to a peer, Send and Data indications
//! before that. The caller moves datagrams between it and the server's address
//! ([`Client::poll_transmit`], [`Client::handle`]) and calls [`Client::handle_timeout`]
//! at [`Client::next_timeout`].

use std::{
    collections::{HashMap, VecDeque},
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};

use rand_core::{OsRng, RngCore};

use crate::{auth, msg::*};

/// First retransmission; doubles each time (RFC 8489 §6.2.1, a little shorter).
const RTO: Duration = Duration::from_millis(250);
const MAX_TRIES: u32 = 7;
const REQUESTED_LIFETIME: u32 = 600;
const PERMISSION_REFRESH: Duration = Duration::from_secs(240);
const CHANNEL_REFRESH: Duration = Duration::from_secs(480);
const FIRST_CHANNEL: u16 = 0x4000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientEvent {
    /// The relayed address peers send to.
    Allocated(SocketAddr),
    /// The allocation could not be made or was lost; nothing more will come.
    Failed(String),
    Data {
        from: SocketAddr,
        data: Vec<u8>,
    },
}

#[derive(Clone, Debug)]
enum Kind {
    Allocate,
    Refresh(u32),
    Permission(Vec<IpAddr>),
    Bind(u16, SocketAddr),
}

struct Pending {
    kind: Kind,
    msg: Vec<u8>,
    next: Instant,
    tries: u32,
    /// Already sent again with fresh credentials after a 401 or 438.
    retried_auth: bool,
}

struct Channel {
    number: u16,
    confirmed: bool,
    refresh_at: Instant,
}

pub struct Client {
    server: SocketAddr,
    username: String,
    password: String,
    realm: String,
    nonce: String,
    key: Option<[u8; 16]>,
    relayed: Option<SocketAddr>,
    failed: bool,
    pending: HashMap<[u8; 12], Pending>,
    out: VecDeque<Vec<u8>>,
    events: VecDeque<ClientEvent>,
    refresh_at: Option<Instant>,
    perms: HashMap<IpAddr, Instant>,
    channels: HashMap<SocketAddr, Channel>,
    by_number: HashMap<u16, SocketAddr>,
    next_channel: u16,
    indication: u64,
}

impl Client {
    /// Starts allocating on `server` with `creds`.
    pub fn new(server: SocketAddr, creds: auth::Credentials, now: Instant) -> Self {
        let mut c = Self {
            server,
            username: creds.username,
            password: creds.password,
            realm: String::new(),
            nonce: String::new(),
            key: None,
            relayed: None,
            failed: false,
            pending: HashMap::new(),
            out: VecDeque::new(),
            events: VecDeque::new(),
            refresh_at: None,
            perms: HashMap::new(),
            channels: HashMap::new(),
            by_number: HashMap::new(),
            next_channel: FIRST_CHANNEL,
            indication: 0,
        };
        c.request(Kind::Allocate, now);
        c
    }

    pub fn server(&self) -> SocketAddr {
        self.server
    }

    pub fn relayed(&self) -> Option<SocketAddr> {
        self.relayed
    }

    /// A datagram for the server, if one is due.
    pub fn poll_transmit(&mut self) -> Option<Vec<u8>> {
        self.out.pop_front()
    }

    pub fn poll_event(&mut self) -> Option<ClientEvent> {
        self.events.pop_front()
    }

    /// Lets `ip` send to the relayed address. Peers on the same TURN server send from
    /// its address, so permitting that is enough for them.
    pub fn permit(&mut self, ip: IpAddr, now: Instant) {
        let ip = ip.to_canonical();
        if self.perms.contains_key(&ip) || self.relayed.is_none() {
            self.perms.entry(ip).or_insert(now);
            return;
        }
        self.perms.insert(ip, now + PERMISSION_REFRESH);
        self.request(Kind::Permission(vec![ip]), now);
    }

    /// Sends `data` to `peer` through the relay.
    pub fn send(&mut self, peer: SocketAddr, data: &[u8], now: Instant) {
        if self.relayed.is_none() || self.failed {
            return;
        }
        let peer = SocketAddr::new(peer.ip().to_canonical(), peer.port());
        self.permit(peer.ip(), now);
        if let Some(ch) = self.channels.get(&peer) {
            if ch.confirmed {
                self.out.push_back(channel_data(ch.number, data));
                return;
            }
        } else if self.next_channel <= 0x4FFF {
            let number = self.next_channel;
            self.next_channel += 1;
            self.channels.insert(
                peer,
                Channel {
                    number,
                    confirmed: false,
                    refresh_at: now + CHANNEL_REFRESH,
                },
            );
            self.by_number.insert(number, peer);
            self.request(Kind::Bind(number, peer), now);
        }
        // Until the channel is bound: a Send indication.
        self.indication += 1;
        let mut txid = [0u8; 12];
        txid[..4].copy_from_slice(b"nxsd");
        txid[4..].copy_from_slice(&self.indication.to_be_bytes());
        let msg = Builder::new(SEND, Class::Indication, txid)
            .addr(XOR_PEER_ADDRESS, peer)
            .attr(DATA_ATTR, data)
            .build();
        self.out.push_back(msg);
    }

    /// A datagram from the server.
    pub fn handle(&mut self, data: &[u8], now: Instant) {
        if let Some((number, payload)) = parse_channel_data(data) {
            if let Some(peer) = self.by_number.get(&number) {
                self.events.push_back(ClientEvent::Data {
                    from: *peer,
                    data: payload.to_vec(),
                });
            }
            return;
        }
        let Some(m) = Msg::parse(data) else { return };
        match m.class {
            Class::Indication if m.method == DATA => {
                if let (Some(from), Some(d)) = (m.addr(XOR_PEER_ADDRESS), m.get(DATA_ATTR)) {
                    self.events.push_back(ClientEvent::Data {
                        from,
                        data: d.to_vec(),
                    });
                }
            }
            Class::Success | Class::Error => self.response(&m, now),
            _ => {}
        }
    }

    fn response(&mut self, m: &Msg, now: Instant) {
        let Some(p) = self.pending.remove(&m.txid) else {
            return;
        };
        // Answers we signed for must be signed back.
        if let Some(key) = self.key
            && m.class == Class::Success
            && !m.verify(&key)
        {
            self.pending.insert(m.txid, p);
            return;
        }
        if m.class == Class::Error {
            let code = m.error_code().unwrap_or(0);
            let challenge = code == 401 || code == 438;
            if challenge
                && (!p.retried_auth || code == 438)
                && let (Some(realm), Some(nonce)) = (m.text(REALM), m.text(NONCE))
            {
                self.realm = realm.to_string();
                self.nonce = nonce.to_string();
                self.key = Some(auth::long_term_key(
                    &self.username,
                    &self.realm,
                    &self.password,
                ));
                let txid = self.request(p.kind, now);
                if let Some(again) = self.pending.get_mut(&txid) {
                    again.retried_auth = true;
                }
                return;
            }
            match p.kind {
                Kind::Allocate | Kind::Refresh(_) => {
                    self.fail(format!("TURN error {code}"));
                }
                Kind::Bind(number, peer) => {
                    // Keep using indications for this peer.
                    self.channels.remove(&peer);
                    self.by_number.remove(&number);
                }
                Kind::Permission(_) => {}
            }
            return;
        }
        match p.kind {
            Kind::Allocate => {
                let Some(relayed) = m.addr(XOR_RELAYED_ADDRESS) else {
                    return self.fail("no relayed address".into());
                };
                let lifetime = m.u32(LIFETIME).unwrap_or(REQUESTED_LIFETIME).max(60);
                self.relayed = Some(relayed);
                self.refresh_at = Some(now + Duration::from_secs(u64::from(lifetime) * 4 / 5));
                self.events.push_back(ClientEvent::Allocated(relayed));
                // Permissions asked for before the allocation existed.
                let waiting: Vec<IpAddr> = self.perms.keys().copied().collect();
                if !waiting.is_empty() {
                    for ip in &waiting {
                        self.perms.insert(*ip, now + PERMISSION_REFRESH);
                    }
                    self.request(Kind::Permission(waiting), now);
                }
            }
            Kind::Refresh(lifetime) => {
                let lifetime = m.u32(LIFETIME).unwrap_or(lifetime).max(60);
                self.refresh_at = Some(now + Duration::from_secs(u64::from(lifetime) * 4 / 5));
            }
            Kind::Bind(_, peer) => {
                if let Some(ch) = self.channels.get_mut(&peer) {
                    ch.confirmed = true;
                    ch.refresh_at = now + CHANNEL_REFRESH;
                }
            }
            Kind::Permission(_) => {}
        }
    }

    fn fail(&mut self, why: String) {
        if !self.failed {
            self.failed = true;
            self.pending.clear();
            self.events.push_back(ClientEvent::Failed(why));
        }
    }

    pub fn handle_timeout(&mut self, now: Instant) {
        if self.failed {
            return;
        }
        let due: Vec<[u8; 12]> = self
            .pending
            .iter()
            .filter(|(_, p)| p.next <= now)
            .map(|(t, _)| *t)
            .collect();
        for txid in due {
            let Some(p) = self.pending.get_mut(&txid) else {
                continue;
            };
            if p.tries >= MAX_TRIES {
                let p = self.pending.remove(&txid).expect("present");
                if matches!(p.kind, Kind::Allocate | Kind::Refresh(_)) {
                    self.fail("the TURN server does not answer".into());
                    return;
                }
                continue;
            }
            p.tries += 1;
            p.next = now + RTO * 2u32.pow(p.tries.min(6));
            self.out.push_back(p.msg.clone());
        }
        if self.refresh_at.is_some_and(|t| t <= now) {
            self.refresh_at = None;
            self.request(Kind::Refresh(REQUESTED_LIFETIME), now);
        }
        let stale: Vec<IpAddr> = self
            .perms
            .iter()
            .filter(|(_, t)| **t <= now)
            .map(|(ip, _)| *ip)
            .collect();
        if !stale.is_empty() && self.relayed.is_some() {
            for ip in &stale {
                self.perms.insert(*ip, now + PERMISSION_REFRESH);
            }
            self.request(Kind::Permission(stale), now);
        }
        let rebind: Vec<(u16, SocketAddr)> = self
            .channels
            .iter()
            .filter(|(_, c)| c.confirmed && c.refresh_at <= now)
            .map(|(p, c)| (c.number, *p))
            .collect();
        for (number, peer) in rebind {
            if let Some(c) = self.channels.get_mut(&peer) {
                c.refresh_at = now + CHANNEL_REFRESH;
            }
            self.request(Kind::Bind(number, peer), now);
        }
    }

    pub fn next_timeout(&self) -> Option<Instant> {
        if self.failed {
            return None;
        }
        let pending = self.pending.values().map(|p| p.next);
        let perms = self
            .perms
            .values()
            .copied()
            .filter(|_| self.relayed.is_some());
        let channels = self
            .channels
            .values()
            .filter(|c| c.confirmed)
            .map(|c| c.refresh_at);
        pending
            .chain(perms)
            .chain(channels)
            .chain(self.refresh_at)
            .min()
    }

    /// Gives the allocation back, best effort.
    pub fn close(&mut self, now: Instant) {
        if self.relayed.is_some() && !self.failed {
            self.request(Kind::Refresh(0), now);
        }
        self.failed = true;
    }

    fn request(&mut self, kind: Kind, now: Instant) -> [u8; 12] {
        let mut txid = [0u8; 12];
        OsRng.fill_bytes(&mut txid);
        let method = match kind {
            Kind::Allocate => ALLOCATE,
            Kind::Refresh(_) => REFRESH,
            Kind::Permission(_) => CREATE_PERMISSION,
            Kind::Bind(..) => CHANNEL_BIND,
        };
        let mut b = Builder::new(method, Class::Request, txid);
        b = match &kind {
            Kind::Allocate => b
                .attr(REQUESTED_TRANSPORT, &[TRANSPORT_UDP, 0, 0, 0])
                .u32(LIFETIME, REQUESTED_LIFETIME),
            Kind::Refresh(l) => b.u32(LIFETIME, *l),
            Kind::Permission(ips) => ips
                .iter()
                .fold(b, |b, ip| b.addr(XOR_PEER_ADDRESS, SocketAddr::new(*ip, 0))),
            Kind::Bind(number, peer) => {
                let mut v = number.to_be_bytes().to_vec();
                v.extend_from_slice(&[0, 0]);
                b.attr(CHANNEL_NUMBER, &v).addr(XOR_PEER_ADDRESS, *peer)
            }
        };
        if let Some(key) = self.key {
            b = b
                .attr(USERNAME, self.username.as_bytes())
                .attr(REALM, self.realm.as_bytes())
                .attr(NONCE, self.nonce.as_bytes())
                .integrity(&key);
        }
        let msg = b.fingerprint().build();
        self.out.push_back(msg.clone());
        self.pending.insert(
            txid,
            Pending {
                kind,
                msg,
                next: now + RTO,
                tries: 1,
                retried_auth: false,
            },
        );
        txid
    }
}
