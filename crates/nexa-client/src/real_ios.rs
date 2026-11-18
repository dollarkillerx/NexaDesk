//! The real devices on iOS. Reached only through `nexa_init`.
//!
//! The iOS app is a viewer only. Touches and the on-screen keyboard reach the core as
//! commands through the C ABI, so there is no key capture here; the video is decoded by
//! VideoToolbox and drawn with Metal into a `CAMetalLayer` the app owns. Not yet here:
//! sound (Opus and the audio unit), the clipboard, and watching for network changes.

use std::{ffi::c_void, sync::mpsc::Sender};

use nexa_auth::{KeyStore, KeychainStore};
use nexa_media::{
    Decoder, PresentMode, Presenter,
    macos::{DecoderOutput, MetalPresenter, VtDecoder, decode_caps},
};
use nexa_session::clip::ClipPlatform;
use nexa_wire::control::{Caps, Codec, Os};

use crate::{
    abi::NEXA_VIEW_CAMETALLAYER,
    deps::{Deps, KeySource, KeySourceError},
    input::InputCmd,
};

pub struct RealDeps;

/// Keys come from the app as commands (the on-screen keyboard, a hardware keyboard
/// through UIKit), so there is nothing to start or stop.
struct AppKeys;

impl KeySource for AppKeys {
    fn set_active(&mut self, _: bool) -> Result<(), KeySourceError> {
        Ok(())
    }
}

fn qos_user_interactive() {
    // SAFETY: documented call on the current thread with a valid class.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
    }
}

impl Deps for RealDeps {
    /// An iPhone or iPad keyboard has ⌘ and ⌥ where a Mac's are, and the keys
    /// are rewritten for the host as from a Mac.
    fn os(&self) -> Os {
        Os::Macos
    }

    fn key_store(&self) -> Box<dyn KeyStore> {
        Box::new(KeychainStore::new("app.nexadesk", "viewer-device-key"))
    }

    fn caps(&self) -> Caps {
        decode_caps()
    }

    fn decoder(&self, codec: Codec) -> Result<Box<dyn Decoder>, String> {
        VtDecoder::new(codec, DecoderOutput::Display)
            .map(|d| Box::new(d) as Box<dyn Decoder>)
            .map_err(|e| e.to_string())
    }

    fn key_source(&self, _: Sender<InputCmd>) -> Box<dyn KeySource> {
        Box::new(AppKeys)
    }

    fn fallback_key_source(&self, _: Sender<InputCmd>) -> Option<Box<dyn KeySource>> {
        None
    }

    fn clipboard(&self) -> Result<Box<dyn ClipPlatform>, String> {
        // iOS shows a prompt for every read of another app's pasteboard, so it can not be
        // synced in the background; a paste-driven sync comes later.
        Err("the clipboard is not shared on iOS yet".into())
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
        if kind != NEXA_VIEW_CAMETALLAYER {
            return Err(format!("unsupported view kind {kind}"));
        }
        // SAFETY: forwarded from the caller, including the main-thread check inside.
        unsafe { MetalPresenter::attach_layer(handle, width_px, height_px, scale, mode) }
            .map(|p| Box::new(p) as Box<dyn Presenter>)
            .map_err(|e| e.to_string())
    }

    fn thread_init(&self) -> Option<fn()> {
        Some(qos_user_interactive)
    }
}
