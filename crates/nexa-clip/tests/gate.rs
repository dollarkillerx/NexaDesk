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

// ------------------------------------------- the peer still downloading the paste

const GRACE_MS: u64 = nexa_clip::PASTE_GATE_PROGRESS_GRACE.as_millis() as u64;
const MAX_HOLD_MS: u64 = nexa_clip::PASTE_GATE_MAX_HOLD.as_millis() as u64;

impl Harness {
    fn progress(&mut self, lamport: u64, at: u64) {
        self.gate.on_progress(generation(lamport), At(at));
    }
}

#[test]
fn a_paste_waits_while_the_peer_is_still_downloading_its_content() {
    let mut h = Harness::new();
    h.paste(Some(generation(5)), 0);
    h.key(7, 1);
    h.progress(5, 500);
    h.poll(DEADLINE_MS + 100);
    assert!(h.take().is_empty(), "the content is still on its way");
    h.progress(5, 3_000);
    h.poll(6_000);
    assert!(h.take().is_empty());
    h.applied(5, 6_500);
    assert_eq!(h.take(), [7]);
    let ended = h.gate.take_notices();
    assert_eq!(ended[0].reason, ReleaseReason::Applied);
    assert_eq!(ended[0].waited, Duration::from_millis(6_500));
}

#[test]
fn a_download_that_started_before_the_paste_counts() {
    let mut h = Harness::new();
    h.progress(5, 100);
    h.paste(Some(generation(5)), 2_000);
    h.key(7, 2_001);
    assert_eq!(h.gate.next_deadline(), Some(At(100 + GRACE_MS)));
}

#[test]
fn a_stalled_download_releases_the_paste_after_the_grace() {
    let mut h = Harness::new();
    h.paste(Some(generation(5)), 0);
    h.key(7, 1);
    h.progress(5, 500);
    h.poll(500 + GRACE_MS - 1);
    assert!(h.take().is_empty());
    h.poll(500 + GRACE_MS);
    assert_eq!(h.take(), [7]);
    assert_eq!(h.gate.take_notices()[0].reason, ReleaseReason::Deadline);
}

#[test]
fn fetching_another_generation_does_not_extend_the_hold() {
    let mut h = Harness::new();
    h.progress(4, 0);
    h.paste(Some(generation(5)), 0);
    h.key(7, 1);
    h.progress(4, 500);
    assert_eq!(h.gate.next_deadline(), Some(At(DEADLINE_MS)));
    h.poll(DEADLINE_MS);
    assert_eq!(h.take(), [7]);
}

#[test]
fn no_paste_is_held_longer_than_the_cap() {
    let mut h = Harness::new();
    h.paste(Some(generation(5)), 0);
    h.key(7, 1);
    let mut at = 0;
    while at < MAX_HOLD_MS + 20_000 {
        h.progress(5, at);
        h.poll(at);
        at += 1_000;
    }
    assert_eq!(h.take(), [7]);
    assert_eq!(
        h.gate.take_notices()[0].waited,
        Duration::from_millis(MAX_HOLD_MS)
    );
}

#[test]
fn a_queued_paste_also_waits_for_its_download() {
    let mut h = Harness::new();
    h.paste(Some(generation(5)), 0);
    h.key(7, 1);
    h.paste(Some(generation(6)), 2);
    h.key(8, 3);
    h.progress(6, 400);
    h.applied(5, 500);
    assert_eq!(h.take(), [7]);
    h.poll(DEADLINE_MS + 100);
    assert!(h.take().is_empty(), "generation 6 is still downloading");
    h.applied(6, 2_000);
    assert_eq!(h.take(), [8]);
}

// A paste recorded before the clipboard thread has said what it waits for: that thread
// was busy (a large picture being read or written) for longer than the input thread
// may wait, and the paste used to go ahead as if nothing were pending.

#[test]
fn an_unanswered_paste_is_held_until_the_answer_names_its_generation() {
    let mut h = Harness::new();
    h.gate.on_paste_unanswered(1, At(0), &mut h.out);
    h.key(7, 1);
    h.pointer(100, 2);
    assert_eq!(h.take(), [100], "the V key waits, the pointer does not");

    h.gate
        .on_paste_answered(1, Some(generation(5)), At(40), &mut h.out);
    assert!(h.take().is_empty(), "now it waits for generation 5");
    h.applied(5, 90);
    assert_eq!(h.take(), [7]);
    let ended = h.gate.take_notices();
    assert_eq!(ended[0].reason, ReleaseReason::Applied);
    assert_eq!(ended[0].until, Some(generation(5)));
    assert_eq!(ended[0].waited, Duration::from_millis(90));
}

#[test]
fn an_unanswered_paste_goes_at_once_when_there_is_nothing_to_wait_for() {
    let mut h = Harness::new();
    h.gate.on_paste_unanswered(1, At(0), &mut h.out);
    h.key(7, 1);
    h.gate.on_paste_answered(1, None, At(35), &mut h.out);
    assert_eq!(h.take(), [7]);
    assert!(!h.gate.is_holding());

    // Also when what it waits for is already there.
    h.applied(5, 40);
    h.gate.on_paste_unanswered(2, At(50), &mut h.out);
    h.key(8, 51);
    h.gate
        .on_paste_answered(2, Some(generation(5)), At(60), &mut h.out);
    assert_eq!(h.take(), [8]);
}

#[test]
fn an_unanswered_paste_gives_up_at_the_deadline_and_ignores_a_stale_answer() {
    let mut h = Harness::new();
    h.gate.on_paste_unanswered(1, At(0), &mut h.out);
    h.key(7, 1);
    h.poll(DEADLINE_MS - 1);
    assert!(h.take().is_empty());
    h.poll(DEADLINE_MS);
    assert_eq!(h.take(), [7]);
    let ended = h.gate.take_notices();
    assert_eq!(ended[0].reason, ReleaseReason::Deadline);
    assert_eq!(ended[0].until, None);

    // The answer for a hold that is over, or for a ticket never given, changes nothing.
    h.paste(Some(generation(9)), DEADLINE_MS + 10);
    h.key(8, DEADLINE_MS + 11);
    h.gate
        .on_paste_answered(1, None, At(DEADLINE_MS + 20), &mut h.out);
    h.gate
        .on_paste_answered(77, None, At(DEADLINE_MS + 21), &mut h.out);
    assert!(h.take().is_empty(), "generation 9 is still awaited");
    h.applied(9, DEADLINE_MS + 30);
    assert_eq!(h.take(), [8]);
}

#[test]
fn an_answered_paste_waits_for_its_download_like_any_other() {
    let mut h = Harness::new();
    h.progress(5, 10);
    h.gate.on_paste_unanswered(1, At(20), &mut h.out);
    h.key(7, 21);
    h.gate
        .on_paste_answered(1, Some(generation(5)), At(60), &mut h.out);
    h.poll(20 + DEADLINE_MS + 100);
    assert!(h.take().is_empty(), "the host is still fetching it");
    h.applied(5, 3_000);
    assert_eq!(h.take(), [7]);
}

#[test]
fn a_late_answer_still_leaves_the_peer_its_time_to_confirm() {
    let mut h = Harness::new();
    h.gate.on_paste_unanswered(1, At(0), &mut h.out);
    h.key(7, 1);
    // The clipboard thread took 700 ms over a large picture; the peer hears of the
    // copy only now.
    h.gate
        .on_paste_answered(1, Some(generation(5)), At(700), &mut h.out);
    h.poll(DEADLINE_MS + 100);
    assert!(h.take().is_empty(), "100 ms would not be a fair chance");
    h.applied(5, 1_000);
    assert_eq!(h.take(), [7]);
    assert_eq!(
        h.gate.take_notices()[0].waited,
        Duration::from_millis(1_000),
        "counted from the key press"
    );

    // Without a confirmation it gives up one timeout after the answer.
    h.gate.on_paste_unanswered(2, At(2_000), &mut h.out);
    h.key(8, 2_001);
    h.gate
        .on_paste_answered(2, Some(generation(6)), At(2_700), &mut h.out);
    h.poll(2_700 + DEADLINE_MS - 1);
    assert!(h.take().is_empty());
    h.poll(2_700 + DEADLINE_MS);
    assert_eq!(h.take(), [8]);
}

#[test]
fn an_unanswered_paste_behind_another_keeps_its_place() {
    let mut h = Harness::new();
    h.paste(Some(generation(5)), 0);
    h.key(7, 1);
    h.gate.on_paste_unanswered(1, At(2), &mut h.out);
    h.key(8, 3);
    h.gate.on_paste_unanswered(2, At(4), &mut h.out);
    h.key(9, 5);
    // The second's answer comes while the first is still held: nothing to wait for.
    h.gate.on_paste_answered(2, None, At(10), &mut h.out);
    h.gate
        .on_paste_answered(1, Some(generation(6)), At(11), &mut h.out);
    assert!(h.take().is_empty());
    h.applied(5, 20);
    assert_eq!(h.take(), [7], "the next paste waits for generation 6");
    h.applied(6, 30);
    assert_eq!(h.take(), [8, 9]);
}
