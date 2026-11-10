//! Everything that touches the machine, so tests can run the whole client with fakes.
//! The real implementations are in `real.rs`, reachable only from `nexa_init`.

use std::{ffi::c_void, sync::mpsc::Sender};

use nexa_auth::KeyStore;
use nexa_media::{Decoder, PresentMode, Presenter};
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
    /// Sockets for connections by device ID (§4.1). Tests keep them on loopback.
    fn link_options(&self) -> nexa_session::rendezvous::link::LinkOptions {
        nexa_session::rendezvous::link::LinkOptions::default()
    }
}
