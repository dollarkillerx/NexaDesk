//! Send-rate control (§4.9): delay-gradient and loss controllers, allocation,
//! encoder-update throttling and the pacer window.
//!
//! Sans-IO: the caller reports every sent packet and every `TransportFeedback`,
//! and always passes the current time in.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use nexa_wire::control::Mode;
use nexa_wire::datagram::{PacketStatus, TransportFeedback};

/// §4.9 【初值】 Normal state: multiplicative growth per second.
pub const GROWTH_PER_SEC: f64 = 0.08;
/// §4.9 【初值】 Overuse: `target = 0.85 × measured receive rate`.
pub const OVERUSE_BACKOFF: f64 = 0.85;
/// §4.9 【初值】 Number of frame-group samples in the delay regression.
pub const TREND_WINDOW: usize = 20;
/// §4.9 Loss above this cuts the target by `1 − 0.5 × loss`.
pub const LOSS_DECREASE_ABOVE: f64 = 0.10;
pub const LOSS_BACKOFF_GAIN: f64 = 0.5;
/// §4.9 Loss below this permits growth.
pub const LOSS_GROW_BELOW: f64 = 0.02;
/// §4.9 Sending below this fraction of target forbids any increase.
pub const APP_LIMITED_RATIO: f64 = 0.8;
/// §4.9 Startup probe: FEC padding brings the send rate to 2× target for 1 s.
pub const PROBE_DURATION: Duration = Duration::from_secs(1);
pub const PROBE_RATE_MULTIPLIER: f64 = 2.0;
/// §4.5 On a path switch the rate restarts at 70% of its previous value.
pub const PATH_SWITCH_BACKOFF: f64 = 0.7;

pub const AUDIO_BPS: u64 = 128_000;
pub const VIDEO_HEADROOM: f64 = 0.95;
pub const FEC_LOSS_GAIN: f64 = 2.0;
pub const FEC_MAX: f64 = 0.25;
pub const FEC_MARGIN_GAME: f64 = 0.05;
pub const FEC_MARGIN_OFFICE: f64 = 0.02;
/// §4.9 gives no margin for Balanced; midpoint of the two it does give.
pub const FEC_MARGIN_BALANCED: f64 = 0.035;

pub const ENCODER_MIN_INTERVAL: Duration = Duration::from_millis(100);
pub const ENCODER_MIN_CHANGE: f64 = 0.05;

pub const PACE_TARGET_FACTOR: f64 = 1.5;
pub const PACE_CAPACITY_FACTOR: f64 = 0.8;
pub const PACE_MAX_FRAME_FRACTION: f64 = 0.5;

// The values below are not given by §4.9; they follow draft-ietf-rmcat-gcc
// where it has an equivalent and were otherwise set against the simulator.

/// Filter on the accumulated delay before regression.
const TREND_SMOOTHING: f64 = 0.9;
/// Scales slope × window into threshold units (ms). Large enough that a few
/// percent of overshoot crosses the minimum threshold.
const TREND_GAIN: f64 = 16.0;
const THRESHOLD_INITIAL: f64 = 12.5;
const THRESHOLD_MIN: f64 = 6.0;
const THRESHOLD_MAX: f64 = 600.0;
const THRESHOLD_K_UP: f64 = 0.0087;
const THRESHOLD_K_DOWN: f64 = 0.039;
const THRESHOLD_OUTLIER_MARGIN: f64 = 15.0;
const THRESHOLD_MAX_STEP_MS: f64 = 100.0;
/// Overuse must persist this long, and across more than one sample.
const OVERUSE_MIN_MS: f64 = 10.0;
/// A pause this long makes the previous delay history meaningless.
const GROUP_GAP_RESET_US: i64 = 2_000_000;

/// A cut is applied at most this often: the detector keeps reporting Overuse
/// while the queue it saw drains, and each repeat would cut again.
const DECREASE_INTERVAL: Duration = Duration::from_millis(500);
/// Within this fraction of the last congestion point, growth is additive.
const NEAR_CONGESTION_BAND: f64 = 0.10;
/// Additive slope, as a fraction of the last congestion point per second.
const ADDITIVE_PER_SEC: f64 = 0.01;
/// Bounds growth across a feedback gap.
const MAX_UPDATE_STEP: Duration = Duration::from_millis(200);

const RECV_WINDOW_US: i64 = 250_000;
const SEND_WINDOW_US: i64 = 500_000;
const RECV_MIN_SPAN_US: i64 = 50_000;

/// Loss is judged over this window, and acted on at most this often.
const LOSS_WINDOW: Duration = Duration::from_millis(500);
const LOSS_MIN_PACKETS: u64 = 20;
const LOSS_EWMA_ALPHA: f64 = 0.3;

/// A probe that survived without congestion lifts the target to this share of
/// what the receiver measured.
const PROBE_ACCEPT_FACTOR: f64 = 0.85;

/// Frame trains shorter than this say nothing useful about the bottleneck at
/// the 250 µs feedback resolution.
const CAPACITY_MIN_PACKETS: u32 = 8;
const CAPACITY_MIN_SPAN_US: i64 = 1_000;

const TICK_US: i64 = 250;
const HISTORY_MAX_AGE: Duration = Duration::from_secs(2);
const HISTORY_MAX_SEQ_GAP: u32 = 4096;

pub const fn fec_margin(mode: Mode) -> f64 {
    match mode {
        Mode::Game => FEC_MARGIN_GAME,
        Mode::Office => FEC_MARGIN_OFFICE,
        Mode::Balanced => FEC_MARGIN_BALANCED,
    }
}

/// §4.9 `fec = clamp(2 × loss_ewma + margin, 0, 0.25)`.
pub fn fec_ratio(loss_ewma: f64, mode: Mode) -> f64 {
    (FEC_LOSS_GAIN * loss_ewma + fec_margin(mode)).clamp(0.0, FEC_MAX)
}

/// §4.9 `pace_rate = max(1.5 × target, 0.8 × capacity)`, or `1.5 × target`
/// when the capacity is unknown.
pub fn pace_rate_bps(target_bps: u64, capacity_bps: Option<u64>) -> u64 {
    let by_target = target_bps as f64 * PACE_TARGET_FACTOR;
    let by_capacity = capacity_bps.map_or(0.0, |c| c as f64 * PACE_CAPACITY_FACTOR);
    by_target.max(by_capacity) as u64
}

/// §4.9 Time over which to spread one frame: `frame_bytes × 8 ÷ pace_rate`,
/// never more than half a frame interval.
pub fn pacing_window(
    frame_bytes: u64,
    target_bps: u64,
    capacity_bps: Option<u64>,
    frame_interval: Duration,
) -> Duration {
    let ceiling = frame_interval.mul_f64(PACE_MAX_FRAME_FRACTION);
    let rate = pace_rate_bps(target_bps, capacity_bps);
    if rate == 0 {
        return ceiling;
    }
    Duration::from_secs_f64((frame_bytes as f64 * 8.0 / rate as f64).min(ceiling.as_secs_f64()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelayState {
    Overuse,
    Normal,
    Underuse,
}

/// §2: receives each new allocation; the assembly layer forwards it.
pub trait RateObserver: Send + Sync {
    fn on_rate(&self, r: RateAllocation);
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateAllocation {
    pub target_bps: u64,
    pub video_bps: u64,
    pub audio_bps: u64,
    /// Repair overhead as a fraction of the video rate.
    pub fec_ratio: f64,
    pub bulk_bps: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct RateConfig {
    pub initial_bps: u64,
    pub min_bps: u64,
    pub max_bps: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct SentPacket {
    pub tx_seq: u32,
    /// The frame this packet carries, FEC repair included. `None` for audio,
    /// which counts for loss and rate but is not part of a delay group.
    pub frame_id: Option<u32>,
    pub bytes: u32,
}

/// §4.9 Encoder updates: at least 100 ms apart and at least a 5% change.
///
/// Change is measured against the last value applied, so slow drift still
/// gets through once it adds up.
#[derive(Debug, Default)]
pub struct EncoderThrottle {
    applied: Option<(u64, Instant)>,
}

impl EncoderThrottle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the bitrate to program, or `None` to leave the encoder alone.
    pub fn update(&mut self, desired_bps: u64, now: Instant) -> Option<u64> {
        if let Some((applied, at)) = self.applied {
            let change = desired_bps.abs_diff(applied) as f64 / applied.max(1) as f64;
            if now.saturating_duration_since(at) < ENCODER_MIN_INTERVAL
                || change < ENCODER_MIN_CHANGE
            {
                return None;
            }
        }
        self.applied = Some((desired_bps, now));
        Some(desired_bps)
    }
}

/// Sliding-window byte counter on a microsecond timeline.
#[derive(Debug)]
struct RateMeter {
    window_us: i64,
    samples: VecDeque<(i64, u64)>,
    total: u64,
    first_us: Option<i64>,
    min_span_us: i64,
}

impl RateMeter {
    fn new(window_us: i64, min_span_us: i64) -> Self {
        Self {
            window_us,
            samples: VecDeque::new(),
            total: 0,
            first_us: None,
            min_span_us,
        }
    }

    fn add(&mut self, at_us: i64, bytes: u64) {
        self.first_us.get_or_insert(at_us);
        self.samples.push_back((at_us, bytes));
        self.total += bytes;
        self.expire(at_us);
    }

    fn expire(&mut self, now_us: i64) {
        while let Some(&(at, bytes)) = self.samples.front() {
            if now_us - at < self.window_us {
                break;
            }
            self.samples.pop_front();
            self.total -= bytes;
        }
    }

    /// Bits per second over the window, or `None` until enough time has passed.
    fn rate_bps(&mut self, now_us: i64) -> Option<f64> {
        self.expire(now_us);
        let span = now_us - self.first_us?;
        if span < self.min_span_us {
            return None;
        }
        Some(self.total as f64 * 8e6 / span.min(self.window_us) as f64)
    }

    fn clear(&mut self) {
        self.samples.clear();
        self.total = 0;
        self.first_us = None;
    }
}

/// §4.9 delay detector: regress accumulated delay across frame groups and
/// compare the slope with an adaptive threshold.
#[derive(Debug)]
struct TrendDetector {
    accumulated: f64,
    smoothed: f64,
    window: VecDeque<(f64, f64)>,
    threshold: f64,
    last_arrival_ms: Option<f64>,
    overuse_ms: f64,
    overuse_count: u32,
    state: DelayState,
}

impl TrendDetector {
    fn new() -> Self {
        Self {
            accumulated: 0.0,
            smoothed: 0.0,
            window: VecDeque::with_capacity(TREND_WINDOW + 1),
            threshold: THRESHOLD_INITIAL,
            last_arrival_ms: None,
            overuse_ms: 0.0,
            overuse_count: 0,
            state: DelayState::Normal,
        }
    }

    /// Forgets the delay history but keeps the adapted threshold.
    fn restart(&mut self) {
        self.accumulated = 0.0;
        self.smoothed = 0.0;
        self.window.clear();
        self.last_arrival_ms = None;
        self.overuse_ms = 0.0;
        self.overuse_count = 0;
        self.state = DelayState::Normal;
    }

    fn update(&mut self, send_delta_ms: f64, arrival_delta_ms: f64, arrival_ms: f64) {
        self.accumulated += arrival_delta_ms - send_delta_ms;
        self.smoothed =
            TREND_SMOOTHING * self.smoothed + (1.0 - TREND_SMOOTHING) * self.accumulated;
        self.window.push_back((arrival_ms, self.smoothed));
        if self.window.len() > TREND_WINDOW {
            self.window.pop_front();
        }
        let step_ms = self
            .last_arrival_ms
            .map_or(0.0, |last| (arrival_ms - last).max(0.0));
        self.last_arrival_ms = Some(arrival_ms);
        if self.window.len() < TREND_WINDOW {
            return;
        }
        let modified = self.slope() * TREND_WINDOW as f64 * TREND_GAIN;
        self.classify(modified, step_ms);
        self.adapt_threshold(modified, step_ms);
    }

    fn slope(&self) -> f64 {
        let n = self.window.len() as f64;
        let origin = self.window.front().map_or(0.0, |&(x, _)| x);
        let (sum_x, sum_y) = self
            .window
            .iter()
            .fold((0.0, 0.0), |(sx, sy), &(x, y)| (sx + x - origin, sy + y));
        let (mean_x, mean_y) = (sum_x / n, sum_y / n);
        let (mut num, mut den) = (0.0, 0.0);
        for &(x, y) in &self.window {
            let dx = x - origin - mean_x;
            num += dx * (y - mean_y);
            den += dx * dx;
        }
        if den > 0.0 { num / den } else { 0.0 }
    }

    fn classify(&mut self, modified: f64, step_ms: f64) {
        if modified > self.threshold {
            self.overuse_ms += step_ms;
            self.overuse_count += 1;
            if self.overuse_ms >= OVERUSE_MIN_MS && self.overuse_count > 1 {
                self.state = DelayState::Overuse;
            }
            return;
        }
        self.overuse_ms = 0.0;
        self.overuse_count = 0;
        self.state = if modified < -self.threshold {
            DelayState::Underuse
        } else {
            DelayState::Normal
        };
    }

    fn adapt_threshold(&mut self, modified: f64, step_ms: f64) {
        let magnitude = modified.abs();
        if magnitude > self.threshold + THRESHOLD_OUTLIER_MARGIN {
            return;
        }
        let k = if magnitude < self.threshold {
            THRESHOLD_K_DOWN
        } else {
            THRESHOLD_K_UP
        };
        self.threshold += k * step_ms.min(THRESHOLD_MAX_STEP_MS) * (magnitude - self.threshold);
        self.threshold = self.threshold.clamp(THRESHOLD_MIN, THRESHOLD_MAX);
    }
}

#[derive(Debug, Clone, Copy)]
struct SentRecord {
    frame_id: Option<u32>,
    bytes: u32,
    sent_at: Instant,
    reported_lost: bool,
}

/// Send records indexed by `tx_seq`, which wraps.
#[derive(Debug, Default)]
struct SendHistory {
    first_seq: u32,
    records: VecDeque<Option<SentRecord>>,
}

impl SendHistory {
    fn push(&mut self, seq: u32, rec: SentRecord) {
        if self.records.is_empty() {
            self.first_seq = seq;
        }
        let distance = seq.wrapping_sub(self.first_seq);
        let offset = distance as usize;
        // Behind the window, or already recorded: not a new packet.
        if distance > u32::MAX / 2 || offset < self.records.len() {
            return;
        }
        let gap = offset - self.records.len();
        if gap > HISTORY_MAX_SEQ_GAP as usize {
            self.records.clear();
            self.first_seq = seq;
        } else {
            self.records.extend(std::iter::repeat_n(None, gap));
        }
        self.records.push_back(Some(rec));
    }

    fn get_mut(&mut self, seq: u32) -> Option<&mut SentRecord> {
        self.records
            .get_mut(seq.wrapping_sub(self.first_seq) as usize)?
            .as_mut()
    }

    fn take(&mut self, seq: u32) -> Option<SentRecord> {
        self.records
            .get_mut(seq.wrapping_sub(self.first_seq) as usize)?
            .take()
    }

    fn expire(&mut self, now: Instant) {
        while let Some(front) = self.records.front() {
            if front.is_some_and(|r| now.saturating_duration_since(r.sent_at) < HISTORY_MAX_AGE) {
                break;
            }
            self.records.pop_front();
            self.first_seq = self.first_seq.wrapping_add(1);
        }
    }
}

/// One frame's packets as seen in feedback. Timing uses the last *received*
/// packet, so a lost tail cannot pair one packet's send time with another's
/// arrival.
#[derive(Debug, Clone, Copy)]
struct Group {
    frame_id: u32,
    last_sent: Instant,
    first_arrival: i64,
    last_arrival: i64,
    bytes_after_first: u64,
    packets: u32,
}

impl Group {
    fn start(frame_id: u32, rec: &SentRecord, arrival: i64) -> Self {
        Self {
            frame_id,
            last_sent: rec.sent_at,
            first_arrival: arrival,
            last_arrival: arrival,
            bytes_after_first: 0,
            packets: 1,
        }
    }

    fn extend(&mut self, rec: &SentRecord, arrival: i64) {
        self.last_sent = self.last_sent.max(rec.sent_at);
        self.last_arrival = self.last_arrival.max(arrival);
        self.bytes_after_first += u64::from(rec.bytes);
        self.packets += 1;
    }

    /// Dispersion of the frame train at the receiver.
    fn train_rate_bps(&self) -> Option<f64> {
        let span = self.last_arrival - self.first_arrival;
        (self.packets >= CAPACITY_MIN_PACKETS && span >= CAPACITY_MIN_SPAN_US)
            .then(|| self.bytes_after_first as f64 * 8e6 / span as f64)
    }
}

/// The GCC-style sender of §4.9, run by `media-tx` on the host.
#[derive(Debug)]
pub struct RateController {
    cfg: RateConfig,
    mode: Mode,
    epoch: Instant,
    target: f64,
    history: SendHistory,
    detector: TrendDetector,
    current_group: Option<Group>,
    previous_group: Option<Group>,
    /// Latest arrival clock reading, unwrapped from the 32-bit wire value.
    arrival_clock: Option<(u32, i64)>,
    newest_arrival: i64,
    recv_meter: RateMeter,
    send_meter: RateMeter,
    video_meter: RateMeter,
    loss_window: VecDeque<(Instant, u64, u64)>,
    loss_ewma: f64,
    last_loss_tick: Option<Instant>,
    capacity: Option<f64>,
    probe_ends: Option<Instant>,
    probe_congested: bool,
    last_update: Option<Instant>,
    last_decrease: Option<Instant>,
    /// Receive rate at the last Overuse, which estimates the bottleneck.
    congestion_point: Option<f64>,
}

impl RateController {
    pub fn new(cfg: RateConfig, mode: Mode, now: Instant) -> Self {
        let cfg = RateConfig {
            min_bps: cfg.min_bps.max(1),
            max_bps: cfg.max_bps.max(cfg.min_bps.max(1)),
            ..cfg
        };
        Self {
            cfg,
            mode,
            epoch: now,
            target: cfg.initial_bps.clamp(cfg.min_bps, cfg.max_bps) as f64,
            history: SendHistory::default(),
            detector: TrendDetector::new(),
            current_group: None,
            previous_group: None,
            arrival_clock: None,
            newest_arrival: 0,
            recv_meter: RateMeter::new(RECV_WINDOW_US, RECV_MIN_SPAN_US),
            // A partly filled window overstates the rate, which would wrongly
            // clear the app-limited check at session start.
            send_meter: RateMeter::new(SEND_WINDOW_US, SEND_WINDOW_US),
            video_meter: RateMeter::new(SEND_WINDOW_US, RECV_MIN_SPAN_US),
            loss_window: VecDeque::new(),
            loss_ewma: 0.0,
            last_loss_tick: None,
            capacity: None,
            probe_ends: Some(now + PROBE_DURATION),
            probe_congested: false,
            last_update: None,
            last_decrease: None,
            congestion_point: None,
        }
    }

    pub fn target_bps(&self) -> u64 {
        self.target as u64
    }

    pub fn delay_state(&self) -> DelayState {
        self.detector.state
    }

    pub fn loss_ewma(&self) -> f64 {
        self.loss_ewma
    }

    /// Link-capacity estimate, or `None` until a usable frame train is seen.
    pub fn link_capacity_bps(&self) -> Option<u64> {
        self.capacity.map(|c| c as u64)
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    /// Extra FEC padding to send right now for the startup probe.
    pub fn probe_padding_bps(&self, now: Instant) -> u64 {
        match self.probe_ends {
            Some(end) if now < end => (self.target * (PROBE_RATE_MULTIPLIER - 1.0)) as u64,
            _ => 0,
        }
    }

    pub fn on_packet_sent(&mut self, p: SentPacket, now: Instant) {
        let at = self.micros(now);
        self.history.expire(now);
        self.history.push(
            p.tx_seq,
            SentRecord {
                frame_id: p.frame_id,
                bytes: p.bytes,
                sent_at: now,
                reported_lost: false,
            },
        );
        self.send_meter.add(at, u64::from(p.bytes));
        if p.frame_id.is_some() {
            self.video_meter.add(at, u64::from(p.bytes));
        }
    }

    /// Bulk stream bytes handed to quinn. They are real sending for the
    /// app-limited rule, though only media packets produce feedback.
    pub fn on_bulk_sent(&mut self, bytes: u64, now: Instant) {
        let at = self.micros(now);
        self.send_meter.add(at, bytes);
    }

    pub fn on_feedback(&mut self, fb: &TransportFeedback, now: Instant) {
        let mut arrival = self.unwrap_arrival(fb.base_arrival_us);
        let (mut received, mut lost) = (0u64, 0u64);
        for (i, status) in fb.statuses.iter().enumerate() {
            let seq = fb.base_tx_seq.wrapping_add(i as u32);
            match *status {
                PacketStatus::NotReceived => {
                    if let Some(rec) = self.history.get_mut(seq).filter(|r| !r.reported_lost) {
                        rec.reported_lost = true;
                        lost += 1;
                    }
                }
                PacketStatus::Received { delta_ticks } => {
                    arrival += i64::from(delta_ticks) * TICK_US;
                    if let Some(rec) = self.history.take(seq) {
                        received += 1;
                        self.on_received(&rec, arrival);
                    }
                }
            }
        }
        self.record_loss(now, received, lost);
        self.update_target(now);
    }

    /// §4.5 side effect: the new path is unknown, so drop everything measured
    /// on the old one and restart from 70% of the previous rate.
    pub fn on_path_switch(&mut self, now: Instant) {
        self.target = (self.target * PATH_SWITCH_BACKOFF)
            .clamp(self.cfg.min_bps as f64, self.cfg.max_bps as f64);
        self.detector.restart();
        self.current_group = None;
        self.previous_group = None;
        self.recv_meter.clear();
        self.loss_window.clear();
        self.capacity = None;
        self.congestion_point = None;
        self.probe_ends = None;
        self.last_decrease = None;
        self.last_update = Some(now);
    }

    /// §4.9 allocation for the current target.
    pub fn allocation(&mut self, now: Instant) -> RateAllocation {
        let audio = AUDIO_BPS as f64;
        let fec = fec_ratio(self.loss_ewma, self.mode);
        let spendable = (self.target - audio).max(0.0);
        let video = spendable / (1.0 + fec) * VIDEO_HEADROOM;
        // The measured rate includes repair packets, which use the link too.
        let at = self.micros(now);
        let actual_video = self.video_meter.rate_bps(at).unwrap_or(0.0);
        let bulk = if self.mode == Mode::Game {
            0.0
        } else {
            (spendable - actual_video).max(0.0)
        };
        RateAllocation {
            target_bps: self.target_bps(),
            video_bps: video as u64,
            audio_bps: AUDIO_BPS,
            fec_ratio: fec,
            bulk_bps: bulk as u64,
        }
    }

    /// §4.9 pacer window for a frame of `frame_bytes`, repair packets included.
    pub fn pacing_window(&self, frame_bytes: u64, frame_interval: Duration) -> Duration {
        pacing_window(
            frame_bytes,
            self.target_bps(),
            self.link_capacity_bps(),
            frame_interval,
        )
    }

    fn micros(&self, now: Instant) -> i64 {
        now.saturating_duration_since(self.epoch).as_micros() as i64
    }

    fn unwrap_arrival(&mut self, wire: u32) -> i64 {
        let extended = match self.arrival_clock {
            None => i64::from(wire),
            Some((last_wire, last)) => last + i64::from(wire.wrapping_sub(last_wire) as i32),
        };
        self.arrival_clock = Some((wire, extended));
        extended
    }

    fn on_received(&mut self, rec: &SentRecord, arrival: i64) {
        self.recv_meter.add(arrival, u64::from(rec.bytes));
        self.newest_arrival = self.newest_arrival.max(arrival);
        let Some(frame) = rec.frame_id else { return };
        if let Some(group) = self.current_group.as_mut().filter(|g| g.frame_id == frame) {
            group.extend(rec, arrival);
            return;
        }
        if let Some(finished) = self
            .current_group
            .replace(Group::start(frame, rec, arrival))
        {
            self.finish_group(finished);
        }
    }

    fn finish_group(&mut self, group: Group) {
        if let Some(rate) = group.train_rate_bps() {
            self.capacity = Some(self.capacity.map_or(rate, |c| c.max(rate)));
        }
        if let Some(prev) = self.previous_group.replace(group) {
            let arrival_delta = group.last_arrival - prev.last_arrival;
            if arrival_delta > GROUP_GAP_RESET_US {
                self.detector.restart();
            } else if let (Some(send_delta), true) = (
                group.last_sent.checked_duration_since(prev.last_sent),
                arrival_delta >= 0,
            ) {
                self.detector.update(
                    send_delta.as_secs_f64() * 1e3,
                    arrival_delta as f64 / 1e3,
                    group.last_arrival as f64 / 1e3,
                );
            }
        }
    }

    fn record_loss(&mut self, now: Instant, received: u64, lost: u64) {
        self.loss_window.push_back((now, received, lost));
        while self
            .loss_window
            .front()
            .is_some_and(|&(t, ..)| now.saturating_duration_since(t) >= LOSS_WINDOW)
        {
            self.loss_window.pop_front();
        }
    }

    fn loss_fraction(&self) -> Option<f64> {
        let (received, lost) = self
            .loss_window
            .iter()
            .fold((0, 0), |(r, l), &(_, rx, ls)| (r + rx, l + ls));
        (received + lost >= LOSS_MIN_PACKETS).then(|| lost as f64 / (received + lost) as f64)
    }

    fn grown(&self, dt: f64) -> f64 {
        let near = self
            .congestion_point
            .filter(|&point| (self.target - point).abs() <= point * NEAR_CONGESTION_BAND);
        match near {
            Some(point) => self.target + point * ADDITIVE_PER_SEC * dt,
            None => self.target * (1.0 + GROWTH_PER_SEC).powf(dt),
        }
    }

    fn update_target(&mut self, now: Instant) {
        let dt = self.last_update.map_or(0.0, |last| {
            now.saturating_duration_since(last)
                .min(MAX_UPDATE_STEP)
                .as_secs_f64()
        });
        self.last_update = Some(now);
        let at = self.micros(now);
        let recv = self.recv_meter.rate_bps(self.newest_arrival);
        let app_limited =
            self.send_meter.rate_bps(at).unwrap_or(0.0) < APP_LIMITED_RATIO * self.target;
        let grown = if app_limited {
            self.target
        } else {
            self.grown(dt)
        };

        let delay_based = self.delay_based(recv, grown, now);
        let loss_based = self.loss_based(grown, now);
        self.target = delay_based
            .min(loss_based)
            .clamp(self.cfg.min_bps as f64, self.cfg.max_bps as f64);
        self.finish_probe(recv, now);
    }

    fn delay_based(&mut self, recv: Option<f64>, grown: f64, now: Instant) -> f64 {
        match self.detector.state {
            DelayState::Normal => grown,
            DelayState::Underuse => self.target,
            DelayState::Overuse => {
                self.probe_congested = true;
                let may_cut = self
                    .last_decrease
                    .is_none_or(|t| now.saturating_duration_since(t) >= DECREASE_INTERVAL);
                match recv {
                    Some(recv) if may_cut => {
                        self.last_decrease = Some(now);
                        self.congestion_point = Some(recv);
                        // Under Overuse the receive rate is the bottleneck rate.
                        self.capacity = self.capacity.map(|c| c.min(recv));
                        self.target.min(OVERUSE_BACKOFF * recv)
                    }
                    _ => self.target,
                }
            }
        }
    }

    fn loss_based(&mut self, grown: f64, now: Instant) -> f64 {
        let Some(loss) = self.loss_fraction() else {
            return grown;
        };
        let tick_due = self
            .last_loss_tick
            .is_none_or(|t| now.saturating_duration_since(t) >= LOSS_WINDOW);
        if tick_due {
            self.last_loss_tick = Some(now);
            self.loss_ewma += LOSS_EWMA_ALPHA * (loss - self.loss_ewma);
        }
        if loss > LOSS_DECREASE_ABOVE {
            self.probe_congested = true;
            return if tick_due {
                self.target * (1.0 - LOSS_BACKOFF_GAIN * loss)
            } else {
                self.target
            };
        }
        if loss < LOSS_GROW_BELOW {
            grown
        } else {
            self.target
        }
    }

    /// A clean probe is evidence the link carries what the receiver measured.
    fn finish_probe(&mut self, recv: Option<f64>, now: Instant) {
        let Some(end) = self.probe_ends else { return };
        if self.probe_congested {
            self.probe_ends = None;
            return;
        }
        if now < end {
            return;
        }
        self.probe_ends = None;
        if let Some(recv) = recv {
            self.capacity = Some(self.capacity.map_or(recv, |c| c.max(recv)));
            self.target = self
                .target
                .max(PROBE_ACCEPT_FACTOR * recv)
                .min(self.cfg.max_bps as f64);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MBPS: f64 = 1e6;
    const PACKET_BYTES: u32 = 1200;
    /// Close to the 32-bit wrap, so every simulation crosses it.
    const ARRIVAL_CLOCK_OFFSET: u64 = u32::MAX as u64 - 2_000_000;
    const FEEDBACK_INTERVAL_US: u64 = 25_000;
    const FEEDBACK_DELAY_US: u64 = 10_000;
    const STEP_US: u64 = 100;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn unit(&mut self) -> f64 {
            (self.next() >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    /// Drop-tail FIFO bottleneck.
    struct Bottleneck {
        busy_until_us: u64,
        one_way_us: u64,
        max_queue_us: u64,
        random_loss: f64,
        rng: Rng,
    }

    impl Bottleneck {
        /// Returns `(arrival_us, queueing_us)`, or `None` if dropped.
        fn transit(&mut self, t_us: u64, bytes: u32, capacity_bps: f64) -> Option<(u64, u64)> {
            if self.rng.unit() < self.random_loss {
                return None;
            }
            let queueing = self.busy_until_us.saturating_sub(t_us);
            if queueing > self.max_queue_us {
                return None;
            }
            let serialization = (f64::from(bytes) * 8e6 / capacity_bps) as u64;
            self.busy_until_us = t_us.max(self.busy_until_us) + serialization;
            Some((self.busy_until_us + self.one_way_us, queueing))
        }
    }

    #[derive(Clone, Copy)]
    struct Scenario {
        secs: f64,
        fps: f64,
        initial_bps: f64,
        capacity: fn(f64) -> f64,
        random_loss: f64,
        app_cap_bps: Option<f64>,
    }

    impl Default for Scenario {
        fn default() -> Self {
            Self {
                secs: 60.0,
                fps: 60.0,
                initial_bps: 5.0 * MBPS,
                capacity: |_| 20.0 * MBPS,
                random_loss: 0.0,
                app_cap_bps: None,
            }
        }
    }

    #[derive(Default)]
    struct Trace {
        /// (seconds, target bps, capacity estimate bps)
        targets: Vec<(f64, f64, Option<f64>)>,
        /// (arrival seconds, queueing ms) per delivered packet.
        queue: Vec<(f64, f64)>,
        /// (arrival seconds, bytes) per delivered packet.
        delivered: Vec<(f64, u32)>,
    }

    impl Trace {
        fn target_at(&self, t: f64) -> f64 {
            self.targets
                .iter()
                .rev()
                .find(|s| s.0 <= t)
                .map_or(0.0, |s| s.1)
        }

        fn first_at_or_above(&self, from: f64, level: f64) -> Option<f64> {
            self.targets
                .iter()
                .find(|s| s.0 >= from && s.1 >= level)
                .map(|s| s.0)
        }

        fn first_at_or_below(&self, from: f64, level: f64) -> Option<f64> {
            self.targets
                .iter()
                .find(|s| s.0 >= from && s.1 <= level)
                .map(|s| s.0)
        }

        fn max_target(&self, from: f64, to: f64) -> f64 {
            self.targets
                .iter()
                .filter(|s| s.0 >= from && s.0 < to)
                .map(|s| s.1)
                .fold(0.0, f64::max)
        }

        fn mean_target(&self, from: f64, to: f64) -> f64 {
            let v: Vec<f64> = self
                .targets
                .iter()
                .filter(|s| s.0 >= from && s.0 < to)
                .map(|s| s.1)
                .collect();
            v.iter().sum::<f64>() / v.len() as f64
        }

        fn utilization(&self, from: f64, to: f64, capacity_bps: f64) -> f64 {
            let bytes: u64 = self
                .delivered
                .iter()
                .filter(|d| d.0 >= from && d.0 < to)
                .map(|d| u64::from(d.1))
                .sum();
            bytes as f64 * 8.0 / ((to - from) * capacity_bps)
        }

        /// (mean, p95, max) queueing in ms over the interval.
        fn first_queue_below(&self, from: f64, level_ms: f64) -> Option<f64> {
            self.queue
                .iter()
                .find(|q| q.0 >= from && q.1 < level_ms)
                .map(|q| q.0)
        }

        fn queue_stats(&self, from: f64, to: f64) -> (f64, f64, f64) {
            let mut v: Vec<f64> = self
                .queue
                .iter()
                .filter(|q| q.0 >= from && q.0 < to)
                .map(|q| q.1)
                .collect();
            v.sort_by(f64::total_cmp);
            let mean = v.iter().sum::<f64>() / v.len().max(1) as f64;
            (
                mean,
                v.get(v.len() * 95 / 100).copied().unwrap_or(0.0),
                v.last().copied().unwrap_or(0.0),
            )
        }
    }

    /// Discrete-event loop: an encoder, a pacer, the bottleneck, and a receiver
    /// that reports every 25 ms in 250 µs ticks.
    fn simulate(s: Scenario) -> Trace {
        let t0 = Instant::now();
        let at = |us: u64| t0 + Duration::from_micros(us);
        let cfg = RateConfig {
            initial_bps: s.initial_bps as u64,
            min_bps: 300_000,
            max_bps: 1_000_000_000,
        };
        let mut ctl = RateController::new(cfg, Mode::Office, t0);
        let mut link = Bottleneck {
            busy_until_us: 0,
            one_way_us: 15_000,
            max_queue_us: 300_000,
            random_loss: s.random_loss,
            rng: Rng(0x1234_5678_9ABC_DEF1),
        };
        let mut frame_noise = Rng(0xFEED_FACE_1234_5678);
        let mut trace = Trace::default();

        let mut tx: VecDeque<(u64, u32, Option<u32>)> = VecDeque::new();
        let mut rx: VecDeque<(u32, Option<u64>)> = VecDeque::new();
        let mut reports: VecDeque<(u64, TransportFeedback)> = VecDeque::new();
        let (mut seq, mut frame, mut fb_seq) = (0xFFFF_F000u32, 0u32, 0u16);
        let frame_interval_us = (1e6 / s.fps) as u64;
        let (mut next_frame, mut next_audio, mut next_report, mut next_sample) =
            (0u64, 0u64, FEEDBACK_INTERVAL_US, 0u64);
        let end_us = (s.secs * 1e6) as u64;

        let mut now = 0u64;
        while now < end_us {
            let wall = at(now);
            if now >= next_frame {
                next_frame += frame_interval_us;
                let alloc = ctl.allocation(wall);
                let mut video = alloc.video_bps as f64;
                if let Some(cap) = s.app_cap_bps {
                    video = video.min(cap);
                }
                let per_frame = video / 8.0 / s.fps * (0.8 + 0.4 * frame_noise.unit());
                let padding = ctl.probe_padding_bps(wall) as f64 / 8.0 / s.fps;
                let wire = per_frame * (1.0 + alloc.fec_ratio) + padding;
                let packets = (wire / f64::from(PACKET_BYTES)).ceil().max(1.0) as u64;
                let window = ctl
                    .pacing_window(wire as u64, Duration::from_micros(frame_interval_us))
                    .as_micros() as u64;
                for i in 0..packets {
                    tx.push_back((now + window * i / packets, PACKET_BYTES, Some(frame)));
                }
                frame = frame.wrapping_add(1);
            }
            if now >= next_audio {
                next_audio += 20_000;
                tx.push_back((now, 320, None));
            }
            // Send everything due, in time order.
            let mut i = 0;
            while i < tx.len() {
                let (when, bytes, group) = tx[i];
                if when <= now {
                    tx.remove(i);
                    ctl.on_packet_sent(
                        SentPacket {
                            tx_seq: seq,
                            frame_id: group,
                            bytes,
                        },
                        wall,
                    );
                    let outcome = link.transit(now, bytes, (s.capacity)(now as f64 / 1e6));
                    if let Some((arrival, queueing)) = outcome {
                        trace
                            .queue
                            .push((arrival as f64 / 1e6, queueing as f64 / 1e3));
                        trace.delivered.push((arrival as f64 / 1e6, bytes));
                    }
                    rx.push_back((seq, outcome.map(|o| o.0)));
                    seq = seq.wrapping_add(1);
                } else {
                    i += 1;
                }
            }
            if now >= next_report {
                next_report += FEEDBACK_INTERVAL_US;
                if let Some(fb) = build_feedback(&mut rx, now, &mut fb_seq) {
                    reports.push_back((now + FEEDBACK_DELAY_US, fb));
                }
            }
            while reports.front().is_some_and(|r| r.0 <= now) {
                let (_, fb) = reports.pop_front().unwrap();
                let mut wire = Vec::new();
                fb.encode(&mut wire);
                ctl.on_feedback(&TransportFeedback::parse(&wire).unwrap(), wall);
            }
            if now >= next_sample {
                next_sample += 100_000;
                trace.targets.push((
                    now as f64 / 1e6,
                    ctl.target_bps() as f64,
                    ctl.link_capacity_bps().map(|c| c as f64),
                ));
            }
            now += STEP_US;
        }
        trace
    }

    fn build_feedback(
        rx: &mut VecDeque<(u32, Option<u64>)>,
        now: u64,
        fb_seq: &mut u16,
    ) -> Option<TransportFeedback> {
        // Packets past the last arrival are still in flight or indistinguishable from it.
        let last = rx.iter().rposition(|p| p.1.is_some_and(|a| a <= now))?;
        let reported: Vec<_> = rx.drain(..=last).collect();
        let base_tick = reported.iter().find_map(|p| p.1)? / 250;
        let mut previous_tick = base_tick;
        let statuses = reported
            .iter()
            .map(|p| match p.1 {
                None => PacketStatus::NotReceived,
                Some(a) if a > now => PacketStatus::NotReceived,
                Some(a) => {
                    let tick = a / 250;
                    let delta = (tick - previous_tick) as u16;
                    previous_tick = tick;
                    PacketStatus::Received { delta_ticks: delta }
                }
            })
            .collect();
        *fb_seq = fb_seq.wrapping_add(1);
        Some(TransportFeedback {
            fb_seq: *fb_seq,
            base_tx_seq: reported[0].0,
            base_arrival_us: (ARRIVAL_CLOCK_OFFSET + base_tick * 250) as u32,
            statuses,
        })
    }

    fn report(name: &str, trace: &Trace, from: f64, to: f64, capacity: f64) {
        let (mean, p95, max) = trace.queue_stats(from, to);
        println!(
            "[{name}] window {from}-{to}s: target mean {:.2} Mbps ({:.1}% of C), utilization {:.1}%, queue mean {mean:.2} ms p95 {p95:.2} ms max {max:.2} ms",
            trace.mean_target(from, to) / MBPS,
            trace.mean_target(from, to) / capacity * 100.0,
            trace.utilization(from, to, capacity) * 100.0,
        );
    }

    #[test]
    fn converges_near_capacity_without_standing_queue() {
        let capacity = 20.0 * MBPS;
        let trace = simulate(Scenario {
            secs: 90.0,
            ..Default::default()
        });
        let convergence = trace
            .first_at_or_above(0.0, 0.85 * capacity)
            .expect("never converged");
        println!(
            "[converge 20M from 5M] reached 85% of C at {convergence:.1}s; capacity estimate {:?}",
            trace.targets.last().unwrap().2
        );
        report("converge 20M from 5M", &trace, 30.0, 90.0, capacity);

        assert!(convergence < 25.0, "converged at {convergence}");
        let mean_target = trace.mean_target(30.0, 90.0);
        assert!(
            (0.80 * capacity..1.08 * capacity).contains(&mean_target),
            "mean target {mean_target}"
        );
        assert!(trace.utilization(30.0, 90.0, capacity) > 0.80);
        let (mean, p95, _) = trace.queue_stats(30.0, 90.0);
        assert!(mean < 15.0, "mean queue {mean} ms");
        assert!(p95 < 60.0, "p95 queue {p95} ms");
    }

    #[test]
    fn converges_at_high_rate_and_learns_capacity() {
        let capacity = 100.0 * MBPS;
        let trace = simulate(Scenario {
            secs: 90.0,
            fps: 120.0,
            initial_bps: 20.0 * MBPS,
            capacity: |_| 100.0 * MBPS,
            ..Default::default()
        });
        let convergence = trace
            .first_at_or_above(0.0, 0.85 * capacity)
            .expect("never converged");
        println!(
            "[converge 100M from 20M @120fps] reached 85% of C at {convergence:.1}s; capacity estimate {:?}",
            trace.targets.last().unwrap().2
        );
        report(
            "converge 100M from 20M @120fps",
            &trace,
            40.0,
            90.0,
            capacity,
        );

        let (mean, _, _) = trace.queue_stats(40.0, 90.0);
        assert!(mean < 15.0, "mean queue {mean} ms");
        assert!(trace.utilization(40.0, 90.0, capacity) > 0.80);
        let estimate = trace.targets.last().unwrap().2.expect("capacity unknown");
        assert!(
            (0.8 * capacity..1.25 * capacity).contains(&estimate),
            "capacity estimate {estimate}"
        );
    }

    #[test]
    fn reacts_to_capacity_drop() {
        let trace = simulate(Scenario {
            secs: 120.0,
            initial_bps: 15.0 * MBPS,
            capacity: |t| if t < 50.0 { 30.0 * MBPS } else { 10.0 * MBPS },
            ..Default::default()
        });
        let new_capacity = 10.0 * MBPS;
        let reaction = trace
            .first_at_or_below(50.0, 1.1 * new_capacity)
            .expect("never reacted")
            - 50.0;
        let (_, _, transient_max) = trace.queue_stats(50.0, 60.0);
        // Skip the moment of the drop, when the queue has not filled yet.
        let drained = trace
            .first_queue_below(51.0, 20.0)
            .expect("queue never drained")
            - 50.0;
        println!(
            "[drop 30M->10M at 50s] target before {:.2} Mbps; within 110% of new C after {reaction:.2}s; transient queue max {transient_max:.1} ms; queue below 20 ms after {drained:.2}s",
            trace.target_at(49.9) / MBPS
        );
        report("drop 30M->10M, settled", &trace, 80.0, 120.0, new_capacity);

        assert!(reaction < 2.0, "took {reaction}s");
        let (mean, _, _) = trace.queue_stats(80.0, 120.0);
        assert!(mean < 20.0, "settled queue {mean} ms");
        assert!(trace.max_target(80.0, 120.0) < 1.2 * new_capacity);
        assert!(trace.mean_target(80.0, 120.0) > 0.7 * new_capacity);
    }

    #[test]
    fn ignores_random_loss_below_two_percent() {
        let capacity = 20.0 * MBPS;
        let trace = simulate(Scenario {
            secs: 90.0,
            random_loss: 0.015,
            ..Default::default()
        });
        report("1.5% random loss", &trace, 30.0, 90.0, capacity);
        assert!(
            trace
                .first_at_or_above(0.0, 0.85 * capacity)
                .expect("never converged")
                < 30.0
        );
        assert!(trace.mean_target(30.0, 90.0) > 0.8 * capacity);
    }

    #[test]
    fn reduces_under_heavy_loss() {
        let trace = simulate(Scenario {
            secs: 20.0,
            initial_bps: 10.0 * MBPS,
            capacity: |_| 100.0 * MBPS,
            random_loss: 0.25,
            ..Default::default()
        });
        println!(
            "[25% random loss] target 1s {:.2} Mbps, 5s {:.2} Mbps, 20s {:.2} Mbps",
            trace.target_at(1.0) / MBPS,
            trace.target_at(5.0) / MBPS,
            trace.target_at(19.9) / MBPS
        );
        assert!(trace.target_at(5.0) < 0.6 * trace.target_at(1.0));
        assert!(trace.target_at(19.9) < trace.target_at(5.0));
    }

    #[test]
    fn does_not_grow_while_app_limited() {
        let trace = simulate(Scenario {
            secs: 30.0,
            initial_bps: 10.0 * MBPS,
            capacity: |_| 100.0 * MBPS,
            app_cap_bps: Some(2.0 * MBPS),
            ..Default::default()
        });
        let after_probe = trace.target_at(3.0);
        println!(
            "[app-limited at 2M, C=100M] target after probe {:.2} Mbps, at end {:.2} Mbps",
            after_probe / MBPS,
            trace.target_at(29.9) / MBPS
        );
        assert!(trace.max_target(3.0, 30.0) <= after_probe * 1.001);
    }

    #[test]
    fn startup_probe_pads_for_one_second_and_lifts_target() {
        let t = Instant::now();
        let ctl = RateController::new(
            RateConfig {
                initial_bps: 8_000_000,
                min_bps: 1,
                max_bps: u64::MAX,
            },
            Mode::Office,
            t,
        );
        assert_eq!(ctl.probe_padding_bps(t), 8_000_000);
        assert_eq!(
            ctl.probe_padding_bps(t + Duration::from_millis(999)),
            8_000_000
        );
        assert_eq!(ctl.probe_padding_bps(t + PROBE_DURATION), 0);

        let trace = simulate(Scenario {
            secs: 3.0,
            initial_bps: 5.0 * MBPS,
            ..Default::default()
        });
        assert!(
            trace.target_at(1.5) > 1.4 * 5.0 * MBPS,
            "target {}",
            trace.target_at(1.5)
        );
    }

    fn controller(t: Instant, bps: u64) -> RateController {
        let mut c = RateController::new(
            RateConfig {
                initial_bps: bps,
                min_bps: 100_000,
                max_bps: 1_000_000_000,
            },
            Mode::Office,
            t,
        );
        c.probe_ends = None;
        c
    }

    /// Sends and acknowledges one 10-packet frame per 16 ms at a controlled
    /// rate, adding `extra_delay_ms` per frame to the arrival spacing.
    fn feed_frames(
        c: &mut RateController,
        t0: Instant,
        frames: u32,
        bits_per_frame: u64,
        extra_delay_ms: f64,
        loss_every: Option<u32>,
    ) -> Instant {
        let (mut arrival, mut seq) = (0i64, 0u32);
        let mut now = t0;
        let per_packet = (bits_per_frame / 8 / 10) as u32;
        for f in 0..frames {
            now = t0 + Duration::from_millis(u64::from(f) * 16);
            let base_seq = seq;
            let mut statuses = Vec::new();
            for i in 0..10u32 {
                c.on_packet_sent(
                    SentPacket {
                        tx_seq: seq,
                        frame_id: Some(f),
                        bytes: per_packet,
                    },
                    now,
                );
                seq += 1;
                let lost = loss_every.is_some_and(|n| (f * 10 + i) % n == 0);
                statuses.push(if lost {
                    PacketStatus::NotReceived
                } else {
                    PacketStatus::Received { delta_ticks: 0 }
                });
            }
            arrival += 16_000 + (extra_delay_ms * 1e3) as i64;
            let fb = TransportFeedback {
                fb_seq: f as u16,
                base_tx_seq: base_seq,
                base_arrival_us: arrival as u32,
                statuses,
            };
            c.on_feedback(&fb, now);
        }
        now
    }

    /// 16 ms per frame of `bits`: bits × 62.5 bits per second.
    fn frame_bits(bps: u64) -> u64 {
        bps * 16 / 1000
    }

    #[test]
    fn overuse_cuts_to_85_percent_of_receive_rate() {
        let t = Instant::now();
        let mut c = controller(t, 10_000_000);
        feed_frames(&mut c, t, 120, frame_bits(10_000_000), 2.0, None);
        assert_eq!(c.delay_state(), DelayState::Overuse);
        // Arrival spacing is 18 ms per 160 kbit frame: 8.9 Mbps.
        let expected = 0.85 * 160_000.0 / 0.018;
        let ratio = c.target_bps() as f64 / expected;
        assert!(
            (0.75..1.02).contains(&ratio),
            "target {} expected about {expected}",
            c.target_bps()
        );
    }

    #[test]
    fn repeated_overuse_cuts_are_spaced() {
        let t = Instant::now();
        let mut c = controller(t, 10_000_000);
        let end = feed_frames(&mut c, t, 120, frame_bits(10_000_000), 2.0, None);
        assert_eq!(c.delay_state(), DelayState::Overuse);

        // Pretend a cut just happened and the rate has been restored.
        c.last_decrease = Some(end);
        c.target = 10_000_000.0;
        let empty = TransportFeedback {
            fb_seq: 0,
            base_tx_seq: 0,
            base_arrival_us: 0,
            statuses: Vec::new(),
        };
        c.on_feedback(&empty, end + Duration::from_millis(499));
        assert_eq!(c.target_bps(), 10_000_000, "inside the interval");
        c.on_feedback(&empty, end + Duration::from_millis(500));
        assert!(c.target_bps() < 10_000_000, "after the interval");
    }

    #[test]
    fn underuse_holds_the_target() {
        let t = Instant::now();
        let mut c = controller(t, 10_000_000);
        let mut arrival = 0i64;
        let mut seq = 0u32;
        let mut last = 0;
        for f in 0..120u32 {
            let now = t + Duration::from_millis(u64::from(f) * 16);
            let base_seq = seq;
            for _ in 0..10 {
                c.on_packet_sent(
                    SentPacket {
                        tx_seq: seq,
                        frame_id: Some(f),
                        bytes: 2000,
                    },
                    now,
                );
                seq += 1;
            }
            arrival += 14_000;
            c.on_feedback(
                &TransportFeedback {
                    fb_seq: 0,
                    base_tx_seq: base_seq,
                    base_arrival_us: arrival as u32,
                    statuses: vec![PacketStatus::Received { delta_ticks: 0 }; 10],
                },
                now,
            );
            if f == 40 {
                last = c.target_bps();
            }
        }
        assert_eq!(c.delay_state(), DelayState::Underuse);
        assert_eq!(c.target_bps(), last);
    }

    #[test]
    fn normal_grows_eight_percent_per_second() {
        let t = Instant::now();
        let mut c = controller(t, 10_000_000);
        feed_frames(&mut c, t, 250, frame_bits(10_500_000), 0.0, None);
        let grown = c.target_bps() as f64 / 10_000_000.0;
        // No growth until the send meter has a full window (500 ms).
        let expected = 1.08f64.powf(249.0 * 0.016 - 0.5);
        assert!(
            (grown / expected - 1.0).abs() < 0.03,
            "grew {grown}, expected {expected}"
        );
    }

    #[test]
    fn growth_is_additive_near_the_congestion_point() {
        let t = Instant::now();
        let mut c = controller(t, 10_000_000);
        c.congestion_point = Some(10_000_000.0);
        feed_frames(&mut c, t, 250, frame_bits(11_000_000), 0.0, None);
        let gained = c.target_bps() as f64 - 10_000_000.0;
        let expected = 10_000_000.0 * ADDITIVE_PER_SEC * (249.0 * 0.016 - 0.5);
        assert!(
            (gained / expected - 1.0).abs() < 0.05,
            "gained {gained}, expected {expected}"
        );
    }

    #[test]
    fn app_limited_boundary() {
        let t = Instant::now();
        let target = 10_000_000u64;
        let mut limited = controller(t, target);
        // 75% of target on the wire: below the 80% line.
        feed_frames(
            &mut limited,
            t,
            200,
            frame_bits(target * 75 / 100),
            0.0,
            None,
        );
        assert_eq!(limited.target_bps(), target);

        let mut free = controller(t, target);
        feed_frames(&mut free, t, 200, frame_bits(target * 85 / 100), 0.0, None);
        assert!(free.target_bps() > target);
    }

    #[test]
    fn loss_bands() {
        let t = Instant::now();
        let target = 10_000_000u64;
        let bits = frame_bits(target);

        let mut low = controller(t, target);
        feed_frames(&mut low, t, 200, bits, 0.0, Some(100)); // 1%
        assert!(low.target_bps() > target, "1% must allow growth");

        let mut middle = controller(t, target);
        feed_frames(&mut middle, t, 200, bits, 0.0, Some(20)); // 5%
        assert_eq!(middle.target_bps(), target, "5% holds");

        let mut high = controller(t, target);
        feed_frames(&mut high, t, 200, bits, 0.0, Some(5)); // 20%
        // 3.2 s at one cut per 500 ms, each ×(1 − 0.5 × 0.2).
        assert!(
            high.target_bps() < target * 60 / 100,
            "20% cuts: {}",
            high.target_bps()
        );
        assert!(high.target_bps() > target / 4);
    }

    #[test]
    fn loss_ewma_drives_fec() {
        let t = Instant::now();
        let mut c = controller(t, 10_000_000);
        feed_frames(&mut c, t, 400, frame_bits(10_000_000), 0.0, Some(20));
        assert!((c.loss_ewma() - 0.05).abs() < 0.01, "{}", c.loss_ewma());
        let a = c.allocation(t + Duration::from_secs(7));
        assert!((a.fec_ratio - (2.0 * c.loss_ewma() + FEC_MARGIN_OFFICE)).abs() < 1e-9);
    }

    #[test]
    fn allocation_matches_design() {
        assert_eq!(fec_ratio(0.0, Mode::Game), 0.05);
        assert_eq!(fec_ratio(0.0, Mode::Office), 0.02);
        assert_eq!(fec_ratio(0.5, Mode::Game), 0.25);

        let t = Instant::now();
        let mut c = controller(t, 10_000_000);
        for i in 0..100u32 {
            let now = t + Duration::from_millis(u64::from(i) * 5);
            c.on_packet_sent(
                SentPacket {
                    tx_seq: i,
                    frame_id: Some(i),
                    bytes: 3125,
                },
                now,
            );
        }
        let now = t + Duration::from_millis(500);
        let a = c.allocation(now);
        assert_eq!(a.audio_bps, 128_000);
        assert_eq!(a.video_bps, (9_872_000.0 / 1.02 * 0.95) as u64);
        // Measured video incl. FEC is 5 Mbps.
        let expected_bulk = 9_872_000.0 - 5_000_000.0;
        assert!(
            (a.bulk_bps as f64 / expected_bulk - 1.0).abs() < 0.03,
            "bulk {}",
            a.bulk_bps
        );

        c.set_mode(Mode::Game);
        let game = c.allocation(now);
        assert_eq!(game.bulk_bps, 0);
        assert_eq!(game.video_bps, (9_872_000.0 / 1.05 * 0.95) as u64);
    }

    #[test]
    fn allocation_survives_target_below_audio() {
        let t = Instant::now();
        let mut c = RateController::new(
            RateConfig {
                initial_bps: 50_000,
                min_bps: 50_000,
                max_bps: 100_000,
            },
            Mode::Office,
            t,
        );
        let a = c.allocation(t);
        assert_eq!((a.video_bps, a.bulk_bps), (0, 0));
    }

    #[test]
    fn encoder_throttle_needs_both_interval_and_change() {
        let t = Instant::now();
        let mut e = EncoderThrottle::new();
        assert_eq!(e.update(10_000_000, t), Some(10_000_000));
        assert_eq!(
            e.update(12_000_000, t + Duration::from_millis(99)),
            None,
            "too soon"
        );
        assert_eq!(
            e.update(10_400_000, t + Duration::from_millis(200)),
            None,
            "below 5%"
        );
        assert_eq!(
            e.update(10_500_000, t + Duration::from_millis(201)),
            Some(10_500_000)
        );
        // Drift measured against the applied value accumulates.
        let base = t + Duration::from_millis(500);
        assert_eq!(e.update(10_700_000, base), None);
        assert_eq!(
            e.update(11_100_000, base + Duration::from_millis(1)),
            Some(11_100_000)
        );
        assert_eq!(e.update(8_000_000, base + Duration::from_millis(100)), None);
        assert_eq!(
            e.update(8_000_000, base + Duration::from_millis(101)),
            Some(8_000_000)
        );
    }

    #[test]
    fn pacer_formula_matches_design() {
        const FRAME: Duration = Duration::from_nanos(8_333_333); // 120 fps
        // Unknown capacity falls back to 1.5 × target.
        assert_eq!(pace_rate_bps(100_000_000, None), 150_000_000);
        // Capacity dominates when 0.8 × C exceeds 1.5 × target.
        assert_eq!(pace_rate_bps(100_000_000, Some(1_000_000_000)), 800_000_000);
        // Target dominates otherwise.
        assert_eq!(pace_rate_bps(100_000_000, Some(120_000_000)), 150_000_000);

        // M0: 100 Mbps at 120 fps. Old formula: capped at half a frame, 4.17 ms.
        let frame_bytes = 100_000_000u64 / 8 / 120;
        let w = pacing_window(frame_bytes, 100_000_000, None, FRAME);
        assert!((w.as_secs_f64() * 1e3 - 4.1667).abs() < 0.01, "{w:?}");
        // Revised formula on a 1 Gbps link: bytes × 8 / 800 Mbps = 1.04 ms.
        let w = pacing_window(frame_bytes, 100_000_000, Some(1_000_000_000), FRAME);
        assert!((w.as_secs_f64() * 1e3 - 1.0417).abs() < 0.01, "{w:?}");
        // A small frame is proportionally shorter.
        let w = pacing_window(10_000, 100_000_000, None, FRAME);
        assert!((w.as_secs_f64() * 1e3 - 0.5333).abs() < 0.01, "{w:?}");
        // An oversized frame never exceeds half a frame interval.
        assert_eq!(
            pacing_window(10_000_000, 1_000_000, Some(2_000_000), FRAME),
            FRAME.mul_f64(0.5)
        );
        // Zero rate must not divide by zero.
        assert_eq!(pacing_window(1000, 0, None, FRAME), FRAME.mul_f64(0.5));
    }

    #[test]
    fn controller_exposes_pacer_with_measured_capacity() {
        let t = Instant::now();
        let mut c = controller(t, 10_000_000);
        assert_eq!(c.link_capacity_bps(), None);
        for i in 0..20u32 {
            c.on_packet_sent(
                SentPacket {
                    tx_seq: i,
                    frame_id: Some(1),
                    bytes: 1000,
                },
                t,
            );
        }
        c.on_packet_sent(
            SentPacket {
                tx_seq: 20,
                frame_id: Some(2),
                bytes: 1000,
            },
            t,
        );
        let mut statuses = vec![PacketStatus::Received { delta_ticks: 0 }];
        statuses.extend(vec![PacketStatus::Received { delta_ticks: 1 }; 19]);
        statuses.push(PacketStatus::Received { delta_ticks: 40 });
        c.on_feedback(
            &TransportFeedback {
                fb_seq: 1,
                base_tx_seq: 0,
                base_arrival_us: 0,
                statuses,
            },
            t,
        );
        // 19 packets after the first over 19 × 250 µs: 32 Mbps.
        let estimate = c.link_capacity_bps().unwrap();
        assert_eq!(estimate, 32_000_000);
        let frame = Duration::from_millis(16);
        assert_eq!(
            c.pacing_window(1_000_000, frame),
            pacing_window(1_000_000, c.target_bps(), Some(estimate), frame)
        );
    }

    #[test]
    fn capacity_estimate_holds_while_app_limited() {
        let t = Instant::now();
        let mut c = controller(t, 10_000_000);
        c.capacity = Some(1_000_000_000.0);
        feed_frames(&mut c, t, 200, frame_bits(2_000_000), 0.0, None);
        assert_eq!(c.link_capacity_bps(), Some(1_000_000_000));
    }

    #[test]
    fn overuse_brings_the_capacity_estimate_down() {
        let t = Instant::now();
        let mut c = controller(t, 10_000_000);
        c.capacity = Some(1_000_000_000.0);
        feed_frames(&mut c, t, 120, frame_bits(10_000_000), 2.0, None);
        let estimate = c.link_capacity_bps().unwrap();
        assert!(estimate < 20_000_000, "{estimate}");
    }

    #[test]
    fn path_switch_restarts_at_seventy_percent() {
        let t = Instant::now();
        let mut c = controller(t, 10_000_000);
        c.capacity = Some(50_000_000.0);
        c.congestion_point = Some(9_000_000.0);
        c.on_path_switch(t);
        assert_eq!(c.target_bps(), 7_000_000);
        assert_eq!(c.link_capacity_bps(), None);
        assert_eq!(c.delay_state(), DelayState::Normal);
    }

    #[test]
    fn feedback_for_unknown_or_repeated_packets_is_harmless() {
        let t = Instant::now();
        let mut c = controller(t, 10_000_000);
        let fb = TransportFeedback {
            fb_seq: 1,
            base_tx_seq: 50,
            base_arrival_us: 7,
            statuses: vec![
                PacketStatus::Received { delta_ticks: 3 },
                PacketStatus::NotReceived,
            ],
        };
        c.on_feedback(&fb, t);
        assert_eq!(c.loss_fraction(), None);

        for seq in 0..40 {
            c.on_packet_sent(
                SentPacket {
                    tx_seq: seq,
                    frame_id: None,
                    bytes: 100,
                },
                t,
            );
        }
        let lost_all = TransportFeedback {
            fb_seq: 2,
            base_tx_seq: 0,
            base_arrival_us: 0,
            statuses: vec![PacketStatus::NotReceived; 30],
        };
        c.on_feedback(&lost_all, t);
        c.on_feedback(&lost_all, t);
        // Counted once despite the duplicate report.
        let (_, lost) = c
            .loss_window
            .iter()
            .fold((0, 0), |(r, l), &(_, rx, ls)| (r + rx, l + ls));
        assert_eq!(lost, 30);
    }

    #[test]
    fn send_history_handles_wrap_and_stale_sequence_numbers() {
        let t = Instant::now();
        let rec = SentRecord {
            frame_id: None,
            bytes: 1,
            sent_at: t,
            reported_lost: false,
        };
        let mut h = SendHistory::default();
        h.push(u32::MAX, rec);
        h.push(0, rec);
        assert!(h.get_mut(u32::MAX).is_some());
        assert!(h.get_mut(0).is_some());
        // An older number must not wipe what is already recorded.
        h.push(u32::MAX - 10, rec);
        assert!(h.get_mut(0).is_some());
        assert!(h.take(0).is_some());
        assert!(h.take(0).is_none());
    }

    #[test]
    fn hostile_feedback_does_not_panic() {
        let mut rng = Rng(0x0123_4567_89AB_CDEF);
        let t = Instant::now();
        let mut c = controller(t, 10_000_000);
        for round in 0..20_000u64 {
            let now = t + Duration::from_micros(round * 500);
            let seq = rng.next() as u32 % 64;
            c.on_packet_sent(
                SentPacket {
                    tx_seq: round as u32 ^ seq,
                    frame_id: Some(rng.next() as u32 % 8),
                    bytes: rng.next() as u32 % 2000,
                },
                now,
            );
            let count = rng.next() as usize % 20;
            let statuses = (0..count)
                .map(|_| {
                    if rng.next().is_multiple_of(3) {
                        PacketStatus::NotReceived
                    } else {
                        PacketStatus::Received {
                            delta_ticks: rng.next() as u16,
                        }
                    }
                })
                .collect();
            c.on_feedback(
                &TransportFeedback {
                    fb_seq: rng.next() as u16,
                    base_tx_seq: round as u32 ^ (rng.next() as u32 % 128),
                    base_arrival_us: rng.next() as u32,
                    statuses,
                },
                now,
            );
            let _ = c.allocation(now);
        }
    }

    #[test]
    fn detector_classifies_slopes() {
        let mut d = TrendDetector::new();
        let mut arrival = 0.0;
        for _ in 0..40 {
            arrival += 17.0;
            d.update(16.0, 17.0, arrival);
        }
        assert_eq!(d.state, DelayState::Overuse);

        let mut d = TrendDetector::new();
        let mut arrival = 0.0;
        for _ in 0..40 {
            arrival += 15.0;
            d.update(16.0, 15.0, arrival);
        }
        assert_eq!(d.state, DelayState::Underuse);

        let mut rng = Rng(0xABCD_EF01_2345_6789);
        let mut d = TrendDetector::new();
        let mut previous = 0.0;
        let mut overuses = 0;
        for i in 1..=5000 {
            // ±2 ms of noise on each arrival time.
            let arrival = f64::from(i) * 16.0 + rng.unit() * 4.0 - 2.0;
            d.update(16.0, arrival - previous, arrival);
            previous = arrival;
            overuses += u32::from(d.state == DelayState::Overuse);
        }
        assert_eq!(overuses, 0, "arrival jitter must not look like a trend");
    }
}
