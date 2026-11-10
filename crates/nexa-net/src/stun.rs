//! Minimal STUN (RFC 8489) for candidate gathering and connectivity checks (§4.2, §4.4).
//!
//! Binding messages only, with XOR-MAPPED-ADDRESS, USERNAME and
//! MESSAGE-INTEGRITY-SHA256 under a short-term key. Everything is a pure
//! transformation of byte slices: transaction ids come from the caller, who
//! must draw them from a CSPRNG.
//!
//! Inputs come straight off the network, so [`StunMessage::parse`] returns an
//! error for anything malformed and never panics.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use hmac::{Hmac, Mac};
use sha2::Sha256;
use thiserror::Error;

pub const MAGIC_COOKIE: u32 = 0x2112_A442;
pub const HEADER_LEN: usize = 20;
/// RFC 8489 §14.3: fewer than 509 bytes when sending.
pub const MAX_USERNAME_SEND: usize = 508;
/// RFC 8489 §14.3: parse up to 763 bytes for RFC 5389 compatibility.
pub const MAX_USERNAME_PARSE: usize = 763;

const ATTR_USERNAME: u16 = 0x0006;
const ATTR_MESSAGE_INTEGRITY_SHA256: u16 = 0x001C;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
/// Attribute types below this must be understood by the receiver.
const COMPREHENSION_OPTIONAL_FROM: u16 = 0x8000;

const TYPE_BINDING_REQUEST: u16 = 0x0001;
const TYPE_BINDING_INDICATION: u16 = 0x0011;
const TYPE_BINDING_SUCCESS: u16 = 0x0101;
const TYPE_BINDING_ERROR: u16 = 0x0111;

const ATTR_HEADER_LEN: usize = 4;
const TAG_LEN: usize = 32;
const MIN_TRUNCATED_TAG_LEN: usize = 16;
const FAMILY_V4: u8 = 0x01;
const FAMILY_V6: u8 = 0x02;

#[derive(Debug, Error, PartialEq, Eq, Clone, Copy)]
pub enum StunError {
    #[error("buffer too short: need {need}, have {have}")]
    TooShort { need: usize, have: usize },
    #[error("not a STUN message")]
    NotStun,
    #[error("unsupported STUN message type {0:#06x}")]
    UnsupportedType(u16),
    #[error("message length does not match the datagram")]
    BadMessageLength,
    #[error("attribute overruns the message")]
    AttributeOverrun,
    #[error("malformed attribute: {0}")]
    BadAttribute(&'static str),
    #[error("username too long")]
    UsernameTooLong,
    #[error("message has no MESSAGE-INTEGRITY-SHA256")]
    MissingIntegrity,
    #[error("MESSAGE-INTEGRITY-SHA256 mismatch")]
    IntegrityMismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TransactionId(pub [u8; 12]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StunClass {
    Request,
    Indication,
    Success,
    Error,
}

/// A parsed Binding message borrowing from the datagram.
///
/// Fields are unauthenticated until [`verify_integrity`](Self::verify_integrity)
/// succeeds. Attributes after MESSAGE-INTEGRITY-SHA256 are not covered by the
/// tag, so they are never exposed here (RFC 8489 §14.6).
#[derive(Debug, Clone, Copy)]
pub struct StunMessage<'a> {
    pub class: StunClass,
    pub txid: TransactionId,
    pub username: Option<&'a str>,
    pub mapped: Option<SocketAddr>,
    /// First comprehension-required attribute this subset does not implement.
    /// RFC 8489 requires a response carrying one to be discarded.
    pub unknown_required: Option<u16>,
    raw: &'a [u8],
    /// Offset of the integrity attribute header, and its value length.
    integrity: Option<(usize, usize)>,
}

impl<'a> StunMessage<'a> {
    pub fn parse(buf: &'a [u8]) -> Result<Self, StunError> {
        if buf.len() < HEADER_LEN {
            return Err(StunError::TooShort {
                need: HEADER_LEN,
                have: buf.len(),
            });
        }
        let ty = be16(buf, 0);
        if ty & 0xC000 != 0 || be32(buf, 4) != MAGIC_COOKIE {
            return Err(StunError::NotStun);
        }
        let class = match ty {
            TYPE_BINDING_REQUEST => StunClass::Request,
            TYPE_BINDING_INDICATION => StunClass::Indication,
            TYPE_BINDING_SUCCESS => StunClass::Success,
            TYPE_BINDING_ERROR => StunClass::Error,
            other => return Err(StunError::UnsupportedType(other)),
        };
        let body_len = usize::from(be16(buf, 2));
        if !body_len.is_multiple_of(4) || HEADER_LEN + body_len != buf.len() {
            return Err(StunError::BadMessageLength);
        }
        let mut txid = [0u8; 12];
        txid.copy_from_slice(&buf[8..HEADER_LEN]);

        let mut msg = StunMessage {
            class,
            txid: TransactionId(txid),
            username: None,
            mapped: None,
            unknown_required: None,
            raw: buf,
            integrity: None,
        };
        let mut pos = HEADER_LEN;
        while pos < buf.len() {
            if buf.len() - pos < ATTR_HEADER_LEN {
                return Err(StunError::AttributeOverrun);
            }
            let attr = be16(buf, pos);
            let len = usize::from(be16(buf, pos + 2));
            let start = pos + ATTR_HEADER_LEN;
            let padded = len.next_multiple_of(4);
            if buf.len() - start < padded {
                return Err(StunError::AttributeOverrun);
            }
            if msg.integrity.is_none() {
                msg.apply_attribute(attr, &buf[start..start + len], pos)?;
            }
            pos = start + padded;
        }
        Ok(msg)
    }

    /// Only the first occurrence of an attribute type counts (RFC 8489 §14).
    fn apply_attribute(
        &mut self,
        attr: u16,
        value: &'a [u8],
        attr_offset: usize,
    ) -> Result<(), StunError> {
        match attr {
            ATTR_USERNAME if self.username.is_none() => {
                if value.len() > MAX_USERNAME_PARSE {
                    return Err(StunError::BadAttribute("username length"));
                }
                self.username = Some(
                    std::str::from_utf8(value)
                        .map_err(|_| StunError::BadAttribute("username encoding"))?,
                );
            }
            ATTR_XOR_MAPPED_ADDRESS if self.mapped.is_none() => {
                self.mapped = Some(decode_xor_address(value, &self.txid)?);
            }
            ATTR_MESSAGE_INTEGRITY_SHA256 => {
                if !(MIN_TRUNCATED_TAG_LEN..=TAG_LEN).contains(&value.len())
                    || !value.len().is_multiple_of(4)
                {
                    return Err(StunError::BadAttribute("integrity length"));
                }
                self.integrity = Some((attr_offset, value.len()));
            }
            ATTR_USERNAME | ATTR_XOR_MAPPED_ADDRESS => {}
            other if other < COMPREHENSION_OPTIONAL_FROM && self.unknown_required.is_none() => {
                self.unknown_required = Some(other);
            }
            _ => {}
        }
        Ok(())
    }

    /// Checks the tag in constant time. `key` is the short-term key, i.e. the
    /// ICE `pwd` bytes (RFC 8489 §9.1.1; our passwords are generated ASCII, so
    /// OpaqueString processing is the identity).
    ///
    /// Truncated tags are rejected: RFC 8489 §14.6 allows them only when a
    /// usage opts in, and ours does not.
    pub fn verify_integrity(&self, key: &[u8]) -> Result<(), StunError> {
        let (offset, len) = self.integrity.ok_or(StunError::MissingIntegrity)?;
        if len != TAG_LEN {
            return Err(StunError::IntegrityMismatch);
        }
        let tag = &self.raw[offset + ATTR_HEADER_LEN..offset + ATTR_HEADER_LEN + TAG_LEN];
        let mac = integrity_mac(key, &self.raw[..offset])?;
        mac.verify_slice(tag)
            .map_err(|_| StunError::IntegrityMismatch)
    }
}

/// The MAC runs over everything before the attribute, with the header length
/// rewritten to end just after the attribute (RFC 8489 §14.6).
fn integrity_mac(key: &[u8], before_attr: &[u8]) -> Result<Hmac<Sha256>, StunError> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|_| StunError::IntegrityMismatch)?;
    let covered = (before_attr.len() - HEADER_LEN + ATTR_HEADER_LEN + TAG_LEN) as u16;
    mac.update(&before_attr[..2]);
    mac.update(&covered.to_be_bytes());
    mac.update(&before_attr[4..]);
    Ok(mac)
}

fn be16(b: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([b[o], b[o + 1]])
}

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn v6_mask(txid: &TransactionId) -> [u8; 16] {
    let mut mask = [0u8; 16];
    mask[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    mask[4..].copy_from_slice(&txid.0);
    mask
}

const COOKIE_HIGH: u16 = (MAGIC_COOKIE >> 16) as u16;

fn decode_xor_address(value: &[u8], txid: &TransactionId) -> Result<SocketAddr, StunError> {
    if value.len() < 4 {
        return Err(StunError::BadAttribute("address length"));
    }
    let port = be16(value, 2) ^ COOKIE_HIGH;
    let ip = match (value[1], value.len()) {
        (FAMILY_V4, 8) => IpAddr::V4(Ipv4Addr::from(be32(value, 4) ^ MAGIC_COOKIE)),
        (FAMILY_V6, 20) => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&value[4..]);
            for (o, m) in octets.iter_mut().zip(v6_mask(txid)) {
                *o ^= m;
            }
            IpAddr::V6(Ipv6Addr::from(octets))
        }
        (FAMILY_V4 | FAMILY_V6, _) => return Err(StunError::BadAttribute("address length")),
        _ => return Err(StunError::BadAttribute("address family")),
    };
    Ok(SocketAddr::new(ip, port))
}

fn start_message(ty: u16, txid: &TransactionId) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    out.extend_from_slice(&ty.to_be_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    out.extend_from_slice(&txid.0);
    out
}

fn push_attribute(out: &mut Vec<u8>, attr: u16, value: &[u8]) {
    out.extend_from_slice(&attr.to_be_bytes());
    out.extend_from_slice(&(value.len() as u16).to_be_bytes());
    out.extend_from_slice(value);
    out.resize(out.len().next_multiple_of(4), 0);
}

fn finish_message(mut out: Vec<u8>, key: Option<&[u8]>) -> Result<Vec<u8>, StunError> {
    if let Some(key) = key {
        let tag = integrity_mac(key, &out)?.finalize().into_bytes();
        push_attribute(&mut out, ATTR_MESSAGE_INTEGRITY_SHA256, &tag);
    }
    let body_len = (out.len() - HEADER_LEN) as u16;
    out[2..4].copy_from_slice(&body_len.to_be_bytes());
    Ok(out)
}

/// Encodes a Binding request. Server-reflexive gathering passes neither
/// `username` nor `key`; connectivity checks pass both (§4.4).
pub fn encode_binding_request(
    txid: &TransactionId,
    username: Option<&str>,
    key: Option<&[u8]>,
) -> Result<Vec<u8>, StunError> {
    let mut out = start_message(TYPE_BINDING_REQUEST, txid);
    if let Some(name) = username {
        if name.len() > MAX_USERNAME_SEND {
            return Err(StunError::UsernameTooLong);
        }
        push_attribute(&mut out, ATTR_USERNAME, name.as_bytes());
    }
    finish_message(out, key)
}

/// Encodes a Binding success response carrying the peer's reflexive address.
pub fn encode_binding_success(
    txid: &TransactionId,
    mapped: SocketAddr,
    key: Option<&[u8]>,
) -> Result<Vec<u8>, StunError> {
    let mut out = start_message(TYPE_BINDING_SUCCESS, txid);
    let port = (mapped.port() ^ COOKIE_HIGH).to_be_bytes();
    let mut value = vec![0, 0, port[0], port[1]];
    match mapped.ip() {
        IpAddr::V4(ip) => {
            value[1] = FAMILY_V4;
            value.extend_from_slice(&(u32::from(ip) ^ MAGIC_COOKIE).to_be_bytes());
        }
        IpAddr::V6(ip) => {
            value[1] = FAMILY_V6;
            value.extend(ip.octets().iter().zip(v6_mask(txid)).map(|(o, m)| o ^ m));
        }
    }
    push_attribute(&mut out, ATTR_XOR_MAPPED_ADDRESS, &value);
    finish_message(out, key)
}

/// The USERNAME of a connectivity check (§4.4).
pub fn check_username(remote_ufrag: &str, local_ufrag: &str) -> String {
    format!("{remote_ufrag}:{local_ufrag}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NatClass {
    /// Both servers saw the same mapping: endpoint-independent.
    Easy,
    /// The mapping depends on the destination. `port_delta` is `b.port - a.port`,
    /// used for incremental port prediction.
    Hard { port_delta: i32 },
}

/// Classifies the NAT from the mappings two different STUN servers reported (§4.2).
///
/// §4.2 compares only ports; a differing address also counts as `Hard`, since
/// a mapping that changes its public IP per destination is not
/// endpoint-independent. Both inputs should come from the same address family.
pub fn classify_nat(mapped_a: SocketAddr, mapped_b: SocketAddr) -> NatClass {
    if mapped_a == mapped_b {
        NatClass::Easy
    } else {
        NatClass::Hard {
            port_delta: i32::from(mapped_b.port()) - i32::from(mapped_a.port()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RFC_PASSWORD: &[u8] = b"VOkJxbRl1RmTxUk/WvJxBt";
    const RFC_TXID: TransactionId = TransactionId([
        0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae,
    ]);

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn rfc5769_request() -> Vec<u8> {
        hex("00010058 2112a442 b7e7a701 bc34d686 fa87dfae
             80220010 5354554e 20746573 7420636c 69656e74
             00240004 6e0001ff
             80290008 932ff9b1 51263b36
             00060009 6576746a 3a683676 59202020
             00080014 9aeaa70c bfd8cb56 781ef2b5 b2d3f249 c1b571a2
             80280004 e57a3bcf")
    }

    fn rfc5769_v4_response() -> Vec<u8> {
        hex("0101003c 2112a442 b7e7a701 bc34d686 fa87dfae
             8022000b 74657374 20766563 746f7220
             00200008 0001a147 e112a643
             00080014 2b91f599 fd9e90c3 8c7489f9 2af9ba53 f06be7d7
             80280004 c07d4c96")
    }

    fn rfc5769_v6_response() -> Vec<u8> {
        hex("01010048 2112a442 b7e7a701 bc34d686 fa87dfae
             8022000b 74657374 20766563 746f7220
             00200014 0002a147 0113a9fa a5d3f179 bc25f4b5 bed2b9d9
             00080014 a382954e 4be67bf1 1784c97c 8292c275 bfe3ed41
             80280004 c8fb0b4c")
    }

    #[test]
    fn parses_rfc5769_request() {
        let raw = rfc5769_request();
        let m = StunMessage::parse(&raw).unwrap();
        assert_eq!(m.class, StunClass::Request);
        assert_eq!(m.txid, RFC_TXID);
        // Non-zero padding bytes in the vector must be ignored.
        assert_eq!(m.username, Some("evtj:h6vY"));
        // PRIORITY is comprehension-required and outside this subset.
        assert_eq!(m.unknown_required, Some(0x0024));
        // The vector only has the SHA-1 attribute.
        assert_eq!(
            m.verify_integrity(RFC_PASSWORD),
            Err(StunError::MissingIntegrity)
        );
    }

    #[test]
    fn parses_rfc5769_responses() {
        let raw = rfc5769_v4_response();
        let m = StunMessage::parse(&raw).unwrap();
        assert_eq!(m.class, StunClass::Success);
        assert_eq!(m.mapped, Some("192.0.2.1:32853".parse().unwrap()));

        let raw = rfc5769_v6_response();
        let m = StunMessage::parse(&raw).unwrap();
        assert_eq!(
            m.mapped,
            Some(
                "[2001:db8:1234:5678:11:2233:4455:6677]:32853"
                    .parse()
                    .unwrap()
            )
        );
    }

    /// Bytes produced independently with Python's `hmac` over the RFC 8489 §14.6 rules.
    #[test]
    fn matches_independent_integrity_vector() {
        let expected = hex("00010034 2112a442 b7e7a701 bc34d686 fa87dfae
             00060009 6576746a 3a683676 59000000
             001c0020 9a241fa2 2a3d77e4 81fb2ad2 9fa9ce63 403cd45c 00799672 3331e370 538de124");
        let encoded =
            encode_binding_request(&RFC_TXID, Some("evtj:h6vY"), Some(RFC_PASSWORD)).unwrap();
        assert_eq!(encoded, expected);
        StunMessage::parse(&expected)
            .unwrap()
            .verify_integrity(RFC_PASSWORD)
            .unwrap();
    }

    /// The header length covers a trailing FINGERPRINT, but the HMAC must not.
    #[test]
    fn verifies_tag_followed_by_fingerprint() {
        let raw = hex("01010038 2112a442 b7e7a701 bc34d686 fa87dfae
             00200008 0001a147 e112a643
             001c0020 c1893a5d a57c1a33 bb3ab658 7d1ef4b1 8daf3a17 7fb7c761 2df103d4 574e37ad
             80280004 c07d4c96");
        let m = StunMessage::parse(&raw).unwrap();
        m.verify_integrity(RFC_PASSWORD).unwrap();
        assert_eq!(m.mapped, Some("192.0.2.1:32853".parse().unwrap()));
    }

    #[test]
    fn request_round_trip() {
        let txid = TransactionId([7; 12]);
        let name = check_username("remote", "local");
        let raw = encode_binding_request(&txid, Some(&name), Some(b"pwd")).unwrap();
        let m = StunMessage::parse(&raw).unwrap();
        assert_eq!(m.class, StunClass::Request);
        assert_eq!(m.txid, txid);
        assert_eq!(m.username, Some("remote:local"));
        assert_eq!(m.unknown_required, None);
        m.verify_integrity(b"pwd").unwrap();
        assert_eq!(
            m.verify_integrity(b"other"),
            Err(StunError::IntegrityMismatch)
        );
    }

    #[test]
    fn plain_gathering_request_is_header_only() {
        let raw = encode_binding_request(&RFC_TXID, None, None).unwrap();
        assert_eq!(raw.len(), HEADER_LEN);
        assert_eq!(
            StunMessage::parse(&raw).unwrap().verify_integrity(b"x"),
            Err(StunError::MissingIntegrity)
        );
    }

    #[test]
    fn success_round_trip_v4_and_v6() {
        let txid = TransactionId([0xA5; 12]);
        for addr in [
            "203.0.113.7:65535",
            "[2001:db8::1]:1",
            "[::ffff:1.2.3.4]:443",
            "0.0.0.0:0",
        ] {
            let addr: SocketAddr = addr.parse().unwrap();
            let raw = encode_binding_success(&txid, addr, Some(b"k")).unwrap();
            let m = StunMessage::parse(&raw).unwrap();
            assert_eq!(m.class, StunClass::Success);
            assert_eq!(m.mapped, Some(addr));
            m.verify_integrity(b"k").unwrap();
        }
    }

    #[test]
    fn flipping_any_byte_breaks_the_message() {
        let raw =
            encode_binding_success(&RFC_TXID, "203.0.113.7:4000".parse().unwrap(), Some(b"k"))
                .unwrap();
        for i in 0..raw.len() {
            for bit in 0..8 {
                let mut bad = raw.clone();
                bad[i] ^= 1 << bit;
                let verified = StunMessage::parse(&bad).and_then(|m| m.verify_integrity(b"k"));
                assert!(verified.is_err(), "byte {i} bit {bit}");
            }
        }
    }

    /// An attacker can append attributes after the tag without invalidating it.
    #[test]
    fn attributes_after_the_tag_are_not_exposed() {
        let good: SocketAddr = "203.0.113.7:4000".parse().unwrap();
        let evil: SocketAddr = "198.51.100.66:666".parse().unwrap();
        let mut raw = encode_binding_request(&RFC_TXID, Some("real"), Some(b"k")).unwrap();
        let forged = encode_binding_success(&RFC_TXID, evil, None).unwrap();
        raw.extend_from_slice(&forged[HEADER_LEN..]);
        raw.extend_from_slice(&[0x00, 0x06, 0x00, 0x04]);
        raw.extend_from_slice(b"fake");
        let len = (raw.len() - HEADER_LEN) as u16;
        raw[2..4].copy_from_slice(&len.to_be_bytes());

        let m = StunMessage::parse(&raw).unwrap();
        m.verify_integrity(b"k").unwrap();
        assert_eq!(m.mapped, None);
        assert_eq!(m.username, Some("real"));

        let resp = encode_binding_success(&RFC_TXID, good, Some(b"k")).unwrap();
        assert_eq!(StunMessage::parse(&resp).unwrap().mapped, Some(good));
    }

    #[test]
    fn first_duplicate_wins() {
        let mut raw = encode_binding_request(&RFC_TXID, Some("first"), None).unwrap();
        raw.extend_from_slice(&[0x00, 0x06, 0x00, 0x04]);
        raw.extend_from_slice(b"next");
        let len = (raw.len() - HEADER_LEN) as u16;
        raw[2..4].copy_from_slice(&len.to_be_bytes());
        assert_eq!(StunMessage::parse(&raw).unwrap().username, Some("first"));
    }

    #[test]
    fn truncated_tag_is_rejected() {
        let mut raw = start_message(TYPE_BINDING_REQUEST, &RFC_TXID);
        push_attribute(&mut raw, ATTR_MESSAGE_INTEGRITY_SHA256, &[0u8; 16]);
        let len = (raw.len() - HEADER_LEN) as u16;
        raw[2..4].copy_from_slice(&len.to_be_bytes());
        let m = StunMessage::parse(&raw).unwrap();
        assert_eq!(m.verify_integrity(b"k"), Err(StunError::IntegrityMismatch));
    }

    fn with_body(body: &[u8]) -> Vec<u8> {
        let mut raw = start_message(TYPE_BINDING_SUCCESS, &RFC_TXID);
        raw.extend_from_slice(body);
        let len = (raw.len() - HEADER_LEN) as u16;
        raw[2..4].copy_from_slice(&len.to_be_bytes());
        raw
    }

    #[test]
    fn rejects_malformed_messages() {
        assert!(matches!(
            StunMessage::parse(&[0; 19]),
            Err(StunError::TooShort { .. })
        ));

        let good = encode_binding_request(&RFC_TXID, None, None).unwrap();
        let mut bad = good.clone();
        bad[0] = 0x40; // A QUIC long-header-ish first byte.
        assert_eq!(StunMessage::parse(&bad).unwrap_err(), StunError::NotStun);
        let mut bad = good.clone();
        bad[4] ^= 1;
        assert_eq!(StunMessage::parse(&bad).unwrap_err(), StunError::NotStun);
        let mut bad = good.clone();
        bad[1] = 0x02; // Method other than Binding.
        assert_eq!(
            StunMessage::parse(&bad).unwrap_err(),
            StunError::UnsupportedType(0x0002)
        );
        let mut bad = good.clone();
        bad.push(0);
        assert_eq!(
            StunMessage::parse(&bad).unwrap_err(),
            StunError::BadMessageLength
        );
        let mut bad = good.clone();
        bad[3] = 4; // Claims a body that is not there.
        assert_eq!(
            StunMessage::parse(&bad).unwrap_err(),
            StunError::BadMessageLength
        );

        let cases: &[(&str, &[u8])] = &[
            ("truncated attribute header", &[0x00, 0x20, 0x00, 0x08]),
            (
                "value overruns",
                &[0x00, 0x06, 0x00, 0xFF, b'a', b'b', b'c', b'd'],
            ),
            (
                "padding overruns",
                &[0x00, 0x06, 0x00, 0x05, b'a', b'b', b'c', b'd'],
            ),
            (
                "bad family",
                &[0x00, 0x20, 0x00, 0x08, 0, 3, 0, 0, 0, 0, 0, 0],
            ),
            ("v4 with v6 length", &[0x00, 0x20, 0x00, 0x04, 0, 1, 0, 0]),
            ("short address", &[0x00, 0x20, 0x00, 0x00]),
            (
                "non-UTF-8 username",
                &[0x00, 0x06, 0x00, 0x04, 0xFF, 0xFE, 0xFD, 0xFC],
            ),
            ("integrity too short", &[0x00, 0x1C, 0x00, 0x04, 1, 2, 3, 4]),
        ];
        for (name, body) in cases {
            assert!(StunMessage::parse(&with_body(body)).is_err(), "{name}");
        }
    }

    #[test]
    fn refuses_to_send_oversized_username() {
        let name = "x".repeat(MAX_USERNAME_SEND + 1);
        assert_eq!(
            encode_binding_request(&RFC_TXID, Some(&name), None).unwrap_err(),
            StunError::UsernameTooLong
        );
        assert!(encode_binding_request(&RFC_TXID, Some(&name[1..]), None).is_ok());
    }

    /// Parsers face the network: arbitrary bytes must never panic.
    #[test]
    fn parser_survives_garbage() {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let seed =
            encode_binding_success(&RFC_TXID, "[2001:db8::1]:1".parse().unwrap(), Some(b"k"))
                .unwrap();
        for round in 0..200_000 {
            let mut b: Vec<u8> = if round % 2 == 0 {
                let len = (next() % 128) as usize;
                let mut b: Vec<u8> = (0..len).map(|_| next() as u8).collect();
                if b.len() >= 8 {
                    // Get past the header checks so the attribute walk is exercised.
                    b[0] = 0x01;
                    b[1] = 0x01;
                    let body = b.len().saturating_sub(HEADER_LEN) as u16;
                    b[2..4].copy_from_slice(&body.to_be_bytes());
                    b[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
                }
                b
            } else {
                seed.clone()
            };
            if round % 2 == 1 {
                for _ in 0..(next() % 6) {
                    let i = (next() as usize) % b.len();
                    b[i] = next() as u8;
                }
                b.truncate((next() as usize) % (b.len() + 1));
            }
            if let Ok(m) = StunMessage::parse(&b) {
                let _ = m.verify_integrity(b"k");
            }
        }
    }

    #[test]
    fn classifies_nat() {
        let a: SocketAddr = "203.0.113.7:40000".parse().unwrap();
        assert_eq!(classify_nat(a, a), NatClass::Easy);
        assert_eq!(
            classify_nat(a, "203.0.113.7:40002".parse().unwrap()),
            NatClass::Hard { port_delta: 2 }
        );
        assert_eq!(
            classify_nat(a, "203.0.113.7:39990".parse().unwrap()),
            NatClass::Hard { port_delta: -10 }
        );
        assert_eq!(
            classify_nat(a, "203.0.113.8:40000".parse().unwrap()),
            NatClass::Hard { port_delta: 0 }
        );
    }
}
