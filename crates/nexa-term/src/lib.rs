//! A shell on a pseudo-terminal: what a remote command-line session talks to.
//!
//! On Linux and macOS the shell is the signed-in user's login shell, started as a login
//! shell in their home folder with `TERM=xterm-256color`; on Windows it is PowerShell on
//! a ConPTY, in the user's profile folder. It gets a session and process group of its own, so
//! ending the [`Pty`] (dropping it) hangs up on everything started from it, and anything
//! still there a moment later is killed. Nothing outlives the session.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::Pty;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::Pty;

/// What to start.
#[derive(Clone, Debug)]
pub struct Spawn {
    /// The program; `None` is the user's login shell (`$SHELL`, else `/bin/sh`), or
    /// `powershell.exe` on Windows.
    pub shell: Option<std::path::PathBuf>,
    /// Where it starts; `None` is `$HOME` (`%USERPROFILE%` on Windows).
    pub cwd: Option<std::path::PathBuf>,
    pub cols: u16,
    pub rows: u16,
}

impl Default for Spawn {
    fn default() -> Self {
        Self {
            shell: None,
            cwd: None,
            cols: 80,
            rows: 24,
        }
    }
}

/// Columns and rows within what a terminal can be.
pub fn clamp_size(cols: u16, rows: u16) -> (u16, u16) {
    (cols.clamp(2, 1000), rows.clamp(1, 500))
}
