//! Keyboard and pointer chord rewriting between controller and controlled OS (§7.2).
//!
//! Everything here is OS-independent: capture and injection belong to the assembly layer.

pub mod hid;
pub mod keymap;
pub mod keytable;
#[cfg(any(target_os = "macos", windows))]
pub mod platform;
pub mod rewriter;
pub mod user;

pub use hid::{Chord, ChordKey, Hid, ModSet};
pub use keymap::{Compiled, Keymap, KeymapError};
pub use nexa_wire::control::{FocusClass, Os};
pub use rewriter::{Output, PhysButton, PhysKey, PhysWheel, RewriteMode, Rewriter};
