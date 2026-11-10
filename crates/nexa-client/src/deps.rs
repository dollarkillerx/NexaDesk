//! Everything that touches the machine, so tests can run the whole client with fakes.
//! The real implementations are in `real.rs`, reachable only from `nexa_init`.

use std::{ffi::c_void, sync::mpsc::Sender, time::Instant};

use nexa_auth::KeyStore;
use nexa_media::{AudioDecoder, Decoder, PresentMode, Presenter};
use nexa_session::clip::ClipPlatform;
use nexa_wire::control::{Caps, Codec, Os};

use crate::input::InputCmd;

/// Why local key capture could not start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySourceError {
    PermissionDenied,
    Failed,
}

/// Local keyboard capture. `set_active(true)` starts swallowing keys, so it must only
/// ever be called between `nexa_session_set_input_active` calls.
pub trait KeySource: Send {
    fn set_active(&mut self, active: bool) -> Result<(), KeySourceError>;
}

/// The host's cursor shape, sized for the local view (§6.9).
#[derive(Clone, Debug, PartialEq)]
pub struct LocalCursor {
    /// The host's shape id; the same id with the same size is the same cursor.
    pub id: u64,
    pub png: std::sync::Arc<Vec<u8>>,
    /// Size and hot spot in points of the local screen.
    pub width_pt: f64,
    pub height_pt: f64,
    pub hot_x_pt: f64,
    pub hot_y_pt: f64,
}

/// The local cursor over the video view. Implementations act on the main thread.
pub trait CursorDisplay: Send {
    /// Shows `cursor` while the pointer is over the view; `None` hides it there.
    fn show(&mut self, cursor: Option<&LocalCursor>);
    /// The ordinary cursor: game mode with the lock released, or nothing from the host yet.
    fn reset(&mut self);
    /// Moves the local cursor to a point in the view, in points from its top left
    /// (the host moved its cursor, §6.9).
    fn warp(&mut self, x_pt: f64, y_pt: f64);
}

/// Game mode's pointer lock (§7.4): the cursor stays put and motion arrives as
/// `InputCmd::Relative`. Dropping it gives the mouse back.
pub trait PointerLock: Send {}

/// A playing audio output. Dropping it stops playback.
pub trait AudioOutput: Send {}

/// Fills interleaved 48 kHz stereo; the `Instant` is when the first sample is heard.
pub type AudioRender = Box<dyn FnMut(&mut [f32], Instant) + Send>;

pub trait Deps: Send + Sync + 'static {
    fn os(&self) -> Os;
    fn key_store(&self) -> Box<dyn KeyStore>;
    /// May take seconds; called off the caller's thread, once.
    fn caps(&self) -> Caps;
    fn decoder(&self, codec: Codec) -> Result<Box<dyn Decoder>, String>;
    /// Events are pushed into `tx`. Starting real capture is deferred to `set_active`.
    fn key_source(&self, tx: Sender<InputCmd>) -> Box<dyn KeySource>;
    /// A capture that works without Accessibility. Called on the main thread.
    fn fallback_key_source(&self, tx: Sender<InputCmd>) -> Option<Box<dyn KeySource>>;
    fn clipboard(&self) -> Result<Box<dyn ClipPlatform>, String>;
    /// Called on the UI thread with the handle from `nexa_session_attach_view`.
    ///
    /// # Safety
    /// `handle` must be valid for this kind of view and outlive the presenter.
    unsafe fn attach_view(
        &self,
        handle: *mut c_void,
        kind: u32,
        width_px: u32,
        height_px: u32,
        scale: f32,
        mode: PresentMode,
    ) -> Result<Box<dyn Presenter>, String>;
    /// Thread setup for media threads (§1.2).
    fn thread_init(&self) -> Option<fn()> {
        None
    }
    /// The local cursor for a view just attached. Called on the UI thread with the
    /// handle from `nexa_session_attach_view`; the default draws nothing.
    ///
    /// # Safety
    /// As for [`Deps::attach_view`].
    unsafe fn cursor_display(&self, handle: *mut c_void, kind: u32) -> Box<dyn CursorDisplay> {
        let _ = (handle, kind);
        Box::new(NoCursor)
    }
    /// Locks the pointer for game mode. Must never leave the mouse dissociated.
    fn pointer_lock(&self, tx: Sender<InputCmd>) -> Result<Box<dyn PointerLock>, String> {
        let _ = tx;
        Err("pointer lock is not available".into())
    }
    fn audio_decoder(&self) -> Result<Box<dyn AudioDecoder>, String> {
        Err("no audio decoder".into())
    }
    /// Starts playback pulling from `render`.
    fn audio_output(&self, render: AudioRender) -> Result<Box<dyn AudioOutput>, String> {
        let _ = render;
        Err("no audio output".into())
    }
    /// Sockets for connections by device ID (§4.1). Tests keep them on loopback.
    /// The default does no port mapping and watches nothing; the real
    /// dependencies turn those on.
    fn link_options(&self) -> nexa_session::rendezvous::link::LinkOptions {
        nexa_session::rendezvous::link::LinkOptions::default()
    }
    /// Tailscale's LocalAPI status (§4.7): DERP marking for ip:port sessions
    /// and the connect card's device list. `None`: address-only.
    fn tailnet(&self) -> Option<std::sync::Arc<nexa_net::tailscale::TailnetMonitor>> {
        None
    }
}

/// A [`CursorDisplay`] that does nothing: headless use and tests.
pub struct NoCursor;

impl CursorDisplay for NoCursor {
    fn show(&mut self, _: Option<&LocalCursor>) {}
    fn reset(&mut self) {}
    fn warp(&mut self, _: f64, _: f64) {}
}
