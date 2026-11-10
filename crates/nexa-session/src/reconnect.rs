//! §4.4 Reconnecting: after the transport drops, try again with exponential
//! backoff for 30 s, then give up (Closed).
//!
//! A reconnect is a new connection: the rendezvous again (with the host's
//! pinned key, so nothing else at that ID is accepted), then the same QUIC
//! and the same handshake, so the host's pairing and consent rules decide
//! exactly as for a first connection. Nothing from the dropped connection is
//! reused to skip them.

use std::time::{Duration, Instant};

/// §4.4: Reconnecting lasts 30 s.
pub const RECONNECT_WINDOW: Duration = Duration::from_secs(30);
pub const FIRST_DELAY: Duration = Duration::from_millis(500);
pub const MAX_DELAY: Duration = Duration::from_secs(8);

#[derive(Clone, Debug)]
pub struct Backoff {
    started: Instant,
    window: Duration,
    next: Duration,
    attempts: u32,
}

impl Backoff {
    pub fn new(now: Instant) -> Self {
        Self::with_window(now, RECONNECT_WINDOW)
    }

    pub fn with_window(now: Instant, window: Duration) -> Self {
        Self {
            started: now,
            window,
            next: FIRST_DELAY,
            attempts: 0,
        }
    }

    /// How long to wait before the next attempt, or `None` when the window
    /// is used up. The last wait is cut short to fit inside the window.
    pub fn next_delay(&mut self, now: Instant) -> Option<Duration> {
        let left = self.remaining(now);
        if left.is_zero() {
            return None;
        }
        let d = self.next.min(left);
        self.next = (self.next * 2).min(MAX_DELAY);
        self.attempts += 1;
        Some(d)
    }

    /// Time left in the window: an attempt may not run past it.
    pub fn remaining(&self, now: Instant) -> Duration {
        (self.started + self.window).saturating_duration_since(now)
    }

    /// Attempts started so far.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    pub fn started(&self) -> Instant {
        self.started
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_up_to_a_cap_and_stops_at_thirty_seconds() {
        let t = Instant::now();
        let mut b = Backoff::new(t);
        let mut at = t;
        let mut delays = Vec::new();
        while let Some(d) = b.next_delay(at) {
            delays.push(d.as_millis());
            // Each attempt takes a second and fails.
            at += d + Duration::from_secs(1);
        }
        assert_eq!(&delays[..6], &[500, 1000, 2000, 4000, 8000, 8000]);
        assert!(at >= t + RECONNECT_WINDOW);
        assert!(at <= t + RECONNECT_WINDOW + Duration::from_secs(1));
        assert_eq!(b.attempts() as usize, delays.len());
        assert_eq!(b.remaining(at), Duration::ZERO);
    }
}
