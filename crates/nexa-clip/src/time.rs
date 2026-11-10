//! The time abstraction used by the state machines.

use std::ops::Add;
use std::time::{Duration, Instant};

/// A monotonic point in time. Implemented for [`Instant`] in production and
/// [`ManualInstant`] in tests.
pub trait ClipInstant: Copy + Ord + Add<Duration, Output = Self> {
    /// Time elapsed since `earlier`, or zero if `earlier` is in the future.
    fn saturating_since(self, earlier: Self) -> Duration;
}

impl ClipInstant for Instant {
    fn saturating_since(self, earlier: Self) -> Duration {
        self.saturating_duration_since(earlier)
    }
}

/// A manually advanced clock value, in milliseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ManualInstant(pub u64);

impl Add<Duration> for ManualInstant {
    type Output = Self;

    fn add(self, rhs: Duration) -> Self {
        Self(self.0 + rhs.as_millis() as u64)
    }
}

impl ClipInstant for ManualInstant {
    fn saturating_since(self, earlier: Self) -> Duration {
        Duration::from_millis(self.0.saturating_sub(earlier.0))
    }
}
