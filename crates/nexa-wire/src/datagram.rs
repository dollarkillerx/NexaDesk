//! QUIC DATAGRAM layouts (§3.1–3.3). All multi-byte fields are little-endian.

use crate::{Result, WireError};

pub const TYPE_VIDEO: u8 = 0x01;
pub const TYPE_AUDIO: u8 = 0x02;
pub const TYPE_POINTER: u8 = 0x03;
pub const TYPE_FEEDBACK: u8 = 0x04;
pub const TYPE_VIDEO_REPORT: u8 = 0x05;
pub const TYPE_NACK: u8 = 0x06;
pub const TYPE_CLOCK: u8 = 0x07;

/// reed-solomon-simd's limit for arbitrary original/recovery combinations.
pub const MAX_SHARDS: u16 = 32768;

fn need(buf: &[u8], n: usize) -> Result<()> {
    if buf.len() < n {
        return Err(WireError::TooShort {
            need: n,
            have: buf.len(),
        });
    }
    Ok(())
}

fn expect_type(buf: &[u8], t: u8) -> Result<()> {
    need(buf, 1)?;
    if buf[0] != t {
        return Err(WireError::BadType(buf[0]));
    }
    Ok(())
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn i32_at(b: &[u8], o: usize) -> i32 {
    u32_at(b, o) as i32
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

pub mod flags {
    pub const IDR: u8 = 1 << 0;
    pub const RECOVERY_POINT: u8 = 1 << 1;
}

/// §3.2 VideoShard header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoShardHeader {
    pub stream: u8,
    pub flags: u8,
    pub ref_epoch: u8,
    pub tx_seq: u32,
    pub frame_id: u32,
    pub shard_index: u16,
    pub k: u16,
    pub m: u16,
    pub shard_len: u16,
    pub frame_len: u32,
    pub capture_ts_us: u32,
}

impl VideoShardHeader {
    pub const LEN: usize = 28;

    pub fn write(&self, out: &mut [u8; Self::LEN]) {
        out[0] = TYPE_VIDEO;
        out[1] = self.stream;
        out[2] = self.flags;
        out[3] = self.ref_epoch;
        out[4..8].copy_from_slice(&self.tx_seq.to_le_bytes());
        out[8..12].copy_from_slice(&self.frame_id.to_le_bytes());
        out[12..14].copy_from_slice(&self.shard_index.to_le_bytes());
        out[14..16].copy_from_slice(&self.k.to_le_bytes());
        out[16..18].copy_from_slice(&self.m.to_le_bytes());
        out[18..20].copy_from_slice(&self.shard_len.to_le_bytes());
        out[20..24].copy_from_slice(&self.frame_len.to_le_bytes());
        out[24..28].copy_from_slice(&self.capture_ts_us.to_le_bytes());
    }

    /// Parses and validates a whole datagram, returning the header and payload.
    pub fn parse(buf: &[u8]) -> Result<(Self, &[u8])> {
        expect_type(buf, TYPE_VIDEO)?;
        need(buf, Self::LEN)?;
        let h = Self {
            stream: buf[1],
            flags: buf[2],
            ref_epoch: buf[3],
            tx_seq: u32_at(buf, 4),
            frame_id: u32_at(buf, 8),
            shard_index: u16_at(buf, 12),
            k: u16_at(buf, 14),
            m: u16_at(buf, 16),
            shard_len: u16_at(buf, 18),
            frame_len: u32_at(buf, 20),
            capture_ts_us: u32_at(buf, 24),
        };
        if h.k == 0 || h.k > MAX_SHARDS || h.m > MAX_SHARDS {
            return Err(WireError::Invalid("shard counts"));
        }
        if u32::from(h.shard_index) >= u32::from(h.k) + u32::from(h.m) {
            return Err(WireError::Invalid("shard_index"));
        }
        if h.shard_len == 0 || !h.shard_len.is_multiple_of(2) {
            return Err(WireError::Invalid("shard_len"));
        }
        let capacity = u64::from(h.k) * u64::from(h.shard_len);
        let floor = u64::from(h.k - 1) * u64::from(h.shard_len);
        if u64::from(h.frame_len) > capacity || u64::from(h.frame_len) <= floor {
            return Err(WireError::Invalid("frame_len"));
        }
        let payload = &buf[Self::LEN..];
        if payload.len() != usize::from(h.shard_len) {
            return Err(WireError::Invalid("payload length"));
        }
        Ok((h, payload))
    }
}

pub mod audio_flags {
    /// First packet after the host (re)started capture: the receiver resets its
    /// jitter buffer and decoder instead of concealing a gap.
    pub const RESTART: u8 = 1 << 0;
}

/// AudioPacket (§3.1 type 0x02). §3.1 reserves the type; the layout is defined here:
///
/// ```text
///  0 u8  type          0x02
///  1 u8  stream        audio stream, 0 for system audio
///  2 u8  flags         bit0 RESTART (see `audio_flags`)
///  3 u8  frame_10ms    duration of the payload in 10 ms units (always 1 today)
///  4 u32 tx_seq        the transport sequence shared with VideoShard (§3.2), so the
///                      viewer's TransportFeedback covers audio and video alike
///  8 u32 seq           audio packet number, +1 per packet; gaps are losses
/// 12 u32 capture_ts_us host monotonic clock, low 32 bits, same clock as VideoShard
/// 16 u8[] payload      exactly one Opus packet (RFC 6716), 48 kHz stereo, 1..=1275 bytes
/// ```
///
/// One Opus packet per datagram keeps a loss to one 10 ms frame that the decoder can
/// conceal; there is no FEC or retransmission for audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioHeader {
    pub stream: u8,
    pub flags: u8,
    pub frame_10ms: u8,
    pub tx_seq: u32,
    pub seq: u32,
    pub capture_ts_us: u32,
}

impl AudioHeader {
    pub const LEN: usize = 16;
    /// RFC 6716 §3.4: an Opus packet is at most 1275 bytes.
    pub const MAX_PAYLOAD: usize = 1275;

    pub fn write(&self, out: &mut [u8; Self::LEN]) {
        out[0] = TYPE_AUDIO;
        out[1] = self.stream;
        out[2] = self.flags;
        out[3] = self.frame_10ms;
        out[4..8].copy_from_slice(&self.tx_seq.to_le_bytes());
        out[8..12].copy_from_slice(&self.seq.to_le_bytes());
        out[12..16].copy_from_slice(&self.capture_ts_us.to_le_bytes());
    }

    /// The whole datagram: header followed by `payload`.
    pub fn encode(&self, payload: &[u8], out: &mut Vec<u8>) {
        let mut h = [0u8; Self::LEN];
        self.write(&mut h);
        out.extend_from_slice(&h);
        out.extend_from_slice(payload);
    }

    /// Parses and validates a whole datagram, returning the header and the Opus packet.
    pub fn parse(buf: &[u8]) -> Result<(Self, &[u8])> {
        expect_type(buf, TYPE_AUDIO)?;
        need(buf, Self::LEN + 1)?;
        let h = Self {
            stream: buf[1],
            flags: buf[2],
            frame_10ms: buf[3],
            tx_seq: u32_at(buf, 4),
            seq: u32_at(buf, 8),
            capture_ts_us: u32_at(buf, 12),
        };
        if h.frame_10ms == 0 || h.frame_10ms > 12 {
            return Err(WireError::Invalid("audio frame duration"));
        }
        let payload = &buf[Self::LEN..];
        if payload.len() > Self::MAX_PAYLOAD {
            return Err(WireError::Invalid("audio payload length"));
        }
        Ok((h, payload))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketStatus {
    NotReceived,
    /// Gap since the previous received packet, in 250 µs ticks.
    Received {
        delta_ticks: u16,
    },
}

/// §3.3 TransportFeedback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportFeedback {
    pub fb_seq: u16,
    pub base_tx_seq: u32,
    pub base_arrival_us: u32,
    pub statuses: Vec<PacketStatus>,
}

impl TransportFeedback {
    const FIXED: usize = 14;

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.push(TYPE_FEEDBACK);
        out.push(0);
        out.extend_from_slice(&self.fb_seq.to_le_bytes());
        out.extend_from_slice(&self.base_tx_seq.to_le_bytes());
        out.extend_from_slice(&self.base_arrival_us.to_le_bytes());
        out.extend_from_slice(&(self.statuses.len() as u16).to_le_bytes());
        for s in &self.statuses {
            match *s {
                PacketStatus::NotReceived => out.push(0),
                PacketStatus::Received { delta_ticks } if delta_ticks <= 0xFD => {
                    out.push(delta_ticks as u8 + 1)
                }
                PacketStatus::Received { delta_ticks } => {
                    out.push(0xFF);
                    out.extend_from_slice(&delta_ticks.to_le_bytes());
                }
            }
        }
    }

    pub fn parse(buf: &[u8]) -> Result<Self> {
        expect_type(buf, TYPE_FEEDBACK)?;
        need(buf, Self::FIXED)?;
        let count = usize::from(u16_at(buf, 12));
        let mut statuses = Vec::with_capacity(count.min(buf.len()));
        let mut o = Self::FIXED;
        for _ in 0..count {
            need(buf, o + 1)?;
            let v = buf[o];
            o += 1;
            statuses.push(match v {
                0 => PacketStatus::NotReceived,
                0xFF => {
                    need(buf, o + 2)?;
                    let d = u16_at(buf, o);
                    o += 2;
                    PacketStatus::Received { delta_ticks: d }
                }
                v => PacketStatus::Received {
                    delta_ticks: u16::from(v) - 1,
                },
            });
        }
        if o != buf.len() {
            return Err(WireError::Invalid("trailing feedback bytes"));
        }
        Ok(Self {
            fb_seq: u16_at(buf, 2),
            base_tx_seq: u32_at(buf, 4),
            base_arrival_us: u32_at(buf, 8),
            statuses,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ReportKind {
    Loss = 1,
    NeedIdr = 2,
    FrameAck = 3,
    DecoderSlow = 4,
}

/// §3.3 VideoReport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoReport {
    pub stream: u8,
    pub client_epoch: u8,
    pub kind: ReportKind,
    pub last_good_frame: u32,
    pub first_lost_frame: u32,
    pub latest_seen_frame: u32,
}

impl VideoReport {
    pub const LEN: usize = 16;

    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut o = [0u8; Self::LEN];
        o[0] = TYPE_VIDEO_REPORT;
        o[1] = self.stream;
        o[2] = self.client_epoch;
        o[3] = self.kind as u8;
        o[4..8].copy_from_slice(&self.last_good_frame.to_le_bytes());
        o[8..12].copy_from_slice(&self.first_lost_frame.to_le_bytes());
        o[12..16].copy_from_slice(&self.latest_seen_frame.to_le_bytes());
        o
    }

    pub fn parse(buf: &[u8]) -> Result<Self> {
        expect_type(buf, TYPE_VIDEO_REPORT)?;
        if buf.len() != Self::LEN {
            return Err(WireError::Invalid("report length"));
        }
        let kind = match buf[3] {
            1 => ReportKind::Loss,
            2 => ReportKind::NeedIdr,
            3 => ReportKind::FrameAck,
            4 => ReportKind::DecoderSlow,
            _ => return Err(WireError::Invalid("report kind")),
        };
        Ok(Self {
            stream: buf[1],
            client_epoch: buf[2],
            kind,
            last_good_frame: u32_at(buf, 4),
            first_lost_frame: u32_at(buf, 8),
            latest_seen_frame: u32_at(buf, 12),
        })
    }
}

/// §3.3 Pointer. `x`/`y` are absolute pixels, or wrapping running totals when `relative`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pointer {
    pub relative: bool,
    pub seq: u16,
    pub x: i32,
    pub y: i32,
    pub wheel_h_total: i32,
    pub wheel_v_total: i32,
    pub display: u8,
}

impl Pointer {
    pub const LEN: usize = 24;

    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut o = [0u8; Self::LEN];
        o[0] = TYPE_POINTER;
        o[1] = u8::from(self.relative);
        o[2..4].copy_from_slice(&self.seq.to_le_bytes());
        o[4..8].copy_from_slice(&self.x.to_le_bytes());
        o[8..12].copy_from_slice(&self.y.to_le_bytes());
        o[12..16].copy_from_slice(&self.wheel_h_total.to_le_bytes());
        o[16..20].copy_from_slice(&self.wheel_v_total.to_le_bytes());
        o[20] = self.display;
        o
    }

    pub fn parse(buf: &[u8]) -> Result<Self> {
        expect_type(buf, TYPE_POINTER)?;
        if buf.len() != Self::LEN {
            return Err(WireError::Invalid("pointer length"));
        }
        Ok(Self {
            relative: buf[1] & 1 != 0,
            seq: u16_at(buf, 2),
            x: i32_at(buf, 4),
            y: i32_at(buf, 8),
            wheel_h_total: i32_at(buf, 12),
            wheel_v_total: i32_at(buf, 16),
            display: buf[20],
        })
    }
}

/// Selective retransmit request. §3.1 reserves type 0x06; layout defined here:
/// `[type][stream][count u16][frame_id u32][shard_index u16 × count]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nack {
    pub stream: u8,
    pub frame_id: u32,
    pub shards: Vec<u16>,
}

impl Nack {
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.push(TYPE_NACK);
        out.push(self.stream);
        out.extend_from_slice(&(self.shards.len() as u16).to_le_bytes());
        out.extend_from_slice(&self.frame_id.to_le_bytes());
        for s in &self.shards {
            out.extend_from_slice(&s.to_le_bytes());
        }
    }

    pub fn parse(buf: &[u8]) -> Result<Self> {
        expect_type(buf, TYPE_NACK)?;
        need(buf, 8)?;
        let count = usize::from(u16_at(buf, 2));
        if buf.len() != 8 + count * 2 {
            return Err(WireError::Invalid("nack length"));
        }
        Ok(Self {
            stream: buf[1],
            frame_id: u32_at(buf, 4),
            shards: (0..count).map(|i| u16_at(buf, 8 + i * 2)).collect(),
        })
    }
}

/// §3.3 clock probe: a ping carries `t1`; the pong echoes it and adds `t2`, `t3`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clock {
    Ping { t1: u64 },
    Pong { t1: u64, t2: u64, t3: u64 },
}

impl Clock {
    pub const LEN: usize = 32;

    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut o = [0u8; Self::LEN];
        o[0] = TYPE_CLOCK;
        let (kind, t1, t2, t3) = match *self {
            Clock::Ping { t1 } => (0, t1, 0, 0),
            Clock::Pong { t1, t2, t3 } => (1, t1, t2, t3),
        };
        o[1] = kind;
        o[8..16].copy_from_slice(&t1.to_le_bytes());
        o[16..24].copy_from_slice(&t2.to_le_bytes());
        o[24..32].copy_from_slice(&t3.to_le_bytes());
        o
    }

    pub fn parse(buf: &[u8]) -> Result<Self> {
        expect_type(buf, TYPE_CLOCK)?;
        if buf.len() != Self::LEN {
            return Err(WireError::Invalid("clock length"));
        }
        match buf[1] {
            0 => Ok(Clock::Ping { t1: u64_at(buf, 8) }),
            1 => Ok(Clock::Pong {
                t1: u64_at(buf, 8),
                t2: u64_at(buf, 16),
                t3: u64_at(buf, 24),
            }),
            _ => Err(WireError::Invalid("clock kind")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> VideoShardHeader {
        VideoShardHeader {
            stream: 2,
            flags: flags::IDR,
            ref_epoch: 9,
            tx_seq: 0xAABB_CCDD,
            frame_id: 77,
            shard_index: 4,
            k: 3,
            m: 2,
            shard_len: 1134,
            frame_len: 2500,
            capture_ts_us: 123_456,
        }
    }

    fn packet(h: &VideoShardHeader) -> Vec<u8> {
        let mut b = [0u8; VideoShardHeader::LEN];
        h.write(&mut b);
        let mut v = b.to_vec();
        v.resize(VideoShardHeader::LEN + usize::from(h.shard_len), 0x5A);
        v
    }

    #[test]
    fn video_header_is_exactly_as_documented() {
        let p = packet(&header());
        assert_eq!(p[0], 0x01);
        assert_eq!(&p[4..8], &0xAABB_CCDDu32.to_le_bytes());
        assert_eq!(&p[12..14], &4u16.to_le_bytes());
        assert_eq!(&p[24..28], &123_456u32.to_le_bytes());
    }

    #[test]
    fn video_round_trip() {
        let p = packet(&header());
        let (h, body) = VideoShardHeader::parse(&p).unwrap();
        assert_eq!(h, header());
        assert_eq!(body.len(), 1134);
    }

    #[test]
    fn video_rejects_inconsistent_fields() {
        type Mutate = fn(&mut VideoShardHeader);
        let cases: Vec<(&str, Mutate)> = vec![
            ("k zero", |h| h.k = 0),
            ("index out of range", |h| h.shard_index = 5),
            ("odd shard_len", |h| h.shard_len = 1133),
            ("frame_len above capacity", |h| h.frame_len = 3 * 1134 + 1),
            ("frame_len fits fewer shards", |h| h.frame_len = 2 * 1134),
            ("k too large", |h| h.k = MAX_SHARDS + 1),
        ];
        for (name, mutate) in cases {
            let mut h = header();
            mutate(&mut h);
            assert!(VideoShardHeader::parse(&packet(&h)).is_err(), "{name}");
        }
        let mut short = packet(&header());
        short.pop();
        assert!(VideoShardHeader::parse(&short).is_err());
    }

    #[test]
    fn feedback_round_trip_with_long_gap() {
        let f = TransportFeedback {
            fb_seq: 7,
            base_tx_seq: 1000,
            base_arrival_us: 55,
            statuses: vec![
                PacketStatus::Received { delta_ticks: 0 },
                PacketStatus::NotReceived,
                PacketStatus::Received { delta_ticks: 0xFD },
                PacketStatus::Received { delta_ticks: 0xFE },
                PacketStatus::Received { delta_ticks: 9000 },
            ],
        };
        let mut b = Vec::new();
        f.encode(&mut b);
        assert_eq!(TransportFeedback::parse(&b).unwrap(), f);
    }

    #[test]
    fn feedback_rejects_truncation_and_trailing_bytes() {
        let f = TransportFeedback {
            fb_seq: 1,
            base_tx_seq: 2,
            base_arrival_us: 3,
            statuses: vec![PacketStatus::Received { delta_ticks: 9000 }],
        };
        let mut b = Vec::new();
        f.encode(&mut b);
        assert!(TransportFeedback::parse(&b[..b.len() - 1]).is_err());
        b.push(0);
        assert!(TransportFeedback::parse(&b).is_err());
    }

    #[test]
    fn small_messages_round_trip() {
        let r = VideoReport {
            stream: 1,
            client_epoch: 4,
            kind: ReportKind::NeedIdr,
            last_good_frame: 10,
            first_lost_frame: 11,
            latest_seen_frame: 20,
        };
        assert_eq!(VideoReport::parse(&r.to_bytes()).unwrap(), r);

        let p = Pointer {
            relative: true,
            seq: 65535,
            x: -5,
            y: i32::MIN,
            wheel_h_total: 120,
            wheel_v_total: -240,
            display: 3,
        };
        assert_eq!(Pointer::parse(&p.to_bytes()).unwrap(), p);

        let n = Nack {
            stream: 0,
            frame_id: 42,
            shards: vec![1, 2, 900],
        };
        let mut b = Vec::new();
        n.encode(&mut b);
        assert_eq!(Nack::parse(&b).unwrap(), n);

        for c in [
            Clock::Ping { t1: 5 },
            Clock::Pong {
                t1: 5,
                t2: 6,
                t3: 7,
            },
        ] {
            assert_eq!(Clock::parse(&c.to_bytes()).unwrap(), c);
        }
    }

    #[test]
    fn audio_layout_is_as_documented_and_round_trips() {
        let h = AudioHeader {
            stream: 0,
            flags: audio_flags::RESTART,
            frame_10ms: 1,
            tx_seq: 0x0102_0304,
            seq: 77,
            capture_ts_us: 0xDEAD_BEEF,
        };
        let mut b = Vec::new();
        h.encode(&[1, 2, 3], &mut b);
        assert_eq!(b.len(), AudioHeader::LEN + 3);
        assert_eq!(b[0], TYPE_AUDIO);
        assert_eq!(
            &b[4..8],
            &0x0102_0304u32.to_le_bytes(),
            "tx_seq where VideoShard has it"
        );
        assert_eq!(&b[8..12], &77u32.to_le_bytes());
        let (parsed, payload) = AudioHeader::parse(&b).unwrap();
        assert_eq!(parsed, h);
        assert_eq!(payload, &[1, 2, 3]);
    }

    #[test]
    fn audio_rejects_empty_oversized_and_bad_durations() {
        let h = AudioHeader {
            stream: 0,
            flags: 0,
            frame_10ms: 1,
            tx_seq: 1,
            seq: 1,
            capture_ts_us: 1,
        };
        let mut empty = Vec::new();
        h.encode(&[], &mut empty);
        assert!(AudioHeader::parse(&empty).is_err());
        let mut big = Vec::new();
        h.encode(&[0; AudioHeader::MAX_PAYLOAD + 1], &mut big);
        assert!(AudioHeader::parse(&big).is_err());
        let mut zero = Vec::new();
        AudioHeader { frame_10ms: 0, ..h }.encode(&[1], &mut zero);
        assert!(AudioHeader::parse(&zero).is_err());
    }

    /// Parsers face the network: arbitrary bytes must never panic.
    #[test]
    fn parsers_survive_garbage() {
        let mut x = 0x1234_5678_9ABC_DEF1u64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..200_000 {
            let len = (next() % 64) as usize;
            let mut b: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            if let Some(first) = b.first_mut() {
                *first = (next() % 9) as u8;
            }
            let _ = VideoShardHeader::parse(&b);
            let _ = TransportFeedback::parse(&b);
            let _ = VideoReport::parse(&b);
            let _ = Pointer::parse(&b);
            let _ = Nack::parse(&b);
            let _ = Clock::parse(&b);
            let _ = AudioHeader::parse(&b);
        }
    }
}
