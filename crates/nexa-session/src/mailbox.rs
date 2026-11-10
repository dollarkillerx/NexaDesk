//! Single-slot "newest wins" hand-off between decode and render (§1.3).

use std::{
    sync::{Condvar, Mutex},
    time::Duration,
};

pub struct Mailbox<T> {
    slot: Mutex<Option<T>>,
    ready: Condvar,
}

impl<T> Default for Mailbox<T> {
    fn default() -> Self {
        Self {
            slot: Mutex::new(None),
            ready: Condvar::new(),
        }
    }
}

impl<T> Mailbox<T> {
    /// Stores `v`, returning the older value it replaced.
    pub fn put(&self, v: T) -> Option<T> {
        let old = self.slot.lock().unwrap().replace(v);
        self.ready.notify_one();
        old
    }

    pub fn take(&self, timeout: Duration) -> Option<T> {
        let g = self.slot.lock().unwrap();
        let (mut g, _) = self
            .ready
            .wait_timeout_while(g, timeout, |s| s.is_none())
            .unwrap();
        g.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_wins() {
        let m = Mailbox::default();
        assert_eq!(m.put(1), None);
        assert_eq!(m.put(2), Some(1));
        assert_eq!(m.take(Duration::ZERO), Some(2));
        assert_eq!(m.take(Duration::from_millis(1)), None);
    }
}
