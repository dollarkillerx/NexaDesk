//! OS-specific clipboard backends.

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(windows)]
pub mod windows;
#[cfg(windows)]
mod windows_image;
