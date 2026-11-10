//! VideoToolbox round trips on synthetic frames. No screen capture.
#![cfg(target_os = "macos")]

use std::sync::Arc;
use std::time::Instant;

use nexa_media::macos::bitstream::{NalCodec, NalKind, annexb_nals, nal_kind};
use nexa_media::macos::{DecoderOutput, MetalPresenter, Nv12Pool, PixelBuffer, VtDecoder, VtEncoder, probe_caps};
use nexa_media::{
    CapturedFrame, CpuFrame, Decoder, EncodeOpts, Encoder, EncoderConfig, FrameMeta, MediaError, NativeImage, PresentMode,
    Presenter, Recovery,
};
use nexa_wire::control::{Chroma, Codec, Mode};

fn cfg(codec: Codec, width: u32, height: u32) -> EncoderConfig {
    EncoderConfig {
        codec,
        chroma: Chroma::Yuv420,
        mode: Mode::Office,
        width,
        height,
        fps: 60,
        bitrate_bps: 8_000_000,
    }
}

fn gradient(pool: &Nv12Pool, n: usize, width: usize) -> PixelBuffer {
    let pb = pool.get().unwrap();
    pb.with_planes_mut(|y, stride, uv, _| {
        for (r, row) in y.chunks_exact_mut(stride).enumerate() {
            for (c, p) in row[..width].iter_mut().enumerate() {
                *p = 40 + ((r * 3 + c + n * 7) % 160) as u8;
            }
        }
        uv.fill(128);
    })
    .unwrap();
    pb
}

fn frame(pb: &PixelBuffer) -> CapturedFrame {
    CapturedFrame { image: NativeImage::PixelBuffer(pb.clone()), captured_at: Instant::now(), dirty: Vec::new() }
}

fn meta(n: u32) -> FrameMeta {
    FrameMeta { frame_id: n, capture_ts_us: 0 }
}

fn nal(codec: Codec) -> NalCodec {
    if codec == Codec::Hevc { NalCodec::Hevc } else { NalCodec::H264 }
}

fn decoded_size(frame: &nexa_media::DecodedFrame) -> (u32, u32) {
    match &frame.image {
        NativeImage::PixelBuffer(pb) => (pb.width(), pb.height()),
        NativeImage::Cpu(_) => panic!("VtDecoder returned a CPU frame"),
    }
}

fn round_trip(codec: Codec) {
    let (w, h) = (640, 360);
    let pool = Nv12Pool::new(w, h).unwrap();
    let mut encoder = VtEncoder::new(cfg(codec, w, h)).unwrap();
    assert_eq!(encoder.codec(), codec);
    assert!(encoder.caps().hardware, "expected a hardware encoder");
    assert!(encoder.caps().ltr, "M0: EnableLTR works with low-latency rate control");
    assert!(encoder.rejected_properties().is_empty(), "{:?}", encoder.rejected_properties());
    let mut decoder = VtDecoder::new(codec, DecoderOutput::CpuReadable420v).unwrap();
    let mut out = Vec::new();

    // The decoder must refuse before it has parameter sets.
    let pb = gradient(&pool, 0, w as usize);
    encoder.encode(&frame(&pb), 0, EncodeOpts::default(), &mut out).unwrap();
    assert!(annexb_nals(&out).any(|n| nal_kind(nal(codec), n) == NalKind::Sps));
    let d = decoder.decode(&out, meta(0)).unwrap();
    assert_eq!(decoded_size(&d), (w, h));

    for n in 1..20u32 {
        let pb = gradient(&pool, n as usize, w as usize);
        let m = encoder.encode(&frame(&pb), n, EncodeOpts::default(), &mut out).unwrap();
        assert_eq!(m.frame_id, n);
        assert!(!m.key, "low-latency mode should not insert key frames");
        assert!(!annexb_nals(&out).any(|x| nal_kind(nal(codec), x) == NalKind::Sps), "no in-band sets on P frames");
        assert_eq!(decoded_size(&decoder.decode(&out, meta(n)).unwrap()), (w, h));
    }

    // force_idr and EncodeOpts::force_key both yield key frames with parameter sets.
    encoder.force_idr();
    let pb = gradient(&pool, 21, w as usize);
    let m = encoder.encode(&frame(&pb), 21, EncodeOpts::default(), &mut out).unwrap();
    assert!(m.key && !m.recovery_point);
    assert!(annexb_nals(&out).any(|n| nal_kind(nal(codec), n) == NalKind::Sps));
    let pb = gradient(&pool, 22, w as usize);
    let m = encoder.encode(&frame(&pb), 22, EncodeOpts { force_key: true }, &mut out).unwrap();
    assert!(m.key);
}

#[test]
fn hevc_round_trip() {
    round_trip(Codec::Hevc);
}

#[test]
fn h264_round_trip() {
    round_trip(Codec::H264);
}

#[test]
fn decoder_rejects_parameterless_input_and_garbage_without_panicking() {
    for codec in [Codec::Hevc, Codec::H264] {
        let mut decoder = VtDecoder::new(codec, DecoderOutput::Display).unwrap();
        assert!(decoder.decode(&[], meta(0)).is_err());
        assert!(decoder.decode(&[0, 0, 1], meta(0)).is_err());
        assert!(decoder.decode(&[0, 0, 0, 1, 0x65, 1, 2, 3], meta(0)).is_err());
        let garbage: Vec<u8> = (0..4096u32).map(|i| (i.wrapping_mul(2654435761) >> 11) as u8).collect();
        let _ = decoder.decode(&garbage, meta(0));
    }
}

#[test]
fn malformed_access_units_do_not_panic_or_kill_the_session() {
    for codec in [Codec::Hevc, Codec::H264] {
        let (w, h) = (640, 360);
        let pool = Nv12Pool::new(w, h).unwrap();
        let mut encoder = VtEncoder::new(cfg(codec, w, h)).unwrap();
        let mut decoder = VtDecoder::new(codec, DecoderOutput::CpuReadable420v).unwrap();
        let mut key = Vec::new();
        let pb = gradient(&pool, 0, w as usize);
        encoder.encode(&frame(&pb), 0, EncodeOpts::default(), &mut key).unwrap();
        decoder.decode(&key, meta(0)).unwrap();

        let mut p = Vec::new();
        let pb = gradient(&pool, 1, w as usize);
        encoder.encode(&frame(&pb), 1, EncodeOpts::default(), &mut p).unwrap();

        // Corrupt, truncate and scramble slice data, then corrupt the parameter sets.
        let mut x = 0x1234_5678u32;
        for trial in 0..300u32 {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let mut bad = p.clone();
            match trial % 4 {
                0 => bad.truncate((x as usize) % p.len()),
                1 => bad[(x as usize) % p.len()] ^= 0x5a,
                2 => {
                    let at = (x as usize) % p.len();
                    bad.splice(at..at, [0u8, 0, 1, (x >> 24) as u8]);
                }
                _ => {
                    let at = (x as usize) % key.len().min(100);
                    bad = key.clone();
                    bad[at] ^= (x >> 16) as u8 | 1;
                }
            }
            let _ = decoder.decode(&bad, meta(trial));
        }

        // The decoder still works afterwards from a fresh key frame.
        encoder.force_idr();
        let pb = gradient(&pool, 2, w as usize);
        encoder.encode(&frame(&pb), 2, EncodeOpts::default(), &mut key).unwrap();
        decoder.decode(&key, meta(2)).expect("recovers on the next key frame");
    }
}

#[test]
fn decoder_rebuilds_when_parameter_sets_change() {
    let mut decoder = VtDecoder::new(Codec::Hevc, DecoderOutput::CpuReadable420v).unwrap();
    for (w, h, n) in [(640u32, 360u32, 0u32), (800, 450, 1), (640, 360, 2)] {
        let pool = Nv12Pool::new(w, h).unwrap();
        let mut encoder = VtEncoder::new(cfg(Codec::Hevc, w, h)).unwrap();
        let mut out = Vec::new();
        let pb = gradient(&pool, n as usize, w as usize);
        encoder.encode(&frame(&pb), n, EncodeOpts::default(), &mut out).unwrap();
        assert_eq!(decoded_size(&decoder.decode(&out, meta(n)).unwrap()), (w, h));
    }
}

#[test]
fn cpu_frames_are_accepted() {
    let (w, h) = (320u32, 180u32);
    let cpu = Arc::new(CpuFrame {
        width: w,
        height: h,
        y: vec![0x70; (w * h) as usize],
        uv: vec![0x80; (w * h / 2) as usize],
    });
    let mut encoder = VtEncoder::new(cfg(Codec::H264, w, h)).unwrap();
    let image = CapturedFrame { image: NativeImage::Cpu(cpu), captured_at: Instant::now(), dirty: Vec::new() };
    let mut out = Vec::new();
    assert!(encoder.encode(&image, 0, EncodeOpts::default(), &mut out).unwrap().key);
    assert!(out.starts_with(&[0, 0, 0, 1]));
}

#[test]
fn invalid_cpu_frame_is_an_error() {
    let cpu = Arc::new(CpuFrame { width: 320, height: 180, y: vec![0; 10], uv: vec![] });
    let mut encoder = VtEncoder::new(cfg(Codec::H264, 320, 180)).unwrap();
    let image = CapturedFrame { image: NativeImage::Cpu(cpu), captured_at: Instant::now(), dirty: Vec::new() };
    assert!(matches!(
        encoder.encode(&image, 0, EncodeOpts::default(), &mut Vec::new()),
        Err(MediaError::Other(_))
    ));
}

#[test]
fn unsupported_configurations_are_rejected() {
    let mut c = cfg(Codec::Hevc, 640, 360);
    c.chroma = Chroma::Yuv444;
    assert!(matches!(VtEncoder::new(c), Err(MediaError::Unsupported(_))));
    let mut c = cfg(Codec::Av1, 640, 360);
    c.codec = Codec::Av1;
    assert!(matches!(VtEncoder::new(c), Err(MediaError::Unsupported(_))));
    assert!(VtDecoder::new(Codec::Av1, DecoderOutput::Display).is_err());
}

#[test]
fn invalidate_without_acknowledged_frames_reports_idr() {
    let (w, h) = (640, 360);
    let pool = Nv12Pool::new(w, h).unwrap();
    let mut encoder = VtEncoder::new(cfg(Codec::Hevc, w, h)).unwrap();
    let mut out = Vec::new();
    for n in 0..5u32 {
        let pb = gradient(&pool, n as usize, w as usize);
        encoder.encode(&frame(&pb), n, EncodeOpts::default(), &mut out).unwrap();
    }
    assert_eq!(encoder.invalidate_from(3).unwrap(), Recovery::Idr);
    let pb = gradient(&pool, 5, w as usize);
    let m = encoder.encode(&frame(&pb), 5, EncodeOpts::default(), &mut out).unwrap();
    assert!(m.key && m.recovery_point);
}

#[test]
fn set_bitrate_applies_live() {
    let pool = Nv12Pool::new(640, 360).unwrap();
    let mut encoder = VtEncoder::new(cfg(Codec::Hevc, 640, 360)).unwrap();
    let mut out = Vec::new();
    let pb = gradient(&pool, 0, 640);
    encoder.encode(&frame(&pb), 0, EncodeOpts::default(), &mut out).unwrap();
    encoder.set_bitrate(2_000_000).unwrap();
    let pb = gradient(&pool, 1, 640);
    assert!(!encoder.encode(&frame(&pb), 1, EncodeOpts::default(), &mut out).unwrap().key);
    assert!(encoder.set_bitrate(0).is_err());
}

#[test]
fn probe_reports_real_sessions() {
    let caps = probe_caps();
    let has = |list: &[nexa_wire::control::CodecCap], c: Codec| list.iter().any(|x| x.codec == c as i32);
    assert!(has(&caps.encode, Codec::H264) || has(&caps.encode, Codec::Hevc), "{caps:?}");
    assert!(has(&caps.decode, Codec::H264) || has(&caps.decode, Codec::Hevc), "{caps:?}");
    assert!(!has(&caps.encode, Codec::Av1));
    assert!(caps.max_fps >= 60);
}

#[test]
fn presenter_draws_and_presents_on_an_unattached_layer() {
    let (w, h) = (640u32, 360u32);
    let pool = Nv12Pool::new(w, h).unwrap();
    let mut encoder = VtEncoder::new(cfg(Codec::Hevc, w, h)).unwrap();
    // The default output: GPU-compressed buffers must still bind and draw.
    let mut decoder = VtDecoder::new(Codec::Hevc, DecoderOutput::Display).unwrap();
    let mut presenter = MetalPresenter::offscreen(w, h).unwrap();
    presenter.resize(w, h, 1.0).unwrap();
    let mut out = Vec::new();
    for n in 0..4u32 {
        let pb = gradient(&pool, n as usize, w as usize);
        encoder.encode(&frame(&pb), n, EncodeOpts::default(), &mut out).unwrap();
        let decoded = decoder.decode(&out, meta(n)).unwrap();
        presenter.set_mode(if n % 2 == 0 { PresentMode::Vsync } else { PresentMode::Immediate }).unwrap();
        presenter.present(&decoded).unwrap();
        let bgra = presenter.render_to_bgra(&decoded, w * 2, h).unwrap();
        // Half a pillarbox on each side of 640x360 video in a 1280x360 target.
        assert_eq!(bgra.len(), (w * 2 * h * 4) as usize);
        let pixel = |x: usize, y: usize| &bgra[(y * (w as usize * 2) + x) * 4..][..4];
        assert_eq!(pixel(0, 100), [0, 0, 0, 255], "letterbox bar should be black");
        assert_ne!(pixel(w as usize, 100), [0, 0, 0, 255], "video area should not be black");
    }
}
