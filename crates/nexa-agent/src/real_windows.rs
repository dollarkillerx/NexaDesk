//! The real Windows host (docs/WINDOWS.md stage 2): DXGI capture, Media Foundation
//! H.264, SendInput, the Windows clipboard and cursor. Everything is in physical
//! pixels, so a ScreenInfo's points and pixels are the same numbers.

use std::time::Instant;

use bytes::Bytes;
use nexa_clip::{
    ClipKind, ClipboardBackend, FormatInfo, Gen, LocalSnapshot, platform::windows::WindowsClipboard,
};
use nexa_input::{Hid, platform::windows::Injector};
use nexa_media::{
    CaptureConfig, Capturer, CursorSource, Encoder, EncoderConfig,
    windows::{Display, DxgiCapturer, MfEncoder, WinCursorSource, displays},
};
use nexa_session::{clip::ClipPlatform, input::InputSink};

use nexa_agent::{Platform, ScreenInfo};

pub struct RealPlatform;

struct InjectorSink(Injector);

impl InputSink for InjectorSink {
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

struct Clipboard(WindowsClipboard);

impl ClipboardBackend for Clipboard {
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
        inline: &[(ClipKind, Bytes)],
    ) -> nexa_clip::Result<()> {
        self.0.write_promise(generation, formats, inline)
    }
}

impl ClipPlatform for Clipboard {
    fn poll(&mut self) -> nexa_clip::Result<Option<LocalSnapshot>> {
        self.0.poll()
    }
}

fn describe(d: &Display, index: usize) -> ScreenInfo {
    let (x, y, w, h) = d.rect;
    ScreenInfo {
        display_id: d.id,
        origin_points: (f64::from(x), f64::from(y)),
        size_points: (f64::from(w), f64::from(h)),
        size_pixels: (w, h),
        refresh_hz: d.refresh_hz,
        name: if d.primary {
            "Main display".to_owned()
        } else {
            format!("Display {}", index + 1)
        },
    }
}

impl Platform for RealPlatform {
    fn screen(&self) -> Result<ScreenInfo, String> {
        let all = displays()?;
        all.first()
            .map(|d| describe(d, 0))
            .ok_or_else(|| "no display".to_owned())
    }

    fn displays(&self) -> Result<Vec<ScreenInfo>, String> {
        Ok(displays()?
            .iter()
            .enumerate()
            .map(|(i, d)| describe(d, i))
            .collect())
    }

    fn cursor_source(&self) -> Option<Box<dyn CursorSource>> {
        Some(Box::new(WinCursorSource::new()))
    }

    fn capturer(&self, config: CaptureConfig) -> nexa_media::Result<Box<dyn Capturer>> {
        Ok(Box::new(DxgiCapturer::new(config)?))
    }

    fn encoder(&self, config: EncoderConfig) -> nexa_media::Result<Box<dyn Encoder>> {
        Ok(Box::new(MfEncoder::new(config)?))
    }

    fn input_available(&self) -> bool {
        true
    }

    fn input_sink(&self) -> Result<Box<dyn InputSink>, String> {
        Ok(Box::new(InjectorSink(Injector::new())))
    }

    fn clipboard(&self) -> Result<Box<dyn ClipPlatform>, String> {
        Ok(Box::new(Clipboard(WindowsClipboard::new())))
    }
}
