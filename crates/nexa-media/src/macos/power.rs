//! The display, kept on while someone is watching it and woken when someone comes to.
//!
//! A display that has gone to sleep leaves ScreenCaptureKit's list, so capture of it
//! fails, and nothing a viewer does from afar turns it back on. Apple's Screen Sharing
//! holds the same two assertions: no idle display sleep while connected, and user
//! activity declared to wake it.

use std::{
    sync::atomic::{AtomicU32, Ordering::Relaxed},
    thread,
    time::{Duration, Instant},
};

use objc2_core_foundation::CFString;

type IOPMAssertionID = u32;
const OK: i32 = 0;
/// kIOPMAssertionLevelOn.
const LEVEL_ON: u32 = 255;
/// kIOPMUserActiveLocal: as if someone at the Mac touched it.
const USER_ACTIVE_LOCAL: u32 = 0;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: &CFString,
        level: u32,
        name: &CFString,
        id: *mut IOPMAssertionID,
    ) -> i32;
    fn IOPMAssertionRelease(id: IOPMAssertionID) -> i32;
    fn IOPMAssertionDeclareUserActivity(
        name: &CFString,
        user_type: u32,
        id: *mut IOPMAssertionID,
    ) -> i32;
}

/// Keeps the display from sleeping for idleness while held.
pub struct DisplayAwake(IOPMAssertionID);

impl DisplayAwake {
    /// `None` if power management said no; capture goes on regardless.
    pub fn hold(reason: &str) -> Option<Self> {
        let kind = CFString::from_static_str("PreventUserIdleDisplaySleep");
        let name = CFString::from_str(reason);
        let mut id: IOPMAssertionID = 0;
        // SAFETY: valid CF strings for the call; `id` is written on success.
        let status = unsafe { IOPMAssertionCreateWithName(&kind, LEVEL_ON, &name, &mut id) };
        (status == OK).then_some(Self(id))
    }
}

impl Drop for DisplayAwake {
    fn drop(&mut self) {
        // SAFETY: an assertion this value created and has not released.
        unsafe { IOPMAssertionRelease(self.0) };
    }
}

/// The last user-activity assertion: passed back so the system updates it rather than
/// piling up new ones; it lapses by itself.
static ACTIVITY: AtomicU32 = AtomicU32::new(0);

/// Turns a sleeping display on, as a touch of the mouse would.
pub fn wake_display(reason: &str) {
    let name = CFString::from_str(reason);
    let mut id = ACTIVITY.load(Relaxed);
    // SAFETY: a valid CF string; `id` is read and written as documented.
    let status = unsafe { IOPMAssertionDeclareUserActivity(&name, USER_ACTIVE_LOCAL, &mut id) };
    if status == OK {
        ACTIVITY.store(id, Relaxed);
    }
}

/// Wakes the display and asks `find` again until it says yes or `within` runs out.
pub fn wake_and_wait<T>(
    reason: &str,
    within: Duration,
    find: impl FnMut() -> Option<T>,
) -> Option<T> {
    retry_after(|| wake_display(reason), within, find)
}

fn retry_after<T>(
    wake: impl FnOnce(),
    within: Duration,
    mut find: impl FnMut() -> Option<T>,
) -> Option<T> {
    if let Some(found) = find() {
        return Some(found);
    }
    wake();
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(200));
        if let Some(found) = find() {
            return Some(found);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn found_at_once_needs_no_waking() {
        let mut asked = 0;
        let r = retry_after(
            || panic!("woken"),
            Duration::from_secs(1),
            || {
                asked += 1;
                Some(7)
            },
        );
        assert_eq!((r, asked), (Some(7), 1));
    }

    #[test]
    fn gives_up_after_the_wait() {
        let start = Instant::now();
        let mut woken = false;
        let r: Option<()> = retry_after(|| woken = true, Duration::from_millis(300), || None);
        assert!(r.is_none() && woken);
        assert!(start.elapsed() >= Duration::from_millis(300));
    }

    #[test]
    fn holding_the_display_awake_is_allowed() {
        // Releases on drop; nothing to see but that power management takes it.
        assert!(DisplayAwake::hold("NexaDesk test").is_some());
    }
}
