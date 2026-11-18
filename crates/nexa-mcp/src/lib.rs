//! NexaDesk as an MCP server (docs/MCP.md): an AI agent connects to another computer
//! the way the app does, sees its screen and uses its keyboard and mouse.
//!
//! The library is the whole server, so tests can drive it with fake devices; the
//! binary only wires up the real ones and stdio.

pub mod image;
pub mod keys;
pub mod server;
pub mod viewer;
