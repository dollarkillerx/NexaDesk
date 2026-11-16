//! STUN messages as TURN uses them (RFC 8489, RFC 8656): any method, the attributes
//! TURN needs, MESSAGE-INTEGRITY (HMAC-SHA1 under the long-term key) and FINGERPRINT;
//! and ChannelData frames. Parsing never panics on what comes off the network.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;

pub const MAGIC: u32 = 0x2112_A442;
pub const HEADER: usize = 20;

pub const BINDING: u16 = 0x001;
pub const ALLOCATE: u16 = 0x003;
pub const REFRESH: u16 = 0x004;
pub const SEND: u16 = 0x006;
pub const DATA: u16 = 0x007;
pub const CREATE_PERMISSION: u16 = 0x008;
pub const CHANNEL_BIND: u16 = 0x009;

pub const USERNAME: u16 = 0x0006;
pub const MESSAGE_INTEGRITY: u16 = 0x0008;
pub const ERROR_CODE: u16 = 0x0009;
pub const CHANNEL_NUMBER: u16 = 0x000C;
pub const LIFETIME: u16 = 0x000D;
pub const XOR_PEER_ADDRESS: u16 = 0x0012;
pub const DATA_ATTR: u16 = 0x0013;
pub const REALM: u16 = 0x0014;
pub const NONCE: u16 = 0x0015;
pub const XOR_RELAYED_ADDRESS: u16 = 0x0016;
pub const REQUESTED_ADDRESS_FAMILY: u16 = 0x0017;
pub const EVEN_PORT: u16 = 0x0018;
pub const REQUESTED_TRANSPORT: u16 = 0x0019;
pub const DONT_FRAGMENT: u16 = 0x001A;
pub const XOR_MAPPED_ADDRESS: u16 = 0x0020;
pub const RESERVATION_TOKEN: u16 = 0x0022;
pub const SOFTWARE: u16 = 0x8022;
pub const FINGERPRINT: u16 = 0x8028;

/// UDP, in REQUESTED-TRANSPORT.
pub const TRANSPORT_UDP: u8 = 17;
const FINGERPRINT_XOR: u32 = 0x5354_554E;
const CRC32: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Request,
    Indication,
    Success,
    Error,
}

impl Class {
    fn bits(self) -> u16 {
        match self {
            Class::Request => 0,
            Class::Indication => 1,
            Class::Success => 2,
            Class::Error => 3,
        }
    }
}

/// The message type for a method and class: the class bits sit inside the method's.
pub fn message_type(method: u16, class: Class) -> u16 {
    let c = class.bits();
    (method & 0x000F)
        | ((method & 0x0070) << 1)
        | ((method & 0x0F80) << 2)
        | ((c & 1) << 4)
        | ((c & 2) << 7)
}

fn split_type(t: u16) -> (u16, Class) {
    let method = (t & 0x000F) | ((t >> 1) & 0x0070) | ((t >> 2) & 0x0F80);
    let class = match ((t >> 4) & 1) | ((t >> 7) & 2) {
        0 => Class::Request,
        1 => Class::Indication,
        2 => Class::Success,
        _ => Class::Error,
    };
    (method, class)
}

/// Whether a datagram is a STUN message (as opposed to ChannelData).
pub fn is_stun(b: &[u8]) -> bool {
    b.len() >= HEADER && b[0] & 0xC0 == 0 && u32::from_be_bytes([b[4], b[5], b[6], b[7]]) == MAGIC
}

#[derive(Clone, Debug)]
pub struct Msg<'a> {
    pub method: u16,
    pub class: Class,
    pub txid: [u8; 12],
    raw: &'a [u8],
    /// Type, value, and the offset of the attribute header.
    attrs: Vec<(u16, &'a [u8], usize)>,
}

impl<'a> Msg<'a> {
    pub fn parse(raw: &'a [u8]) -> Option<Self> {
        if !is_stun(raw) {
            return None;
        }
        let len = usize::from(u16::from_be_bytes([raw[2], raw[3]]));
        if !len.is_multiple_of(4) || HEADER + len != raw.len() {
            return None;
        }
        let (method, class) = split_type(u16::from_be_bytes([raw[0], raw[1]]));
        let mut txid = [0u8; 12];
        txid.copy_from_slice(&raw[8..HEADER]);
        let mut attrs = Vec::new();
        let mut pos = HEADER;
        let mut after_integrity = false;
        while pos < raw.len() {
            if raw.len() - pos < 4 {
                return None;
            }
            let ty = u16::from_be_bytes([raw[pos], raw[pos + 1]]);
            let n = usize::from(u16::from_be_bytes([raw[pos + 2], raw[pos + 3]]));
            let start = pos + 4;
            let padded = n.div_ceil(4) * 4;
            if raw.len() - start < padded {
                return None;
            }
            // RFC 8489 §14.5: after MESSAGE-INTEGRITY only FINGERPRINT counts.
            if !after_integrity || ty == FINGERPRINT {
                attrs.push((ty, &raw[start..start + n], pos));
            }
            if ty == MESSAGE_INTEGRITY {
                after_integrity = true;
            }
            pos = start + padded;
        }
        Some(Self {
            method,
            class,
            txid,
            raw,
            attrs,
        })
    }

    pub fn get(&self, ty: u16) -> Option<&'a [u8]> {
        self.attrs.iter().find(|a| a.0 == ty).map(|a| a.1)
    }

    pub fn get_all(&self, ty: u16) -> impl Iterator<Item = &'a [u8]> + '_ {
        self.attrs.iter().filter(move |a| a.0 == ty).map(|a| a.1)
    }

    pub fn text(&self, ty: u16) -> Option<&'a str> {
        std::str::from_utf8(self.get(ty)?).ok()
    }

    pub fn addr(&self, ty: u16) -> Option<SocketAddr> {
        decode_xor(self.get(ty)?, &self.txid)
    }

    pub fn addrs(&self, ty: u16) -> Vec<SocketAddr> {
        self.get_all(ty)
            .filter_map(|v| decode_xor(v, &self.txid))
            .collect()
    }

    pub fn u32(&self, ty: u16) -> Option<u32> {
        let v = self.get(ty)?;
        (v.len() == 4).then(|| u32::from_be_bytes([v[0], v[1], v[2], v[3]]))
    }

    pub fn error_code(&self) -> Option<u16> {
        let v = self.get(ERROR_CODE)?;
        (v.len() >= 4).then(|| u16::from(v[2] & 0x07) * 100 + u16::from(v[3]))
    }

    /// Attribute types below 0x8000 that this subset does not know: a request carrying
    /// one must be answered with 420.
    pub fn unknown_required(&self) -> Vec<u16> {
        self.attrs
            .iter()
            .map(|a| a.0)
            .filter(|t| *t < 0x8000 && !KNOWN.contains(t))
            .collect()
    }

    pub fn has_integrity(&self) -> bool {
        self.get(MESSAGE_INTEGRITY).is_some()
    }

    /// MESSAGE-INTEGRITY checks out under `key`.
    pub fn verify(&self, key: &[u8]) -> bool {
        let Some(&(_, value, offset)) = self.attrs.iter().find(|a| a.0 == MESSAGE_INTEGRITY) else {
            return false;
        };
        if value.len() != 20 {
            return false;
        }
        let mut head = self.raw[..offset].to_vec();
        let covered = (offset + 24 - HEADER) as u16;
        head[2..4].copy_from_slice(&covered.to_be_bytes());
        let mut mac = <Hmac<Sha1> as KeyInit>::new_from_slice(key).expect("any key length");
        mac.update(&head);
        mac.verify_slice(value).is_ok()
    }
}

const KNOWN: &[u16] = &[
    USERNAME,
    MESSAGE_INTEGRITY,
    ERROR_CODE,
    CHANNEL_NUMBER,
    LIFETIME,
    XOR_PEER_ADDRESS,
    DATA_ATTR,
    REALM,
    NONCE,
    XOR_RELAYED_ADDRESS,
    REQUESTED_ADDRESS_FAMILY,
    EVEN_PORT,
    REQUESTED_TRANSPORT,
    DONT_FRAGMENT,
    XOR_MAPPED_ADDRESS,
    RESERVATION_TOKEN,
    0x0024, // PRIORITY, sent by some ICE agents
    0x0025, // USE-CANDIDATE
];

fn decode_xor(v: &[u8], txid: &[u8; 12]) -> Option<SocketAddr> {
    if v.len() < 4 {
        return None;
    }
    let port = u16::from_be_bytes([v[2], v[3]]) ^ (MAGIC >> 16) as u16;
    match (v[1], v.len()) {
        (0x01, 8) => {
            let x = u32::from_be_bytes([v[4], v[5], v[6], v[7]]) ^ MAGIC;
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(x)), port))
        }
        (0x02, 20) => {
            let mut mask = [0u8; 16];
            mask[..4].copy_from_slice(&MAGIC.to_be_bytes());
            mask[4..].copy_from_slice(txid);
            let mut ip = [0u8; 16];
            for i in 0..16 {
                ip[i] = v[4 + i] ^ mask[i];
            }
            Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(ip)), port))
        }
        _ => None,
    }
}

fn encode_xor(a: SocketAddr, txid: &[u8; 12]) -> Vec<u8> {
    let port = a.port() ^ (MAGIC >> 16) as u16;
    match a.ip().to_canonical() {
        IpAddr::V4(v4) => {
            let mut v = vec![0, 0x01];
            v.extend_from_slice(&port.to_be_bytes());
            v.extend_from_slice(&(u32::from(v4) ^ MAGIC).to_be_bytes());
            v
        }
        IpAddr::V6(v6) => {
            let mut mask = [0u8; 16];
            mask[..4].copy_from_slice(&MAGIC.to_be_bytes());
            mask[4..].copy_from_slice(txid);
            let mut v = vec![0, 0x02];
            v.extend_from_slice(&port.to_be_bytes());
            v.extend(v6.octets().iter().zip(mask).map(|(a, b)| a ^ b));
            v
        }
    }
}

/// Builds one message. Integrity and fingerprint go last, in that order.
pub struct Builder {
    buf: Vec<u8>,
    txid: [u8; 12],
}

impl Builder {
    pub fn new(method: u16, class: Class, txid: [u8; 12]) -> Self {
        let mut buf = Vec::with_capacity(128);
        buf.extend_from_slice(&message_type(method, class).to_be_bytes());
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&MAGIC.to_be_bytes());
        buf.extend_from_slice(&txid);
        Self { buf, txid }
    }

    pub fn attr(mut self, ty: u16, value: &[u8]) -> Self {
        self.buf.extend_from_slice(&ty.to_be_bytes());
        self.buf
            .extend_from_slice(&(value.len() as u16).to_be_bytes());
        self.buf.extend_from_slice(value);
        while !self.buf.len().is_multiple_of(4) {
            self.buf.push(0);
        }
        self
    }

    pub fn addr(self, ty: u16, a: SocketAddr) -> Self {
        let v = encode_xor(a, &self.txid);
        self.attr(ty, &v)
    }

    pub fn u32(self, ty: u16, v: u32) -> Self {
        self.attr(ty, &v.to_be_bytes())
    }

    pub fn error(self, code: u16, reason: &str) -> Self {
        let mut v = vec![0, 0, (code / 100) as u8, (code % 100) as u8];
        v.extend_from_slice(reason.as_bytes());
        self.attr(ERROR_CODE, &v)
    }

    fn set_len(&mut self, extra: usize) {
        let len = (self.buf.len() - HEADER + extra) as u16;
        self.buf[2..4].copy_from_slice(&len.to_be_bytes());
    }

    pub fn integrity(mut self, key: &[u8]) -> Self {
        self.set_len(24);
        let mut mac = <Hmac<Sha1> as KeyInit>::new_from_slice(key).expect("any key length");
        mac.update(&self.buf);
        let tag = mac.finalize().into_bytes();
        self.attr(MESSAGE_INTEGRITY, &tag)
    }

    pub fn fingerprint(mut self) -> Self {
        self.set_len(8);
        let crc = CRC32.checksum(&self.buf) ^ FINGERPRINT_XOR;
        self.attr(FINGERPRINT, &crc.to_be_bytes())
    }

    pub fn build(mut self) -> Vec<u8> {
        self.set_len(0);
        self.buf
    }
}

/// A ChannelData frame: channel 0x4000–0x4FFF, then the data, padded for UDP.
pub fn channel_data(channel: u16, data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + data.len() + 3);
    v.extend_from_slice(&channel.to_be_bytes());
    v.extend_from_slice(&(data.len() as u16).to_be_bytes());
    v.extend_from_slice(data);
    while !v.len().is_multiple_of(4) {
        v.push(0);
    }
    v
}

/// The channel and data of a ChannelData frame.
pub fn parse_channel_data(b: &[u8]) -> Option<(u16, &[u8])> {
    if b.len() < 4 || !(0x40..=0x4F).contains(&b[0]) {
        return None;
    }
    let ch = u16::from_be_bytes([b[0], b[1]]);
    let n = usize::from(u16::from_be_bytes([b[2], b[3]]));
    (b.len() >= 4 + n).then(|| (ch, &b[4..4 + n]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_round_trip() {
        assert_eq!(message_type(BINDING, Class::Request), 0x0001);
        assert_eq!(message_type(BINDING, Class::Success), 0x0101);
        assert_eq!(message_type(ALLOCATE, Class::Error), 0x0113);
        assert_eq!(message_type(DATA, Class::Indication), 0x0017);
        for m in [
            BINDING,
            ALLOCATE,
            REFRESH,
            SEND,
            DATA,
            CREATE_PERMISSION,
            CHANNEL_BIND,
        ] {
            for c in [
                Class::Request,
                Class::Indication,
                Class::Success,
                Class::Error,
            ] {
                assert_eq!(split_type(message_type(m, c)), (m, c));
            }
        }
    }

    #[test]
    fn rfc5769_integrity_and_fingerprint() {
        // RFC 5769 §2.4: request with long-term credentials.
        let hex = "000100602112a442\
            78ad3433c6ad72c029da412e\
            00060012e3839ee38388e383aae38383e382afe382b90000\
            0015001c662f2f3439396b39353464364f4c33346f4c394653547679363473 41\
            0014000b6578616d706c652e6f726700\
            00080014f67024656dd64a3e02b8e0712e85c9a28ca89666"
            .replace(' ', "");
        let raw = data_encoding::HEXLOWER.decode(hex.as_bytes()).unwrap();
        let m = Msg::parse(&raw).unwrap();
        assert_eq!(m.method, BINDING);
        assert_eq!(m.text(REALM), Some("example.org"));
        let key = crate::auth::long_term_key(m.text(USERNAME).unwrap(), "example.org", "TheMatrIX");
        assert!(m.verify(&key));
        assert!(!m.verify(b"wrong"));
    }

    #[test]
    fn built_messages_parse_and_verify() {
        let peer: SocketAddr = "[2001:db8::1]:3478".parse().unwrap();
        let raw = Builder::new(CREATE_PERMISSION, Class::Request, [7; 12])
            .addr(XOR_PEER_ADDRESS, peer)
            .addr(XOR_PEER_ADDRESS, "203.0.113.9:0".parse().unwrap())
            .attr(USERNAME, b"u")
            .integrity(b"key")
            .fingerprint()
            .build();
        let m = Msg::parse(&raw).unwrap();
        assert_eq!(m.addrs(XOR_PEER_ADDRESS).len(), 2);
        assert_eq!(m.addr(XOR_PEER_ADDRESS), Some(peer));
        assert!(m.verify(b"key"));
        assert!(m.unknown_required().is_empty());
    }

    #[test]
    fn garbage_never_panics() {
        for n in 0..64 {
            let mut b = vec![0u8; n];
            if n >= 8 {
                b[4..8].copy_from_slice(&MAGIC.to_be_bytes());
            }
            for i in 0..n {
                b[i] = b[i].wrapping_add((i as u8).wrapping_mul(37));
                let _ = Msg::parse(&b);
                let _ = parse_channel_data(&b);
            }
        }
    }

    #[test]
    fn channel_data_round_trips() {
        let f = channel_data(0x4001, b"abc");
        assert_eq!(f.len(), 8);
        assert_eq!(parse_channel_data(&f), Some((0x4001, &b"abc"[..])));
        assert_eq!(parse_channel_data(&[0x40, 0, 0, 9, 1]), None);
    }
}
