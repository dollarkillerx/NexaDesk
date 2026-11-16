//! Capture and encode for one browser: a thread that hands Annex-B frames to the
//! WebRTC loop. Frames are dropped before encoding, never after, when the loop is behind
//! (§1.3), and a key frame is forced whenever the browser asks for one.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering::Relaxed},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use nexa_media::{CaptureConfig, CaptureEvent, EncodeOpts, EncoderConfig};
use nexa_wire::control::{Chroma, Codec, Mode};
use tokio::sync::mpsc;

use crate::{WebConfig, WebHost, WebScreen};

pub(crate) struct Frame {
    pub data: Vec<u8>,
    /// Since the stream started; becomes the RTP timestamp.
    pub at: Duration,
}

/// What the person in the browser asked for. `Auto` follows the network on its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Quality {
    #[default]
    Auto,
    /// Sharpest picture, at the cost of motion: for reading text.
    Clear,
    Balanced,
    /// A small, slow, cheap picture that keeps working on a poor network.
    Weak,
}

impl Quality {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "auto" => Self::Auto,
            "clear" => Self::Clear,
            "balanced" => Self::Balanced,
            "weak" => Self::Weak,
            _ => return None,
        })
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Clear => "clear",
            Self::Balanced => "balanced",
            Self::Weak => "weak",
        }
    }
}

/// How large the picture may be, chosen in the page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Resolution {
    /// As many pixels as the browser's window shows, so nothing is scaled up there.
    #[default]
    Auto,
    /// The host's own pixels.
    Native,
    /// At most this many lines, whatever the shape of the screen.
    Lines(u32),
}

impl Resolution {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "auto" => Self::Auto,
            "native" => Self::Native,
            "1080" => Self::Lines(1080),
            "720" => Self::Lines(720),
            _ => return None,
        })
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Native => "native",
            Self::Lines(1080) => "1080",
            Self::Lines(_) => "720",
        }
    }
}

/// The largest picture there is no point sending: 8K.
const LARGEST: (u32, u32) = (7680, 4320);
/// The picture the bitrate range of a preset is made for.
const REFERENCE_PIXELS: u64 = 1920 * 1200;
/// However large the picture, no more than this.
const MOST_BPS: u32 = 50_000_000;

/// `p` with its picture size set by `resolution`. `window` is the browser window in device
/// pixels, for Auto. A preset made small for a weak network stays at least that small;
/// a larger picture gets a proportionally higher bitrate ceiling.
pub(crate) fn sized(p: Preset, resolution: Resolution, window: Option<(u32, u32)>) -> Preset {
    let wanted = match resolution {
        Resolution::Auto => match window {
            Some((w, h)) => (w.clamp(320, LARGEST.0), h.clamp(240, LARGEST.1)),
            None => p.max_size,
        },
        Resolution::Native => LARGEST,
        // The width follows the screen's shape.
        Resolution::Lines(lines) => (LARGEST.0, lines),
    };
    let small = u64::from(p.max_size.0) * u64::from(p.max_size.1) < REFERENCE_PIXELS;
    let max_size = if small {
        (wanted.0.min(p.max_size.0), wanted.1.min(p.max_size.1))
    } else {
        wanted
    };
    // The width of a "lines" box is open-ended: no screen is wider than twice its height.
    let (w, h) = (max_size.0.min(LARGEST.0), max_size.1.min(LARGEST.1));
    let pixels = u64::from(w.min(h.saturating_mul(2))) * u64::from(h);
    let max_bps = if pixels > REFERENCE_PIXELS {
        ((u64::from(p.max_bps) * pixels / REFERENCE_PIXELS) as u32).min(MOST_BPS.max(p.max_bps))
    } else {
        p.max_bps
    };
    Preset {
        max_size,
        max_bps,
        ..p
    }
}

/// What a quality setting means for the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Preset {
    pub max_size: (u32, u32),
    pub fps: u32,
    pub min_bps: u32,
    pub max_bps: u32,
}

/// The stream for `quality`; `reduced` is Auto's answer to a poor network.
pub(crate) fn preset(quality: Quality, reduced: bool, config: &WebConfig) -> Preset {
    match (quality, reduced) {
        (Quality::Auto, false) => Preset {
            max_size: config.max_size,
            fps: config.fps,
            min_bps: config.min_bitrate_bps,
            max_bps: config.max_bitrate_bps,
        },
        (Quality::Auto, true) => Preset {
            max_size: (1280, 800),
            fps: 30,
            min_bps: 300_000,
            max_bps: 3_000_000,
        },
        (Quality::Clear, _) => Preset {
            max_size: config.max_size,
            fps: 30,
            min_bps: 2_000_000,
            max_bps: config.max_bitrate_bps,
        },
        (Quality::Balanced, _) => Preset {
            max_size: (1600, 1000),
            fps: 30,
            min_bps: 1_000_000,
            max_bps: 8_000_000,
        },
        (Quality::Weak, _) => Preset {
            max_size: (1280, 800),
            fps: 15,
            min_bps: 300_000,
            max_bps: 1_500_000,
        },
    }
}

/// Shared with the WebRTC loop.
#[derive(Default)]
pub(crate) struct Control {
    pub force_key: AtomicBool,
    /// 0 = unchanged.
    pub bitrate: AtomicU32,
    pub stop: AtomicBool,
    /// Why the thread stopped by itself, if it did.
    pub failed: std::sync::Mutex<Option<String>>,
    /// The stream to send, set by the WebRTC loop; the thread rebuilds capture and
    /// encoder when it changes.
    pub preset: std::sync::Mutex<Option<Preset>>,
    /// What the thread is sending now: width, height, fps. 0 until the first frame.
    pub sending: std::sync::Mutex<(u32, u32, u32)>,
}

/// The size sent to the browser: the display's pixels scaled down to fit `max`, even.
pub(crate) fn stream_size(pixels: (u32, u32), max: (u32, u32)) -> (u32, u32) {
    let (w, h) = (pixels.0.max(2) as f64, pixels.1.max(2) as f64);
    let scale = (max.0 as f64 / w).min(max.1 as f64 / h).min(1.0);
    let even = |v: f64| ((v * scale) as u32 / 2 * 2).max(2);
    (even(w), even(h))
}

pub(crate) struct MediaThread {
    pub control: Arc<Control>,
    handle: Option<JoinHandle<()>>,
}

impl MediaThread {
    pub(crate) fn spawn(
        host: Arc<dyn WebHost>,
        screen: WebScreen,
        config: &WebConfig,
        out: mpsc::Sender<Frame>,
    ) -> Self {
        let control = Arc::new(Control::default());
        *control.preset.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(preset(Quality::Auto, false, config));
        let start_bps = config.start_bitrate_bps;
        let ctl = control.clone();
        let handle = thread::Builder::new()
            .name("web-capture-encode".into())
            .spawn(move || {
                if let Err(e) = run(host, screen, start_bps, &ctl, out) {
                    *ctl.failed.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
                }
            })
            .expect("spawn media thread");
        Self {
            control,
            handle: Some(handle),
        }
    }
}

/// Capture and encoder settings for `p` on `screen`.
fn configs(screen: &WebScreen, p: Preset, bitrate_bps: u32) -> (CaptureConfig, EncoderConfig) {
    let (w, h) = stream_size(screen.size_pixels, p.max_size);
    (
        CaptureConfig {
            display: screen.display,
            width: w,
            height: h,
            fps: p.fps,
            show_cursor: true,
            chroma: Chroma::Yuv420,
        },
        EncoderConfig {
            codec: Codec::H264,
            chroma: Chroma::Yuv420,
            mode: Mode::Balanced,
            width: w,
            height: h,
            fps: p.fps,
            bitrate_bps: bitrate_bps.clamp(p.min_bps, p.max_bps),
        },
    )
}

impl Drop for MediaThread {
    fn drop(&mut self) {
        self.control.stop.store(true, Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn run(
    host: Arc<dyn WebHost>,
    screen: WebScreen,
    start_bps: u32,
    ctl: &Control,
    out: mpsc::Sender<Frame>,
) -> Result<(), String> {
    let take_preset = || ctl.preset.lock().unwrap_or_else(|e| e.into_inner()).take();
    let mut current = take_preset().ok_or("no stream settings")?;
    let mut bitrate = start_bps;
    let (mut capture, mut encode) = configs(&screen, current, bitrate);
    let mut capturer = host.capturer(capture).map_err(|e| e.to_string())?;
    let mut encoder = host.encoder(encode).map_err(|e| e.to_string())?;
    *ctl.sending.lock().unwrap_or_else(|e| e.into_inner()) =
        (capture.width, capture.height, capture.fps);
    let start = Instant::now();
    let mut frame_id: u32 = 0;
    let mut first = true;
    let mut buf = Vec::new();
    while !ctl.stop.load(Relaxed) {
        // A new quality: a new size or frame rate needs a new capture and encoder.
        if let Some(next) = take_preset()
            && next != current
        {
            current = next;
            (capture, encode) = configs(&screen, current, bitrate);
            capturer = host.capturer(capture).map_err(|e| e.to_string())?;
            encoder = host.encoder(encode).map_err(|e| e.to_string())?;
            *ctl.sending.lock().unwrap_or_else(|e| e.into_inner()) =
                (capture.width, capture.height, capture.fps);
            first = true;
        }
        let frame = match capturer.next(Duration::from_millis(100)) {
            Ok(CaptureEvent::Frame(f)) => f,
            Ok(CaptureEvent::Idle) => continue,
            Ok(CaptureEvent::ModeChanged(_) | CaptureEvent::AccessLost) => {
                // Start over at the same size: the browser scales whatever arrives.
                capturer = host.capturer(capture).map_err(|e| e.to_string())?;
                encoder = host.encoder(encode).map_err(|e| e.to_string())?;
                first = true;
                continue;
            }
            Err(e) => return Err(e.to_string()),
        };
        if out.capacity() == 0 {
            // The network loop is behind: skip this frame, and make the next one
            // independent of anything the browser may not have.
            ctl.force_key.store(true, Relaxed);
            continue;
        }
        let bps = ctl.bitrate.swap(0, Relaxed);
        if bps != 0 {
            bitrate = bps.clamp(current.min_bps, current.max_bps);
            let _ = encoder.set_bitrate(bitrate);
        }
        let opts = EncodeOpts {
            force_key: first || ctl.force_key.swap(false, Relaxed),
        };
        buf.clear();
        match encoder.encode(&frame, frame_id, opts, &mut buf) {
            Ok(_) => {
                first = false;
                frame_id = frame_id.wrapping_add(1);
                let at = frame.captured_at.saturating_duration_since(start);
                if out
                    .try_send(Frame {
                        data: buf.clone(),
                        at,
                    })
                    .is_err()
                {
                    if out.is_closed() {
                        return Ok(());
                    }
                    ctl.force_key.store(true, Relaxed);
                }
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_get_smaller_as_the_network_gets_worse() {
        let c = WebConfig::default();
        let full = preset(Quality::Auto, false, &c);
        let reduced = preset(Quality::Auto, true, &c);
        let weak = preset(Quality::Weak, false, &c);
        assert_eq!(full.max_size, c.max_size);
        assert!(reduced.max_size.0 < full.max_size.0 && reduced.max_bps < full.max_bps);
        assert!(weak.fps < reduced.fps && weak.max_bps < reduced.max_bps);
        assert_eq!(
            preset(Quality::Clear, true, &c),
            preset(Quality::Clear, false, &c)
        );
        for q in ["auto", "clear", "balanced", "weak"] {
            assert_eq!(Quality::parse(q).unwrap().name(), q);
        }
        assert_eq!(Quality::parse("ultra"), None);
    }

    #[test]
    fn the_picture_follows_the_chosen_resolution() {
        let c = WebConfig::default();
        let full = preset(Quality::Auto, false, &c);
        let retina = (3024, 1890);
        // Auto: as large as the browser window, with more bitrate for more pixels.
        let p = sized(full, Resolution::Auto, Some(retina));
        assert_eq!(p.max_size, retina);
        assert!(p.max_bps > full.max_bps && p.max_bps <= 50_000_000);
        assert_eq!(stream_size((3840, 2160), p.max_size), (3024, 1700));
        // Before the page has said how large its window is: the preset's own size.
        assert_eq!(sized(full, Resolution::Auto, None), full);
        // Native: whatever the host has.
        let native = sized(full, Resolution::Native, Some((800, 600)));
        assert_eq!(stream_size((3840, 2160), native.max_size), (3840, 2160));
        // 720 lines on a 16:10 screen.
        let lines = sized(full, Resolution::Lines(720), None);
        assert_eq!(stream_size((2560, 1600), lines.max_size), (1152, 720));
        assert_eq!(lines.max_bps, full.max_bps);
        // A weak network keeps its small picture.
        let weak = preset(Quality::Weak, false, &c);
        assert_eq!(
            sized(weak, Resolution::Native, None).max_size,
            weak.max_size
        );
        for r in ["auto", "native", "1080", "720"] {
            assert_eq!(Resolution::parse(r).unwrap().name(), r);
        }
        assert_eq!(Resolution::parse("4k"), None);
    }

    #[test]
    fn streams_fit_the_box_and_stay_even() {
        assert_eq!(stream_size((3024, 1964), (1920, 1200)), (1846, 1200));
        assert_eq!(stream_size((1280, 720), (1920, 1200)), (1280, 720));
        let (w, h) = stream_size((1921, 1081), (1920, 1200));
        assert!(w <= 1920 && h <= 1080 && w % 2 == 0 && h % 2 == 0 && w >= 1916);
        assert_eq!(stream_size((0, 0), (1920, 1200)), (2, 2));
    }
}
