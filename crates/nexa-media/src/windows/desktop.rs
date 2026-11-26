//! Following the input desktop (docs/WINDOWS_SERVICE.md §4).
//!
//! A UAC prompt or the lock screen switches input to another desktop (Winlogon). A thread
//! that captures the screen, reads the cursor or injects input works only on the desktop it
//! is attached to, so each such thread checks, before it works, which desktop receives
//! input now and moves there. Only a process running as SYSTEM may open the Winlogon
//! desktop: anyone else's check fails and the thread stays where it is, which is how the
//! host behaved before (the picture waits on its last frame, input does nothing).
//!
//! `SetThreadDesktop` refuses a thread that owns a window or a hook; the capture, cursor
//! and inject threads own neither.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use windows::Win32::{
    Foundation::HANDLE,
    System::{
        StationsAndDesktops::{
            CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, GetThreadDesktop,
            GetUserObjectInformationW, HDESK, OpenInputDesktop, SetThreadDesktop, UOI_NAME,
        },
        Threading::GetCurrentThreadId,
    },
};

/// GENERIC_ALL: capturing, reading the cursor and injecting all need the desktop fully.
const ALL: DESKTOP_ACCESS_FLAGS = DESKTOP_ACCESS_FLAGS(0x1000_0000);

/// How often a thread looks, unless asked to look now: a few syscalls each time, and a
/// switch is noticed within this long.
const LOOK_EVERY: Duration = Duration::from_millis(200);

#[derive(Default)]
struct Here {
    /// The desktop this thread opened and moved to; closed when it moves again.
    opened: Option<HDESK>,
    looked: Option<Instant>,
}

thread_local! {
    static HERE: RefCell<Here> = RefCell::new(Here::default());
}

/// A desktop's name ("Default", "Winlogon").
fn name_of(desk: HDESK) -> Option<String> {
    let mut buf = [0u16; 64];
    let mut needed = 0u32;
    // SAFETY: the buffer and its length in bytes; `needed` is written.
    unsafe {
        GetUserObjectInformationW(
            HANDLE(desk.0),
            UOI_NAME,
            Some(buf.as_mut_ptr().cast()),
            (buf.len() * 2) as u32,
            Some(&mut needed),
        )
    }
    .ok()?;
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

/// Moves the calling thread to the desktop that receives input now, if it is elsewhere
/// and may open it. Looks at most every [`LOOK_EVERY`] unless `now` is set (before
/// creating something tied to the desktop, such as a duplication). True when it moved.
pub fn follow_input_desktop(now: bool) -> bool {
    HERE.with(|here| {
        let mut here = here.borrow_mut();
        let t = Instant::now();
        if !now && here.looked.is_some_and(|at| t < at + LOOK_EVERY) {
            return false;
        }
        here.looked = Some(t);
        // SAFETY: a plain open; the handle is closed below unless kept.
        let Ok(input) = (unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, ALL) }) else {
            return false; // not ours to open (the secure desktop, unless SYSTEM)
        };
        // SAFETY: the calling thread's own desktop; not to be closed.
        let current = unsafe { GetThreadDesktop(GetCurrentThreadId()) }
            .ok()
            .and_then(name_of);
        let target = name_of(input);
        if target.is_none() || target == current {
            // SAFETY: opened above and not kept.
            let _ = unsafe { CloseDesktop(input) };
            return false;
        }
        // SAFETY: a desktop handle that stays open for as long as the thread is on it.
        if let Err(e) = unsafe { SetThreadDesktop(input) } {
            eprintln!(
                "nexa-agent: desktop: could not move thread {:?} to {}: {e}",
                std::thread::current().name().unwrap_or("?"),
                target.as_deref().unwrap_or("?")
            );
            // SAFETY: opened above and not kept.
            let _ = unsafe { CloseDesktop(input) };
            return false;
        }
        if let Some(old) = here.opened.replace(input) {
            // SAFETY: the one this thread opened before and has just left.
            let _ = unsafe { CloseDesktop(old) };
        }
        eprintln!(
            "nexa-agent: desktop: thread {:?} moved from {} to {}",
            std::thread::current().name().unwrap_or("?"),
            current.as_deref().unwrap_or("?"),
            target.as_deref().unwrap_or("?")
        );
        true
    })
}
