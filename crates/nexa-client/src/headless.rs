//! The devices of a viewer without a window (`nexa-mcp`, docs/MCP.md §3): the real
//! network, codecs and decoder, and nothing that touches this machine's keyboard,
//! pasteboard, cursor or speakers.
//!
//! - **No local capture.** Input comes only from `nexa_session_command`-style
//!   commands; the key source does nothing, so activating input never swallows keys
//!   here.
//! - **No local pasteboard.** The clipboard is held in memory
//!   ([`crate::memclip::MemoryClipboard`]): nothing copied on this machine is sent to
//!   the host, and nothing from the host lands in this machine's clipboard.
//! - **Its own identity.** The device key is a file in the caller's data directory,
//!   not the app's Keychain item, so hosts see a different device and none of the
//!   app's pairings carry over.
//! - **Keys as named.** No keyboard rules between this OS and the host's: the caller
//!   names the host's own keys (Ctrl+C for a Windows host, ⌘C for a Mac).
//! - **Readable frames.** The decoder hands out `420v` buffers the CPU can read, for
//!   screenshots ([`crate::snapshot`]).

use std::{
    ffi::c_void,
    path::PathBuf,
    sync::{Arc, mpsc::Sender},
};

use nexa_auth::{FileKeyStore, KeyStore};
use nexa_media::{Decoder, PresentMode, Presenter};
use nexa_session::clip::ClipPlatform;
use nexa_wire::control::{Caps, Chroma, Codec, Os};

use crate::{
    deps::{Deps, KeySource, KeySourceError},
    input::InputCmd,
    memclip::MemoryClipboard,
};

pub struct HeadlessDeps {
    key_file: PathBuf,
    clipboard: Arc<MemoryClipboard>,
}

impl HeadlessDeps {
    /// `key_file`: where this viewer's device key lives (a 0600 file, made on first
    /// use). `clipboard`: what sessions with the clipboard permission share.
    pub fn new(key_file: PathBuf, clipboard: Arc<MemoryClipboard>) -> Self {
        Self {
            key_file,
            clipboard,
        }
    }
}

struct NoKeys;

impl KeySource for NoKeys {
    fn set_active(&mut self, _: bool) -> Result<(), KeySourceError> {
        Ok(())
    }
}

impl Deps for HeadlessDeps {
    fn os(&self) -> Os {
        platform::OS
    }

    fn key_store(&self) -> Box<dyn KeyStore> {
        Box::new(FileKeyStore::new(self.key_file.clone()))
    }

    fn caps(&self) -> Caps {
        let mut caps = platform::caps();
        // Screenshots are read as 4:2:0; a 4:4:4 stream would only be converted down.
        caps.decode.retain(|c| c.chroma != Chroma::Yuv444 as i32);
        caps
    }

    fn decoder(&self, codec: Codec) -> Result<Box<dyn Decoder>, String> {
        platform::decoder(codec)
    }

    fn key_source(&self, _: Sender<InputCmd>) -> Box<dyn KeySource> {
        Box::new(NoKeys)
    }

    fn rewrites_keys(&self) -> bool {
        false
    }

    /// Only `nexa-mcp` runs without a window: it is an AI agent.
    fn is_agent(&self) -> bool {
        true
    }

    fn fallback_key_source(&self, _: Sender<InputCmd>) -> Option<Box<dyn KeySource>> {
        None
    }

    fn clipboard(&self) -> Result<Box<dyn ClipPlatform>, String> {
        Ok(self.clipboard.platform())
    }

    unsafe fn attach_view(
        &self,
        _: *mut c_void,
        _: u32,
        _: u32,
        _: u32,
        _: f32,
        _: PresentMode,
    ) -> Result<Box<dyn Presenter>, String> {
        Err("a viewer without a window has no views; use Session::attach_presenter".into())
    }

    fn link_options(&self) -> nexa_session::rendezvous::link::LinkOptions {
        nexa_session::rendezvous::link::LinkOptions::system()
    }

    fn tailnet(&self) -> Option<std::sync::Arc<nexa_net::tailscale::TailnetMonitor>> {
        Some(nexa_session::rendezvous::link::system_tailnet())
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use nexa_media::{
        Decoder,
        macos::{DecoderOutput, VtDecoder, probe_caps},
    };
    use nexa_wire::control::{Caps, Codec, Os};

    pub const OS: Os = Os::Macos;

    pub fn caps() -> Caps {
        match nexa_media::caps_cache::default_path() {
            Some(path) => nexa_media::macos::cached_caps(&path),
            None => probe_caps(),
        }
    }

    /// `420v` buffers the CPU can read.
    pub fn decoder(codec: Codec) -> Result<Box<dyn Decoder>, String> {
        VtDecoder::new(codec, DecoderOutput::CpuReadable420v)
            .map(|d| Box::new(d) as Box<dyn Decoder>)
            .map_err(|e| e.to_string())
    }
}

#[cfg(windows)]
mod platform {
    use nexa_media::{Decoder, windows::MfDecoder};
    use nexa_wire::control::{Caps, Codec, Os};

    pub const OS: Os = Os::Windows;

    pub fn caps() -> Caps {
        nexa_media::windows::viewer_caps()
    }

    /// Decoded on the CPU, so a picture is in memory to read (`NativeImage::Cpu`).
    pub fn decoder(codec: Codec) -> Result<Box<dyn Decoder>, String> {
        MfDecoder::new_cpu(codec)
            .map(|d| Box::new(d) as Box<dyn Decoder>)
            .map_err(|e| e.to_string())
    }
}
