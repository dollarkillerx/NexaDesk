//! macOS capture and injection: event taps, relative pointer, `CGEvent`
//! injection, and frontmost-app tracking (§7.3–7.5).
//!
//! Real effects on the machine (`CGEventPost`, dissociating the mouse) live only
//! in the `cfg(not(test))` items [`HidTarget`] and [`SystemAssociation`], so the
//! unit tests in this crate cannot reach them.

mod capture;
mod flags;
mod focus;
mod inject;
mod pointer;
mod tap;

pub use capture::{
    CaptureError, CaptureEvent, HostKey, KeyCapture, KeyCaptureConfig, LocalKeyMonitor,
};
pub use focus::{FocusWatcher, TERMINAL_BUNDLE_IDS, classify_bundle_id};
pub use inject::{EventTarget, INJECTED_EVENT_MARKER, Injector, InjectorError, PidTarget};
pub use pointer::{MouseAssociation, PointerAccumulator, PointerCapture};

#[cfg(not(test))]
pub use inject::HidTarget;
#[cfg(not(test))]
pub use pointer::SystemAssociation;

/// Whether this process has Accessibility permission. Never shows a prompt.
pub fn accessibility_granted() -> bool {
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> bool;
    }
    // SAFETY: takes no arguments and only reads the process's TCC state.
    unsafe { AXIsProcessTrusted() }
}
