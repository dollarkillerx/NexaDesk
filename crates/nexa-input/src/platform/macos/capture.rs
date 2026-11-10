//! Keyboard capture (§7.4): the `CGEventTap` backend, the `NSEvent` fallback,
//! and the filter both share.
//!
//! Capture while input is active swallows every key; while inactive, the tap is
//! turned off so the session cannot interfere with the local machine.

use std::collections::BTreeSet;
use std::ptr::{self, NonNull};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSEvent, NSEventMask, NSEventType};
use objc2_core_graphics::{CGEvent, CGEventField, CGEventTapOptions, CGEventType};
use smallvec::SmallVec;

use super::flags::{collapsed_mods, is_down_in_flags};
use super::inject::INJECTED_EVENT_MARKER;
use super::tap::{TapError, TapHandler, TapSpec, TapThread};
use crate::hid::{Hid, ModSet, key};
use crate::keytable::mac_to_hid;
use crate::rewriter::PhysKey;

const MAC_CAPS_LOCK: u16 = 0x39;

/// An output of capture, in physical order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureEvent {
    /// A key for [`Rewriter::on_key`](crate::Rewriter::on_key).
    Key(PhysKey),
    /// The host key was pressed (§7.4). It is never forwarded: leave game mode
    /// or show the toolbar.
    HostKey,
    /// The system disabled the tap for a moment and key releases may be lost.
    /// Call [`Rewriter::reset`](crate::Rewriter::reset).
    Interrupted,
}

/// The local-only chord from §7.4.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostKey {
    pub mods: ModSet,
    pub key: Hid,
}

impl HostKey {
    /// `Ctrl+Option+Cmd+G`, the macOS default. Not Q: `Ctrl+Cmd+Q` is the system
    /// Lock Screen shortcut, which one early-released key would trigger.
    pub fn macos_default() -> Self {
        Self {
            mods: ModSet::from_iter([key::LCTRL, key::LALT, key::LGUI]),
            key: key::G,
        }
    }
}

impl Default for HostKey {
    fn default() -> Self {
        Self::macos_default()
    }
}

/// Settings for [`KeyCapture`] and [`LocalKeyMonitor`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyCaptureConfig {
    pub host_key: HostKey,
    /// Whether input is captured from the start. Normally false until the
    /// assembly layer calls `set_active`.
    pub start_active: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CaptureError {
    /// The tap could not be created: Accessibility is not granted. Fall back to
    /// [`LocalKeyMonitor`] and tell the user Cmd+Tab cannot be captured.
    #[error("accessibility permission is not granted")]
    PermissionDenied,
    #[error("could not start the capture thread")]
    Setup,
}

impl From<TapError> for CaptureError {
    fn from(value: TapError) -> Self {
        match value {
            TapError::Denied => Self::PermissionDenied,
            TapError::Setup => Self::Setup,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RawKind {
    KeyDown,
    KeyUp,
    FlagsChanged,
}

/// The fields of a keyboard event, however it was obtained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RawKey {
    pub kind: RawKind,
    pub keycode: u16,
    pub flags: u64,
    pub autorepeat: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    Pass,
    Swallow,
}

type Events = SmallVec<[CaptureEvent; 6]>;

/// Turns raw keyboard events into [`CaptureEvent`]s and decides their fate.
#[derive(Debug)]
pub(super) struct KeyFilter {
    host: HostKey,
    was_active: bool,
    held_mods: ModSet,
    held_keys: BTreeSet<Hid>,
    /// The host key's main key while physically down; none of it is forwarded.
    swallowing: Option<Hid>,
}

impl KeyFilter {
    pub(super) fn new(host: HostKey) -> Self {
        Self {
            host,
            was_active: false,
            held_mods: ModSet::EMPTY,
            held_keys: BTreeSet::new(),
            swallowing: None,
        }
    }

    fn clear(&mut self) {
        self.held_mods = ModSet::EMPTY;
        self.held_keys.clear();
        self.swallowing = None;
    }

    /// Records a change of session state. Called from `set_active` rather than
    /// discovered from events, since a disabled tap sees none.
    pub(super) fn on_active_changed(&mut self, active: bool) {
        self.clear();
        self.was_active = active;
    }

    /// Forgets held keys after an interruption, when releases may be missing.
    pub(super) fn on_interrupted(&mut self) {
        self.clear();
    }

    pub(super) fn process(&mut self, raw: RawKey, active: bool, out: &mut Events) -> Verdict {
        if active != self.was_active {
            // The Rewriter is reset at the same moment by the assembly layer.
            self.clear();
            self.was_active = active;
        }
        if !active {
            return Verdict::Pass;
        }
        match raw.kind {
            RawKind::FlagsChanged => self.on_flags_changed(raw, out),
            RawKind::KeyDown | RawKind::KeyUp => {
                if let Some(hid) = mac_to_hid(raw.keycode) {
                    self.on_key(raw, hid, out);
                }
            }
        }
        // While active, every key belongs to the session, mapped or not.
        Verdict::Swallow
    }

    fn on_flags_changed(&mut self, raw: RawKey, out: &mut Events) {
        // Caps Lock reports one flags change per toggle, not press and release.
        if raw.keycode == MAC_CAPS_LOCK {
            emit_key(out, key::CAPS_LOCK, true, false);
            emit_key(out, key::CAPS_LOCK, false, false);
            return;
        }
        let Some(hid) = mac_to_hid(raw.keycode).filter(|h| h.is_modifier()) else {
            return; // Fn and other flag-only keys have no HID usage.
        };
        let down = is_down_in_flags(hid, raw.flags).unwrap_or(!self.held_mods.contains(hid));
        match (down, self.held_mods.contains(hid)) {
            (true, false) => {
                self.held_mods.insert(hid);
                emit_key(out, hid, true, false);
            }
            (false, true) => {
                self.held_mods.remove(hid);
                emit_key(out, hid, false, false);
            }
            // Pressed before capture began, or already released for the host key.
            _ => {}
        }
    }

    fn on_key(&mut self, raw: RawKey, hid: Hid, out: &mut Events) {
        let down = raw.kind == RawKind::KeyDown;
        if self.swallowing == Some(hid) {
            if !down {
                self.swallowing = None;
            }
            return;
        }
        if down && !raw.autorepeat && self.is_host_chord(hid, raw.flags) {
            self.swallowing = Some(hid);
            // The chord's modifiers already went out; take them back now.
            for held in self.held_mods.iter() {
                emit_key(out, held, false, false);
            }
            self.held_mods = ModSet::EMPTY;
            out.push(CaptureEvent::HostKey);
            return;
        }
        match (down, raw.autorepeat) {
            (true, false) => {
                self.held_keys.insert(hid);
                emit_key(out, hid, true, false);
            }
            (true, true) if self.held_keys.contains(&hid) => emit_key(out, hid, true, true),
            (true, true) => {} // repeat of a key held before capture began
            (false, _) => {
                if self.held_keys.remove(&hid) {
                    emit_key(out, hid, false, false);
                }
            }
        }
    }

    fn is_host_chord(&self, hid: Hid, flags: u64) -> bool {
        hid == self.host.key && collapsed_mods(flags) == self.host.mods.collapsed()
    }
}

fn emit_key(out: &mut Events, hid: Hid, down: bool, repeat: bool) {
    out.push(CaptureEvent::Key(PhysKey { hid, down, repeat }));
}

/// Shared by the tap and the fallback monitor.
#[derive(Debug)]
struct KeyCore {
    active: AtomicBool,
    filter: Mutex<KeyFilter>,
    tx: Sender<CaptureEvent>,
}

impl KeyCore {
    fn new(config: KeyCaptureConfig, tx: Sender<CaptureEvent>) -> Self {
        let core = Self {
            active: AtomicBool::new(false),
            filter: Mutex::new(KeyFilter::new(config.host_key)),
            tx,
        };
        core.set_active(config.start_active);
        core
    }

    fn set_active(&self, active: bool) {
        // Held-key tracking restarts, as the Rewriter does when it is reset.
        if let Ok(mut filter) = self.filter.lock() {
            filter.on_active_changed(active);
        }
        self.active.store(active, Ordering::SeqCst);
    }

    fn handle(&self, raw: RawKey) -> Verdict {
        let active = self.active.load(Ordering::SeqCst);
        let mut out = Events::new();
        let verdict = self
            .filter
            .lock()
            .map(|mut f| f.process(raw, active, &mut out))
            .unwrap_or(Verdict::Pass);
        for event in out {
            if self.tx.send(event).is_err() {
                // Nobody is listening, so swallowing would just eat the input.
                self.active.store(false, Ordering::SeqCst);
                return Verdict::Pass;
            }
        }
        verdict
    }

    fn interrupted(&self) {
        if let Ok(mut filter) = self.filter.lock() {
            filter.on_interrupted();
        }
        let _ = self.tx.send(CaptureEvent::Interrupted);
    }

    fn panicked(&self) {
        self.active.store(false, Ordering::SeqCst);
    }
}

/// Reads a keyboard event from a tap.
pub(super) fn raw_key_from_cg(kind: CGEventType, event: &CGEvent) -> Option<RawKey> {
    let kind = match kind {
        CGEventType::KeyDown => RawKind::KeyDown,
        CGEventType::KeyUp => RawKind::KeyUp,
        CGEventType::FlagsChanged => RawKind::FlagsChanged,
        _ => return None,
    };
    let field = |f| CGEvent::integer_value_field(Some(event), f);
    Some(RawKey {
        kind,
        keycode: field(CGEventField::KeyboardEventKeycode) as u16,
        flags: CGEvent::flags(Some(event)).bits(),
        autorepeat: field(CGEventField::KeyboardEventAutorepeat) != 0,
    })
}

fn is_ours(event: &CGEvent) -> bool {
    CGEvent::integer_value_field(Some(event), CGEventField::EventSourceUserData)
        == INJECTED_EVENT_MARKER
}

struct KeyTapHandler(KeyCore);

impl TapHandler for KeyTapHandler {
    fn on_event(&self, kind: CGEventType, event: NonNull<CGEvent>) -> *mut CGEvent {
        // SAFETY: the tap passes a valid event for the duration of the callback.
        let cg = unsafe { event.as_ref() };
        if is_ours(cg) {
            return event.as_ptr(); // our own injection must not loop back
        }
        let Some(raw) = raw_key_from_cg(kind, cg) else {
            return event.as_ptr();
        };
        match self.0.handle(raw) {
            Verdict::Pass => event.as_ptr(),
            Verdict::Swallow => ptr::null_mut(),
        }
    }

    fn wants_enabled(&self) -> bool {
        self.0.active.load(Ordering::SeqCst)
    }

    fn on_interrupted(&self) {
        self.0.interrupted();
    }

    fn on_panicked(&self) {
        self.0.panicked();
    }
}

/// Keyboard capture through a `CGEventTap`.
pub struct KeyCapture {
    tap: TapThread<KeyTapHandler>,
}

impl KeyCapture {
    /// Starts the capture thread. Needs Accessibility permission; on
    /// [`CaptureError::PermissionDenied`] use [`LocalKeyMonitor`] instead.
    pub fn start(config: KeyCaptureConfig) -> Result<(Self, Receiver<CaptureEvent>), CaptureError> {
        let (tx, rx) = mpsc::channel();
        let spec = TapSpec {
            name: "input-capture",
            options: CGEventTapOptions::Default,
            mask: event_bit(CGEventType::KeyDown)
                | event_bit(CGEventType::KeyUp)
                | event_bit(CGEventType::FlagsChanged),
        };
        let tap = TapThread::spawn(spec, KeyTapHandler(KeyCore::new(config, tx)))?;
        Ok((Self { tap }, rx))
    }

    /// Turns capture on or off (`nexa_session_set_input_active`). When off, the
    /// tap is disabled and every event reaches the local machine untouched.
    pub fn set_active(&self, active: bool) {
        self.tap.handler().0.set_active(active);
        self.tap.set_enabled(active);
    }

    /// Whether input is currently being captured.
    pub fn is_active(&self) -> bool {
        self.tap.handler().0.active.load(Ordering::SeqCst)
    }
}

fn event_bit(kind: CGEventType) -> u64 {
    1 << kind.0
}

/// The fallback when Accessibility is not granted: an `NSEvent` local monitor.
///
/// It only sees events aimed at this application and cannot stop system
/// shortcuts such as Cmd+Tab, so tell the user (§7.4). Must be installed on the
/// main thread and needs the application's run loop.
pub struct LocalKeyMonitor {
    core: std::sync::Arc<KeyCore>,
    monitor: Retained<AnyObject>,
}

impl LocalKeyMonitor {
    /// Installs the monitor. Returns `None` if AppKit refuses.
    pub fn install(
        config: KeyCaptureConfig,
        _mtm: MainThreadMarker,
    ) -> Option<(Self, Receiver<CaptureEvent>)> {
        let (tx, rx) = mpsc::channel();
        let core = std::sync::Arc::new(KeyCore::new(config, tx));
        let handler_core = std::sync::Arc::clone(&core);
        let block = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
            // SAFETY: AppKit passes a valid event for the duration of the call.
            let ns = unsafe { event.as_ref() };
            let Some(raw) = raw_key_from_ns(ns) else {
                return event.as_ptr();
            };
            match handler_core.handle(raw) {
                Verdict::Pass => event.as_ptr(),
                Verdict::Swallow => ptr::null_mut(),
            }
        });
        let mask = NSEventMask::KeyDown | NSEventMask::KeyUp | NSEventMask::FlagsChanged;
        // SAFETY: the block has the signature AppKit documents for the handler.
        let monitor =
            unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &block) };
        monitor.map(|monitor| (Self { core, monitor }, rx))
    }

    /// See [`KeyCapture::set_active`].
    pub fn set_active(&self, active: bool) {
        self.core.set_active(active);
    }

    /// Whether input is currently being captured.
    pub fn is_active(&self) -> bool {
        self.core.active.load(Ordering::SeqCst)
    }
}

impl Drop for LocalKeyMonitor {
    fn drop(&mut self) {
        // SAFETY: `monitor` is the object AppKit returned from the add call.
        unsafe { NSEvent::removeMonitor(&self.monitor) };
    }
}

fn raw_key_from_ns(event: &NSEvent) -> Option<RawKey> {
    let kind = match event.r#type() {
        NSEventType::KeyDown => RawKind::KeyDown,
        NSEventType::KeyUp => RawKind::KeyUp,
        NSEventType::FlagsChanged => RawKind::FlagsChanged,
        _ => return None,
    };
    let autorepeat = kind != RawKind::FlagsChanged && event.isARepeat();
    Some(RawKey {
        kind,
        keycode: event.keyCode(),
        flags: event.modifierFlags().bits() as u64,
        autorepeat,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keytable::hid_to_mac;
    use objc2_core_graphics::CGEventFlags;

    const CTRL: u64 = 0x40000 | 0x1;
    const OPT: u64 = 0x80000 | 0x20;
    const CMD: u64 = 0x100000 | 0x8;
    const LSHIFT: u64 = 0x20000 | 0x2;

    fn raw(kind: RawKind, hid: Hid, flags: u64, autorepeat: bool) -> RawKey {
        RawKey {
            kind,
            keycode: hid_to_mac(hid).unwrap(),
            flags,
            autorepeat,
        }
    }

    fn down(hid: Hid, flags: u64) -> RawKey {
        raw(RawKind::KeyDown, hid, flags, false)
    }

    fn up(hid: Hid, flags: u64) -> RawKey {
        raw(RawKind::KeyUp, hid, flags, false)
    }

    fn flags(hid: Hid, flags: u64) -> RawKey {
        raw(RawKind::FlagsChanged, hid, flags, false)
    }

    struct Rig {
        filter: KeyFilter,
    }

    impl Rig {
        fn new() -> Self {
            Self {
                filter: KeyFilter::new(HostKey::macos_default()),
            }
        }

        fn feed(&mut self, event: RawKey, active: bool) -> (Verdict, Vec<CaptureEvent>) {
            let mut out = Events::new();
            let verdict = self.filter.process(event, active, &mut out);
            (verdict, out.to_vec())
        }

        fn active(&mut self, event: RawKey) -> Vec<CaptureEvent> {
            let (verdict, out) = self.feed(event, true);
            assert_eq!(verdict, Verdict::Swallow);
            out
        }

        fn press_host_mods(&mut self) {
            self.active(flags(key::LCTRL, CTRL));
            self.active(flags(key::LALT, CTRL | OPT));
            self.active(flags(key::LGUI, CTRL | OPT | CMD));
        }
    }

    fn key_ev(hid: Hid, down: bool, repeat: bool) -> CaptureEvent {
        CaptureEvent::Key(PhysKey { hid, down, repeat })
    }

    #[test]
    fn inactive_events_pass_through_and_emit_nothing() {
        let mut rig = Rig::new();
        for event in [down(key::A, 0), up(key::A, 0), flags(key::LSHIFT, LSHIFT)] {
            assert_eq!(rig.feed(event, false), (Verdict::Pass, vec![]));
        }
    }

    #[test]
    fn inactive_host_chord_is_not_recognized() {
        let mut rig = Rig::new();
        let (verdict, out) = rig.feed(down(key::G, CTRL | OPT | CMD), false);
        assert_eq!((verdict, out), (Verdict::Pass, vec![]));
    }

    #[test]
    fn keys_are_swallowed_and_converted_while_active() {
        let mut rig = Rig::new();
        assert_eq!(
            rig.active(down(key::A, 0)),
            vec![key_ev(key::A, true, false)]
        );
        assert_eq!(
            rig.active(up(key::A, 0)),
            vec![key_ev(key::A, false, false)]
        );
    }

    #[test]
    fn repeats_are_marked() {
        let mut rig = Rig::new();
        rig.active(down(key::A, 0));
        let repeat = raw(RawKind::KeyDown, key::A, 0, true);
        assert_eq!(rig.active(repeat), vec![key_ev(key::A, true, true)]);
        assert_eq!(
            rig.active(up(key::A, 0)),
            vec![key_ev(key::A, false, false)]
        );
    }

    #[test]
    fn repeat_of_a_key_held_before_capture_is_dropped() {
        let mut rig = Rig::new();
        assert_eq!(rig.active(raw(RawKind::KeyDown, key::A, 0, true)), vec![]);
        assert_eq!(rig.active(up(key::A, 0)), vec![]);
    }

    #[test]
    fn unmapped_keys_are_swallowed_silently() {
        let mut rig = Rig::new();
        let fn_key = RawKey {
            kind: RawKind::FlagsChanged,
            keycode: 0x3F,
            flags: 0x800000,
            autorepeat: false,
        };
        assert_eq!(rig.active(fn_key), vec![]);
    }

    #[test]
    fn modifiers_track_left_and_right_from_device_bits() {
        let mut rig = Rig::new();
        assert_eq!(
            rig.active(flags(key::RSHIFT, 0x20000 | 0x4)),
            vec![key_ev(key::RSHIFT, true, false)]
        );
        assert_eq!(
            rig.active(flags(key::LSHIFT, 0x20000 | 0x4 | 0x2)),
            vec![key_ev(key::LSHIFT, true, false)]
        );
        assert_eq!(
            rig.active(flags(key::RSHIFT, 0x20000 | 0x2)),
            vec![key_ev(key::RSHIFT, false, false)]
        );
        assert_eq!(
            rig.active(flags(key::LSHIFT, 0x100)),
            vec![key_ev(key::LSHIFT, false, false)]
        );
    }

    #[test]
    fn modifiers_without_device_bits_toggle() {
        let mut rig = Rig::new();
        assert_eq!(
            rig.active(flags(key::LGUI, 0x100000)),
            vec![key_ev(key::LGUI, true, false)]
        );
        assert_eq!(
            rig.active(flags(key::LGUI, 0)),
            vec![key_ev(key::LGUI, false, false)]
        );
    }

    #[test]
    fn caps_lock_toggle_becomes_a_tap() {
        let mut rig = Rig::new();
        let event = raw(RawKind::FlagsChanged, key::CAPS_LOCK, 0x10000, false);
        assert_eq!(
            rig.active(event),
            vec![
                key_ev(key::CAPS_LOCK, true, false),
                key_ev(key::CAPS_LOCK, false, false),
            ]
        );
    }

    #[test]
    fn host_key_is_recognized_and_never_forwarded() {
        let mut rig = Rig::new();
        rig.press_host_mods();
        let out = rig.active(down(key::G, CTRL | OPT | CMD));
        assert_eq!(out.last(), Some(&CaptureEvent::HostKey));
        assert!(
            !out.iter()
                .any(|e| matches!(e, CaptureEvent::Key(k) if k.hid == key::G))
        );
        // Repeats and the release of G stay local too.
        assert_eq!(
            rig.active(raw(RawKind::KeyDown, key::G, CTRL | OPT | CMD, true)),
            vec![]
        );
        assert_eq!(rig.active(up(key::G, CTRL | OPT | CMD)), vec![]);
    }

    #[test]
    fn host_key_releases_the_chord_modifiers() {
        let mut rig = Rig::new();
        rig.press_host_mods();
        let out = rig.active(down(key::G, CTRL | OPT | CMD));
        for hid in [key::LCTRL, key::LALT, key::LGUI] {
            assert!(
                out.contains(&key_ev(hid, false, false)),
                "{hid} not released"
            );
        }
        // Their physical releases then produce nothing further.
        assert_eq!(rig.active(flags(key::LGUI, CTRL | OPT)), vec![]);
        assert_eq!(rig.active(flags(key::LALT, CTRL)), vec![]);
        assert_eq!(rig.active(flags(key::LCTRL, 0x100)), vec![]);
    }

    #[test]
    fn host_key_needs_exactly_its_modifiers() {
        let mut rig = Rig::new();
        let out = rig.active(down(key::G, CTRL | OPT | CMD | LSHIFT));
        assert_eq!(out, vec![key_ev(key::G, true, false)]);
    }

    #[test]
    fn host_key_ignores_side_and_keyboard_state_bits() {
        let mut rig = Rig::new();
        // Right-hand Command plus Caps Lock and numeric-pad bits.
        let flags = 0x40000 | 0x1 | 0x80000 | 0x20 | 0x100000 | 0x10 | 0x10000 | 0x200000;
        let out = rig.active(down(key::G, flags));
        assert!(out.contains(&CaptureEvent::HostKey));
    }

    #[test]
    fn plain_q_is_an_ordinary_key() {
        let mut rig = Rig::new();
        assert_eq!(
            rig.active(down(key::G, 0)),
            vec![key_ev(key::G, true, false)]
        );
    }

    #[test]
    fn going_inactive_forgets_held_keys() {
        let mut rig = Rig::new();
        rig.active(down(key::A, 0));
        rig.feed(down(key::B, 0), false);
        // After re-activating, the release of the old key is not forwarded.
        assert_eq!(rig.active(up(key::A, 0)), vec![]);
    }

    #[test]
    fn an_interrupt_forgets_held_keys() {
        let mut rig = Rig::new();
        rig.active(down(key::A, 0));
        rig.filter.on_interrupted();
        assert_eq!(rig.active(up(key::A, 0)), vec![]);
    }

    #[test]
    fn toggling_active_forgets_held_modifiers_even_with_no_events_in_between() {
        let (tx, _rx) = mpsc::channel();
        let core = KeyCore::new(
            KeyCaptureConfig {
                start_active: true,
                ..Default::default()
            },
            tx,
        );
        assert_eq!(core.handle(flags(key::LCTRL, CTRL)), Verdict::Swallow);
        // The tap is off while inactive, so the filter sees no events here.
        core.set_active(false);
        core.set_active(true);
        // Held state restarted, so the next press is reported again.
        let mut out = Events::new();
        let verdict = core
            .filter
            .lock()
            .unwrap()
            .process(flags(key::LCTRL, CTRL), true, &mut out);
        assert_eq!(verdict, Verdict::Swallow);
        assert_eq!(out.to_vec(), vec![key_ev(key::LCTRL, true, false)]);
    }

    #[test]
    fn a_consumer_that_goes_away_stops_the_swallowing() {
        let (tx, rx) = mpsc::channel();
        let core = KeyCore::new(
            KeyCaptureConfig {
                start_active: true,
                ..Default::default()
            },
            tx,
        );
        drop(rx);
        assert_eq!(core.handle(down(key::A, 0)), Verdict::Pass);
        assert!(!core.active.load(Ordering::SeqCst));
    }

    #[test]
    fn a_panic_disables_swallowing() {
        let (tx, _rx) = mpsc::channel();
        let core = KeyCore::new(
            KeyCaptureConfig {
                start_active: true,
                ..Default::default()
            },
            tx,
        );
        core.panicked();
        assert_eq!(core.handle(down(key::A, 0)), Verdict::Pass);
    }

    #[test]
    fn cg_key_events_convert_field_for_field() {
        let event = CGEvent::new_keyboard_event(None, hid_to_mac(key::J).unwrap(), true).unwrap();
        CGEvent::set_flags(Some(&event), CGEventFlags::from_bits_retain(CMD));
        CGEvent::set_integer_value_field(Some(&event), CGEventField::KeyboardEventAutorepeat, 1);
        let raw = raw_key_from_cg(CGEventType::KeyDown, &event).unwrap();
        assert_eq!(
            raw,
            RawKey {
                kind: RawKind::KeyDown,
                keycode: hid_to_mac(key::J).unwrap(),
                flags: CMD,
                autorepeat: true,
            }
        );
    }

    #[test]
    fn non_keyboard_event_types_are_ignored() {
        let event = CGEvent::new_keyboard_event(None, 0, true).unwrap();
        assert_eq!(raw_key_from_cg(CGEventType::MouseMoved, &event), None);
    }

    #[test]
    fn our_own_injected_events_are_recognized() {
        let event = CGEvent::new_keyboard_event(None, 0, true).unwrap();
        assert!(!is_ours(&event));
        CGEvent::set_integer_value_field(
            Some(&event),
            CGEventField::EventSourceUserData,
            INJECTED_EVENT_MARKER,
        );
        assert!(is_ours(&event));
    }
}
