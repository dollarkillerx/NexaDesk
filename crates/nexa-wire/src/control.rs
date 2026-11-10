//! Control-plane protobuf messages (§3.4). Derived by hand so builds need no protoc.

use prost::{Enumeration, Message, Oneof};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Enumeration)]
#[repr(i32)]
pub enum Os {
    Unknown = 0,
    Windows = 1,
    Macos = 2,
    Linux = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Enumeration)]
#[repr(i32)]
pub enum Codec {
    Unknown = 0,
    H264 = 1,
    Hevc = 2,
    Av1 = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Enumeration)]
#[repr(i32)]
pub enum Chroma {
    Yuv420 = 0,
    Yuv444 = 1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Enumeration)]
#[repr(i32)]
pub enum Mode {
    Office = 0,
    Balanced = 1,
    Game = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Enumeration)]
#[repr(i32)]
pub enum FocusClass {
    Default = 0,
    Terminal = 1,
    Ide = 2,
}

#[derive(Clone, PartialEq, Message)]
pub struct CodecCap {
    #[prost(enumeration = "Codec", tag = "1")]
    pub codec: i32,
    #[prost(enumeration = "Chroma", tag = "2")]
    pub chroma: i32,
    #[prost(uint32, tag = "3")]
    pub max_w: u32,
    #[prost(uint32, tag = "4")]
    pub max_h: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct Caps {
    #[prost(message, repeated, tag = "1")]
    pub encode: Vec<CodecCap>,
    #[prost(message, repeated, tag = "2")]
    pub decode: Vec<CodecCap>,
    #[prost(bool, tag = "3")]
    pub rfi: bool,
    #[prost(bool, tag = "4")]
    pub ltr: bool,
    #[prost(uint32, tag = "5")]
    pub max_fps: u32,
}

#[derive(Clone, PartialEq, Oneof)]
pub enum Auth {
    #[prost(bool, tag = "5")]
    Known(bool),
    #[prost(bool, tag = "6")]
    Code(bool),
    #[prost(bool, tag = "7")]
    Password(bool),
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientHello {
    #[prost(uint32, tag = "1")]
    pub proto: u32,
    #[prost(string, tag = "2")]
    pub device_name: String,
    #[prost(enumeration = "Os", tag = "3")]
    pub os: i32,
    #[prost(message, optional, tag = "4")]
    pub caps: Option<Caps>,
    #[prost(oneof = "Auth", tags = "5, 6, 7")]
    pub auth: Option<Auth>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AuthStep {
    #[prost(bytes = "vec", tag = "1")]
    pub spake_msg: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub confirm: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    pub salt: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Permissions {
    #[prost(bool, tag = "1")]
    pub view: bool,
    #[prost(bool, tag = "2")]
    pub input: bool,
    #[prost(bool, tag = "3")]
    pub clipboard: bool,
    #[prost(bool, tag = "4")]
    pub file: bool,
    #[prost(bool, tag = "5")]
    pub audio: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct Display {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(sint32, tag = "2")]
    pub x: i32,
    #[prost(sint32, tag = "3")]
    pub y: i32,
    #[prost(uint32, tag = "4")]
    pub w: u32,
    #[prost(uint32, tag = "5")]
    pub h: u32,
    #[prost(string, tag = "6")]
    pub name: String,
    #[prost(uint32, tag = "7")]
    pub refresh_hz: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct SessionAccept {
    #[prost(message, optional, tag = "1")]
    pub perms: Option<Permissions>,
    #[prost(message, repeated, tag = "2")]
    pub displays: Vec<Display>,
    #[prost(message, optional, tag = "3")]
    pub caps: Option<Caps>,
    #[prost(enumeration = "Os", tag = "4")]
    pub os: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct StreamConfig {
    #[prost(uint32, tag = "1")]
    pub display: u32,
    #[prost(enumeration = "Codec", tag = "2")]
    pub codec: i32,
    #[prost(enumeration = "Mode", tag = "3")]
    pub mode: i32,
    #[prost(uint32, tag = "4")]
    pub w: u32,
    #[prost(uint32, tag = "5")]
    pub h: u32,
    #[prost(uint32, tag = "6")]
    pub fps: u32,
    #[prost(enumeration = "Chroma", tag = "7")]
    pub chroma: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct FocusContext {
    #[prost(enumeration = "FocusClass", tag = "1")]
    pub cls: i32,
    #[prost(string, tag = "2")]
    pub app_id: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct CursorShape {
    #[prost(uint64, tag = "1")]
    pub id: u64,
    #[prost(uint32, tag = "2")]
    pub w: u32,
    #[prost(uint32, tag = "3")]
    pub h: u32,
    #[prost(sint32, tag = "4")]
    pub hot_x: i32,
    #[prost(sint32, tag = "5")]
    pub hot_y: i32,
    #[prost(bytes = "vec", tag = "6")]
    pub png: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CursorSet {
    #[prost(uint64, tag = "1")]
    pub id: u64,
    #[prost(bool, tag = "2")]
    pub hidden: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct PathSwitch {
    #[prost(uint32, tag = "1")]
    pub path_id: u32,
}

// Input stream.

#[derive(Clone, PartialEq, Message)]
pub struct KeyEvent {
    #[prost(uint32, tag = "1")]
    pub hid: u32,
    #[prost(bool, tag = "2")]
    pub down: bool,
    #[prost(bool, tag = "3")]
    pub repeat: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct ButtonEvent {
    #[prost(uint32, tag = "1")]
    pub button: u32,
    #[prost(bool, tag = "2")]
    pub down: bool,
    #[prost(sint32, tag = "3")]
    pub x: i32,
    #[prost(sint32, tag = "4")]
    pub y: i32,
    #[prost(uint32, tag = "5")]
    pub display: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct TextInput {
    #[prost(string, tag = "1")]
    pub utf8: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct KeyStateSync {
    #[prost(uint32, repeated, tag = "1")]
    pub pressed: Vec<u32>,
    #[prost(uint64, tag = "2")]
    pub seq: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_round_trip() {
        let m = ClientHello {
            proto: 1,
            device_name: "mac".into(),
            os: Os::Macos as i32,
            caps: Some(Caps {
                encode: vec![CodecCap {
                    codec: Codec::Hevc as i32,
                    chroma: Chroma::Yuv420 as i32,
                    max_w: 3840,
                    max_h: 2160,
                }],
                decode: vec![],
                rfi: false,
                ltr: true,
                max_fps: 120,
            }),
            auth: Some(Auth::Password(true)),
        };
        assert_eq!(
            ClientHello::decode(m.encode_to_vec().as_slice()).unwrap(),
            m
        );
    }

    #[test]
    fn input_round_trip() {
        let m = ButtonEvent {
            button: 1,
            down: true,
            x: -20,
            y: 30,
            display: 1,
        };
        assert_eq!(
            ButtonEvent::decode(m.encode_to_vec().as_slice()).unwrap(),
            m
        );
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(ClientHello::decode(&[0xFFu8; 16][..]).is_err());
    }
}
