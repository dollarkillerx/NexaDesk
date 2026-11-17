//! Injection with SendInput: the same contract as the macOS injector (the agent wraps
//! both in `InputSink`). Keys go by scan code, so the Windows keyboard layout gives
//! them meaning as it would a local keyboard's; text goes as Unicode.

use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

use windows::Win32::UI::{
    Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
        KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE,
        MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
        MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
        MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
        MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY,
    },
    WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    },
};

use super::scancode::scancode_of;
use crate::Hid;

/// Keys and buttons are let go when the viewer has not said what it holds for this long.
const SYNC_TIMEOUT: Duration = Duration::from_secs(3);
/// XBUTTON1 and XBUTTON2 in `mouseData`.
const XBUTTON1: i32 = 1;
const XBUTTON2: i32 = 2;

#[derive(Default)]
pub struct Injector {
    held_keys: BTreeSet<u32>,
    held_buttons: BTreeSet<u32>,
    last_sync: Option<Instant>,
    pub dropped_keys: u64,
}

fn send(inputs: &[INPUT]) {
    if inputs.is_empty() {
        return;
    }
    // SAFETY: well-formed INPUT records of the documented size.
    unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
}

fn key_input(scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn mouse_input(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// A desktop position in pixels as SendInput's 0..=65535 over the virtual desktop.
fn normalized(x: f64, y: f64) -> (i32, i32) {
    // SAFETY: plain metric queries.
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
        )
    };
    let nx = ((x - f64::from(vx)) * 65535.0 / f64::from(vw - 1)).round();
    let ny = ((y - f64::from(vy)) * 65535.0 / f64::from(vh - 1)).round();
    (nx.clamp(0.0, 65535.0) as i32, ny.clamp(0.0, 65535.0) as i32)
}

fn absolute_move(x: f64, y: f64) -> INPUT {
    let (nx, ny) = normalized(x, y);
    mouse_input(
        nx,
        ny,
        0,
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
    )
}

/// Down and up flags, and `mouseData`, for a wire button (0 left, 1 right, 2 middle, 3–4 side).
fn button_flags(button: u32, down: bool) -> Option<(MOUSE_EVENT_FLAGS, i32)> {
    Some(match (button, down) {
        (0, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (0, false) => (MOUSEEVENTF_LEFTUP, 0),
        (1, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (1, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (2, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (2, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (3, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
        (3, false) => (MOUSEEVENTF_XUP, XBUTTON1),
        (4, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
        (4, false) => (MOUSEEVENTF_XUP, XBUTTON2),
        _ => return None,
    })
}

impl Injector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn key(&mut self, hid: Hid, down: bool, repeat: bool, now: Instant) {
        let Some(code) = scancode_of(hid) else {
            self.dropped_keys += 1;
            return;
        };
        if down && !repeat {
            self.restart_timeout_if_idle(now);
        }
        let mut flags = KEYEVENTF_SCANCODE;
        if code & 0xE000 == 0xE000 {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !down {
            flags |= KEYEVENTF_KEYUP;
        }
        send(&[key_input(code & 0xff, flags)]);
        if down {
            self.held_keys.insert(hid.0);
        } else {
            self.held_keys.remove(&hid.0);
        }
    }

    /// `x`, `y`: desktop pixels.
    pub fn move_absolute(&mut self, x: f64, y: f64) {
        send(&[absolute_move(x, y)]);
    }

    pub fn move_relative(&mut self, dx: i32, dy: i32) {
        send(&[mouse_input(dx, dy, 0, MOUSEEVENTF_MOVE)]);
    }

    pub fn button(&mut self, button: u32, down: bool, at: Option<(f64, f64)>, now: Instant) {
        let Some((flags, data)) = button_flags(button, down) else {
            return;
        };
        if down {
            self.restart_timeout_if_idle(now);
        }
        let mut inputs = Vec::with_capacity(2);
        if let Some((x, y)) = at {
            inputs.push(absolute_move(x, y));
        }
        inputs.push(mouse_input(0, 0, data, flags));
        send(&inputs);
        if down {
            self.held_buttons.insert(button);
        } else {
            self.held_buttons.remove(&button);
        }
    }

    /// 120ths of a notch, up and left positive (the wire's convention).
    pub fn wheel(&mut self, dx: i32, dy: i32) {
        let mut inputs = Vec::with_capacity(2);
        if dy != 0 {
            inputs.push(mouse_input(0, 0, dy, MOUSEEVENTF_WHEEL));
        }
        if dx != 0 {
            // Windows' horizontal wheel is right-positive.
            inputs.push(mouse_input(0, 0, -dx, MOUSEEVENTF_HWHEEL));
        }
        send(&inputs);
    }

    pub fn text(&mut self, text: &str) {
        let mut inputs = Vec::new();
        for unit in text.encode_utf16() {
            inputs.push(key_input(unit, KEYEVENTF_UNICODE));
            inputs.push(key_input(unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
        }
        send(&inputs);
    }

    /// A new press after a quiet period starts a fresh 3 s window, as on macOS. Without
    /// it a viewer that never says what it holds (a browser) had every key and button let
    /// go at the first `poll_timeout`, 100 ms into any pause: a held Ctrl, Shift or drag
    /// ended before the next key or move.
    fn restart_timeout_if_idle(&mut self, now: Instant) {
        if self.held_keys.is_empty() && self.held_buttons.is_empty() {
            self.last_sync = Some(now);
        }
    }

    /// Lets go of keys the viewer no longer holds.
    pub fn apply_key_state(&mut self, pressed: &[u32], now: Instant) {
        self.last_sync = Some(now);
        let stale: Vec<u32> = self
            .held_keys
            .iter()
            .copied()
            .filter(|k| !pressed.contains(k))
            .collect();
        for k in stale {
            self.key(Hid(k), false, false, now);
        }
    }

    pub fn poll_timeout(&mut self, now: Instant) {
        if self.timed_out(now) {
            self.release_all(now);
        }
    }

    /// Whether something is held and the viewer has not confirmed it for `SYNC_TIMEOUT`.
    fn timed_out(&self, now: Instant) -> bool {
        let holding = !self.held_keys.is_empty() || !self.held_buttons.is_empty();
        holding
            && self
                .last_sync
                .is_none_or(|t| now.saturating_duration_since(t) >= SYNC_TIMEOUT)
    }

    pub fn release_all(&mut self, now: Instant) {
        for b in self.held_buttons.clone() {
            self.button(b, false, None, now);
        }
        for k in self.held_keys.clone() {
            self.key(Hid(k), false, false, now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the bookkeeping that decides when held input is let go: nothing is sent, so
    /// the desktop running the tests gets no keys. `key` and `button` call
    /// `restart_timeout_if_idle` for each press as below.
    #[test]
    fn a_viewer_that_never_says_what_it_holds_keeps_a_held_key_for_the_timeout() {
        let t0 = Instant::now();
        let mut injector = Injector::new();
        // A browser: Ctrl goes down and no key state ever follows.
        injector.restart_timeout_if_idle(t0);
        injector.held_keys.insert(0x0007_00E0);
        assert!(!injector.timed_out(t0 + Duration::from_millis(100)));
        assert!(!injector.timed_out(t0 + Duration::from_millis(2900)));
        assert!(injector.timed_out(t0 + SYNC_TIMEOUT));

        // A second key while Ctrl is held does not stretch the window.
        injector.restart_timeout_if_idle(t0 + Duration::from_secs(2));
        assert!(injector.timed_out(t0 + SYNC_TIMEOUT));

        // Let go of everything; the next press starts again.
        injector.held_keys.clear();
        assert!(!injector.timed_out(t0 + Duration::from_secs(10)));
        let t1 = t0 + Duration::from_secs(10);
        injector.restart_timeout_if_idle(t1);
        injector.held_buttons.insert(0);
        assert!(!injector.timed_out(t1 + Duration::from_millis(500)));
    }

    #[test]
    fn nothing_held_never_times_out() {
        assert!(!Injector::new().timed_out(Instant::now() + SYNC_TIMEOUT * 10));
    }
}
