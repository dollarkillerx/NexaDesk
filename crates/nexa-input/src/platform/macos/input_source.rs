//! Keyboard input sources (ABC, Pinyin, Kotoeri, …) through Text Input Sources (TIS).
//!
//! A viewer follows its person's switches (Caps Lock, the Globe key, ⌃Space, the menu
//! bar) and the host selects the same source, so typing on the other Mac feels like
//! typing on this one. Since macOS 14 the TIS calls assert that they run on the main
//! thread: every function here must be called there. The agent's main thread is taken
//! by its runtime, so it selects through a short-lived helper process
//! (`nexa-agent select-input-source <id>`).

use std::ffi::{CStr, c_char, c_void};
use std::ptr;

use crate::SwitchAction;

type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFArrayRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFIndex = isize;
type TISInputSourceRef = *const c_void;

const UTF8: u32 = 0x0800_0100;

#[repr(C)]
struct CallBacks {
    _private: [u8; 0],
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: CallBacks;
    static kCFTypeDictionaryValueCallBacks: CallBacks;
    fn CFRelease(cf: CFTypeRef);
    fn CFStringCreateWithBytes(
        alloc: CFTypeRef,
        bytes: *const u8,
        len: CFIndex,
        encoding: u32,
        external: u8,
    ) -> CFStringRef;
    fn CFStringGetLength(s: CFStringRef) -> CFIndex;
    fn CFStringGetMaximumSizeForEncoding(len: CFIndex, encoding: u32) -> CFIndex;
    fn CFStringGetCString(s: CFStringRef, buf: *mut c_char, size: CFIndex, encoding: u32) -> u8;
    fn CFDictionaryCreate(
        alloc: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        count: CFIndex,
        key_callbacks: *const CallBacks,
        value_callbacks: *const CallBacks,
    ) -> CFDictionaryRef;
    fn CFArrayGetCount(a: CFArrayRef) -> CFIndex;
    fn CFArrayGetValueAtIndex(a: CFArrayRef, i: CFIndex) -> CFTypeRef;
    static kCFBooleanTrue: CFTypeRef;
    fn CFBooleanGetValue(b: CFTypeRef) -> u8;
    fn CFPreferencesCopyAppValue(key: CFStringRef, app: CFStringRef) -> CFTypeRef;
    fn CFNumberGetValue(number: CFTypeRef, kind: CFIndex, out: *mut c_void) -> u8;
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    static kTISPropertyInputSourceID: CFStringRef;
    static kTISPropertyInputSourceCategory: CFStringRef;
    static kTISCategoryKeyboardInputSource: CFStringRef;
    static kTISPropertyInputSourceIsSelectCapable: CFStringRef;
    static kTISPropertyInputSourceIsASCIICapable: CFStringRef;
    static kTISPropertyInputSourceLanguages: CFStringRef;
    /// The distributed notification posted when the selected keyboard input source
    /// changes, by any means.
    static kTISNotifySelectedKeyboardInputSourceChanged: CFStringRef;
    fn TISCopyCurrentKeyboardInputSource() -> TISInputSourceRef;
    fn TISGetInputSourceProperty(source: TISInputSourceRef, key: CFStringRef) -> CFTypeRef;
    fn TISCreateInputSourceList(properties: CFDictionaryRef, include_all: u8) -> CFArrayRef;
    fn TISSelectInputSource(source: TISInputSourceRef) -> i32;
}

/// The name (a CFStringRef) of the distributed notification posted when the selected
/// keyboard input source changes, by any means: what a viewer observes.
pub fn input_source_changed_notification() -> *const c_void {
    // SAFETY: a constant exported by Carbon.
    unsafe { kTISNotifySelectedKeyboardInputSourceChanged }
}

/// A CF string's text. `s` must be a CFString or null.
unsafe fn text_of(s: CFStringRef) -> Option<String> {
    if s.is_null() {
        return None;
    }
    // SAFETY: `s` is a CFString (the caller's promise); the buffer is sized for it.
    unsafe {
        let size = CFStringGetMaximumSizeForEncoding(CFStringGetLength(s), UTF8) + 1;
        let mut buf = vec![0 as c_char; usize::try_from(size).ok()?];
        if CFStringGetCString(s, buf.as_mut_ptr(), size, UTF8) == 0 {
            return None;
        }
        Some(CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
    }
}

/// Whether `id` looks like an input source ID: reverse-DNS, short, no control characters.
/// What the viewer sends is checked before it reaches TIS or a command line.
pub fn is_input_source_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && id.contains('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// The selected keyboard input source's ID. Main thread only.
pub fn current_input_source() -> Option<String> {
    // SAFETY: TIS returns a retained source (released below) or null; the property is
    // a CFString owned by the source.
    unsafe {
        let source = TISCopyCurrentKeyboardInputSource();
        if source.is_null() {
            return None;
        }
        let id = text_of(TISGetInputSourceProperty(source, kTISPropertyInputSourceID));
        CFRelease(source);
        id
    }
}

/// An enabled keyboard input source the person can select.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputSourceInfo {
    pub id: String,
    /// Types plain ASCII letters: ABC, US, a romaji mode.
    pub ascii: bool,
    /// Its languages, most important first (`ja`, `zh-Hans`, …).
    pub languages: Vec<String>,
}

/// A new CFString; the caller releases it.
unsafe fn cf_string(text: &str) -> CFStringRef {
    // SAFETY: the bytes are valid UTF-8 for their length.
    unsafe { CFStringCreateWithBytes(ptr::null(), text.as_ptr(), text.len() as CFIndex, UTF8, 0) }
}

/// The enabled keyboard input sources that can be selected, in the system's order.
/// Main thread only.
pub fn enabled_input_sources() -> Vec<InputSourceInfo> {
    let mut found = Vec::new();
    // SAFETY: the filter and the list are released below; the list's elements and their
    // properties are owned by the list and read before it is released.
    unsafe {
        let keys = [
            kTISPropertyInputSourceCategory,
            kTISPropertyInputSourceIsSelectCapable,
        ];
        let values = [kTISCategoryKeyboardInputSource, kCFBooleanTrue];
        let filter = CFDictionaryCreate(
            ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            2,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        );
        if filter.is_null() {
            return found;
        }
        let list = TISCreateInputSourceList(filter, 0);
        CFRelease(filter);
        if list.is_null() {
            return found;
        }
        for i in 0..CFArrayGetCount(list) {
            let source = CFArrayGetValueAtIndex(list, i);
            let Some(id) = text_of(TISGetInputSourceProperty(source, kTISPropertyInputSourceID))
            else {
                continue;
            };
            let ascii = TISGetInputSourceProperty(source, kTISPropertyInputSourceIsASCIICapable);
            let langs = TISGetInputSourceProperty(source, kTISPropertyInputSourceLanguages);
            let mut languages = Vec::new();
            if !langs.is_null() {
                for j in 0..CFArrayGetCount(langs) {
                    if let Some(l) = text_of(CFArrayGetValueAtIndex(langs, j)) {
                        languages.push(l);
                    }
                }
            }
            found.push(InputSourceInfo {
                id,
                ascii: !ascii.is_null() && CFBooleanGetValue(ascii) != 0,
                languages,
            });
        }
        CFRelease(list);
    }
    found
}

/// Where a switching key goes from `current`, among `sources` (in the system's order):
/// the next one for 🌐, an English layout for 英数, Japanese for かな. `None` when there
/// is nowhere to go (or already there).
pub fn switch_target(
    sources: &[InputSourceInfo],
    current: Option<&str>,
    action: SwitchAction,
) -> Option<String> {
    let at = current.and_then(|c| sources.iter().position(|s| s.id == c));
    let pick = match action {
        SwitchAction::Next => {
            if sources.len() < 2 {
                return None;
            }
            &sources[at.map_or(0, |i| (i + 1) % sources.len())]
        }
        SwitchAction::Ascii => {
            if at.is_some_and(|i| sources[i].ascii && !is_japanese(&sources[i])) {
                return None;
            }
            // A plain layout (ABC, US) before an input method's own roman mode.
            sources
                .iter()
                .find(|s| s.ascii && s.id.contains(".keylayout."))
                .or_else(|| sources.iter().find(|s| s.ascii && !is_japanese(s)))?
        }
        SwitchAction::Japanese => {
            if at.is_some_and(|i| is_japanese(&sources[i]) && !sources[i].ascii) {
                return None;
            }
            // Hiragana before katakana: the system lists an input method's modes in order.
            sources.iter().find(|s| is_japanese(s) && !s.ascii)?
        }
    };
    (Some(pick.id.as_str()) != current).then(|| pick.id.clone())
}

fn is_japanese(s: &InputSourceInfo) -> bool {
    s.languages.first().is_some_and(|l| l == "ja")
}

/// Does what a switching key does on this Mac and returns the source now selected.
/// Main thread only.
pub fn switch_input_source(action: SwitchAction) -> Result<String, String> {
    let current = current_input_source();
    let sources = enabled_input_sources();
    match switch_target(&sources, current.as_deref(), action) {
        Some(id) => select_input_source(&id).map(|()| id),
        None => current.ok_or_else(|| "no input source is selected".to_owned()),
    }
}

/// Whether this Mac's 🌐 key is set to change input source (System Settings, Keyboard,
/// "Press 🌐 key to"), which is the default. The other settings (emoji, dictation, do
/// nothing) leave input source alone.
pub fn globe_changes_input_source() -> bool {
    // SAFETY: the strings are released; the value, a CFNumber when present, is read and
    // released.
    unsafe {
        let key = cf_string("AppleFnUsageType");
        let app = cf_string("com.apple.HIToolbox");
        let value = CFPreferencesCopyAppValue(key, app);
        CFRelease(key);
        CFRelease(app);
        if value.is_null() {
            return true;
        }
        let mut usage: i32 = 1;
        // kCFNumberSInt32Type.
        let ok = CFNumberGetValue(value, 3, (&raw mut usage).cast());
        CFRelease(value);
        ok == 0 || usage == 1
    }
}

/// Selects the enabled input source with this ID. Main thread only.
pub fn select_input_source(id: &str) -> Result<(), String> {
    if !is_input_source_id(id) {
        return Err(format!("not an input source ID: {id:?}"));
    }
    // SAFETY: every CF object created here is released on every path; the list's
    // elements are owned by the list, which outlives their use.
    unsafe {
        let name = cf_string(id);
        if name.is_null() {
            return Err("could not make a CFString".to_owned());
        }
        let keys = [kTISPropertyInputSourceID];
        let values = [name];
        let filter = CFDictionaryCreate(
            ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        );
        CFRelease(name);
        if filter.is_null() {
            return Err("could not make a CFDictionary".to_owned());
        }
        // Enabled sources only: one the person has not added is not theirs to type with.
        let list = TISCreateInputSourceList(filter, 0);
        CFRelease(filter);
        if list.is_null() || CFArrayGetCount(list) == 0 {
            if !list.is_null() {
                CFRelease(list);
            }
            return Err(format!("{id} is not an enabled input source on this Mac"));
        }
        let status = TISSelectInputSource(CFArrayGetValueAtIndex(list, 0));
        CFRelease(list);
        if status == 0 {
            Ok(())
        } else {
            Err(format!("TISSelectInputSource({id}): OS status {status}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(id: &str, ascii: bool, lang: &str) -> InputSourceInfo {
        InputSourceInfo {
            id: id.into(),
            ascii,
            languages: vec![lang.into()],
        }
    }

    /// The Mac mini's: ABC, Simplified Pinyin and Japanese (hiragana, then romaji).
    fn mini() -> Vec<InputSourceInfo> {
        vec![
            src("com.apple.keylayout.ABC", true, "en"),
            src("com.apple.inputmethod.SCIM.ITABC", false, "zh-Hans"),
            src(
                "com.apple.inputmethod.Kotoeri.RomajiTyping.Japanese",
                false,
                "ja",
            ),
            src(
                "com.apple.inputmethod.Kotoeri.RomajiTyping.Roman",
                true,
                "ja",
            ),
        ]
    }

    #[test]
    fn the_globe_key_goes_round_the_list() {
        let s = mini();
        let next = |c: &str| switch_target(&s, Some(c), SwitchAction::Next);
        assert_eq!(
            next("com.apple.keylayout.ABC").as_deref(),
            Some("com.apple.inputmethod.SCIM.ITABC")
        );
        assert_eq!(
            next("com.apple.inputmethod.Kotoeri.RomajiTyping.Roman").as_deref(),
            Some("com.apple.keylayout.ABC")
        );
        assert_eq!(
            switch_target(&s[..1], Some("com.apple.keylayout.ABC"), SwitchAction::Next),
            None
        );
    }

    #[test]
    fn eisu_picks_a_plain_layout_and_kana_picks_hiragana() {
        let s = mini();
        let pinyin = Some("com.apple.inputmethod.SCIM.ITABC");
        assert_eq!(
            switch_target(&s, pinyin, SwitchAction::Ascii).as_deref(),
            Some("com.apple.keylayout.ABC")
        );
        assert_eq!(
            switch_target(&s, pinyin, SwitchAction::Japanese).as_deref(),
            Some("com.apple.inputmethod.Kotoeri.RomajiTyping.Japanese")
        );
        // Already there: nothing to do.
        assert_eq!(
            switch_target(&s, Some("com.apple.keylayout.ABC"), SwitchAction::Ascii),
            None
        );
        // Japanese romaji types ASCII but is still Japanese: 英数 goes to ABC.
        assert_eq!(
            switch_target(
                &s,
                Some("com.apple.inputmethod.Kotoeri.RomajiTyping.Roman"),
                SwitchAction::Ascii
            )
            .as_deref(),
            Some("com.apple.keylayout.ABC")
        );
        // No Japanese input method: かな does nothing.
        assert_eq!(switch_target(&s[..2], pinyin, SwitchAction::Japanese), None);
    }

    #[test]
    fn only_reverse_dns_ids_pass() {
        assert!(is_input_source_id("com.apple.keylayout.ABC"));
        assert!(is_input_source_id("com.apple.inputmethod.SCIM.ITABC"));
        assert!(is_input_source_id("com.google.inputmethod.Japanese.base"));
        assert!(!is_input_source_id(""));
        assert!(!is_input_source_id("ABC"));
        assert!(!is_input_source_id("com.apple; rm -rf /"));
        assert!(!is_input_source_id("com.apple.\nkeylayout"));
        assert!(!is_input_source_id(&"a.".repeat(101)));
    }
}
