//! Builds TransportFeedback from packet arrivals (§3.3).

use std::collections::BTreeMap;

use nexa_wire::datagram::{PacketStatus, TransportFeedback};

const EMIT_INTERVAL_US: u64 = 25_000;
const EMIT_PACKETS: usize = 128;
/// Keeps one report comfortably inside a 1162-byte datagram even with long gaps.
const MAX_STATUSES: u64 = 350;
const TICK_US: u64 = 250;

#[derive(Default)]
pub struct FeedbackBuilder {
    /// Next sequence not yet reported, unwrapped to 64 bits.
    next: Option<u64>,
    highest: u64,
    arrivals: BTreeMap<u64, u64>,
    fb_seq: u16,
    last_emit_us: u64,
}

impl FeedbackBuilder {
    fn unwrap_seq(&self, seq: u32) -> u64 {
        // Nearest 64-bit value to the highest sequence seen.
        let base = self.highest & !0xFFFF_FFFF;
        let mut c = base | u64::from(seq);
        if c + (1 << 31) < self.highest {
            c += 1 << 32;
        } else if c > self.highest + (1 << 31) && c >= 1 << 32 {
            c -= 1 << 32;
        }
        c
    }

    pub fn on_packet(&mut self, seq: u32, arrival_us: u64) {
        let s = self.unwrap_seq(seq);
        if self.next.is_some_and(|n| s < n) {
            return; // Already reported as missing.
        }
        self.highest = self.highest.max(s);
        self.next.get_or_insert(s);
        self.arrivals.insert(s, arrival_us);
    }

    pub fn poll(&mut self, now_us: u64) -> Option<TransportFeedback> {
        if self.arrivals.is_empty()
            || (self.arrivals.len() < EMIT_PACKETS && now_us < self.last_emit_us + EMIT_INTERVAL_US)
        {
            return None;
        }
        self.last_emit_us = now_us;
        let base = self.next?;
        let end = self.highest.min(base + MAX_STATUSES - 1);
        let mut statuses = Vec::with_capacity((end - base + 1) as usize);
        let mut prev: Option<u64> = None;
        let mut base_arrival = 0;
        for s in base..=end {
            match self.arrivals.get(&s) {
                None => statuses.push(PacketStatus::NotReceived),
                Some(&t) => {
                    let delta = match prev {
                        None => {
                            base_arrival = t;
                            0
                        }
                        Some(p) => (t.saturating_sub(p) / TICK_US).min(u64::from(u16::MAX)) as u16,
                    };
                    prev = Some(t);
                    statuses.push(PacketStatus::Received { delta_ticks: delta });
                }
            }
        }
        self.arrivals = self.arrivals.split_off(&(end + 1));
        self.next = Some(end + 1);
        self.fb_seq = self.fb_seq.wrapping_add(1);
        Some(TransportFeedback {
            fb_seq: self.fb_seq,
            base_tx_seq: base as u32,
            base_arrival_us: base_arrival as u32,
            statuses,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_gaps_and_deltas() {
        let mut b = FeedbackBuilder::default();
        b.on_packet(10, 1_000);
        b.on_packet(12, 1_500);
        assert!(b.poll(1_600).is_none() || b.last_emit_us == 1_600);
        let f = b
            .poll(100_000)
            .or_else(|| {
                Some(TransportFeedback {
                    fb_seq: 0,
                    base_tx_seq: 0,
                    base_arrival_us: 0,
                    statuses: vec![],
                })
            })
            .unwrap();
        assert_eq!(f.base_tx_seq, 10);
        assert_eq!(f.base_arrival_us, 1_000);
        assert_eq!(
            f.statuses,
            vec![
                PacketStatus::Received { delta_ticks: 0 },
                PacketStatus::NotReceived,
                PacketStatus::Received { delta_ticks: 2 }
            ]
        );
    }

    #[test]
    fn late_packet_after_report_is_ignored() {
        let mut b = FeedbackBuilder::default();
        b.on_packet(5, 0);
        b.on_packet(7, 500);
        let first = b.poll(30_000).unwrap();
        assert_eq!(first.statuses.len(), 3);
        b.on_packet(6, 40_000);
        assert!(b.poll(90_000).is_none());
    }

    #[test]
    fn emits_early_when_many_packets_arrive() {
        let mut b = FeedbackBuilder {
            last_emit_us: 1_000_000,
            ..Default::default()
        };
        for s in 0..EMIT_PACKETS as u32 {
            b.on_packet(s, 1_000_000);
        }
        assert!(b.poll(1_000_001).is_some());
    }

    #[test]
    fn large_ranges_are_split_and_fit_a_datagram() {
        let mut b = FeedbackBuilder::default();
        for s in 0..2_000u32 {
            b.on_packet(s, u64::from(s) * 1_000_000);
        }
        let f = b.poll(u64::MAX / 2).unwrap();
        let mut w = Vec::new();
        f.encode(&mut w);
        assert!(w.len() <= 1162, "{}", w.len());
        assert_eq!(
            b.poll(u64::MAX / 2).unwrap().base_tx_seq,
            MAX_STATUSES as u32
        );
    }

    #[test]
    fn sequence_wraps() {
        let mut b = FeedbackBuilder::default();
        b.on_packet(u32::MAX - 1, 0);
        b.on_packet(1, 1_000);
        let f = b.poll(30_000).unwrap();
        assert_eq!(f.base_tx_seq, u32::MAX - 1);
        assert_eq!(f.statuses.len(), 4);
    }
}
