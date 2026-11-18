//! What an iOS viewer can decode. It encodes nothing, so unlike macOS's
//! `probe_caps` there is no encode session to time: the hardware decoders are
//! asked directly.

use nexa_wire::control::{Caps, Chroma, Codec, CodecCap};
use objc2_core_media::{CMVideoCodecType, kCMVideoCodecType_H264, kCMVideoCodecType_HEVC};
use objc2_video_toolbox::VTIsHardwareDecodeSupported;

/// Every iPhone and iPad with a hardware HEVC decoder plays 4K at 60 frames.
const MAX_SIZE: (u32, u32) = (3840, 2160);
/// A phone's panel shows at most 120; 60 keeps the host's encoder and the link
/// at the rate most screens show.
const MAX_FPS: u32 = 60;

fn cap(codec: Codec, codec_type: CMVideoCodecType) -> Option<CodecCap> {
    // SAFETY: a plain query taking a codec FourCC.
    unsafe { VTIsHardwareDecodeSupported(codec_type) }.then_some(CodecCap {
        codec: codec as i32,
        chroma: Chroma::Yuv420 as i32,
        max_w: MAX_SIZE.0,
        max_h: MAX_SIZE.1,
    })
}

/// Without a hardware decoder (the iOS Simulator), VideoToolbox still decodes H.264 in
/// software, fast enough for this size.
const SOFTWARE_SIZE: (u32, u32) = (1920, 1080);

/// Hardware decode of HEVC and H.264, HEVC first (the host prefers the first); H.264
/// in software when there is no hardware decoder at all.
pub fn decode_caps() -> Caps {
    let mut decode: Vec<CodecCap> = [
        cap(Codec::Hevc, kCMVideoCodecType_HEVC),
        cap(Codec::H264, kCMVideoCodecType_H264),
    ]
    .into_iter()
    .flatten()
    .collect();
    if decode.is_empty() {
        decode.push(CodecCap {
            codec: Codec::H264 as i32,
            chroma: Chroma::Yuv420 as i32,
            max_w: SOFTWARE_SIZE.0,
            max_h: SOFTWARE_SIZE.1,
        });
    }
    Caps {
        encode: Vec::new(),
        decode,
        rfi: false,
        ltr: false,
        max_fps: MAX_FPS,
    }
}
