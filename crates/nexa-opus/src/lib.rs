//! Opus through libopus (statically linked, built from the source `audiopus_sys`
//! bundles): the codec for Windows, which has no system Opus. macOS uses AudioToolbox
//! instead (`nexa_media::macos::opus`).
//!
//! Same contract as there: 48 kHz stereo interleaved float, one 10 ms (480-sample)
//! frame per packet.

use std::{ffi::c_int, fmt, ptr::NonNull};

use audiopus_sys as ffi;

pub const SAMPLE_RATE: i32 = 48_000;
pub const CHANNELS: usize = 2;
/// Samples per channel in one 10 ms frame.
pub const FRAME_SAMPLES: usize = 480;
/// Interleaved samples in one frame.
pub const FRAME_LEN: usize = FRAME_SAMPLES * CHANNELS;
/// Large enough for any Opus packet.
const MAX_PACKET: usize = 1500;

/// A libopus error code, with what was being done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error {
    pub what: &'static str,
    pub code: i32,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // SAFETY: opus_strerror returns a static string for every code.
        let text = unsafe { std::ffi::CStr::from_ptr(ffi::opus_strerror(self.code)) };
        write!(
            f,
            "{}: {} ({})",
            self.what,
            text.to_string_lossy(),
            self.code
        )
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

fn check(what: &'static str, code: c_int) -> Result<c_int> {
    if code < 0 {
        Err(Error { what, code })
    } else {
        Ok(code)
    }
}

pub struct Encoder(NonNull<ffi::OpusEncoder>);

// SAFETY: the encoder state is plain memory owned by this value, used through &mut.
unsafe impl Send for Encoder {}

impl Encoder {
    /// A general-audio encoder at `bitrate_bps`.
    pub fn new(bitrate_bps: u32) -> Result<Self> {
        let mut err = 0;
        // SAFETY: valid rate, channel count and application; err receives the status.
        let raw = unsafe {
            ffi::opus_encoder_create(
                SAMPLE_RATE,
                CHANNELS as c_int,
                ffi::OPUS_APPLICATION_AUDIO as c_int,
                &mut err,
            )
        };
        check("opus_encoder_create", err)?;
        let enc = Self(NonNull::new(raw).ok_or(Error {
            what: "opus_encoder_create",
            code: ffi::OPUS_ALLOC_FAIL,
        })?);
        // SAFETY: a live encoder; OPUS_SET_BITRATE takes one opus_int32.
        check("OPUS_SET_BITRATE", unsafe {
            ffi::opus_encoder_ctl(
                enc.0.as_ptr(),
                ffi::OPUS_SET_BITRATE_REQUEST as c_int,
                bitrate_bps as i32,
            )
        })?;
        Ok(enc)
    }

    /// Encodes exactly one frame ([`FRAME_LEN`] samples), replacing `out`'s contents.
    pub fn encode(&mut self, pcm: &[f32], out: &mut Vec<u8>) -> Result<()> {
        if pcm.len() != FRAME_LEN {
            return Err(Error {
                what: "opus_encode_float: frame size",
                code: ffi::OPUS_BAD_ARG,
            });
        }
        out.clear();
        out.resize(MAX_PACKET, 0);
        // SAFETY: a live encoder, one frame of input, an output buffer of the stated size.
        let n = check("opus_encode_float", unsafe {
            ffi::opus_encode_float(
                self.0.as_ptr(),
                pcm.as_ptr(),
                FRAME_SAMPLES as c_int,
                out.as_mut_ptr(),
                MAX_PACKET as i32,
            )
        })?;
        out.truncate(n as usize);
        Ok(())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: created by opus_encoder_create, destroyed once.
        unsafe { ffi::opus_encoder_destroy(self.0.as_ptr()) }
    }
}

pub struct Decoder(NonNull<ffi::OpusDecoder>);

// SAFETY: as for Encoder.
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new() -> Result<Self> {
        let mut err = 0;
        // SAFETY: valid rate and channel count; err receives the status.
        let raw = unsafe { ffi::opus_decoder_create(SAMPLE_RATE, CHANNELS as c_int, &mut err) };
        check("opus_decoder_create", err)?;
        Ok(Self(NonNull::new(raw).ok_or(Error {
            what: "opus_decoder_create",
            code: ffi::OPUS_ALLOC_FAIL,
        })?))
    }

    /// Decodes one packet into exactly one frame; `None` conceals a lost one.
    pub fn decode(&mut self, packet: Option<&[u8]>, out: &mut [f32]) -> Result<()> {
        if out.len() != FRAME_LEN {
            return Err(Error {
                what: "opus_decode_float: frame size",
                code: ffi::OPUS_BAD_ARG,
            });
        }
        let (data, len) = match packet {
            Some(p) if !p.is_empty() => (p.as_ptr(), p.len() as i32),
            _ => (std::ptr::null(), 0),
        };
        // SAFETY: a live decoder; the packet (or null for concealment) and an output of
        // exactly one frame.
        let n = check("opus_decode_float", unsafe {
            ffi::opus_decode_float(
                self.0.as_ptr(),
                data,
                len,
                out.as_mut_ptr(),
                FRAME_SAMPLES as c_int,
                0,
            )
        })?;
        // A packet longer than 10 ms would not fit; shorter ones are padded with silence.
        out[n as usize * CHANNELS..].fill(0.0);
        Ok(())
    }

    pub fn reset(&mut self) {
        // SAFETY: a live decoder; OPUS_RESET_STATE takes no argument.
        unsafe {
            ffi::opus_decoder_ctl(self.0.as_ptr(), ffi::OPUS_RESET_STATE as c_int);
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: created by opus_decoder_create, destroyed once.
        unsafe { ffi::opus_decoder_destroy(self.0.as_ptr()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frame: usize) -> Vec<f32> {
        (0..FRAME_SAMPLES)
            .flat_map(|i| {
                let t = (frame * FRAME_SAMPLES + i) as f32 / SAMPLE_RATE as f32;
                let s = (t * 440.0 * std::f32::consts::TAU).sin() * 0.5;
                [s, s]
            })
            .collect()
    }

    #[test]
    fn a_tone_survives_the_round_trip() {
        let mut enc = Encoder::new(64_000).unwrap();
        let mut dec = Decoder::new().unwrap();
        let (mut packet, mut out) = (Vec::new(), vec![0.0; FRAME_LEN]);
        let mut energy = 0.0;
        for f in 0..50 {
            enc.encode(&tone(f), &mut packet).unwrap();
            assert!(!packet.is_empty() && packet.len() < 400, "{}", packet.len());
            dec.decode(Some(&packet), &mut out).unwrap();
            if f >= 10 {
                energy += out.iter().map(|s| s * s).sum::<f32>();
            }
        }
        // A 0.5-amplitude sine has mean square 0.125.
        let mean = energy / (40 * FRAME_LEN) as f32;
        assert!((0.09..0.16).contains(&mean), "{mean}");
    }

    #[test]
    fn a_lost_packet_is_concealed_and_sizes_are_checked() {
        let mut enc = Encoder::new(64_000).unwrap();
        let mut dec = Decoder::new().unwrap();
        let (mut packet, mut out) = (Vec::new(), vec![1.0; FRAME_LEN]);
        enc.encode(&tone(0), &mut packet).unwrap();
        dec.decode(Some(&packet), &mut out).unwrap();
        dec.decode(None, &mut out).unwrap();
        dec.reset();
        assert!(enc.encode(&[0.0; 10], &mut packet).is_err());
        assert!(dec.decode(Some(&packet), &mut [0.0; 10]).is_err());
    }
}
