//! `libnexa_client`: the macOS viewer core behind the C ABI of §10.1.
//!
//! The C ABI ([`abi`]) is the product; the rest is public only so that integration
//! tests can run the same code with fake devices through [`deps::Deps`].

pub mod abi;
pub mod audio;
pub mod client;
pub mod cursor;
pub mod deps;
pub mod input;
pub mod keymap;
pub mod presenter;
pub mod proto;
#[cfg(target_os = "macos")]
mod real;
#[cfg(target_os = "macos")]
mod real_cursor;
