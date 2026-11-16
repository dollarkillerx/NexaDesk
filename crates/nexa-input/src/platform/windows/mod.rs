//! Windows injection (docs/WINDOWS.md): keys by scan code, text by Unicode, the mouse
//! on the virtual desktop, all through SendInput.

mod inject;
mod scancode;

pub use inject::Injector;
pub use scancode::scancode_of;
