//! Acting as the person logged in to this session (docs/WINDOWS_SERVICE.md §5).
//!
//! On Windows the host may run as SYSTEM, started by the NexaDesk service so that it can
//! see and use UAC prompts and the lock screen. Whatever a remote person can use to act
//! through the host must still happen as the session's user: the command line, files
//! received and sent, the keys DPAPI keeps for that user, the user's folders. Otherwise
//! whoever connects would be SYSTEM, and a program of the user's could have the host write
//! where only SYSTEM may (a junction in place of the downloads folder, say).
//!
//! When the host is not SYSTEM (the usual case, and every other platform) all of this is
//! a no-op: it already is the user.

#[cfg(windows)]
pub mod launch;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

/// Whether this process runs as SYSTEM. Never on other platforms.
#[cfg(not(windows))]
pub fn is_system() -> bool {
    false
}

/// Runs `f` as the session's user. Elsewhere than a SYSTEM host on Windows, simply runs it.
#[cfg(not(windows))]
pub fn as_user<T>(f: impl FnOnce() -> std::io::Result<T>) -> std::io::Result<T> {
    f()
}
