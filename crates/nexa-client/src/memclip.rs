//! A clipboard held in memory, for a viewer without a window (`nexa-mcp`, docs/MCP.md
//! §3.1): what the host copies lands here, and what is put here goes to the host,
//! without this machine's own clipboard ever being read or written.
//!
//! One clipboard for the whole viewer, as a real one is: with several sessions, a copy
//! on one host reaches the others too. Each session reads it through its own
//! [`ClipPlatform`], which notices only changes made after the session began.

use std::sync::{Arc, Mutex, MutexGuard};

use bytes::Bytes;
use nexa_clip::{ClipKind, ClipboardBackend, FakeBackend, FormatInfo, Gen, LocalSnapshot};
use nexa_session::clip::ClipPlatform;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The clipboard's content: the in-memory backend `nexa_clip` keeps for this.
#[derive(Default)]
pub struct MemoryClipboard {
    backend: Mutex<FakeBackend>,
}

impl MemoryClipboard {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Copies `text`, as a person copying it would: sessions with the clipboard
    /// permission send it to their hosts.
    pub fn set_text(&self, text: &str) {
        lock(&self.backend).user_copy(&[(ClipKind::Text, text.as_bytes())]);
    }

    /// The text on the clipboard: the last copy from a host, or what was set here.
    pub fn text(&self) -> Option<String> {
        lock(&self.backend)
            .content(ClipKind::Text)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    /// How many times the clipboard changed, here or from a host.
    pub fn changes(&self) -> u64 {
        lock(&self.backend).change_count()
    }

    /// The clipboard as one session sees it.
    pub fn platform(self: &Arc<Self>) -> Box<dyn ClipPlatform> {
        Box::new(SessionView {
            last_seen: self.changes(),
            clip: self.clone(),
        })
    }
}

struct SessionView {
    clip: Arc<MemoryClipboard>,
    last_seen: u64,
}

impl ClipboardBackend for SessionView {
    fn read_formats(&mut self) -> nexa_clip::Result<Vec<FormatInfo>> {
        lock(&self.clip.backend).read_formats()
    }

    fn read(&mut self, kind: ClipKind) -> nexa_clip::Result<Vec<u8>> {
        lock(&self.clip.backend).read(kind)
    }

    fn write_promise(
        &mut self,
        generation: Gen,
        formats: &[FormatInfo],
        inline: &[(ClipKind, Bytes)],
    ) -> nexa_clip::Result<()> {
        lock(&self.clip.backend).write_promise(generation, formats, inline)
    }
}

impl ClipPlatform for SessionView {
    fn poll(&mut self) -> nexa_clip::Result<Option<LocalSnapshot>> {
        let backend = lock(&self.clip.backend);
        if backend.change_count() == self.last_seen {
            return Ok(None);
        }
        self.last_seen = backend.change_count();
        // A copy the host made carries the owner marker, so it is not sent back.
        Ok(Some(backend.snapshot()))
    }

    fn current(&mut self) -> nexa_clip::Result<Option<LocalSnapshot>> {
        Ok(Some(lock(&self.clip.backend).snapshot()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_notices_only_later_copies_and_reads_what_a_host_wrote() {
        let clip = MemoryClipboard::new();
        clip.set_text("before");
        let mut view = clip.platform();
        assert!(view.poll().unwrap().is_none(), "copied before the session");
        clip.set_text("hello");
        let snap = view.poll().unwrap().expect("a change");
        assert!(!snap.owner_marker);
        assert!(view.poll().unwrap().is_none());
        // The host's copy arrives through the session.
        let text = Bytes::from_static("从主机复制".as_bytes());
        let info = FormatInfo::of(ClipKind::Text, &text);
        view.write_promise(
            (7, nexa_clip::DeviceId([1; 16])),
            &[info],
            &[(ClipKind::Text, text)],
        )
        .unwrap();
        assert_eq!(clip.text().as_deref(), Some("从主机复制"));
        let snap = view.poll().unwrap().expect("the write is a change");
        assert!(
            snap.owner_marker,
            "and marked as the host's, so not sent back"
        );
    }
}
