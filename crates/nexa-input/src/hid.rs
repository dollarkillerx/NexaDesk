//! USB HID usages, modifier sets, and chord-string parsing (§7.1).

use std::fmt;
use std::ops::{BitAnd, BitOr, BitOrAssign, Sub};
use std::str::FromStr;

use thiserror::Error;

const PAGE_KEYBOARD: u32 = 0x07;

/// A key as carried on the wire: `(page << 16) | usage`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hid(pub u32);

impl Hid {
    /// A usage on the keyboard page.
    pub const fn key(usage: u16) -> Self {
        Hid((PAGE_KEYBOARD << 16) | usage as u32)
    }

    pub const fn page(self) -> u16 {
        (self.0 >> 16) as u16
    }

    pub const fn usage(self) -> u16 {
        self.0 as u16
    }

    pub const fn is_modifier(self) -> bool {
        self.page() == PAGE_KEYBOARD as u16 && self.usage() >= 0xE0 && self.usage() <= 0xE7
    }
}

impl fmt::Display for Hid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match NAMES.iter().find(|(_, h)| h == self) {
            Some((name, _)) => f.write_str(name),
            None => write!(f, "{:#x}", self.0),
        }
    }
}

/// Keyboard-page usage constants.
pub mod key {
    use super::Hid;

    macro_rules! keys {
        ($($name:ident = $usage:literal),* $(,)?) => {
            $(pub const $name: Hid = Hid::key($usage);)*
        };
    }

    keys! {
        A = 0x04, B = 0x05, C = 0x06, D = 0x07, E = 0x08, F = 0x09, G = 0x0A, H = 0x0B,
        I = 0x0C, J = 0x0D, K = 0x0E, L = 0x0F, M = 0x10, N = 0x11, O = 0x12, P = 0x13,
        Q = 0x14, R = 0x15, S = 0x16, T = 0x17, U = 0x18, V = 0x19, W = 0x1A, X = 0x1B,
        Y = 0x1C, Z = 0x1D,
        D1 = 0x1E, D2 = 0x1F, D3 = 0x20, D4 = 0x21, D5 = 0x22,
        D6 = 0x23, D7 = 0x24, D8 = 0x25, D9 = 0x26, D0 = 0x27,
        ENTER = 0x28, ESC = 0x29, BACKSPACE = 0x2A, TAB = 0x2B, SPACE = 0x2C,
        MINUS = 0x2D, EQUAL = 0x2E, BRACKET_LEFT = 0x2F, BRACKET_RIGHT = 0x30, BACKSLASH = 0x31,
        NON_US_HASH = 0x32, SEMICOLON = 0x33, QUOTE = 0x34, GRAVE = 0x35, COMMA = 0x36,
        PERIOD = 0x37, SLASH = 0x38, CAPS_LOCK = 0x39,
        F1 = 0x3A, F2 = 0x3B, F3 = 0x3C, F4 = 0x3D, F5 = 0x3E, F6 = 0x3F,
        F7 = 0x40, F8 = 0x41, F9 = 0x42, F10 = 0x43, F11 = 0x44, F12 = 0x45,
        PRINT_SCREEN = 0x46, SCROLL_LOCK = 0x47, PAUSE = 0x48, INSERT = 0x49,
        HOME = 0x4A, PAGE_UP = 0x4B, DELETE = 0x4C, END = 0x4D, PAGE_DOWN = 0x4E,
        RIGHT = 0x4F, LEFT = 0x50, DOWN = 0x51, UP = 0x52,
        NON_US_BACKSLASH = 0x64, APPLICATION = 0x65,
        F13 = 0x68, F14 = 0x69, F15 = 0x6A, F16 = 0x6B, F17 = 0x6C, F18 = 0x6D,
        F19 = 0x6E, F20 = 0x6F, F21 = 0x70, F22 = 0x71, F23 = 0x72, F24 = 0x73,
        INTL1 = 0x87, INTL2 = 0x88, INTL3 = 0x89, INTL4 = 0x8A, INTL5 = 0x8B,
        LANG1 = 0x90, LANG2 = 0x91,
        LCTRL = 0xE0, LSHIFT = 0xE1, LALT = 0xE2, LGUI = 0xE3,
        RCTRL = 0xE4, RSHIFT = 0xE5, RALT = 0xE6, RGUI = 0xE7,
    }
}

/// Canonical name first for each key: `Display` uses the first match.
const NAMES: &[(&str, Hid)] = &[
    ("A", key::A),
    ("B", key::B),
    ("C", key::C),
    ("D", key::D),
    ("E", key::E),
    ("F", key::F),
    ("G", key::G),
    ("H", key::H),
    ("I", key::I),
    ("J", key::J),
    ("K", key::K),
    ("L", key::L),
    ("M", key::M),
    ("N", key::N),
    ("O", key::O),
    ("P", key::P),
    ("Q", key::Q),
    ("R", key::R),
    ("S", key::S),
    ("T", key::T),
    ("U", key::U),
    ("V", key::V),
    ("W", key::W),
    ("X", key::X),
    ("Y", key::Y),
    ("Z", key::Z),
    ("1", key::D1),
    ("2", key::D2),
    ("3", key::D3),
    ("4", key::D4),
    ("5", key::D5),
    ("6", key::D6),
    ("7", key::D7),
    ("8", key::D8),
    ("9", key::D9),
    ("0", key::D0),
    ("Enter", key::ENTER),
    ("Return", key::ENTER),
    ("Esc", key::ESC),
    ("Escape", key::ESC),
    ("Backspace", key::BACKSPACE),
    ("Tab", key::TAB),
    ("Space", key::SPACE),
    ("Minus", key::MINUS),
    ("-", key::MINUS),
    ("Equal", key::EQUAL),
    ("=", key::EQUAL),
    ("BracketLeft", key::BRACKET_LEFT),
    ("[", key::BRACKET_LEFT),
    ("BracketRight", key::BRACKET_RIGHT),
    ("]", key::BRACKET_RIGHT),
    ("Backslash", key::BACKSLASH),
    ("\\", key::BACKSLASH),
    ("NonUsHash", key::NON_US_HASH),
    ("Semicolon", key::SEMICOLON),
    (";", key::SEMICOLON),
    ("Quote", key::QUOTE),
    ("'", key::QUOTE),
    ("Grave", key::GRAVE),
    ("`", key::GRAVE),
    ("Comma", key::COMMA),
    (",", key::COMMA),
    ("Period", key::PERIOD),
    (".", key::PERIOD),
    ("Slash", key::SLASH),
    ("/", key::SLASH),
    ("CapsLock", key::CAPS_LOCK),
    ("F1", key::F1),
    ("F2", key::F2),
    ("F3", key::F3),
    ("F4", key::F4),
    ("F5", key::F5),
    ("F6", key::F6),
    ("F7", key::F7),
    ("F8", key::F8),
    ("F9", key::F9),
    ("F10", key::F10),
    ("F11", key::F11),
    ("F12", key::F12),
    ("F13", key::F13),
    ("F14", key::F14),
    ("F15", key::F15),
    ("F16", key::F16),
    ("F17", key::F17),
    ("F18", key::F18),
    ("F19", key::F19),
    ("F20", key::F20),
    ("F21", key::F21),
    ("F22", key::F22),
    ("F23", key::F23),
    ("F24", key::F24),
    ("PrintScreen", key::PRINT_SCREEN),
    ("PrtSc", key::PRINT_SCREEN),
    ("ScrollLock", key::SCROLL_LOCK),
    ("Pause", key::PAUSE),
    ("Insert", key::INSERT),
    ("Home", key::HOME),
    ("End", key::END),
    ("PageUp", key::PAGE_UP),
    ("PgUp", key::PAGE_UP),
    ("PageDown", key::PAGE_DOWN),
    ("PgDn", key::PAGE_DOWN),
    ("Delete", key::DELETE),
    ("Del", key::DELETE),
    ("Left", key::LEFT),
    ("←", key::LEFT),
    ("Right", key::RIGHT),
    ("→", key::RIGHT),
    ("Up", key::UP),
    ("↑", key::UP),
    ("Down", key::DOWN),
    ("↓", key::DOWN),
    ("NonUsBackslash", key::NON_US_BACKSLASH),
    ("Application", key::APPLICATION),
    ("Intl1", key::INTL1),
    ("Ro", key::INTL1),
    ("Intl2", key::INTL2),
    ("KatakanaHiragana", key::INTL2),
    ("Intl3", key::INTL3),
    ("Yen", key::INTL3),
    ("Intl4", key::INTL4),
    ("Henkan", key::INTL4),
    ("変換", key::INTL4),
    ("Intl5", key::INTL5),
    ("Muhenkan", key::INTL5),
    ("無変換", key::INTL5),
    ("Lang1", key::LANG1),
    ("Kana", key::LANG1),
    ("かな", key::LANG1),
    ("Lang2", key::LANG2),
    ("Eisu", key::LANG2),
    ("英数", key::LANG2),
    ("LCtrl", key::LCTRL),
    ("LShift", key::LSHIFT),
    ("LAlt", key::LALT),
    ("LGui", key::LGUI),
    ("RCtrl", key::RCTRL),
    ("RShift", key::RSHIFT),
    ("RAlt", key::RALT),
    ("RGui", key::RGUI),
];

/// Looks up a non-modifier key by any of its names, case-insensitively.
pub fn parse_key_name(name: &str) -> Option<Hid> {
    NAMES
        .iter()
        .find(|(n, h)| !h.is_modifier() && n.eq_ignore_ascii_case(name))
        .map(|(_, h)| *h)
}

/// Looks up a modifier by name. Unqualified names mean the left-hand key;
/// matching against held keys is side-insensitive (see [`ModSet::collapsed`]).
pub fn parse_modifier_name(name: &str) -> Option<Hid> {
    const ALIASES: &[(&str, Hid)] = &[
        ("Cmd", key::LGUI),
        ("Command", key::LGUI),
        ("Win", key::LGUI),
        ("Windows", key::LGUI),
        ("Gui", key::LGUI),
        ("Super", key::LGUI),
        ("Meta", key::LGUI),
        ("LGui", key::LGUI),
        ("LCmd", key::LGUI),
        ("LWin", key::LGUI),
        ("RGui", key::RGUI),
        ("RCmd", key::RGUI),
        ("RWin", key::RGUI),
        ("Alt", key::LALT),
        ("Option", key::LALT),
        ("Opt", key::LALT),
        ("LAlt", key::LALT),
        ("LOption", key::LALT),
        ("RAlt", key::RALT),
        ("ROption", key::RALT),
        ("AltGr", key::RALT),
        ("Ctrl", key::LCTRL),
        ("Control", key::LCTRL),
        ("LCtrl", key::LCTRL),
        ("RCtrl", key::RCTRL),
        ("Shift", key::LSHIFT),
        ("LShift", key::LSHIFT),
        ("RShift", key::RSHIFT),
    ];
    ALIASES
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, h)| *h)
}

/// A set of the eight HID modifiers; bit `n` is usage `0xE0 + n`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ModSet(u8);

impl ModSet {
    pub const EMPTY: ModSet = ModSet(0);
    const LEFT_HAND: u8 = 0x0F;

    fn bit(hid: Hid) -> u8 {
        debug_assert!(hid.is_modifier());
        1 << (hid.usage() - 0xE0)
    }

    pub fn single(hid: Hid) -> Self {
        ModSet(Self::bit(hid))
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn len(self) -> u32 {
        self.0.count_ones()
    }

    pub fn contains(self, hid: Hid) -> bool {
        hid.is_modifier() && self.0 & Self::bit(hid) != 0
    }

    pub const fn is_superset(self, other: ModSet) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn insert(&mut self, hid: Hid) {
        self.0 |= Self::bit(hid);
    }

    pub fn remove(&mut self, hid: Hid) {
        self.0 &= !Self::bit(hid);
    }

    /// Folds right-hand modifiers onto their left-hand counterparts, for
    /// side-insensitive comparison.
    pub const fn collapsed(self) -> Self {
        ModSet((self.0 | (self.0 >> 4)) & Self::LEFT_HAND)
    }

    /// Ascending by usage.
    pub fn iter(self) -> impl Iterator<Item = Hid> {
        (0u16..8)
            .filter(move |i| self.0 & (1 << i) != 0)
            .map(|i| Hid::key(0xE0 + i))
    }
}

impl BitOr for ModSet {
    type Output = ModSet;
    fn bitor(self, rhs: ModSet) -> ModSet {
        ModSet(self.0 | rhs.0)
    }
}

impl BitOrAssign for ModSet {
    fn bitor_assign(&mut self, rhs: ModSet) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for ModSet {
    type Output = ModSet;
    fn bitand(self, rhs: ModSet) -> ModSet {
        ModSet(self.0 & rhs.0)
    }
}

impl Sub for ModSet {
    type Output = ModSet;
    fn sub(self, rhs: ModSet) -> ModSet {
        ModSet(self.0 & !rhs.0)
    }
}

impl FromIterator<Hid> for ModSet {
    fn from_iter<I: IntoIterator<Item = Hid>>(iter: I) -> Self {
        let mut set = ModSet::EMPTY;
        for hid in iter {
            set.insert(hid);
        }
        set
    }
}

/// The non-modifier part of a chord.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChordKey {
    /// Modifiers only, as in a tap rule (`when = "Cmd"`).
    None,
    Key(Hid),
    /// `*`: see [`matches_wildcard`].
    Any,
    Click,
    Wheel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chord {
    pub mods: ModSet,
    pub key: ChordKey,
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum ChordError {
    #[error("empty component in chord {0:?}")]
    EmptyPart(String),
    #[error("unknown key {part:?} in chord {chord:?}")]
    UnknownKey { chord: String, part: String },
    #[error("more than one non-modifier key in chord {0:?}")]
    MultipleKeys(String),
}

impl FromStr for Chord {
    type Err = ChordError;

    fn from_str(s: &str) -> Result<Self, ChordError> {
        let mut mods = ModSet::EMPTY;
        let mut chord_key = ChordKey::None;
        for raw in s.split('+') {
            let part = raw.trim();
            if part.is_empty() {
                return Err(ChordError::EmptyPart(s.to_owned()));
            }
            if let Some(m) = parse_modifier_name(part) {
                mods.insert(m);
                continue;
            }
            let parsed = if part == "*" {
                ChordKey::Any
            } else if part.eq_ignore_ascii_case("Click") {
                ChordKey::Click
            } else if part.eq_ignore_ascii_case("Wheel") {
                ChordKey::Wheel
            } else {
                ChordKey::Key(parse_key_name(part).ok_or_else(|| ChordError::UnknownKey {
                    chord: s.to_owned(),
                    part: part.to_owned(),
                })?)
            };
            if chord_key != ChordKey::None {
                return Err(ChordError::MultipleKeys(s.to_owned()));
            }
            chord_key = parsed;
        }
        Ok(Chord {
            mods,
            key: chord_key,
        })
    }
}

/// Whether `*` applies to this key: character-producing keys and Enter.
///
/// Tab, Esc, Space, Backspace, Delete, navigation and function keys are
/// excluded, so that for instance Ctrl+Tab does not become Cmd+Tab on a Mac.
pub fn matches_wildcard(hid: Hid) -> bool {
    if hid.page() != PAGE_KEYBOARD as u16 {
        return false;
    }
    matches!(hid.usage(), 0x04..=0x28 | 0x2D..=0x38 | 0x64 | 0x87 | 0x89)
}
