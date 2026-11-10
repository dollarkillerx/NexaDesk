//! IPC messages; `proto/agent_ipc.proto` is the published schema.

use prost::{Enumeration, Message, Oneof};

#[derive(Clone, PartialEq, Eq, Message)]
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

impl From<&nexa_wire::control::Permissions> for Permissions {
    fn from(p: &nexa_wire::control::Permissions) -> Self {
        Self {
            view: p.view,
            input: p.input,
            clipboard: p.clipboard,
            file: p.file,
            audio: p.audio,
        }
    }
}

impl From<nexa_auth::Perms> for Permissions {
    fn from(p: nexa_auth::Perms) -> Self {
        Self {
            view: p.view,
            input: p.input,
            clipboard: p.clipboard,
            file: p.file,
            audio: p.audio,
        }
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct Hello {
    #[prost(uint32, tag = "1")]
    pub version: u32,
    #[prost(string, tag = "2")]
    pub client: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct Empty {}

#[derive(Clone, PartialEq, Message)]
pub struct RevokePairing {
    #[prost(string, tag = "1")]
    pub device_key: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct SetAccessPolicy {
    #[prost(string, tag = "1")]
    pub device_key: String,
    #[prost(bool, tag = "2")]
    pub unattended: bool,
    #[prost(message, optional, tag = "3")]
    pub perms: Option<Permissions>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Decide {
    #[prost(uint64, tag = "1")]
    pub req_id: u64,
    #[prost(bool, tag = "2")]
    pub allow: bool,
    #[prost(message, optional, tag = "3")]
    pub perms: Option<Permissions>,
    #[prost(bool, tag = "4")]
    pub save: bool,
    #[prost(bool, tag = "5")]
    pub skip_next_time: bool,
}

#[derive(Clone, PartialEq, Oneof)]
pub enum UiBody {
    #[prost(message, tag = "1")]
    Hello(Hello),
    #[prost(message, tag = "2")]
    GetStatus(Empty),
    #[prost(message, tag = "3")]
    SetAccessPolicy(SetAccessPolicy),
    #[prost(message, tag = "4")]
    ListPairings(Empty),
    #[prost(message, tag = "5")]
    RevokePairing(RevokePairing),
    #[prost(message, tag = "6")]
    Disconnect(Empty),
    #[prost(message, tag = "7")]
    RotateCode(Empty),
    #[prost(message, tag = "8")]
    Decide(Decide),
    #[prost(message, tag = "9")]
    Shutdown(Empty),
}

#[derive(Clone, PartialEq, Message)]
pub struct UiToAgent {
    #[prost(oneof = "UiBody", tags = "1, 2, 3, 4, 5, 6, 7, 8, 9")]
    pub body: Option<UiBody>,
}

impl From<UiBody> for UiToAgent {
    fn from(body: UiBody) -> Self {
        Self { body: Some(body) }
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct SessionInfo {
    #[prost(uint64, tag = "1")]
    pub id: u64,
    #[prost(string, tag = "2")]
    pub device_name: String,
    #[prost(string, tag = "3")]
    pub device_id: String,
    #[prost(string, tag = "4")]
    pub device_key: String,
    #[prost(string, tag = "5")]
    pub remote: String,
    #[prost(message, optional, tag = "6")]
    pub perms: Option<Permissions>,
}

impl From<&crate::SessionInfo> for SessionInfo {
    fn from(s: &crate::SessionInfo) -> Self {
        Self {
            id: s.id,
            device_name: s.device_name.clone(),
            device_id: s.device_id.clone(),
            device_key: s.device_key.clone(),
            remote: s.remote.to_string(),
            perms: Some((&s.perms).into()),
        }
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct Status {
    #[prost(uint32, tag = "1")]
    pub version: u32,
    #[prost(string, tag = "2")]
    pub listen: String,
    #[prost(string, tag = "3")]
    pub code: String,
    #[prost(string, tag = "4")]
    pub device_name: String,
    #[prost(string, tag = "5")]
    pub device_id: String,
    #[prost(string, tag = "6")]
    pub device_key: String,
    #[prost(message, optional, tag = "7")]
    pub session: Option<SessionInfo>,
    #[prost(bool, tag = "8")]
    pub input_available: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Enumeration)]
#[repr(i32)]
pub enum AuthKind {
    Unspecified = 0,
    Paired = 1,
    Code = 2,
    Password = 3,
}

#[derive(Clone, PartialEq, Message)]
pub struct IncomingRequest {
    #[prost(uint64, tag = "1")]
    pub req_id: u64,
    #[prost(string, tag = "2")]
    pub device_name: String,
    #[prost(string, tag = "3")]
    pub device_id: String,
    #[prost(string, tag = "4")]
    pub device_key: String,
    #[prost(string, tag = "5")]
    pub remote: String,
    #[prost(enumeration = "AuthKind", tag = "6")]
    pub auth: i32,
    #[prost(message, optional, tag = "7")]
    pub perms: Option<Permissions>,
    #[prost(bool, tag = "8")]
    pub offer_save: bool,
    #[prost(bool, tag = "9")]
    pub save_default: bool,
    #[prost(bool, tag = "10")]
    pub offer_skip_next_time: bool,
    #[prost(bool, tag = "11")]
    pub takeover: bool,
    #[prost(uint32, tag = "12")]
    pub timeout_ms: u32,
    #[prost(bool, tag = "13")]
    pub input_available: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct RequestClosed {
    #[prost(uint64, tag = "1")]
    pub req_id: u64,
    #[prost(string, tag = "2")]
    pub reason: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct SessionStarted {
    #[prost(message, optional, tag = "1")]
    pub session: Option<SessionInfo>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SessionEnded {
    #[prost(uint64, tag = "1")]
    pub id: u64,
    #[prost(string, tag = "2")]
    pub device_name: String,
    #[prost(string, tag = "3")]
    pub reason: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct PairedDevice {
    #[prost(string, tag = "1")]
    pub device_key: String,
    #[prost(string, tag = "2")]
    pub device_id: String,
    #[prost(string, tag = "3")]
    pub name: String,
    #[prost(message, optional, tag = "4")]
    pub perms: Option<Permissions>,
    #[prost(uint64, tag = "5")]
    pub added_at: u64,
    #[prost(uint64, tag = "6")]
    pub last_seen: u64,
    #[prost(bool, tag = "7")]
    pub unattended: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct Pairings {
    #[prost(message, repeated, tag = "1")]
    pub devices: Vec<PairedDevice>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Enumeration)]
#[repr(i32)]
pub enum NoticeKind {
    Warning = 0,
    ConnectionFailed = 1,
    Clipboard = 2,
}

#[derive(Clone, PartialEq, Message)]
pub struct Notice {
    #[prost(enumeration = "NoticeKind", tag = "1")]
    pub kind: i32,
    #[prost(string, tag = "2")]
    pub text: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct Refused {
    #[prost(string, tag = "1")]
    pub command: String,
    #[prost(string, tag = "2")]
    pub reason: String,
}

#[derive(Clone, PartialEq, Oneof)]
pub enum AgentBody {
    #[prost(message, tag = "1")]
    Status(Status),
    #[prost(message, tag = "2")]
    Incoming(IncomingRequest),
    #[prost(message, tag = "3")]
    SessionStarted(SessionStarted),
    #[prost(message, tag = "4")]
    SessionEnded(SessionEnded),
    #[prost(message, tag = "5")]
    Pairings(Pairings),
    #[prost(message, tag = "6")]
    RequestClosed(RequestClosed),
    #[prost(message, tag = "7")]
    Notice(Notice),
    #[prost(message, tag = "8")]
    Refused(Refused),
}

#[derive(Clone, PartialEq, Message)]
pub struct AgentToUi {
    #[prost(oneof = "AgentBody", tags = "1, 2, 3, 4, 5, 6, 7, 8")]
    pub body: Option<AgentBody>,
}

impl From<AgentBody> for AgentToUi {
    fn from(body: AgentBody) -> Self {
        Self { body: Some(body) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Golden bytes shared with the C# codec tests (ui/NexaDesk.Tests/ProtoTests.cs).
    #[test]
    fn golden_encodings() {
        let decide = UiToAgent::from(UiBody::Decide(Decide {
            req_id: 300,
            allow: true,
            perms: Some(Permissions {
                view: true,
                input: false,
                clipboard: true,
                file: false,
                audio: false,
            }),
            save: true,
            skip_next_time: false,
        }));
        assert_eq!(
            hex(&decide.encode_to_vec()),
            "420d08ac0210011a0408011801 2001".replace(' ', "")
        );
        let hello = UiToAgent::from(UiBody::Hello(Hello {
            version: 1,
            client: "ui".into(),
        }));
        assert_eq!(
            hex(&hello.encode_to_vec()),
            "0a0608011202 7569".replace(' ', "")
        );

        let incoming = AgentToUi::from(AgentBody::Incoming(IncomingRequest {
            req_id: 7,
            device_name: "王".into(),
            device_id: "d".into(),
            device_key: "ab".into(),
            remote: "1.2.3.4:5".into(),
            auth: AuthKind::Code as i32,
            perms: Some(Permissions {
                view: true,
                ..Permissions::default()
            }),
            offer_save: true,
            save_default: false,
            offer_skip_next_time: false,
            takeover: true,
            timeout_ms: 60_000,
            input_available: true,
        }));
        assert_eq!(
            hex(&incoming.encode_to_vec()),
            "12290807 1203e78e8b 1a0164 22026162 2a09312e322e332e343a35 3002 3a020801 4001 5801 60e0d403 6801"
                .replace(' ', "")
        );
    }
}
