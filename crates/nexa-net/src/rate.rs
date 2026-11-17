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
/// A frame larger than its budget (a key frame) is spread at the rate the link is
/// known to take, for at most this long. See [`pacing_window`].
pub const PACE_MAX_WINDOW: Duration = Duration::from_millis(250);
/// That rate is what is being sent, not the target, which runs ahead of it while the
/// picture needs less (up to twice, below the wanted rate): a path is known to take
/// only what it has carried. But at least this share of the target, so that a key
/// frame after a still screen is not held back.
const PACE_PROVEN_FLOOR: f64 = 0.5;

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

/// A target that falls below this share of its highest in the last
/// [`RECOVERY_DROP_WINDOW`] marks that rate for recovery (libwebrtc's large-drop
/// probe: 66% within 5 s). A short hiccup, or one met while little was being sent (a
/// still screen, where the receive rate says nothing about the link), would otherwise
/// leave the rate low for minutes: a still screen does not grow it at all. Cuts come
/// in steps (the queue cut halves at most, every 500 ms), so one step alone says
/// little; the drop over a few seconds is what counts.
///
/// Only when the link itself fell that far: the receive rate the cut went by is under
/// the same share of the highest of that window. A sender whose target ran ahead of
/// what it sends (an encoder that fills 60% of its budget) meets the link's limit at a
/// receive rate as high as ever, and its cut to 85% of that is well under 66% of the
/// target: there is no old rate to go back to then, and the probe for it sent half as
/// much again as the link had ever carried (a 60 to 130 ms queue and a second cut
/// after every real one).
const RECOVERY_CUT_BELOW: f64 = 0.66;
const RECOVERY_DROP_WINDOW: Duration = Duration::from_secs(5);
/// Below this target, sending under it does not count as app-limited: an encoder
/// given a tiny budget for a large picture (VideoToolbox, 0.16 Mbit/s for 3072×1920)
/// drops frames and sends less than it was given, which otherwise read as "nothing to
/// send" and held the rate at the floor for good.
const APP_LIMITED_ABOVE_BPS: f64 = 2_000_000.0;
/// Below the rate the picture wants (the session's initial rate), sending this share
/// of the target is enough to grow it, not [`APP_LIMITED_RATIO`]: a hardware encoder
/// on desktop content fills 55 to 65% of what it is given (agent.log, 57 frames a
/// second), which read as "nothing more to send" and left a target that had been cut
/// at 3 Mbit/s for an hour on a link ten times as fast. libwebrtc lets its estimate
/// grow to 1.5 times what is sent. Not above the wanted rate: there the target would
/// run ahead of a link it has never filled.
const APP_LIMITED_RATIO_BELOW_WANTED: f64 = 0.5;
/// A packet that arrives this long after the one before it, and later than its send
/// time explains by [`STALL_EXCESS_US`] or three packet-times at the target,
/// was held by the radio (Wi-Fi off-channel for AirDrop/Continuity, power save),
/// not by a queue: a congested bottleneck keeps delivering at its own rate. The delay
/// it adds and drains in one burst says nothing about congestion, so the detector
/// starts afresh instead of reading it as overuse.
const STALL_GAP_US: i64 = 25_000;
const STALL_EXCESS_US: i64 = 20_000;
/// Recovery probes pad up to this multiple of the target, never past the old one.
const RECOVERY_PROBE_MULTIPLIER: f64 = 3.0;
const RECOVERY_PROBE_DURATION: Duration = Duration::from_millis(300);
/// Quiet time after a cut or a probe before the next recovery probe.
const RECOVERY_PROBE_GAP: Duration = Duration::from_millis(500);
/// A recovery probe starts only while at least this many frames went out in the last
/// [`SEND_WINDOW_US`] (30 a second): its padding rides on frames, and a still screen
/// has too few to carry it. The recovery waits for the picture to move.
const RECOVERY_PROBE_MIN_FRAMES: usize = 15;

/// Queueing delay is how much later than its fastest recent frame a frame's fastest
/// packet arrived: the least one-way delay of each frame, less the least of those
/// over this window. Taking the least per frame sets aside jitter and the batches a
/// cellular link delivers in; what is left is the queue.
const QUEUE_BASE_WINDOW_US: i64 = 20_000_000;
/// A queue this deep, held for [`QUEUE_PERSIST_US`], is congestion whatever the delay
/// trend says: a radio stall restarts the trend, and a link that delivers in bursts
/// (a phone's hotspot) stalls all the time, so the trend never builds there.
const QUEUE_HIGH_US: i64 = 80_000;
/// Long enough for a key frame's burst on a still screen to drain.
const QUEUE_PERSIST_US: i64 = 500_000;
/// A trend reading Overuse is noise (jitter) unless the queue is at least this deep.
const QUEUE_CONFIRM_US: i64 = 10_000;
/// A probe that builds this much queue has found the link's limit: it ends there,
/// and a recovery probe gives up on the old rate. A probe lasts less than
/// [`QUEUE_PERSIST_US`], and on a link that stalls all the time the trend is
/// restarted too often to see one, so without this every probe looked clean and kept
/// padding a hotspot past what it carries (overflow, losses, frozen picture).
const QUEUE_PROBE_LIMIT_US: i64 = 50_000;
/// A queue reading older than this says nothing about now (nothing was sent since).
const QUEUE_FRESH: Duration = Duration::from_millis(300);
/// A cut for a lasting queue keeps at least this share of the target.
const QUEUE_CUT_FLOOR: f64 = 0.5;
/// A lasting queue is being fed, not just draining, while at least this share of
/// what arrives is sent: the link then takes no more than is sent, and the cut goes
/// by that when it is the lesser. The receive rate alone reads high on a link that
/// delivers in batches (seven of them in a window six apart: 5.6 for 5 Mbit/s), and
/// 85% of that was no cut at all for a target just under the link, whose queue then
/// stood for seconds. Not when less is sent (a key frame's burst on a still screen,
/// draining): the send rate says nothing about the link then.
const QUEUE_FED_SHARE: f64 = 0.8;
/// Sending under this share of the target, a lasting queue is not ours: a still
/// screen's few frames held by the radio through a bad patch of Wi-Fi (agent.log: 0.2
/// Mbit/s sent at a 17.5 Mbit/s target, 15 radio stalls in 5 s, an 81 ms queue).
/// Not the app-limited share: when the screen starts to move on a hotspot, the send
/// rate takes a few hundred ms to show it, and the queue must be cut meanwhile.
const QUEUE_OURS_ABOVE: f64 = 0.1;
/// ...and with less than this many seconds of that rate still on the way (sent, not
/// yet reported). A queue we built holds our packets: a keyframe or a probe that
/// filled a slow link is in flight long after the send rate has gone quiet again,
/// while a still screen held by the radio has next to nothing out there.
const QUEUE_OURS_FLIGHT_S: f64 = 0.5;
/// A queue under this is no sign of congestion (where a lasting queue stops
/// counting, half of [`QUEUE_HIGH_US`]). At or over it, nothing is done that needs
/// the link to have room: no growth on a half-used target, no new recovery probe.
const QUEUE_CLEAR_US: i64 = QUEUE_HIGH_US / 2;
/// The link estimate is the median of the frame trains of this window: a train
/// delivered in one batch (a cellular link) reads as a link many times faster than
/// it is, and one such reading used to stay the estimate for good.
const CAPACITY_WINDOW: Duration = Duration::from_secs(10);
const CAPACITY_SAMPLES: usize = 32;
/// The pacer may spread a frame at the link estimate, but no faster than this many
/// times the target.
const PACE_CAPACITY_MAX_OF_TARGET: f64 = 4.0;

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

/// The capacity's side of the pace rate: `0.8 × capacity`, at most four times the
/// target; zero while the capacity is unknown.
fn pace_by_capacity(target_bps: u64, capacity_bps: Option<u64>) -> f64 {
    capacity_bps.map_or(0.0, |c| {
        (c as f64 * PACE_CAPACITY_FACTOR).min(target_bps as f64 * PACE_CAPACITY_MAX_OF_TARGET)
    })
}

/// §4.9 `pace_rate = max(1.5 × target, 0.8 × capacity)`, or `1.5 × target`
/// when the capacity is unknown.
pub fn pace_rate_bps(target_bps: u64, capacity_bps: Option<u64>) -> u64 {
    let by_target = target_bps as f64 * PACE_TARGET_FACTOR;
    by_target.max(pace_by_capacity(target_bps, capacity_bps)) as u64
}

/// §4.9 Time over which to spread one frame: `frame_bytes × 8 ÷ pace_rate`, at most
/// half a frame interval.
///
/// Except for a frame larger than its budget, a key frame: that one takes as long as
/// a run of ordinary frames of its size would (less the half interval the last of
/// them leaves free), at the rate the link is known to take: the target (the
/// controller passes what it is really sending, see [`PACE_PROVEN_FLOOR`]), or the
/// capacity's side of the pace rate on a link measured faster. Squeezed into half an
/// interval, a key frame of four budgets left at eight to sixteen times the target,
/// some 140 packets in 8 ms. On a path that reorders (a far one, 40 ms of jitter)
/// QUIC discards as a duplicate any packet that arrives more than 128 packet numbers
/// late: a fifth of that burst, more than FEC repairs. Every key frame sent to repair
/// a loss was itself lost, and the picture froze for seconds. On a slow link the
/// burst only filled the bottleneck's queue. Never over more than
/// [`PACE_MAX_WINDOW`]: a target that is wrongly low must not hold a key frame back.
pub fn pacing_window(
    frame_bytes: u64,
    target_bps: u64,
    capacity_bps: Option<u64>,
    frame_interval: Duration,
) -> Duration {
    pacing_window_at(
        frame_bytes,
        target_bps,
        target_bps as f64,
        capacity_bps,
        frame_interval,
    )
}

/// [`pacing_window`], with the rate this path has carried given apart from the target.
fn pacing_window_at(
    frame_bytes: u64,
    target_bps: u64,
    proven_bps: f64,
    capacity_bps: Option<u64>,
    frame_interval: Duration,
) -> Duration {
    let ceiling = frame_interval.mul_f64(PACE_MAX_FRAME_FRACTION);
    let rate = pace_rate_bps(target_bps, capacity_bps);
    if rate == 0 {
        return ceiling;
    }
    let bits = frame_bytes as f64 * 8.0;
    let ceiling = ceiling.as_secs_f64();
    let known = proven_bps.max(pace_by_capacity(target_bps, capacity_bps));
    let as_a_run = (bits / known - ceiling).min(PACE_MAX_WINDOW.as_secs_f64());
    Duration::from_secs_f64((bits / rate as f64).min(ceiling).max(as_a_run))
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
    /// The least one-way delay of its packets, on an offset clock (see [`QueueDelay`]).
    min_owd: i64,
}

impl Group {
    fn start(frame_id: u32, rec: &SentRecord, arrival: i64, owd: i64) -> Self {
        Self {
            frame_id,
            last_sent: rec.sent_at,
            first_arrival: arrival,
            last_arrival: arrival,
            bytes_after_first: 0,
            packets: 1,
            min_owd: owd,
        }
    }

    fn extend(&mut self, rec: &SentRecord, arrival: i64, owd: i64) {
        self.min_owd = self.min_owd.min(owd);
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

/// How deep the bottleneck's queue is, from one-way delays. The sender's and the
/// receiver's clocks differ by an unknown offset, which cancels out: the queue is a
/// frame's least delay less the least of the last [`QUEUE_BASE_WINDOW_US`].
#[derive(Debug, Default)]
struct QueueDelay {
    /// (arrival µs, least delay) of recent frames, increasing in delay: a sliding
    /// minimum.
    window: VecDeque<(i64, i64)>,
    /// The last frame's queueing, µs.
    current: i64,
    /// When the queue last went over [`QUEUE_HIGH_US`] and has stayed there.
    high_since: Option<i64>,
    /// Frames measured so far, to tell when a feedback brought a new reading.
    readings: u64,
}

impl QueueDelay {
    fn on_group(&mut self, arrival: i64, min_owd: i64) {
        while self.window.back().is_some_and(|&(_, d)| d >= min_owd) {
            self.window.pop_back();
        }
        self.window.push_back((arrival, min_owd));
        while self
            .window
            .front()
            .is_some_and(|&(at, _)| arrival - at > QUEUE_BASE_WINDOW_US)
        {
            self.window.pop_front();
        }
        let base = self.window.front().map_or(min_owd, |&(_, d)| d);
        self.current = min_owd - base;
        self.readings += 1;
        if self.current >= QUEUE_HIGH_US {
            self.high_since.get_or_insert(arrival);
        } else if self.current < QUEUE_HIGH_US / 2 {
            self.high_since = None;
        }
    }

    /// A queue over [`QUEUE_HIGH_US`] for [`QUEUE_PERSIST_US`] up to `now` (arrival µs).
    fn lasting(&self, now: i64) -> bool {
        self.high_since
            .is_some_and(|since| now - since >= QUEUE_PERSIST_US)
    }

    fn clear(&mut self) {
        *self = Self::default();
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
    /// Recent frame-train rates (when, bps), whose median is `capacity`.
    trains: VecDeque<(Instant, f64)>,
    queue: QueueDelay,
    /// When the queue reading last changed.
    queue_read_at: Option<Instant>,
    probe_ends: Option<Instant>,
    probe_congested: bool,
    /// What a recovery probe pads up to; `None` for the startup probe.
    probe_cap: Option<f64>,
    last_probe_end: Option<Instant>,
    /// The target before a sharp cut, which recovery probes may climb back to.
    recover_to: Option<f64>,
    /// The highest target of the last [`RECOVERY_DROP_WINDOW`] and when it was set.
    recent_high: Option<(Instant, f64)>,
    /// The highest receive rate of the last [`RECOVERY_DROP_WINDOW`]: what the link
    /// carried.
    carried_high: Option<(Instant, f64)>,
    /// The target when the running recovery probe began.
    probe_from: f64,
    /// When each of the last frames began to go out (µs), for
    /// [`RECOVERY_PROBE_MIN_FRAMES`].
    frame_starts: VecDeque<i64>,
    last_frame_sent: Option<u32>,
    /// Send time and arrival (µs) of the last packet reported received.
    last_rx: Option<(Instant, i64)>,
    /// Radio stalls seen (see [`STALL_GAP_US`]).
    stalls: u64,
    last_update: Option<Instant>,
    last_decrease: Option<Instant>,
    /// Receive rate at the last Overuse, which estimates the bottleneck.
    congestion_point: Option<f64>,
    /// Frames are waiting to be sent (see [`RateController::set_send_backlog`]).
    send_backlog: bool,
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
            trains: VecDeque::new(),
            queue: QueueDelay::default(),
            queue_read_at: None,
            probe_ends: Some(now + PROBE_DURATION),
            probe_congested: false,
            probe_cap: None,
            last_probe_end: None,
            recover_to: None,
            recent_high: None,
            carried_high: None,
            probe_from: 0.0,
            frame_starts: VecDeque::new(),
            last_frame_sent: None,
            last_rx: None,
            stalls: 0,
            last_update: None,
            last_decrease: None,
            congestion_point: None,
            send_backlog: false,
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
    /// Radio stalls seen so far (see [`STALL_GAP_US`]).
    pub fn stalls(&self) -> u64 {
        self.stalls
    }

    pub fn link_capacity_bps(&self) -> Option<u64> {
        self.capacity.map(|c| c as u64)
    }

    /// The bottleneck's queue as of the last frame, in ms (see [`QueueDelay`]).
    pub fn queue_delay_ms(&self) -> f64 {
        self.queue.current as f64 / 1e3
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    /// A new ceiling, as the viewer's quality choice sets it: the target drops under
    /// it at once and never climbs past it.
    pub fn set_max_bps(&mut self, max_bps: u64) {
        self.cfg.max_bps = max_bps.max(self.cfg.min_bps);
        self.target = self.target.min(self.cfg.max_bps as f64);
    }

    pub fn max_bps(&self) -> u64 {
        self.cfg.max_bps
    }

    /// Whether encoded frames are waiting behind the one being sent. A sender that
    /// is behind is not idle, however little it got out in the last half second.
    pub fn set_send_backlog(&mut self, waiting: bool) {
        self.send_backlog = waiting;
    }

    /// Extra FEC padding to send right now for the startup probe.
    pub fn probe_padding_bps(&self, now: Instant) -> u64 {
        match self.probe_ends {
            Some(end) if now < end => match self.probe_cap {
                Some(cap) => (cap - self.target).max(0.0) as u64,
                None => (self.target * (PROBE_RATE_MULTIPLIER - 1.0)) as u64,
            },
            _ => 0,
        }
    }

    /// The padding for a recovery probe only. It never goes past a rate this link
    /// carried before the cut, so it is safe to send. (The host does not send the
    /// startup probe's padding: at twice a guessed initial rate it could flood a
    /// slow link at the start of every session.)
    pub fn recovery_padding_bps(&self, now: Instant) -> u64 {
        if self.probe_cap.is_some() {
            self.probe_padding_bps(now)
        } else {
            0
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
            if self.last_frame_sent != p.frame_id {
                self.last_frame_sent = p.frame_id;
                self.frame_starts.push_back(at);
            }
        }
        while self
            .frame_starts
            .front()
            .is_some_and(|&t| at - t >= SEND_WINDOW_US)
        {
            self.frame_starts.pop_front();
        }
    }

    /// Bulk stream bytes handed to quinn. They are real sending for the
    /// app-limited rule, though only media packets produce feedback.
    pub fn on_bulk_sent(&mut self, bytes: u64, now: Instant) {
        let at = self.micros(now);
        self.send_meter.add(at, bytes);
    }

    pub fn on_feedback(&mut self, fb: &TransportFeedback, now: Instant) {
        let readings = self.queue.readings;
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
        if self.queue.readings != readings {
            self.queue_read_at = Some(now);
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
        self.trains.clear();
        self.queue.clear();
        self.queue_read_at = None;
        self.congestion_point = None;
        self.last_rx = None;
        self.probe_ends = None;
        self.probe_cap = None;
        self.recover_to = None;
        self.recent_high = None;
        self.carried_high = None;
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
    pub fn pacing_window(
        &mut self,
        frame_bytes: u64,
        frame_interval: Duration,
        now: Instant,
    ) -> Duration {
        let at = self.micros(now);
        // No reading yet (the first half second): the target is all there is.
        let proven = self.send_meter.rate_bps(at).map_or(self.target, |sent| {
            sent.clamp(PACE_PROVEN_FLOOR * self.target, self.target)
        });
        pacing_window_at(
            frame_bytes,
            self.target_bps(),
            proven,
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
        if let Some((sent, at)) = self.last_rx.replace((rec.sent_at, arrival)) {
            let gap = arrival - at;
            let send_gap = rec.sent_at.saturating_duration_since(sent).as_micros() as i64;
            let packet_us = (f64::from(rec.bytes) * 8e6 / self.target.max(1.0)) as i64;
            if gap > STALL_GAP_US && gap - send_gap > STALL_EXCESS_US.max(3 * packet_us) {
                self.stalls += 1;
                self.detector.restart();
                self.current_group = None;
                self.previous_group = None;
            }
        }
        let Some(frame) = rec.frame_id else { return };
        let owd = arrival - self.micros(rec.sent_at);
        if let Some(group) = self.current_group.as_mut().filter(|g| g.frame_id == frame) {
            group.extend(rec, arrival, owd);
            return;
        }
        if let Some(finished) = self
            .current_group
            .replace(Group::start(frame, rec, arrival, owd))
        {
            self.finish_group(finished);
        }
    }

    fn finish_group(&mut self, group: Group) {
        self.queue.on_group(group.last_arrival, group.min_owd);
        if let Some(rate) = group.train_rate_bps() {
            self.add_train(rate, group.last_sent);
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

    /// A frame train's rate joins the link estimate, the median of recent ones.
    fn add_train(&mut self, rate: f64, at: Instant) {
        self.trains.push_back((at, rate));
        while self.trains.len() > CAPACITY_SAMPLES
            || self
                .trains
                .front()
                .is_some_and(|&(t, _)| at.saturating_duration_since(t) > CAPACITY_WINDOW)
                && self.trains.len() > 3
        {
            self.trains.pop_front();
        }
        let mut rates: Vec<f64> = self.trains.iter().map(|&(_, r)| r).collect();
        rates.sort_by(f64::total_cmp);
        self.capacity = Some(rates[rates.len() / 2]);
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
        // No reading yet (the first half second) is not "nothing sent".
        let measured = self.send_meter.rate_bps(at);
        let sent = measured.unwrap_or(0.0);
        let wanted = self.cfg.initial_bps.min(self.cfg.max_bps) as f64;
        // Growing on half the target is for a link with room: any queue, or no
        // fresh reading of it, and the full share is asked for again.
        let queue_clear = self.queue.current < QUEUE_CLEAR_US
            && self
                .queue_read_at
                .is_some_and(|t| now.saturating_duration_since(t) < QUEUE_FRESH);
        let ratio = if self.target < wanted && queue_clear {
            APP_LIMITED_RATIO_BELOW_WANTED
        } else {
            APP_LIMITED_RATIO
        };
        let app_limited = self.target >= APP_LIMITED_ABOVE_BPS && sent < ratio * self.target;
        let idle = measured.is_some_and(|s| s < QUEUE_OURS_ABOVE * self.target)
            && !self.send_backlog
            && self.in_flight_bits() < QUEUE_OURS_ABOVE * self.target * QUEUE_OURS_FLIGHT_S;
        let grown = if app_limited {
            self.target
        } else {
            self.grown(dt)
        };

        if self.probe_ends.is_some() && self.queue.current >= QUEUE_PROBE_LIMIT_US {
            self.probe_congested = true;
        }
        if let Some(recv) = recv {
            let stale =
                |(at, _): (Instant, f64)| now.saturating_duration_since(at) >= RECOVERY_DROP_WINDOW;
            if self.carried_high.is_none_or(|c| recv >= c.1 || stale(c)) {
                self.carried_high = Some((now, recv));
            }
        }
        let carrying = match (recv, measured) {
            (Some(recv), Some(sent)) if sent >= QUEUE_FED_SHARE * recv => Some(recv.min(sent)),
            _ => recv,
        };
        let delay_based = self.delay_based(recv, carrying, grown, app_limited, idle, now);
        let loss_based = self.loss_based(grown, now);
        self.target = delay_based
            .min(loss_based)
            .clamp(self.cfg.min_bps as f64, self.cfg.max_bps as f64);
        self.note_drop(now);
        self.finish_probe(recv, now);
        self.maybe_probe_for_recovery(now);
    }

    /// Bits sent and not yet reported received or lost.
    fn in_flight_bits(&self) -> f64 {
        self.history
            .records
            .iter()
            .flatten()
            .filter(|r| !r.reported_lost)
            .map(|r| f64::from(r.bytes) * 8.0)
            .sum()
    }

    /// Marks the recent high for recovery once the target has fallen well below it.
    fn note_drop(&mut self, now: Instant) {
        let high = match self.recent_high {
            Some((at, high))
                if high > self.target
                    && now.saturating_duration_since(at) < RECOVERY_DROP_WINDOW =>
            {
                high
            }
            _ => {
                self.recent_high = Some((now, self.target));
                return;
            }
        };
        if self.target < RECOVERY_CUT_BELOW * high {
            // The receive rate the cut went by, when a cut it was (not loss alone).
            let cut_at = self
                .last_decrease
                .filter(|&t| now.saturating_duration_since(t) < RECOVERY_DROP_WINDOW)
                .and(self.congestion_point);
            let link_fell = match (cut_at, self.carried_high) {
                (Some(at_cut), Some((_, carried))) => at_cut < RECOVERY_CUT_BELOW * carried,
                _ => true,
            };
            if link_fell {
                self.recover_to = Some(self.recover_to.map_or(high, |r| r.max(high)));
            }
            self.recent_high = Some((now, self.target));
        }
    }

    /// After a sharp drop, short probes find out at once whether the link carries the
    /// old rate again, instead of growing back to it at 8% a second. Also while the
    /// screen is still, as libwebrtc probes after a large drop in its
    /// application-limited region: a still screen does not grow the target at all, so
    /// without a probe the first motion after a hiccup came out blurry for minutes.
    /// Never again once a probe runs into congestion: then the link really is worse.
    fn maybe_probe_for_recovery(&mut self, now: Instant) {
        let Some(goal) = self.recover_to else { return };
        if self.target >= goal * 0.95 {
            self.recover_to = None;
            return;
        }
        let quiet = |t: Option<Instant>| {
            t.is_none_or(|t| now.saturating_duration_since(t) >= RECOVERY_PROBE_GAP)
        };
        if self.probe_ends.is_none()
            && self.queue.current < QUEUE_CLEAR_US
            && self.detector.state == DelayState::Normal
            && self.frame_starts.len() >= RECOVERY_PROBE_MIN_FRAMES
            && quiet(self.last_decrease)
            && quiet(self.last_probe_end)
        {
            self.probe_ends = Some(now + RECOVERY_PROBE_DURATION);
            self.probe_congested = false;
            self.probe_from = self.target;
            self.probe_cap = Some((self.target * RECOVERY_PROBE_MULTIPLIER).min(goal));
        }
    }

    fn delay_based(
        &mut self,
        recv: Option<f64>,
        carrying: Option<f64>,
        grown: f64,
        app_limited: bool,
        idle: bool,
        now: Instant,
    ) -> f64 {
        // A queue that stays deep is congestion, whatever the trend says: on a hotspot
        // the trend never builds (it stalls all the time), and a target that a still
        // screen left high floods the link when it moves. Unless next to nothing is
        // being sent (see [`QUEUE_OURS_ABOVE`]): cutting for that queue took a 17
        // Mbit/s target to 2 in two seconds.
        let fresh = self
            .queue_read_at
            .is_some_and(|t| now.saturating_duration_since(t) < QUEUE_FRESH);
        // By what the link carries (see [`QUEUE_FED_SHARE`]).
        if fresh && !idle && self.queue.lasting(self.newest_arrival) {
            return self.cut(carrying, now, QUEUE_CUT_FLOOR);
        }
        match self.detector.state {
            DelayState::Normal => grown,
            DelayState::Underuse => self.target,
            // Sending well below the target (a still screen), a rising delay is the
            // radio, not a queue we built: Wi-Fi going off-channel for a moment (AWDL)
            // looks just like it. The receive rate then measures what was sent, not
            // the link, and cutting to it took a 100 Mbit/s LAN to the floor in
            // seconds. Real congestion still shows as loss, or as a lasting queue.
            DelayState::Overuse if app_limited => self.target,
            // A rising trend with no queue behind it is jitter (a far, noisy path),
            // which used to cut such a link to the floor.
            DelayState::Overuse if self.queue.current < QUEUE_CONFIRM_US => self.target,
            DelayState::Overuse => self.cut(recv, now, 0.0),
        }
    }

    /// Cuts to just under the receive rate (the bottleneck's, while congested), at
    /// most every [`DECREASE_INTERVAL`] and to no less than `floor` × the target.
    fn cut(&mut self, recv: Option<f64>, now: Instant, floor: f64) -> f64 {
        self.probe_congested = true;
        let may_cut = self
            .last_decrease
            .is_none_or(|t| now.saturating_duration_since(t) >= DECREASE_INTERVAL);
        match recv {
            Some(recv) if may_cut => {
                self.last_decrease = Some(now);
                self.congestion_point = Some(recv);
                // Under congestion the receive rate is the bottleneck rate (when
                // something was received to measure it).
                if recv > 0.0 {
                    self.capacity = self.capacity.map(|c| c.min(recv));
                }
                self.target
                    .min(OVERUSE_BACKOFF * recv)
                    .max(floor * self.target)
            }
            _ => self.target,
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
            self.last_probe_end = Some(now);
            if self.probe_cap.take().is_some() {
                // A recovery probe met congestion: the old rate is not there now.
                self.recover_to = None;
            }
            return;
        }
        if now < end {
            return;
        }
        self.probe_ends = None;
        let recovering = self.probe_cap.take().is_some();
        self.last_probe_end = Some(now);
        if let Some(recv) = recv {
            self.capacity = Some(self.capacity.map_or(recv, |c| c.max(recv)));
            self.target = self
                .target
                .max(PROBE_ACCEPT_FACTOR * recv)
                .min(self.cfg.max_bps as f64);
        }
        // A recovery probe that lifted nothing has shown all it can (little rode on
        // it, or the target is within reach of the old one already): the same probe
        // again would show the same. It used to be sent again every 0.8 s for as long
        // as the screen stayed still, 300 ms of padding at tens of Mbit/s each time.
        if recovering && self.target <= self.probe_from {
            self.recover_to = None;
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
        /// Wi-Fi: each packet waits up to this long more for its turn on the air,
        /// in order (a later packet never overtakes an earlier one).
        jitter_us: u64,
        last_arrival_us: u64,
        /// The radio away (another channel, as AWDL does) for `.1` µs every `.0` µs:
        /// what arrives meanwhile waits for it to come back.
        stalls: Option<(u64, u64)>,
        /// Only between these times (µs), when set.
        stalls_between: Option<(u64, u64)>,
        /// A cellular link hands packets over in batches, every this many µs.
        batch_us: u64,
        /// Jitter per packet, independently: a later packet may overtake an earlier
        /// one (a path through several queues), unlike [`Bottleneck::jitter_us`].
        reorder_jitter_us: u64,
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
            let extra = (self.rng.unit() * self.jitter_us as f64) as u64;
            let mut arrival =
                (self.busy_until_us + self.one_way_us + extra).max(self.last_arrival_us);
            let reordered = (self.rng.unit() * self.reorder_jitter_us as f64) as u64;
            let stalling = self
                .stalls_between
                .is_none_or(|(from, to)| (from..to).contains(&arrival));
            if let Some((every, away)) = self.stalls.filter(|_| stalling) {
                let into = arrival % every;
                if into < away {
                    arrival += away - into;
                }
            }
            if self.batch_us > 0 {
                arrival = arrival.div_ceil(self.batch_us) * self.batch_us;
            }
            self.last_arrival_us = arrival;
            let arrival = arrival + reordered;
            Some((arrival, queueing))
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
        /// What the encoder wants at time t (s): a still screen, then video.
        app_demand: Option<fn(f64) -> f64>,
        /// A keyframe of this many bytes every this many seconds, paced like any
        /// frame (at most half a frame interval): the burst a still screen sends.
        keyframes: Option<(f64, f64)>,
        /// No keyframes from this time (s) on.
        keyframes_until: f64,
        /// See [`Bottleneck::jitter_us`].
        jitter_ms: f64,
        /// See [`Bottleneck::stalls`], in ms.
        stalls_ms: Option<(f64, f64)>,
        /// Those stalls only between these times (s); all along when `None`.
        stalls_between: Option<(f64, f64)>,
        /// The share of its budget the encoder fills at budgets below 1 Mbit/s (a
        /// hardware encoder dropping frames it cannot fit).
        low_budget_fill: f64,
        /// The share of its budget the encoder fills at any budget: VideoToolbox on
        /// desktop content sent 0.55 to 0.65 of the target at 57 frames a second.
        fill: f64,
        mode: Mode,
        /// Frames a second at time t (s) when it is not `fps` throughout: a still
        /// screen sends a frame only when something changes.
        fps_at: Option<fn(f64) -> f64>,
        one_way_ms: f64,
        /// How much the bottleneck queues before it drops.
        max_queue_ms: f64,
        /// See [`Bottleneck::batch_us`], in ms.
        batch_ms: f64,
        /// See [`Bottleneck::reorder_jitter_us`], in ms.
        reorder_jitter_ms: f64,
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
                app_demand: None,
                keyframes: None,
                keyframes_until: f64::INFINITY,
                jitter_ms: 0.0,
                stalls_ms: None,
                stalls_between: None,
                low_budget_fill: 1.0,
                fill: 1.0,
                mode: Mode::Office,
                fps_at: None,
                one_way_ms: 15.0,
                max_queue_ms: 300.0,
                batch_ms: 0.0,
                reorder_jitter_ms: 0.0,
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
        /// (seconds, bytes) per packet sent, delivered or not.
        sent: Vec<(f64, u32)>,
        /// Every 100 ms, what the controller saw and what the link held.
        samples: Vec<Sample>,
    }

    /// One line of [`Trace::dump`].
    #[derive(Clone, Copy)]
    struct Sample {
        t: f64,
        target: f64,
        /// Sent over the last 100 ms, bit/s, and how much of it was probe padding.
        sent: f64,
        padding: f64,
        /// The bottleneck's queue, ms, and the controller's reading of it.
        link_queue_ms: f64,
        seen_queue_ms: f64,
        state: DelayState,
        probing: bool,
        /// Packets the pacer still holds.
        pacer: usize,
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

        /// The timeline of `from..to` (s), one line per 100 ms: send rate, target,
        /// queue, detector state, receive rate and probe state side by side.
        #[allow(dead_code)]
        fn dump(&self, from: f64, to: f64) {
            println!("   t    target   sent  (pad)   recv  link-q seen-q state    probe pacer");
            for s in self.samples.iter().filter(|s| s.t >= from && s.t < to) {
                let recv = self.utilization(s.t - 0.1, s.t, MBPS);
                println!(
                    "{:6.1} {:7.2} {:6.2} {:6.2} {:6.2} {:6.0} {:6.0} {:8} {:5} {:5}",
                    s.t,
                    s.target / MBPS,
                    s.sent / MBPS,
                    s.padding / MBPS,
                    recv,
                    s.link_queue_ms,
                    s.seen_queue_ms,
                    format!("{:?}", s.state),
                    if s.probing { "probe" } else { "-" },
                    s.pacer
                );
            }
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
        let mut ctl = RateController::new(cfg, s.mode, t0);
        let mut link = Bottleneck {
            busy_until_us: 0,
            one_way_us: (s.one_way_ms * 1e3) as u64,
            max_queue_us: (s.max_queue_ms * 1e3) as u64,
            batch_us: (s.batch_ms * 1e3) as u64,
            reorder_jitter_us: (s.reorder_jitter_ms * 1e3) as u64,
            random_loss: s.random_loss,
            rng: Rng(0x1234_5678_9ABC_DEF1),
            jitter_us: (s.jitter_ms * 1e3) as u64,
            last_arrival_us: 0,
            stalls: s
                .stalls_ms
                .map(|(every, away)| ((every * 1e3) as u64, (away * 1e3) as u64)),
            stalls_between: s
                .stalls_between
                .map(|(from, to)| ((from * 1e6) as u64, (to * 1e6) as u64)),
        };
        let mut frame_noise = Rng(0xFEED_FACE_1234_5678);
        let mut trace = Trace::default();

        let mut tx: VecDeque<(u64, u32, Option<u32>)> = VecDeque::new();
        let mut rx: VecDeque<(u32, Option<u64>)> = VecDeque::new();
        let mut reports: VecDeque<(u64, TransportFeedback)> = VecDeque::new();
        let (mut seq, mut frame, mut fb_seq) = (0xFFFF_F000u32, 0u32, 0u16);
        let mut frame_interval_us;
        let (mut next_frame, mut next_audio, mut next_report, mut next_sample) =
            (0u64, 0u64, FEEDBACK_INTERVAL_US, 0u64);
        let end_us = (s.secs * 1e6) as u64;
        let (mut sent_bytes, mut padding_bytes) = (0u64, 0.0f64);

        let mut now = 0u64;
        while now < end_us {
            let wall = at(now);
            if now >= next_frame {
                let fps = s.fps_at.map_or(s.fps, |f| f(now as f64 / 1e6));
                frame_interval_us = (1e6 / fps) as u64;
                next_frame += frame_interval_us;
                let alloc = ctl.allocation(wall);
                let mut video = alloc.video_bps as f64;
                if let Some(cap) = s.app_cap_bps {
                    video = video.min(cap);
                }
                video *= s.fill;
                if video < 1.0 * MBPS {
                    video *= s.low_budget_fill;
                }
                if let Some(demand) = s.app_demand {
                    video = video.min(demand(now as f64 / 1e6));
                }
                let mut per_frame = video / 8.0 / fps * (0.8 + 0.4 * frame_noise.unit());
                let keyframes = s
                    .keyframes
                    .filter(|_| (now as f64) < s.keyframes_until * 1e6);
                if let Some((every, bytes)) = keyframes {
                    let every_frames = (every * fps).round().max(1.0) as u32;
                    if frame % every_frames == 0 {
                        per_frame = per_frame.max(bytes);
                    }
                }
                let padding = ctl.probe_padding_bps(wall) as f64 / 8.0 / fps;
                padding_bytes += padding;
                let wire = per_frame * (1.0 + alloc.fec_ratio) + padding;
                let packets = (wire / f64::from(PACKET_BYTES)).ceil().max(1.0) as u64;
                let window = ctl
                    .pacing_window(wire as u64, Duration::from_micros(frame_interval_us), wall)
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
                    sent_bytes += u64::from(bytes);
                    trace.sent.push((now as f64 / 1e6, bytes));
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
                trace.samples.push(Sample {
                    t: now as f64 / 1e6,
                    target: ctl.target_bps() as f64,
                    sent: sent_bytes as f64 * 80.0,
                    padding: padding_bytes * 80.0,
                    link_queue_ms: link.busy_until_us.saturating_sub(now) as f64 / 1e3,
                    seen_queue_ms: ctl.queue_delay_ms(),
                    state: ctl.delay_state(),
                    probing: ctl.probe_padding_bps(wall) > 0,
                    pacer: tx.len(),
                });
                (sent_bytes, padding_bytes) = (0, 0.0);
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
        let mut last = rx.iter().rposition(|p| p.1.is_some_and(|a| a <= now))?;
        // As the viewer's FeedbackBuilder: stop before a gap whose later packets came
        // less than 50 ms ago (it may be a packet overtaken, not lost).
        for i in 0..=last {
            if rx[i].1.is_none_or(|a| a > now) {
                let after = rx.iter().skip(i).find_map(|p| p.1.filter(|&a| a <= now));
                if after.is_some_and(|a| now - a < 50_000) {
                    if i == 0 {
                        return None;
                    }
                    last = i - 1;
                    break;
                }
            }
        }
        let reported: Vec<_> = rx.drain(..=last).collect();
        let base_tick = reported.iter().find_map(|p| p.1)? / 250;
        let mut previous_tick = base_tick;
        let statuses = reported
            .iter()
            .map(|p| match p.1 {
                None => PacketStatus::NotReceived,
                Some(a) if a > now => PacketStatus::NotReceived,
                Some(a) => {
                    // As the viewer's FeedbackBuilder: from the latest arrival so far.
                    let tick = (a / 250).max(previous_tick);
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

    /// A still screen whose keyframes go out as bursts: what the host on a fast LAN
    /// logged, its target cut to the floor within seconds while the link estimate
    /// read 20–266 Mbit/s, then video that the encoder could not send.
    #[test]
    fn keyframe_bursts_on_a_still_screen_do_not_cut_to_the_floor() {
        let trace = simulate(Scenario {
            secs: 30.0,
            initial_bps: 35.0 * MBPS,
            capacity: |_| 100.0 * MBPS,
            app_demand: Some(|t| if t < 15.0 { 0.2 * MBPS } else { f64::INFINITY }),
            keyframes: Some((2.0, 400_000.0)),
            jitter_ms: 8.0,
            stalls_ms: Some((1_000.0, 60.0)),
            ..Default::default()
        });
        let still = trace.target_at(14.9);
        let video = trace.first_at_or_above(15.0, 20.0 * MBPS);
        println!(
            "[still + 400 kB keyframes every 2 s, video from 15 s, C=100M] target at 5 s {:.2}, at 14.9 s {:.2} Mbps; 20 Mbps after {:?} s of video; at 30 s {:.2} Mbps",
            trace.target_at(5.0) / MBPS,
            still / MBPS,
            video.map(|t| t - 15.0),
            trace.target_at(29.9) / MBPS
        );
        assert!(
            still >= 5.0 * MBPS,
            "a still screen cut the target to {still}"
        );
        assert!(
            video.is_some_and(|t| t - 15.0 <= 5.0),
            "video waited {video:?} s for 20 Mbps"
        );
    }

    /// Active use on Wi-Fi: content that comes and goes (video, then a pause, then
    /// typing) while the radio leaves the channel for a moment every second. The host
    /// logged its target sliding from 35 to 0.3 Mbit/s within a minute of this on a
    /// fast LAN.
    #[test]
    fn wifi_stalls_during_active_use_do_not_slide_the_rate_down() {
        let capacity = 100.0 * MBPS;
        let trace = simulate(Scenario {
            secs: 90.0,
            initial_bps: 35.0 * MBPS,
            capacity: |_| 100.0 * MBPS,
            app_demand: Some(|t| match (t as u64) % 7 {
                0..=3 => f64::INFINITY,
                4 => 3.0 * MBPS,
                _ => 0.5 * MBPS,
            }),
            keyframes: Some((5.0, 400_000.0)),
            jitter_ms: 8.0,
            stalls_ms: Some((1_000.0, 60.0)),
            ..Default::default()
        });
        let lowest = trace
            .targets
            .iter()
            .filter(|s| s.0 >= 10.0)
            .map(|s| s.1)
            .fold(f64::INFINITY, f64::min);
        println!(
            "[active use, Wi-Fi stalls 60 ms/s, C=100M] target mean 10-90 s {:.2} Mbps, lowest {:.2} Mbps, at 90 s {:.2} Mbps",
            trace.mean_target(10.0, 90.0) / MBPS,
            lowest / MBPS,
            trace.target_at(89.9) / MBPS
        );
        report("wifi-active", &trace, 10.0, 90.0, capacity);
        assert!(lowest >= 10.0 * MBPS, "slid to {:.2} Mbps", lowest / MBPS);
    }

    /// A phone's hotspot: 5 Mbit/s, 180 ms round trip, a deep buffer, packets handed
    /// over in 20 ms batches, and a screen that is mostly still with bursts of motion.
    /// The session came from Wi-Fi with a 14 Mbit/s target. The batches read as radio
    /// stalls and as a link of hundreds of Mbit/s, the still screen kept the target,
    /// and every burst of motion flooded the hotspot (300 ms+ of queue, losses).
    #[test]
    fn a_hotspot_is_not_flooded_when_the_screen_starts_moving() {
        let capacity = 5.0 * MBPS;
        let trace = simulate(Scenario {
            secs: 60.0,
            initial_bps: 14.0 * MBPS,
            capacity: |_| 5.0 * MBPS,
            app_demand: Some(|t| match (t as u64) % 10 {
                0..=5 => 0.4 * MBPS,
                _ => f64::INFINITY,
            }),
            keyframes: Some((5.0, 300_000.0)),
            jitter_ms: 5.0,
            one_way_ms: 90.0,
            max_queue_ms: 1_000.0,
            // Batches far enough apart to read as radio stalls, as a real hotspot's
            // did (6 to 18 every 5 s in agent.log).
            batch_ms: 40.0,
            ..Default::default()
        });
        // The motion of the later cycles, once the first has taught it the link.
        let mut motion_queue = Vec::new();
        for cycle in 2..6 {
            let from = f64::from(cycle) * 10.0 + 6.0;
            motion_queue.push(trace.queue_stats(from, from + 4.0).1);
        }
        let worst_p95 = motion_queue.iter().copied().fold(0.0, f64::max);
        let estimate = trace.targets.last().unwrap().2.unwrap_or(0.0);
        println!(
            "[hotspot 5M, 180 ms, 40 ms batches] queue p95 in motion per cycle {motion_queue:.0?} ms; target mean 20-60 s {:.2} Mbps; link estimate {:.1} Mbps",
            trace.mean_target(20.0, 60.0) / MBPS,
            estimate / MBPS
        );
        report("hotspot", &trace, 20.0, 60.0, capacity);
        assert!(worst_p95 < 150.0, "queue p95 {worst_p95:.0} ms in motion");
        assert!(trace.mean_target(20.0, 60.0) <= 1.2 * capacity);
        assert!(estimate < 10.0 * capacity, "link estimate {estimate}");
    }

    /// A still screen while the Wi-Fi goes through a bad patch: for a few seconds the
    /// radio is away most of the time, then the screen moves. agent.log showed 15
    /// radio stalls in 5 s, an 81 ms queue and a 0.4 Mbit/s receive rate while 0.2
    /// Mbit/s was sent. That little cannot have built a queue, yet the lasting-queue
    /// cut took a 17 Mbit/s target to 2 in two seconds, and the still screen kept it
    /// there for an hour (a blurry picture on a fast link).
    #[test]
    fn a_burst_of_wifi_stalls_on_a_still_screen_does_not_cut_the_rate() {
        let capacity = 100.0 * MBPS;
        let trace = simulate(Scenario {
            secs: 25.0,
            // A still screen sends a frame only when something changes: agent.log
            // showed 7 a second.
            fps: 7.0,
            initial_bps: 20.0 * MBPS,
            capacity: |_| 100.0 * MBPS,
            app_demand: Some(|t| if t < 10.0 { 0.2 * MBPS } else { f64::INFINITY }),
            jitter_ms: 5.0,
            stalls_ms: Some((300.0, 270.0)),
            stalls_between: Some((4.0, 7.0)),
            ..Default::default()
        });
        let still = trace.target_at(9.9);
        let video = trace.first_at_or_above(10.0, 20.0 * MBPS);
        println!(
            "[still, Wi-Fi away 270 ms of every 300 for 3 s, video from 10 s, C=100M] target at 9.9 s {:.2} Mbps; 20 Mbps after {:?} s of video",
            still / MBPS,
            video.map(|t| t - 10.0)
        );
        report("wifi-bad-patch", &trace, 10.0, 25.0, capacity);
        assert!(
            still >= 10.0 * MBPS,
            "the stalls cut a still screen to {still}"
        );
        assert!(
            video.is_some_and(|t| t - 10.0 <= 5.0),
            "video waited {video:?} s for 20 Mbps"
        );
    }

    /// A video (an encoder that fills 65% of its budget) through fifteen seconds of a
    /// slow link, then the fast one again. Sending 59% of the target read as "nothing
    /// more to send", so the target stayed where the slow link had left it: agent.log
    /// showed 2.3 to 2.9 Mbit/s over 45 minutes on a link of tens.
    #[test]
    fn a_video_climbs_back_after_a_slow_spell() {
        let trace = simulate(Scenario {
            secs: 60.0,
            initial_bps: 20.0 * MBPS,
            capacity: |t| {
                if (5.0..20.0).contains(&t) {
                    3.0 * MBPS
                } else {
                    100.0 * MBPS
                }
            },
            fill: 0.65,
            mode: Mode::Balanced,
            ..Default::default()
        });
        let low = trace.target_at(19.9);
        let back = trace.first_at_or_above(20.0, 15.0 * MBPS);
        println!(
            "[video filling 65%, 3 Mbit/s from 5 to 20 s, C=100M] target at 19.9 s {:.2} Mbps; 15 Mbps after {:?} s; at 60 s {:.2} Mbps",
            low / MBPS,
            back.map(|t| t - 20.0),
            trace.target_at(59.9) / MBPS
        );
        assert!(low <= 4.5 * MBPS, "stayed above the slow link: {low}");
        assert!(
            back.is_some_and(|t| t - 20.0 <= 45.0),
            "the video waited {back:?} s for 15 Mbps"
        );
    }

    /// A still screen on a 5 Mbit/s hotspot with a target left at 14 from Wi-Fi: only
    /// a keyframe now and then, each of which fills the link for half a second. By the
    /// time that queue counts as lasting, the burst is out of the send-rate window, so
    /// the near-idle gate took it for a queue that is not ours and kept the 14: the
    /// first motion then flooded the hotspot. The burst is still in flight, though.
    #[test]
    fn a_still_screen_s_keyframes_still_teach_a_slow_link() {
        let capacity = 5.0 * MBPS;
        let trace = simulate(Scenario {
            secs: 40.0,
            initial_bps: 14.0 * MBPS,
            capacity: |_| 5.0 * MBPS,
            app_demand: Some(|t| if t < 30.0 { 0.2 * MBPS } else { f64::INFINITY }),
            fps_at: Some(|t| if t < 30.0 { 7.0 } else { 60.0 }),
            keyframes: Some((5.0, 300_000.0)),
            keyframes_until: 30.0,
            jitter_ms: 5.0,
            one_way_ms: 90.0,
            max_queue_ms: 1_000.0,
            batch_ms: 40.0,
            ..Default::default()
        });
        let before = trace.target_at(29.9);
        let (_, p95, max) = trace.queue_stats(30.0, 40.0);
        println!(
            "[still + keyframes on a 5M hotspot from 14, motion at 30 s] target at 29.9 s {:.2} Mbps; queue in motion p95 {p95:.0} ms max {max:.0} ms",
            before / MBPS
        );
        assert!(before <= 1.6 * capacity, "the still screen kept {before}");
        assert!(p95 < 150.0, "queue p95 {p95:.0} ms when the screen moved");
    }

    /// A video whose encoder fills 60% of its budget, on a 5 Mbit/s link, from a low
    /// target: growing on half the target must stop when the link is full (a queue
    /// shows), not go on because the picture wants more.
    #[test]
    fn growth_on_half_the_target_stops_at_the_link() {
        let capacity = 5.0 * MBPS;
        let trace = simulate(Scenario {
            secs: 90.0,
            initial_bps: 14.0 * MBPS,
            capacity: |_| 5.0 * MBPS,
            fill: 0.6,
            mode: Mode::Balanced,
            jitter_ms: 5.0,
            one_way_ms: 30.0,
            max_queue_ms: 1_000.0,
            ..Default::default()
        });
        let (mean, p95, max) = trace.queue_stats(30.0, 90.0);
        let highest = trace.max_target(30.0, 90.0);
        println!(
            "[video filling 60% on 5M, wanted 14] target mean 30-90 s {:.2}, highest {:.2} Mbps; delivered {:.2} Mbps; queue mean {mean:.0} p95 {p95:.0} max {max:.0} ms",
            trace.mean_target(30.0, 90.0) / MBPS,
            highest / MBPS,
            trace.utilization(30.0, 90.0, capacity) * capacity / MBPS
        );
        assert!(p95 < 80.0, "queue p95 {p95:.0} ms");
        // Each cut is to 85% of the link, of which this encoder fills 60%: half the
        // link, from where the target climbs back. Two thirds on average.
        assert!(
            trace.utilization(30.0, 90.0, capacity) > 0.6,
            "the link is left unused"
        );
        assert!(max < 80.0, "queue up to {max:.0} ms");
    }

    /// A key frame in the middle of motion on a 5 Mbit/s hotspot, with the target
    /// just under the link: the queue it leaves must be drained, not stand. The
    /// batches the hotspot delivers in made the receive rate read 5.6, 85% of which
    /// is no cut for a 4.8 Mbit/s target, so half a second of queue stood for eight.
    #[test]
    fn a_queue_left_standing_is_drained() {
        let trace = simulate(Scenario {
            secs: 45.0,
            initial_bps: 4.6 * MBPS,
            capacity: |_| 5.0 * MBPS,
            keyframes: Some((30.0, 300_000.0)),
            jitter_ms: 5.0,
            one_way_ms: 90.0,
            max_queue_ms: 1_000.0,
            batch_ms: 40.0,
            ..Default::default()
        });
        let (_, _, deepest) = trace.queue_stats(30.0, 32.0);
        let drained = trace
            .queue
            .iter()
            .rev()
            .find(|q| q.0 < 45.0 && q.1 >= 80.0)
            .map_or(0.0, |q| q.0 - 30.0);
        println!(
            "[key frame in motion on a 5M hotspot] target before {:.2} Mbps; queue up to {deepest:.0} ms, under 80 ms after {drained:.1} s; target at 45 s {:.2} Mbps",
            trace.target_at(29.9) / MBPS,
            trace.target_at(44.9) / MBPS
        );
        assert!(deepest > 150.0, "no queue to drain: {deepest:.0} ms");
        // About a second to see it (a queue of half a second delays its own
        // reading by as much, and must then last half a second), then two to drain.
        assert!(drained < 4.0, "the queue stood for {drained:.1} s");
        assert!(trace.target_at(44.9) > 3.0 * MBPS);
    }

    /// A video, a hiccup on the link that cuts the target, then a still screen that
    /// still sends its 60 small frames a second. The recovery probes climb back half
    /// way on padding alone; from there one more shows nothing new, and it used to be
    /// sent again every 0.8 s for as long as the screen stayed still: 300 ms of
    /// padding at tens of Mbit/s each time.
    #[test]
    fn a_recovery_probe_that_lifts_nothing_is_not_sent_again() {
        let trace = simulate(Scenario {
            secs: 30.0,
            initial_bps: 35.0 * MBPS,
            capacity: |t| {
                if (7.0..7.6).contains(&t) {
                    3.0 * MBPS
                } else {
                    100.0 * MBPS
                }
            },
            app_demand: Some(|t| if t < 7.8 { f64::INFINITY } else { 0.3 * MBPS }),
            ..Default::default()
        });
        let probes = |from: f64, to: f64| {
            let during = trace.samples.iter().filter(|s| s.t >= from && s.t < to);
            during.filter(|s| s.probing).count()
        };
        println!(
            "[video, hiccup at 7 s, still from 7.8 s, C=100M] target before {:.1}, after the hiccup {:.1}, at 15 s {:.1} Mbps; 100 ms steps with a probe on: {} from 8 to 15 s, {} from 15 to 30 s",
            trace.target_at(6.9) / MBPS,
            trace.target_at(8.2) / MBPS,
            trace.target_at(15.0) / MBPS,
            probes(8.0, 15.0),
            probes(15.0, 30.0)
        );
        // The probes still run on the still screen, and bring a good part back...
        assert!(trace.target_at(15.0) >= 0.4 * trace.target_at(6.9));
        // ...and then stop.
        assert_eq!(probes(15.0, 30.0), 0);
    }

    /// The same hiccup, then a screen that is really still: seven frames a second
    /// cannot carry a probe's padding. None is spent on it, and the recovery waits
    /// for the picture to move.
    #[test]
    fn a_recovery_waits_for_frames_to_carry_its_probes() {
        let trace = simulate(Scenario {
            secs: 30.0,
            initial_bps: 35.0 * MBPS,
            capacity: |t| {
                if (7.0..7.6).contains(&t) {
                    3.0 * MBPS
                } else {
                    100.0 * MBPS
                }
            },
            app_demand: Some(|t| {
                if (7.8..20.0).contains(&t) {
                    0.2 * MBPS
                } else {
                    f64::INFINITY
                }
            }),
            fps_at: Some(|t| if (7.8..20.0).contains(&t) { 7.0 } else { 60.0 }),
            ..Default::default()
        });
        let padded: f64 = trace
            .samples
            .iter()
            .filter(|s| s.t >= 9.0 && s.t < 20.0)
            .map(|s| s.padding)
            .sum();
        let before = trace.target_at(6.9);
        let back = trace.first_at_or_above(20.0, 0.8 * before);
        println!(
            "[video, hiccup at 7 s, 7 frames a second from 7.8 to 20 s, C=100M] target before {:.1}, at 19.9 s {:.1} Mbps; padding while still {:.2} Mbit; back at 80% {:?} s after the picture moves",
            before / MBPS,
            trace.target_at(19.9) / MBPS,
            padded / 10.0 / MBPS,
            back.map(|t| t - 20.0)
        );
        assert_eq!(padded, 0.0, "padding on a still screen");
        assert!(
            back.is_some_and(|t| t - 20.0 <= 5.0),
            "the video waited {back:?} for its old rate"
        );
    }

    /// Office work, a video and a game on four kinds of network: a table to compare
    /// rate-control changes by (`cargo test workloads -- --ignored --nocapture`).
    #[test]
    #[ignore = "prints a comparison table"]
    fn workloads_on_networks() {
        let workloads: [(&str, Scenario); 3] = [
            (
                "office",
                Scenario {
                    mode: Mode::Office,
                    fill: 0.7,
                    // Still, with two seconds of scrolling every ten.
                    app_demand: Some(|t| match (t as u64) % 10 {
                        6 | 7 => f64::INFINITY,
                        _ => 0.2 * MBPS,
                    }),
                    fps_at: Some(|t| match (t as u64) % 10 {
                        6 | 7 => 60.0,
                        _ => 7.0,
                    }),
                    ..Default::default()
                },
            ),
            (
                "video",
                Scenario {
                    mode: Mode::Balanced,
                    fill: 0.65,
                    ..Default::default()
                },
            ),
            (
                "game",
                Scenario {
                    mode: Mode::Game,
                    ..Default::default()
                },
            ),
        ];
        type Net = (&'static str, f64, fn(Scenario) -> Scenario);
        let networks: [Net; 4] = [
            ("Wi-Fi, light stalls", 100.0, |s| Scenario {
                initial_bps: 20.0 * MBPS,
                capacity: |_| 100.0 * MBPS,
                jitter_ms: 8.0,
                stalls_ms: Some((1_000.0, 60.0)),
                ..s
            }),
            ("Wi-Fi, bad 3 s at 20 s", 100.0, |s| Scenario {
                initial_bps: 20.0 * MBPS,
                capacity: |_| 100.0 * MBPS,
                jitter_ms: 5.0,
                stalls_ms: Some((300.0, 270.0)),
                stalls_between: Some((20.0, 23.0)),
                ..s
            }),
            ("hotspot 5M, 180 ms", 5.0, |s| Scenario {
                initial_bps: 14.0 * MBPS,
                capacity: |_| 5.0 * MBPS,
                jitter_ms: 5.0,
                one_way_ms: 90.0,
                max_queue_ms: 1_000.0,
                batch_ms: 40.0,
                ..s
            }),
            ("far 300 ms, jitter, 1%", 20.0, |s| Scenario {
                initial_bps: 20.0 * MBPS,
                capacity: |_| 20.0 * MBPS,
                random_loss: 0.01,
                reorder_jitter_ms: 40.0,
                one_way_ms: 150.0,
                ..s
            }),
        ];
        println!(
            "network | workload | target mean, lowest, at 70 s | sent, delivered (Mbit/s) | lost % | queue p95, p99, max (ms) | pacer max (packets) | target back after the bad patch (s)"
        );
        for (net, capacity, with) in networks {
            for (work, base) in workloads {
                let trace = simulate(with(Scenario { secs: 70.0, ..base }));
                let lowest = trace
                    .targets
                    .iter()
                    .filter(|s| s.0 >= 10.0)
                    .map(|s| s.1)
                    .fold(f64::INFINITY, f64::min);
                let (_, p95, max) = trace.queue_stats(30.0, 70.0);
                let mut queue: Vec<f64> = trace
                    .queue
                    .iter()
                    .filter(|q| q.0 >= 30.0 && q.0 < 70.0)
                    .map(|q| q.1)
                    .collect();
                queue.sort_by(f64::total_cmp);
                let p99 = queue.get(queue.len() * 99 / 100).copied().unwrap_or(0.0);
                let bytes = |v: &[(f64, u32)]| -> f64 {
                    v.iter()
                        .filter(|p| p.0 >= 30.0 && p.0 < 70.0)
                        .map(|p| f64::from(p.1))
                        .sum()
                };
                let (sent, delivered) = (bytes(&trace.sent), bytes(&trace.delivered));
                let pacer = trace
                    .samples
                    .iter()
                    .filter(|s| s.t >= 10.0)
                    .map(|s| s.pacer)
                    .max()
                    .unwrap_or(0);
                // After the bad patch of the second network (20 to 23 s): when the
                // target is back at 90% of what it was before.
                let back = if net.contains("bad 3 s") {
                    let before = trace.target_at(19.9);
                    match trace.first_at_or_above(23.0, 0.9 * before) {
                        Some(t) if trace.target_at(23.0) < 0.9 * before => {
                            format!("{:.1}", t - 23.0)
                        }
                        Some(_) => "0 (no drop)".into(),
                        None => "never".into(),
                    }
                } else {
                    "-".into()
                };
                println!(
                    "{net} | {work} | {:.1}, {:.1}, {:.1} | {:.1}, {:.1} | {:.1} | {:.0}, {:.0}, {:.0} | {pacer} | {back}",
                    trace.mean_target(30.0, 70.0) / MBPS,
                    lowest / MBPS,
                    trace.target_at(69.9) / MBPS,
                    sent * 8.0 / 40.0 / MBPS,
                    trace.utilization(30.0, 70.0, capacity * MBPS) * capacity,
                    (1.0 - delivered / sent.max(1.0)).max(0.0) * 100.0,
                    p95,
                    p99,
                    max
                );
            }
        }
    }

    /// A far path with jitter: 300 ms round trip, each packet up to 40 ms late on its
    /// own (so they can arrive out of order), 1% loss,
    /// 20 Mbit/s. The jitter reads as a rising delay trend, but there is no queue:
    /// it used to cut the rate to the floor (0.1 Mbit/s, 11 fps end to end).
    #[test]
    fn a_far_jittery_path_is_not_cut_to_the_floor() {
        let capacity = 20.0 * MBPS;
        let trace = simulate(Scenario {
            secs: 60.0,
            initial_bps: 5.0 * MBPS,
            capacity: |_| 20.0 * MBPS,
            random_loss: 0.01,
            reorder_jitter_ms: 40.0,
            one_way_ms: 150.0,
            ..Default::default()
        });
        let lowest = trace
            .targets
            .iter()
            .filter(|s| s.0 >= 5.0)
            .map(|s| s.1)
            .fold(f64::INFINITY, f64::min);
        println!(
            "[far, jittery 40 ms, 1% loss, C=20M] target mean 20-60 s {:.2} Mbps, lowest after 5 s {:.2} Mbps",
            trace.mean_target(20.0, 60.0) / MBPS,
            lowest / MBPS
        );
        report("far-jittery", &trace, 20.0, 60.0, capacity);
        assert!(lowest >= 2.0 * MBPS, "cut to {:.2} Mbps", lowest / MBPS);
        assert!(trace.mean_target(20.0, 60.0) >= 5.0 * MBPS);
    }

    /// On a slow link packets arrive far apart anyway: that is the queue, not the
    /// radio, and the rate must still come down to the link.
    #[test]
    fn a_slow_link_is_still_congestion_not_a_stall() {
        for capacity in [1.5 * MBPS, 0.6 * MBPS] {
            let trace = simulate(Scenario {
                secs: 30.0,
                initial_bps: 8.0 * MBPS,
                capacity: if capacity > 1.0 * MBPS {
                    |_| 1.5 * MBPS
                } else {
                    |_| 0.6 * MBPS
                },
                ..Default::default()
            });
            let (mean_q, p95, _) = trace.queue_stats(15.0, 30.0);
            println!(
                "[slow link {:.1} Mbps from 8] target mean 15-30 s {:.2} Mbps, queue mean {mean_q:.1} ms p95 {p95:.1} ms",
                capacity / MBPS,
                trace.mean_target(15.0, 30.0) / MBPS
            );
            // The queue itself is no different without stall detection: a slow link's
            // standing queue is a matter of its own, not of telling stalls apart.
            assert!(
                trace.mean_target(15.0, 30.0) <= capacity * 1.3,
                "stayed above the link"
            );
        }
    }

    /// At the floor, an encoder that cannot fill its tiny budget must not keep the
    /// rate there: the host logged 0.3 Mbit/s for a minute of video on a fast LAN.
    #[test]
    fn an_encoder_starved_at_the_floor_is_not_app_limited() {
        let trace = simulate(Scenario {
            secs: 60.0,
            initial_bps: 300_000.0,
            capacity: |_| 100.0 * MBPS,
            low_budget_fill: 0.6,
            ..Default::default()
        });
        let out = trace.first_at_or_above(0.0, 2.0 * MBPS);
        println!(
            "[starved encoder from the floor, C=100M] 2 Mbps after {out:?} s; at 60 s {:.2} Mbps",
            trace.target_at(59.9) / MBPS
        );
        assert!(
            out.is_some_and(|t| t <= 30.0),
            "stuck at the floor: {out:?}"
        );
    }

    /// A still screen (little to send), one short hiccup on the link, then video:
    /// the hiccup must not leave the rate at the still screen's level for long.
    #[test]
    fn a_hiccup_while_the_screen_is_still_does_not_starve_the_video_after() {
        let capacity = 100.0 * MBPS;
        let trace = simulate(Scenario {
            secs: 40.0,
            initial_bps: 35.0 * MBPS,
            capacity: |t| {
                if (5.0..5.3).contains(&t) {
                    1.0 * MBPS
                } else {
                    100.0 * MBPS
                }
            },
            app_demand: Some(|t| if t < 10.0 { 0.3 * MBPS } else { f64::INFINITY }),
            ..Default::default()
        });
        let low = trace.target_at(9.9);
        let back = trace.first_at_or_above(10.0, 30.0 * MBPS);
        println!(
            "[still, hiccup at 5 s, video from 10 s, C=100M] target before video {:.2} Mbps; at 30 Mbps after {:?} s; at 40 s {:.2} Mbps",
            low / MBPS,
            back.map(|t| t - 10.0),
            trace.target_at(39.9) / MBPS
        );
        report("still-then-video", &trace, 20.0, 40.0, capacity);
        assert!(
            back.is_some_and(|t| t - 10.0 <= 5.0),
            "the video waited {back:?} s for 30 Mbps"
        );
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

    #[test]
    fn a_lower_ceiling_takes_effect_at_once() {
        let t0 = Instant::now();
        let mut c = RateController::new(
            RateConfig {
                initial_bps: 20_000_000,
                min_bps: 300_000,
                max_bps: 200_000_000,
            },
            Mode::Balanced,
            t0,
        );
        c.set_max_bps(2_000_000);
        assert_eq!(c.target_bps(), 2_000_000);
        assert_eq!(c.max_bps(), 2_000_000);
        // Never under the floor.
        c.set_max_bps(1);
        assert_eq!(c.max_bps(), 300_000);
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
        // Still sending at the target: a queue of our own, not a still screen
        // (overuse while app-limited holds the target).
        for i in 0..520u32 {
            c.on_packet_sent(
                SentPacket {
                    tx_seq: 0x4000_0000 + i,
                    frame_id: None,
                    bytes: 1200,
                },
                end + Duration::from_micros(u64::from(i) * 960),
            );
        }
        c.on_feedback(&empty, end + Duration::from_millis(499));
        assert_eq!(c.target_bps(), 10_000_000, "inside the interval");
        c.on_feedback(&empty, end + Duration::from_millis(500));
        assert!(c.target_bps() < 10_000_000, "after the interval");
    }

    #[test]
    fn overuse_while_sending_little_holds_the_target() {
        let t = Instant::now();
        let mut c = controller(t, 10_000_000);
        let end = feed_frames(&mut c, t, 120, frame_bits(10_000_000), 2.0, None);
        assert_eq!(c.delay_state(), DelayState::Overuse);
        c.last_decrease = Some(end);
        c.target = 10_000_000.0;
        let empty = TransportFeedback {
            fb_seq: 0,
            base_tx_seq: 0,
            base_arrival_us: 0,
            statuses: Vec::new(),
        };
        // Nothing sent since: the delay is not a queue this sender built.
        c.on_feedback(&empty, end + Duration::from_millis(600));
        assert_eq!(c.target_bps(), 10_000_000);
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
        // Capacity dominates when 0.8 × C exceeds 1.5 × target, up to 4 × target: a
        // link estimate many times the target (a cellular link's batches) would let a
        // key frame flood it.
        assert_eq!(pace_rate_bps(100_000_000, Some(300_000_000)), 240_000_000);
        assert_eq!(pace_rate_bps(100_000_000, Some(1_000_000_000)), 400_000_000);
        // Target dominates otherwise.
        assert_eq!(pace_rate_bps(100_000_000, Some(120_000_000)), 150_000_000);

        // M0: 100 Mbps at 120 fps. Old formula: capped at half a frame, 4.17 ms.
        let frame_bytes = 100_000_000u64 / 8 / 120;
        let w = pacing_window(frame_bytes, 100_000_000, None, FRAME);
        assert!((w.as_secs_f64() * 1e3 - 4.1667).abs() < 0.01, "{w:?}");
        // Revised formula on a 1 Gbps link: bytes × 8 / 400 Mbps (4 × target) = 2.08 ms.
        let w = pacing_window(frame_bytes, 100_000_000, Some(1_000_000_000), FRAME);
        assert!((w.as_secs_f64() * 1e3 - 2.0833).abs() < 0.01, "{w:?}");
        // A small frame is proportionally shorter.
        let w = pacing_window(10_000, 100_000_000, None, FRAME);
        assert!((w.as_secs_f64() * 1e3 - 0.5333).abs() < 0.01, "{w:?}");
        // A frame of exactly its budget still goes out in half a frame interval.
        let w = pacing_window(frame_bytes, 100_000_000, Some(120_000_000), FRAME);
        assert!((w.as_secs_f64() * 1e3 - 4.1667).abs() < 0.01, "{w:?}");
        // A key frame of four times the budget is not squeezed into it: it takes as
        // long as four frames would, three and a half intervals...
        let w = pacing_window(4 * frame_bytes, 100_000_000, None, FRAME);
        assert!((w.as_secs_f64() * 1e3 - 29.167).abs() < 0.01, "{w:?}");
        // ...but not on a link known to take four times the target: half an
        // interval as before, and two intervals for ten times the budget.
        let w = pacing_window(4 * frame_bytes, 100_000_000, Some(1_000_000_000), FRAME);
        assert!((w.as_secs_f64() * 1e3 - 4.1667).abs() < 0.01, "{w:?}");
        let w = pacing_window(10 * frame_bytes, 100_000_000, Some(1_000_000_000), FRAME);
        assert!((w.as_secs_f64() * 1e3 - 16.667).abs() < 0.01, "{w:?}");
        // Never over more than a quarter of a second, whatever the target says.
        assert_eq!(
            pacing_window(10_000_000, 1_000_000, Some(2_000_000), FRAME),
            PACE_MAX_WINDOW
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
            c.pacing_window(1_000_000, frame, t),
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
