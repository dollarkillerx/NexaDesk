//! The real devices. Reached only through `nexa_init`.

use std::{
    ffi::c_void,
    sync::{Arc, mpsc::Sender},
    thread,
};

use dispatch2::DispatchQueue;
use nexa_auth::{KeyStore, KeychainStore};
use nexa_clip::{
    ClipKind, ClipboardBackend, FormatInfo, Gen, LocalSnapshot,
    platform::macos::MacClipboardBackend,
};
use nexa_input::platform::macos::{
    CaptureError, CaptureEvent, KeyCapture, KeyCaptureConfig, LocalKeyMonitor, PointerCapture,
    SystemAssociation, accessibility_granted,
};
use nexa_media::{
    AudioDecoder, Decoder, PresentMode, Presenter,
    macos::{AudioOutputUnit, DecoderOutput, MetalPresenter, OpusDecoder, VtDecoder, probe_caps},
};
use nexa_session::clip::ClipPlatform;
use nexa_wire::control::{Caps, Codec, Os};
use objc2::MainThreadMarker;

use crate::{
    abi::NEXA_VIEW_NSVIEW,
    deps::{
        AudioOutput, AudioRender, CursorDisplay, Deps, KeySource, KeySourceError, NoCursor,
        PointerLock,
    },
    input::InputCmd,
    real_cursor::MacCursorDisplay,
};

pub struct RealDeps;

fn forward(events: std::sync::mpsc::Receiver<CaptureEvent>, tx: Sender<InputCmd>) {
    let _ = thread::Builder::new()
        .name("input-bridge".into())
        .spawn(move || {
            while let Ok(event) = events.recv() {
                let cmd = match event {
                    CaptureEvent::Key(k) => InputCmd::Key(k),
                    CaptureEvent::HostKey => InputCmd::HostKey,
                    CaptureEvent::Interrupted => InputCmd::Interrupted,
                    CaptureEvent::Text(c) => InputCmd::Text(c.to_string()),
                };
                if tx.send(cmd).is_err() {
                    return; // dropping `events` makes the tap stop swallowing keys
                }
            }
        });
}

/// Follows this Mac's keyboard input source: the system posts a distributed
/// notification on every change, however it was made (Caps Lock, the Globe key, ⌃Space,
/// the menu bar). TIS asserts that it is read on the main thread, so the observer is
/// added and removed there, and its callback runs there.
struct InputSourceWatcher(usize);

/// What the observer's callback reaches, by pointer, for as long as it is registered.
struct SourceWatch {
    tx: Sender<InputCmd>,
}

type CFNotificationCenterRef = *const c_void;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFNotificationCenterGetDistributedCenter() -> CFNotificationCenterRef;
    fn CFNotificationCenterAddObserver(
        center: CFNotificationCenterRef,
        observer: *const c_void,
        callback: extern "C" fn(
            CFNotificationCenterRef,
            *mut c_void,
            *const c_void,
            *const c_void,
            *const c_void,
        ),
        name: *const c_void,
        object: *const c_void,
        suspension: isize,
    );
    fn CFNotificationCenterRemoveObserver(
        center: CFNotificationCenterRef,
        observer: *const c_void,
        name: *const c_void,
        object: *const c_void,
    );
}

/// CFNotificationSuspensionBehaviorDeliverImmediately.
const DELIVER_IMMEDIATELY: isize = 4;

extern "C" fn on_input_source_changed(
    _center: CFNotificationCenterRef,
    observer: *mut c_void,
    _name: *const c_void,
    _object: *const c_void,
    _info: *const c_void,
) {
    if observer.is_null() {
        return;
    }
    // SAFETY: the observer is the `SourceWatch` registered with this callback, freed only
    // after it is removed, on this same (main) thread.
    let watch = unsafe { &*observer.cast::<SourceWatch>() };
    if let Some(id) = nexa_input::platform::macos::current_input_source() {
        let _ = watch.tx.send(InputCmd::InputSource(id));
    }
}

impl InputSourceWatcher {
    fn start(tx: Sender<InputCmd>) -> Self {
        let watch = Box::into_raw(Box::new(SourceWatch { tx })) as usize;
        DispatchQueue::main().exec_async(move || {
            nexa_media::diag!("input source: following this Mac's");
            // SAFETY: registered on the main thread, so the callback runs there too; the
            // pointer stays valid until `drop` removes it, also on the main thread.
            unsafe {
                CFNotificationCenterAddObserver(
                    CFNotificationCenterGetDistributedCenter(),
                    watch as *const c_void,
                    on_input_source_changed,
                    nexa_input::platform::macos::input_source_changed_notification(),
                    std::ptr::null(),
                    DELIVER_IMMEDIATELY,
                );
            }
        });
        Self(watch)
    }
}

impl Drop for InputSourceWatcher {
    fn drop(&mut self) {
        let watch = self.0;
        DispatchQueue::main().exec_async(move || {
            // SAFETY: after the observer is removed nothing reaches the pointer, which is
            // the box `start` made.
            unsafe {
                CFNotificationCenterRemoveObserver(
                    CFNotificationCenterGetDistributedCenter(),
                    watch as *const c_void,
                    nexa_input::platform::macos::input_source_changed_notification(),
                    std::ptr::null(),
                );
                drop(Box::from_raw(watch as *mut SourceWatch));
            }
        });
    }
}

/// Starts the tap the first time capture is switched on.
struct TapSource {
    tx: Sender<InputCmd>,
    tap: Option<KeyCapture>,
}

impl KeySource for TapSource {
    fn set_active(&mut self, active: bool) -> Result<(), KeySourceError> {
        if self.tap.is_none() {
            if !active {
                return Ok(());
            }
            let (tap, events) =
                KeyCapture::start(KeyCaptureConfig::default()).map_err(|e| match e {
                    CaptureError::PermissionDenied => KeySourceError::PermissionDenied,
                    CaptureError::Setup => KeySourceError::Failed,
                })?;
            forward(events, self.tx.clone());
            self.tap = Some(tap);
        }
        if let Some(tap) = &self.tap {
            tap.set_active(active);
        }
        Ok(())
    }
}

/// `LocalKeyMonitor` is a main-thread AppKit object; everything `set_active` touches
/// is atomics and a mutex, and removal is sent back to the main queue.
struct MonitorSource(Option<SendMonitor>);

struct SendMonitor(LocalKeyMonitor);

// SAFETY: see `MonitorSource`.
unsafe impl Send for SendMonitor {}

impl KeySource for MonitorSource {
    fn set_active(&mut self, active: bool) -> Result<(), KeySourceError> {
        if let Some(m) = &self.0 {
            m.0.set_active(active);
        }
        Ok(())
    }
}

impl Drop for MonitorSource {
    fn drop(&mut self) {
        if let Some(monitor) = self.0.take() {
            DispatchQueue::main().exec_async(move || drop(monitor));
        }
    }
}

/// Game mode's lock: a listen-only pointer tap with the mouse dissociated from the
/// cursor. Dropping the capture re-associates first, then stops the tap.
struct GameLock(#[allow(dead_code)] PointerCapture<SystemAssociation>);

impl PointerLock for GameLock {}

struct Speaker(#[allow(dead_code)] AudioOutputUnit);

impl AudioOutput for Speaker {}

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
    fn current(&mut self) -> nexa_clip::Result<Option<LocalSnapshot>> {
        Ok(Some(self.0.snapshot()))
    }
    fn held_back(&mut self) -> nexa_clip::Result<Option<LocalSnapshot>> {
        Ok(self.0.held_back())
    }
}

fn qos_user_interactive() {
    // SAFETY: documented call on the current thread with a valid class.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
    }
}

impl Deps for RealDeps {
    fn os(&self) -> Os {
        Os::Macos
    }

    fn key_store(&self) -> Box<dyn KeyStore> {
        Box::new(KeychainStore::new("app.nexadesk", "viewer-device-key"))
    }

    fn caps(&self) -> Caps {
        // §3.5: shared with the agent, keyed by OS build and GPU.
        match nexa_media::caps_cache::default_path() {
            Some(path) => nexa_media::macos::cached_caps(&path),
            None => probe_caps(),
        }
    }

    fn decoder(&self, codec: Codec) -> Result<Box<dyn Decoder>, String> {
        VtDecoder::new(codec, DecoderOutput::Display)
            .map(|d| Box::new(d) as Box<dyn Decoder>)
            .map_err(|e| e.to_string())
    }

    fn key_source(&self, tx: Sender<InputCmd>) -> Box<dyn KeySource> {
        Box::new(TapSource { tx, tap: None })
    }

    fn input_source_watcher(&self, tx: Sender<InputCmd>) -> Option<Box<dyn Send>> {
        Some(Box::new(InputSourceWatcher::start(tx)))
    }

    fn fallback_key_source(&self, tx: Sender<InputCmd>) -> Option<Box<dyn KeySource>> {
        if accessibility_granted() {
            return None;
        }
        let mtm = MainThreadMarker::new()?;
        let (monitor, events) = LocalKeyMonitor::install(KeyCaptureConfig::default(), mtm)?;
        forward(events, tx);
        Some(Box::new(MonitorSource(Some(SendMonitor(monitor)))))
    }

    fn clipboard(&self) -> Result<Box<dyn ClipPlatform>, String> {
        // Content is fetched before it is written (§8.6), so no promise ever reaches this.
        let resolver: nexa_clip::platform::macos::PromiseResolver = Arc::new(|_, _| None);
        Ok(Box::new(GeneralClipboard(MacClipboardBackend::general(
            resolver,
        ))))
    }

    unsafe fn attach_view(
        &self,
        handle: *mut c_void,
        kind: u32,
        width_px: u32,
        height_px: u32,
        scale: f32,
        mode: PresentMode,
    ) -> Result<Box<dyn Presenter>, String> {
        if kind != NEXA_VIEW_NSVIEW {
            return Err(format!("unsupported view kind {kind}"));
        }
        // SAFETY: forwarded from the caller, including the main-thread check inside.
        unsafe { MetalPresenter::attach(handle, width_px, height_px, scale, mode) }
            .map(|p| Box::new(p) as Box<dyn Presenter>)
            .map_err(|e| e.to_string())
    }

    unsafe fn cursor_display(&self, handle: *mut c_void, kind: u32) -> Box<dyn CursorDisplay> {
        if kind != NEXA_VIEW_NSVIEW {
            return Box::new(NoCursor);
        }
        // SAFETY: forwarded from the caller (main thread, a valid NSView).
        match unsafe { MacCursorDisplay::attach(handle) } {
            Some(d) => Box::new(d),
            None => Box::new(NoCursor),
        }
    }

    fn pointer_lock(&self, tx: Sender<InputCmd>) -> Result<Box<dyn PointerLock>, String> {
        let (capture, motion) = PointerCapture::start(0).map_err(|e| format!("{e:?}"))?;
        thread::Builder::new()
            .name("pointer-bridge".into())
            .spawn(move || {
                // Ends when the capture is dropped and its tap stops.
                while let Ok(p) = motion.recv() {
                    if tx.send(InputCmd::Relative(p.x, p.y)).is_err() {
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
        AudioOutputUnit::start(render)
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
        // Without it the card simply has no LAN section.
        nexa_net::discovery::Browser::start().ok().map(Arc::new)
    }

    fn thread_init(&self) -> Option<fn()> {
        Some(qos_user_interactive)
    }
}
