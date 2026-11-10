//! Opus through AudioToolbox's `AudioConverter`: the system codec, so no libopus to
//! build or ship. 48 kHz stereo float, one 480-frame (10 ms) packet per call.

use std::ffi::c_void;
use std::ptr;

use crate::{AUDIO_CHANNELS, AUDIO_FRAME_LEN, AUDIO_FRAME_SAMPLES, AUDIO_SAMPLE_RATE, AudioDecoder, AudioEncoder, MediaError, Result};

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AudioStreamPacketDescription {
    start_offset: i64,
    variable_frames_in_packet: u32,
    data_byte_size: u32,
}

#[repr(C)]
struct AudioBuffer {
    number_channels: u32,
    data_byte_size: u32,
    data: *mut c_void,
}

#[repr(C)]
struct AudioBufferList {
    number_buffers: u32,
    buffers: [AudioBuffer; 1],
}

type AudioConverterRef = *mut c_void;
type InputProc = unsafe extern "C" fn(
    converter: AudioConverterRef,
    io_packets: *mut u32,
    io_data: *mut AudioBufferList,
    out_descriptions: *mut *mut AudioStreamPacketDescription,
    user: *mut c_void,
) -> i32;

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioConverterNew(
        source: *const AudioStreamBasicDescription,
        destination: *const AudioStreamBasicDescription,
        out: *mut AudioConverterRef,
    ) -> i32;
    fn AudioConverterDispose(converter: AudioConverterRef) -> i32;
    fn AudioConverterReset(converter: AudioConverterRef) -> i32;
    fn AudioConverterSetProperty(converter: AudioConverterRef, id: u32, size: u32, data: *const c_void) -> i32;
    fn AudioConverterFillComplexBuffer(
        converter: AudioConverterRef,
        input: InputProc,
        user: *mut c_void,
        io_output_packets: *mut u32,
        output: *mut AudioBufferList,
        output_descriptions: *mut AudioStreamPacketDescription,
    ) -> i32;
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

const FORMAT_LPCM: u32 = fourcc(b"lpcm");
const FORMAT_OPUS: u32 = fourcc(b"opus");
const FLAG_FLOAT: u32 = 1 << 0;
const FLAG_PACKED: u32 = 1 << 3;
const PROP_ENCODE_BITRATE: u32 = fourcc(b"brat");
/// Returned by our input procs to say "nothing more for now"; any non-zero value
/// works, and the converter keeps its state for the next call.
const NO_MORE_INPUT: i32 = fourcc(b"nmor") as i32;
/// RFC 6716 §3.4.
const MAX_PACKET: usize = 1275;

fn pcm_format() -> AudioStreamBasicDescription {
    let bytes = (AUDIO_CHANNELS * 4) as u32;
    AudioStreamBasicDescription {
        sample_rate: f64::from(AUDIO_SAMPLE_RATE),
        format_id: FORMAT_LPCM,
        format_flags: FLAG_FLOAT | FLAG_PACKED,
        bytes_per_packet: bytes,
        frames_per_packet: 1,
        bytes_per_frame: bytes,
        channels_per_frame: AUDIO_CHANNELS as u32,
        bits_per_channel: 32,
        reserved: 0,
    }
}

fn opus_format() -> AudioStreamBasicDescription {
    AudioStreamBasicDescription {
        sample_rate: f64::from(AUDIO_SAMPLE_RATE),
        format_id: FORMAT_OPUS,
        frames_per_packet: AUDIO_FRAME_SAMPLES as u32,
        channels_per_frame: AUDIO_CHANNELS as u32,
        ..Default::default()
    }
}

fn status(what: &'static str, s: i32) -> Result<()> {
    if s == 0 { Ok(()) } else { Err(MediaError::Os { what, status: s }) }
}

struct Converter(AudioConverterRef);

// SAFETY: an AudioConverter may be used from any thread, one call at a time, which
// `&mut self` on every user guarantees.
unsafe impl Send for Converter {}

impl Converter {
    fn new(from: &AudioStreamBasicDescription, to: &AudioStreamBasicDescription) -> Result<Self> {
        let mut c = ptr::null_mut();
        // SAFETY: valid descriptions and out-pointer.
        status("AudioConverterNew", unsafe { AudioConverterNew(from, to, &mut c) })?;
        if c.is_null() {
            return Err(MediaError::Other("AudioConverterNew returned null".into()));
        }
        Ok(Self(c))
    }
}

impl Drop for Converter {
    fn drop(&mut self) {
        // SAFETY: created by AudioConverterNew and disposed once.
        unsafe { AudioConverterDispose(self.0) };
    }
}

/// What an input proc hands over on its next call.
struct Feed {
    data: *const u8,
    len: usize,
    /// Packets (encoder: frames) the data holds; 0 once handed over.
    packets: u32,
    description: AudioStreamPacketDescription,
}

unsafe extern "C" fn feed_proc(
    _converter: AudioConverterRef,
    io_packets: *mut u32,
    io_data: *mut AudioBufferList,
    out_descriptions: *mut *mut AudioStreamPacketDescription,
    user: *mut c_void,
) -> i32 {
    // SAFETY: `user` is the Feed the caller keeps alive for the converter call, and
    // the other pointers are valid for this call per the AudioConverter contract.
    unsafe {
        let feed = &mut *(user as *mut Feed);
        if feed.packets == 0 {
            *io_packets = 0;
            return NO_MORE_INPUT;
        }
        *io_packets = feed.packets;
        let list = &mut *io_data;
        list.number_buffers = 1;
        list.buffers[0].number_channels = AUDIO_CHANNELS as u32;
        list.buffers[0].data_byte_size = feed.len as u32;
        list.buffers[0].data = feed.data as *mut c_void;
        if !out_descriptions.is_null() {
            *out_descriptions = &mut feed.description;
        }
        feed.packets = 0;
        0
    }
}

/// 10 ms Opus packets from 48 kHz stereo float.
pub struct OpusEncoder {
    converter: Converter,
}

impl OpusEncoder {
    pub fn new(bitrate_bps: u32) -> Result<Self> {
        let converter = Converter::new(&pcm_format(), &opus_format())?;
        // SAFETY: the property takes a UInt32.
        status("AudioConverter bitrate", unsafe {
            AudioConverterSetProperty(converter.0, PROP_ENCODE_BITRATE, 4, (&raw const bitrate_bps).cast())
        })?;
        Ok(Self { converter })
    }
}

impl AudioEncoder for OpusEncoder {
    fn encode(&mut self, pcm: &[f32], out: &mut Vec<u8>) -> Result<()> {
        if pcm.len() != AUDIO_FRAME_LEN {
            return Err(MediaError::Other(format!("expected {AUDIO_FRAME_LEN} samples, got {}", pcm.len())));
        }
        let mut feed = Feed {
            data: pcm.as_ptr().cast(),
            len: std::mem::size_of_val(pcm),
            packets: AUDIO_FRAME_SAMPLES as u32,
            description: AudioStreamPacketDescription::default(),
        };
        let mut packet = [0u8; MAX_PACKET + 1];
        let mut list = AudioBufferList {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: AUDIO_CHANNELS as u32,
                data_byte_size: packet.len() as u32,
                data: packet.as_mut_ptr().cast(),
            }],
        };
        let mut description = AudioStreamPacketDescription::default();
        let mut packets = 1u32;
        // SAFETY: every pointer is valid for the call; `feed` outlives it.
        let s = unsafe {
            AudioConverterFillComplexBuffer(
                self.converter.0,
                feed_proc,
                (&raw mut feed).cast(),
                &mut packets,
                &mut list,
                &mut description,
            )
        };
        if s != 0 && s != NO_MORE_INPUT {
            return Err(MediaError::Os { what: "AudioConverter encode", status: s });
        }
        if packets == 0 {
            // Still filling its look-ahead; nothing to send for this frame.
            out.clear();
            return Ok(());
        }
        let n = (description.data_byte_size as usize).min(list.buffers[0].data_byte_size as usize).min(MAX_PACKET);
        let start = (description.start_offset.max(0) as usize).min(packet.len());
        out.clear();
        out.extend_from_slice(&packet[start..(start + n).min(packet.len())]);
        Ok(())
    }
}

/// Decodes 10 ms Opus packets to 48 kHz stereo float.
pub struct OpusDecoder {
    converter: Converter,
}

impl OpusDecoder {
    pub fn new() -> Result<Self> {
        Ok(Self { converter: Converter::new(&opus_format(), &pcm_format())? })
    }
}

impl AudioDecoder for OpusDecoder {
    fn decode(&mut self, packet: Option<&[u8]>, out: &mut [f32]) -> Result<()> {
        if out.len() != AUDIO_FRAME_LEN {
            return Err(MediaError::Other("output must be one frame".into()));
        }
        // AudioConverter has no way to ask Opus for concealment.
        let Some(packet) = packet.filter(|p| !p.is_empty() && p.len() <= MAX_PACKET) else {
            return Err(MediaError::Unsupported("Opus concealment through AudioConverter"));
        };
        let mut feed = Feed {
            data: packet.as_ptr(),
            len: packet.len(),
            packets: 1,
            description: AudioStreamPacketDescription {
                start_offset: 0,
                variable_frames_in_packet: 0,
                data_byte_size: packet.len() as u32,
            },
        };
        let mut list = AudioBufferList {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: AUDIO_CHANNELS as u32,
                data_byte_size: std::mem::size_of_val(out) as u32,
                data: out.as_mut_ptr().cast(),
            }],
        };
        let mut frames = AUDIO_FRAME_SAMPLES as u32;
        // SAFETY: every pointer is valid for the call; `feed` and `out` outlive it.
        let s = unsafe {
            AudioConverterFillComplexBuffer(
                self.converter.0,
                feed_proc,
                (&raw mut feed).cast(),
                &mut frames,
                &mut list,
                ptr::null_mut(),
            )
        };
        if s != 0 && s != NO_MORE_INPUT {
            return Err(MediaError::Os { what: "AudioConverter decode", status: s });
        }
        // A short decode (the codec's pre-skip at the start) leaves the rest silent.
        let got = (frames as usize * AUDIO_CHANNELS).min(out.len());
        let len = out.len();
        if got < len {
            out.copy_within(0..got, len - got);
            out[..len - got].fill(0.0);
        }
        Ok(())
    }

    fn reset(&mut self) {
        // SAFETY: a valid converter.
        unsafe { AudioConverterReset(self.converter.0) };
    }
}
