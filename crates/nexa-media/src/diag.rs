//! A small line log for the viewer (`viewer.log` next to the host's `agent.log`):
//! what the platform code chose and what failed, so a picture problem on someone's
//! PC leaves a trace. Off until [`set_file`]; never fails the caller.

use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

/// Past this the file is moved to `.1` and a new one started.
const MAX_BYTES: u64 = 1 << 20;

struct Sink {
    path: PathBuf,
    file: File,
    written: u64,
}

static SINK: Mutex<Option<Sink>> = Mutex::new(None);

/// Starts logging to `path` (appending), creating its folder.
pub fn set_file(path: &Path) {
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let Ok(file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let written = file.metadata().map(|m| m.len()).unwrap_or(0);
    *SINK.lock().unwrap_or_else(|e| e.into_inner()) = Some(Sink {
        path: path.to_owned(),
        file,
        written,
    });
}

/// UTC `YYYY-MM-DD HH:MM:SS.mmm` for a time since the epoch.
fn stamp(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let (secs, ms) = (d.as_secs(), d.subsec_millis());
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}.{ms:03}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

/// Writes one line, if a file is set.
pub fn line(args: fmt::Arguments<'_>) {
    let mut sink = SINK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(s) = sink.as_mut() else { return };
    let text = format!("{} {args}\n", stamp(SystemTime::now()));
    if s.written + text.len() as u64 > MAX_BYTES {
        let old = s.path.with_extension("log.1");
        let _ = fs::rename(&s.path, &old);
        if let Ok(f) = OpenOptions::new().create(true).append(true).open(&s.path) {
            s.file = f;
            s.written = 0;
        }
    }
    if s.file.write_all(text.as_bytes()).is_ok() {
        s.written += text.len() as u64;
    }
}

/// `diag!("decoder: {kind}")`: one line in the viewer log.
#[macro_export]
macro_rules! diag {
    ($($t:tt)*) => { $crate::diag::line(format_args!($($t)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_are_utc_calendar_times() {
        assert_eq!(stamp(UNIX_EPOCH), "1970-01-01 00:00:00.000Z");
        let t = UNIX_EPOCH + std::time::Duration::from_millis(1_794_898_800_123);
        assert_eq!(stamp(t), "2026-11-17 07:00:00.123Z");
        let leap = UNIX_EPOCH + std::time::Duration::from_secs(951_782_400); // 2000-02-29
        assert_eq!(&stamp(leap)[..10], "2000-02-29");
    }

    #[test]
    fn lines_go_to_the_file_and_it_rolls_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs").join("viewer.log");
        set_file(&path);
        crate::diag!("decoder: {}", "GPU");
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.trim_end().ends_with("decoder: GPU"), "{text}");
        let big = "x".repeat(4096);
        for _ in 0..300 {
            line(format_args!("{big}"));
        }
        assert!(path.with_extension("log.1").exists());
        assert!(fs::metadata(&path).unwrap().len() <= MAX_BYTES);
        *SINK.lock().unwrap() = None;
    }
}
