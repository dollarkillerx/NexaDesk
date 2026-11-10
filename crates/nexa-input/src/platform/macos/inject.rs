//! Input injection and key-state reconciliation (§7.5).
//!
//! Events are built here and handed to an [`EventTarget`]; only that seam
//! decides whether they reach the machine. Tests install a recording target.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventType,
    CGMouseButton, CGScrollEventUnit,
};

use super::flags::{FLAG_ALPHA_SHIFT, flags_for};
use crate::hid::{Hid, ModSet, key};
use crate::keytable::hid_to_mac;

/// Stamped on every injected event; capture passes these through untouched.
pub const INJECTED_EVENT_MARKER: i64 = 0x4E58_4431;

const SYNC_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_UNICODE_UNITS: usize = 20;
const DEFAULT_DOUBLE_CLICK: Duration = Duration::from_millis(500);
const CLICK_SLOP_POINTS: f64 = 4.0;
/// Pixels scrolled per wheel notch. A starting value, to be tuned by hand.
const WHEEL_PIXELS_PER_NOTCH: f64 = 30.0;
const VK_RETURN: u16 = 0x24;
const VK_TAB: u16 = 0x30;

/// Where injected events go.
pub trait EventTarget {
    fn post(&mut self, event: &CGEvent);
    /// The current cursor position in global display points.
    fn cursor(&self) -> (f64, f64);
}

/// Posts at the HID tap: this is what actually drives the computer.
#[cfg(not(test))]
#[derive(Clone, Copy, Debug, Default)]
pub struct HidTarget;

#[cfg(not(test))]
impl EventTarget for HidTarget {
    fn post(&mut self, event: &CGEvent) {
        CGEvent::post(
            objc2_core_graphics::CGEventTapLocation::HIDEventTap,
            Some(event),
        );
    }

    fn cursor(&self) -> (f64, f64) {
        cursor_position()
    }
}

/// Posts to one process with `CGEventPostToPid`.
#[derive(Clone, Copy, Debug)]
pub struct PidTarget {
    pub pid: libc::pid_t,
}

impl EventTarget for PidTarget {
    fn post(&mut self, event: &CGEvent) {
        CGEvent::post_to_pid(self.pid, Some(event));
    }

    fn cursor(&self) -> (f64, f64) {
        cursor_position()
    }
}

fn cursor_position() -> (f64, f64) {
    let event = CGEvent::new(None);
    let point = CGEvent::location(event.as_deref());
    (point.x, point.y)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InjectorError {
    #[error("could not create a CGEventSource")]
    NoEventSource,
}

struct Source(CFRetained<CGEventSource>);
// SAFETY: CGEventSource is a CF object that is safe to use from any thread;
// it is only reached through the owning Injector.
unsafe impl Send for Source {}

#[derive(Clone, Copy, Debug)]
struct LastClick {
    button: u32,
    at: Instant,
    position: (f64, f64),
}

/// Counts consecutive clicks of a button, as macOS itself would.
#[derive(Clone, Debug)]
struct ClickTracker {
    interval: Duration,
    last: Option<LastClick>,
    count: i64,
}

impl ClickTracker {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: None,
            count: 0,
        }
    }

    fn on_down(&mut self, button: u32, position: (f64, f64), now: Instant) -> i64 {
        let continues = self.last.is_some_and(|last| {
            last.button == button
                && now.saturating_duration_since(last.at) <= self.interval
                && (last.position.0 - position.0).abs() <= CLICK_SLOP_POINTS
                && (last.position.1 - position.1).abs() <= CLICK_SLOP_POINTS
        });
        self.count = if continues { self.count + 1 } else { 1 };
        self.last = Some(LastClick {
            button,
            at: now,
            position,
        });
        self.count
    }

    fn on_up(&self) -> i64 {
        self.count.max(1)
    }
}

fn mouse_types(button: u32) -> (CGEventType, CGEventType, CGEventType, CGMouseButton) {
    match button {
        0 => (
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
            CGEventType::LeftMouseDragged,
            CGMouseButton::Left,
        ),
        1 => (
            CGEventType::RightMouseDown,
            CGEventType::RightMouseUp,
            CGEventType::RightMouseDragged,
            CGMouseButton::Right,
        ),
        n => (
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
            CGEventType::OtherMouseDragged,
            CGMouseButton(n),
        ),
    }
}

/// Splits text for `CGEventKeyboardSetUnicodeString`: runs of at most 20 UTF-16
/// units, never cutting a surrogate pair, with newlines and tabs turned into
/// their keys because a unicode event does not perform them.
#[derive(Clone, Debug, PartialEq, Eq)]
enum TextPiece {
    Unicode(Vec<u16>),
    Key(u16),
}

fn split_text(text: &str) -> Vec<TextPiece> {
    let mut pieces = Vec::new();
    let mut run: Vec<u16> = Vec::new();
    let mut units = [0u16; 2];
    for ch in text.chars() {
        let special = match ch {
            '\n' => Some(VK_RETURN),
            '\t' => Some(VK_TAB),
            '\r' => None, // the '\n' of a CRLF carries the line break
            _ => {
                let encoded = ch.encode_utf16(&mut units);
                if run.len() + encoded.len() > MAX_UNICODE_UNITS {
                    pieces.push(TextPiece::Unicode(std::mem::take(&mut run)));
                }
                run.extend_from_slice(encoded);
                continue;
            }
        };
        if let Some(code) = special {
            if !run.is_empty() {
                pieces.push(TextPiece::Unicode(std::mem::take(&mut run)));
            }
            pieces.push(TextPiece::Key(code));
        }
    }
    if !run.is_empty() {
        pieces.push(TextPiece::Unicode(run));
    }
    pieces
}

fn carry_add(carry: &mut f64, delta: f64) -> i32 {
    *carry += delta;
    let whole = carry.trunc();
    *carry -= whole;
    whole as i32
}

/// Injects input and keeps the remote key state honest.
///
/// Dropping it releases anything still held, so a torn-down session cannot
/// leave a key stuck.
pub struct Injector<T: EventTarget> {
    target: T,
    source: Source,
    held_keys: BTreeSet<Hid>,
    held_mods: ModSet,
    caps_on: bool,
    held_buttons: BTreeSet<u32>,
    clicks: ClickTracker,
    last_sync: Instant,
    wheel_carry_h: f64,
    wheel_carry_v: f64,
    dropped_keys: u64,
}

#[cfg(not(test))]
impl Injector<HidTarget> {
    /// An injector that drives the real machine.
    pub fn system() -> Result<Self, InjectorError> {
        Self::new(HidTarget)
    }
}

impl<T: EventTarget> Injector<T> {
    pub fn new(target: T) -> Result<Self, InjectorError> {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .ok_or(InjectorError::NoEventSource)?;
        Ok(Self {
            target,
            source: Source(source),
            held_keys: BTreeSet::new(),
            held_mods: ModSet::EMPTY,
            caps_on: false,
            held_buttons: BTreeSet::new(),
            clicks: ClickTracker::new(DEFAULT_DOUBLE_CLICK),
            last_sync: Instant::now(),
            wheel_carry_h: 0.0,
            wheel_carry_v: 0.0,
            dropped_keys: 0,
        })
    }

    /// Keys this Injector believes are down.
    pub fn held_keys(&self) -> impl Iterator<Item = Hid> + '_ {
        self.held_keys.iter().copied()
    }

    /// Keys dropped because macOS has no keycode for them.
    pub fn dropped_keys(&self) -> u64 {
        self.dropped_keys
    }

    /// Sets the double-click window used for automatic click counting.
    pub fn set_double_click_interval(&mut self, interval: Duration) {
        self.clicks.interval = interval;
    }

    fn flags(&self) -> u64 {
        let caps = if self.caps_on { FLAG_ALPHA_SHIFT } else { 0 };
        flags_for(self.held_mods) | caps
    }

    fn emit(&mut self, event: &CGEvent, flags: u64) {
        CGEvent::set_flags(Some(event), CGEventFlags::from_bits_retain(flags));
        CGEvent::set_integer_value_field(
            Some(event),
            CGEventField::EventSourceUserData,
            INJECTED_EVENT_MARKER,
        );
        self.target.post(event);
    }

    /// Injects a key. Key-down and key-up keep the held set in step.
    pub fn key(&mut self, hid: Hid, down: bool, repeat: bool, now: Instant) {
        let Some(code) = hid_to_mac(hid) else {
            self.dropped_keys += 1;
            return;
        };
        if !down && !self.held_keys.contains(&hid) {
            return; // already released, by reconciliation or a timeout
        }
        if down && !repeat {
            self.restart_timeout_if_idle(now);
            self.held_keys.insert(hid);
            if hid == key::CAPS_LOCK {
                self.caps_on = !self.caps_on;
            }
        } else if !down {
            self.held_keys.remove(&hid);
        }
        if hid.is_modifier() {
            if down {
                self.held_mods.insert(hid);
            } else {
                self.held_mods.remove(hid);
            }
        }
        self.post_key(
            code,
            hid.is_modifier() || hid == key::CAPS_LOCK,
            down,
            repeat,
        );
    }

    /// A new press after a quiet period starts a fresh 3 s window.
    fn restart_timeout_if_idle(&mut self, now: Instant) {
        if self.held_keys.is_empty() && self.held_buttons.is_empty() {
            self.last_sync = now;
        }
    }

    fn post_key(&mut self, code: u16, flags_changed: bool, down: bool, repeat: bool) {
        let Some(event) = CGEvent::new_keyboard_event(Some(&self.source.0), code, down) else {
            return;
        };
        if flags_changed {
            CGEvent::set_type(Some(&event), CGEventType::FlagsChanged);
        }
        CGEvent::set_integer_value_field(
            Some(&event),
            CGEventField::KeyboardEventAutorepeat,
            i64::from(repeat),
        );
        let flags = self.flags();
        self.emit(&event, flags);
    }

    fn move_event_type(&self) -> (CGEventType, CGMouseButton) {
        match self.held_buttons.iter().next() {
            Some(&button) => {
                let (_, _, dragged, cg_button) = mouse_types(button);
                (dragged, cg_button)
            }
            None => (CGEventType::MouseMoved, CGMouseButton::Left),
        }
    }

    /// Moves to an absolute position in global display points. A held button
    /// turns the move into a drag.
    pub fn move_absolute(&mut self, x: f64, y: f64) {
        let (kind, button) = self.move_event_type();
        let at = CGPoint::new(x, y);
        let Some(event) = CGEvent::new_mouse_event(Some(&self.source.0), kind, at, button) else {
            return;
        };
        let flags = self.flags();
        self.emit(&event, flags);
    }

    /// Moves by a relative amount. The delta fields carry the raw movement
    /// for games; the position advances from the current cursor.
    pub fn move_relative(&mut self, dx: i32, dy: i32) {
        let (x, y) = self.target.cursor();
        let (kind, button) = self.move_event_type();
        let at = CGPoint::new(x + f64::from(dx), y + f64::from(dy));
        let Some(event) = CGEvent::new_mouse_event(Some(&self.source.0), kind, at, button) else {
            return;
        };
        CGEvent::set_integer_value_field(
            Some(&event),
            CGEventField::MouseEventDeltaX,
            i64::from(dx),
        );
        CGEvent::set_integer_value_field(
            Some(&event),
            CGEventField::MouseEventDeltaY,
            i64::from(dy),
        );
        let flags = self.flags();
        self.emit(&event, flags);
    }

    /// Presses or releases a mouse button, counting double clicks itself. Pass
    /// `at` to align the pointer first (§3.3); `None` uses the current spot.
    pub fn button(&mut self, button: u32, down: bool, at: Option<(f64, f64)>, now: Instant) {
        let position = at.unwrap_or_else(|| self.target.cursor());
        let clicks = if down {
            self.clicks.on_down(button, position, now)
        } else {
            self.clicks.on_up()
        };
        self.button_with_clicks(button, down, Some(position), clicks, now);
    }

    /// Presses or releases a mouse button with an explicit click count.
    pub fn button_with_clicks(
        &mut self,
        button: u32,
        down: bool,
        at: Option<(f64, f64)>,
        clicks: i64,
        now: Instant,
    ) {
        if !down && !self.held_buttons.contains(&button) {
            return;
        }
        let (x, y) = at.unwrap_or_else(|| self.target.cursor());
        let (down_kind, up_kind, _, cg_button) = mouse_types(button);
        let kind = if down { down_kind } else { up_kind };
        let at = CGPoint::new(x, y);
        let Some(event) = CGEvent::new_mouse_event(Some(&self.source.0), kind, at, cg_button)
        else {
            return;
        };
        CGEvent::set_integer_value_field(
            Some(&event),
            CGEventField::MouseEventButtonNumber,
            i64::from(button),
        );
        CGEvent::set_integer_value_field(Some(&event), CGEventField::MouseEventClickState, clicks);
        if down {
            self.restart_timeout_if_idle(now);
            self.held_buttons.insert(button);
        } else {
            self.held_buttons.remove(&button);
        }
        let flags = self.flags();
        self.emit(&event, flags);
    }

    /// Scrolls by `dx`/`dy` in 1/120 of a notch: positive is right and up.
    /// Fractions of a pixel carry over to the next call.
    pub fn wheel(&mut self, dx: i32, dy: i32) {
        let scale = WHEEL_PIXELS_PER_NOTCH / 120.0;
        let vertical = carry_add(&mut self.wheel_carry_v, f64::from(dy) * scale);
        let horizontal = carry_add(&mut self.wheel_carry_h, f64::from(dx) * scale);
        if vertical == 0 && horizontal == 0 {
            return;
        }
        let Some(event) = CGEvent::new_scroll_wheel_event2(
            Some(&self.source.0),
            CGScrollEventUnit::Pixel,
            2,
            vertical,
            -horizontal, // macOS has positive as left
            0,
        ) else {
            return;
        };
        let flags = self.flags();
        self.emit(&event, flags);
    }

    /// Types `text` independently of the keyboard layout (§7.5). Modifier state
    /// is cleared on these events so held modifiers cannot turn it into
    /// shortcuts.
    pub fn text(&mut self, text: &str) {
        for piece in split_text(text) {
            match piece {
                TextPiece::Key(code) => {
                    for down in [true, false] {
                        if let Some(event) =
                            CGEvent::new_keyboard_event(Some(&self.source.0), code, down)
                        {
                            self.emit(&event, 0);
                        }
                    }
                }
                TextPiece::Unicode(units) => {
                    for down in [true, false] {
                        self.post_unicode(&units, down);
                    }
                }
            }
        }
    }

    fn post_unicode(&mut self, units: &[u16], down: bool) {
        let Some(event) = CGEvent::new_keyboard_event(Some(&self.source.0), 0, down) else {
            return;
        };
        // SAFETY: `units` is a live UTF-16 buffer of exactly the length passed.
        unsafe {
            CGEvent::keyboard_set_unicode_string(Some(&event), units.len() as _, units.as_ptr());
        }
        self.emit(&event, 0);
    }

    /// Applies a `KeyStateSync`: releases whatever is held but no longer
    /// reported as pressed (§7.5). `pressed` holds raw `Hid` values.
    pub fn apply_key_state(&mut self, pressed: &[u32], now: Instant) {
        self.last_sync = now;
        let stale: Vec<Hid> = self
            .held_keys
            .iter()
            .copied()
            .filter(|hid| !pressed.contains(&hid.0))
            .collect();
        for hid in release_order(stale) {
            self.key(hid, false, false, now);
        }
    }

    /// Call periodically: releases everything if keys are held and no
    /// `KeyStateSync` has arrived for 3 s.
    pub fn poll_timeout(&mut self, now: Instant) {
        let holding = !self.held_keys.is_empty() || !self.held_buttons.is_empty();
        if holding && now.saturating_duration_since(self.last_sync) >= SYNC_TIMEOUT {
            self.release_all(now);
        }
    }

    /// Releases all keys and buttons, as on disconnect (§7.5).
    pub fn release_all(&mut self, now: Instant) {
        let buttons: Vec<u32> = self.held_buttons.iter().copied().collect();
        for button in buttons {
            self.button_with_clicks(button, false, None, self.clicks.on_up(), now);
        }
        let keys: Vec<Hid> = self.held_keys.iter().copied().collect();
        for hid in release_order(keys) {
            self.key(hid, false, false, now);
        }
    }
}

/// Ordinary keys first, then modifiers, so no chord fires on the way out.
fn release_order(mut keys: Vec<Hid>) -> Vec<Hid> {
    keys.sort_by_key(|hid| hid.is_modifier());
    keys
}

impl<T: EventTarget> Drop for Injector<T> {
    fn drop(&mut self) {
        self.release_all(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    const LCMD: u64 = 0x100000 | 0x8;
    const LSHIFT: u64 = 0x20000 | 0x2;

    #[derive(Clone, Debug, PartialEq)]
    struct Posted {
        kind: CGEventType,
        keycode: i64,
        flags: u64,
        autorepeat: i64,
        click_state: i64,
        button: i64,
        delta_x: i64,
        delta_y: i64,
        location: (f64, f64),
        scroll_v: i64,
        scroll_h: i64,
        marker: i64,
        text: Vec<u16>,
    }

    fn inspect(event: &CGEvent) -> Posted {
        let field = |f| CGEvent::integer_value_field(Some(event), f);
        let point = CGEvent::location(Some(event));
        let mut text = [0u16; 32];
        let mut length = 0;
        // SAFETY: the buffer and length pointer are valid for the call.
        unsafe {
            CGEvent::keyboard_get_unicode_string(
                Some(event),
                text.len() as _,
                &mut length,
                text.as_mut_ptr(),
            );
        }
        Posted {
            kind: CGEvent::r#type(Some(event)),
            keycode: field(CGEventField::KeyboardEventKeycode),
            flags: CGEvent::flags(Some(event)).bits(),
            autorepeat: field(CGEventField::KeyboardEventAutorepeat),
            click_state: field(CGEventField::MouseEventClickState),
            button: field(CGEventField::MouseEventButtonNumber),
            delta_x: field(CGEventField::MouseEventDeltaX),
            delta_y: field(CGEventField::MouseEventDeltaY),
            location: (point.x, point.y),
            scroll_v: field(CGEventField::ScrollWheelEventPointDeltaAxis1),
            scroll_h: field(CGEventField::ScrollWheelEventPointDeltaAxis2),
            marker: field(CGEventField::EventSourceUserData),
            text: text[..length as usize].to_vec(),
        }
    }

    /// Records instead of posting: nothing reaches the machine.
    #[derive(Clone, Default)]
    struct Recording {
        events: Arc<Mutex<Vec<Posted>>>,
        cursor: (f64, f64),
    }

    impl EventTarget for Recording {
        fn post(&mut self, event: &CGEvent) {
            self.events.lock().unwrap().push(inspect(event));
        }

        fn cursor(&self) -> (f64, f64) {
            self.cursor
        }
    }

    fn rig() -> (Injector<Recording>, Arc<Mutex<Vec<Posted>>>) {
        let target = Recording {
            cursor: (100.0, 200.0),
            ..Recording::default()
        };
        let events = Arc::clone(&target.events);
        (Injector::new(target).unwrap(), events)
    }

    fn take(events: &Arc<Mutex<Vec<Posted>>>) -> Vec<Posted> {
        std::mem::take(&mut *events.lock().unwrap())
    }

    #[test]
    fn key_press_and_release_set_keycode_and_type() {
        let (mut inj, events) = rig();
        let now = Instant::now();
        inj.key(key::A, true, false, now);
        inj.key(key::A, false, false, now);
        let posted = take(&events);
        assert_eq!(posted.len(), 2);
        assert_eq!(posted[0].kind, CGEventType::KeyDown);
        assert_eq!(posted[0].keycode, 0x00);
        assert_eq!(posted[0].autorepeat, 0);
        assert_eq!(posted[1].kind, CGEventType::KeyUp);
        assert_eq!(posted[0].marker, INJECTED_EVENT_MARKER);
    }

    #[test]
    fn repeat_sets_the_autorepeat_field() {
        let (mut inj, events) = rig();
        let now = Instant::now();
        inj.key(key::A, true, false, now);
        inj.key(key::A, true, true, now);
        let posted = take(&events);
        assert_eq!(posted[1].autorepeat, 1);
        assert_eq!(posted[1].kind, CGEventType::KeyDown);
        assert_eq!(inj.held_keys().count(), 1);
        inj.key(key::A, false, false, now);
    }

    #[test]
    fn modifier_keys_are_flags_changed_events() {
        let (mut inj, events) = rig();
        let now = Instant::now();
        inj.key(key::LGUI, true, false, now);
        inj.key(key::LGUI, false, false, now);
        let posted = take(&events);
        assert_eq!(posted[0].kind, CGEventType::FlagsChanged);
        assert_eq!(posted[0].flags, LCMD, "press carries its own flag");
        assert_eq!(posted[1].kind, CGEventType::FlagsChanged);
        assert_eq!(posted[1].flags, 0, "release clears it");
    }

    #[test]
    fn modifier_flags_are_set_explicitly_on_keys() {
        let (mut inj, events) = rig();
        let now = Instant::now();
        inj.key(key::LGUI, true, false, now);
        inj.key(key::LSHIFT, true, false, now);
        inj.key(key::V, true, false, now);
        let last = take(&events).pop().unwrap();
        assert_eq!(last.flags, LCMD | LSHIFT);
        inj.release_all(now);
    }

    #[test]
    fn caps_lock_toggles_and_shows_in_flags() {
        let (mut inj, events) = rig();
        let now = Instant::now();
        inj.key(key::CAPS_LOCK, true, false, now);
        inj.key(key::CAPS_LOCK, false, false, now);
        inj.key(key::A, true, false, now);
        let posted = take(&events);
        assert_eq!(posted[0].kind, CGEventType::FlagsChanged);
        assert_eq!(posted[2].flags, FLAG_ALPHA_SHIFT);
        inj.release_all(now);
    }

    #[test]
    fn keys_without_a_mac_code_are_dropped_and_counted() {
        let (mut inj, events) = rig();
        let now = Instant::now();
        inj.key(key::INTL4, true, false, now);
        assert!(take(&events).is_empty());
        assert_eq!(inj.dropped_keys(), 1);
    }

    #[test]
    fn release_without_a_press_does_nothing() {
        let (mut inj, events) = rig();
        inj.key(key::A, false, false, Instant::now());
        assert!(take(&events).is_empty());
    }

    #[test]
    fn absolute_move_is_a_mouse_moved_event() {
        let (mut inj, events) = rig();
        inj.move_absolute(10.0, 20.0);
        let posted = take(&events);
        assert_eq!(posted[0].kind, CGEventType::MouseMoved);
        assert_eq!(posted[0].location, (10.0, 20.0));
    }

    #[test]
    fn a_held_button_turns_a_move_into_a_drag() {
        let (mut inj, events) = rig();
        let now = Instant::now();
        inj.button(0, true, None, now);
        inj.move_absolute(1.0, 2.0);
        inj.button(0, false, None, now);
        inj.button(1, true, None, now);
        inj.move_absolute(3.0, 4.0);
        inj.button(1, false, None, now);
        inj.button(2, true, None, now);
        inj.move_absolute(5.0, 6.0);
        inj.button(2, false, None, now);
        let posted = take(&events);
        assert_eq!(posted[1].kind, CGEventType::LeftMouseDragged);
        assert_eq!(posted[4].kind, CGEventType::RightMouseDragged);
        assert_eq!(posted[7].kind, CGEventType::OtherMouseDragged);
    }

    #[test]
    fn relative_move_sets_delta_fields_and_advances_from_cursor() {
        let (mut inj, events) = rig();
        inj.move_relative(5, -3);
        let posted = take(&events);
        assert_eq!(posted[0].kind, CGEventType::MouseMoved);
        assert_eq!((posted[0].delta_x, posted[0].delta_y), (5, -3));
        assert_eq!(posted[0].location, (105.0, 197.0));
    }

    #[test]
    fn buttons_use_the_right_event_types_and_button_numbers() {
        let (mut inj, events) = rig();
        let now = Instant::now();
        inj.button(0, true, None, now);
        inj.button(0, false, None, now);
        inj.button(1, true, None, now);
        inj.button(1, false, None, now);
        inj.button(4, true, None, now);
        inj.button(4, false, None, now);
        let posted = take(&events);
        assert_eq!(posted[0].kind, CGEventType::LeftMouseDown);
        assert_eq!(posted[1].kind, CGEventType::LeftMouseUp);
        assert_eq!(posted[2].kind, CGEventType::RightMouseDown);
        assert_eq!(posted[3].kind, CGEventType::RightMouseUp);
        assert_eq!(posted[4].kind, CGEventType::OtherMouseDown);
        assert_eq!(posted[4].button, 4);
        assert_eq!(posted[5].kind, CGEventType::OtherMouseUp);
    }

    #[test]
    fn button_events_align_to_the_given_position() {
        let (mut inj, events) = rig();
        inj.button(0, true, Some((7.0, 8.0)), Instant::now());
        assert_eq!(take(&events)[0].location, (7.0, 8.0));
        inj.release_all(Instant::now());
    }

    #[test]
    fn explicit_click_count_lands_in_click_state() {
        let (mut inj, events) = rig();
        let now = Instant::now();
        inj.button_with_clicks(0, true, None, 3, now);
        inj.button_with_clicks(0, false, None, 3, now);
        let posted = take(&events);
        assert_eq!(posted[0].click_state, 3);
        assert_eq!(posted[1].click_state, 3);
    }

    #[test]
    fn rapid_clicks_count_up_and_reset() {
        let (mut inj, events) = rig();
        let start = Instant::now();
        let click = |inj: &mut Injector<Recording>, at: Instant, pos: (f64, f64)| {
            inj.button(0, true, Some(pos), at);
            inj.button(0, false, Some(pos), at);
        };
        click(&mut inj, start, (1.0, 1.0));
        click(&mut inj, start + Duration::from_millis(100), (2.0, 2.0));
        click(&mut inj, start + Duration::from_millis(200), (1.0, 1.0));
        click(&mut inj, start + Duration::from_secs(5), (1.0, 1.0));
        let downs: Vec<i64> = take(&events)
            .iter()
            .filter(|p| p.kind == CGEventType::LeftMouseDown)
            .map(|p| p.click_state)
            .collect();
        assert_eq!(downs, vec![1, 2, 3, 1]);
    }

    #[test]
    fn a_click_that_moved_away_starts_over() {
        let (mut inj, events) = rig();
        let start = Instant::now();
        inj.button(0, true, Some((0.0, 0.0)), start);
        inj.button(0, false, Some((0.0, 0.0)), start);
        inj.button(0, true, Some((50.0, 0.0)), start);
        let last = take(&events).pop().unwrap();
        assert_eq!(last.click_state, 1);
        inj.release_all(start);
    }

    #[test]
    fn modifiers_apply_to_clicks() {
        let (mut inj, events) = rig();
        let now = Instant::now();
        inj.key(key::LGUI, true, false, now);
        inj.button(0, true, None, now);
        let last = take(&events).pop().unwrap();
        assert_eq!(last.flags, LCMD);
        inj.release_all(now);
    }

    #[test]
    fn wheel_is_expressed_in_pixels_with_the_horizontal_sign_flipped() {
        let (mut inj, events) = rig();
        inj.wheel(120, 240);
        let posted = take(&events);
        assert_eq!(posted[0].kind, CGEventType::ScrollWheel);
        assert_eq!(posted[0].scroll_v, 60);
        assert_eq!(posted[0].scroll_h, -30);
    }

    #[test]
    fn small_wheel_amounts_accumulate() {
        let (mut inj, events) = rig();
        inj.wheel(0, 2); // half a pixel
        assert!(take(&events).is_empty());
        inj.wheel(0, 2);
        assert_eq!(take(&events)[0].scroll_v, 1);
    }

    #[test]
    fn text_uses_unicode_strings_with_no_flags() {
        let (mut inj, events) = rig();
        let now = Instant::now();
        inj.key(key::LSHIFT, true, false, now);
        take(&events);
        inj.text("héllo");
        let posted = take(&events);
        assert_eq!(posted.len(), 2);
        assert_eq!(posted[0].kind, CGEventType::KeyDown);
        assert_eq!(posted[1].kind, CGEventType::KeyUp);
        let expected: Vec<u16> = "héllo".encode_utf16().collect();
        assert_eq!(posted[0].text, expected);
        assert_eq!(posted[0].flags, 0);
        inj.release_all(now);
    }

    #[test]
    fn text_splits_at_twenty_units_without_breaking_surrogates() {
        let text = "😀".repeat(15); // 30 UTF-16 units
        let pieces = split_text(&text);
        assert_eq!(pieces.len(), 2);
        let mut total = 0;
        for piece in &pieces {
            let TextPiece::Unicode(units) = piece else {
                panic!("expected unicode")
            };
            assert!(units.len() <= MAX_UNICODE_UNITS);
            assert_eq!(units.len() % 2, 0, "surrogate pair was cut");
            total += units.len();
        }
        assert_eq!(total, 30);
    }

    #[test]
    fn newlines_and_tabs_become_keys() {
        assert_eq!(
            split_text("a\r\nb\tc"),
            vec![
                TextPiece::Unicode("a".encode_utf16().collect()),
                TextPiece::Key(VK_RETURN),
                TextPiece::Unicode("b".encode_utf16().collect()),
                TextPiece::Key(VK_TAB),
                TextPiece::Unicode("c".encode_utf16().collect()),
            ]
        );
    }

    #[test]
    fn text_with_a_newline_taps_return() {
        let (mut inj, events) = rig();
        inj.text("\n");
        let posted = take(&events);
        assert_eq!(posted.len(), 2);
        assert_eq!(posted[0].keycode, i64::from(VK_RETURN));
        assert_eq!(posted[0].kind, CGEventType::KeyDown);
    }

    #[test]
    fn key_state_sync_releases_only_stale_keys() {
        let (mut inj, events) = rig();
        let now = Instant::now();
        inj.key(key::A, true, false, now);
        inj.key(key::B, true, false, now);
        take(&events);
        inj.apply_key_state(&[key::A.0], now);
        let posted = take(&events);
        assert_eq!(posted.len(), 1);
        assert_eq!(posted[0].kind, CGEventType::KeyUp);
        assert_eq!(posted[0].keycode, i64::from(hid_to_mac(key::B).unwrap()));
        assert_eq!(inj.held_keys().collect::<Vec<_>>(), vec![key::A]);
        inj.release_all(now);
    }

    #[test]
    fn key_state_sync_never_presses_anything() {
        let (mut inj, events) = rig();
        inj.apply_key_state(&[key::A.0], Instant::now());
        assert!(take(&events).is_empty());
    }

    #[test]
    fn three_seconds_without_a_sync_releases_held_keys() {
        let (mut inj, events) = rig();
        let start = Instant::now();
        inj.apply_key_state(&[], start);
        inj.key(key::A, true, false, start + Duration::from_secs(10));
        take(&events);
        inj.poll_timeout(start + Duration::from_secs(12));
        assert!(take(&events).is_empty(), "under the limit");
        inj.poll_timeout(start + Duration::from_secs(13));
        assert_eq!(take(&events).len(), 1, "at the limit");
        assert_eq!(inj.held_keys().count(), 0);
    }

    #[test]
    fn a_button_press_after_idle_starts_a_fresh_timeout() {
        let (mut inj, events) = rig();
        let start = Instant::now();
        inj.apply_key_state(&[], start);
        inj.button(0, true, None, start + Duration::from_secs(10));
        take(&events);
        inj.poll_timeout(start + Duration::from_secs(11));
        assert!(take(&events).is_empty());
        inj.poll_timeout(start + Duration::from_secs(13));
        assert_eq!(take(&events)[0].kind, CGEventType::LeftMouseUp);
    }

    #[test]
    fn a_sync_keeps_held_keys_alive() {
        let (mut inj, events) = rig();
        let start = Instant::now();
        inj.key(key::A, true, false, start);
        take(&events);
        inj.apply_key_state(&[key::A.0], start + Duration::from_secs(2));
        inj.poll_timeout(start + Duration::from_secs(4));
        assert!(take(&events).is_empty());
        inj.release_all(start);
    }

    #[test]
    fn idle_injector_does_nothing_on_timeout() {
        let (mut inj, events) = rig();
        inj.poll_timeout(Instant::now() + Duration::from_secs(60));
        assert!(take(&events).is_empty());
    }

    #[test]
    fn release_all_releases_buttons_keys_then_modifiers() {
        let (mut inj, events) = rig();
        let now = Instant::now();
        inj.key(key::LGUI, true, false, now);
        inj.key(key::A, true, false, now);
        inj.button(0, true, None, now);
        take(&events);
        inj.release_all(now);
        let posted = take(&events);
        assert_eq!(
            posted.iter().map(|p| p.kind).collect::<Vec<_>>(),
            vec![
                CGEventType::LeftMouseUp,
                CGEventType::KeyUp,
                CGEventType::FlagsChanged,
            ]
        );
        assert_eq!(posted[2].flags, 0);
        assert_eq!(inj.held_keys().count(), 0);
    }

    #[test]
    fn dropping_an_injector_releases_what_is_held() {
        let (mut inj, events) = rig();
        inj.key(key::A, true, false, Instant::now());
        take(&events);
        drop(inj);
        assert_eq!(take(&events)[0].kind, CGEventType::KeyUp);
    }

    /// The real delivery path, aimed at a child process that has no event loop,
    /// so nothing else on the machine can see it. A key event only.
    #[test]
    fn events_can_be_posted_to_one_process() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("2")
            .spawn()
            .unwrap();
        let target = PidTarget {
            pid: child.id() as libc::pid_t,
        };
        let mut inj = Injector::new(target).unwrap();
        let now = Instant::now();
        inj.key(key::A, true, false, now);
        inj.key(key::A, false, false, now);
        drop(inj);
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
