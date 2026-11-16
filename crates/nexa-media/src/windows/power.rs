//! The display kept on while someone watches it, as on macOS (`macos::power`).
//!
//! Windows ties the request to the thread that makes it, so the capturer holds it on
//! its own thread for as long as it runs.

use windows::Win32::System::Power::{
    ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
};

/// Keeps the display and the system awake while held, on the thread that made it.
pub struct DisplayAwake(());

impl DisplayAwake {
    pub fn hold() -> Self {
        // SAFETY: a plain request for this thread.
        unsafe {
            SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED)
        };
        Self(())
    }
}

impl Drop for DisplayAwake {
    fn drop(&mut self) {
        // SAFETY: as above: back to the default for this thread.
        unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
    }
}

/// Turns a sleeping display on, as a touch of the mouse would.
pub fn wake_display() {
    // SAFETY: a one-off request: resets the display's idle timer.
    unsafe { SetThreadExecutionState(ES_DISPLAY_REQUIRED) };
}
