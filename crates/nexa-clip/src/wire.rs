//! Messages of the Clipboard stream (design §9.2) and their encoding.
//!
//! Each message is one protobuf-encoded `Envelope`; the stream's `varint length`
//! framing (§3.1) belongs to `nexa-wire`. The schema, kept in sync by hand with
//! the prost derives below (so no `protoc` is needed):
//!
//! ```protobuf
//! message Gen      { uint64 lamport = 1; bytes device = 2; }         // device: 16 bytes
//! message Format   { uint32 kind = 1; uint64 size = 2; bytes hash = 3; } // hash: 16 bytes
//! message Inline   { uint32 kind = 1; bytes data = 2; }
//! message Announce { Gen generation = 1; bytes origin = 2;
//!                    repeated Format formats = 3; repeated Inline inline = 4; }
//! message Applied  { Gen generation = 1; uint32 status = 2; }
//! message Fetch    { Gen generation = 1; uint32 kind = 2; uint64 offset = 3; uint64 len = 4; }
//! message Data     { Gen generation = 1; uint32 kind = 2; uint64 offset = 3;
//!                    bytes data = 4; bool eof = 5; uint32 status = 6; }
//! message Envelope { oneof body { Announce announce = 1; Applied applied = 2;
//!                                 Fetch fetch = 3; Data data = 4; } }
//! ```
//!
//! `kind`: 1 text, 2 html, 3 rtf, 4 png. An unknown kind inside an announcement
//! is skipped (a newer peer may offer formats we do not know). `status`: see
//! [`ClipStatus`]; 0 is omitted on the wire, so success costs nothing.

use bytes::Bytes;
use prost::Message;

use crate::backend::{ClipError, Result};
use crate::types::{ClipKind, ContentHash, DeviceId, FormatInfo, Gen};

/// Outcome carried by [`ClipApplied`] and [`ClipData`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipStatus {
    /// Applied, or data follows.
    Ok,
    /// The OS clipboard stayed locked (§8.4).
    Busy,
    /// Every offered format exceeds the size limit (§8.4).
    TooLarge,
    /// The generation is no longer the newest on the replying side.
    Stale,
    /// A direction switch or policy refused the operation (§9.5).
    Denied,
    /// The request failed validation. Also the decoding of unknown values.
    Invalid,
}

impl ClipStatus {
    fn to_wire(self) -> u32 {
        match self {
            Self::Ok => 0,
            Self::Busy => 1,
            Self::TooLarge => 2,
            Self::Stale => 3,
            Self::Denied => 4,
            Self::Invalid => 5,
        }
    }

    fn from_wire(value: u32) -> Self {
        match value {
            0 => Self::Ok,
            1 => Self::Busy,
            2 => Self::TooLarge,
            3 => Self::Stale,
            4 => Self::Denied,
            _ => Self::Invalid,
        }
    }
}

/// Announces a new clipboard entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipAnnounce {
    /// The entry's generation.
    pub generation: Gen,
    /// The device that produced the entry; equals `generation.1`.
    pub origin: DeviceId,
    /// Offered formats.
    pub formats: Vec<FormatInfo>,
    /// Content sent along with the announcement.
    pub inline: Vec<(ClipKind, Bytes)>,
}

/// Reports what happened to an announced generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipApplied {
    /// The announced generation.
    pub generation: Gen,
    /// The outcome.
    pub status: ClipStatus,
}

/// Requests a byte range of a promised format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipFetch {
    /// The generation the promise belongs to.
    pub generation: Gen,
    /// The format.
    pub kind: ClipKind,
    /// First byte requested.
    pub offset: u64,
    /// Number of bytes requested.
    pub len: u64,
}

/// Carries a chunk of a format, or the reason there is none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipData {
    /// The generation the chunk belongs to.
    pub generation: Gen,
    /// The format.
    pub kind: ClipKind,
    /// Offset of `bytes` within the format.
    pub offset: u64,
    /// The chunk.
    pub bytes: Bytes,
    /// Whether this chunk reaches the end of the format.
    pub eof: bool,
    /// Anything but [`ClipStatus::Ok`] means the fetch failed. An addition to
    /// §9.2, which has no way to deliver the `Stale` that §8.2 requires.
    pub status: ClipStatus,
}

/// Any message on the Clipboard stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipMessage {
    /// See [`ClipAnnounce`].
    Announce(ClipAnnounce),
    /// See [`ClipApplied`].
    Applied(ClipApplied),
    /// See [`ClipFetch`].
    Fetch(ClipFetch),
    /// See [`ClipData`].
    Data(ClipData),
}

mod pb {
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct Gen {
        #[prost(uint64, tag = "1")]
        pub lamport: u64,
        #[prost(bytes = "vec", tag = "2")]
        pub device: Vec<u8>,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct Format {
        #[prost(uint32, tag = "1")]
        pub kind: u32,
        #[prost(uint64, tag = "2")]
        pub size: u64,
        #[prost(bytes = "vec", tag = "3")]
        pub hash: Vec<u8>,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct Inline {
        #[prost(uint32, tag = "1")]
        pub kind: u32,
        #[prost(bytes = "bytes", tag = "2")]
        pub data: ::bytes::Bytes,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct Announce {
        #[prost(message, optional, tag = "1")]
        pub generation: Option<Gen>,
        #[prost(bytes = "vec", tag = "2")]
        pub origin: Vec<u8>,
        #[prost(message, repeated, tag = "3")]
        pub formats: Vec<Format>,
        #[prost(message, repeated, tag = "4")]
        pub inline: Vec<Inline>,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct Applied {
        #[prost(message, optional, tag = "1")]
        pub generation: Option<Gen>,
        #[prost(uint32, tag = "2")]
        pub status: u32,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct Fetch {
        #[prost(message, optional, tag = "1")]
        pub generation: Option<Gen>,
        #[prost(uint32, tag = "2")]
        pub kind: u32,
        #[prost(uint64, tag = "3")]
        pub offset: u64,
        #[prost(uint64, tag = "4")]
        pub len: u64,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct Data {
        #[prost(message, optional, tag = "1")]
        pub generation: Option<Gen>,
        #[prost(uint32, tag = "2")]
        pub kind: u32,
        #[prost(uint64, tag = "3")]
        pub offset: u64,
        #[prost(bytes = "bytes", tag = "4")]
        pub data: ::bytes::Bytes,
        #[prost(bool, tag = "5")]
        pub eof: bool,
        #[prost(uint32, tag = "6")]
        pub status: u32,
    }

    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Body {
        #[prost(message, tag = "1")]
        Announce(Announce),
        #[prost(message, tag = "2")]
        Applied(Applied),
        #[prost(message, tag = "3")]
        Fetch(Fetch),
        #[prost(message, tag = "4")]
        Data(Data),
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct Envelope {
        #[prost(oneof = "Body", tags = "1, 2, 3, 4")]
        pub body: Option<Body>,
    }
}

fn gen_to_pb(generation: Gen) -> Option<pb::Gen> {
    Some(pb::Gen {
        lamport: generation.0,
        device: generation.1.0.to_vec(),
    })
}

fn bad(what: &str) -> ClipError {
    ClipError::Decode(what.to_owned())
}

fn device_from(bytes: &[u8]) -> Result<DeviceId> {
    bytes
        .try_into()
        .map(DeviceId)
        .map_err(|_| bad("device id must be 16 bytes"))
}

fn gen_from_pb(generation: Option<pb::Gen>) -> Result<Gen> {
    let generation = generation.ok_or_else(|| bad("missing generation"))?;
    Ok((generation.lamport, device_from(&generation.device)?))
}

fn kind_from(value: u32) -> Result<ClipKind> {
    ClipKind::from_wire(value).ok_or_else(|| bad("unknown clipboard kind"))
}

fn format_from_pb(format: pb::Format) -> Result<Option<FormatInfo>> {
    let Some(kind) = ClipKind::from_wire(format.kind) else {
        return Ok(None);
    };
    let hash = <[u8; 16]>::try_from(format.hash.as_slice())
        .map_err(|_| bad("content hash must be 16 bytes"))?;
    Ok(Some(FormatInfo {
        kind,
        size: format.size,
        hash: ContentHash(hash),
    }))
}

impl ClipMessage {
    /// Encodes the message, without the stream's length prefix.
    pub fn encode(&self) -> Vec<u8> {
        let body = match self {
            Self::Announce(m) => pb::Body::Announce(pb::Announce {
                generation: gen_to_pb(m.generation),
                origin: m.origin.0.to_vec(),
                formats: m
                    .formats
                    .iter()
                    .map(|f| pb::Format {
                        kind: f.kind.to_wire(),
                        size: f.size,
                        hash: f.hash.0.to_vec(),
                    })
                    .collect(),
                inline: m
                    .inline
                    .iter()
                    .map(|(kind, data)| pb::Inline {
                        kind: kind.to_wire(),
                        data: data.clone(),
                    })
                    .collect(),
            }),
            Self::Applied(m) => pb::Body::Applied(pb::Applied {
                generation: gen_to_pb(m.generation),
                status: m.status.to_wire(),
            }),
            Self::Fetch(m) => pb::Body::Fetch(pb::Fetch {
                generation: gen_to_pb(m.generation),
                kind: m.kind.to_wire(),
                offset: m.offset,
                len: m.len,
            }),
            Self::Data(m) => pb::Body::Data(pb::Data {
                generation: gen_to_pb(m.generation),
                kind: m.kind.to_wire(),
                offset: m.offset,
                data: m.bytes.clone(),
                eof: m.eof,
                status: m.status.to_wire(),
            }),
        };
        pb::Envelope { body: Some(body) }.encode_to_vec()
    }

    /// Decodes a message produced by [`ClipMessage::encode`].
    pub fn decode(input: &[u8]) -> Result<Self> {
        let envelope = pb::Envelope::decode(input).map_err(|e| ClipError::Decode(e.to_string()))?;
        match envelope.body.ok_or_else(|| bad("empty envelope"))? {
            pb::Body::Announce(m) => {
                let formats = m
                    .formats
                    .into_iter()
                    .map(format_from_pb)
                    .collect::<Result<Vec<_>>>()?;
                Ok(Self::Announce(ClipAnnounce {
                    generation: gen_from_pb(m.generation)?,
                    origin: device_from(&m.origin)?,
                    formats: formats.into_iter().flatten().collect(),
                    inline: m
                        .inline
                        .into_iter()
                        .filter_map(|i| ClipKind::from_wire(i.kind).map(|kind| (kind, i.data)))
                        .collect(),
                }))
            }
            pb::Body::Applied(m) => Ok(Self::Applied(ClipApplied {
                generation: gen_from_pb(m.generation)?,
                status: ClipStatus::from_wire(m.status),
            })),
            pb::Body::Fetch(m) => Ok(Self::Fetch(ClipFetch {
                generation: gen_from_pb(m.generation)?,
                kind: kind_from(m.kind)?,
                offset: m.offset,
                len: m.len,
            })),
            pb::Body::Data(m) => Ok(Self::Data(ClipData {
                generation: gen_from_pb(m.generation)?,
                kind: kind_from(m.kind)?,
                offset: m.offset,
                bytes: m.data,
                eof: m.eof,
                status: ClipStatus::from_wire(m.status),
            })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generation() -> Gen {
        (42, DeviceId([7; 16]))
    }

    fn round_trip(message: ClipMessage) {
        assert_eq!(ClipMessage::decode(&message.encode()).unwrap(), message);
    }

    #[test]
    fn every_message_round_trips() {
        let data = Bytes::from_static(b"hello");
        round_trip(ClipMessage::Announce(ClipAnnounce {
            generation: generation(),
            origin: generation().1,
            formats: vec![FormatInfo::of(ClipKind::Text, &data)],
            inline: vec![(ClipKind::Text, data.clone())],
        }));
        round_trip(ClipMessage::Applied(ClipApplied {
            generation: generation(),
            status: ClipStatus::Busy,
        }));
        round_trip(ClipMessage::Fetch(ClipFetch {
            generation: generation(),
            kind: ClipKind::Png,
            offset: 1 << 40,
            len: 9,
        }));
        round_trip(ClipMessage::Data(ClipData {
            generation: generation(),
            kind: ClipKind::Html,
            offset: 3,
            bytes: data,
            eof: true,
            status: ClipStatus::Ok,
        }));
    }

    #[test]
    fn success_status_costs_no_bytes() {
        let ok = ClipMessage::Applied(ClipApplied {
            generation: generation(),
            status: ClipStatus::Ok,
        });
        let busy = ClipMessage::Applied(ClipApplied {
            generation: generation(),
            status: ClipStatus::Busy,
        });
        assert_eq!(ok.encode().len() + 2, busy.encode().len());
    }

    #[test]
    fn unknown_format_kinds_are_skipped_in_announcements() {
        let envelope = pb::Envelope {
            body: Some(pb::Body::Announce(pb::Announce {
                generation: gen_to_pb(generation()),
                origin: generation().1.0.to_vec(),
                formats: vec![pb::Format {
                    kind: 99,
                    size: 1,
                    hash: vec![0; 16],
                }],
                inline: vec![pb::Inline {
                    kind: 99,
                    data: Bytes::from_static(b"x"),
                }],
            })),
        };
        let ClipMessage::Announce(announce) =
            ClipMessage::decode(&envelope.encode_to_vec()).unwrap()
        else {
            panic!("expected an announcement");
        };
        assert!(announce.formats.is_empty());
        assert!(announce.inline.is_empty());
    }

    #[test]
    fn malformed_input_is_an_error_not_a_panic() {
        assert!(ClipMessage::decode(&[]).is_err());
        assert!(ClipMessage::decode(&[0xff, 0xff, 0xff]).is_err());
        let short_device = pb::Envelope {
            body: Some(pb::Body::Applied(pb::Applied {
                generation: Some(pb::Gen {
                    lamport: 1,
                    device: vec![1, 2, 3],
                }),
                status: 0,
            })),
        };
        assert!(ClipMessage::decode(&short_device.encode_to_vec()).is_err());
    }
}
