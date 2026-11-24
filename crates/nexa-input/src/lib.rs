//! Keyboard and pointer chord rewriting between controller and controlled OS (§7.2).
//!
//! Everything here is OS-independent: capture and injection belong to the assembly layer.

pub mod hid;
pub mod keymap;
pub mod keytable;
#[cfg(any(target_os = "macos", windows))]
pub mod platform;
pub mod rewriter;
pub mod scanfilter;
pub mod user;

pub use hid::{Chord, ChordKey, Hid, ModSet};
pub use keymap::{Compiled, Keymap, KeymapError};
pub use nexa_wire::control::{FocusClass, Os};
pub use rewriter::{Output, PhysButton, PhysKey, PhysWheel, RewriteMode, Rewriter};

/// What a key that switches input source asks of the host: 🌐 its next source, 英数 an
/// English layout, かな its Japanese input method.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwitchAction {
    Next,
    Ascii,
    Japanese,
}
