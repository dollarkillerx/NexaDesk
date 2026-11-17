//! Windows injection (docs/WINDOWS.md): keys by scan code, text by Unicode, the mouse
//! on the virtual desktop, all through SendInput; and the viewer's keyboard capture.

mod capture;
mod inject;
mod pointer;
mod scancode;

pub use capture::{CaptureEvent, KeyCapture};
pub use inject::Injector;
pub use pointer::{Motion, PointerCapture};
pub use scancode::scancode_of;
