//! Paste gate behaviour (design §8.3).

use std::time::Duration;

use nexa_clip::{
    DeviceId, EventClass, Gen, InputGate, ManualInstant as At, PASTE_GATE_TIMEOUT, ReleaseReason,
};

fn generation(lamport: u64) -> Gen {
    (lamport, DeviceId([1; 16]))
}

const DEADLINE_MS: u64 = PASTE_GATE_TIMEOUT.as_millis() as u64;

struct Harness {
    gate: InputGate<u32, At>,
    out: Vec<u32>,
}

impl Harness {
    fn new() -> Self {
        Self {
            gate: InputGate::new(),
            out: Vec::new(),
        }
    }

    fn paste(&mut self, required: Option<Gen>, at: u64) {
        self.gate.on_paste(required, At(at), &mut self.out);
    }

    fn key(&mut self, event: u32, at: u64) {
        self.gate
            .on_event(event, EventClass::Ordered, At(at), &mut self.out);
    }

    fn pointer(&mut self, event: u32, at: u64) {
        self.gate
            .on_event(event, EventClass::Pointer, At(at), &mut self.out);
    }

    fn applied(&mut self, lamport: u64, at: u64) {
        self.gate
            .on_applied(generation(lamport), At(at), &mut self.out);
    }

    fn poll(&mut self, at: u64) {
        self.gate.poll(At(at), &mut self.out);
    }

    fn take(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.out)
    }
}

#[test]
fn applied_before_paste_does_not_hold() {
    let mut h = Harness::new();
    h.applied(5, 0);
    h.paste(Some(generation(5)), 10);
    h.key(1, 10);
    assert!(!h.gate.is_holding());
    assert_eq!(h.take(), [1]);
}

#[test]
fn paste_with_nothing_outstanding_does_not_hold() {
    let mut h = Harness::new();
    h.paste(None, 0);
    h.key(1, 0);
    assert_eq!(h.take(), [1]);
    assert_eq!(h.gate.next_deadline(), None);
}

#[test]
fn applied_during_hold_releases_in_order() {
    let mut h = Harness::new();
    h.key(1, 0);
    h.paste(Some(generation(5)), 10);
    h.key(2, 10);
    h.key(3, 20);
    h.key(4, 30);
    assert_eq!(h.take(), [1]);
    assert!(h.gate.is_holding());

    h.applied(5, 40);
    assert_eq!(h.take(), [2, 3, 4]);
    assert!(!h.gate.is_holding());

    h.key(5, 50);
    assert_eq!(h.take(), [5]);
    let notices = h.gate.take_notices();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].reason, ReleaseReason::Applied);
    assert_eq!(notices[0].waited, Duration::from_millis(30));
}

#[test]
fn older_generation_does_not_release_and_newer_does() {
    let mut h = Harness::new();
    h.paste(Some(generation(5)), 0);
    h.key(1, 0);
    h.applied(4, 10);
    assert_eq!(h.take(), Vec::<u32>::new());
    h.applied(6, 20);
    assert_eq!(h.take(), [1]);
}

#[test]
fn deadline_expiry_releases_in_order_and_reports_it() {
    let mut h = Harness::new();
    h.paste(Some(generation(5)), 100);
    h.key(1, 100);
    h.key(2, 200);
    assert_eq!(h.gate.next_deadline(), Some(At(100 + DEADLINE_MS)));

    h.poll(100 + DEADLINE_MS - 1);
    assert_eq!(h.take(), Vec::<u32>::new());

    h.poll(100 + DEADLINE_MS);
    assert_eq!(h.take(), [1, 2]);
    assert_eq!(h.gate.stats().released_by_deadline, 1);
    assert_eq!(h.gate.take_notices()[0].reason, ReleaseReason::Deadline);
}

#[test]
fn a_late_poll_never_lets_a_new_event_overtake_the_buffer() {
    let mut h = Harness::new();
    h.paste(Some(generation(5)), 0);
    h.key(1, 0);
    h.key(2, DEADLINE_MS + 500);
    assert_eq!(h.take(), [1, 2]);
    h.paste(Some(generation(7)), 2000);
    h.key(3, 2000);
    h.pointer(4, 2000 + DEADLINE_MS);
    assert_eq!(h.take(), [3, 4]);
}

#[test]
fn late_applied_after_deadline_is_harmless() {
    let mut h = Harness::new();
    h.paste(Some(generation(5)), 0);
    h.key(1, 0);
    h.poll(DEADLINE_MS);
    h.applied(5, DEADLINE_MS + 10);
    h.key(2, DEADLINE_MS + 20);
    assert_eq!(h.take(), [1, 2]);
}

#[test]
fn pointer_events_bypass_while_buttons_and_keys_keep_order() {
    let mut h = Harness::new();
    h.paste(Some(generation(5)), 0);
    h.key(1, 0);
    h.pointer(100, 1);
    h.key(2, 2); // a button press
    h.pointer(101, 3);
    assert_eq!(h.take(), [100, 101]);
    h.applied(5, 4);
    assert_eq!(h.take(), [1, 2]);
}

#[test]
fn newer_copy_during_hold_without_a_second_paste_uses_the_first_generation() {
    let mut h = Harness::new();
    h.paste(Some(generation(5)), 0);
    h.key(1, 0);
    // A copy producing generation 6 happens here: the held paste still refers
    // to generation 5, so its confirmation is enough.
    h.applied(5, 10);
    assert_eq!(h.take(), [1]);
    assert!(!h.gate.is_holding());
}

#[test]
fn newer_copy_during_hold_skipping_the_older_ack_also_releases() {
    let mut h = Harness::new();
    h.paste(Some(generation(5)), 0);
    h.key(1, 0);
    h.applied(6, 10);
    assert_eq!(h.take(), [1]);
}

#[test]
fn second_paste_during_hold_waits_for_its_own_generation() {
    let mut h = Harness::new();
    h.paste(Some(generation(5)), 0);
    h.key(1, 0);
    h.paste(Some(generation(6)), 100);
    h.key(2, 100);
    h.key(3, 110);
    assert_eq!(h.take(), Vec::<u32>::new());

    h.applied(5, 120);
    assert_eq!(h.take(), [1]);
    assert!(h.gate.is_holding());

    h.applied(6, 130);
    assert_eq!(h.take(), [2, 3]);
    assert!(!h.gate.is_holding());
    assert_eq!(h.gate.stats().released_by_applied, 2);
}

#[test]
fn second_paste_is_satisfied_at_once_if_its_generation_was_already_confirmed() {
    let mut h = Harness::new();
    h.paste(Some(generation(5)), 0);
    h.key(1, 0);
    h.paste(Some(generation(6)), 10);
    h.key(2, 10);
    h.applied(6, 20);
    assert_eq!(h.take(), [1, 2]);
    assert!(!h.gate.is_holding());
}

#[test]
fn second_paste_deadline_counts_from_its_own_press() {
    let mut h = Harness::new();
    h.paste(Some(generation(5)), 0);
    h.key(1, 0);
    h.paste(Some(generation(6)), 300);
    h.key(2, 300);

    h.poll(DEADLINE_MS);
    assert_eq!(h.take(), [1]);
    assert_eq!(h.gate.next_deadline(), Some(At(300 + DEADLINE_MS)));

    h.poll(300 + DEADLINE_MS);
    assert_eq!(h.take(), [2]);
    assert!(!h.gate.is_holding());
}

#[test]
fn one_late_poll_can_expire_several_queued_pastes_in_order() {
    let mut h = Harness::new();
    h.paste(Some(generation(5)), 0);
    h.key(1, 0);
    h.paste(Some(generation(6)), 100);
    h.key(2, 100);
    h.poll(5000);
    assert_eq!(h.take(), [1, 2]);
    assert_eq!(h.gate.stats().released_by_deadline, 2);
}

#[test]
fn flush_releases_everything_in_order() {
    let mut h = Harness::new();
    h.paste(Some(generation(5)), 0);
    h.key(1, 0);
    h.paste(Some(generation(6)), 10);
    h.key(2, 10);
    h.gate.flush(At(20), &mut h.out);
    assert_eq!(h.take(), [1, 2]);
    assert!(!h.gate.is_holding());
}

#[test]
fn stats_count_hits_and_waiting_time() {
    let mut h = Harness::new();
    h.paste(None, 0);
    h.paste(Some(generation(5)), 0);
    h.applied(5, 25);
    let stats = h.gate.stats();
    assert_eq!(stats.pastes, 2);
    assert_eq!(stats.held, 1);
    assert_eq!(stats.total_wait, Duration::from_millis(25));
}
