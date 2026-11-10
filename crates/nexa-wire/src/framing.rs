//! Reliable-stream framing (§3.1): one stream-type byte, then `varint length + body`.

use bytes::{Buf, BufMut, Bytes, BytesMut};

use crate::{Result, WireError};

/// Stream types (§3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StreamType {
    Control = 1,
    Input = 2,
    Clipboard = 3,
    File = 4,
}

impl StreamType {
    pub fn from_byte(b: u8) -> Result<Self> {
        match b {
            1 => Ok(Self::Control),
            2 => Ok(Self::Input),
            3 => Ok(Self::Clipboard),
            4 => Ok(Self::File),
            _ => Err(WireError::Invalid("stream type")),
        }
    }
}

pub const DEFAULT_MAX_FRAME: usize = 1 << 20;

pub fn put_varint(mut v: u64, out: &mut impl BufMut) {
    while v >= 0x80 {
        out.put_u8((v as u8) | 0x80);
        v >>= 7;
    }
    out.put_u8(v as u8);
}

/// Appends one length-prefixed frame.
pub fn encode_frame(body: &[u8], out: &mut BytesMut) {
    put_varint(body.len() as u64, out);
    out.extend_from_slice(body);
}

/// Incremental decoder: feed whatever the stream delivered, then drain frames.
#[derive(Debug)]
pub struct FrameDecoder {
    buf: BytesMut,
    max_frame: usize,
}

impl FrameDecoder {
    pub fn new(max_frame: usize) -> Self {
        Self {
            buf: BytesMut::new(),
            max_frame,
        }
    }

    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// `Ok(None)` means more bytes are needed. An `Err` is fatal for the stream.
    pub fn next_frame(&mut self) -> Result<Option<Bytes>> {
        let mut len = 0u64;
        let mut consumed = 0;
        loop {
            let Some(&b) = self.buf.get(consumed) else {
                return Ok(None);
            };
            if consumed == 9 {
                return Err(WireError::Invalid("varint too long"));
            }
            len |= u64::from(b & 0x7F) << (7 * consumed);
            consumed += 1;
            if b & 0x80 == 0 {
                break;
            }
        }
        if len > self.max_frame as u64 {
            return Err(WireError::FrameTooLarge(len));
        }
        let len = len as usize;
        if self.buf.len() < consumed + len {
            return Ok(None);
        }
        self.buf.advance(consumed);
        Ok(Some(self.buf.split_to(len).freeze()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_byte_by_byte() {
        let bodies: Vec<Vec<u8>> =
            vec![vec![], vec![1], vec![7; 127], vec![9; 128], vec![3; 70_000]];
        let mut wire = BytesMut::new();
        for b in &bodies {
            encode_frame(b, &mut wire);
        }
        let mut d = FrameDecoder::new(DEFAULT_MAX_FRAME);
        let mut out = Vec::new();
        for byte in wire.iter() {
            d.push(&[*byte]);
            while let Some(f) = d.next_frame().unwrap() {
                out.push(f.to_vec());
            }
        }
        assert_eq!(out, bodies);
    }

    #[test]
    fn rejects_oversize_before_buffering_it() {
        let mut d = FrameDecoder::new(16);
        let mut w = BytesMut::new();
        put_varint(17, &mut w);
        d.push(&w);
        assert_eq!(d.next_frame(), Err(WireError::FrameTooLarge(17)));
    }

    #[test]
    fn rejects_endless_varint() {
        let mut d = FrameDecoder::new(16);
        d.push(&[0xFF; 12]);
        assert!(d.next_frame().is_err());
    }
}
