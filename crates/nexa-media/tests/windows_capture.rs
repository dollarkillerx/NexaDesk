//! On a real Windows desktop: several streams of one display at once, each with its own
//! size, frame rate and cursor, as several viewers make them. DXGI allows one desktop
//! duplication per display per process, so they must share it (`nexa_media::fanout`).
//!
//! Needs a screen, so it runs only when asked:
//! `cargo test -p nexa-media --test windows_capture -- --ignored --nocapture`

#![cfg(windows)]

use std::{
    thread,
    time::{Duration, Instant},
};

use nexa_media::{
    CaptureConfig, CaptureEvent, Capturer, EncodeOpts, EncoderConfig, MediaError, NativeImage,
    windows::{DxgiCapturer, encoder},
};
use nexa_wire::control::{Chroma, Codec, Mode};

struct Stream {
    width: u32,
    height: u32,
    fps: u32,
    show_cursor: bool,
}

#[derive(Debug, Default)]
struct Seen {
    frames: u32,
    encoded: u32,
    size: Option<(u32, u32)>,
    gpu: bool,
}

impl Seen {
    /// Frames came, and the encoder made something of them. The hardware encoder shares
    /// the duplication's device, so that is part of the test; the software encoder holds
    /// its first frames back, longer than these short runs.
    fn streamed(&self) -> bool {
        self.frames > 0 && (self.encoded > 0 || !self.gpu)
    }
}

/// Captures and encodes `s` for `run`, as a session's media thread does.
fn stream(s: &Stream, run: Duration) -> Result<Seen, MediaError> {
    let mut capturer = DxgiCapturer::new(CaptureConfig {
        display: 0,
        width: s.width,
        height: s.height,
        fps: s.fps,
        show_cursor: s.show_cursor,
        chroma: Chroma::Yuv420,
    })?;
    let mut encoder = encoder(EncoderConfig {
        codec: Codec::H264,
        chroma: Chroma::Yuv420,
        mode: Mode::Balanced,
        width: s.width,
        height: s.height,
        fps: s.fps,
        bitrate_bps: 4_000_000,
    })?;
    let mut seen = Seen::default();
    let mut out = Vec::new();
    let end = Instant::now() + run;
    while Instant::now() < end {
        let frame = match capturer.next(Duration::from_millis(100))? {
            CaptureEvent::Frame(f) => f,
            CaptureEvent::Idle => continue,
            CaptureEvent::ModeChanged(_) | CaptureEvent::AccessLost => {
                return Err(MediaError::Other("the capture was lost".into()));
            }
        };
        seen.frames += 1;
        seen.size = Some(match &frame.image {
            NativeImage::Texture(t) => {
                seen.gpu = true;
                (t.width(), t.height())
            }
            NativeImage::Cpu(c) => (c.width, c.height),
        });
        out.clear();
        let opts = EncodeOpts {
            force_key: seen.encoded == 0,
        };
        match encoder.encode(&frame, seen.frames, opts, &mut out) {
            Ok(_) if !out.is_empty() => seen.encoded += 1,
            Ok(_) | Err(MediaError::FrameDropped) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(seen)
}

#[test]
#[ignore = "needs a Windows desktop"]
fn one_viewer_streams() {
    let s = Stream {
        width: 1280,
        height: 720,
        fps: 30,
        show_cursor: true,
    };
    let seen = stream(&s, Duration::from_secs(2));
    println!("{seen:?}");
    let seen = seen.unwrap();
    assert!(seen.streamed());
    assert_eq!(seen.size, Some((1280, 720)));
}

#[test]
#[ignore = "needs a Windows desktop"]
fn two_viewers_of_one_display_both_stream() {
    let a = Stream {
        width: 1280,
        height: 720,
        fps: 30,
        show_cursor: true,
    };
    let b = Stream {
        width: 960,
        height: 540,
        fps: 15,
        show_cursor: true,
    };
    let run = Duration::from_secs(2);
    let (first, second) = thread::scope(|scope| {
        let first = scope.spawn(|| stream(&a, run));
        // The second viewer arrives while the first is streaming.
        thread::sleep(Duration::from_millis(300));
        let second = scope.spawn(|| stream(&b, run));
        (first.join().unwrap(), second.join().unwrap())
    });
    println!("first: {first:?}\nsecond: {second:?}");
    let (first, second) = (first.unwrap(), second.unwrap());
    assert!(first.streamed());
    assert!(second.streamed());
    assert_eq!(first.size, Some((1280, 720)));
    assert_eq!(second.size, Some((960, 540)));
}

#[test]
#[ignore = "needs a Windows desktop"]
fn a_viewer_rebuilds_its_stream_while_another_watches() {
    let watching = Stream {
        width: 1280,
        height: 720,
        fps: 30,
        show_cursor: true,
    };
    thread::scope(|scope| {
        let other = scope.spawn(|| stream(&watching, Duration::from_secs(3)));
        thread::sleep(Duration::from_millis(300));
        // A new quality each time: the old stages go, new ones come, three times over.
        for (width, height) in [(1920, 1080), (640, 360), (1280, 720)] {
            let s = Stream {
                width,
                height,
                fps: 30,
                show_cursor: false,
            };
            let seen = stream(&s, Duration::from_millis(500)).unwrap();
            println!("rebuilt at {width}x{height}: {seen:?}");
            assert!(
                seen.frames > 0,
                "a joining stream starts from the latest picture"
            );
            assert_eq!(seen.size, Some((width, height)));
        }
        let other = other.join().unwrap().unwrap();
        println!("watching all along: {other:?}");
        assert!(other.streamed());
    });
}
