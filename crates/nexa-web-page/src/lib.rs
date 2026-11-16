//! The browser page and the bit of HTTP that serves it, shared by the two places it is
//! served from: a host (`nexa-web`, for browsers that can reach it) and the signaling
//! server (`nexa-signal`, for browsers anywhere, through TURN).
//!
//! The page asks `GET /mode` which one it came from: `{"mode":"direct"}` from a host,
//! `{"mode":"server"}` from the signaling server, where it also asks for a device ID.

pub mod assets;
pub mod http;

/// What `GET /mode` answers on a host.
pub const MODE_DIRECT: &[u8] = br#"{"mode":"direct"}"#;
/// What `GET /mode` answers on the signaling server.
pub const MODE_SERVER: &[u8] = br#"{"mode":"server"}"#;
