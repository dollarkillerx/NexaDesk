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

use nexa_media::{
    CaptureConfig, CaptureEvent, CapturedFrame, Capturer, EncodeOpts, Encoder, EncoderConfig,
    MediaError, NativeImage,
};
use nexa_session::host::IDLE_REFRESH_AFTER;
use nexa_wire::control::{Chroma, Codec, Mode};
use tokio::sync::mpsc;

use crate::{
    StreamStats, WebConfig, WebEvent, WebHost, WebScreen,
    display::{self, Shown},
};

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
/// pixels, for Auto. Left on Auto, a preset made small for a weak network stays at least
/// that small; a size the person chose is the size they get (picking 1080p while Auto had
/// stepped down did nothing before). A larger picture gets a proportionally higher
/// bitrate ceiling.
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
    let small = resolution == Resolution::Auto
        && u64::from(p.max_size.0) * u64::from(p.max_size.1) < REFERENCE_PIXELS;
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
    /// Set when the page's channel opens: the cursor feed says everything again.
    pub cursor_resync: Arc<AtomicBool>,
    /// The display the page asked for (`display.rs`), set by the WebRTC loop; the
    /// thread switches to it, or says why not.
    pub display: std::sync::Mutex<Option<display::Wanted>>,
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

/// What a stream is of, and where it reports.
pub(crate) struct Stream {
    pub host: Arc<dyn WebHost>,
    /// The session's, for its stream statistics.
    pub id: u64,
    /// The display captured; a switch changes it for input and the cursor too.
    pub shown: Arc<Shown>,
    pub show_cursor: bool,
    /// Where the page is told about a display switch (the `input` channel).
    pub notes: Option<mpsc::Sender<String>>,
}

impl MediaThread {
    pub(crate) fn spawn(stream: Stream, config: &WebConfig, out: mpsc::Sender<Frame>) -> Self {
        let control = Arc::new(Control::default());
        *control.preset.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(preset(Quality::Auto, false, config));
        let start_bps = config.start_bitrate_bps;
        let ctl = control.clone();
        let handle = thread::Builder::new()
            .name("web-capture-encode".into())
            .spawn(move || {
                if let Err(e) = run(stream, start_bps, &ctl, out) {
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
fn configs(
    screen: &WebScreen,
    p: Preset,
    bitrate_bps: u32,
    show_cursor: bool,
) -> (CaptureConfig, EncoderConfig) {
    let (w, h) = stream_size(screen.size_pixels, p.max_size);
    (
        CaptureConfig {
            display: screen.display,
            width: w,
            height: h,
            fps: p.fps,
            show_cursor,
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

/// A stream's capture and encode stages.
type Stages = (Box<dyn Capturer>, Box<dyn Encoder>);

/// The least step between two frames' times: one RTP tick is 1/90 000 s, so frames
/// closer than that would share a timestamp.
const MIN_FRAME_STEP: Duration = Duration::from_millis(1);

/// Encoder failures in a row before the session gives up. One is not enough: the
/// Windows encoder answers a hardware failure by going on in software from the next
/// frame.
const MAX_ENCODE_FAILURES: u32 = 30;

/// How long to wait for a new picture: long enough for one at `fps`, short enough that
/// an idle refresh goes out on time.
/// How often a stream says what it did (`WebEvent::StreamStats`), as the app's do.
const STATS_EVERY: Duration = Duration::from_secs(5);

fn frame_wait(fps: u32) -> Duration {
    Duration::from_secs_f64(2.0 / f64::from(fps.max(1)))
}

/// The capture and encode stages for one stream setting. To replace running stages, let
/// go of the old ones first: DXGI desktop duplication allows one per display per
/// process, so a capture made while the old one is alive fails (`DuplicateOutput`,
/// E_INVALIDARG) and ends the session.
fn stages(
    host: &dyn WebHost,
    capture: CaptureConfig,
    encode: EncoderConfig,
) -> Result<Stages, String> {
    let capturer = host.capturer(capture).map_err(|e| e.to_string())?;
    let encoder = host.encoder(encode).map_err(|e| e.to_string())?;
    Ok((capturer, encoder))
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
    stream: Stream,
    start_bps: u32,
    ctl: &Control,
    out: mpsc::Sender<Frame>,
) -> Result<(), String> {
    let Stream {
        host,
        id,
        shown,
        show_cursor,
        notes,
    } = stream;
    let mut screen = shown.screen();
    // A note about a switch that did not fit in the queue: tried again each turn.
    let mut note: Option<String> = None;
    let take_preset = || ctl.preset.lock().unwrap_or_else(|e| e.into_inner()).take();
    let mut current = take_preset().ok_or("no stream settings")?;
    let mut bitrate = start_bps;
    let (mut capture, mut encode) = configs(&screen, current, bitrate, show_cursor);
    let (mut capturer, mut encoder) = stages(&*host, capture, encode)?;
    *ctl.sending.lock().unwrap_or_else(|e| e.into_inner()) =
        (capture.width, capture.height, capture.fps);
    let start = Instant::now();
    let mut frame_id: u32 = 0;
    let mut first = true;
    let mut buf = Vec::new();
    // The last picture, repeated on the app's schedule once the screen goes quiet
    // (`IDLE_REFRESH_AFTER`). The Windows hardware encoder can hold a frame back until the
    // next one goes in, and a still screen sends nothing more: without this the browser
    // kept showing the change before last until something else moved.
    let mut last: Option<NativeImage> = None;
    let mut last_content = Instant::now();
    let mut refresh_step = IDLE_REFRESH_AFTER.len();
    let mut failures = 0;
    let mut sent_at: Option<Duration> = None;
    let mut stats = StreamStats::default();
    let mut stats_from = Instant::now();
    while !ctl.stop.load(Relaxed) {
        if stats_from.elapsed() >= STATS_EVERY {
            host.event(WebEvent::StreamStats {
                id,
                stats: StreamStats {
                    width: capture.width,
                    height: capture.height,
                    fps: capture.fps,
                    over: stats_from.elapsed(),
                    target_bps: bitrate,
                    ..stats
                },
            });
            stats = StreamStats::default();
            stats_from = Instant::now();
        }
        // A new quality: a new size or frame rate needs a new capture and encoder.
        if let Some(next) = take_preset()
            && next != current
        {
            current = next;
            (capture, encode) = configs(&screen, current, bitrate, show_cursor);
            // The old stages go first (see `stages`), the old picture with them.
            last = None;
            drop((capturer, encoder));
            (capturer, encoder) = stages(&*host, capture, encode)?;
            *ctl.sending.lock().unwrap_or_else(|e| e.into_inner()) =
                (capture.width, capture.height, capture.fps);
            first = true;
        }
        // Another display: a new capture and encoder, at the same stream setting.
        let wanted = ctl.display.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(wanted) = wanted {
            // Read again: monitors come and go. Unreadable is the same as not there.
            let displays = host.displays().unwrap_or_default();
            let err = match wanted.find(&displays) {
                None => Some("unavailable"),
                Some((index, d)) if d.screen == screen => {
                    shown.set(index, screen);
                    None
                }
                Some((index, d)) => {
                    let (next_capture, next_encode) =
                        configs(&d.screen, current, bitrate, show_cursor);
                    // The old stages go first (see `stages`): one capture at a time.
                    last = None;
                    drop((capturer, encoder));
                    first = true;
                    match stages(&*host, next_capture, next_encode) {
                        Ok(s) => {
                            (capturer, encoder) = s;
                            (capture, encode) = (next_capture, next_encode);
                            screen = d.screen;
                            // Input and the cursor follow from here on.
                            shown.set(index, screen);
                            None
                        }
                        // As the native session does: stay on what worked. If even that
                        // will not start again, the page is told before the picture ends,
                        // rather than left waiting on a frozen frame.
                        Err(_) => match stages(&*host, capture, encode) {
                            Ok(s) => {
                                (capturer, encoder) = s;
                                Some("failed")
                            }
                            Err(e) => {
                                if let Some(tx) = &notes {
                                    let n = display::note(
                                        shown.index(),
                                        &screen,
                                        &displays,
                                        Some("failed"),
                                    );
                                    let _ = tx.try_send(n);
                                }
                                return Err(e);
                            }
                        },
                    }
                }
            };
            *ctl.sending.lock().unwrap_or_else(|e| e.into_inner()) =
                (capture.width, capture.height, capture.fps);
            note = Some(display::note(shown.index(), &screen, &displays, err));
        }
        if let (Some(n), Some(tx)) = (note.take(), &notes)
            && let Err(mpsc::error::TrySendError::Full(n)) = tx.try_send(n)
        {
            note = Some(n);
        }
        let mut repeat = false;
        let frame = match capturer.next(frame_wait(capture.fps)) {
            Ok(CaptureEvent::Frame(f)) => {
                last = Some(f.image.clone());
                last_content = Instant::now();
                refresh_step = 0;
                f
            }
            Ok(CaptureEvent::Idle) => {
                let due = refresh_step < IDLE_REFRESH_AFTER.len()
                    && last_content.elapsed() >= IDLE_REFRESH_AFTER[refresh_step];
                // A picture that failed is tried again at once, still screen or not: the
                // browser has not got it, and an encoder that never recovers must reach
                // `MAX_ENCODE_FAILURES` and end the session.
                let retry = failures > 0;
                let Some(image) = last.clone().filter(|_| due || retry) else {
                    continue;
                };
                if due {
                    refresh_step += 1;
                }
                repeat = true;
                // Now, not when it was captured: the browser takes a frame with the
                // previous one's timestamp for the same frame.
                CapturedFrame {
                    image,
                    captured_at: Instant::now(),
                    dirty: Vec::new(),
                }
            }
            Ok(CaptureEvent::ModeChanged(_) | CaptureEvent::AccessLost) => {
                // Start over at the same size: the browser scales whatever arrives.
                last = None;
                drop((capturer, encoder));
                (capturer, encoder) = stages(&*host, capture, encode)?;
                first = true;
                continue;
            }
            Err(e) => return Err(e.to_string()),
        };
        if out.capacity() == 0 {
            // The network loop is behind: skip this frame. Not a key frame next: the
            // encoder never saw this one, so the next refers only to frames the browser
            // has. A forced key frame, many times larger, only added to the backlog.
            stats.skipped += 1;
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
        let encoding = Instant::now();
        let encoded = encoder.encode(&frame, frame_id, opts, &mut buf);
        stats.encode_time += encoding.elapsed();
        match encoded {
            Ok(_) => {
                first = false;
                failures = 0;
                stats.frames += 1;
                stats.repeats += u32::from(repeat);
                stats.bytes += buf.len() as u64;
                frame_id = frame_id.wrapping_add(1);
                // Always later than the last frame sent: a refresh is stamped when it is made, and a
                // picture captured just before that can arrive after it.
                let at = frame
                    .captured_at
                    .saturating_duration_since(start)
                    .max(sent_at.map_or(Duration::ZERO, |t| t + MIN_FRAME_STEP));
                sent_at = Some(at);
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
            // The encoder skipped this frame on purpose (it was behind, as on a large
            // screen at 60 fps): nothing was produced and the next frame goes on as
            // usual.
            Err(MediaError::FrameDropped) => {}
            // A frame that failed, as the app's sessions treat it: the next one is a key
            // frame, and the Windows encoder goes on in software. The session ends only
            // if failures keep coming ("the hardware encoder stopped asking for
            // frames" ended browser sessions at once).
            Err(e) => {
                failures += 1;
                stats.failures += 1;
                if failures >= MAX_ENCODE_FAILURES {
                    return Err(e.to_string());
                }
                ctl.force_key.store(true, Relaxed);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FakeHost, display};

    fn stream(
        host: Arc<FakeHost>,
        screen: WebScreen,
        notes: Option<mpsc::Sender<String>>,
    ) -> Stream {
        Stream {
            host,
            id: 0,
            shown: Shown::new(0, screen),
            show_cursor: false,
            notes,
        }
    }

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
        // A weak network keeps its small picture, unless a size is chosen.
        let weak = preset(Quality::Weak, false, &c);
        assert_eq!(
            sized(weak, Resolution::Auto, Some(retina)).max_size,
            weak.max_size
        );
        assert_eq!(sized(weak, Resolution::Native, None).max_size, LARGEST);
        let reduced = preset(Quality::Auto, true, &c);
        assert_eq!(
            stream_size(
                (2560, 1440),
                sized(reduced, Resolution::Lines(1080), None).max_size
            ),
            (1920, 1080)
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

    /// Stands in for DXGI desktop duplication: while one capture of the display is
    /// alive, another fails. `made` counts the captures made.
    fn one_capture_host(alive: Arc<AtomicU32>, made: Arc<AtomicU32>) -> FakeHost {
        FakeHost::default()
            .with_capturer(move || {
                if alive.fetch_add(1, Relaxed) > 0 {
                    alive.fetch_sub(1, Relaxed);
                    return Err(MediaError::Other("already duplicating this display".into()));
                }
                made.fetch_add(1, Relaxed);
                Ok(Box::new(Capture(alive.clone())))
            })
            .with_encoder(|| Ok(Box::new(NoEncode)))
    }

    struct Capture(Arc<AtomicU32>);

    impl Drop for Capture {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Relaxed);
        }
    }

    impl Capturer for Capture {
        fn next(&mut self, timeout: Duration) -> nexa_media::Result<CaptureEvent> {
            thread::sleep(timeout.min(Duration::from_millis(5)));
            Ok(CaptureEvent::Idle)
        }
    }

    struct NoEncode;

    impl Encoder for NoEncode {
        fn caps(&self) -> nexa_media::EncoderCaps {
            nexa_media::EncoderCaps::default()
        }
        fn encode(
            &mut self,
            _: &nexa_media::CapturedFrame,
            _: nexa_media::FrameId,
            _: EncodeOpts,
            _: &mut Vec<u8>,
        ) -> nexa_media::Result<nexa_media::EncodedMeta> {
            Err(MediaError::FrameDropped)
        }
        fn set_bitrate(&mut self, _: u32) -> nexa_media::Result<()> {
            Ok(())
        }
        fn invalidate_from(
            &mut self,
            _: nexa_media::FrameId,
        ) -> nexa_media::Result<nexa_media::Recovery> {
            Err(MediaError::Other("test".into()))
        }
        fn on_frame_acked(&mut self, _: nexa_media::FrameId) {}
        fn force_idr(&mut self) {}
    }

    /// The page reports its window as soon as it opens, which changes the stream: on
    /// Windows the session ended right there, the new capture made beside the old one.
    #[test]
    fn a_new_stream_setting_lets_go_of_the_old_capture_first() {
        let c = WebConfig::default();
        let (alive, made) = (Arc::<AtomicU32>::default(), Arc::<AtomicU32>::default());
        let host = Arc::new(one_capture_host(alive.clone(), made.clone()));
        let screen = WebScreen {
            display: 0,
            origin_points: (0.0, 0.0),
            size_points: (1920.0, 1080.0),
            size_pixels: (1920, 1080),
        };
        let (tx, _rx) = mpsc::channel(4);
        let media = MediaThread::spawn(stream(host, screen, None), &c, tx);
        let wait_for = |n: u32| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while made.load(Relaxed) < n && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
        };
        // The first setting has been taken once the first capture exists.
        wait_for(1);
        *media.control.preset.lock().unwrap() = Some(preset(Quality::Weak, false, &c));
        wait_for(2);
        assert_eq!(*media.control.failed.lock().unwrap(), None);
        assert_eq!(made.load(Relaxed), 2);
        drop(media);
        assert_eq!(alive.load(Relaxed), 0);
    }

    /// Switching displays: the capture moves to the one asked for, input and the cursor
    /// with it, never two captures at once; a display that is not there, or cannot be
    /// captured, leaves the session where it was, and the page is told either way.
    #[test]
    fn switching_displays_moves_the_one_capture_and_says_so() {
        let c = WebConfig::default();
        let main = display(1, "Built-in", (0.0, 0.0), (3024, 1964));
        let side = display(2, "DELL", (1512.0, -200.0), (2560, 1440));
        // Display 3 is listed but its capture fails, as an unplugged one's does.
        let broken = display(3, "Gone", (-1920.0, 0.0), (1920, 1080));
        let alive = Arc::<AtomicU32>::default();
        let most = Arc::<AtomicU32>::default();
        let captured = Arc::new(std::sync::Mutex::new(Vec::<(u32, u32, u32)>::new()));
        let (a, m, log) = (alive.clone(), most.clone(), captured.clone());
        let host = Arc::new(
            FakeHost::default()
                .with_displays(vec![main.clone(), side.clone(), broken])
                .with_capturer_for(move |config| {
                    let now = a.fetch_add(1, Relaxed) + 1;
                    m.fetch_max(now, Relaxed);
                    if now > 1 || config.display == 3 {
                        a.fetch_sub(1, Relaxed);
                        return Err(MediaError::Other("cannot capture this display".into()));
                    }
                    log.lock()
                        .unwrap()
                        .push((config.display, config.width, config.height));
                    Ok(Box::new(Capture(a.clone())))
                })
                .with_encoder(|| Ok(Box::new(NoEncode))),
        );
        let (notes_tx, mut notes) = mpsc::channel(4);
        let s = stream(host, main.screen, Some(notes_tx));
        let shown = s.shown.clone();
        let (tx, _rx) = mpsc::channel(4);
        let media = MediaThread::spawn(s, &c, tx);
        let note = |notes: &mut mpsc::Receiver<String>| -> serde_json::Value {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Ok(n) = notes.try_recv() {
                    return serde_json::from_str(&n).unwrap();
                }
                assert!(Instant::now() < deadline, "no note from the media thread");
                thread::sleep(Duration::from_millis(5));
            }
        };
        let ask = |index: u32| {
            *media.control.display.lock().unwrap() = Some(display::Wanted { index, id: None })
        };

        ask(1);
        let n = note(&mut notes);
        assert_eq!((n["i"].clone(), n.get("err")), (1.into(), None), "{n}");
        assert_eq!(n["pts"], serde_json::json!([1280.0, 720.0]));
        assert_eq!(n["list"].as_array().unwrap().len(), 3);
        assert_eq!((shown.index(), shown.screen()), (1, side.screen));
        // The same stream setting, fitted to the new display's shape.
        assert_eq!(
            *captured.lock().unwrap(),
            [(1, 1846, 1200), (2, 1920, 1080)]
        );
        assert_eq!(*media.control.sending.lock().unwrap(), (1920, 1080, 60));

        // No such display: nothing changes.
        ask(7);
        let n = note(&mut notes);
        assert_eq!(
            (n["i"].clone(), n["err"].clone()),
            (1.into(), "unavailable".into())
        );
        assert_eq!(shown.index(), 1);
        assert_eq!(captured.lock().unwrap().len(), 2);

        // One that cannot be captured: back on the one that worked.
        ask(2);
        let n = note(&mut notes);
        assert_eq!(
            (n["i"].clone(), n["err"].clone()),
            (1.into(), "failed".into())
        );
        assert_eq!((shown.index(), shown.screen()), (1, side.screen));
        assert_eq!(captured.lock().unwrap().last(), Some(&(2, 1920, 1080)));

        // And back to the main display.
        ask(0);
        assert_eq!(note(&mut notes)["i"], 0);
        assert_eq!(shown.screen(), main.screen);
        assert_eq!(*media.control.failed.lock().unwrap(), None);
        drop(media);
        assert_eq!(most.load(Relaxed), 1, "two captures were alive at once");
        assert_eq!(alive.load(Relaxed), 0);
    }

    /// Neither the display asked for nor the old one will capture again (a GPU reset,
    /// both monitors gone): the picture ends, but the page is told first, instead of
    /// waiting on a frozen frame.
    #[test]
    fn a_switch_that_leaves_nothing_to_show_still_answers() {
        let c = WebConfig::default();
        let main = display(1, "Built-in", (0.0, 0.0), (1920, 1080));
        let side = display(2, "DELL", (1920.0, 0.0), (1920, 1080));
        let made = Arc::<AtomicU32>::default();
        let m = made.clone();
        let host = Arc::new(
            FakeHost::default()
                .with_displays(vec![main.clone(), side])
                .with_capturer_for(move |_| {
                    // The first capture works; nothing after it does.
                    if m.fetch_add(1, Relaxed) == 0 {
                        Ok(Box::new(Capture(Arc::default())))
                    } else {
                        Err(MediaError::Other("no capture".into()))
                    }
                })
                .with_encoder(|| Ok(Box::new(NoEncode))),
        );
        let (notes_tx, mut notes) = mpsc::channel(4);
        let (tx, _rx) = mpsc::channel(4);
        let media = MediaThread::spawn(stream(host, main.screen, Some(notes_tx)), &c, tx);
        *media.control.display.lock().unwrap() = Some(display::Wanted {
            index: 1,
            id: Some(2),
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let n = loop {
            if let Ok(n) = notes.try_recv() {
                break serde_json::from_str::<serde_json::Value>(&n).unwrap();
            }
            assert!(Instant::now() < deadline, "the page was never told");
            thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(
            (n["i"].clone(), n["err"].clone()),
            (0.into(), "failed".into())
        );
        while media.control.failed.lock().unwrap().is_none() {
            assert!(Instant::now() < deadline, "the stream did not end");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// A screen that changes once and then stays still (or keeps changing), and an
    /// encoder that writes one byte a frame unless told to fail.
    struct Screen {
        moving: bool,
        shown: bool,
    }

    impl Capturer for Screen {
        fn next(&mut self, timeout: Duration) -> nexa_media::Result<CaptureEvent> {
            if self.moving || !std::mem::replace(&mut self.shown, true) {
                thread::sleep(Duration::from_millis(1));
                return Ok(CaptureEvent::Frame(CapturedFrame {
                    image: NativeImage::Cpu(Arc::new(nexa_media::CpuFrame {
                        width: 2,
                        height: 2,
                        y: vec![0; 4],
                        uv: vec![0; 2],
                    })),
                    captured_at: Instant::now(),
                    dirty: Vec::new(),
                }));
            }
            thread::sleep(timeout);
            Ok(CaptureEvent::Idle)
        }
    }

    struct ScriptEncoder {
        calls: Arc<AtomicU32>,
        fail: fn(u32) -> bool,
    }

    impl Encoder for ScriptEncoder {
        fn caps(&self) -> nexa_media::EncoderCaps {
            nexa_media::EncoderCaps::default()
        }
        fn encode(
            &mut self,
            _: &CapturedFrame,
            frame_id: nexa_media::FrameId,
            _: EncodeOpts,
            out: &mut Vec<u8>,
        ) -> nexa_media::Result<nexa_media::EncodedMeta> {
            let n = self.calls.fetch_add(1, Relaxed) + 1;
            if (self.fail)(n) {
                return Err(MediaError::Other(
                    "the hardware encoder stopped asking for frames".into(),
                ));
            }
            out.push(1);
            Ok(nexa_media::EncodedMeta {
                frame_id,
                key: true,
                recovery_point: false,
            })
        }
        fn set_bitrate(&mut self, _: u32) -> nexa_media::Result<()> {
            Ok(())
        }
        fn invalidate_from(
            &mut self,
            _: nexa_media::FrameId,
        ) -> nexa_media::Result<nexa_media::Recovery> {
            Err(MediaError::Other("test".into()))
        }
        fn on_frame_acked(&mut self, _: nexa_media::FrameId) {}
        fn force_idr(&mut self) {}
    }

    fn script(
        moving: bool,
        fail: fn(u32) -> bool,
    ) -> (MediaThread, mpsc::Receiver<Frame>, Arc<AtomicU32>) {
        let calls = Arc::new(AtomicU32::new(0));
        let counted = calls.clone();
        let host = Arc::new(
            FakeHost::default()
                .with_capturer(move || {
                    Ok(Box::new(Screen {
                        moving,
                        shown: false,
                    }))
                })
                .with_encoder(move || {
                    Ok(Box::new(ScriptEncoder {
                        calls: counted.clone(),
                        fail,
                    }))
                }),
        );
        let screen = WebScreen {
            display: 0,
            origin_points: (0.0, 0.0),
            size_points: (1920.0, 1080.0),
            size_pixels: (1920, 1080),
        };
        let (tx, rx) = mpsc::channel(8);
        let media = MediaThread::spawn(stream(host, screen, None), &WebConfig::default(), tx);
        (media, rx, calls)
    }

    /// Frames sent within `within`, up to `n` of them.
    fn frames(rx: &mut mpsc::Receiver<Frame>, n: usize, within: Duration) -> Vec<Duration> {
        let deadline = Instant::now() + within;
        let mut at = Vec::new();
        while at.len() < n && Instant::now() < deadline {
            match rx.try_recv() {
                Ok(f) => at.push(f.at),
                Err(_) => thread::sleep(Duration::from_millis(5)),
            }
        }
        at
    }

    /// The Windows hardware encoder can hold a frame back until the next goes in: on a
    /// still screen the browser showed the change before last until something moved.
    #[test]
    fn a_still_screen_is_sent_again_after_it_goes_quiet() {
        let (media, mut rx, calls) = script(false, |_| false);
        // The picture, then once each at 100, 300, 600 and 1000 ms of quiet.
        let at = frames(&mut rx, 6, Duration::from_secs(3));
        assert_eq!(at.len(), 5, "{at:?}");
        assert!(
            at.windows(2).all(|w| w[0] < w[1]),
            "each a new frame: {at:?}"
        );
        assert!(at[4] >= Duration::from_millis(1000), "{at:?}");
        assert_eq!(calls.load(Relaxed), 5);
        assert_eq!(*media.control.failed.lock().unwrap(), None);
    }

    #[test]
    fn one_encoder_failure_does_not_end_the_stream() {
        // The first frame fails, as a hardware encoder does just before software takes over.
        let (media, mut rx, _) = script(false, |n| n == 1);
        assert!(!frames(&mut rx, 1, Duration::from_secs(2)).is_empty());
        assert_eq!(*media.control.failed.lock().unwrap(), None);
    }

    #[test]
    fn an_encoder_that_keeps_failing_ends_the_stream() {
        // A still screen too: the failed picture is tried again until the end.
        for moving in [true, false] {
            let (media, mut rx, calls) = script(moving, |_| true);
            let deadline = Instant::now() + Duration::from_secs(5);
            while media.control.failed.lock().unwrap().is_none() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(
                media.control.failed.lock().unwrap().as_deref(),
                Some("the hardware encoder stopped asking for frames"),
                "moving: {moving}"
            );
            assert_eq!(calls.load(Relaxed), MAX_ENCODE_FAILURES);
            assert!(rx.try_recv().is_err());
        }
    }
}
