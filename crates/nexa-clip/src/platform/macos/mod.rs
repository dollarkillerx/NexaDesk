//! The macOS clipboard, via `NSPasteboard` (§8.4).
//!
//! Change detection is polling: AppKit offers no notification. Promises use
//! `NSPasteboardItem` with a data provider; the provider blocks in the supplied
//! [`PromiseResolver`] until the peer's data arrives (§8.2).
//!
//! Formats are mapped per the §8.4 table. PNG is also offered as `public.tiff`,
//! since many Mac apps only paste TIFF, and a TIFF-only clipboard is read as
//! PNG, the canonical image format.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSBitmapImageRepPropertyKey, NSPasteboard,
    NSPasteboardItem, NSPasteboardItemDataProvider, NSPasteboardType, NSPasteboardWriting,
};
use objc2_foundation::{NSArray, NSData, NSDictionary, NSString};

use crate::backend::{ClipError, ClipboardBackend, Result};
use crate::format::canonical_text;
use crate::sync::LocalSnapshot;
use crate::types::{ClipKind, DeviceId, FormatInfo, Gen};

/// How often to call [`MacClipboardBackend::poll`] (§8.4).
pub const POLL_INTERVAL: Duration = Duration::from_millis(100);

const TEXT_TYPE: &str = "public.utf8-plain-text";
const HTML_TYPE: &str = "public.html";
const RTF_TYPE: &str = "public.rtf";
const PNG_TYPE: &str = "public.png";
const TIFF_TYPE: &str = "public.tiff";
const OWNER_TYPE: &str = "app.nexadesk.owner";
const CONCEALED_TYPE: &str = "org.nspasteboard.ConcealedType";

const ALL_KINDS: [ClipKind; 4] = [ClipKind::Text, ClipKind::Html, ClipKind::Rtf, ClipKind::Png];

fn ns(name: &str) -> Retained<NSString> {
    NSString::from_str(name)
}

/// Pasteboard types offered for a format, preferred first.
fn types_of(kind: ClipKind) -> &'static [&'static str] {
    match kind {
        ClipKind::Text => &[TEXT_TYPE],
        ClipKind::Html => &[HTML_TYPE],
        ClipKind::Rtf => &[RTF_TYPE],
        ClipKind::Png => &[PNG_TYPE, TIFF_TYPE],
    }
}

fn kind_of_type(name: &str) -> Option<ClipKind> {
    match name {
        TEXT_TYPE => Some(ClipKind::Text),
        HTML_TYPE => Some(ClipKind::Html),
        RTF_TYPE => Some(ClipKind::Rtf),
        PNG_TYPE | TIFF_TYPE => Some(ClipKind::Png),
        _ => None,
    }
}

fn owner_value(generation: Gen) -> String {
    let (lamport, device) = generation;
    let hex: String = device.0.iter().map(|b| format!("{b:02x}")).collect();
    format!("{lamport}:{hex}")
}

fn parse_owner(value: &str) -> Option<Gen> {
    let (lamport, hex) = value.split_once(':')?;
    if hex.len() != 32 {
        return None;
    }
    let mut device = [0u8; 16];
    for (i, byte) in device.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some((lamport.parse().ok()?, DeviceId(device)))
}

fn png_to_tiff(png: &[u8]) -> Option<Vec<u8>> {
    let rep = NSBitmapImageRep::imageRepWithData(&NSData::with_bytes(png))?;
    Some(rep.TIFFRepresentation()?.to_vec())
}

fn tiff_to_png(tiff: &[u8]) -> Option<Vec<u8>> {
    let rep = NSBitmapImageRep::imageRepWithData(&NSData::with_bytes(tiff))?;
    let properties = NSDictionary::<NSBitmapImageRepPropertyKey, objc2::runtime::AnyObject>::new();
    // SAFETY: the empty dictionary has the key and value types the method wants.
    let png =
        unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &properties) };
    Some(png?.to_vec())
}

/// Answers a promise: fetch the canonical content for this generation, blocking
/// the caller up to the design's 10 s timeout (§8.2). `None` gives up.
///
/// Runs on whatever thread AppKit delivers the request on, usually the main
/// thread. It must not depend on the thread that owns the backend being free
/// to do anything but run the [`ClipSync`](crate::ClipSync) event loop.
pub type PromiseResolver = Arc<dyn Fn(Gen, ClipKind) -> Option<Vec<u8>> + Send + Sync>;

struct ProviderIvars {
    generation: Gen,
    resolver: PromiseResolver,
    /// So PNG and TIFF requests share one fetch.
    png: Mutex<Option<Vec<u8>>>,
    /// Kinds rendered so far, so [`MacClipboardBackend::read`] can avoid
    /// re-entering the resolver for a format that is still a promise.
    rendered: Mutex<BTreeSet<ClipKind>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "NexaPromiseProvider"]
    #[ivars = ProviderIvars]
    struct PromiseProvider;

    unsafe impl NSObjectProtocol for PromiseProvider {}

    unsafe impl NSPasteboardItemDataProvider for PromiseProvider {
        #[unsafe(method(pasteboard:item:provideDataForType:))]
        fn provide(
            &self,
            _pasteboard: Option<&NSPasteboard>,
            item: &NSPasteboardItem,
            pasteboard_type: &NSPasteboardType,
        ) {
            self.fulfil(item, pasteboard_type);
        }
    }
);

impl PromiseProvider {
    fn new(generation: Gen, resolver: PromiseResolver) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ProviderIvars {
            generation,
            resolver,
            png: Mutex::new(None),
            rendered: Mutex::new(BTreeSet::new()),
        });
        // SAFETY: NSObject's init takes no arguments and the ivars are set.
        unsafe { msg_send![super(this), init] }
    }

    fn fetch(&self, kind: ClipKind) -> Option<Vec<u8>> {
        let ivars = self.ivars();
        if kind == ClipKind::Png
            && let Ok(cache) = ivars.png.lock()
            && let Some(png) = cache.as_ref()
        {
            return Some(png.clone());
        }
        let bytes = (ivars.resolver)(ivars.generation, kind)?;
        if kind == ClipKind::Png
            && let Ok(mut cache) = ivars.png.lock()
        {
            *cache = Some(bytes.clone());
        }
        Some(bytes)
    }

    fn fulfil(&self, item: &NSPasteboardItem, pasteboard_type: &NSPasteboardType) {
        let name = pasteboard_type.to_string();
        let Some(kind) = kind_of_type(&name) else {
            return;
        };
        let bytes = self.fetch(kind).and_then(|canonical| {
            if name == TIFF_TYPE {
                png_to_tiff(&canonical)
            } else {
                Some(canonical)
            }
        });
        // Empty data is the design's "write empty" on failure.
        let data = NSData::with_bytes(bytes.as_deref().unwrap_or_default());
        item.setData_forType(&data, pasteboard_type);
        if let Ok(mut rendered) = self.ivars().rendered.lock() {
            rendered.insert(kind);
        }
    }

    fn is_rendered(&self, kind: ClipKind) -> bool {
        self.ivars()
            .rendered
            .lock()
            .is_ok_and(|rendered| rendered.contains(&kind))
    }
}

/// What this backend last wrote.
struct Written {
    generation: Gen,
    formats: Vec<FormatInfo>,
    promised: Vec<ClipKind>,
    /// Kept alive for as long as the pasteboard may ask it for data.
    provider: Option<Retained<PromiseProvider>>,
}

/// [`ClipboardBackend`] over an `NSPasteboard`.
pub struct MacClipboardBackend {
    board: Retained<NSPasteboard>,
    resolver: PromiseResolver,
    change_count: isize,
    written: Option<Written>,
}

// SAFETY: every method takes `&mut self`, so the pasteboard handle is used by
// one thread at a time; NSPasteboard is not confined to the main thread. The
// provider's ivars are Send + Sync.
unsafe impl Send for MacClipboardBackend {}

impl MacClipboardBackend {
    /// Uses `board`; the caller decides which pasteboard this is.
    pub fn new(board: Retained<NSPasteboard>, resolver: PromiseResolver) -> Self {
        let change_count = board.changeCount();
        Self {
            board,
            resolver,
            change_count,
            written: None,
        }
    }

    /// The system clipboard.
    #[cfg(not(test))]
    pub fn general(resolver: PromiseResolver) -> Self {
        Self::new(NSPasteboard::generalPasteboard(), resolver)
    }

    /// Checks `changeCount` and describes the clipboard if it moved. Call this
    /// every [`POLL_INTERVAL`] and feed the result to
    /// [`ClipEvent::LocalChanged`](crate::ClipEvent::LocalChanged).
    pub fn poll(&mut self) -> Result<Option<LocalSnapshot>> {
        let count = self.board.changeCount();
        if count == self.change_count {
            return Ok(None);
        }
        self.change_count = count;
        Ok(Some(self.snapshot()))
    }

    /// Describes the clipboard as it is now, without touching our own promises.
    pub fn snapshot(&self) -> LocalSnapshot {
        let sensitive = self.has_type(CONCEALED_TYPE);
        let Some(owner) = self.owner_on_board() else {
            return self.third_party_snapshot(sensitive);
        };
        let formats = self
            .written
            .as_ref()
            .filter(|written| written.generation == owner)
            .map(|written| written.formats.clone())
            .unwrap_or_default();
        LocalSnapshot {
            formats,
            data: Vec::new(),
            owner_marker: true,
            sensitive,
        }
    }

    fn third_party_snapshot(&self, sensitive: bool) -> LocalSnapshot {
        let mut formats = Vec::new();
        let mut data = Vec::new();
        for kind in ALL_KINDS {
            if let Some(bytes) = self.read_canonical(kind) {
                formats.push(FormatInfo::of(kind, &bytes));
                data.push((kind, Bytes::from(bytes)));
            }
        }
        LocalSnapshot {
            formats,
            data,
            owner_marker: false,
            sensitive,
        }
    }

    /// The generation in the owner marker, if the clipboard has one.
    pub fn owner_on_board(&self) -> Option<Gen> {
        let value = self.board.stringForType(&ns(OWNER_TYPE))?;
        parse_owner(&value.to_string())
    }

    fn has_type(&self, name: &str) -> bool {
        let types = NSArray::from_retained_slice(&[ns(name)]);
        self.board.availableTypeFromArray(&types).is_some()
    }

    fn read_canonical(&self, kind: ClipKind) -> Option<Vec<u8>> {
        match kind {
            ClipKind::Text => {
                let text = self.board.stringForType(&ns(TEXT_TYPE))?.to_string();
                Some(canonical_text(&text).into_owned().into_bytes())
            }
            ClipKind::Html => Some(self.board.dataForType(&ns(HTML_TYPE))?.to_vec()),
            ClipKind::Rtf => Some(self.board.dataForType(&ns(RTF_TYPE))?.to_vec()),
            ClipKind::Png => {
                if let Some(png) = self.board.dataForType(&ns(PNG_TYPE)) {
                    Some(png.to_vec())
                } else {
                    tiff_to_png(&self.board.dataForType(&ns(TIFF_TYPE))?.to_vec())
                }
            }
        }
        .filter(|bytes| !bytes.is_empty())
    }

    /// The written entry, if the clipboard still holds it and `kind` is an
    /// unrendered promise on it.
    fn is_open_promise(&self, kind: ClipKind) -> bool {
        let Some(written) = &self.written else {
            return false;
        };
        self.owner_on_board() == Some(written.generation)
            && written.promised.contains(&kind)
            && !written
                .provider
                .as_ref()
                .is_some_and(|provider| provider.is_rendered(kind))
    }

    fn set_inline(&self, item: &NSPasteboardItem, kind: ClipKind, bytes: &[u8]) -> bool {
        let data = NSData::with_bytes(bytes);
        let mut ok = true;
        for &name in types_of(kind) {
            let converted;
            let payload = if name == TIFF_TYPE {
                let Some(tiff) = png_to_tiff(bytes) else {
                    continue; // the PNG alone is still usable
                };
                converted = NSData::with_bytes(&tiff);
                &converted
            } else {
                &data
            };
            ok &= item.setData_forType(payload, &ns(name));
        }
        ok
    }
}

impl ClipboardBackend for MacClipboardBackend {
    fn read_formats(&mut self) -> Result<Vec<FormatInfo>> {
        Ok(self.snapshot().formats)
    }

    fn read(&mut self, kind: ClipKind) -> Result<Vec<u8>> {
        if self.is_open_promise(kind) {
            return Err(ClipError::Deferred(kind));
        }
        self.read_canonical(kind)
            .ok_or(ClipError::NotAvailable(kind))
    }

    fn write_promise(
        &mut self,
        generation: Gen,
        formats: &[FormatInfo],
        inline: &[(ClipKind, Bytes)],
    ) -> Result<()> {
        let item = NSPasteboardItem::new();
        if !item.setString_forType(&ns(&owner_value(generation)), &ns(OWNER_TYPE)) {
            return Err(ClipError::Busy);
        }

        let mut promised = Vec::new();
        let mut promised_types: Vec<Retained<NSString>> = Vec::new();
        for info in formats {
            match inline.iter().find(|(kind, _)| *kind == info.kind) {
                Some((kind, bytes)) => {
                    if !self.set_inline(&item, *kind, bytes) {
                        return Err(ClipError::Busy);
                    }
                }
                None => {
                    promised.push(info.kind);
                    promised_types.extend(types_of(info.kind).iter().map(|name| ns(name)));
                }
            }
        }

        let provider = if promised.is_empty() {
            None
        } else {
            let provider = PromiseProvider::new(generation, Arc::clone(&self.resolver));
            let as_provider = ProtocolObject::from_ref(&*provider);
            let types = NSArray::from_retained_slice(&promised_types);
            if !item.setDataProvider_forTypes(as_provider, &types) {
                return Err(ClipError::Busy);
            }
            Some(provider)
        };

        self.board.clearContents();
        let writing: Retained<ProtocolObject<dyn NSPasteboardWriting>> =
            ProtocolObject::from_retained(item);
        if !self
            .board
            .writeObjects(&NSArray::from_retained_slice(&[writing]))
        {
            return Err(ClipError::Busy);
        }
        self.written = Some(Written {
            generation,
            formats: formats.to_vec(),
            promised,
            provider,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests;
