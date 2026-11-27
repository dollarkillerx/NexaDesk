//! service.log beside the program, in the install folder only administrators can write.

use std::{io::Write, path::PathBuf, sync::Mutex};

use windows_sys::Win32::{Foundation::SYSTEMTIME, System::SystemInformation::GetLocalTime};

const MOST_BYTES: u64 = 1 << 20;
static GATE: Mutex<()> = Mutex::new(());

fn path() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.with_file_name("service.log"))
}

/// Appends a line; a log that cannot be written is simply not written.
pub fn log(line: &str) {
    let _gate = GATE.lock();
    let Some(path) = path() else { return };
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MOST_BYTES) {
        let _ = std::fs::rename(&path, path.with_extension("log.1"));
    }
    let mut t = SYSTEMTIME::default();
    // SAFETY: writes the structure.
    unsafe { GetLocalTime(&mut t) };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} {line}",
            t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
        );
    }
}
