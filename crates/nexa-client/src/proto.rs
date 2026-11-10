//! Messages crossing the C ABI; `proto/nexa_client.proto` is the published schema.

use nexa_wire::control::Mode;
use prost::{Enumeration, Message, Oneof};

#[derive(Clone, PartialEq, Message)]
pub struct ClientConfig {
    #[prost(string, tag = "1")]
    pub device_name: String,
    #[prost(string, tag = "2")]
    pub data_dir: String,
    /// Where files from hosts are saved; empty means `~/Downloads/NexaDesk`.
    #[prost(string, tag = "3")]
    pub downloads_dir: String,
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
    /// The nexa-signal server, used when `address` is a device ID.
    #[prost(string, tag = "8")]
    pub signal_server: String,
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

/// Mute (false) or unmute the host's system audio. Needs the audio permission.
#[derive(Clone, PartialEq, Message)]
pub struct SetAudio {
    #[prost(bool, tag = "1")]
    pub enabled: bool,
}

/// Stream another of the host's displays (`Connected.displays[i].id`).
#[derive(Clone, PartialEq, Message)]
pub struct SelectDisplay {
    #[prost(uint32, tag = "1")]
    pub display: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct SetClipboardPolicy {
    #[prost(bool, tag = "1")]
    pub outbound: bool,
    #[prost(bool, tag = "2")]
    pub inbound: bool,
}

/// Send these local files to the host. Needs `Permissions.file`.
#[derive(Clone, PartialEq, Message)]
pub struct SendFiles {
    #[prost(string, repeated, tag = "1")]
    pub paths: Vec<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CancelTransfer {
    #[prost(uint64, tag = "1")]
    pub id: u64,
}

/// Accept or decline an incoming file waiting in `TRANSFER_NEEDS_CONFIRM`.
#[derive(Clone, PartialEq, Message)]
pub struct AnswerTransfer {
    #[prost(uint64, tag = "1")]
    pub id: u64,
    #[prost(bool, tag = "2")]
    pub accept: bool,
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
    #[prost(message, tag = "20")]
    Audio(SetAudio),
    #[prost(message, tag = "21")]
    Display(SelectDisplay),
    #[prost(message, tag = "22")]
    SendFiles(SendFiles),
    #[prost(message, tag = "23")]
    CancelTransfer(CancelTransfer),
    #[prost(message, tag = "24")]
    AnswerTransfer(AnswerTransfer),
}

#[derive(Clone, PartialEq, Message)]
pub struct SessionCommand {
    #[prost(oneof = "Command", tags = "1, 2, 3, 4, 5, 6, 20, 21, 22, 23, 24")]
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
pub struct Connecting {
    #[prost(enumeration = "ConnectStage", tag = "1")]
    pub stage: i32,
}

/// §4.4's states up to the QUIC handshake. ip:port connections only report Quic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Enumeration)]
#[repr(i32)]
pub enum ConnectStage {
    Quic = 0,
    Resolving = 1,
    Gathering = 2,
    Checking = 3,
}

/// How the session's packets travel (§11 diagnostics).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Enumeration)]
#[repr(i32)]
pub enum PathKind {
    Unknown = 0,
    /// To the host's own address: LAN, public IP or ip:port.
    Direct = 1,
    /// Through both NATs by hole punching.
    Punched = 2,
    Relay = 3,
    Tailscale = 4,
    /// The relay over WebSocket (TCP), when UDP is blocked.
    RelayWss = 5,
    /// Tailscale, relayed through DERP.
    TailscaleDerp = 6,
}

/// One of the host's displays.
#[derive(Clone, PartialEq, Message)]
pub struct DisplayInfo {
    /// What `SelectDisplay` takes.
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(uint32, tag = "3")]
    pub w: u32,
    #[prost(uint32, tag = "4")]
    pub h: u32,
}

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
    #[prost(message, repeated, tag = "20")]
    pub displays: Vec<DisplayInfo>,
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
    /// Which display is streamed (`DisplayInfo.id`).
    #[prost(uint32, tag = "20")]
    pub display: u32,
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
    #[prost(enumeration = "PathKind", tag = "12")]
    pub path: i32,
    /// The remote end of the path, e.g. "203.0.113.5:4000" or "relay 198.51.100.1:3480".
    #[prost(string, tag = "13")]
    pub path_detail: String,
    /// Smoothed path RTT from connectivity probes, when known.
    #[prost(uint64, tag = "14")]
    pub path_rtt_us: u64,
    /// Audio packets played, and frames made up for lost or late ones.
    #[prost(uint64, tag = "30")]
    pub audio_played: u64,
    #[prost(uint64, tag = "31")]
    pub audio_concealed: u64,
    /// Mean time an audio packet waits in the jitter buffer.
    #[prost(uint32, tag = "32")]
    pub audio_buffer_us: u32,
    /// §6.1 segments measured here, over the last frames presented.
    #[prost(message, repeated, tag = "40")]
    pub latency: Vec<LatencySegment>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Enumeration)]
#[repr(i32)]
pub enum Segment {
    /// Capture at the host to the last shard here: host pipeline and network,
    /// through the clock offset (§3.3).
    HostAndNetwork = 0,
    /// First shard to last (reassembly).
    Receive = 1,
    /// Waiting for the decoder.
    DecodeQueue = 2,
    Decode = 3,
    /// Decoded, waiting for the render thread.
    PresentQueue = 4,
    Present = 5,
    /// Capture to presented.
    Total = 6,
}

#[derive(Clone, PartialEq, Message)]
pub struct LatencySegment {
    #[prost(enumeration = "Segment", tag = "1")]
    pub segment: i32,
    #[prost(uint32, tag = "2")]
    pub p50_us: u32,
    #[prost(uint32, tag = "3")]
    pub p99_us: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Enumeration)]
#[repr(i32)]
pub enum TransferState {
    Waiting = 0,
    NeedsConfirm = 1,
    Running = 2,
    Done = 3,
    Failed = 4,
    Cancelled = 5,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Enumeration)]
#[repr(i32)]
pub enum FailReason {
    None = 0,
    Declined = 1,
    TooLarge = 2,
    NotPermitted = 3,
    Invalid = 4,
    HashMismatch = 5,
    NoSpace = 6,
    Io = 7,
    ConnectionLost = 8,
    Timeout = 9,
    NotAFile = 10,
    Busy = 11,
}

/// A file transfer of this session moved on. Same numbering as the agent IPC's.
#[derive(Clone, PartialEq, Message)]
pub struct Transfer {
    #[prost(uint64, tag = "1")]
    pub id: u64,
    /// From this Mac to the host.
    #[prost(bool, tag = "2")]
    pub outgoing: bool,
    #[prost(string, tag = "3")]
    pub name: String,
    #[prost(uint64, tag = "4")]
    pub size: u64,
    #[prost(uint64, tag = "5")]
    pub done: u64,
    #[prost(enumeration = "TransferState", tag = "6")]
    pub state: i32,
    #[prost(enumeration = "FailReason", tag = "7")]
    pub reason: i32,
    #[prost(string, tag = "8")]
    pub path: String,
    #[prost(bool, tag = "9")]
    pub by_peer: bool,
}

impl Transfer {
    pub fn of(e: &nexa_session::transfer::TransferEvent) -> Self {
        use nexa_session::transfer::{Direction, FailReason as R, TransferState as S};
        let mut t = Self {
            id: e.id,
            outgoing: e.direction == Direction::Outgoing,
            name: e.name.clone(),
            size: e.size,
            done: e.done,
            ..Default::default()
        };
        t.state = match &e.state {
            S::Waiting => TransferState::Waiting,
            S::NeedsConfirm => TransferState::NeedsConfirm,
            S::Running => TransferState::Running,
            S::Done { path } => {
                t.path = path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                TransferState::Done
            }
            S::Cancelled { by_peer } => {
                t.by_peer = *by_peer;
                TransferState::Cancelled
            }
            S::Failed(r) => {
                t.reason = match r {
                    R::Declined => FailReason::Declined,
                    R::TooLarge => FailReason::TooLarge,
                    R::NotPermitted => FailReason::NotPermitted,
                    R::Invalid => FailReason::Invalid,
                    R::HashMismatch => FailReason::HashMismatch,
                    R::NoSpace => FailReason::NoSpace,
                    R::Io => FailReason::Io,
                    R::ConnectionLost => FailReason::ConnectionLost,
                    R::Timeout => FailReason::Timeout,
                    R::NotAFile => FailReason::NotAFile,
                    R::Busy => FailReason::Busy,
                } as i32;
                TransferState::Failed
            }
        } as i32;
        t
    }
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
    /// The ID is not registered with the server right now.
    Offline = 16,
    /// Every path failed connectivity checks, relay included.
    NoPath = 17,
    /// The signaling server could not be reached or refused us.
    SignalServer = 18,
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

/// §4.4 Reconnecting: attempt `attempt` is starting. `Connected` for the same
/// tag ends it; `Disconnected` means it gave up.
#[derive(Clone, PartialEq, Message)]
pub struct Reconnecting {
    #[prost(uint32, tag = "1")]
    pub attempt: u32,
    /// Why the transport dropped.
    #[prost(string, tag = "2")]
    pub reason: String,
    /// What is left of the 30 s window.
    #[prost(uint32, tag = "3")]
    pub remaining_ms: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct TailnetDevice {
    #[prost(string, tag = "1")]
    pub name: String,
    /// `ip:port`, to connect to as such.
    #[prost(string, tag = "2")]
    pub address: String,
    /// False: through DERP.
    #[prost(bool, tag = "3")]
    pub direct: bool,
    #[prost(string, tag = "4")]
    pub os: String,
    #[prost(string, tag = "5")]
    pub dns_name: String,
}

/// Tag 0: Tailscale peers that answer on the NexaDesk port (§4.7).
#[derive(Clone, PartialEq, Message)]
pub struct TailnetDevices {
    #[prost(message, repeated, tag = "1")]
    pub devices: Vec<TailnetDevice>,
}

/// A host heard on the LAN over mDNS (§4.11). Unverified: only an address to try.
#[derive(Clone, PartialEq, Message)]
pub struct LanDevice {
    #[prost(string, tag = "1")]
    pub name: String,
    /// `ip:port`, the best of what it announced.
    #[prost(string, tag = "2")]
    pub address: String,
    #[prost(string, tag = "3")]
    pub os: String,
    /// Its `DeviceId` as announced. Not proof of anything.
    #[prost(string, tag = "4")]
    pub device_id: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct LanDevices {
    #[prost(message, repeated, tag = "1")]
    pub devices: Vec<LanDevice>,
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
    #[prost(message, tag = "40")]
    Reconnecting(Reconnecting),
    #[prost(message, tag = "41")]
    Tailnet(TailnetDevices),
    #[prost(message, tag = "42")]
    Transfer(Transfer),
    #[prost(message, tag = "43")]
    Lan(LanDevices),
}

#[derive(Clone, PartialEq, Message)]
pub struct Event {
    #[prost(uint64, tag = "20")]
    pub tag: u64,
    #[prost(
        oneof = "EventBody",
        tags = "1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 40, 41, 42, 43"
    )]
    pub body: Option<EventBody>,
}

// ---------------------------------------------------------------------------------
// Keyboard settings (§11.2): nexa_keymap_rules / _save / _preview. OS numbers as in
// `Connected.host_os`: 1 windows, 2 macos, 3 linux.
// ---------------------------------------------------------------------------------

#[derive(Clone, PartialEq, Message)]
pub struct KeymapRule {
    #[prost(bool, tag = "1")]
    pub builtin: bool,
    #[prost(string, tag = "2")]
    pub ctx: String,
    #[prost(string, tag = "3")]
    pub when: String,
    #[prost(string, repeated, tag = "4")]
    pub then: Vec<String>,
    #[prost(string, tag = "5")]
    pub hold: String,
    #[prost(string, tag = "6")]
    pub tag: String,
    #[prost(bool, tag = "7")]
    pub enabled: bool,
}

/// For `nexa_keymap_rules`; also the first fields of the other two requests.
#[derive(Clone, PartialEq, Message)]
pub struct KeymapRequest {
    /// Where keymap.toml is; empty means ~/Library/Application Support/NexaDesk.
    #[prost(string, tag = "1")]
    pub data_dir: String,
    #[prost(uint32, tag = "2")]
    pub local_os: u32,
    #[prost(uint32, tag = "3")]
    pub remote_os: u32,
    /// Save: the whole list as shown. Preview: used instead of the file when `edited`.
    #[prost(message, repeated, tag = "4")]
    pub rules: Vec<KeymapRule>,
    #[prost(bool, tag = "5")]
    pub edited: bool,
    /// Preview only: the chord pressed, e.g. "Cmd+Shift+Z".
    #[prost(string, tag = "6")]
    pub chord: String,
    /// Preview only: 0 default, 1 terminal, 2 ide.
    #[prost(uint32, tag = "7")]
    pub ctx: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct KeymapView {
    #[prost(message, repeated, tag = "1")]
    pub rules: Vec<KeymapRule>,
    /// keymap.toml could not be used; the rules shown are the built-in ones.
    #[prost(string, tag = "2")]
    pub error: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct KeyOut {
    #[prost(string, tag = "1")]
    pub key: String,
    #[prost(bool, tag = "2")]
    pub down: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct KeyPreview {
    #[prost(message, repeated, tag = "1")]
    pub events: Vec<KeyOut>,
    #[prost(string, repeated, tag = "2")]
    pub sent: Vec<String>,
    #[prost(bool, tag = "3")]
    pub paste: bool,
    #[prost(string, tag = "4")]
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// The same golden bytes as ui/NexaDesk.Tests/LanDiscoveryTests.cs.
    #[test]
    fn lan_event_matches_the_csharp_golden_bytes() {
        let e = Event {
            tag: 0,
            body: Some(EventBody::Lan(LanDevices {
                devices: vec![LanDevice {
                    name: "a".into(),
                    address: "b".into(),
                    os: "c".into(),
                    device_id: "d".into(),
                }],
            })),
        };
        assert_eq!(hex(&e.encode_to_vec()), "da020e0a0c0a01611201621a0163220164");
    }

    /// The same golden bytes as ui/NexaDesk.Tests/SessionFeatureTests.cs.
    #[test]
    fn feature_commands_and_events_match_the_csharp_golden_bytes() {
        let cmd = |c| SessionCommand { cmd: Some(c) }.encode_to_vec();
        assert_eq!(
            hex(&cmd(Command::Audio(SetAudio { enabled: true }))),
            "a201020801"
        );
        assert_eq!(
            hex(&cmd(Command::Audio(SetAudio { enabled: false }))),
            "a20100"
        );
        assert_eq!(
            hex(&cmd(Command::Display(SelectDisplay { display: 1 }))),
            "aa01020801"
        );
        let event = |b| {
            Event {
                tag: 0,
                body: Some(b),
            }
            .encode_to_vec()
        };
        let stream = StreamChanged {
            codec: 2,
            mode: Mode::Game as i32,
            w: 0,
            h: 0,
            fps: 0,
            display: 1,
        };
        assert_eq!(hex(&event(EventBody::Stream(stream))), "220708021002a00101");
        let stats = Stats {
            audio_played: 5,
            audio_concealed: 2,
            audio_buffer_us: 1000,
            ..Default::default()
        };
        assert_eq!(
            hex(&event(EventBody::Stats(stats))),
            "420af00105f801028002e807"
        );
        let display = DisplayInfo {
            id: 1,
            name: "d".into(),
            w: 2,
            h: 3,
        };
        let connected = Connected {
            host_name: "n".into(),
            displays: vec![display],
            ..Default::default()
        };
        assert_eq!(
            hex(&event(EventBody::Connected(connected))),
            "1a0f12016ea20109080112016418022003"
        );
        assert_eq!(
            hex(&cmd(Command::SendFiles(SendFiles {
                paths: vec!["/a".into()]
            }))),
            "b201040a022f61"
        );
        assert_eq!(
            hex(&cmd(Command::AnswerTransfer(AnswerTransfer {
                id: 3,
                accept: true
            }))),
            "c2010408031001"
        );
        let transfer = Transfer {
            id: 2,
            outgoing: false,
            name: "a".into(),
            size: 300,
            done: 300,
            state: TransferState::Done as i32,
            reason: 0,
            path: "/d/a".into(),
            by_peer: false,
        };
        assert_eq!(
            hex(&event(EventBody::Transfer(transfer))),
            "d20213 0802 1a0161 20ac02 28ac02 3003 42042f642f61".replace(' ', "")
        );
        let stats = Stats {
            latency: vec![LatencySegment {
                segment: Segment::Decode as i32,
                p50_us: 1500,
                p99_us: 4000,
            }],
            ..Default::default()
        };
        assert_eq!(
            hex(&event(EventBody::Stats(stats))),
            "420bc20208080310dc0b18a01f"
        );
    }
}
