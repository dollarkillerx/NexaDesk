//! The real devices on Windows (docs/WINDOWS.md stage 3). Reached only through
//! `nexa_init`.
//!
//! Picture: Media Foundation's H.264 decoder and a D3D11 presenter on the view's HWND.
//! Keys: a low-level keyboard hook (Win, Alt+Tab included). Game mode: Raw Input with
//! the cursor clipped in place.

use std::{
    ffi::c_void,
    sync::{
        Arc,
        mpsc::{Receiver, Sender},
    },
    thread,
};

use nexa_auth::{FileKeyStore, KeyStore};
use nexa_clip::{
    ClipKind, ClipboardBackend, FormatInfo, Gen, LocalSnapshot, platform::windows::WindowsClipboard,
};
use nexa_input::platform::windows::{CaptureEvent, KeyCapture, Motion, PointerCapture};
use nexa_media::{
    AudioDecoder, Decoder, PresentMode, Presenter,
    windows::{D3dPresenter, MfDecoder, OpusDecoder, WasapiOutput},
};
use nexa_session::clip::ClipPlatform;
use nexa_wire::control::{Caps, Codec, Os};

use crate::{
    abi::NEXA_VIEW_HWND,
    deps::{
        AudioOutput, AudioRender, CursorDisplay, Deps, KeySource, KeySourceError, NoCursor,
        PointerLock,
    },
    input::InputCmd,
};

pub struct RealDeps;

struct Speaker(#[allow(dead_code)] WasapiOutput);

impl AudioOutput for Speaker {}

fn forward(events: Receiver<CaptureEvent>, tx: Sender<InputCmd>) {
    let _ = thread::Builder::new()
        .name("input-bridge".into())
        .spawn(move || {
            while let Ok(event) = events.recv() {
                let cmd = match event {
                    CaptureEvent::Key(k) => InputCmd::Key(k),
                    CaptureEvent::HostKey => InputCmd::HostKey,
                };
                if tx.send(cmd).is_err() {
                    return;
                }
            }
        });
}

/// Installs the keyboard hook the first time capture is switched on.
struct HookSource {
    tx: Sender<InputCmd>,
    hook: Option<KeyCapture>,
}

impl KeySource for HookSource {
    fn set_active(&mut self, active: bool) -> Result<(), KeySourceError> {
        if self.hook.is_none() {
            if !active {
                return Ok(());
            }
            let (hook, events) = KeyCapture::start().map_err(|_| KeySourceError::Failed)?;
            forward(events, self.tx.clone());
            self.hook = Some(hook);
        }
        if let Some(hook) = &self.hook {
            hook.set_active(active);
        }
        Ok(())
    }
}

/// Game mode's lock. Dropping the capture frees the cursor and stops Raw Input.
struct GameLock(#[allow(dead_code)] PointerCapture);

impl PointerLock for GameLock {}

struct GeneralClipboard(WindowsClipboard);

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
    fn current(&mut self) -> nexa_clip::Result<Option<LocalSnapshot>> {
        self.0.snapshot().map(Some)
    }
}

impl Deps for RealDeps {
    fn os(&self) -> Os {
        Os::Windows
    }

    fn key_store(&self) -> Box<dyn KeyStore> {
        // Encrypted with DPAPI for this Windows account (nexa-auth's FileKeyStore).
        Box::new(FileKeyStore::new(
            crate::abi::default_data_dir().join("viewer-device-key"),
        ))
    }

    fn caps(&self) -> Caps {
        nexa_media::windows::viewer_caps()
    }

    fn decoder(&self, codec: Codec) -> Result<Box<dyn Decoder>, String> {
        MfDecoder::new(codec)
            .map(|d| Box::new(d) as Box<dyn Decoder>)
            .map_err(|e| e.to_string())
    }

    fn key_source(&self, tx: Sender<InputCmd>) -> Box<dyn KeySource> {
        Box::new(HookSource { tx, hook: None })
    }

    fn fallback_key_source(&self, _tx: Sender<InputCmd>) -> Option<Box<dyn KeySource>> {
        None
    }

    fn clipboard(&self) -> Result<Box<dyn ClipPlatform>, String> {
        Ok(Box::new(GeneralClipboard(WindowsClipboard::new())))
    }

    unsafe fn attach_view(
        &self,
        handle: *mut c_void,
        kind: u32,
        width_px: u32,
        height_px: u32,
        _scale: f32,
        mode: PresentMode,
    ) -> Result<Box<dyn Presenter>, String> {
        if kind != NEXA_VIEW_HWND {
            return Err(format!("unsupported view kind {kind}"));
        }
        // SAFETY: forwarded from the caller: a live HWND that outlives the attachment.
        let mut presenter = unsafe { D3dPresenter::attach(handle, width_px, height_px) }
            .map_err(|e| e.to_string())?;
        presenter.set_mode(mode).map_err(|e| e.to_string())?;
        Ok(Box::new(presenter))
    }

    unsafe fn cursor_display(&self, handle: *mut c_void, kind: u32) -> Box<dyn CursorDisplay> {
        if kind != NEXA_VIEW_HWND {
            return Box::new(NoCursor);
        }
        // SAFETY: forwarded from the caller (UI thread, the view's live HWND).
        match unsafe { crate::real_windows_cursor::WinCursorDisplay::attach(handle) } {
            Some(d) => Box::new(d),
            None => Box::new(NoCursor),
        }
    }

    fn pointer_lock(&self, tx: Sender<InputCmd>) -> Result<Box<dyn PointerLock>, String> {
        let (capture, motion) = PointerCapture::start()?;
        thread::Builder::new()
            .name("pointer-bridge".into())
            .spawn(move || {
                // Ends when the capture is dropped and its thread lets go of the sender.
                while let Ok(Motion { x, y }) = motion.recv() {
                    if tx.send(InputCmd::Relative(x, y)).is_err() {
                        return;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Box::new(GameLock(capture)))
    }

    fn audio_decoder(&self) -> Result<Box<dyn AudioDecoder>, String> {
        OpusDecoder::new()
            .map(|d| Box::new(d) as Box<dyn AudioDecoder>)
            .map_err(|e| e.to_string())
    }

    fn audio_output(&self, render: AudioRender) -> Result<Box<dyn AudioOutput>, String> {
        WasapiOutput::start(render)
            .map(|o| Box::new(Speaker(o)) as Box<dyn AudioOutput>)
            .map_err(|e| e.to_string())
    }

    fn link_options(&self) -> nexa_session::rendezvous::link::LinkOptions {
        nexa_session::rendezvous::link::LinkOptions::system()
    }

    fn tailnet(&self) -> Option<Arc<nexa_net::tailscale::TailnetMonitor>> {
        Some(nexa_session::rendezvous::link::system_tailnet())
    }

    fn lan(&self) -> Option<Arc<nexa_net::discovery::Browser>> {
        nexa_net::discovery::Browser::start().ok().map(Arc::new)
    }
}
