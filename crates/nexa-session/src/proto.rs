//! Envelopes for the Control and Input streams, and what §3.4 leaves out.
//!
//! §3.1 frames each message as `varint length + protobuf`, but protobuf does not say
//! which message a body is, and both streams carry several kinds. Every body on the
//! Control and Input streams is therefore one of the two `oneof` envelopes below. The
//! Clipboard stream already has its own (`nexa_clip::ClipMessage`).
//!
//! ```protobuf
//! message SessionReject { RejectReason reason = 1; string message = 2; }
//! message StreamRequest { Mode mode = 1; uint32 fps = 2;     // viewer → host, 0 = host's choice
//!                         uint32 display = 3; }              // index into SessionAccept.displays
//! message Goodbye       { CloseReason reason = 1; string message = 2; }
//! message CursorMoved   { sint32 x = 1; sint32 y = 2; uint32 display = 3; }  // host → viewer
//! message AudioControl  { bool enabled = 1; }                // viewer → host
//! message ControlMsg { oneof body {
//!   ClientHello hello = 1;  AuthStep auth = 2;  SessionAccept accept = 3;
//!   SessionReject reject = 4;  StreamRequest request = 5;  StreamConfig config = 6;
//!   FocusContext focus = 7;  Goodbye bye = 8;  PathSwitch path_switch = 9;
//!   CursorShape cursor_shape = 20;  CursorSet cursor_set = 21;
//!   CursorMoved cursor_moved = 22;  AudioControl audio = 23;
//!   CandidateUpdate candidates = 24;
//! } }
//! message CandidateUpdate { repeated WireCandidate candidates = 1; }   // §4.5, both ways
//! ```
//!
//! §6.9 cursor messages, as this implementation fills them in:
//! - `CursorShape.id` is a hash of the image, hot spot and size, so the same cursor
//!   always has the same id and is sent once per session. `w`, `h`, `hot_x` and
//!   `hot_y` are in **stream pixels**: the size the cursor covers on the video, which
//!   is what the viewer needs to draw it at the right size. `png` may have more
//!   pixels than `w`×`h` (a Retina cursor); the viewer uses them for sharpness.
//! - `CursorSet` names the current shape, or `hidden`. It always follows the
//!   `CursorShape` for a new id.
//! - `CursorMoved` is in stream pixels of `display`. The host sends it only when its
//!   cursor moved somewhere other than where the viewer last put it, i.e. when a
//!   program (or the person at the host) moved it.
//! - None of the three are sent in game mode, where the cursor is drawn into the
//!   video, nor when the session has no input permission (the host's cursor is
//!   then drawn into the video in every mode, because the viewer's own pointer does
//!   not drive it).
//!
//! `AudioControl` turns the host's audio capture on and off (the viewer's mute).
//! The host ignores it unless the session was granted the audio permission.
//!
//! ```text
//! message InputMsg { oneof body {
//!   KeyEvent key = 1;  ButtonEvent button = 2;  TextInput text = 3;  KeyStateSync sync = 4;
//! } }
//! ```

use nexa_wire::control::{
    AuthStep, ButtonEvent, ClientHello, CursorSet, CursorShape, FocusContext, KeyEvent,
    KeyStateSync, PathSwitch, SessionAccept, StreamConfig, TextInput,
};
use prost::{Enumeration, Message, Oneof};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Enumeration)]
#[repr(i32)]
pub enum RejectReason {
    Unspecified = 0,
    VersionMismatch = 1,
    AuthFailed = 2,
    RateLimited = 3,
    NotPaired = 4,
    ConsentDenied = 5,
    NoCommonCodec = 6,
    Timeout = 7,
    MethodUnavailable = 8,
    Internal = 9,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Enumeration)]
#[repr(i32)]
pub enum CloseReason {
    Unspecified = 0,
    /// The person at the host ended the session.
    HostEnded = 1,
    /// The viewer closed the session.
    ViewerEnded = 2,
    /// A newer session took over (§5.3).
    Replaced = 3,
    /// Capture or encode could not continue.
    PipelineFailed = 4,
    /// The agent is exiting.
    Shutdown = 5,
}

#[derive(Clone, PartialEq, Message)]
pub struct SessionReject {
    #[prost(enumeration = "RejectReason", tag = "1")]
    pub reason: i32,
    #[prost(string, tag = "2")]
    pub message: String,
}

/// Viewer's first message after [`SessionAccept`], and how it later switches mode (§6.7).
#[derive(Clone, PartialEq, Message)]
pub struct StreamRequest {
    #[prost(enumeration = "nexa_wire::control::Mode", tag = "1")]
    pub mode: i32,
    #[prost(uint32, tag = "2")]
    pub fps: u32,
    /// Which of `SessionAccept.displays` to stream (§6.7). 0 is the first.
    #[prost(uint32, tag = "3")]
    pub display: u32,
}

/// Host → viewer: a program on the host moved the cursor (§6.9). Stream pixels.
#[derive(Clone, PartialEq, Message)]
pub struct CursorMoved {
    #[prost(sint32, tag = "1")]
    pub x: i32,
    #[prost(sint32, tag = "2")]
    pub y: i32,
    #[prost(uint32, tag = "3")]
    pub display: u32,
}

/// Viewer → host: start or stop sending system audio (the viewer's mute).
#[derive(Clone, PartialEq, Message)]
pub struct AudioControl {
    #[prost(bool, tag = "1")]
    pub enabled: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct Goodbye {
    #[prost(enumeration = "CloseReason", tag = "1")]
    pub reason: i32,
    #[prost(string, tag = "2")]
    pub message: String,
}

#[derive(Clone, PartialEq, Oneof)]
pub enum ControlBody {
    #[prost(message, tag = "1")]
    Hello(ClientHello),
    #[prost(message, tag = "2")]
    Auth(AuthStep),
    #[prost(message, tag = "3")]
    Accept(SessionAccept),
    #[prost(message, tag = "4")]
    Reject(SessionReject),
    #[prost(message, tag = "5")]
    Request(StreamRequest),
    #[prost(message, tag = "6")]
    Config(StreamConfig),
    #[prost(message, tag = "7")]
    Focus(FocusContext),
    #[prost(message, tag = "8")]
    Bye(Goodbye),
    /// Viewer → host (§4.5): follow the path the host labelled `path_id`. Only
    /// meaningful on sessions that came through the rendezvous.
    #[prost(message, tag = "9")]
    PathSwitch(PathSwitch),
    /// Host → viewer (§6.9). Tags from 20 leave room below for the transport.
    #[prost(message, tag = "20")]
    CursorShape(CursorShape),
    #[prost(message, tag = "21")]
    CursorSet(CursorSet),
    #[prost(message, tag = "22")]
    CursorMoved(CursorMoved),
    /// Viewer → host.
    #[prost(message, tag = "23")]
    Audio(AudioControl),
    /// Either way (§4.5): our candidates after a network change. Only on
    /// sessions that came through the rendezvous; checked like any other.
    #[prost(message, tag = "24")]
    Candidates(CandidateUpdate),
}

/// §4.5: after a network change, candidates go over the authenticated control
/// stream rather than signaling.
#[derive(Clone, PartialEq, Message)]
pub struct CandidateUpdate {
    #[prost(message, repeated, tag = "1")]
    pub candidates: Vec<nexa_wire::signal::WireCandidate>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ControlMsg {
    #[prost(
        oneof = "ControlBody",
        tags = "1, 2, 3, 4, 5, 6, 7, 8, 9, 20, 21, 22, 23, 24"
    )]
    pub body: Option<ControlBody>,
}

impl From<ControlBody> for ControlMsg {
    fn from(body: ControlBody) -> Self {
        Self { body: Some(body) }
    }
}

#[derive(Clone, PartialEq, Oneof)]
pub enum InputBody {
    #[prost(message, tag = "1")]
    Key(KeyEvent),
    #[prost(message, tag = "2")]
    Button(ButtonEvent),
    #[prost(message, tag = "3")]
    Text(TextInput),
    #[prost(message, tag = "4")]
    Sync(KeyStateSync),
}

#[derive(Clone, PartialEq, Message)]
pub struct InputMsg {
    #[prost(oneof = "InputBody", tags = "1, 2, 3, 4")]
    pub body: Option<InputBody>,
}

impl From<InputBody> for InputMsg {
    fn from(body: InputBody) -> Self {
        Self { body: Some(body) }
    }
}

/// QUIC application close codes.
pub mod close {
    pub const NORMAL: u32 = 0;
    pub const REJECTED: u32 = 1;
    pub const HOST_ENDED: u32 = 2;
    pub const VIEWER_ENDED: u32 = 3;
    pub const REPLACED: u32 = 4;
    pub const PIPELINE_FAILED: u32 = 5;
    pub const SHUTDOWN: u32 = 6;

    pub fn for_reason(reason: super::CloseReason) -> u32 {
        use super::CloseReason as R;
        match reason {
            R::Unspecified => NORMAL,
            R::HostEnded => HOST_ENDED,
            R::ViewerEnded => VIEWER_ENDED,
            R::Replaced => REPLACED,
            R::PipelineFailed => PIPELINE_FAILED,
            R::Shutdown => SHUTDOWN,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_round_trip_and_garbage() {
        let m: ControlMsg = ControlBody::Request(StreamRequest {
            mode: nexa_wire::control::Mode::Game as i32,
            fps: 120,
            display: 1,
        })
        .into();
        assert_eq!(ControlMsg::decode(m.encode_to_vec().as_slice()).unwrap(), m);
        assert!(ControlMsg::decode(&[0xFFu8; 16][..]).is_err());
    }

    #[test]
    fn cursor_and_audio_messages_round_trip() {
        let bodies = [
            ControlBody::CursorShape(CursorShape {
                id: u64::MAX - 3,
                w: 16,
                h: 24,
                hot_x: -1,
                hot_y: 5,
                png: vec![0x89, b'P', b'N', b'G'],
            }),
            ControlBody::CursorSet(CursorSet {
                id: 42,
                hidden: true,
            }),
            ControlBody::CursorMoved(CursorMoved {
                x: -10,
                y: 2000,
                display: 1,
            }),
            ControlBody::Audio(AudioControl { enabled: true }),
        ];
        for body in bodies {
            let m = ControlMsg::from(body);
            assert_eq!(ControlMsg::decode(m.encode_to_vec().as_slice()).unwrap(), m);
        }
    }

    /// A host or viewer from before these messages sees an empty envelope, which
    /// `ControlChannel::recv` skips, rather than an error.
    #[test]
    fn unknown_bodies_decode_as_empty() {
        #[derive(Clone, PartialEq, Message)]
        struct Future {
            #[prost(message, optional, tag = "99")]
            x: Option<AudioControl>,
        }
        let bytes = Future {
            x: Some(AudioControl { enabled: true }),
        }
        .encode_to_vec();
        assert_eq!(ControlMsg::decode(bytes.as_slice()).unwrap().body, None);
    }

    #[test]
    fn input_round_trip() {
        let m: InputMsg = InputBody::Key(KeyEvent {
            hid: 7 << 16 | 4,
            down: true,
            repeat: false,
        })
        .into();
        assert_eq!(InputMsg::decode(m.encode_to_vec().as_slice()).unwrap(), m);
    }
}
