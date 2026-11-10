//! The bulk allocation (§4.9) as a token bucket shared by every bulk writer.
//!
//! quinn's congestion control is off (§4.8), so anything written to a reliable stream
//! leaves at line rate unless the application holds it back. Clipboard transfers and
//! file transfers both take their bytes from one [`BulkLimiter`] per session, whose
//! rate comes from a [`BulkRate`]:
//!
//! - On the host, [`HostBulkRate`]: `bulk = max(0, target − audio − actual video)` from
//!   the session's rate controller, which also counts the bytes as sent.
//! - On the viewer, which has no controller for its uplink (feedback only describes
//!   the host → viewer direction), [`UplinkScavenger`]: a delay-based scavenger that
//!   backs off as soon as the path's round trip grows, so an upload never queues in
//!   front of input and feedback datagrams.
//!
//! Both are floored at [`BULK_FLOOR_BPS`]. §4.9 gives game mode no bulk at all; a
//! trickle instead keeps a running transfer and a clipboard fetch (which gives up
//! after 10 s without progress) alive.

use std::{
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use nexa_net::rate::RateController;

/// Never less than this, whatever the allocation says.
pub const BULK_FLOOR_BPS: u64 = 1_000_000;
/// The viewer's uplink scavenger never goes above this (the old fixed cap).
pub const UPLINK_CEILING_BPS: u64 = 100_000_000;
/// How much may go out at once after an idle spell.
const BURST: Duration = Duration::from_millis(20);
const MAX_SLEEP: Duration = Duration::from_millis(50);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Where the bulk rate comes from.
pub trait BulkRate: Send + Sync {
    /// Bits per second bulk writes may use now.
    fn bulk_bps(&self, now: Instant) -> u64;
    /// `bytes` of bulk data were just handed to QUIC.
    fn on_sent(&self, bytes: u64, now: Instant) {
        let _ = (bytes, now);
    }
}

/// A constant rate, for tests and for anything without a better source.
pub struct FixedRate(pub u64);

impl BulkRate for FixedRate {
    fn bulk_bps(&self, _: Instant) -> u64 {
        self.0
    }
}

/// §4.9 on the host: the allocation's `bulk_bps`, and the bytes reported back so the
/// app-limited rule sees them.
pub struct HostBulkRate(pub Arc<Mutex<RateController>>);

impl BulkRate for HostBulkRate {
    fn bulk_bps(&self, now: Instant) -> u64 {
        lock(&self.0).allocation(now).bulk_bps
    }

    fn on_sent(&self, bytes: u64, now: Instant) {
        lock(&self.0).on_bulk_sent(bytes, now);
    }
}

/// A LEDBAT-style rate from round-trip samples: grow while the round trip stays
/// within [`Self::TARGET_QUEUE`] of the lowest seen, shrink as soon as it does not.
#[derive(Debug, Clone)]
pub struct DelayScavenger {
    rate: f64,
    /// Lowest round trip per [`Self::BUCKET`], newest last (LEDBAT's base history).
    base: std::collections::VecDeque<(Instant, Duration)>,
    last: Option<Instant>,
}

impl Default for DelayScavenger {
    fn default() -> Self {
        Self {
            rate: Self::START_BPS as f64,
            base: std::collections::VecDeque::new(),
            last: None,
        }
    }
}

impl DelayScavenger {
    pub const START_BPS: u64 = 4_000_000;
    /// Queueing delay the scavenger tolerates before it backs off.
    pub const TARGET_QUEUE: Duration = Duration::from_millis(20);
    const STEP: Duration = Duration::from_millis(100);
    /// Growth per step while the queue is short: about 2.6× per second.
    const GROWTH: f64 = 1.1;
    const BACKOFF: f64 = 0.7;
    /// The lowest round trip is kept per bucket for [`Self::BUCKETS`] buckets, so a
    /// queue that lasts less than a minute is never mistaken for the path's delay,
    /// while a path change is followed within a minute.
    const BUCKET: Duration = Duration::from_secs(10);
    const BUCKETS: usize = 6;

    fn base_rtt(&mut self, rtt: Duration, now: Instant) -> Duration {
        match self.base.back_mut() {
            Some((start, min)) if now.saturating_duration_since(*start) < Self::BUCKET => {
                *min = (*min).min(rtt);
            }
            _ => {
                self.base.push_back((now, rtt));
                if self.base.len() > Self::BUCKETS {
                    self.base.pop_front();
                }
            }
        }
        self.base.iter().map(|(_, m)| *m).min().unwrap_or(rtt)
    }

    /// Feeds one round-trip sample and returns the rate to use.
    pub fn update(&mut self, rtt: Duration, now: Instant) -> u64 {
        let base = self.base_rtt(rtt, now);
        let due = self
            .last
            .is_none_or(|l| now.saturating_duration_since(l) >= Self::STEP);
        if due {
            self.last = Some(now);
            let queue = rtt.saturating_sub(base);
            if queue > Self::TARGET_QUEUE {
                self.rate *= Self::BACKOFF;
            } else if queue < Self::TARGET_QUEUE / 2 {
                self.rate *= Self::GROWTH;
            }
            self.rate = self
                .rate
                .clamp(BULK_FLOOR_BPS as f64, UPLINK_CEILING_BPS as f64);
        }
        self.rate as u64
    }
}

/// The viewer's uplink: [`DelayScavenger`] over the connection's smoothed round trip.
pub struct UplinkScavenger {
    conn: quinn::Connection,
    state: Mutex<DelayScavenger>,
}

impl UplinkScavenger {
    pub fn new(conn: quinn::Connection) -> Self {
        Self {
            conn,
            state: Mutex::new(DelayScavenger::default()),
        }
    }
}

impl BulkRate for UplinkScavenger {
    fn bulk_bps(&self, now: Instant) -> u64 {
        let rtt = self.conn.rtt();
        lock(&self.state).update(rtt, now)
    }
}

struct Bucket {
    /// Bytes that may be sent now; negative after a write larger than what was there.
    tokens: f64,
    last: Instant,
}

/// One session's bulk budget, shared by the clipboard and every file transfer.
pub struct BulkLimiter {
    rate: Arc<dyn BulkRate>,
    bucket: Mutex<Bucket>,
}

impl BulkLimiter {
    pub fn new(rate: Arc<dyn BulkRate>) -> Arc<Self> {
        Arc::new(Self {
            rate,
            bucket: Mutex::new(Bucket {
                tokens: 0.0,
                last: Instant::now(),
            }),
        })
    }

    pub fn fixed(bps: u64) -> Arc<Self> {
        Self::new(Arc::new(FixedRate(bps)))
    }

    /// The rate in force right now, floor applied.
    pub fn current_bps(&self) -> u64 {
        self.rate.bulk_bps(Instant::now()).max(BULK_FLOOR_BPS)
    }

    /// Takes `bytes` from the bucket, or returns how long to wait first.
    fn try_take(&self, bytes: usize, now: Instant) -> Result<(), Duration> {
        let bytes_per_sec = self.rate.bulk_bps(now).max(BULK_FLOOR_BPS) as f64 / 8.0;
        let mut b = lock(&self.bucket);
        let dt = now.saturating_duration_since(b.last).as_secs_f64();
        b.last = now;
        let burst = (bytes_per_sec * BURST.as_secs_f64()).max(bytes as f64);
        b.tokens = (b.tokens + dt * bytes_per_sec).min(burst);
        if b.tokens >= 0.0 {
            b.tokens -= bytes as f64;
            drop(b);
            self.rate.on_sent(bytes as u64, now);
            return Ok(());
        }
        let wait = Duration::from_secs_f64(-b.tokens / bytes_per_sec);
        Err(wait.clamp(Duration::from_micros(500), MAX_SLEEP))
    }

    /// Waits until `bytes` may be written. The rate is read again on every wake, so a
    /// change in the allocation applies to a transfer already under way.
    pub async fn acquire(&self, bytes: usize) {
        loop {
            match self.try_take(bytes, Instant::now()) {
                Ok(()) => return,
                Err(wait) => tokio::time::sleep(wait).await,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bucket_paces_to_the_rate() {
        let limiter = BulkLimiter::fixed(8_000_000); // 1 MB/s
        let t0 = Instant::now();
        let mut now = t0;
        let mut sent = 0usize;
        while now < t0 + Duration::from_secs(2) {
            match limiter.try_take(10_000, now) {
                Ok(()) => sent += 10_000,
                Err(wait) => now += wait,
            }
        }
        let rate = sent as f64 / 2.0;
        assert!((rate - 1_000_000.0).abs() < 60_000.0, "{rate} B/s");
    }

    #[test]
    fn the_floor_applies_to_a_zero_allocation() {
        let limiter = BulkLimiter::fixed(0);
        assert_eq!(limiter.current_bps(), BULK_FLOOR_BPS);
    }

    #[test]
    fn the_scavenger_grows_on_a_short_queue_and_backs_off_on_a_long_one() {
        let mut s = DelayScavenger::default();
        let t0 = Instant::now();
        let base = Duration::from_millis(10);
        let mut rate = 0;
        for i in 0..20 {
            rate = s.update(base, t0 + DelayScavenger::STEP * i);
        }
        assert!(rate > DelayScavenger::START_BPS * 4, "{rate}");
        let grown = rate;
        let later = t0 + DelayScavenger::STEP * 21;
        let backed = s.update(base + Duration::from_millis(40), later);
        assert!(backed < grown, "{backed} < {grown}");
        for i in 0..100 {
            rate = s.update(
                base + Duration::from_millis(60),
                later + DelayScavenger::STEP * (i + 1),
            );
        }
        assert_eq!(
            rate, BULK_FLOOR_BPS,
            "ten seconds of queue are still a queue"
        );
        for i in 0..200 {
            rate = s.update(base, later + DelayScavenger::STEP * (i + 200));
        }
        assert_eq!(rate, UPLINK_CEILING_BPS);
        // A path whose delay really went up is followed once its old minimum ages out.
        let moved = later + Duration::from_secs(60);
        for i in 0..700 {
            rate = s.update(Duration::from_millis(80), moved + DelayScavenger::STEP * i);
        }
        assert!(rate > BULK_FLOOR_BPS * 10, "{rate}");
    }
}
