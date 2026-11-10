//! Crash-safe file replacement (§11).

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::Path,
};

use rand_core::{OsRng, RngCore};

/// Replaces `path` so that a reader sees either the old or the new contents.
///
/// On Unix the file is created 0600 before any data is written to it.
pub(crate) fn atomic_write(path: &Path, data: &[u8]) -> io::Result<()> {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let tmp = dir.join(format!(
        ".{}.{:016x}.tmp",
        name.to_string_lossy(),
        OsRng.next_u64()
    ));

    let write = || -> io::Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut f = options.open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    };
    if let Err(e) = write() {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    Ok(())
}
