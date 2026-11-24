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
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    static kTISPropertyInputSourceID: CFStringRef;
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

/// Selects the enabled input source with this ID. Main thread only.
pub fn select_input_source(id: &str) -> Result<(), String> {
    if !is_input_source_id(id) {
        return Err(format!("not an input source ID: {id:?}"));
    }
    // SAFETY: every CF object created here is released on every path; the list's
    // elements are owned by the list, which outlives their use.
    unsafe {
        let name = CFStringCreateWithBytes(ptr::null(), id.as_ptr(), id.len() as CFIndex, UTF8, 0);
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
