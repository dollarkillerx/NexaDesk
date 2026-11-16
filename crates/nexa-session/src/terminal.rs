//! The host's command line on a stream of its own ([`StreamType::Terminal`]): one shell
//! per stream, for as long as the stream lasts.
//!
//! ```protobuf
//! message TermSize { uint32 cols = 1; uint32 rows = 2; }
//! message TermMsg { oneof body {
//!   TermSize open = 1;     // viewer → host, first: start a shell this size
//!   bytes data = 2;        // both ways: keystrokes in, output out
//!   TermSize resize = 3;   // viewer → host
//!   string exit = 4;       // host → viewer, last: why the shell is gone
//! } }
//! ```
//!
//! The viewer ending its side ends the shell, and everything it started; so does the
//! session ending. A host that did not grant the terminal answers `exit` at once.
//!
//! [`StreamType::Terminal`]: nexa_wire::framing::StreamType::Terminal

use prost::{Message, Oneof};

/// The most output, or input, one message carries.
pub const CHUNK: usize = 16 * 1024;

/// `exit` when the terminal was not granted.
pub const EXIT_DENIED: &str = "denied";
/// `exit` when the shell ended by itself.
pub const EXIT_EXITED: &str = "exited";

#[derive(Clone, Copy, PartialEq, Eq, Message)]
pub struct TermSize {
    #[prost(uint32, tag = "1")]
    pub cols: u32,
    #[prost(uint32, tag = "2")]
    pub rows: u32,
}

#[derive(Clone, PartialEq, Oneof)]
pub enum TermBody {
    #[prost(message, tag = "1")]
    Open(TermSize),
    #[prost(bytes = "vec", tag = "2")]
    Data(Vec<u8>),
    #[prost(message, tag = "3")]
    Resize(TermSize),
    #[prost(string, tag = "4")]
    Exit(String),
}

#[derive(Clone, PartialEq, Message)]
pub struct TermMsg {
    #[prost(oneof = "TermBody", tags = "1, 2, 3, 4")]
    pub body: Option<TermBody>,
}

impl From<TermBody> for TermMsg {
    fn from(body: TermBody) -> Self {
        Self { body: Some(body) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        for body in [
            TermBody::Open(TermSize {
                cols: 120,
                rows: 40,
            }),
            TermBody::Data(b"ls -la\r".to_vec()),
            TermBody::Resize(TermSize { cols: 80, rows: 24 }),
            TermBody::Exit(EXIT_EXITED.into()),
        ] {
            let m = TermMsg::from(body);
            assert_eq!(TermMsg::decode(m.encode_to_vec().as_slice()).unwrap(), m);
        }
    }

    #[test]
    fn a_full_chunk_fits_a_frame() {
        let m = TermMsg::from(TermBody::Data(vec![0xff; CHUNK]));
        assert!(m.encoded_len() < crate::framed::MAX_TERMINAL_FRAME);
    }
}
