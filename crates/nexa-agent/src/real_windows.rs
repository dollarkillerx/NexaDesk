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
    AudioCapture, AudioEncoder, CaptureConfig, Capturer, CursorSource, Encoder, EncoderConfig,
    windows::{Display, DxgiCapturer, OpusEncoder, WasapiLoopback, WinCursorSource, displays},
};
use nexa_session::{clip::ClipPlatform, input::InputSink};

use nexa_agent::{Platform, ScreenInfo};
use nexa_media::windows::follow_input_desktop;

pub struct RealPlatform;

struct InjectorSink(Injector, CtrlAltDel);

/// Ctrl+Alt+Del from the viewer. SendInput cannot make it (Windows keeps the sequence for
/// itself), so a host running as SYSTEM asks the NexaDesk service, which may
/// (docs/WINDOWS_SERVICE.md §3). Anyone else's host just types the keys, as before.
#[derive(Default)]
struct CtrlAltDel {
    /// The Ctrl and Alt keys held, by HID usage.
    held: Vec<u16>,
}

impl CtrlAltDel {
    const KEYBOARD: u32 = 0x07;
    const CTRL: [u16; 2] = [0xE0, 0xE4];
    const ALT: [u16; 2] = [0xE2, 0xE6];
    /// Delete, and the keypad's Delete.
    const DELETE: [u16; 2] = [0x4C, 0x63];

    fn key(&mut self, hid: u32, down: bool) {
        if hid >> 16 != Self::KEYBOARD {
            return;
        }
        let usage = hid as u16;
        if Self::CTRL.contains(&usage) || Self::ALT.contains(&usage) {
            self.held.retain(|&u| u != usage);
            if down {
                self.held.push(usage);
            }
        } else if down
            && Self::DELETE.contains(&usage)
            && self.held.iter().any(|u| Self::CTRL.contains(u))
            && self.held.iter().any(|u| Self::ALT.contains(u))
            && nexa_user::is_system()
        {
            // Off the input thread: the service answers in a moment.
            let _ = std::thread::Builder::new()
                .name("ctrl-alt-del".into())
                .spawn(ask_service_for_sas);
        }
    }

    fn set(&mut self, pressed: &[u32]) {
        self.held.clear();
        for &hid in pressed {
            self.key(hid, true);
        }
    }
}

/// Asks the service (nexa-service, its `\\.\pipe\nexadesk-service`) to send Ctrl+Alt+Del.
fn ask_service_for_sas() {
    use std::io::{BufRead, BufReader, Write};
    let result = (|| -> std::io::Result<String> {
        let mut pipe = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(r"\\.\pipe\nexadesk-service")?;
        pipe.write_all(b"\"sas\"\n")?;
        let mut answer = String::new();
        BufReader::new(pipe).read_line(&mut answer)?;
        Ok(answer.trim().to_owned())
    })();
    match result {
        Ok(answer) if answer == "\"done\"" => eprintln!("nexa-agent: sent Ctrl+Alt+Del"),
        Ok(answer) => eprintln!("nexa-agent: Ctrl+Alt+Del not sent: {answer}"),
        Err(e) => eprintln!("nexa-agent: Ctrl+Alt+Del not sent: the service: {e}"),
    }
}

/// Input goes to the desktop receiving it now: a UAC prompt's or the lock screen's when this
/// host runs as SYSTEM (docs/WINDOWS_SERVICE.md §4). Otherwise nothing changes.
impl InputSink for InjectorSink {
    fn key(&mut self, hid: u32, down: bool, repeat: bool, now: Instant) {
        follow_input_desktop(false);
        if !repeat {
            self.1.key(hid, down);
        }
        self.0.key(Hid(hid), down, repeat, now);
    }
    fn move_absolute(&mut self, x: f64, y: f64) {
        follow_input_desktop(false);
        self.0.move_absolute(x, y);
    }
    fn move_relative(&mut self, dx: i32, dy: i32) {
        follow_input_desktop(false);
        self.0.move_relative(dx, dy);
    }
    fn button(&mut self, button: u32, down: bool, at: Option<(f64, f64)>, now: Instant) {
        follow_input_desktop(false);
        self.0.button(button, down, at, now);
    }
    fn wheel(&mut self, dx: i32, dy: i32) {
        follow_input_desktop(false);
        self.0.wheel(dx, dy);
    }
    fn text(&mut self, text: &str) {
        follow_input_desktop(false);
        self.0.text(text);
    }
    fn apply_key_state(&mut self, pressed: &[u32], now: Instant) {
        follow_input_desktop(false);
        self.1.set(pressed);
        self.0.apply_key_state(pressed, now);
    }
    fn poll_timeout(&mut self, now: Instant) {
        self.0.poll_timeout(now);
    }
    fn release_all(&mut self, now: Instant) {
        follow_input_desktop(false);
        self.1.set(&[]);
        self.0.release_all(now);
    }
    fn system(&mut self, action: nexa_session::proto::SystemAction, _now: Instant) {
        match action {
            // Win+L cannot be typed with SendInput: Windows keeps it for itself.
            nexa_session::proto::SystemAction::LockScreen => {
                follow_input_desktop(true);
                // SAFETY: no arguments; it only asks Windows to lock this session.
                if unsafe { windows_sys::Win32::System::Shutdown::LockWorkStation() } == 0 {
                    eprintln!(
                        "nexa-agent: could not lock the screen: {}",
                        std::io::Error::last_os_error()
                    );
                } else {
                    eprintln!("nexa-agent: locked the screen, as the viewer asked");
                }
            }
        }
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

    /// What the default output device plays, whichever display is shown.
    fn audio_capture(&self, _display_id: u32) -> Result<Box<dyn AudioCapture>, String> {
        WasapiLoopback::start()
            .map(|c| Box::new(c) as Box<dyn AudioCapture>)
            .map_err(|e| e.to_string())
    }

    fn audio_encoder(&self) -> Result<Box<dyn AudioEncoder>, String> {
        OpusEncoder::new(nexa_session::audio::OPUS_BITRATE)
            .map(|e| Box::new(e) as Box<dyn AudioEncoder>)
            .map_err(|e| e.to_string())
    }

    fn capturer(&self, config: CaptureConfig) -> nexa_media::Result<Box<dyn Capturer>> {
        Ok(Box::new(DxgiCapturer::new(config)?))
    }

    fn agent_view(&self) -> Option<std::sync::Arc<dyn nexa_agent::agent_view::AgentView>> {
        Some(std::sync::Arc::new(crate::win_view::WinView))
    }

    fn keep_display_awake(&self) -> Option<Box<dyn Send>> {
        // Windows ties the request to a thread: one of its own, until the guard goes.
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        std::thread::Builder::new()
            .name("nexa-agent-awake".into())
            .spawn(move || {
                let _awake = nexa_media::windows::DisplayAwake::hold();
                let _ = rx.recv();
            })
            .ok()?;
        Some(Box::new(tx))
    }

    fn encoder(&self, config: EncoderConfig) -> nexa_media::Result<Box<dyn Encoder>> {
        nexa_media::windows::encoder(config)
    }

    fn input_available(&self) -> bool {
        true
    }

    fn input_sink(&self) -> Result<Box<dyn InputSink>, String> {
        Ok(Box::new(InjectorSink(
            Injector::new(),
            CtrlAltDel::default(),
        )))
    }

    fn clipboard(&self) -> Result<Box<dyn ClipPlatform>, String> {
        Ok(Box::new(Clipboard(WindowsClipboard::new())))
    }
}
