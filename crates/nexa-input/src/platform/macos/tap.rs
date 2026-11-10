//! A `CGEventTap` on its own thread and `CFRunLoop`, shared by key and pointer
//! capture.

use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread::{self, JoinHandle};

use objc2_core_foundation::{
    CFMachPort, CFRetained, CFRunLoop, kCFRunLoopCommonModes, kCFRunLoopDefaultMode,
};
use objc2_core_graphics::{
    CGEvent, CGEventMask, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType,
};

/// Upper bound on how long stopping can take if `CFRunLoopStop` lands before
/// the thread has entered the loop.
const RUN_SLICE_SECONDS: f64 = 0.25;

/// What a tap does with its events. Called on the tap's thread.
pub(super) trait TapHandler: Send + Sync + 'static {
    /// Returns the event to deliver, or null to swallow it.
    fn on_event(&self, kind: CGEventType, event: NonNull<CGEvent>) -> *mut CGEvent;
    /// Whether a tap the system disabled should be turned back on.
    fn wants_enabled(&self) -> bool;
    /// The system disabled the tap, so events may have been lost.
    fn on_interrupted(&self) {}
    /// `on_event` panicked; the event was passed through.
    fn on_panicked(&self) {}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TapError {
    /// `CGEventTapCreate` returned null: the permission is missing.
    Denied,
    /// The thread or run-loop source could not be set up.
    Setup,
}

struct SharedPort(CFRetained<CFMachPort>);
// SAFETY: CFMachPort is a CF object, and CGEventTapEnable may be called from
// any thread; nothing else is reached through this handle.
unsafe impl Send for SharedPort {}
unsafe impl Sync for SharedPort {}

struct SharedLoop(CFRetained<CFRunLoop>);
// SAFETY: CFRunLoopStop and CFRunLoopWakeUp are documented as thread-safe.
unsafe impl Send for SharedLoop {}

struct Inner<H> {
    handler: H,
    port: OnceLock<SharedPort>,
}

/// Decides what to do with the system's "your tap was disabled" notices.
pub(super) fn should_reenable(kind: CGEventType, wanted: bool) -> bool {
    is_disabled_notice(kind) && wanted
}

fn is_disabled_notice(kind: CGEventType) -> bool {
    kind == CGEventType::TapDisabledByTimeout || kind == CGEventType::TapDisabledByUserInput
}

unsafe extern "C-unwind" fn trampoline<H: TapHandler>(
    _proxy: CGEventTapProxy,
    kind: CGEventType,
    event: NonNull<CGEvent>,
    user: *mut c_void,
) -> *mut CGEvent {
    // SAFETY: `user` is the `Inner` that the tap thread keeps alive for as long
    // as this tap exists (see `run_tap`).
    let inner = unsafe { &*user.cast::<Inner<H>>() };
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        if is_disabled_notice(kind) {
            // The re-enable happens first: a handler failure must not leave
            // the tap off, and the handler only records the interruption.
            if should_reenable(kind, inner.handler.wants_enabled())
                && let Some(port) = inner.port.get()
            {
                CGEvent::tap_enable(&port.0, true);
            }
            inner.handler.on_interrupted();
            event.as_ptr()
        } else {
            inner.handler.on_event(kind, event)
        }
    }));
    outcome.unwrap_or_else(|_| {
        // Always let the event through: a broken capture must not eat input.
        let _ = catch_unwind(AssertUnwindSafe(|| inner.handler.on_panicked()));
        event.as_ptr()
    })
}

pub(super) struct TapSpec {
    pub name: &'static str,
    pub options: CGEventTapOptions,
    pub mask: CGEventMask,
}

/// Owns the tap's thread. Dropping it stops the thread and removes the tap.
pub(super) struct TapThread<H: TapHandler> {
    inner: Arc<Inner<H>>,
    stop: Arc<AtomicBool>,
    run_loop: Arc<Mutex<Option<SharedLoop>>>,
    thread: Option<JoinHandle<()>>,
}

impl<H: TapHandler> TapThread<H> {
    pub(super) fn spawn(spec: TapSpec, handler: H) -> Result<Self, TapError> {
        let inner = Arc::new(Inner {
            handler,
            port: OnceLock::new(),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let run_loop = Arc::new(Mutex::new(None));
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);

        let thread = {
            let inner = Arc::clone(&inner);
            let stop = Arc::clone(&stop);
            let run_loop = Arc::clone(&run_loop);
            thread::Builder::new()
                .name(spec.name.to_owned())
                .spawn(move || run_tap(spec, inner, stop, run_loop, ready_tx))
                .map_err(|_| TapError::Setup)?
        };

        let result = ready_rx.recv().unwrap_or(Err(TapError::Setup));
        if let Err(error) = result {
            let _ = thread.join();
            return Err(error);
        }
        Ok(Self {
            inner,
            stop,
            run_loop,
            thread: Some(thread),
        })
    }

    pub(super) fn handler(&self) -> &H {
        &self.inner.handler
    }

    /// Turns delivery on or off. A disabled tap does not see input at all, so
    /// it cannot delay or lose anything.
    pub(super) fn set_enabled(&self, enabled: bool) {
        if let Some(port) = self.inner.port.get() {
            CGEvent::tap_enable(&port.0, enabled);
        }
    }

    #[cfg(test)]
    pub(super) fn is_enabled(&self) -> bool {
        self.inner
            .port
            .get()
            .is_some_and(|port| CGEvent::tap_is_enabled(&port.0))
    }
}

impl<H: TapHandler> Drop for TapThread<H> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(guard) = self.run_loop.lock()
            && let Some(run_loop) = guard.as_ref()
        {
            run_loop.0.stop();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_tap<H: TapHandler>(
    spec: TapSpec,
    inner: Arc<Inner<H>>,
    stop: Arc<AtomicBool>,
    run_loop_slot: Arc<Mutex<Option<SharedLoop>>>,
    ready: mpsc::SyncSender<Result<(), TapError>>,
) {
    // SAFETY: only touches the calling thread's scheduling class.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
    }

    let user = Arc::as_ptr(&inner).cast_mut().cast::<c_void>();
    // SAFETY: `trampoline::<H>` matches the callback contract, and `user` stays
    // valid because this function holds `inner` until after the tap is
    // invalidated.
    let port = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            spec.options,
            spec.mask,
            Some(trampoline::<H>),
            user,
        )
    };
    let Some(port) = port else {
        let _ = ready.send(Err(TapError::Denied));
        return;
    };
    let source = CFMachPort::new_run_loop_source(None, Some(&port), 0);
    let run_loop = CFRunLoop::current();
    let (Some(source), Some(run_loop)) = (source, run_loop) else {
        port.invalidate();
        let _ = ready.send(Err(TapError::Setup));
        return;
    };

    // SAFETY: these are constant CFStrings exported by CoreFoundation.
    let (common_modes, default_mode) = unsafe { (kCFRunLoopCommonModes, kCFRunLoopDefaultMode) };
    run_loop.add_source(Some(&source), common_modes);
    let _ = inner.port.set(SharedPort(port.clone()));
    CGEvent::tap_enable(&port, inner.handler.wants_enabled());
    if let Ok(mut slot) = run_loop_slot.lock() {
        *slot = Some(SharedLoop(run_loop.clone()));
    }
    let _ = ready.send(Ok(()));

    while !stop.load(Ordering::SeqCst) {
        CFRunLoop::run_in_mode(default_mode, RUN_SLICE_SECONDS, false);
    }

    CGEvent::tap_enable(&port, false);
    run_loop.remove_source(Some(&source), common_modes);
    port.invalidate();
    if let Ok(mut slot) = run_loop_slot.lock() {
        *slot = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_disabled_notices_while_wanted_are_reenabled() {
        assert!(should_reenable(CGEventType::TapDisabledByTimeout, true));
        assert!(should_reenable(CGEventType::TapDisabledByUserInput, true));
        assert!(!should_reenable(CGEventType::TapDisabledByTimeout, false));
        assert!(!should_reenable(CGEventType::KeyDown, true));
    }

    struct Idle {
        wanted: AtomicBool,
    }
    impl TapHandler for Idle {
        fn on_event(&self, _: CGEventType, event: NonNull<CGEvent>) -> *mut CGEvent {
            event.as_ptr()
        }
        fn wants_enabled(&self) -> bool {
            self.wanted.load(Ordering::SeqCst)
        }
    }

    /// A real tap, as narrow as possible: listen-only, mouse moves only (no
    /// keys, so no privacy prompt), alive well under the 3 s limit. Nothing is
    /// logged and every event passes untouched.
    #[test]
    fn a_listen_only_tap_starts_and_stops_cleanly() {
        let spec = TapSpec {
            name: "nexa-test-tap",
            options: CGEventTapOptions::ListenOnly,
            mask: 1 << CGEventType::MouseMoved.0,
        };
        let started = std::time::Instant::now();
        let idle = Idle {
            wanted: AtomicBool::new(true),
        };
        let tap = match TapThread::spawn(spec, idle) {
            Ok(tap) => tap,
            Err(TapError::Denied) => return, // no window server access here
            Err(other) => panic!("tap setup failed: {other:?}"),
        };
        // Enabling is asynchronous at the window server, so give it a moment. A
        // disabled tap is told so through its callback, which is the same path
        // the system's timeout takes.
        let settles_to = |tap: &TapThread<Idle>, want: bool| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
            while tap.is_enabled() != want && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            tap.is_enabled() == want
        };
        assert!(settles_to(&tap, true));
        tap.set_enabled(false);
        assert!(
            settles_to(&tap, true),
            "a tap that is still wanted turns itself back on"
        );
        tap.handler().wanted.store(false, Ordering::SeqCst);
        tap.set_enabled(false);
        assert!(settles_to(&tap, false), "an unwanted tap stays off");
        drop(tap);
        assert!(started.elapsed().as_secs_f64() < 3.0);
    }
}
