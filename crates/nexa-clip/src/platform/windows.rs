//! The Windows clipboard (docs/WINDOWS.md): text, as CF_UNICODETEXT read and written as
//! canonical UTF-8 with LF newlines, and pictures as PNG ([`super::windows_image`]).
//! Changes are noticed by the clipboard's sequence number; our own writes carry a
//! private marker format.
//!
//! Formats offered only as promises (`write_promise` without inline content) are not
//! put on the clipboard yet: Windows' delayed rendering needs a window of its own.

use std::thread;
use std::time::Duration;

use bytes::Bytes;
use windows::{
    Win32::{
        Foundation::{HANDLE, HGLOBAL},
        System::{
            DataExchange::{
                CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
                IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
                SetClipboardData,
            },
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock},
        },
    },
    core::w,
};

use super::windows_image::{decode, dibv5, is_png, png_of_dib};
use crate::{ClipError, ClipKind, ClipboardBackend, FormatInfo, Gen, LocalSnapshot, Result};

/// CF_UNICODETEXT.
const CF_UNICODETEXT: u32 = 13;
/// CF_DIB and CF_DIBV5: a bitmap's header and pixels.
const CF_DIB: u32 = 8;
const CF_DIBV5: u32 = 17;
/// A clipboard opened by someone else: tries this many times, a little apart.
const OPEN_TRIES: u32 = 10;

/// Windows' clipboard, held open only for the length of one call.
pub struct WindowsClipboard {
    owner_format: u32,
    /// "PNG", as browsers and Office register it.
    png_format: u32,
    /// Set by password managers and the like: such content is never sent.
    exclude_format: u32,
    last_sequence: u32,
}

/// The clipboard, open; closed when dropped.
struct Open;

impl Open {
    fn new() -> Result<Self> {
        for _ in 0..OPEN_TRIES {
            // SAFETY: no owner window: the clipboard is opened for this thread.
            if unsafe { OpenClipboard(None) }.is_ok() {
                return Ok(Self);
            }
            thread::sleep(Duration::from_millis(10));
        }
        Err(ClipError::Busy)
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        // SAFETY: opened by `Open::new` on this thread.
        let _ = unsafe { CloseClipboard() };
    }
}

fn available(format: u32) -> bool {
    // SAFETY: a plain query.
    unsafe { IsClipboardFormatAvailable(format) }.is_ok()
}

/// The clipboard's text as canonical UTF-8 (LF newlines), with the clipboard open.
fn read_text(_open: &Open) -> Option<Vec<u8>> {
    // SAFETY: the clipboard is open; the handle is locked, read within its size, unlocked.
    unsafe {
        let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
        let global = HGLOBAL(handle.0);
        let size = GlobalSize(global) / 2;
        let ptr = GlobalLock(global) as *const u16;
        if ptr.is_null() {
            return None;
        }
        let units = std::slice::from_raw_parts(ptr, size);
        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        let text = String::from_utf16_lossy(&units[..end]);
        let _ = GlobalUnlock(global);
        Some(text.replace("\r\n", "\n").into_bytes())
    }
}

/// The bytes of one format, with the clipboard open.
fn read_global(_open: &Open, format: u32) -> Option<Vec<u8>> {
    // SAFETY: the clipboard is open; the handle is locked, read within its size, unlocked.
    unsafe {
        let handle = GetClipboardData(format).ok()?;
        let global = HGLOBAL(handle.0);
        let size = GlobalSize(global);
        let ptr = GlobalLock(global) as *const u8;
        if ptr.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(ptr, size).to_vec();
        let _ = GlobalUnlock(global);
        Some(bytes)
    }
}

/// The clipboard's picture as it is there, copied with the clipboard open.
enum Picture {
    Png(Vec<u8>),
    /// A bitmap (CF_DIBV5 or CF_DIB), made into a PNG once the clipboard is closed.
    Dib(Vec<u8>),
}

impl Picture {
    fn read(open: &Open, png_format: u32) -> Option<Self> {
        if png_format != 0
            && available(png_format)
            && let Some(png) = read_global(open, png_format).filter(|d| is_png(d))
        {
            return Some(Self::Png(png));
        }
        [CF_DIBV5, CF_DIB]
            .into_iter()
            .filter(|&f| available(f))
            .find_map(|f| read_global(open, f))
            .map(Self::Dib)
    }

    /// The PNG. Converting a large bitmap takes a while: never with the clipboard open,
    /// which every other program on this PC would wait for.
    fn into_png(self) -> Option<Vec<u8>> {
        match self {
            Self::Png(png) => Some(png),
            Self::Dib(dib) => png_of_dib(&dib),
        }
    }
}

fn has_picture(png_format: u32) -> bool {
    (png_format != 0 && available(png_format)) || available(CF_DIBV5) || available(CF_DIB)
}

/// A global memory block holding `data`, for SetClipboardData.
fn global_of(data: &[u8]) -> Result<HGLOBAL> {
    // SAFETY: a fresh moveable block of `data.len()` bytes, locked for the copy only.
    unsafe {
        let global = GlobalAlloc(GMEM_MOVEABLE, data.len().max(1)).map_err(|_| ClipError::Busy)?;
        let ptr = GlobalLock(global) as *mut u8;
        if ptr.is_null() {
            return Err(ClipError::Busy);
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        let _ = GlobalUnlock(global);
        Ok(global)
    }
}

impl WindowsClipboard {
    pub fn new() -> Self {
        // SAFETY: registering named formats; the names are static.
        let (owner_format, exclude_format, png_format) = unsafe {
            (
                RegisterClipboardFormatW(w!("NexaDesk.Owner")),
                RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing")),
                RegisterClipboardFormatW(w!("PNG")),
            )
        };
        Self {
            owner_format,
            png_format,
            exclude_format,
            // SAFETY: a plain query.
            last_sequence: unsafe { GetClipboardSequenceNumber() },
        }
    }

    /// The clipboard as the sync engine describes it.
    pub fn snapshot(&mut self) -> Result<LocalSnapshot> {
        let owner_marker = available(self.owner_format);
        let sensitive = self.exclude_format != 0 && available(self.exclude_format);
        let mut formats = Vec::new();
        let mut data = Vec::new();
        let text = available(CF_UNICODETEXT);
        let picture = has_picture(self.png_format);
        if !sensitive && (text || picture) {
            let open = Open::new()?;
            let text = text.then(|| read_text(&open)).flatten();
            let picture = picture
                .then(|| Picture::read(&open, self.png_format))
                .flatten();
            drop(open);
            if let Some(text) = text {
                formats.push(FormatInfo::of(ClipKind::Text, &text));
                data.push((ClipKind::Text, Bytes::from(text)));
            }
            if let Some(png) = picture.and_then(Picture::into_png) {
                formats.push(FormatInfo::of(ClipKind::Png, &png));
                data.push((ClipKind::Png, Bytes::from(png)));
            }
        }
        Ok(LocalSnapshot {
            formats,
            data,
            owner_marker,
            sensitive,
        })
    }

    /// A new snapshot if the clipboard changed since the last call.
    pub fn poll(&mut self) -> Result<Option<LocalSnapshot>> {
        // SAFETY: a plain query.
        let sequence = unsafe { GetClipboardSequenceNumber() };
        if sequence == self.last_sequence {
            return Ok(None);
        }
        self.last_sequence = sequence;
        self.snapshot().map(Some)
    }
}

impl Default for WindowsClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardBackend for WindowsClipboard {
    fn read_formats(&mut self) -> Result<Vec<FormatInfo>> {
        Ok(self.snapshot()?.formats)
    }

    fn read(&mut self, kind: ClipKind) -> Result<Vec<u8>> {
        match kind {
            ClipKind::Text if available(CF_UNICODETEXT) => {
                let open = Open::new()?;
                read_text(&open).ok_or(ClipError::NotAvailable(kind))
            }
            ClipKind::Png if has_picture(self.png_format) => {
                let open = Open::new()?;
                let picture = Picture::read(&open, self.png_format);
                drop(open);
                picture
                    .and_then(Picture::into_png)
                    .ok_or(ClipError::NotAvailable(kind))
            }
            _ => Err(ClipError::NotAvailable(kind)),
        }
    }

    fn write_promise(
        &mut self,
        generation: Gen,
        _formats: &[FormatInfo],
        inline: &[(ClipKind, Bytes)],
    ) -> Result<()> {
        // A picture is taken apart before the clipboard is opened: nobody waits on it.
        let bitmap = inline
            .iter()
            .find(|(kind, _)| *kind == ClipKind::Png)
            .and_then(|(_, png)| decode(png))
            .map(|(w, h, bgra)| dibv5(w, h, &bgra));
        let open = Open::new()?;
        // SAFETY: the clipboard is open; each block handed over belongs to it after.
        unsafe {
            EmptyClipboard().map_err(|_| ClipError::Busy)?;
            for (kind, content) in inline {
                match kind {
                    ClipKind::Text => {
                        let text = String::from_utf8_lossy(content).replace('\n', "\r\n");
                        let mut units: Vec<u16> = text.encode_utf16().collect();
                        units.push(0);
                        let bytes = std::slice::from_raw_parts(
                            units.as_ptr().cast::<u8>(),
                            units.len() * 2,
                        );
                        let global = global_of(bytes)?;
                        SetClipboardData(CF_UNICODETEXT, Some(HANDLE(global.0)))
                            .map_err(|_| ClipError::Busy)?;
                    }
                    ClipKind::Png if self.png_format != 0 => {
                        let global = global_of(content)?;
                        SetClipboardData(self.png_format, Some(HANDLE(global.0)))
                            .map_err(|_| ClipError::Busy)?;
                    }
                    _ => {}
                }
            }
            // Windows makes CF_DIB and CF_BITMAP from this for anyone who asks.
            if let Some(dib) = &bitmap {
                let global = global_of(dib)?;
                SetClipboardData(CF_DIBV5, Some(HANDLE(global.0))).map_err(|_| ClipError::Busy)?;
            }
            let marker = global_of(&generation.0.to_le_bytes())?;
            let _ = SetClipboardData(self.owner_format, Some(HANDLE(marker.0)));
        }
        drop(open);
        // Our own write is not a change to report.
        // SAFETY: a plain query.
        self.last_sequence = unsafe { GetClipboardSequenceNumber() };
        Ok(())
    }
}
