//! The one table of physical keys and their platform codes (§7.1).
//!
//! Each row is a key on the HID keyboard page, keyed by usage. Columns follow
//! §7.1; the Windows and Linux columns are left empty until those platform
//! layers exist. Reverse lookups are computed from this table at compile time,
//! so a key can only ever be described here.

use crate::hid::Hid;

/// How a HID usage maps onto a macOS virtual keycode (`kVK_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MacKey {
    /// A one-to-one mapping: both directions use this keycode.
    Exact(u16),
    /// Injected as this keycode, but capturing that keycode reports the key
    /// that owns it. macOS does not distinguish the two (PrintScreen is F13,
    /// the ISO Non-US `#` key is Backslash), so they share a code.
    SendAs(u16),
    /// macOS has no keycode for this key.
    NoKey,
}

/// One physical key, one row of the §7.1 table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyRow {
    pub hid: Hid,
    /// Set-1 scancode, when the Windows layer is written.
    pub win_scancode: Option<u16>,
    /// Whether the scancode carries the `KEYEVENTF_EXTENDEDKEY` flag.
    pub win_extended: bool,
    pub mac: MacKey,
    pub linux_evdev: Option<u16>,
    pub name: &'static str,
}

const NO: Option<u16> = None;
use MacKey::{Exact, NoKey, SendAs};

macro_rules! key_table {
    ($(($usage:literal, $win:expr, $ext:literal, $mac:expr, $evdev:expr, $name:literal)),* $(,)?) => {
        /// Every key known to the platform layers, ordered by usage.
        pub const KEY_TABLE: &[KeyRow] = &[$(KeyRow {
            hid: Hid::key($usage),
            win_scancode: $win,
            win_extended: $ext,
            mac: $mac,
            linux_evdev: $evdev,
            name: $name,
        }),*];
    };
}

key_table! {
    // Letters.
    (0x04, NO, false, Exact(0x00), NO, "A"),
    (0x05, NO, false, Exact(0x0B), NO, "B"),
    (0x06, NO, false, Exact(0x08), NO, "C"),
    (0x07, NO, false, Exact(0x02), NO, "D"),
    (0x08, NO, false, Exact(0x0E), NO, "E"),
    (0x09, NO, false, Exact(0x03), NO, "F"),
    (0x0A, NO, false, Exact(0x05), NO, "G"),
    (0x0B, NO, false, Exact(0x04), NO, "H"),
    (0x0C, NO, false, Exact(0x22), NO, "I"),
    (0x0D, NO, false, Exact(0x26), NO, "J"),
    (0x0E, NO, false, Exact(0x28), NO, "K"),
    (0x0F, NO, false, Exact(0x25), NO, "L"),
    (0x10, NO, false, Exact(0x2E), NO, "M"),
    (0x11, NO, false, Exact(0x2D), NO, "N"),
    (0x12, NO, false, Exact(0x1F), NO, "O"),
    (0x13, NO, false, Exact(0x23), NO, "P"),
    (0x14, NO, false, Exact(0x0C), NO, "Q"),
    (0x15, NO, false, Exact(0x0F), NO, "R"),
    (0x16, NO, false, Exact(0x01), NO, "S"),
    (0x17, NO, false, Exact(0x11), NO, "T"),
    (0x18, NO, false, Exact(0x20), NO, "U"),
    (0x19, NO, false, Exact(0x09), NO, "V"),
    (0x1A, NO, false, Exact(0x0D), NO, "W"),
    (0x1B, NO, false, Exact(0x07), NO, "X"),
    (0x1C, NO, false, Exact(0x10), NO, "Y"),
    (0x1D, NO, false, Exact(0x06), NO, "Z"),
    // Digits.
    (0x1E, NO, false, Exact(0x12), NO, "1"),
    (0x1F, NO, false, Exact(0x13), NO, "2"),
    (0x20, NO, false, Exact(0x14), NO, "3"),
    (0x21, NO, false, Exact(0x15), NO, "4"),
    (0x22, NO, false, Exact(0x17), NO, "5"),
    (0x23, NO, false, Exact(0x16), NO, "6"),
    (0x24, NO, false, Exact(0x1A), NO, "7"),
    (0x25, NO, false, Exact(0x1C), NO, "8"),
    (0x26, NO, false, Exact(0x19), NO, "9"),
    (0x27, NO, false, Exact(0x1D), NO, "0"),
    // Editing and punctuation.
    (0x28, NO, false, Exact(0x24), NO, "Enter"),
    (0x29, NO, false, Exact(0x35), NO, "Esc"),
    (0x2A, NO, false, Exact(0x33), NO, "Backspace"),
    (0x2B, NO, false, Exact(0x30), NO, "Tab"),
    (0x2C, NO, false, Exact(0x31), NO, "Space"),
    (0x2D, NO, false, Exact(0x1B), NO, "Minus"),
    (0x2E, NO, false, Exact(0x18), NO, "Equal"),
    (0x2F, NO, false, Exact(0x21), NO, "BracketLeft"),
    (0x30, NO, false, Exact(0x1E), NO, "BracketRight"),
    (0x31, NO, false, Exact(0x2A), NO, "Backslash"),
    (0x32, NO, false, SendAs(0x2A), NO, "NonUsHash"),
    (0x33, NO, false, Exact(0x29), NO, "Semicolon"),
    (0x34, NO, false, Exact(0x27), NO, "Quote"),
    (0x35, NO, false, Exact(0x32), NO, "Grave"),
    (0x36, NO, false, Exact(0x2B), NO, "Comma"),
    (0x37, NO, false, Exact(0x2F), NO, "Period"),
    (0x38, NO, false, Exact(0x2C), NO, "Slash"),
    (0x39, NO, false, Exact(0x39), NO, "CapsLock"),
    // Function row.
    (0x3A, NO, false, Exact(0x7A), NO, "F1"),
    (0x3B, NO, false, Exact(0x78), NO, "F2"),
    (0x3C, NO, false, Exact(0x63), NO, "F3"),
    (0x3D, NO, false, Exact(0x76), NO, "F4"),
    (0x3E, NO, false, Exact(0x60), NO, "F5"),
    (0x3F, NO, false, Exact(0x61), NO, "F6"),
    (0x40, NO, false, Exact(0x62), NO, "F7"),
    (0x41, NO, false, Exact(0x64), NO, "F8"),
    (0x42, NO, false, Exact(0x65), NO, "F9"),
    (0x43, NO, false, Exact(0x6D), NO, "F10"),
    (0x44, NO, false, Exact(0x67), NO, "F11"),
    (0x45, NO, false, Exact(0x6F), NO, "F12"),
    // macOS reads the PC system keys as F13-F15.
    (0x46, NO, false, SendAs(0x69), NO, "PrintScreen"),
    (0x47, NO, false, SendAs(0x6B), NO, "ScrollLock"),
    (0x48, NO, false, SendAs(0x71), NO, "Pause"),
    (0x49, NO, false, Exact(0x72), NO, "Insert"), // kVK_Help
    (0x4A, NO, false, Exact(0x73), NO, "Home"),
    (0x4B, NO, false, Exact(0x74), NO, "PageUp"),
    (0x4C, NO, false, Exact(0x75), NO, "Delete"),
    (0x4D, NO, false, Exact(0x77), NO, "End"),
    (0x4E, NO, false, Exact(0x79), NO, "PageDown"),
    (0x4F, NO, false, Exact(0x7C), NO, "Right"),
    (0x50, NO, false, Exact(0x7B), NO, "Left"),
    (0x51, NO, false, Exact(0x7D), NO, "Down"),
    (0x52, NO, false, Exact(0x7E), NO, "Up"),
    // Numeric keypad (not named in hid.rs, but real keyboards have one).
    (0x53, NO, false, Exact(0x47), NO, "NumLock"), // kVK_ANSI_KeypadClear
    (0x54, NO, false, Exact(0x4B), NO, "KeypadDivide"),
    (0x55, NO, false, Exact(0x43), NO, "KeypadMultiply"),
    (0x56, NO, false, Exact(0x4E), NO, "KeypadMinus"),
    (0x57, NO, false, Exact(0x45), NO, "KeypadPlus"),
    (0x58, NO, false, Exact(0x4C), NO, "KeypadEnter"),
    (0x59, NO, false, Exact(0x53), NO, "Keypad1"),
    (0x5A, NO, false, Exact(0x54), NO, "Keypad2"),
    (0x5B, NO, false, Exact(0x55), NO, "Keypad3"),
    (0x5C, NO, false, Exact(0x56), NO, "Keypad4"),
    (0x5D, NO, false, Exact(0x57), NO, "Keypad5"),
    (0x5E, NO, false, Exact(0x58), NO, "Keypad6"),
    (0x5F, NO, false, Exact(0x59), NO, "Keypad7"),
    (0x60, NO, false, Exact(0x5B), NO, "Keypad8"),
    (0x61, NO, false, Exact(0x5C), NO, "Keypad9"),
    (0x62, NO, false, Exact(0x52), NO, "Keypad0"),
    (0x63, NO, false, Exact(0x41), NO, "KeypadDecimal"),
    (0x64, NO, false, Exact(0x0A), NO, "NonUsBackslash"), // kVK_ISO_Section
    (0x65, NO, false, Exact(0x6E), NO, "Application"), // kVK_ContextualMenu
    (0x67, NO, false, Exact(0x51), NO, "KeypadEqual"),
    // F13-F24. macOS has keycodes up to F20.
    (0x68, NO, false, Exact(0x69), NO, "F13"),
    (0x69, NO, false, Exact(0x6B), NO, "F14"),
    (0x6A, NO, false, Exact(0x71), NO, "F15"),
    (0x6B, NO, false, Exact(0x6A), NO, "F16"),
    (0x6C, NO, false, Exact(0x40), NO, "F17"),
    (0x6D, NO, false, Exact(0x4F), NO, "F18"),
    (0x6E, NO, false, Exact(0x50), NO, "F19"),
    (0x6F, NO, false, Exact(0x5A), NO, "F20"),
    (0x70, NO, false, NoKey, NO, "F21"),
    (0x71, NO, false, NoKey, NO, "F22"),
    (0x72, NO, false, NoKey, NO, "F23"),
    (0x73, NO, false, NoKey, NO, "F24"),
    // Volume keys (keyboard-page usages).
    (0x7F, NO, false, Exact(0x4A), NO, "Mute"),
    (0x80, NO, false, Exact(0x48), NO, "VolumeUp"),
    (0x81, NO, false, Exact(0x49), NO, "VolumeDown"),
    (0x85, NO, false, Exact(0x5F), NO, "KeypadComma"), // kVK_JIS_KeypadComma
    // JIS keys (§7.1).
    (0x87, NO, false, Exact(0x5E), NO, "Intl1"), // kVK_JIS_Underscore, Ro
    (0x88, NO, false, NoKey, NO, "Intl2"),
    (0x89, NO, false, Exact(0x5D), NO, "Intl3"), // kVK_JIS_Yen
    (0x8A, NO, false, NoKey, NO, "Intl4"),
    (0x8B, NO, false, NoKey, NO, "Intl5"),
    (0x90, NO, false, Exact(0x68), NO, "Lang1"), // kVK_JIS_Kana
    (0x91, NO, false, Exact(0x66), NO, "Lang2"), // kVK_JIS_Eisu
    // Modifiers.
    (0xE0, NO, false, Exact(0x3B), NO, "LCtrl"),
    (0xE1, NO, false, Exact(0x38), NO, "LShift"),
    (0xE2, NO, false, Exact(0x3A), NO, "LAlt"),
    (0xE3, NO, false, Exact(0x37), NO, "LGui"),
    (0xE4, NO, false, Exact(0x3E), NO, "RCtrl"),
    (0xE5, NO, false, Exact(0x3C), NO, "RShift"),
    (0xE6, NO, false, Exact(0x3D), NO, "RAlt"),
    (0xE7, NO, false, Exact(0x36), NO, "RGui"),
}

const MAC_KEYCODES: usize = 128;
const NO_HID: u32 = 0;
const NO_MAC: u16 = u16::MAX;

const fn build_mac_to_hid() -> [u32; MAC_KEYCODES] {
    let mut out = [NO_HID; MAC_KEYCODES];
    let mut i = 0;
    while i < KEY_TABLE.len() {
        let row = KEY_TABLE[i];
        if let Exact(code) = row.mac {
            assert!((code as usize) < MAC_KEYCODES, "keycode out of range");
            assert!(out[code as usize] == NO_HID, "duplicate macOS keycode");
            out[code as usize] = row.hid.0;
        }
        i += 1;
    }
    out
}

const fn build_hid_to_mac() -> [u16; 256] {
    let mut out = [NO_MAC; 256];
    let mut i = 0;
    while i < KEY_TABLE.len() {
        let row = KEY_TABLE[i];
        let usage = row.hid.usage() as usize;
        assert!(usage < 256, "usage out of range");
        out[usage] = match row.mac {
            Exact(code) | SendAs(code) => code,
            NoKey => NO_MAC,
        };
        i += 1;
    }
    out
}

static MAC_TO_HID: [u32; MAC_KEYCODES] = build_mac_to_hid();
static HID_TO_MAC: [u16; 256] = build_hid_to_mac();

/// The macOS virtual keycode to inject for `hid`.
pub fn hid_to_mac(hid: Hid) -> Option<u16> {
    if hid.page() != Hid::key(0).page() || hid.usage() >= 256 {
        return None;
    }
    match HID_TO_MAC[hid.usage() as usize] {
        NO_MAC => None,
        code => Some(code),
    }
}

/// The HID usage of a key pressed on a Mac.
pub fn mac_to_hid(keycode: u16) -> Option<Hid> {
    match MAC_TO_HID.get(keycode as usize) {
        Some(&raw) if raw != NO_HID => Some(Hid(raw)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid::key;

    #[test]
    fn every_row_round_trips_on_its_own_terms() {
        for row in KEY_TABLE {
            match row.mac {
                Exact(code) => {
                    assert_eq!(hid_to_mac(row.hid), Some(code), "{}", row.name);
                    assert_eq!(mac_to_hid(code), Some(row.hid), "{}", row.name);
                }
                SendAs(code) => {
                    assert_eq!(hid_to_mac(row.hid), Some(code), "{}", row.name);
                    let owner = mac_to_hid(code).expect("SendAs must name an owned keycode");
                    assert_ne!(owner, row.hid, "{}", row.name);
                }
                NoKey => assert_eq!(hid_to_mac(row.hid), None, "{}", row.name),
            }
        }
    }

    #[test]
    fn all_mac_keycodes_round_trip() {
        for code in 0..=u16::from(u8::MAX) {
            if let Some(hid) = mac_to_hid(code) {
                assert_eq!(hid_to_mac(hid), Some(code), "keycode {code:#x}");
            }
        }
    }

    #[test]
    fn every_key_named_in_hid_rs_has_a_row() {
        for usage in 0u16..256 {
            let hid = Hid::key(usage);
            let name = hid.to_string();
            if name.starts_with("0x") {
                continue;
            }
            let row = KEY_TABLE
                .iter()
                .find(|r| r.hid == hid)
                .unwrap_or_else(|| panic!("{name} has no key-table row"));
            assert_eq!(row.name, name, "row name differs from hid.rs");
        }
    }

    #[test]
    fn rows_are_sorted_and_unique() {
        for pair in KEY_TABLE.windows(2) {
            assert!(
                pair[0].hid < pair[1].hid,
                "{} before {}",
                pair[0].name,
                pair[1].name
            );
        }
    }

    #[test]
    fn jis_keys_follow_the_design_table() {
        assert_eq!(hid_to_mac(key::INTL1), Some(0x5E));
        assert_eq!(hid_to_mac(key::INTL3), Some(0x5D));
        assert_eq!(hid_to_mac(key::LANG1), Some(0x68));
        assert_eq!(hid_to_mac(key::LANG2), Some(0x66));
        // No Mac keyboard or keycode has these three.
        assert_eq!(hid_to_mac(key::INTL2), None);
        assert_eq!(hid_to_mac(key::INTL4), None);
        assert_eq!(hid_to_mac(key::INTL5), None);
    }

    #[test]
    fn keys_macos_cannot_tell_apart_share_a_code() {
        assert_eq!(hid_to_mac(key::NON_US_HASH), hid_to_mac(key::BACKSLASH));
        assert_eq!(mac_to_hid(0x2A), Some(key::BACKSLASH));
        assert_eq!(hid_to_mac(key::PRINT_SCREEN), hid_to_mac(key::F13));
        assert_eq!(hid_to_mac(key::SCROLL_LOCK), hid_to_mac(key::F14));
        assert_eq!(hid_to_mac(key::PAUSE), hid_to_mac(key::F15));
        assert_eq!(mac_to_hid(0x69), Some(key::F13));
    }

    #[test]
    fn unknown_input_maps_to_nothing() {
        assert_eq!(mac_to_hid(0x3F), None, "Fn has no keyboard-page usage");
        assert_eq!(mac_to_hid(0xFFFF), None);
        assert_eq!(hid_to_mac(Hid(0x000C_00E9)), None, "consumer page");
    }

    #[test]
    fn windows_columns_are_reserved_and_empty() {
        for row in KEY_TABLE {
            assert_eq!(row.win_scancode, None, "{}", row.name);
            assert!(!row.win_extended, "{}", row.name);
            assert_eq!(row.linux_evdev, None, "{}", row.name);
        }
    }
}
