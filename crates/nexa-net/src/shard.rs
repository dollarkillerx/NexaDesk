//! Frame ⇄ datagram conversion with per-frame Reed-Solomon FEC (§6.2).

use std::time::{Duration, Instant};

use bytes::{BufMut, Bytes, BytesMut};
use nexa_wire::datagram::{MAX_SHARDS, VideoShardHeader};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ShardError {
    #[error("frame needs {0} shards, over the limit")]
    FrameTooLarge(usize),
    #[error("datagram size {0} leaves no room for payload")]
    DatagramTooSmall(usize),
    #[error("FEC failed: {0}")]
    Fec(String),
}

/// Parity count from §6.2: small frames get one parity shard, the rest a ratio capped at 25%.
pub fn parity_count(k: usize, fec: f64) -> usize {
    if fec <= 0.0 {
        0
    } else if k <= 4 {
        1
    } else {
        ((k as f64 * fec).ceil() as usize).min(k.div_ceil(4)).max(1)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FrameInfo {
    pub stream: u8,
    pub flags: u8,
    pub ref_epoch: u8,
    pub frame_id: u32,
    pub capture_ts_us: u32,
}

#[derive(Debug, Default)]
pub struct Packetizer {
    next_seq: u32,
}

impl Packetizer {
    /// Splits one encoded frame into ready-to-send datagrams. `max_datagram` must be
    /// re-read from the connection for every frame.
    pub fn packetize(
        &mut self,
        frame: &[u8],
        info: FrameInfo,
        fec: f64,
        max_datagram: usize,
    ) -> Result<Vec<Bytes>, ShardError> {
        if max_datagram <= VideoShardHeader::LEN + 1 {
            return Err(ShardError::DatagramTooSmall(max_datagram));
        }
        // Frames shorter than one shard use a shorter shard. Never zero: a frame is at least 1 byte long on the wire.
        let room = (max_datagram - VideoShardHeader::LEN) & !1;
        let frame_len = frame.len().max(1);
        let shard_len = room.min((frame_len + 1) & !1);
        let k = frame_len.div_ceil(shard_len);
        if k > usize::from(MAX_SHARDS) {
            return Err(ShardError::FrameTooLarge(k));
        }
        let m = parity_count(k, fec);

        let mut originals: Vec<Vec<u8>> = Vec::with_capacity(k);
        for i in 0..k {
            let mut s = vec![0u8; shard_len];
            let start = i * shard_len;
            let end = (start + shard_len).min(frame.len());
            if start < end {
                s[..end - start].copy_from_slice(&frame[start..end]);
            }
            originals.push(s);
        }
        let recovery = if m > 0 {
            reed_solomon_simd::encode(k, m, &originals)
                .map_err(|e| ShardError::Fec(e.to_string()))?
        } else {
            Vec::new()
        };

        let mut out = Vec::with_capacity(k + m);
        for (idx, shard) in originals.iter().chain(recovery.iter()).enumerate() {
            let header = VideoShardHeader {
                stream: info.stream,
                flags: info.flags,
                ref_epoch: info.ref_epoch,
                tx_seq: self.next_seq,
                frame_id: info.frame_id,
                shard_index: idx as u16,
                k: k as u16,
                m: m as u16,
                shard_len: shard_len as u16,
                frame_len: frame_len as u32,
                capture_ts_us: info.capture_ts_us,
            };
            self.next_seq = self.next_seq.wrapping_add(1);
            let mut hdr = [0u8; VideoShardHeader::LEN];
            header.write(&mut hdr);
            let mut p = BytesMut::with_capacity(VideoShardHeader::LEN + shard_len);
            p.put_slice(&hdr);
            p.put_slice(shard);
            out.push(p.freeze());
        }
        Ok(out)
    }

    pub fn next_seq(&self) -> u32 {
        self.next_seq
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct CompleteFrame {
    pub frame_id: u32,
    pub flags: u8,
    pub ref_epoch: u8,
    pub capture_ts_us: u32,
    pub data: Vec<u8>,
    pub recovered_shards: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AssemblerEvent {
    Complete(CompleteFrame),
    Lost { frame_id: u32 },
}

struct Slot {
    h: VideoShardHeader,
    first_rx: Instant,
    orig: Vec<Option<Vec<u8>>>,
    rec: Vec<Option<Vec<u8>>>,
    count: usize,
}

/// Number of frames tracked at once. §6.2.
const SLOTS: usize = 16;

/// Reassembles one stream. Events are in `frame_id` order: a frame is only
/// released once everything older is complete or declared lost, because the
/// decoder must see frames in order.
pub struct Assembler {
    deadline: Duration,
    reorder_grace: Duration,
    slots: Vec<Option<Slot>>,
    done: Vec<Option<CompleteFrame>>,
    /// Oldest frame not yet emitted.
    next: Option<u32>,
    newest_complete_at: Option<(u32, Instant)>,
}

/// `a` is newer than `b`, with wraparound.
fn newer(a: u32, b: u32) -> bool {
    a != b && a.wrapping_sub(b) < u32::MAX / 2
}

impl Assembler {
    pub fn new(deadline: Duration, reorder_grace: Duration) -> Self {
        Self {
            deadline,
            reorder_grace,
            slots: (0..SLOTS).map(|_| None).collect(),
            done: (0..SLOTS).map(|_| None).collect(),
            next: None,
            newest_complete_at: None,
        }
    }

    fn idx(id: u32) -> usize {
        id as usize % SLOTS
    }

    /// Feeds one parsed shard. Malformed datagrams must be rejected by the caller's `parse`.
    pub fn push(
        &mut self,
        h: VideoShardHeader,
        payload: &[u8],
        now: Instant,
        out: &mut Vec<AssemblerEvent>,
    ) {
        let next = *self.next.get_or_insert(h.frame_id);
        if newer(next, h.frame_id) {
            return; // Already emitted or declared lost.
        }
        // A frame too far ahead forces the oldest ones out.
        while h.frame_id.wrapping_sub(self.next.unwrap()) as usize >= SLOTS {
            self.declare_lost_oldest(out);
        }

        let i = Self::idx(h.frame_id);
        if self.done[i]
            .as_ref()
            .is_some_and(|d| d.frame_id == h.frame_id)
        {
            return;
        }
        let slot = self.slots[i].get_or_insert_with(|| Slot {
            h,
            first_rx: now,
            orig: vec![None; usize::from(h.k)],
            rec: vec![None; usize::from(h.m)],
            count: 0,
        });
        // Header fields must agree across a frame, or an attacker/bug could corrupt the buffers.
        let s = &slot.h;
        if s.frame_id != h.frame_id
            || s.k != h.k
            || s.m != h.m
            || s.shard_len != h.shard_len
            || s.frame_len != h.frame_len
        {
            return;
        }
        let idx = usize::from(h.shard_index);
        let k = usize::from(h.k);
        let cell = if idx < k {
            &mut slot.orig[idx]
        } else {
            &mut slot.rec[idx - k]
        };
        if cell.is_some() {
            return;
        }
        *cell = Some(payload.to_vec());
        slot.count += 1;

        if slot.count >= k {
            let slot = self.slots[i].take().expect("present");
            if let Some(frame) = Self::finish(slot) {
                self.newest_complete_at = match self.newest_complete_at {
                    Some((id, _)) if newer(id, frame.frame_id) => self.newest_complete_at,
                    _ => Some((frame.frame_id, now)),
                };
                self.done[i] = Some(frame);
            }
            self.drain(out);
        }
    }

    fn finish(slot: Slot) -> Option<CompleteFrame> {
        let h = slot.h;
        let (k, m, shard_len) = (usize::from(h.k), usize::from(h.m), usize::from(h.shard_len));
        let frame_len = h.frame_len as usize;
        let mut recovered_shards = 0;
        let mut restored = None;
        if slot.orig.iter().any(Option::is_none) {
            let originals = slot
                .orig
                .iter()
                .enumerate()
                .filter_map(|(i, b)| b.as_ref().map(|b| (i, b.as_slice())));
            let recovery = slot
                .rec
                .iter()
                .enumerate()
                .filter_map(|(i, b)| b.as_ref().map(|b| (i, b.as_slice())));
            let r = reed_solomon_simd::decode(k, m, originals, recovery).ok()?;
            recovered_shards = r.len() as u32;
            restored = Some(r);
        }
        let mut data = Vec::with_capacity(k * shard_len);
        for (i, shard) in slot.orig.iter().enumerate() {
            match shard {
                Some(s) => data.extend_from_slice(s),
                None => data.extend_from_slice(restored.as_ref()?.get(&i)?),
            }
        }
        data.truncate(frame_len);
        Some(CompleteFrame {
            frame_id: h.frame_id,
            flags: h.flags,
            ref_epoch: h.ref_epoch,
            capture_ts_us: h.capture_ts_us,
            data,
            recovered_shards,
        })
    }

    /// Emits everything that is ready, in order.
    fn drain(&mut self, out: &mut Vec<AssemblerEvent>) {
        while let Some(next) = self.next {
            let i = Self::idx(next);
            match self.done[i].take() {
                Some(f) if f.frame_id == next => {
                    out.push(AssemblerEvent::Complete(f));
                    self.next = Some(next.wrapping_add(1));
                }
                other => {
                    self.done[i] = other;
                    break;
                }
            }
        }
    }

    fn declare_lost_oldest(&mut self, out: &mut Vec<AssemblerEvent>) {
        let id = self.next.expect("started");
        let i = Self::idx(id);
        if self.done[i].as_ref().is_some_and(|d| d.frame_id == id) {
            self.drain(out);
            return;
        }
        if self.slots[i].as_ref().is_some_and(|s| s.h.frame_id == id) {
            self.slots[i] = None;
        }
        out.push(AssemblerEvent::Lost { frame_id: id });
        self.next = Some(id.wrapping_add(1));
        self.drain(out);
    }

    /// Applies §6.2's loss rules. Call regularly and after every `push`.
    pub fn poll(&mut self, now: Instant, out: &mut Vec<AssemblerEvent>) {
        while let Some(next) = self.next {
            let i = Self::idx(next);
            let timed_out = self.slots[i]
                .as_ref()
                .is_some_and(|s| s.h.frame_id == next && now >= s.first_rx + self.deadline);
            let overtaken = self
                .newest_complete_at
                .is_some_and(|(id, at)| newer(id, next) && now >= at + self.reorder_grace);
            if !(timed_out || overtaken) {
                break;
            }
            self.declare_lost_oldest(out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX_DG: usize = 1162;

    fn info(id: u32) -> FrameInfo {
        FrameInfo {
            stream: 0,
            flags: 0,
            ref_epoch: 0,
            frame_id: id,
            capture_ts_us: id * 8,
        }
    }

    fn frame(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| seed.wrapping_add((i % 251) as u8))
            .collect()
    }

    fn asm() -> Assembler {
        Assembler::new(Duration::from_millis(12), Duration::from_millis(2))
    }

    fn feed(a: &mut Assembler, p: &Bytes, now: Instant, out: &mut Vec<AssemblerEvent>) {
        let (h, body) = VideoShardHeader::parse(p).unwrap();
        a.push(h, body, now, out);
    }

    fn complete(e: &AssemblerEvent) -> &CompleteFrame {
        match e {
            AssemblerEvent::Complete(f) => f,
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn parity_matches_design_table() {
        assert_eq!(parity_count(4, 0.0), 0);
        assert_eq!(parity_count(1, 0.1), 1);
        assert_eq!(parity_count(4, 0.25), 1);
        assert_eq!(parity_count(92, 0.25), 23);
        assert_eq!(parity_count(100, 0.9), 25);
    }

    #[test]
    fn round_trip_all_sizes_without_loss() {
        for len in [0usize, 1, 2, 1133, 1134, 1135, 5000, 200_000] {
            let f = frame(len, 3);
            let mut p = Packetizer::default();
            let pkts = p.packetize(&f, info(1), 0.0, MAX_DG).unwrap();
            let (mut a, now, mut out) = (asm(), Instant::now(), Vec::new());
            for pk in &pkts {
                feed(&mut a, pk, now, &mut out);
            }
            assert_eq!(out.len(), 1, "len {len}");
            let c = complete(&out[0]);
            // An empty frame goes on the wire as a single zero byte.
            let want = if len == 0 { vec![0] } else { f };
            assert_eq!(c.data, want, "len {len}");
        }
    }

    #[test]
    fn every_packet_fits_and_parses() {
        let mut p = Packetizer::default();
        let pkts = p
            .packetize(&frame(100_000, 1), info(1), 0.25, MAX_DG)
            .unwrap();
        assert!(pkts.iter().all(|x| x.len() <= MAX_DG));
        let (first, _) = VideoShardHeader::parse(&pkts[0]).unwrap();
        assert_eq!(first.shard_len % 2, 0);
        let (last, _) = VideoShardHeader::parse(pkts.last().unwrap()).unwrap();
        assert_eq!(last.tx_seq as usize, pkts.len() - 1);
    }

    #[test]
    fn recovers_up_to_m_losses_and_data_is_exact() {
        let f = frame(100_000, 9);
        let mut p = Packetizer::default();
        let pkts = p.packetize(&f, info(5), 0.25, MAX_DG).unwrap();
        let (h, _) = VideoShardHeader::parse(&pkts[0]).unwrap();
        let (mut a, now, mut out) = (asm(), Instant::now(), Vec::new());
        // Drop exactly m originals, from the front, the worst place.
        for pk in pkts.iter().skip(usize::from(h.m)) {
            feed(&mut a, pk, now, &mut out);
        }
        let c = complete(&out[0]);
        assert_eq!(c.data, f);
        assert_eq!(c.recovered_shards, u32::from(h.m));
    }

    #[test]
    fn duplicates_are_ignored() {
        let f = frame(5000, 2);
        let pkts = Packetizer::default()
            .packetize(&f, info(1), 0.0, MAX_DG)
            .unwrap();
        let (mut a, now, mut out) = (asm(), Instant::now(), Vec::new());
        for pk in pkts.iter().chain(pkts.iter()) {
            feed(&mut a, pk, now, &mut out);
        }
        assert_eq!(out.len(), 1);
        assert_eq!(complete(&out[0]).data, f);
    }

    #[test]
    fn output_is_in_order_despite_reordered_completion() {
        let mut p = Packetizer::default();
        let f1 = p.packetize(&frame(3000, 1), info(1), 0.0, MAX_DG).unwrap();
        let f2 = p.packetize(&frame(3000, 2), info(2), 0.0, MAX_DG).unwrap();
        let (mut a, now, mut out) = (asm(), Instant::now(), Vec::new());
        feed(&mut a, &f1[0], now, &mut out);
        for pk in &f2 {
            feed(&mut a, pk, now, &mut out);
        }
        assert!(out.is_empty(), "frame 2 must wait for frame 1");
        for pk in &f1[1..] {
            feed(&mut a, pk, now, &mut out);
        }
        let ids: Vec<u32> = out.iter().map(|e| complete(e).frame_id).collect();
        assert_eq!(ids, [1, 2]);
    }

    #[test]
    fn loss_is_declared_by_deadline() {
        let f = frame(5000, 2);
        let pkts = Packetizer::default()
            .packetize(&f, info(1), 0.0, MAX_DG)
            .unwrap();
        let (mut a, t0, mut out) = (asm(), Instant::now(), Vec::new());
        feed(&mut a, &pkts[0], t0, &mut out);
        a.poll(t0 + Duration::from_millis(11), &mut out);
        assert!(out.is_empty());
        a.poll(t0 + Duration::from_millis(12), &mut out);
        assert_eq!(out, [AssemblerEvent::Lost { frame_id: 1 }]);
    }

    #[test]
    fn loss_is_declared_when_overtaken_after_grace() {
        let mut p = Packetizer::default();
        let f1 = p.packetize(&frame(5000, 1), info(1), 0.0, MAX_DG).unwrap();
        let f2 = p.packetize(&frame(500, 2), info(2), 0.0, MAX_DG).unwrap();
        let (mut a, t0, mut out) = (asm(), Instant::now(), Vec::new());
        feed(&mut a, &f1[0], t0, &mut out);
        for pk in &f2 {
            feed(&mut a, pk, t0, &mut out);
        }
        a.poll(t0 + Duration::from_millis(1), &mut out);
        assert!(out.is_empty(), "still inside reorder grace");
        a.poll(t0 + Duration::from_millis(2), &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], AssemblerEvent::Lost { frame_id: 1 });
        assert_eq!(complete(&out[1]).frame_id, 2);
    }

    #[test]
    fn wholly_missing_frame_is_detected() {
        let mut p = Packetizer::default();
        let _f1 = p.packetize(&frame(500, 1), info(1), 0.0, MAX_DG).unwrap();
        let f0 = p.packetize(&frame(500, 9), info(0), 0.0, MAX_DG).unwrap();
        let f2 = p.packetize(&frame(500, 2), info(2), 0.0, MAX_DG).unwrap();
        let (mut a, t0, mut out) = (asm(), Instant::now(), Vec::new());
        feed(&mut a, &f0[0], t0, &mut out);
        feed(&mut a, &f2[0], t0, &mut out);
        a.poll(t0 + Duration::from_millis(2), &mut out);
        assert_eq!(complete(&out[0]).frame_id, 0);
        assert_eq!(out[1], AssemblerEvent::Lost { frame_id: 1 });
        assert_eq!(complete(&out[2]).frame_id, 2);
    }

    #[test]
    fn late_shards_of_a_finished_frame_are_ignored() {
        let mut p = Packetizer::default();
        let f1 = p.packetize(&frame(3000, 1), info(1), 0.0, MAX_DG).unwrap();
        let (mut a, t0, mut out) = (asm(), Instant::now(), Vec::new());
        feed(&mut a, &f1[0], t0, &mut out);
        a.poll(t0 + Duration::from_millis(12), &mut out);
        out.clear();
        for pk in &f1[1..] {
            feed(&mut a, pk, t0 + Duration::from_millis(13), &mut out);
        }
        assert!(out.is_empty());
    }

    #[test]
    fn inconsistent_shard_cannot_corrupt_slot() {
        let mut p = Packetizer::default();
        let pkts = p.packetize(&frame(5000, 1), info(1), 0.0, MAX_DG).unwrap();
        let (mut a, t0, mut out) = (asm(), Instant::now(), Vec::new());
        feed(&mut a, &pkts[0], t0, &mut out);
        let (mut h, body) = VideoShardHeader::parse(&pkts[1]).unwrap();
        h.k = 2;
        h.frame_len = 2000;
        a.push(h, body, t0, &mut out);
        for pk in &pkts[1..] {
            feed(&mut a, pk, t0, &mut out);
        }
        assert_eq!(complete(&out[0]).data, frame(5000, 1));
    }

    #[test]
    fn frame_ids_wrap() {
        let mut p = Packetizer::default();
        let start = u32::MAX - 1;
        let (mut a, t0, mut out) = (asm(), Instant::now(), Vec::new());
        for n in 0..4u32 {
            let id = start.wrapping_add(n);
            for pk in p
                .packetize(
                    &frame(800, n as u8),
                    FrameInfo {
                        frame_id: id,
                        ..info(0)
                    },
                    0.0,
                    MAX_DG,
                )
                .unwrap()
            {
                feed(&mut a, &pk, t0, &mut out);
            }
        }
        let ids: Vec<u32> = out.iter().map(|e| complete(e).frame_id).collect();
        assert_eq!(ids, [u32::MAX - 1, u32::MAX, 0, 1]);
    }

    #[test]
    fn huge_gap_flushes_instead_of_hanging() {
        let mut p = Packetizer::default();
        let (mut a, t0, mut out) = (asm(), Instant::now(), Vec::new());
        for pk in p.packetize(&frame(800, 1), info(1), 0.0, MAX_DG).unwrap() {
            feed(&mut a, &pk, t0, &mut out);
        }
        for pk in p
            .packetize(&frame(800, 2), info(1 + 40), 0.0, MAX_DG)
            .unwrap()
        {
            feed(&mut a, &pk, t0, &mut out);
        }
        // Intermediate frames stay pending until the reorder grace passes: order is preserved.
        assert!(!matches!(out.last(), Some(AssemblerEvent::Complete(f)) if f.frame_id == 41));
        a.poll(t0 + Duration::from_millis(2), &mut out);
        assert!(matches!(out.last(), Some(AssemblerEvent::Complete(f)) if f.frame_id == 41));
        let lost = out
            .iter()
            .filter(|e| matches!(e, AssemblerEvent::Lost { .. }))
            .count();
        assert_eq!(lost, 39, "frames 2..=40");
    }
}
