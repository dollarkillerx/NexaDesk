//! The paste gate (design §8.3).
//!
//! Sits between input capture and the wire. When the rewriter recognises a
//! paste while the peer has not yet confirmed the clipboard, the gate holds the
//! paste and everything after it, in order, until the confirmation arrives or
//! the deadline passes.
//!
//! A paste pressed while another one is being held records its own requirement
//! in the queue. When the earlier hold ends, the queue is released up to that
//! point and the later paste is then held for its own generation, so a single
//! `until` never has to stand in for two different pastes. Each paste's deadline
//! counts from its own key press, which bounds every event's delay by the
//! timeout.

use std::collections::VecDeque;
use std::time::Duration;

use crate::time::ClipInstant;
use crate::types::Gen;

/// How long a paste may wait for its clipboard (§8.3).
pub const PASTE_GATE_TIMEOUT: Duration = Duration::from_millis(800);

const MAX_NOTICES: usize = 32;

/// How the gate treats an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventClass {
    /// Pointer motion and wheel: never held.
    Pointer,
    /// Keys, buttons and text: held behind a paste, keeping their order.
    Ordered,
}

/// Why a hold ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseReason {
    /// A sufficient generation was reported.
    Applied,
    /// The deadline passed; the toolbar should tell the user (§8.3).
    Deadline,
    /// [`InputGate::flush`] was called.
    Flushed,
}

/// Describes a hold that ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HoldEnded {
    /// The generation the paste was waiting for.
    pub until: Gen,
    /// Why it ended.
    pub reason: ReleaseReason,
    /// How long the paste waited.
    pub waited: Duration,
}

/// Counters for the HUD (§8.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GateStats {
    /// Pastes seen by the gate.
    pub pastes: u64,
    /// Pastes that had to wait.
    pub held: u64,
    /// Holds ended by a confirmation.
    pub released_by_applied: u64,
    /// Holds ended by the deadline.
    pub released_by_deadline: u64,
    /// Total time pastes spent waiting.
    pub total_wait: Duration,
}

#[derive(Clone, Copy, Debug)]
struct Hold<T> {
    until: Gen,
    pressed_at: T,
    deadline: T,
}

#[derive(Debug)]
enum Slot<E, T> {
    Event(E),
    Paste { until: Gen, pressed_at: T },
}

/// Holds input events behind a paste. `E` is opaque to the gate.
///
/// Released events are appended to the `out` vector of each call, in order, so
/// the caller can reuse one buffer on the input path.
#[derive(Debug)]
pub struct InputGate<E, T> {
    timeout: Duration,
    hold: Option<Hold<T>>,
    queue: VecDeque<Slot<E, T>>,
    applied: Option<Gen>,
    stats: GateStats,
    notices: VecDeque<HoldEnded>,
}

impl<E, T: ClipInstant> Default for InputGate<E, T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E, T: ClipInstant> InputGate<E, T> {
    /// Creates a gate with the default deadline.
    pub fn new() -> Self {
        Self::with_timeout(PASTE_GATE_TIMEOUT)
    }

    /// Creates a gate with a custom deadline.
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            timeout,
            hold: None,
            queue: VecDeque::new(),
            applied: None,
            stats: GateStats::default(),
            notices: VecDeque::new(),
        }
    }

    /// Records a paste. Call it before submitting the key that triggers the
    /// paste. `required` is the result of `ClipSync::paste_requirement`.
    pub fn on_paste(&mut self, required: Option<Gen>, now: T, out: &mut Vec<E>) {
        self.expire(now, out);
        self.stats.pastes += 1;
        let Some(until) = required else { return };
        if self.is_applied(until) {
            return;
        }
        self.stats.held += 1;
        if self.hold.is_some() {
            self.queue.push_back(Slot::Paste {
                until,
                pressed_at: now,
            });
        } else {
            self.start_hold(until, now);
        }
    }

    /// Submits an input event. Anything that may leave now lands in `out`.
    pub fn on_event(&mut self, event: E, class: EventClass, now: T, out: &mut Vec<E>) {
        self.expire(now, out);
        if self.hold.is_some() && class == EventClass::Ordered {
            self.queue.push_back(Slot::Event(event));
        } else {
            out.push(event);
        }
    }

    /// Reports a confirmation for `generation` (`ClipCommand::GateRelease`).
    pub fn on_applied(&mut self, generation: Gen, now: T, out: &mut Vec<E>) {
        if self.applied < Some(generation) {
            self.applied = Some(generation);
        }
        if let Some(hold) = self.hold
            && self.is_applied(hold.until)
        {
            self.end_hold(hold, ReleaseReason::Applied, now);
            self.drain(now, out);
        }
        self.expire(now, out);
    }

    /// Releases anything whose deadline has passed.
    pub fn poll(&mut self, now: T, out: &mut Vec<E>) {
        self.expire(now, out);
    }

    /// When [`InputGate::poll`] next needs to run, if a hold is active.
    pub fn next_deadline(&self) -> Option<T> {
        self.hold.map(|hold| hold.deadline)
    }

    /// Whether a paste is currently being held.
    pub fn is_holding(&self) -> bool {
        self.hold.is_some()
    }

    /// Releases everything immediately, for example when the session ends.
    pub fn flush(&mut self, now: T, out: &mut Vec<E>) {
        if let Some(hold) = self.hold.take() {
            self.end_hold(hold, ReleaseReason::Flushed, now);
        }
        for slot in self.queue.drain(..) {
            if let Slot::Event(event) = slot {
                out.push(event);
            }
        }
    }

    /// Counters for the HUD.
    pub fn stats(&self) -> GateStats {
        self.stats
    }

    /// Drains recent hold results. Only the newest few are kept if nobody reads
    /// them.
    pub fn take_notices(&mut self) -> Vec<HoldEnded> {
        self.notices.drain(..).collect()
    }

    fn is_applied(&self, until: Gen) -> bool {
        self.applied.is_some_and(|applied| applied >= until)
    }

    fn start_hold(&mut self, until: Gen, pressed_at: T) {
        self.hold = Some(Hold {
            until,
            pressed_at,
            deadline: pressed_at + self.timeout,
        });
    }

    fn expire(&mut self, now: T, out: &mut Vec<E>) {
        while let Some(hold) = self.hold {
            if now < hold.deadline {
                return;
            }
            self.end_hold(hold, ReleaseReason::Deadline, now);
            self.drain(now, out);
        }
    }

    fn end_hold(&mut self, hold: Hold<T>, reason: ReleaseReason, now: T) {
        self.hold = None;
        let waited = now.saturating_since(hold.pressed_at);
        self.record(hold.until, reason, waited);
    }

    fn record(&mut self, until: Gen, reason: ReleaseReason, waited: Duration) {
        match reason {
            ReleaseReason::Applied => self.stats.released_by_applied += 1,
            ReleaseReason::Deadline => self.stats.released_by_deadline += 1,
            ReleaseReason::Flushed => {}
        }
        self.stats.total_wait += waited;
        if self.notices.len() == MAX_NOTICES {
            self.notices.pop_front();
        }
        self.notices.push_back(HoldEnded {
            until,
            reason,
            waited,
        });
    }

    /// Releases queued events until the next paste that still has to wait.
    fn drain(&mut self, now: T, out: &mut Vec<E>) {
        while let Some(slot) = self.queue.pop_front() {
            match slot {
                Slot::Event(event) => out.push(event),
                Slot::Paste { until, pressed_at } => {
                    if self.is_applied(until) {
                        let waited = now.saturating_since(pressed_at);
                        self.record(until, ReleaseReason::Applied, waited);
                    } else if pressed_at + self.timeout <= now {
                        let waited = now.saturating_since(pressed_at);
                        self.record(until, ReleaseReason::Deadline, waited);
                    } else {
                        self.start_hold(until, pressed_at);
                        return;
                    }
                }
            }
        }
    }
}
