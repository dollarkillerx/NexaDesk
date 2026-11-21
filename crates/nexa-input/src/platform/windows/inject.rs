//! Injection with SendInput: the same contract as the macOS injector (the agent wraps
//! both in `InputSink`). Keys go by scan code, so the Windows keyboard layout gives
//! them meaning as it would a local keyboard's; text goes as Unicode.
//!
//! Windows drops injected input without a word when it would reach a program that runs
//! as administrator and the agent does not: all of it (pointer, clicks, keys) while such
//! a program is in front, and a click on one of its windows. SendInput still reports
//! every event sent (seen on Windows 11 with Task Manager in front, the pointer staying
//! where it was), so the agent looks for itself on each press and writes what it finds
//! to the agent log; when SendInput does report events it did not send, that is logged
//! too. So "the pointer moves but clicks do nothing" can be told from a bug.

use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

use windows::Win32::{
    Foundation::{CloseHandle, GetLastError, HANDLE, HWND, POINT},
    Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation},
    System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    },
    UI::{
        Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
            KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE,
            MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
            MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
            MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
            MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY,
        },
        WindowsAndMessaging::{
            GetCursorPos, GetForegroundWindow, GetSystemMetrics, GetWindowThreadProcessId,
            SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
            WindowFromPoint,
        },
    },
};
use windows::core::PWSTR;

use super::scancode::scancode_of;
use crate::Hid;

/// Keys and buttons are let go when the viewer has not said what it holds for this long.
const SYNC_TIMEOUT: Duration = Duration::from_secs(3);
/// XBUTTON1 and XBUTTON2 in `mouseData`.
const XBUTTON1: i32 = 1;
const XBUTTON2: i32 = 2;
/// A problem that comes back with every event is logged once per this long.
const LOG_EVERY: Duration = Duration::from_secs(10);

#[derive(Default)]
pub struct Injector {
    held_keys: BTreeSet<u32>,
    held_buttons: BTreeSet<u32>,
    last_sync: Option<Instant>,
    pub dropped_keys: u64,
    /// Whether the agent runs as administrator, which no window can turn away.
    elevated: Option<bool>,
    refused: Throttle,
    admin_in_front: Throttle,
    unreachable_click: Throttle,
    /// The last window seen in front, and its program if that is one input cannot reach.
    front: Option<(usize, Option<String>)>,
}

/// One log line now and then for a problem that repeats.
#[derive(Default)]
struct Throttle {
    last: Option<Instant>,
    quiet: u64,
}

impl Throttle {
    /// `Some` when a line may be written now: how many went unwritten since the last.
    fn ready(&mut self, now: Instant) -> Option<u64> {
        if self
            .last
            .is_some_and(|t| now.saturating_duration_since(t) < LOG_EVERY)
        {
            self.quiet += 1;
            return None;
        }
        self.last = Some(now);
        Some(std::mem::take(&mut self.quiet))
    }
}

fn more_since(quiet: u64) -> String {
    if quiet == 0 {
        String::new()
    } else {
        format!(" ({quiet} more since the last line)")
    }
}

/// The program a window belongs to, and whether it runs as administrator (`None`:
/// its rights cannot be read, which is usual for one that does).
struct Owner {
    pid: u32,
    name: String,
    elevated: Option<bool>,
}

impl std::fmt::Display for Owner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let rights = match self.elevated {
            Some(true) => "runs as administrator",
            Some(false) => "does not run as administrator",
            None => "rights not readable, usually one that runs as administrator",
        };
        write!(f, "{} (process {}, {rights})", self.name, self.pid)
    }
}

fn token_elevated(process: HANDLE) -> Option<bool> {
    let mut token = HANDLE::default();
    // SAFETY: a process handle we hold; the token handle is closed below.
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }.ok()?;
    let mut elevation = TOKEN_ELEVATION::default();
    let mut len = 0;
    // SAFETY: a buffer of the size stated, for the class that fills it.
    let read = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some((&raw mut elevation).cast()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    };
    // SAFETY: the token opened above.
    let _ = unsafe { CloseHandle(token) };
    read.ok()?;
    Some(elevation.TokenIsElevated != 0)
}

fn owner_of(window: HWND) -> Option<Owner> {
    if window.is_invalid() {
        return None;
    }
    let mut pid = 0;
    // SAFETY: plain queries on a window handle and a process handle closed below.
    unsafe {
        GetWindowThreadProcessId(window, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut path = [0u16; 260];
        let mut len = path.len() as u32;
        let name = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &mut len,
        )
        .ok()
        .map(|()| {
            let path = String::from_utf16_lossy(&path[..len as usize]);
            path.rsplit('\\').next().unwrap_or_default().to_owned()
        })
        .unwrap_or_else(|| "?".to_owned());
        let elevated = token_elevated(process);
        let _ = CloseHandle(process);
        Some(Owner {
            pid,
            name,
            elevated,
        })
    }
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
        Self {
            // SAFETY: the pseudo handle of this process, which needs no closing.
            elevated: token_elevated(unsafe { GetCurrentProcess() }),
            ..Self::default()
        }
    }

    /// Sends, and logs what Windows refused and which program was in front then.
    fn send(&mut self, what: &str, inputs: &[INPUT]) {
        if inputs.is_empty() {
            return;
        }
        // SAFETY: well-formed INPUT records of the documented size.
        let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) } as usize;
        if sent >= inputs.len() {
            return;
        }
        // SAFETY: read straight after the call that set it.
        let error = unsafe { GetLastError() }.0;
        let Some(quiet) = self.refused.ready(Instant::now()) else {
            return;
        };
        // SAFETY: a plain query.
        let front = owner_of(unsafe { GetForegroundWindow() })
            .map_or_else(|| "no window".to_owned(), |o| o.to_string());
        eprintln!(
            "nexa-agent: input: Windows refused {} of {} events ({what}), error {error}; in front: {front}{}",
            inputs.len() - sent,
            inputs.len(),
            more_since(quiet),
        );
    }

    /// Whether input from this agent cannot reach `owner`'s windows: it runs as
    /// administrator, or its rights cannot be read, and the agent does not.
    fn out_of_reach(&self, owner: &Owner) -> bool {
        // SAFETY: a plain query.
        self.elevated != Some(true)
            && owner.elevated != Some(false)
            && owner.pid != unsafe { GetCurrentProcessId() }
    }

    /// On a press: while a program that runs as administrator is in front, Windows
    /// drops everything this agent sends. True when that is so.
    fn check_front(&mut self, now: Instant) -> bool {
        if self.elevated == Some(true) {
            return false;
        }
        // SAFETY: a plain query.
        let window = unsafe { GetForegroundWindow() };
        let key = window.0 as usize;
        let blocking = match &self.front {
            Some((k, blocking)) if *k == key => blocking.clone(),
            _ => {
                let blocking = owner_of(window)
                    .filter(|o| self.out_of_reach(o))
                    .map(|o| o.to_string());
                self.front = Some((key, blocking.clone()));
                blocking
            }
        };
        let Some(owner) = blocking else {
            return false;
        };
        if let Some(quiet) = self.admin_in_front.ready(now) {
            eprintln!(
                "nexa-agent: input: {owner} is in front; Windows drops all input from NexaDesk (pointer, clicks, keys) while a program that runs as administrator is in front, unless NexaDesk runs as administrator too{}",
                more_since(quiet),
            );
        }
        true
    }

    /// A press on a window of an administrator program is dropped by Windows unless
    /// the agent runs as administrator too, though SendInput reports it sent.
    fn check_click_target(&mut self, at: Option<(f64, f64)>, now: Instant) {
        if self.elevated == Some(true) {
            return;
        }
        let point = match at {
            Some((x, y)) => POINT {
                x: x.round() as i32,
                y: y.round() as i32,
            },
            None => {
                let mut p = POINT::default();
                // SAFETY: a plain query into a local.
                if unsafe { GetCursorPos(&mut p) }.is_err() {
                    return;
                }
                p
            }
        };
        // SAFETY: plain queries.
        let Some(owner) = owner_of(unsafe { WindowFromPoint(point) }) else {
            return;
        };
        if !self.out_of_reach(&owner) {
            return;
        }
        if let Some(quiet) = self.unreachable_click.ready(now) {
            eprintln!(
                "nexa-agent: input: a click at ({}, {}) is on a window of {owner}; Windows does not pass clicks from NexaDesk to a program that runs as administrator unless NexaDesk does too{}",
                point.x,
                point.y,
                more_since(quiet),
            );
        }
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
        self.send(
            if down { "key down" } else { "key up" },
            &[key_input(code & 0xff, flags)],
        );
        if down && !repeat {
            self.check_front(now);
        }
        if down {
            self.held_keys.insert(hid.0);
        } else {
            self.held_keys.remove(&hid.0);
        }
    }

    /// `x`, `y`: desktop pixels.
    pub fn move_absolute(&mut self, x: f64, y: f64) {
        self.send("pointer move", &[absolute_move(x, y)]);
    }

    pub fn move_relative(&mut self, dx: i32, dy: i32) {
        self.send("pointer move", &[mouse_input(dx, dy, 0, MOUSEEVENTF_MOVE)]);
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
        self.send(if down { "button down" } else { "button up" }, &inputs);
        // One line per press: an administrator program in front explains it already.
        if down && !self.check_front(now) {
            self.check_click_target(at, now);
        }
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
        self.send("wheel", &inputs);
    }

    pub fn text(&mut self, text: &str) {
        let mut inputs = Vec::new();
        for unit in text.encode_utf16() {
            inputs.push(key_input(unit, KEYEVENTF_UNICODE));
            inputs.push(key_input(unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
        }
        self.send("text", &inputs);
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
    fn a_repeating_problem_is_logged_once_per_interval_with_a_count() {
        let t0 = Instant::now();
        let mut throttle = Throttle::default();
        assert_eq!(throttle.ready(t0), Some(0));
        assert_eq!(throttle.ready(t0 + Duration::from_secs(1)), None);
        assert_eq!(throttle.ready(t0 + Duration::from_secs(9)), None);
        assert_eq!(throttle.ready(t0 + LOG_EVERY), Some(2));
        assert_eq!(throttle.ready(t0 + LOG_EVERY * 3), Some(0));
    }

    #[test]
    fn nothing_held_never_times_out() {
        assert!(!Injector::new().timed_out(Instant::now() + SYNC_TIMEOUT * 10));
    }
}
