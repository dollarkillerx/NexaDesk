//! The viewer's share of the §6.1 frame timeline, for the HUD (§12).
//!
//! Measured here: first shard → last shard → decode start → decode done → present
//! call → present done. The host's part (capture to the last shard leaving) is one
//! segment, `HostAndNetwork`, computed from `capture_ts_us` and the clock offset of
//! the `ClockPing`/`ClockPong` sample with the smallest round trip among the last
//! 30 (§3.3). The host's own split (convert, encode, pacing) is not sent over.

use std::{
    collections::VecDeque,
    sync::{Mutex, MutexGuard},
    time::Instant,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Segment {
    HostAndNetwork,
    Receive,
    DecodeQueue,
    Decode,
    PresentQueue,
    Present,
    Total,
}

impl Segment {
    pub const ALL: [Segment; 7] = [
        Segment::HostAndNetwork,
        Segment::Receive,
        Segment::DecodeQueue,
        Segment::Decode,
        Segment::PresentQueue,
        Segment::Present,
        Segment::Total,
    ];

    fn index(self) -> usize {
        self as usize
    }
}

/// Frames waiting for later stamps; anything older is forgotten.
const PENDING: usize = 32;
/// Samples per segment the percentiles are taken over.
const WINDOW: usize = 240;
const CLOCK_SAMPLES: usize = 30;
/// A host-and-network figure beyond this is a clock problem, not a measurement.
const MAX_PLAUSIBLE_US: i64 = 10_000_000;

#[derive(Clone, Copy, Debug)]
struct Stamps {
    frame_id: u32,
    capture_ts_us: u32,
    rx_first: Instant,
    rx_complete: Instant,
    decode: Option<(Instant, Instant)>,
}

#[derive(Default)]
struct Inner {
    pending: VecDeque<Stamps>,
    samples: [VecDeque<u32>; 7],
    /// (round trip, host − viewer offset), µs.
    clock: VecDeque<(u64, i64)>,
}

/// P50 and P99 of one segment, µs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Percentiles {
    pub segment: Segment,
    pub p50_us: u32,
    pub p99_us: u32,
}

pub struct LatencyStats {
    /// The viewer clock that `ClockPing.t1` and arrival times count from.
    base: Instant,
    inner: Mutex<Inner>,
}

impl Default for LatencyStats {
    fn default() -> Self {
        Self::new(Instant::now())
    }
}

fn micros(d: std::time::Duration) -> u32 {
    d.as_micros().min(u128::from(u32::MAX)) as u32
}

impl LatencyStats {
    pub fn new(base: Instant) -> Self {
        Self {
            base,
            inner: Mutex::new(Inner::default()),
        }
    }

    pub fn base(&self) -> Instant {
        self.base
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A ping answered: `t1`, `t4` on this clock, `t2`, `t3` on the host's.
    pub fn on_clock(&self, t1: u64, t2: u64, t3: u64, t4: u64) {
        let rtt = t4.saturating_sub(t1).saturating_sub(t3.saturating_sub(t2));
        let offset = ((t2 as i64 - t1 as i64) + (t3 as i64 - t4 as i64)) / 2;
        let mut inner = self.lock();
        if inner.clock.len() == CLOCK_SAMPLES {
            inner.clock.pop_front();
        }
        inner.clock.push_back((rtt, offset));
    }

    /// Host clock minus this clock, from the best recent sample.
    pub fn clock_offset_us(&self) -> Option<i64> {
        self.lock()
            .clock
            .iter()
            .min_by_key(|(rtt, _)| *rtt)
            .map(|(_, o)| *o)
    }

    /// The frame's last shard arrived (or it was repaired) and it goes to the decoder.
    pub fn on_complete(
        &self,
        frame_id: u32,
        capture_ts_us: u32,
        rx_first: Instant,
        rx_complete: Instant,
    ) {
        let mut inner = self.lock();
        if inner.pending.len() == PENDING {
            inner.pending.pop_front();
        }
        inner.pending.push_back(Stamps {
            frame_id,
            capture_ts_us,
            rx_first,
            rx_complete,
            decode: None,
        });
    }

    pub fn on_decoded(&self, frame_id: u32, start: Instant, done: Instant) {
        if let Some(s) = self
            .lock()
            .pending
            .iter_mut()
            .find(|s| s.frame_id == frame_id)
        {
            s.decode = Some((start, done));
        }
    }

    /// The frame was presented; its sample is complete. Repaints of a frame already
    /// counted find nothing and are ignored.
    pub fn on_presented(&self, frame_id: u32, call: Instant, done: Instant) {
        let offset = self.clock_offset_us();
        let mut inner = self.lock();
        let Some(at) = inner.pending.iter().position(|s| s.frame_id == frame_id) else {
            return;
        };
        let s = inner.pending.remove(at).expect("index is valid");
        // Frames before it were dropped between decode and present.
        inner
            .pending
            .retain(|p| p.frame_id.wrapping_sub(frame_id) < u32::MAX / 2);
        let Some((decode_start, decode_done)) = s.decode else {
            return;
        };
        let mut push = |seg: Segment, us: u32| {
            let ring = &mut inner.samples[seg.index()];
            if ring.len() == WINDOW {
                ring.pop_front();
            }
            ring.push_back(us);
        };
        push(
            Segment::Receive,
            micros(s.rx_complete.saturating_duration_since(s.rx_first)),
        );
        push(
            Segment::DecodeQueue,
            micros(decode_start.saturating_duration_since(s.rx_complete)),
        );
        push(
            Segment::Decode,
            micros(decode_done.saturating_duration_since(decode_start)),
        );
        push(
            Segment::PresentQueue,
            micros(call.saturating_duration_since(decode_done)),
        );
        push(
            Segment::Present,
            micros(done.saturating_duration_since(call)),
        );
        if let Some(offset) = offset {
            let host_us =
                |t: Instant| t.saturating_duration_since(self.base).as_micros() as i64 + offset;
            let since_capture =
                |t: Instant| i64::from((host_us(t) as u32).wrapping_sub(s.capture_ts_us) as i32);
            let net = since_capture(s.rx_complete);
            let total = since_capture(done);
            if (0..MAX_PLAUSIBLE_US).contains(&net) && (0..MAX_PLAUSIBLE_US).contains(&total) {
                push(Segment::HostAndNetwork, net as u32);
                push(Segment::Total, total as u32);
            }
        }
    }

    /// Every segment with at least one sample.
    pub fn summary(&self) -> Vec<Percentiles> {
        let inner = self.lock();
        Segment::ALL
            .iter()
            .filter_map(|&segment| {
                let ring = &inner.samples[segment.index()];
                if ring.is_empty() {
                    return None;
                }
                let mut v: Vec<u32> = ring.iter().copied().collect();
                v.sort_unstable();
                let at = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
                Some(Percentiles {
                    segment,
                    p50_us: at(0.5),
                    p99_us: at(0.99),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn segments_follow_the_stamps_and_the_clock_offset() {
        let base = Instant::now();
        let l = LatencyStats::new(base);
        // Host clock = viewer clock + 1 s; the best of two pings wins.
        l.on_clock(1_000, 1_001_500, 1_001_500, 4_000);
        l.on_clock(10_000, 1_010_500, 1_010_500, 11_000);
        assert_eq!(l.clock_offset_us(), Some(1_000_000));

        let ms = Duration::from_millis;
        for id in 0..10u32 {
            let t = base + ms(100 * u64::from(id) + 50);
            // Captured 20 ms before the first shard arrived, host time.
            let capture = (t - ms(20)).duration_since(base).as_micros() as u32 + 1_000_000;
            l.on_complete(id, capture, t, t + ms(2));
            l.on_decoded(id, t + ms(3), t + ms(7));
            l.on_presented(id, t + ms(8), t + ms(9));
        }
        let s = l.summary();
        let get = |seg| s.iter().find(|p| p.segment == seg).unwrap().p50_us;
        assert_eq!(get(Segment::Receive), 2_000);
        assert_eq!(get(Segment::DecodeQueue), 1_000);
        assert_eq!(get(Segment::Decode), 4_000);
        assert_eq!(get(Segment::PresentQueue), 1_000);
        assert_eq!(get(Segment::Present), 1_000);
        assert_eq!(get(Segment::HostAndNetwork), 22_000);
        assert_eq!(get(Segment::Total), 29_000);
    }

    #[test]
    fn without_a_clock_only_local_segments_appear_and_repaints_are_ignored() {
        let base = Instant::now();
        let l = LatencyStats::new(base);
        l.on_complete(5, 0, base, base);
        l.on_decoded(5, base, base);
        l.on_presented(5, base, base);
        l.on_presented(5, base, base);
        let s = l.summary();
        assert!(s.iter().all(|p| p.segment != Segment::HostAndNetwork));
        assert_eq!(s.len(), 5);
    }
}
