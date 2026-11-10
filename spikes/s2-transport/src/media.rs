//! VideoShard wire format (DETAILED_DESIGN §3.2), RS-FEC packetizer and
//! reassembly (§6.2), with a pattern payload so recovered data is verified.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};

use bytes::{BufMut, Bytes, BytesMut};
use quinn::Connection;

use crate::{Rng, now_us, set_realtime_qos, wait_until};

pub const HDR: usize = 28;
const RING: usize = 1 << 21;

/// tx_seq → send time, readable by the receiver because both ends share a clock.
pub struct SendTimes(Vec<AtomicU64>);

impl SendTimes {
    pub fn new() -> Self {
        Self((0..RING).map(|_| AtomicU64::new(0)).collect())
    }
    pub fn put(&self, seq: u32, t: u64) {
        self.0[seq as usize & (RING - 1)].store(t, Relaxed);
    }
    pub fn get(&self, seq: u32) -> u64 {
        self.0[seq as usize & (RING - 1)].load(Relaxed)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct VideoCfg {
    pub secs: f64,
    pub fps: f64,
    pub mbps: f64,
    pub fec: f64,
    /// Every Nth frame is 5× larger, standing in for scene changes. 0 = never.
    pub big_every: u32,
    /// 0 = design formula (§4.9). Otherwise pace at this link-capacity estimate.
    pub pace_bps: f64,
    /// Control experiment: keep quinn's default congestion controller.
    pub default_cc: bool,
}

#[derive(Default, Debug, Clone)]
pub struct TxStats {
    pub frames: u32,
    pub skipped: u32,
    pub packets: u64,
    pub bytes: u64,
    pub buffer_waits: u64,
    pub fec_encode_us: u64,
    pub shard_len: usize,
    pub max_datagram: usize,
}

fn pattern(frame: u32, i: usize) -> u8 {
    (frame as u8).wrapping_add((i / 7) as u8)
}

pub fn fec_m(k: usize, fec: f64) -> usize {
    if fec <= 0.0 {
        0
    } else if k <= 4 {
        1
    } else {
        ((k as f64 * fec).ceil() as usize).min(k / 4 + 1)
    }
}

pub fn run_sender(conn: Connection, c: VideoCfg, times: Arc<SendTimes>, stop: Arc<AtomicBool>) -> TxStats {
    set_realtime_qos();
    let interval = Duration::from_secs_f64(1.0 / c.fps);
    let target_bps = c.mbps * 1e6;
    let avg_frame = target_bps / 8.0 / c.fps;
    let mut rng = Rng::new(7);
    let mut s = TxStats {
        max_datagram: conn.max_datagram_size().unwrap_or(0),
        ..Default::default()
    };
    let shard_len = (s.max_datagram - HDR) & !1;
    s.shard_len = shard_len;
    let mut seq = 0u32;
    let t0 = Instant::now();
    let total = (c.secs * c.fps) as u32;
    let mut frame_id = 0u32;

    for n in 0..total {
        if stop.load(Relaxed) {
            break;
        }
        let due = t0 + interval * n;
        if Instant::now() > due + interval {
            s.skipped += 1; // Back-pressure rule: drop only before encoding.
            continue;
        }
        wait_until(due);
        let capture = now_us();

        let mul = if c.big_every > 0 && n.is_multiple_of(c.big_every) { 5.0 } else { 0.7 + 0.6 * rng.f64() };
        let frame_len = ((avg_frame * mul) as usize).max(1);
        let k = frame_len.div_ceil(shard_len);
        let m = fec_m(k, c.fec);

        let mut originals: Vec<Vec<u8>> = Vec::with_capacity(k);
        for sh in 0..k {
            let mut v = vec![0u8; shard_len];
            let start = sh * shard_len;
            let end = (start + shard_len).min(frame_len);
            for (i, b) in v.iter_mut().enumerate().take(end - start) {
                *b = pattern(frame_id, start + i);
            }
            originals.push(v);
        }
        let t_fec = Instant::now();
        let recovery = if m > 0 { reed_solomon_simd::encode(k, m, &originals).unwrap() } else { Vec::new() };
        s.fec_encode_us += t_fec.elapsed().as_micros() as u64;

        let n_pkts = k + m;
        let bytes = n_pkts * (HDR + shard_len);
        let window = if c.pace_bps > 0.0 {
            Duration::from_secs_f64(bytes as f64 * 8.0 / c.pace_bps)
        } else {
            interval.mul_f64(0.5).min(Duration::from_secs_f64(bytes as f64 * 8.0 / (1.5 * target_bps)))
        };
        let start = Instant::now();

        for (idx, shard) in originals.iter().chain(recovery.iter()).enumerate() {
            let due_pkt = start + window.mul_f64(idx as f64 / n_pkts as f64);
            if due_pkt > Instant::now() + Duration::from_micros(500) {
                wait_until(due_pkt);
            }
            let mut p = BytesMut::with_capacity(HDR + shard_len);
            p.put_u8(0x01);
            p.put_u8(0);
            p.put_u8(0);
            p.put_u8(0);
            p.put_u32_le(seq);
            p.put_u32_le(frame_id);
            p.put_u16_le(idx as u16);
            p.put_u16_le(k as u16);
            p.put_u16_le(m as u16);
            p.put_u16_le(shard_len as u16);
            p.put_u32_le(frame_len as u32);
            p.put_u32_le(capture as u32);
            p.put_slice(shard);
            // send_datagram silently drops the oldest when full: always check first.
            while conn.datagram_send_buffer_space() < p.len() {
                s.buffer_waits += 1;
                if conn.close_reason().is_some() || stop.load(Relaxed) {
                    return s;
                }
                std::thread::sleep(Duration::from_micros(50));
            }
            times.put(seq, now_us());
            if conn.send_datagram(p.freeze()).is_err() {
                return s;
            }
            seq = seq.wrapping_add(1);
            s.packets += 1;
            s.bytes += (HDR + shard_len) as u64;
        }
        s.frames += 1;
        frame_id += 1;
    }
    s
}

#[derive(Default, Debug)]
pub struct RxStats {
    pub packet_latency_us: Vec<u32>,
    /// (completion time, frame latency)
    pub frames: Vec<(u64, u32)>,
    pub fec_recovered: u64,
    pub corrupt: u64,
    pub max_payload: usize,
}

struct Slot {
    k: usize,
    m: usize,
    shard_len: usize,
    frame_len: usize,
    capture: u32,
    orig: Vec<Option<Bytes>>,
    rec: Vec<Option<Bytes>>,
    count: usize,
}

pub async fn run_receiver(conn: Connection, times: Arc<SendTimes>) -> RxStats {
    let mut st = RxStats::default();
    st.packet_latency_us.reserve(2_000_000);
    let mut slots: HashMap<u32, Slot> = HashMap::new();
    let mut done: Vec<bool> = vec![false; 1 << 18];
    while let Ok(pkt) = conn.read_datagram().await {
        let now = now_us();
        st.max_payload = st.max_payload.max(pkt.len());
        let le16 = |o: usize| u16::from_le_bytes([pkt[o], pkt[o + 1]]) as usize;
        let le32 = |o: usize| u32::from_le_bytes(pkt[o..o + 4].try_into().unwrap());
        if pkt[0] != 0x01 {
            continue;
        }
        let seq = le32(4);
        st.packet_latency_us.push(now.saturating_sub(times.get(seq)) as u32);
        let fid = le32(8);
        if done[fid as usize] {
            continue;
        }
        let (idx, k, m, shard_len, frame_len, capture) = (le16(12), le16(14), le16(16), le16(18), le32(20) as usize, le32(24));
        let slot = slots.entry(fid).or_insert_with(|| Slot {
            k,
            m,
            shard_len,
            frame_len,
            capture,
            orig: vec![None; k],
            rec: vec![None; m],
            count: 0,
        });
        let body = pkt.slice(HDR..);
        let cell = if idx < slot.k { &mut slot.orig[idx] } else { &mut slot.rec[idx - slot.k] };
        if cell.is_none() {
            *cell = Some(body);
            slot.count += 1;
        }
        if slot.count < slot.k {
            continue;
        }
        let slot = slots.remove(&fid).unwrap();
        done[fid as usize] = true;
        let mut ok = true;
        if slot.orig.iter().any(Option::is_none) {
            let originals = slot.orig.iter().enumerate().filter_map(|(i, b)| b.as_ref().map(|b| (i, &b[..])));
            let recovery = slot.rec.iter().enumerate().filter_map(|(i, b)| b.as_ref().map(|b| (i, &b[..])));
            match reed_solomon_simd::decode(slot.k, slot.m, originals, recovery) {
                Ok(restored) => {
                    st.fec_recovered += restored.len() as u64;
                    for (i, data) in &restored {
                        let start = i * slot.shard_len;
                        let end = (start + slot.shard_len).min(slot.frame_len);
                        ok &= (start..end).all(|p| data[p - start] == pattern(fid, p));
                    }
                }
                Err(_) => ok = false,
            }
        }
        for (i, b) in slot.orig.iter().enumerate() {
            if let Some(b) = b {
                let start = i * slot.shard_len;
                let end = (start + slot.shard_len).min(slot.frame_len);
                ok &= (start..end).all(|p| b[p - start] == pattern(fid, p));
            }
        }
        if !ok {
            st.corrupt += 1;
        }
        let done_at = now_us();
        st.frames.push((done_at, (done_at as u32).wrapping_sub(slot.capture)));
    }
    st
}
