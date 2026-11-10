//! NexaDesk wire formats. See docs/DETAILED_DESIGN.md §3.
//!
//! Everything here parses untrusted network input: parsers must return `Err`
//! on bad data and never panic.

pub mod control;
pub mod datagram;
pub mod framing;
pub mod signal;

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq, Clone)]
pub enum WireError {
    #[error("buffer too short: need {need}, have {have}")]
    TooShort { need: usize, have: usize },
    #[error("unexpected datagram type {0:#04x}")]
    BadType(u8),
    #[error("invalid field: {0}")]
    Invalid(&'static str),
    #[error("frame of {0} bytes exceeds the limit")]
    FrameTooLarge(u64),
}

pub type Result<T> = std::result::Result<T, WireError>;
