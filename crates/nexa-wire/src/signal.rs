//! Signaling (§4.3) and relay (§4.6) wire formats.
//!
//! The signaling server and the relay are untrusted: everything decoded here may
//! come from an attacker, so every accessor validates and nothing panics.
//!
//! ```protobuf
//! // One binary WebSocket message carries one envelope.
//! message SignalServerMsg { oneof body {
//!   Challenge challenge = 1;  Registered registered = 2;  PeerInfo peer = 3;
//!   Routed routed = 4;  Incoming incoming = 5;  Answer answer = 6;  Trickle trickle = 7;
//!   SignalError error = 8;
//! } }
//! message SignalClientMsg { oneof body {
//!   Register register = 1;  Lookup lookup = 2;  Connect connect = 3;
//!   Answer answer = 4;  Trickle trickle = 5;
//! } }
//! message Challenge  { uint32 proto = 1; bytes nonce = 2; }
//! message Register   { uint32 proto = 1; bytes pubkey = 2; bytes x25519 = 3;
//!                      bytes x25519_sig = 4; bytes sig = 5; string server_name = 6; }
//! message Registered { string short_id = 1; repeated string stun_servers = 2;
//!                      repeated string relays = 3; string stun_note = 4; }
//! message Lookup     { uint64 req_id = 1; string short_id = 2; }
//! message PeerInfo   { uint64 req_id = 1; string short_id = 2; bytes pubkey = 3;
//!                      bytes x25519 = 4; bytes x25519_sig = 5; }
//! message Connect    { string target_short_id = 1; bytes session_id = 2; bytes sealed = 3; }
//! message Routed     { bytes session_id = 1; RelayGrant relay = 2; }
//! message Incoming   { bytes session_id = 1; bytes from_pubkey = 2; bytes from_x25519 = 3;
//!                      bytes from_x25519_sig = 4; bytes sealed = 5; RelayGrant relay = 6; }
//! message Answer     { bytes session_id = 1; bytes sealed = 2; }
//! message Trickle    { bytes session_id = 1; bytes sealed = 2; }
//! message SignalError { ErrorCode code = 1; bytes session_id = 2; uint64 req_id = 3; string message = 4; }
//! message RelayGrant { string relay = 1; bytes token = 2; }
//!
//! // Inside `sealed` (HPKE, §4.3); the server never sees these.
//! message Rendezvous { string ufrag = 1; string pwd = 2; repeated WireCandidate candidates = 3;
//!                      NatKind nat = 4; sint32 port_delta = 5; }
//! message WireCandidate { bytes ip = 1; uint32 port = 2; WireCandKind kind = 3; uint32 foundation = 4; }
//! ```

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use prost::{Enumeration, Message, Oneof};

use crate::{Result, WireError};

pub const SIGNAL_PROTOCOL: u32 = 1;
/// Largest WebSocket message either side accepts.
pub const MAX_SIGNAL_MESSAGE: usize = 16 * 1024;
/// Largest sealed blob the server forwards.
pub const MAX_SEALED: usize = 8 * 1024;
pub const SESSION_ID_LEN: usize = 16;
pub const NONCE_LEN: usize = 32;
/// Candidates accepted from one peer, offer and trickles together.
pub const MAX_CANDIDATES: usize = 32;
pub const SHORT_ID_DIGITS: usize = 9;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Enumeration)]
#[repr(i32)]
pub enum ErrorCode {
    Unspecified = 0,
    /// The target ID is not registered right now.
    Offline = 1,
    RateLimited = 2,
    /// No such ID, or a session that does not exist or is not yours.
    Unknown = 3,
    BadRequest = 4,
    /// A request other than `Register` before registering.
    NotRegistered = 5,
    /// The server is full.
    Unavailable = 6,
}

#[derive(Clone, PartialEq, Message)]
pub struct Challenge {
    #[prost(uint32, tag = "1")]
    pub proto: u32,
    #[prost(bytes = "vec", tag = "2")]
    pub nonce: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Register {
    #[prost(uint32, tag = "1")]
    pub proto: u32,
    #[prost(bytes = "vec", tag = "2")]
    pub pubkey: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    pub x25519: Vec<u8>,
    /// The device's Ed25519 signature over `x25519` ([`PURPOSE_X25519`]).
    #[prost(bytes = "vec", tag = "4")]
    pub x25519_sig: Vec<u8>,
    /// Signature over [`register_message`] ([`PURPOSE_REGISTER`]).
    #[prost(bytes = "vec", tag = "5")]
    pub sig: Vec<u8>,
    /// The host name the client dialed, bound into `sig` so that a registration
    /// made for one server cannot be replayed at another.
    #[prost(string, tag = "6")]
    pub server_name: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct Registered {
    #[prost(string, tag = "1")]
    pub short_id: String,
    #[prost(string, repeated, tag = "2")]
    pub stun_servers: Vec<String>,
    #[prost(string, repeated, tag = "3")]
    pub relays: Vec<String>,
    /// Why NAT classification is degraded, if it is.
    #[prost(string, tag = "4")]
    pub stun_note: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct Lookup {
    #[prost(uint64, tag = "1")]
    pub req_id: u64,
    #[prost(string, tag = "2")]
    pub short_id: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct PeerInfo {
    #[prost(uint64, tag = "1")]
    pub req_id: u64,
    #[prost(string, tag = "2")]
    pub short_id: String,
    #[prost(bytes = "vec", tag = "3")]
    pub pubkey: Vec<u8>,
    #[prost(bytes = "vec", tag = "4")]
    pub x25519: Vec<u8>,
    #[prost(bytes = "vec", tag = "5")]
    pub x25519_sig: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Connect {
    #[prost(string, tag = "1")]
    pub target_short_id: String,
    #[prost(bytes = "vec", tag = "2")]
    pub session_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    pub sealed: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct RelayGrant {
    /// `ip:port` of the UDP relay.
    #[prost(string, tag = "1")]
    pub relay: String,
    /// An encoded [`RelayToken`].
    #[prost(bytes = "vec", tag = "2")]
    pub token: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Routed {
    #[prost(bytes = "vec", tag = "1")]
    pub session_id: Vec<u8>,
    #[prost(message, optional, tag = "2")]
    pub relay: Option<RelayGrant>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Incoming {
    #[prost(bytes = "vec", tag = "1")]
    pub session_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub from_pubkey: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    pub from_x25519: Vec<u8>,
    #[prost(bytes = "vec", tag = "4")]
    pub from_x25519_sig: Vec<u8>,
    #[prost(bytes = "vec", tag = "5")]
    pub sealed: Vec<u8>,
    #[prost(message, optional, tag = "6")]
    pub relay: Option<RelayGrant>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Answer {
    #[prost(bytes = "vec", tag = "1")]
    pub session_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub sealed: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Trickle {
    #[prost(bytes = "vec", tag = "1")]
    pub session_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub sealed: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SignalError {
    #[prost(enumeration = "ErrorCode", tag = "1")]
    pub code: i32,
    #[prost(bytes = "vec", tag = "2")]
    pub session_id: Vec<u8>,
    #[prost(uint64, tag = "3")]
    pub req_id: u64,
    #[prost(string, tag = "4")]
    pub message: String,
}

#[derive(Clone, PartialEq, Oneof)]
pub enum ServerBody {
    #[prost(message, tag = "1")]
    Challenge(Challenge),
    #[prost(message, tag = "2")]
    Registered(Registered),
    #[prost(message, tag = "3")]
    Peer(PeerInfo),
    #[prost(message, tag = "4")]
    Routed(Routed),
    #[prost(message, tag = "5")]
    Incoming(Incoming),
    #[prost(message, tag = "6")]
    Answer(Answer),
    #[prost(message, tag = "7")]
    Trickle(Trickle),
    #[prost(message, tag = "8")]
    Error(SignalError),
}

#[derive(Clone, PartialEq, Message)]
pub struct SignalServerMsg {
    #[prost(oneof = "ServerBody", tags = "1, 2, 3, 4, 5, 6, 7, 8")]
    pub body: Option<ServerBody>,
}

impl From<ServerBody> for SignalServerMsg {
    fn from(body: ServerBody) -> Self {
        Self { body: Some(body) }
    }
}

#[derive(Clone, PartialEq, Oneof)]
pub enum ClientBody {
    #[prost(message, tag = "1")]
    Register(Register),
    #[prost(message, tag = "2")]
    Lookup(Lookup),
    #[prost(message, tag = "3")]
    Connect(Connect),
    #[prost(message, tag = "4")]
    Answer(Answer),
    #[prost(message, tag = "5")]
    Trickle(Trickle),
}

#[derive(Clone, PartialEq, Message)]
pub struct SignalClientMsg {
    #[prost(oneof = "ClientBody", tags = "1, 2, 3, 4, 5")]
    pub body: Option<ClientBody>,
}

impl From<ClientBody> for SignalClientMsg {
    fn from(body: ClientBody) -> Self {
        Self { body: Some(body) }
    }
}

/// Signature purposes (`DeviceIdentity::sign_for`).
pub const PURPOSE_X25519: &str = "signal x25519 key";
pub const PURPOSE_REGISTER: &str = "signal register";

/// What a `Register.sig` covers: the server's nonce, the X25519 key being
/// registered and the server name the client dialed.
pub fn register_message(nonce: &[u8], x25519: &[u8], server_name: &str) -> Vec<u8> {
    let mut m = Vec::with_capacity(nonce.len() + x25519.len() + server_name.len() + 8);
    for part in [nonce, x25519, server_name.as_bytes()] {
        m.extend_from_slice(&(part.len() as u32).to_be_bytes());
        m.extend_from_slice(part);
    }
    m
}

// ---------------------------------------------------------------------------------
// Sealed contents
// ---------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Enumeration)]
#[repr(i32)]
pub enum WireCandKind {
    Host = 0,
    Ipv6 = 1,
    Srflx = 2,
    Portmap = 3,
    Tailnet = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Enumeration)]
#[repr(i32)]
pub enum NatKind {
    /// Not measured: fewer than two STUN answers.
    Unknown = 0,
    Easy = 1,
    Hard = 2,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireCandidate {
    #[prost(bytes = "vec", tag = "1")]
    pub ip: Vec<u8>,
    #[prost(uint32, tag = "2")]
    pub port: u32,
    #[prost(enumeration = "WireCandKind", tag = "3")]
    pub kind: i32,
    #[prost(uint32, tag = "4")]
    pub foundation: u32,
}

impl WireCandidate {
    pub fn new(addr: SocketAddr, kind: WireCandKind, foundation: u32) -> Self {
        let ip = match addr.ip() {
            IpAddr::V4(v4) => v4.octets().to_vec(),
            IpAddr::V6(v6) => v6.octets().to_vec(),
        };
        Self {
            ip,
            port: u32::from(addr.port()),
            kind: kind as i32,
            foundation,
        }
    }

    /// The address, if it is one a peer may be asked to send to: a well-formed,
    /// unicast, non-zero-port address.
    pub fn addr(&self) -> Result<SocketAddr> {
        let ip = match self.ip.len() {
            4 => IpAddr::V4(Ipv4Addr::from(
                <[u8; 4]>::try_from(&self.ip[..]).unwrap_or([0; 4]),
            )),
            16 => {
                let v6 = Ipv6Addr::from(<[u8; 16]>::try_from(&self.ip[..]).unwrap_or([0; 16]));
                match v6.to_ipv4_mapped() {
                    Some(v4) => IpAddr::V4(v4),
                    None => IpAddr::V6(v6),
                }
            }
            _ => return Err(WireError::Invalid("candidate address length")),
        };
        let port = u16::try_from(self.port).map_err(|_| WireError::Invalid("candidate port"))?;
        if port == 0 || ip.is_unspecified() || ip.is_multicast() {
            return Err(WireError::Invalid("candidate address"));
        }
        if let IpAddr::V4(v4) = ip
            && (v4.is_broadcast() || v4.octets()[0] == 0)
        {
            return Err(WireError::Invalid("candidate address"));
        }
        Ok(SocketAddr::new(ip, port))
    }

    pub fn checked_kind(&self) -> Result<WireCandKind> {
        WireCandKind::try_from(self.kind).map_err(|_| WireError::Invalid("candidate kind"))
    }
}

/// Kinds of sealed message, bound into the HPKE associated data so that one
/// cannot be passed off as another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum SealedKind {
    Offer = 1,
    Answer = 2,
    Trickle = 3,
}

#[derive(Clone, PartialEq, Message)]
pub struct Rendezvous {
    #[prost(string, tag = "1")]
    pub ufrag: String,
    #[prost(string, tag = "2")]
    pub pwd: String,
    #[prost(message, repeated, tag = "3")]
    pub candidates: Vec<WireCandidate>,
    #[prost(enumeration = "NatKind", tag = "4")]
    pub nat: i32,
    #[prost(sint32, tag = "5")]
    pub port_delta: i32,
}

pub const MIN_UFRAG: usize = 4;
pub const MAX_UFRAG: usize = 64;
pub const MIN_PWD: usize = 22;
pub const MAX_PWD: usize = 128;

/// ICE-style credential characters: what our generator produces, and nothing a
/// USERNAME separator or a terminal could misread.
pub fn valid_credential(s: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

// ---------------------------------------------------------------------------------
// Short IDs
// ---------------------------------------------------------------------------------

/// Accepts "123456789", "123 456 789" or "123-456-789"; returns the bare digits.
pub fn normalize_short_id(s: &str) -> Option<String> {
    let digits: String = s
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '\u{00A0}'))
        .collect();
    (digits.len() == SHORT_ID_DIGITS && digits.bytes().all(|b| b.is_ascii_digit()))
        .then_some(digits)
}

/// "123 456 789", for display.
pub fn group_short_id(id: &str) -> String {
    match normalize_short_id(id) {
        Some(d) => format!("{} {} {}", &d[..3], &d[3..6], &d[6..]),
        None => id.to_owned(),
    }
}

// ---------------------------------------------------------------------------------
// Relay (§4.6)
// ---------------------------------------------------------------------------------

pub const RELAY_MAGIC: u8 = 0x7E;
pub const RELAY_VERSION: u8 = 1;
/// §4.6 draws a 10-byte header (`0x7E, ver, session_id`) but §4.8 budgets 12;
/// the two spare bytes carry the frame kind and a reserved zero.
pub const RELAY_HEADER_LEN: usize = 12;
pub const RELAY_SESSION_LEN: usize = 8;

pub type RelaySession = [u8; RELAY_SESSION_LEN];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RelayKind {
    /// Forwarded untouched to the other side of the session.
    Data = 0,
    /// Payload: an encoded [`RelayToken`].
    Bind = 1,
    BindOk = 2,
    /// Payload: one [`BindRefusal`] byte.
    BindRefused = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum BindRefusal {
    BadToken = 1,
    Expired = 2,
    Full = 3,
    RateLimited = 4,
}

impl BindRefusal {
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            1 => Some(Self::BadToken),
            2 => Some(Self::Expired),
            3 => Some(Self::Full),
            4 => Some(Self::RateLimited),
            _ => None,
        }
    }
}

/// Is this datagram plausibly a relay frame? Cheap, for the receive path.
pub fn is_relay_frame(packet: &[u8]) -> bool {
    packet.len() >= RELAY_HEADER_LEN && packet[0] == RELAY_MAGIC && packet[1] == RELAY_VERSION
}

pub fn write_relay_header(kind: RelayKind, session: &RelaySession, out: &mut Vec<u8>) {
    out.extend_from_slice(&[RELAY_MAGIC, RELAY_VERSION, kind as u8, 0]);
    out.extend_from_slice(session);
}

pub fn relay_frame(kind: RelayKind, session: &RelaySession, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(RELAY_HEADER_LEN + payload.len());
    write_relay_header(kind, session, &mut out);
    out.extend_from_slice(payload);
    out
}

/// Splits a relay frame into kind, session and payload.
pub fn parse_relay_frame(packet: &[u8]) -> Result<(RelayKind, RelaySession, &[u8])> {
    if packet.len() < RELAY_HEADER_LEN {
        return Err(WireError::TooShort {
            need: RELAY_HEADER_LEN,
            have: packet.len(),
        });
    }
    if packet[0] != RELAY_MAGIC || packet[1] != RELAY_VERSION || packet[3] != 0 {
        return Err(WireError::BadType(packet[0]));
    }
    let kind = match packet[2] {
        0 => RelayKind::Data,
        1 => RelayKind::Bind,
        2 => RelayKind::BindOk,
        3 => RelayKind::BindRefused,
        _ => return Err(WireError::Invalid("relay frame kind")),
    };
    let mut session = [0u8; RELAY_SESSION_LEN];
    session.copy_from_slice(&packet[4..RELAY_HEADER_LEN]);
    Ok((kind, session, &packet[RELAY_HEADER_LEN..]))
}

/// Which end of a relay session a token admits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RelayRole {
    Viewer = 0,
    Host = 1,
}

/// `token = server signature {session, expiry}` (§4.6), plus the role, so that
/// one side's token cannot take the other side's seat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelayToken {
    pub session: RelaySession,
    pub role: RelayRole,
    /// Unix seconds.
    pub expiry: u64,
    pub sig: [u8; 64],
}

pub const RELAY_TOKEN_LEN: usize = RELAY_SESSION_LEN + 1 + 8 + 64;

impl RelayToken {
    /// The bytes the server signs.
    pub fn signed_part(session: &RelaySession, role: RelayRole, expiry: u64) -> Vec<u8> {
        let mut m = Vec::with_capacity(48);
        m.extend_from_slice(b"nexa/1 relay token\0");
        m.extend_from_slice(session);
        m.push(role as u8);
        m.extend_from_slice(&expiry.to_be_bytes());
        m
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(RELAY_TOKEN_LEN);
        out.extend_from_slice(&self.session);
        out.push(self.role as u8);
        out.extend_from_slice(&self.expiry.to_be_bytes());
        out.extend_from_slice(&self.sig);
        out
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() != RELAY_TOKEN_LEN {
            return Err(WireError::Invalid("relay token length"));
        }
        let mut session = [0u8; RELAY_SESSION_LEN];
        session.copy_from_slice(&b[..8]);
        let role = match b[8] {
            0 => RelayRole::Viewer,
            1 => RelayRole::Host,
            _ => return Err(WireError::Invalid("relay role")),
        };
        let mut expiry = [0u8; 8];
        expiry.copy_from_slice(&b[9..17]);
        let mut sig = [0u8; 64];
        sig.copy_from_slice(&b[17..]);
        Ok(Self {
            session,
            role,
            expiry: u64::from_be_bytes(expiry),
            sig,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_ids() {
        assert_eq!(
            normalize_short_id("123 456-789").as_deref(),
            Some("123456789")
        );
        assert_eq!(normalize_short_id("12345678"), None);
        assert_eq!(normalize_short_id("1234567890"), None);
        assert_eq!(normalize_short_id("12345678a"), None);
        assert_eq!(
            normalize_short_id("１２３４５６７８９"),
            None,
            "full-width digits"
        );
        assert_eq!(group_short_id("123456789"), "123 456 789");
    }

    #[test]
    fn candidate_addresses_are_validated() {
        let good: SocketAddr = "203.0.113.5:4000".parse().unwrap();
        let c = WireCandidate::new(good, WireCandKind::Srflx, 7);
        assert_eq!(c.addr().unwrap(), good);
        assert_eq!(c.checked_kind().unwrap(), WireCandKind::Srflx);
        let v6: SocketAddr = "[2001:db8::1]:9".parse().unwrap();
        assert_eq!(
            WireCandidate::new(v6, WireCandKind::Ipv6, 0)
                .addr()
                .unwrap(),
            v6
        );
        let mapped: SocketAddr = "[::ffff:198.51.100.1]:9".parse().unwrap();
        assert_eq!(
            WireCandidate::new(mapped, WireCandKind::Host, 0)
                .addr()
                .unwrap(),
            "198.51.100.1:9".parse::<SocketAddr>().unwrap()
        );
        for bad in [
            "0.0.0.0:1",
            "203.0.113.5:0",
            "255.255.255.255:9",
            "224.0.0.1:9",
            "[::]:9",
            "0.1.2.3:4",
        ] {
            let c = WireCandidate::new(bad.parse().unwrap(), WireCandKind::Host, 0);
            assert!(c.addr().is_err(), "{bad}");
        }
        let mut c = WireCandidate::new(good, WireCandKind::Host, 0);
        c.ip.push(1);
        assert!(c.addr().is_err());
        c.ip.truncate(4);
        c.port = 70_000;
        assert!(c.addr().is_err());
        c.kind = 99;
        assert!(c.checked_kind().is_err());
    }

    #[test]
    fn credentials() {
        assert!(valid_credential("abcd", MIN_UFRAG, MAX_UFRAG));
        assert!(!valid_credential("ab:d", MIN_UFRAG, MAX_UFRAG));
        assert!(!valid_credential("abc", MIN_UFRAG, MAX_UFRAG));
        assert!(!valid_credential("a\u{1b}cd", MIN_UFRAG, MAX_UFRAG));
    }

    #[test]
    fn relay_frames_round_trip_and_reject_garbage() {
        let sid = [9u8; 8];
        let f = relay_frame(RelayKind::Data, &sid, b"payload");
        assert_eq!(f.len(), RELAY_HEADER_LEN + 7);
        assert!(is_relay_frame(&f));
        let (kind, s, p) = parse_relay_frame(&f).unwrap();
        assert_eq!((kind, s, p), (RelayKind::Data, sid, &b"payload"[..]));
        assert!(parse_relay_frame(&f[..11]).is_err());
        let mut bad = f.clone();
        bad[2] = 9;
        assert!(parse_relay_frame(&bad).is_err());
        let mut bad = f.clone();
        bad[3] = 1;
        assert!(parse_relay_frame(&bad).is_err());
        let mut bad = f;
        bad[0] = 0x40;
        assert!(!is_relay_frame(&bad));
        assert!(parse_relay_frame(&bad).is_err());
    }

    #[test]
    fn tokens_round_trip() {
        let t = RelayToken {
            session: [1, 2, 3, 4, 5, 6, 7, 8],
            role: RelayRole::Host,
            expiry: 1_900_000_000,
            sig: [0xAB; 64],
        };
        let b = t.encode();
        assert_eq!(b.len(), RELAY_TOKEN_LEN);
        assert_eq!(RelayToken::decode(&b).unwrap(), t);
        assert!(RelayToken::decode(&b[1..]).is_err());
        let mut bad = b;
        bad[8] = 2;
        assert!(RelayToken::decode(&bad).is_err());
    }

    /// Everything here faces the network: decoding garbage must not panic.
    #[test]
    fn decoders_survive_garbage() {
        let mut x = 0x0123_4567_89AB_CDEFu64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let seed = SignalServerMsg::from(ServerBody::Incoming(Incoming {
            session_id: vec![1; 16],
            from_pubkey: vec![2; 32],
            from_x25519: vec![3; 32],
            from_x25519_sig: vec![4; 64],
            sealed: vec![5; 200],
            relay: Some(RelayGrant {
                relay: "203.0.113.1:3480".into(),
                token: vec![6; RELAY_TOKEN_LEN],
            }),
        }))
        .encode_to_vec();
        let rdv = Rendezvous {
            ufrag: "abcd".into(),
            pwd: "x".repeat(24),
            candidates: vec![WireCandidate::new(
                "198.51.100.1:5".parse().unwrap(),
                WireCandKind::Host,
                1,
            )],
            nat: NatKind::Hard as i32,
            port_delta: -3,
        }
        .encode_to_vec();
        for round in 0..50_000 {
            let base = if round % 2 == 0 { &seed } else { &rdv };
            let mut b = base.clone();
            for _ in 0..(next() % 8) {
                let i = (next() as usize) % b.len();
                b[i] = next() as u8;
            }
            b.truncate((next() as usize) % (b.len() + 1));
            let _ = SignalServerMsg::decode(b.as_slice());
            let _ = SignalClientMsg::decode(b.as_slice());
            if let Ok(r) = Rendezvous::decode(b.as_slice()) {
                for c in &r.candidates {
                    let _ = c.addr();
                    let _ = c.checked_kind();
                }
            }
            let _ = parse_relay_frame(&b);
            let _ = RelayToken::decode(&b);
        }
    }
}
