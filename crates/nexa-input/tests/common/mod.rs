#![allow(dead_code)]

use nexa_input::hid::{parse_key_name, parse_modifier_name};
use nexa_input::{
    Compiled, FocusClass, Hid, Keymap, Os, Output, PhysButton, PhysKey, PhysWheel, Rewriter,
};
use std::sync::Arc;

pub fn hid(name: &str) -> Hid {
    parse_modifier_name(name)
        .or_else(|| parse_key_name(name))
        .unwrap_or_else(|| panic!("unknown key {name}"))
}

pub fn render<'a>(outputs: impl IntoIterator<Item = &'a Output>) -> String {
    outputs
        .into_iter()
        .map(|o| match o {
            Output::Key {
                hid,
                down: true,
                repeat: false,
            } => format!("+{hid}"),
            Output::Key {
                hid,
                down: true,
                repeat: true,
            } => format!("*{hid}"),
            Output::Key {
                hid, down: false, ..
            } => format!("-{hid}"),
            Output::Button { button, down: true } => format!("+btn{button}"),
            Output::Button {
                button,
                down: false,
            } => format!("-btn{button}"),
            Output::Wheel { .. } => "wheel".to_owned(),
            Output::PasteMarker => "paste".to_owned(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub struct Harness {
    pub rewriter: Rewriter,
    pub ctx: FocusClass,
}

impl Harness {
    pub fn new(profile: Arc<Compiled>) -> Self {
        Self {
            rewriter: Rewriter::new(profile),
            ctx: FocusClass::Default,
        }
    }

    pub fn session(local: Os, remote: Os) -> Self {
        Self::new(Keymap::builtin().for_session(local, remote))
    }

    pub fn mac_to_win() -> Self {
        Self::session(Os::Macos, Os::Windows)
    }

    pub fn win_to_mac() -> Self {
        Self::session(Os::Windows, Os::Macos)
    }

    pub fn terminal(mut self) -> Self {
        self.ctx = FocusClass::Terminal;
        self
    }

    fn key(&mut self, name: &str, down: bool, repeat: bool) -> String {
        let ev = PhysKey {
            hid: hid(name),
            down,
            repeat,
        };
        render(&self.rewriter.on_key(ev, self.ctx))
    }

    pub fn down(&mut self, name: &str) -> String {
        self.key(name, true, false)
    }

    pub fn up(&mut self, name: &str) -> String {
        self.key(name, false, false)
    }

    pub fn repeat(&mut self, name: &str) -> String {
        self.key(name, true, true)
    }

    pub fn button(&mut self, button: u32, down: bool) -> String {
        render(&self.rewriter.on_button(PhysButton { button, down }))
    }

    pub fn wheel(&mut self) -> String {
        render(&self.rewriter.on_wheel(PhysWheel { dx: 0, dy: 120 }))
    }

    pub fn reset(&mut self) -> String {
        render(&self.rewriter.reset())
    }

    /// Presses the keys in order and releases them in reverse, returning the
    /// whole output stream.
    pub fn chord(&mut self, keys: &[&str]) -> String {
        let mut stream: Vec<String> = keys.iter().map(|k| self.down(k)).collect();
        stream.extend(keys.iter().rev().map(|k| self.up(k)));
        let joined = stream
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        self.assert_idle();
        joined
    }

    pub fn assert_idle(&self) {
        assert_eq!(
            self.rewriter.pressed_keys().count(),
            0,
            "out_refs not empty"
        );
        assert!(self.rewriter.out_mods().is_empty(), "out_mods not empty");
    }
}

pub const LETTERS: &[&str] = &[
    "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S",
    "T", "U", "V", "W", "X", "Y", "Z",
];

pub const DIGITS: &[&str] = &["1", "2", "3", "4", "5", "6", "7", "8", "9", "0"];
