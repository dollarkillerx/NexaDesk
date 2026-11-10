//! Messages crossing the C ABI; `proto/nexa_client.proto` is the published schema.

use nexa_wire::control::Mode;
use prost::{Enumeration, Message, Oneof};

#[derive(Clone, PartialEq, Message)]
pub struct ClientConfig {
    #[prost(string, tag = "1")]
    pub device_name: String,
    #[prost(string, tag = "2")]
    pub data_dir: String,
}

#[derive(Clone, PartialEq, Oneof)]
pub enum ConnectAuth {
    #[prost(string, tag = "4")]
    KnownHost(String),
    #[prost(string, tag = "5")]
    Code(String),
    #[prost(string, tag = "6")]
    Password(String),
}

#[derive(Clone, PartialEq, Message)]
pub struct ConnectRequest {
    #[prost(string, tag = "1")]
    pub address: String,
    #[prost(uint64, tag = "2")]
    pub tag: u64,
    #[prost(enumeration = "Mode", tag = "3")]
    pub mode: i32,
    #[prost(uint32, tag = "7")]
    pub fps: u32,
    #[prost(oneof = "ConnectAuth", tags = "4, 5, 6")]
    pub auth: Option<ConnectAuth>,
}

#[derive(Clone, PartialEq, Message)]
pub struct PointerMove {
    #[prost(double, tag = "1")]
    pub x: f64,
    #[prost(double, tag = "2")]
    pub y: f64,
}

#[derive(Clone, PartialEq, Message)]
pub struct PointerButton {
    #[prost(uint32, tag = "1")]
    pub button: u32,
    #[prost(bool, tag = "2")]
    pub down: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct PointerWheel {
    #[prost(sint32, tag = "1")]
    pub dx: i32,
    #[prost(sint32, tag = "2")]
    pub dy: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct SendText {
    #[prost(string, tag = "1")]
    pub utf8: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct SetMode {
    #[prost(enumeration = "Mode", tag = "1")]
    pub mode: i32,
    #[prost(uint32, tag = "2")]
    pub fps: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct SetClipboardPolicy {
    #[prost(bool, tag = "1")]
    pub outbound: bool,
    #[prost(bool, tag = "2")]
    pub inbound: bool,
}

#[derive(Clone, PartialEq, Oneof)]
pub enum Command {
    #[prost(message, tag = "1")]
    Move(PointerMove),
    #[prost(message, tag = "2")]
    Button(PointerButton),
    #[prost(message, tag = "3")]
    Wheel(PointerWheel),
    #[prost(message, tag = "4")]
    Text(SendText),
    #[prost(message, tag = "5")]
    Mode(SetMode),
    #[prost(message, tag = "6")]
    Clipboard(SetClipboardPolicy),
}

#[derive(Clone, PartialEq, Message)]
pub struct SessionCommand {
    #[prost(oneof = "Command", tags = "1, 2, 3, 4, 5, 6")]
    pub cmd: Option<Command>,
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
pub struct KnownHost {
    #[prost(string, tag = "1")]
    pub key: String,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(uint64, tag = "3")]
    pub last_seen: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct Ready {
    #[prost(string, tag = "1")]
    pub this_device_key: String,
    #[prost(message, repeated, tag = "2")]
    pub hosts: Vec<KnownHost>,
    #[prost(string, tag = "3")]
    pub error: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct Connecting {}

#[derive(Clone, PartialEq, Message)]
pub struct Connected {
    #[prost(string, tag = "1")]
    pub host_key: String,
    #[prost(string, tag = "2")]
    pub host_name: String,
    #[prost(message, optional, tag = "3")]
    pub perms: Option<Permissions>,
    #[prost(uint32, tag = "4")]
    pub host_os: u32,
    #[prost(bool, tag = "5")]
    pub paired_now: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct StreamChanged {
    #[prost(uint32, tag = "1")]
    pub codec: u32,
    #[prost(enumeration = "Mode", tag = "2")]
    pub mode: i32,
    #[prost(uint32, tag = "3")]
    pub w: u32,
    #[prost(uint32, tag = "4")]
    pub h: u32,
    #[prost(uint32, tag = "5")]
    pub fps: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Enumeration)]
#[repr(i32)]
pub enum CaptureState {
    Inactive = 0,
    Active = 1,
    Degraded = 2,
    Unavailable = 3,
}

#[derive(Clone, PartialEq, Message)]
pub struct InputCapture {
    #[prost(enumeration = "CaptureState", tag = "1")]
    pub state: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct PasteGate {
    #[prost(uint32, tag = "1")]
    pub waited_ms: u32,
    #[prost(bool, tag = "2")]
    pub timed_out: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClipboardNotice {
    #[prost(string, tag = "1")]
    pub text: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct Stats {
    #[prost(uint64, tag = "1")]
    pub complete: u64,
    #[prost(uint64, tag = "2")]
    pub lost: u64,
    #[prost(uint64, tag = "3")]
    pub fec_recovered: u64,
    #[prost(uint64, tag = "4")]
    pub decoded: u64,
    #[prost(uint64, tag = "5")]
    pub decode_errors: u64,
    #[prost(uint64, tag = "6")]
    pub presented: u64,
    #[prost(uint64, tag = "7")]
    pub rtt_us: u64,
    #[prost(uint32, tag = "8")]
    pub gate_pastes: u32,
    #[prost(uint32, tag = "9")]
    pub gate_held: u32,
    #[prost(uint32, tag = "10")]
    pub gate_timeouts: u32,
    /// UDP bytes received on the connection so far, for a bitrate readout.
    #[prost(uint64, tag = "11")]
    pub rx_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Enumeration)]
#[repr(i32)]
pub enum DisconnectReason {
    Unspecified = 0,
    ClosedLocally = 1,
    HostEnded = 2,
    Replaced = 3,
    PipelineFailed = 4,
    Network = 5,
    PinMismatch = 6,
    NotPaired = 7,
    AuthFailed = 8,
    RateLimited = 9,
    ConsentDenied = 10,
    NoCommonCodec = 11,
    VersionMismatch = 12,
    TimedOut = 13,
    InvalidRequest = 14,
    Internal = 15,
}

#[derive(Clone, PartialEq, Message)]
pub struct Disconnected {
    #[prost(enumeration = "DisconnectReason", tag = "1")]
    pub reason: i32,
    #[prost(string, tag = "2")]
    pub message: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct HostKeyPressed {}

#[derive(Clone, PartialEq, Message)]
pub struct Warning {
    #[prost(string, tag = "1")]
    pub text: String,
}

#[derive(Clone, PartialEq, Oneof)]
pub enum EventBody {
    #[prost(message, tag = "1")]
    Ready(Ready),
    #[prost(message, tag = "2")]
    Connecting(Connecting),
    #[prost(message, tag = "3")]
    Connected(Connected),
    #[prost(message, tag = "4")]
    Stream(StreamChanged),
    #[prost(message, tag = "5")]
    InputCapture(InputCapture),
    #[prost(message, tag = "6")]
    PasteGate(PasteGate),
    #[prost(message, tag = "7")]
    Clipboard(ClipboardNotice),
    #[prost(message, tag = "8")]
    Stats(Stats),
    #[prost(message, tag = "9")]
    Disconnected(Disconnected),
    #[prost(message, tag = "10")]
    HostKey(HostKeyPressed),
    #[prost(message, tag = "11")]
    Warning(Warning),
}

#[derive(Clone, PartialEq, Message)]
pub struct Event {
    #[prost(uint64, tag = "20")]
    pub tag: u64,
    #[prost(oneof = "EventBody", tags = "1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11")]
    pub body: Option<EventBody>,
}
