//! The host's media threads run promptly on Windows. A host has no window and runs in
//! the background (as SYSTEM, from the service), which is just what Windows 11 slows
//! down: efficiency mode, coarse timers, ordinary priority behind whatever is busy. Game
//! streaming hosts register their capture, encode and send threads with the multimedia
//! class scheduler (MMCSS), as these do here; the process-wide part is the agent's
//! (`os_windows::prepare`).

use std::cell::Cell;

use windows::{Win32::System::Threading::AvSetMmThreadCharacteristicsW, core::w};

thread_local! {
    static BOOSTED: Cell<bool> = const { Cell::new(false) };
}

/// Registers the calling thread with MMCSS as media work ("Games": high priority, short
/// waits honoured), once per thread. Nothing happens when MMCSS says no.
pub fn media_thread() {
    if BOOSTED.replace(true) {
        return;
    }
    let mut index = 0u32;
    // SAFETY: a NUL-terminated task name and a local index; the registration lasts for
    // the thread's life, which is what is wanted.
    let _ = unsafe { AvSetMmThreadCharacteristicsW(w!("Games"), &mut index) };
}
