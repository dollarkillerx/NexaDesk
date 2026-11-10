//! Frame-loss recovery protocol (§6.3).
//!
//! M0 showed Apple's H.264 decoder accepts frames with missing references
//! without any error and shows garbage, so decodability must be decided here.

use std::time::{Duration, Instant};

use nexa_wire::datagram::{ReportKind, VideoReport, flags};

use crate::shard::{AssemblerEvent, CompleteFrame};

/// §6.3: escalate to an IDR request after this long unresolved.
const NEED_IDR_AFTER: Duration = Duration::from_millis(500);
/// §6.3: minimum report retransmit interval.
const MIN_REPORT_INTERVAL: Duration = Duration::from_millis(5);
/// §6.3: at most one IDR per second.
const IDR_MIN_INTERVAL: Duration = Duration::from_secs(1);

fn epoch_newer(a: u8, b: u8) -> bool {
    a != b && a.wrapping_sub(b) < 128
}

#[derive(Debug)]
enum State {
    Normal,
    Waiting {
        first_lost: u32,
        since: Instant,
        last_report: Option<Instant>,
    },
}

/// Viewer side: decides which complete frames may reach the decoder.
#[derive(Debug)]
pub struct ReceiverRecovery {
    stream: u8,
    epoch: u8,
    state: State,
    last_good: u32,
    latest_seen: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Decode(CompleteFrame),
    Drop { frame_id: u32 },
    None,
}

impl ReceiverRecovery {
    pub fn new(stream: u8) -> Self {
        Self {
            stream,
            epoch: 0,
            state: State::Normal,
            last_good: 0,
            latest_seen: 0,
        }
    }

    pub fn epoch(&self) -> u8 {
        self.epoch
    }

    pub fn is_waiting(&self) -> bool {
        matches!(self.state, State::Waiting { .. })
    }

    pub fn on_event(&mut self, ev: AssemblerEvent, now: Instant) -> Decision {
        match ev {
            AssemblerEvent::Lost { frame_id } => {
                self.latest_seen = frame_id;
                if matches!(self.state, State::Normal) {
                    self.state = State::Waiting {
                        first_lost: frame_id,
                        since: now,
                        last_report: None,
                    };
                }
                Decision::None
            }
            AssemblerEvent::Complete(f) => {
                self.latest_seen = f.frame_id;
                let newer = epoch_newer(f.ref_epoch, self.epoch);
                let stale = epoch_newer(self.epoch, f.ref_epoch);
                let is_key = f.flags & flags::IDR != 0;
                // Only these frames can be decoded without anything we may have missed.
                let starts_chain = is_key || f.flags & flags::RECOVERY_POINT != 0;

                if (is_key && !stale) || (newer && starts_chain) {
                    self.state = State::Normal;
                    return self.accept(f);
                }
                if newer {
                    // A newer epoch without its recovery point: that frame was lost, and
                    // everything after it references it. Adopt the epoch so the host
                    // accepts our next report, which must go out straight away.
                    self.epoch = f.ref_epoch;
                    let first_lost = self.last_good.wrapping_add(1);
                    match &mut self.state {
                        State::Waiting { last_report, .. } => *last_report = None,
                        State::Normal => {
                            self.state = State::Waiting {
                                first_lost,
                                since: now,
                                last_report: None,
                            };
                        }
                    }
                    return Decision::Drop {
                        frame_id: f.frame_id,
                    };
                }
                if matches!(self.state, State::Normal) && !stale {
                    return self.accept(f);
                }
                Decision::Drop {
                    frame_id: f.frame_id,
                }
            }
        }
    }

    fn accept(&mut self, f: CompleteFrame) -> Decision {
        self.epoch = f.ref_epoch;
        self.last_good = f.frame_id;
        Decision::Decode(f)
    }

    /// Returns a report to send now, if one is due. `rtt` paces retransmits.
    pub fn poll(&mut self, now: Instant, rtt: Duration) -> Option<VideoReport> {
        let State::Waiting {
            first_lost,
            since,
            last_report,
        } = &mut self.state
        else {
            return None;
        };
        if last_report.is_some_and(|t| now < t + rtt.max(MIN_REPORT_INTERVAL)) {
            return None;
        }
        *last_report = Some(now);
        let kind = if now >= *since + NEED_IDR_AFTER {
            ReportKind::NeedIdr
        } else {
            ReportKind::Loss
        };
        Some(VideoReport {
            stream: self.stream,
            client_epoch: self.epoch,
            kind,
            last_good_frame: self.last_good,
            first_lost_frame: *first_lost,
            latest_seen_frame: self.latest_seen,
        })
    }
}

/// What the host must do to the encoder in response to a report.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SenderAction {
    None,
    Invalidate { first_lost: u32 },
    ForceIdr,
}

/// Host side: turns reports into at most one encoder action per epoch.
#[derive(Debug)]
pub struct SenderRecovery {
    epoch: u8,
    next_flags_recovery: bool,
    last_idr: Option<Instant>,
}

impl Default for SenderRecovery {
    fn default() -> Self {
        Self::new()
    }
}

impl SenderRecovery {
    pub fn new() -> Self {
        Self {
            epoch: 0,
            next_flags_recovery: false,
            last_idr: None,
        }
    }

    pub fn epoch(&self) -> u8 {
        self.epoch
    }

    pub fn on_report(&mut self, r: &VideoReport, now: Instant) -> SenderAction {
        // Duplicate or stale: a report is about the epoch the viewer currently holds.
        if r.client_epoch != self.epoch {
            return SenderAction::None;
        }
        let action = match r.kind {
            ReportKind::Loss => SenderAction::Invalidate {
                first_lost: r.first_lost_frame,
            },
            ReportKind::NeedIdr => {
                if self.last_idr.is_some_and(|t| now < t + IDR_MIN_INTERVAL) {
                    return SenderAction::None;
                }
                self.last_idr = Some(now);
                SenderAction::ForceIdr
            }
            ReportKind::FrameAck | ReportKind::DecoderSlow => return SenderAction::None,
        };
        self.epoch = self.epoch.wrapping_add(1);
        self.next_flags_recovery = true;
        action
    }

    /// Consumes the "next encoded frame is a recovery point" flag (§6.3).
    pub fn take_recovery_flag(&mut self) -> bool {
        std::mem::take(&mut self.next_flags_recovery)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(id: u32, epoch: u8) -> AssemblerEvent {
        AssemblerEvent::Complete(CompleteFrame {
            frame_id: id,
            flags: 0,
            ref_epoch: epoch,
            capture_ts_us: 0,
            data: vec![1],
            recovered_shards: 0,
        })
    }

    /// The first frame of a new epoch, as the host marks it.
    fn recovery(id: u32, epoch: u8) -> AssemblerEvent {
        AssemblerEvent::Complete(CompleteFrame {
            frame_id: id,
            flags: flags::RECOVERY_POINT,
            ref_epoch: epoch,
            capture_ts_us: 0,
            data: vec![1],
            recovered_shards: 0,
        })
    }

    fn decoded(d: &Decision) -> bool {
        matches!(d, Decision::Decode(_))
    }

    const RTT: Duration = Duration::from_millis(20);

    #[test]
    fn normal_flow_decodes_everything() {
        let (mut r, t) = (ReceiverRecovery::new(0), Instant::now());
        for i in 0..5 {
            assert!(decoded(&r.on_event(frame(i, 0), t)));
        }
        assert!(r.poll(t, RTT).is_none());
    }

    #[test]
    fn frames_after_a_loss_never_reach_the_decoder_until_a_new_epoch() {
        let (mut r, t) = (ReceiverRecovery::new(0), Instant::now());
        r.on_event(frame(1, 0), t);
        r.on_event(AssemblerEvent::Lost { frame_id: 2 }, t);
        assert!(r.is_waiting());
        assert_eq!(r.on_event(frame(3, 0), t), Decision::Drop { frame_id: 3 });
        assert!(decoded(&r.on_event(recovery(4, 1), t)));
        assert!(!r.is_waiting());
        assert_eq!(r.epoch(), 1);
    }

    #[test]
    fn stale_old_epoch_frame_after_recovery_is_dropped() {
        let (mut r, t) = (ReceiverRecovery::new(0), Instant::now());
        r.on_event(AssemblerEvent::Lost { frame_id: 2 }, t);
        r.on_event(recovery(4, 1), t);
        assert_eq!(r.on_event(frame(5, 0), t), Decision::Drop { frame_id: 5 });
    }

    #[test]
    fn report_content_and_pacing() {
        let (mut r, t) = (ReceiverRecovery::new(3), Instant::now());
        r.on_event(frame(9, 0), t);
        r.on_event(AssemblerEvent::Lost { frame_id: 10 }, t);
        r.on_event(frame(11, 0), t);
        let rep = r.poll(t, RTT).unwrap();
        assert_eq!(
            rep,
            VideoReport {
                stream: 3,
                client_epoch: 0,
                kind: ReportKind::Loss,
                last_good_frame: 9,
                first_lost_frame: 10,
                latest_seen_frame: 11
            }
        );
        assert!(r.poll(t + Duration::from_millis(19), RTT).is_none());
        assert!(r.poll(t + Duration::from_millis(20), RTT).is_some());
    }

    #[test]
    fn report_interval_has_a_floor() {
        let (mut r, t) = (ReceiverRecovery::new(0), Instant::now());
        r.on_event(AssemblerEvent::Lost { frame_id: 1 }, t);
        assert!(r.poll(t, Duration::ZERO).is_some());
        assert!(
            r.poll(t + Duration::from_millis(4), Duration::ZERO)
                .is_none()
        );
        assert!(
            r.poll(t + Duration::from_millis(5), Duration::ZERO)
                .is_some()
        );
    }

    #[test]
    fn escalates_to_need_idr() {
        let (mut r, t) = (ReceiverRecovery::new(0), Instant::now());
        r.on_event(AssemblerEvent::Lost { frame_id: 1 }, t);
        assert_eq!(r.poll(t, RTT).unwrap().kind, ReportKind::Loss);
        assert_eq!(
            r.poll(t + NEED_IDR_AFTER, RTT).unwrap().kind,
            ReportKind::NeedIdr
        );
    }

    fn report(kind: ReportKind, epoch: u8) -> VideoReport {
        VideoReport {
            stream: 0,
            client_epoch: epoch,
            kind,
            last_good_frame: 1,
            first_lost_frame: 2,
            latest_seen_frame: 3,
        }
    }

    #[test]
    fn host_acts_once_per_epoch() {
        let (mut s, t) = (SenderRecovery::new(), Instant::now());
        assert_eq!(
            s.on_report(&report(ReportKind::Loss, 0), t),
            SenderAction::Invalidate { first_lost: 2 }
        );
        assert_eq!(s.epoch(), 1);
        assert!(s.take_recovery_flag());
        assert!(!s.take_recovery_flag());
        assert_eq!(
            s.on_report(&report(ReportKind::Loss, 0), t),
            SenderAction::None,
            "duplicate report"
        );
        assert_eq!(s.epoch(), 1);
    }

    #[test]
    fn idr_is_rate_limited_without_bumping_the_epoch() {
        let (mut s, t) = (SenderRecovery::new(), Instant::now());
        assert_eq!(
            s.on_report(&report(ReportKind::NeedIdr, 0), t),
            SenderAction::ForceIdr
        );
        assert_eq!(
            s.on_report(
                &report(ReportKind::NeedIdr, 1),
                t + Duration::from_millis(500)
            ),
            SenderAction::None
        );
        assert_eq!(s.epoch(), 1);
        assert_eq!(
            s.on_report(&report(ReportKind::NeedIdr, 1), t + IDR_MIN_INTERVAL),
            SenderAction::ForceIdr
        );
    }

    #[test]
    fn epoch_comparison_wraps() {
        assert!(epoch_newer(0, 255));
        assert!(!epoch_newer(255, 0));
    }

    #[test]
    fn end_to_end_loss_recovers_without_decoding_bad_frames() {
        let (mut r, mut s, t) = (
            ReceiverRecovery::new(0),
            SenderRecovery::new(),
            Instant::now(),
        );
        assert!(decoded(&r.on_event(frame(1, s.epoch()), t)));
        r.on_event(AssemblerEvent::Lost { frame_id: 2 }, t);
        let rep = r.poll(t, RTT).unwrap();
        assert_eq!(
            s.on_report(&rep, t),
            SenderAction::Invalidate { first_lost: 2 }
        );
        // Frame 3 left the encoder before the report landed, under the old epoch.
        assert!(!decoded(&r.on_event(frame(3, 0), t)));
        assert!(decoded(&r.on_event(recovery(4, s.epoch()), t)));
        assert!(r.poll(t + Duration::from_secs(1), RTT).is_none());
    }

    /// Found by the end-to-end test: without this, frames after a lost recovery
    /// frame reach the decoder with a missing reference.
    #[test]
    fn lost_recovery_frame_triggers_a_new_round() {
        let (mut r, mut s, t) = (
            ReceiverRecovery::new(0),
            SenderRecovery::new(),
            Instant::now(),
        );
        assert!(decoded(&r.on_event(frame(1, 0), t)));
        r.on_event(AssemblerEvent::Lost { frame_id: 2 }, t);
        let first = r.poll(t, RTT).unwrap();
        assert!(matches!(
            s.on_report(&first, t),
            SenderAction::Invalidate { first_lost: 2 }
        ));

        // Recovery frame 3 is lost too; frame 4 is in the new epoch but references it.
        r.on_event(AssemblerEvent::Lost { frame_id: 3 }, t);
        assert_eq!(r.on_event(frame(4, 1), t), Decision::Drop { frame_id: 4 });
        assert!(r.is_waiting());

        // The report goes out at once, carries the new epoch, and still names frame 2.
        let again = r
            .poll(t + Duration::from_millis(1), RTT)
            .expect("not paced");
        assert_eq!((again.client_epoch, again.first_lost_frame), (1, 2));
        assert_eq!(
            s.on_report(&again, t),
            SenderAction::Invalidate { first_lost: 2 }
        );
        assert_eq!(s.epoch(), 2);

        assert!(!decoded(&r.on_event(frame(5, 1), t)));
        assert!(decoded(&r.on_event(recovery(6, 2), t)));
        assert!(!r.is_waiting());
    }

    #[test]
    fn key_frame_ends_waiting_even_in_the_same_epoch() {
        let (mut r, t) = (ReceiverRecovery::new(0), Instant::now());
        r.on_event(AssemblerEvent::Lost { frame_id: 1 }, t);
        let key = AssemblerEvent::Complete(CompleteFrame {
            frame_id: 2,
            flags: flags::IDR,
            ref_epoch: 0,
            capture_ts_us: 0,
            data: vec![1],
            recovered_shards: 0,
        });
        assert!(decoded(&r.on_event(key, t)));
        assert!(!r.is_waiting());
    }
}
