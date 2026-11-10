//! Capability probing, §3.5: create real sessions instead of trusting tables.

use std::time::Instant;

use nexa_wire::control::{Caps, Chroma, Codec, CodecCap, Mode};
use objc2_core_media::{CMVideoCodecType, kCMVideoCodecType_H264, kCMVideoCodecType_HEVC};
use objc2_video_toolbox::VTIsHardwareDecodeSupported;

use super::{DecoderOutput, Nv12Pool, PixelBuffer, VtDecoder, VtEncoder};
use crate::{CapturedFrame, Decoder, EncodeOpts, Encoder, EncoderConfig, FrameMeta, NativeImage};

const PROBE_WIDTH: u32 = 1920;
const PROBE_HEIGHT: u32 = 1080;
/// Encoding must beat one frame interval at this rate to be advertised.
const MAX_ADVERTISED_FPS: u32 = 120;
const FALLBACK_FPS: u32 = 60;
const WARMUP_FRAMES: u32 = 4;
/// Largest sizes to try, biggest first. The probe size itself is the floor.
const SIZE_LADDER: [(u32, u32); 2] = [(7680, 4320), (3840, 2160)];

struct Probed {
    codec: Codec,
    encoded: bool,
    decoded: bool,
    ltr: bool,
    /// Time to encode the probe's last frame, after warm-up.
    encode_ms: Option<f64>,
    /// Largest size a real encode and decode succeeded at.
    max_size: (u32, u32),
}

fn config(codec: Codec, width: u32, height: u32) -> EncoderConfig {
    EncoderConfig {
        codec,
        chroma: Chroma::Yuv420,
        mode: Mode::Balanced,
        width,
        height,
        fps: 60,
        bitrate_bps: 20_000_000,
    }
}

fn gray_frame(width: u32, height: u32) -> Option<PixelBuffer> {
    let frame = Nv12Pool::new(width, height).and_then(|p| p.get()).ok()?;
    frame
        .with_planes_mut(|y, _, uv, _| {
            y.fill(0x60);
            uv.fill(0x80);
        })
        .ok()?;
    Some(frame)
}

fn captured(frame: &PixelBuffer) -> CapturedFrame {
    CapturedFrame {
        image: NativeImage::PixelBuffer(frame.clone()),
        captured_at: Instant::now(),
        dirty: Vec::new(),
    }
}

fn decodes(codec: Codec, bitstream: &[u8]) -> bool {
    VtDecoder::new(codec, DecoderOutput::Display)
        .is_ok_and(|mut d| d.decode(bitstream, FrameMeta { frame_id: 0, capture_ts_us: 0 }).is_ok())
}

/// Whether one frame of this size really encodes and decodes.
fn size_works(codec: Codec, width: u32, height: u32) -> bool {
    let Some(frame) = gray_frame(width, height) else { return false };
    let Ok(mut encoder) = VtEncoder::new(config(codec, width, height)) else { return false };
    let mut bitstream = Vec::new();
    encoder.encode(&captured(&frame), 0, EncodeOpts::default(), &mut bitstream).is_ok()
        && !bitstream.is_empty()
        && decodes(codec, &bitstream)
}

/// Encodes a few frames in a real session and decodes the key frame in a real session.
fn probe_codec(codec: Codec, codec_type: CMVideoCodecType) -> Probed {
    let mut probed = Probed {
        codec,
        encoded: false,
        decoded: false,
        ltr: false,
        encode_ms: None,
        max_size: (0, 0),
    };
    let Some(frame) = gray_frame(PROBE_WIDTH, PROBE_HEIGHT) else { return probed };
    let Ok(mut encoder) = VtEncoder::new(config(codec, PROBE_WIDTH, PROBE_HEIGHT)) else { return probed };

    let mut key_frame = Vec::new();
    let mut bitstream = Vec::new();
    for n in 0..WARMUP_FRAMES {
        let started = Instant::now();
        let encoded = encoder.encode(&captured(&frame), n, EncodeOpts::default(), &mut bitstream);
        if encoded.is_err() || bitstream.is_empty() {
            return probed;
        }
        if n == 0 {
            key_frame.clone_from(&bitstream);
        }
        // The first frame includes session warm-up, so only the last counts.
        probed.encode_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
    }
    probed.encoded = encoder.caps().hardware;
    probed.ltr = probed.encoded && encoder.caps().ltr;
    drop(encoder);

    // SAFETY: a plain query taking a codec FourCC.
    let hw_decode = unsafe { VTIsHardwareDecodeSupported(codec_type) };
    probed.decoded = hw_decode && decodes(codec, &key_frame);

    if probed.encoded && probed.decoded {
        probed.max_size = SIZE_LADDER
            .into_iter()
            .find(|&(w, h)| size_works(codec, w, h))
            .unwrap_or((PROBE_WIDTH, PROBE_HEIGHT));
    }
    probed
}

fn cap(p: &Probed) -> CodecCap {
    // Chroma 4:4:4 is not advertised: M0 found no hardware 4:4:4 encode, and
    // nothing here can exercise a 4:4:4 hardware decode without such an encoder.
    CodecCap {
        codec: p.codec as i32,
        chroma: Chroma::Yuv420 as i32,
        max_w: p.max_size.0,
        max_h: p.max_size.1,
    }
}

/// Probes hardware encode and decode with real sessions, including the largest
/// workable size. This takes a noticeable fraction of a second, so per §3.5 the
/// caller should cache the result.
pub fn probe_caps() -> Caps {
    let probes = [probe_codec(Codec::Hevc, kCMVideoCodecType_HEVC), probe_codec(Codec::H264, kCMVideoCodecType_H264)];
    let any_ltr = probes.iter().any(|p| p.ltr);

    // Only advertise the high rate when a warm encode clearly fits its budget.
    let slowest_ms = probes.iter().filter_map(|p| p.encode_ms).fold(0.0, f64::max);
    let budget_ms = 1000.0 / f64::from(MAX_ADVERTISED_FPS);
    let max_fps = if slowest_ms <= budget_ms * 2.0 { MAX_ADVERTISED_FPS } else { FALLBACK_FPS };

    Caps {
        encode: probes.iter().filter(|p| p.encoded).map(cap).collect(),
        decode: probes.iter().filter(|p| p.decoded).map(cap).collect(),
        rfi: false,
        ltr: any_ltr,
        max_fps,
    }
}

/// What [`crate::caps_cache`] keys this Mac by: the OS build (the drivers ship
/// with it) and the default Metal device.
pub fn system_key() -> crate::caps_cache::CapsKey {
    use objc2_metal::{MTLCreateSystemDefaultDevice, MTLDevice};
    let os_build = {
        let mut buf = [0u8; 64];
        let mut len = buf.len();
        // SAFETY: the name is a valid C string and `buf`/`len` describe a writable buffer.
        let rc = unsafe {
            libc::sysctlbyname(
                c"kern.osversion".as_ptr(),
                buf.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc == 0 {
            String::from_utf8_lossy(&buf[..len.min(buf.len())])
                .trim_end_matches('\0')
                .to_owned()
        } else {
            String::new()
        }
    };
    let gpu = MTLCreateSystemDefaultDevice()
        .map(|d| d.name().to_string())
        .unwrap_or_default();
    crate::caps_cache::CapsKey { os_build, gpu }
}

/// [`probe_caps`] through `caps.json` at `path` (§3.5): probes only on a new OS
/// build or GPU. Without a usable key (no build string) it always probes.
pub fn cached_caps(path: &std::path::Path) -> Caps {
    let key = system_key();
    if key.os_build.is_empty() {
        return probe_caps();
    }
    crate::caps_cache::cached(path, &key, probe_caps)
}

#[cfg(test)]
mod tests {
    #[test]
    fn this_mac_has_a_cache_key() {
        let key = super::system_key();
        assert!(!key.os_build.is_empty() && key.os_build.chars().all(|c| c.is_ascii_alphanumeric()), "{key:?}");
        assert!(!key.gpu.is_empty(), "{key:?}");
    }
}
