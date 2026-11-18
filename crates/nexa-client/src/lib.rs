//! `libnexa_client`: the viewer core behind the C ABI of §10.1 (macOS, Windows, iOS).
//!
//! The C ABI ([`abi`]) is the product; the rest is public only so that integration
//! tests can run the same code with fake devices through [`deps::Deps`].

pub mod abi;
pub mod audio;
pub mod client;
pub mod cursor;
pub mod deps;
#[cfg(target_os = "macos")]
pub mod headless;
pub mod input;
pub mod keymap;
pub mod presenter;
pub mod proto;
#[cfg(target_os = "macos")]
mod real;
#[cfg(windows)]
#[path = "real_windows.rs"]
mod real;
#[cfg(target_os = "ios")]
#[path = "real_ios.rs"]
mod real;
#[cfg(target_os = "macos")]
mod real_cursor;
#[cfg(windows)]
mod real_windows_cursor;
pub mod snapshot;
pub mod terminal;
