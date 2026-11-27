//! How long typing takes to show at this host (agent.log's stream line): from a key
//! going in to the next picture captured. That is the host's share of typing delay (the
//! program drawing the key, the system composing, capture), the part a viewer's figures
//! cannot see. One measurement at a time for the process: the first key-down not yet
//! shown, matched by the next new picture.

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

static PENDING: Mutex<Option<Instant>> = Mutex::new(None);

/// A key that comes later than this after the last picture is not what it shows.
const TOO_LONG: Duration = Duration::from_secs(2);

/// A key went in (a key-down that should draw something: not a modifier).
pub fn key_down(at: Instant) {
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    if pending.is_none_or(|p| at.duration_since(p) > TOO_LONG) {
        *pending = Some(at);
    }
}

/// A new picture was captured at `at`: how long after the key waiting to be shown.
pub fn picture(at: Instant) -> Option<Duration> {
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    let key = (*pending)?;
    if at < key {
        return None; // captured before the key went in
    }
    *pending = None;
    Some(at - key).filter(|d| *d <= TOO_LONG)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_matched_by_the_next_picture_after_it() {
        let t0 = Instant::now();
        assert_eq!(picture(t0), None);
        key_down(t0);
        key_down(t0 + Duration::from_millis(30)); // the first one still waits
        assert_eq!(picture(t0 - Duration::from_millis(1)), None);
        assert_eq!(
            picture(t0 + Duration::from_millis(40)),
            Some(Duration::from_millis(40))
        );
        assert_eq!(picture(t0 + Duration::from_millis(60)), None);
    }
}
