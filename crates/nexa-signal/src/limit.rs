//! Token buckets, singly and keyed, with a cap on how many keys are tracked.

use std::{collections::HashMap, hash::Hash, time::Instant};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rate {
    /// Tokens added per second.
    pub per_sec: f64,
    pub burst: f64,
}

impl Rate {
    pub const fn new(per_sec: f64, burst: f64) -> Self {
        Self { per_sec, burst }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Bucket {
    tokens: f64,
    last: Instant,
}

impl Bucket {
    pub fn full(rate: Rate, now: Instant) -> Self {
        Self {
            tokens: rate.burst,
            last: now,
        }
    }

    fn refill(&mut self, rate: Rate, now: Instant) {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + dt * rate.per_sec).min(rate.burst);
        self.last = now;
    }

    /// Takes `cost` tokens if there are that many.
    pub fn take(&mut self, rate: Rate, cost: f64, now: Instant) -> bool {
        self.refill(rate, now);
        if self.tokens >= cost {
            self.tokens -= cost;
            true
        } else {
            false
        }
    }

    fn is_full(&mut self, rate: Rate, now: Instant) -> bool {
        self.refill(rate, now);
        self.tokens >= rate.burst
    }
}

/// One bucket per key. When `max_keys` are tracked, idle (full) buckets are
/// dropped; if none are idle, new keys are refused, which fails closed.
#[derive(Debug)]
pub struct Keyed<K> {
    rate: Rate,
    max_keys: usize,
    buckets: HashMap<K, Bucket>,
}

impl<K: Eq + Hash + Clone> Keyed<K> {
    pub fn new(rate: Rate, max_keys: usize) -> Self {
        Self {
            rate,
            max_keys,
            buckets: HashMap::new(),
        }
    }

    pub fn allow(&mut self, key: &K, now: Instant) -> bool {
        self.allow_cost(key, 1.0, now)
    }

    pub fn allow_cost(&mut self, key: &K, cost: f64, now: Instant) -> bool {
        if !self.buckets.contains_key(key) && self.buckets.len() >= self.max_keys {
            let rate = self.rate;
            self.buckets.retain(|_, b| !b.is_full(rate, now));
            if self.buckets.len() >= self.max_keys {
                return false;
            }
        }
        let rate = self.rate;
        self.buckets
            .entry(key.clone())
            .or_insert_with(|| Bucket::full(rate, now))
            .take(rate, cost, now)
    }

    pub fn tracked(&self) -> usize {
        self.buckets.len()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn bucket_bursts_then_refills() {
        let t = Instant::now();
        let rate = Rate::new(2.0, 3.0);
        let mut b = Bucket::full(rate, t);
        assert!(b.take(rate, 1.0, t) && b.take(rate, 1.0, t) && b.take(rate, 1.0, t));
        assert!(!b.take(rate, 1.0, t));
        assert!(b.take(rate, 1.0, t + Duration::from_millis(500)));
        assert!(!b.take(rate, 1.0, t + Duration::from_millis(500)));
    }

    #[test]
    fn keyed_limits_per_key_and_caps_keys() {
        let t = Instant::now();
        let mut k = Keyed::new(Rate::new(1.0, 2.0), 2);
        assert!(k.allow(&1, t) && k.allow(&1, t));
        assert!(!k.allow(&1, t));
        assert!(k.allow(&2, t));
        // Both tracked keys are busy: a third is refused rather than evicting them.
        assert!(!k.allow(&3, t));
        // Once they are idle again, room is made.
        assert!(k.allow(&3, t + Duration::from_secs(10)));
        assert!(k.tracked() <= 2);
    }
}
