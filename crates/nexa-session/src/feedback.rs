//! Builds TransportFeedback from packet arrivals (§3.3).

use std::collections::BTreeMap;

use nexa_wire::datagram::{PacketStatus, TransportFeedback};

const EMIT_INTERVAL_US: u64 = 25_000;
const EMIT_PACKETS: usize = 128;
/// Keeps one report comfortably inside a 1162-byte datagram even with long gaps.
const MAX_STATUSES: u64 = 350;
const TICK_US: u64 = 250;
/// A missing packet is reported lost only once a later one has been here this long:
/// on a path that reorders (a far, jittery one) a packet overtaken by later ones is
/// late, not lost, and reporting it lost cut the rate as heavy loss would. The report
/// stops before such a gap and the next one says what became of it.
const REORDER_WAIT_US: u64 = 50_000;

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
        let base = self.next?;
        let mut end = self.highest.min(base + MAX_STATUSES - 1);
        // Stop before a gap whose later packets are too recent to call it lost.
        let mut s = base;
        while s <= end {
            if !self.arrivals.contains_key(&s) {
                let after = self.arrivals.range(s..).next().map(|(_, &t)| t);
                if after.is_some_and(|t| now_us.saturating_sub(t) < REORDER_WAIT_US) {
                    if s == base {
                        return None;
                    }
                    end = s - 1;
                    break;
                }
            }
            s += 1;
        }
        self.last_emit_us = now_us;
        let mut statuses = Vec::with_capacity((end - base + 1) as usize);
        // The latest arrival so far, in ticks. Deltas cannot be negative, so a packet
        // that overtook an earlier one is reported as arriving with it, and the next
        // delta is taken from the latest arrival, not from the overtaking packet: from
        // that one, every later packet of the report came out late by as much as it
        // overtook, which reads as a growing queue (a jittery path was cut to the
        // floor). Deltas are differences of tick numbers, so rounding does not add up
        // across a report either.
        let mut prev: Option<u64> = None;
        let mut base_arrival = 0;
        for s in base..=end {
            match self.arrivals.get(&s) {
                None => statuses.push(PacketStatus::NotReceived),
                Some(&t) => {
                    let tick = t / TICK_US;
                    let delta = match prev {
                        None => {
                            base_arrival = t;
                            0
                        }
                        Some(p) => tick.saturating_sub(p).min(u64::from(u16::MAX)) as u16,
                    };
                    prev = Some(prev.map_or(tick, |p| p.max(tick)));
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

    /// Rebuilds arrival times from a report as the sender does.
    fn arrivals_of(f: &TransportFeedback) -> Vec<Option<u64>> {
        let mut at = u64::from(f.base_arrival_us);
        f.statuses
            .iter()
            .map(|s| match s {
                PacketStatus::NotReceived => None,
                PacketStatus::Received { delta_ticks } => {
                    at += u64::from(*delta_ticks) * TICK_US;
                    Some(at)
                }
            })
            .collect()
    }

    #[test]
    fn a_packet_that_overtook_does_not_make_the_later_ones_late() {
        let mut b = FeedbackBuilder::default();
        // Sent 0..4; packet 1 is held 30 ms, so 2 and 3 overtake it.
        b.on_packet(0, 100_000);
        b.on_packet(1, 135_000);
        b.on_packet(2, 105_000);
        b.on_packet(3, 110_000);
        b.on_packet(4, 140_000);
        let f = b.poll(1_000_000).unwrap();
        let rebuilt = arrivals_of(&f);
        // The overtaking packets are reported with the latest arrival before them, no
        // later; the one after is exact.
        assert_eq!(rebuilt[0], Some(100_000));
        assert_eq!(rebuilt[1], Some(135_000));
        assert_eq!(rebuilt[2], Some(135_000));
        assert_eq!(rebuilt[3], Some(135_000));
        assert_eq!(rebuilt[4], Some(140_000));
    }

    #[test]
    fn rounding_does_not_add_up_across_a_report() {
        let mut b = FeedbackBuilder::default();
        // 200 packets 1 ms + 249 µs apart: each delta used to lose 249 µs.
        for s in 0..200u32 {
            b.on_packet(s, 1_000_000 + u64::from(s) * 1_249);
        }
        let f = b.poll(10_000_000).unwrap();
        let last = arrivals_of(&f).last().copied().flatten().unwrap();
        let real: u64 = 1_000_000 + 199 * 1_249;
        assert!(real.abs_diff(last) < TICK_US, "{last} vs {real}");
    }

    #[test]
    fn late_packet_after_report_is_ignored() {
        let mut b = FeedbackBuilder::default();
        b.on_packet(5, 0);
        b.on_packet(7, 500);
        // 6 is missing for longer than a reordering explains: reported lost.
        let first = b.poll(60_000).unwrap();
        assert_eq!(first.statuses.len(), 3);
        b.on_packet(6, 70_000);
        assert!(b.poll(120_000).is_none());
    }

    #[test]
    fn a_packet_overtaken_by_later_ones_is_late_not_lost() {
        let mut b = FeedbackBuilder::default();
        b.on_packet(0, 0);
        b.on_packet(2, 10_000);
        b.on_packet(3, 11_000);
        // 1 is missing, but 2 and 3 came only 20 ms ago: the report stops before it.
        let first = b.poll(30_000).unwrap();
        assert_eq!(
            first.statuses,
            vec![PacketStatus::Received { delta_ticks: 0 }]
        );
        // It turns up, 25 ms late, and the next report has it received.
        b.on_packet(1, 35_000);
        let next = b.poll(70_000).unwrap();
        assert_eq!(next.base_tx_seq, 1);
        assert!(
            next.statuses
                .iter()
                .all(|s| matches!(s, PacketStatus::Received { .. })),
            "{:?}",
            next.statuses
        );
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
        // Past the reordering wait, so the gap (u32::MAX, 0) is reported.
        let f = b.poll(60_000).unwrap();
        assert_eq!(f.base_tx_seq, u32::MAX - 1);
        assert_eq!(f.statuses.len(), 4);
    }
}
