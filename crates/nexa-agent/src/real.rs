//! The real machine. Only `main.rs` compiles this, so nothing in the library or its
//! tests can reach the injector, the general pasteboard or the screen.

use std::time::{Duration, Instant};

use nexa_clip::{
    ClipKind, ClipboardBackend, FormatInfo, Gen, LocalSnapshot,
    platform::macos::MacClipboardBackend,
};
use nexa_input::{
    Hid,
    platform::macos::{EventTarget, HidTarget, Injector, accessibility_granted},
};
use nexa_media::{
    AudioCapture, AudioEncoder, CaptureConfig, Capturer, CursorSource, Encoder, EncoderConfig,
    macos::{
        OpusEncoder, ScreenCapturer, SystemAudioCapture, SystemCursorSource, VtEncoder,
        wake_and_wait,
    },
};
use nexa_session::{clip::ClipPlatform, input::InputSink};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayIsBuiltin,
    CGDisplayMode, CGGetActiveDisplayList, CGMainDisplayID,
};

use nexa_agent::{Platform, ScreenInfo};

pub struct RealPlatform;

const WAKE_REASON: &str = "NexaDesk: a viewer connected";
/// How long a sleeping display may take to come back (as the capturer allows).
const WAKE_WAIT: Duration = Duration::from_secs(4);

/// The displays that are on: a sleeping one is not listed.
fn active_displays() -> Result<Vec<CGDirectDisplayID>, String> {
    let mut ids = [0 as CGDirectDisplayID; 16];
    let mut count = 0u32;
    // SAFETY: both pointers are valid for the array's length.
    let status = unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
    if status.0 != 0 {
        return Err(format!("CGGetActiveDisplayList failed: {}", status.0));
    }
    Ok(ids[..count as usize].to_vec())
}

struct InjectorSink<T: EventTarget>(Injector<T>);

impl<T: EventTarget + Send> InputSink for InjectorSink<T> {
    fn key(&mut self, hid: u32, down: bool, repeat: bool, now: Instant) {
        self.0.key(Hid(hid), down, repeat, now);
    }
    fn move_absolute(&mut self, x: f64, y: f64) {
        self.0.move_absolute(x, y);
    }
    fn move_relative(&mut self, dx: i32, dy: i32) {
        self.0.move_relative(dx, dy);
    }
    fn button(&mut self, button: u32, down: bool, at: Option<(f64, f64)>, now: Instant) {
        self.0.button(button, down, at, now);
    }
    fn wheel(&mut self, dx: i32, dy: i32) {
        self.0.wheel(dx, dy);
    }
    fn text(&mut self, text: &str) {
        self.0.text(text);
    }
    fn apply_key_state(&mut self, pressed: &[u32], now: Instant) {
        self.0.apply_key_state(pressed, now);
    }
    fn poll_timeout(&mut self, now: Instant) {
        self.0.poll_timeout(now);
    }
    fn release_all(&mut self, now: Instant) {
        self.0.release_all(now);
    }
}

struct GeneralClipboard(MacClipboardBackend);

impl ClipboardBackend for GeneralClipboard {
    fn read_formats(&mut self) -> nexa_clip::Result<Vec<FormatInfo>> {
        self.0.read_formats()
    }
    fn read(&mut self, kind: ClipKind) -> nexa_clip::Result<Vec<u8>> {
        self.0.read(kind)
    }
    fn write_promise(
        &mut self,
        generation: Gen,
        formats: &[FormatInfo],
        inline: &[(ClipKind, bytes::Bytes)],
    ) -> nexa_clip::Result<()> {
        self.0.write_promise(generation, formats, inline)
    }
}

impl ClipPlatform for GeneralClipboard {
    fn poll(&mut self) -> nexa_clip::Result<Option<LocalSnapshot>> {
        self.0.poll()
    }
}

fn qos_user_interactive() {
    // SAFETY: documented call on the current thread with a valid class.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
    }
}

fn describe(display_id: CGDirectDisplayID, index: usize) -> Result<ScreenInfo, String> {
    let bounds = CGDisplayBounds(display_id);
    let mode = CGDisplayCopyDisplayMode(display_id)
        .ok_or_else(|| format!("display {display_id} has no current mode"))?;
    let size_pixels = (
        CGDisplayMode::pixel_width(Some(&mode)) as u32,
        CGDisplayMode::pixel_height(Some(&mode)) as u32,
    );
    // NSScreen's localized names need the main thread; these are enough to tell apart.
    let name = if CGDisplayIsBuiltin(display_id) {
        "Built-in display".to_owned()
    } else {
        format!("Display {}", index + 1)
    };
    Ok(ScreenInfo {
        display_id,
        origin_points: (bounds.origin.x, bounds.origin.y),
        size_points: (bounds.size.width, bounds.size.height),
        size_pixels,
        refresh_hz: CGDisplayMode::refresh_rate(Some(&mode)).round() as u32,
        name,
    })
}

impl Platform for RealPlatform {
    fn screen(&self) -> Result<ScreenInfo, String> {
        describe(CGMainDisplayID(), 0).or_else(|e| {
            // Asleep, the main display has no current mode: wake it and ask again.
            wake_and_wait(WAKE_REASON, WAKE_WAIT, || {
                describe(CGMainDisplayID(), 0).ok()
            })
            .ok_or(e)
        })
    }

    fn displays(&self) -> Result<Vec<ScreenInfo>, String> {
        let mut ids = active_displays()?;
        if ids.is_empty() {
            // A sleeping display is not active: wake it as a touch of the mouse would,
            // then look again, rather than end the session with "no display 0".
            ids = wake_and_wait(WAKE_REASON, WAKE_WAIT, || {
                active_displays().ok().filter(|ids| !ids.is_empty())
            })
            .unwrap_or_default();
        }
        let main = CGMainDisplayID();
        // The main display is always index 0, which is what a viewer starts on.
        ids.sort_by_key(|id| *id != main);
        ids.iter()
            .enumerate()
            .map(|(i, id)| describe(*id, i))
            .collect()
    }

    fn cursor_source(&self) -> Option<Box<dyn CursorSource>> {
        Some(Box::new(SystemCursorSource::new()))
    }

    fn agent_view(&self) -> Option<std::sync::Arc<dyn nexa_agent::agent_view::AgentView>> {
        Some(std::sync::Arc::new(crate::mac_view::MacView))
    }

    fn audio_capture(&self, display_id: u32) -> Result<Box<dyn AudioCapture>, String> {
        SystemAudioCapture::start(display_id)
            .map(|c| Box::new(c) as Box<dyn AudioCapture>)
            .map_err(|e| e.to_string())
    }

    fn audio_encoder(&self) -> Result<Box<dyn AudioEncoder>, String> {
        OpusEncoder::new(nexa_session::audio::OPUS_BITRATE)
            .map(|e| Box::new(e) as Box<dyn AudioEncoder>)
            .map_err(|e| e.to_string())
    }

    fn capturer(&self, config: CaptureConfig) -> nexa_media::Result<Box<dyn Capturer>> {
        Ok(Box::new(ScreenCapturer::new(config)?))
    }

    fn encoder(&self, config: EncoderConfig) -> nexa_media::Result<Box<dyn Encoder>> {
        Ok(Box::new(VtEncoder::new(config)?))
    }

    fn input_available(&self) -> bool {
        accessibility_granted()
    }

    fn input_sink(&self) -> Result<Box<dyn InputSink>, String> {
        let injector = Injector::<HidTarget>::system().map_err(|e| e.to_string())?;
        Ok(Box::new(InjectorSink(injector)))
    }

    fn clipboard(&self) -> Result<Box<dyn ClipPlatform>, String> {
        // Content is fetched before it is written (nexa_session::clip), so no promise is
        // ever on the pasteboard for AppKit to ask about.
        let resolver: nexa_clip::platform::macos::PromiseResolver =
            std::sync::Arc::new(|_, _| None);
        Ok(Box::new(GeneralClipboard(MacClipboardBackend::general(
            resolver,
        ))))
    }

    fn thread_init(&self) -> Option<fn()> {
        Some(qos_user_interactive)
    }
}
