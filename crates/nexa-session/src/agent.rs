//! An AI agent's look at the host (docs/MCP.md §13): on a stream of its own
//! ([`StreamType::Agent`]), one request per stream.
//!
//! ```protobuf
//! message ObserveRequest {
//!   uint64 after_mark = 1;      // wait until the input stream got this far (0: don't)
//!   uint32 quiet_ms = 2;        // how long the screen must stay still (0: don't wait)
//!   uint32 max_ms = 3;          // the longest wait for that
//!   bool ui = 4;                // the frontmost window's elements
//!   bool interactive_only = 5;
//!   uint32 max_nodes = 6;
//!   bool screenshot = 7;        // a lossless PNG, the pointer drawn in
//! }
//! message ObserveReply {
//!   uint32 display = 1; uint32 w = 2; uint32 h = 3;  // the agent's pixels of that display
//!   bool settled = 4; bool changed = 5; uint32 waited_ms = 6;
//!   string app = 7; string window = 8; string focused = 9;
//!   bool has_pointer = 10; sint32 pointer_x = 11; sint32 pointer_y = 12;
//!   string ui = 13; string ui_error = 14;
//!   uint32 png_len = 15; string screenshot_error = 16;
//! }
//! message AgentMsg { oneof body {
//!   ObserveRequest observe = 1;  // agent → host, the only message it sends
//!   ObserveReply reply = 2;      // host → agent, first
//!   bytes png = 3;               // host → agent: the PNG in pieces, png_len in all
//!   string refused = 4;          // host → agent, instead of a reply
//! } }
//! ```
//!
//! Coordinates are the agent's pixels of the display: its native pixels scaled down,
//! keeping the aspect, until the longer side is at most [`AGENT_SIDE`]. The host gives
//! them in its `StreamConfig` (`codec: Unknown`, no video), the screenshot is that
//! size, element bounds are in them, and pointer input in an agent session uses them.
//!
//! [`StreamType::Agent`]: nexa_wire::framing::StreamType::Agent

use prost::{Message, Oneof};

/// The longer side of an agent's pixels: what models take in without scaling down.
pub const AGENT_SIDE: u32 = 1568;
/// The most of the PNG one message carries.
pub const CHUNK: usize = 64 * 1024;
/// The largest PNG a host sends.
pub const MAX_PNG: usize = 16 << 20;
/// `refused` for a session that may not look (no view permission) or a host that cannot.
pub const REFUSED_UNSUPPORTED: &str = "this computer cannot show its screen to an agent yet";

/// The agent's pixels for a display `w`×`h` pixels.
pub fn agent_size(w: u32, h: u32) -> (u32, u32) {
    let (w, h) = (w.max(1), h.max(1));
    if w.max(h) <= AGENT_SIDE {
        return (w, h);
    }
    let k = f64::from(AGENT_SIDE) / f64::from(w.max(h));
    (
        ((f64::from(w) * k).round() as u32).max(1),
        ((f64::from(h) * k).round() as u32).max(1),
    )
}

#[derive(Clone, Copy, PartialEq, Eq, Message)]
pub struct ObserveRequest {
    #[prost(uint64, tag = "1")]
    pub after_mark: u64,
    #[prost(uint32, tag = "2")]
    pub quiet_ms: u32,
    #[prost(uint32, tag = "3")]
    pub max_ms: u32,
    #[prost(bool, tag = "4")]
    pub ui: bool,
    #[prost(bool, tag = "5")]
    pub interactive_only: bool,
    #[prost(uint32, tag = "6")]
    pub max_nodes: u32,
    #[prost(bool, tag = "7")]
    pub screenshot: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct ObserveReply {
    #[prost(uint32, tag = "1")]
    pub display: u32,
    #[prost(uint32, tag = "2")]
    pub w: u32,
    #[prost(uint32, tag = "3")]
    pub h: u32,
    #[prost(bool, tag = "4")]
    pub settled: bool,
    #[prost(bool, tag = "5")]
    pub changed: bool,
    #[prost(uint32, tag = "6")]
    pub waited_ms: u32,
    #[prost(string, tag = "7")]
    pub app: String,
    #[prost(string, tag = "8")]
    pub window: String,
    #[prost(string, tag = "9")]
    pub focused: String,
    #[prost(bool, tag = "10")]
    pub has_pointer: bool,
    #[prost(sint32, tag = "11")]
    pub pointer_x: i32,
    #[prost(sint32, tag = "12")]
    pub pointer_y: i32,
    #[prost(string, tag = "13")]
    pub ui: String,
    #[prost(string, tag = "14")]
    pub ui_error: String,
    #[prost(uint32, tag = "15")]
    pub png_len: u32,
    #[prost(string, tag = "16")]
    pub screenshot_error: String,
    /// Why the screen could not be watched for changes (no thumbnails): `settled`
    /// and `changed` then say nothing about it.
    #[prost(string, tag = "17")]
    pub watch_error: String,
    /// The centre of each ref in `ui`, in the agent's pixels: `refs[i]` is `@e{i+1}`.
    #[prost(message, repeated, tag = "18")]
    pub refs: Vec<RefCenter>,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct RefCenter {
    #[prost(sint32, tag = "1")]
    pub x: i32,
    #[prost(sint32, tag = "2")]
    pub y: i32,
}

#[derive(Clone, PartialEq, Oneof)]
pub enum AgentBody {
    #[prost(message, tag = "1")]
    Observe(ObserveRequest),
    #[prost(message, tag = "2")]
    Reply(ObserveReply),
    #[prost(bytes = "vec", tag = "3")]
    Png(Vec<u8>),
    #[prost(string, tag = "4")]
    Refused(String),
}

#[derive(Clone, PartialEq, Message)]
pub struct AgentMsg {
    #[prost(oneof = "AgentBody", tags = "1, 2, 3, 4")]
    pub body: Option<AgentBody>,
}

impl From<AgentBody> for AgentMsg {
    fn from(body: AgentBody) -> Self {
        Self { body: Some(body) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_pixels_keep_the_aspect_and_small_displays() {
        assert_eq!(agent_size(2560, 1600), (1568, 980));
        assert_eq!(agent_size(3840, 2160), (1568, 882));
        assert_eq!(agent_size(1080, 1920), (882, 1568));
        assert_eq!(agent_size(1280, 800), (1280, 800));
        assert_eq!(agent_size(0, 0), (1, 1));
    }

    #[test]
    fn messages_round_trip() {
        let m = AgentMsg::from(AgentBody::Observe(ObserveRequest {
            after_mark: 7,
            quiet_ms: 400,
            max_ms: 3000,
            ui: true,
            interactive_only: true,
            max_nodes: 300,
            screenshot: false,
        }));
        assert_eq!(AgentMsg::decode(m.encode_to_vec().as_slice()).unwrap(), m);
    }
}
