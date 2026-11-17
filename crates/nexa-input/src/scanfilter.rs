//! Keyboard capture's bookkeeping for a source that reports every key, modifiers
//! included, as a separate press and release (Windows' low-level keyboard hook):
//! repeats, keys held from before capture, and the host key chord (§7.4).
//! Portable so it is tested everywhere; the hook itself is Windows-only.

use std::collections::BTreeSet;

use crate::{Hid, ModSet, PhysKey, hid::key};

/// What the filter makes of a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanEvent {
    Key(PhysKey),
    /// The host key chord: never forwarded; it ends game mode or shows the toolbar.
    HostKey,
}

/// The local-only chord: Ctrl+Alt+Win+G, the same keys as ⌃⌥⌘G on a Mac.
pub const HOST_MODS: [Hid; 3] = [key::LCTRL, key::LALT, key::LGUI];
pub const HOST_KEY: Hid = key::G;

#[derive(Debug, Default)]
pub struct ScanFilter {
    held: BTreeSet<Hid>,
    mods: ModSet,
    /// The host key's main key while physically down; none of it is forwarded.
    swallowing: Option<Hid>,
}

impl ScanFilter {
    /// Forgets everything held: capture just started or stopped.
    pub fn reset(&mut self) {
        self.held.clear();
        self.mods = ModSet::EMPTY;
        self.swallowing = None;
    }

    fn is_host_chord(&self, hid: Hid) -> bool {
        hid == HOST_KEY && HOST_MODS.iter().all(|m| self.mods.contains(*m))
    }

    pub fn on_key(&mut self, hid: Hid, down: bool, out: &mut Vec<ScanEvent>) {
        if self.swallowing == Some(hid) {
            if !down {
                self.swallowing = None;
            }
            return;
        }
        let repeat = down && self.held.contains(&hid);
        if down && !repeat && self.is_host_chord(hid) {
            self.swallowing = Some(hid);
            // The chord's modifiers already went out: take them back.
            for m in self.mods.iter() {
                out.push(ScanEvent::Key(PhysKey {
                    hid: m,
                    down: false,
                    repeat: false,
                }));
                self.held.remove(&m);
            }
            self.mods = ModSet::EMPTY;
            out.push(ScanEvent::HostKey);
            return;
        }
        if down {
            self.held.insert(hid);
            if hid.is_modifier() {
                self.mods.insert(hid);
            }
        } else {
            if !self.held.remove(&hid) {
                return; // pressed before capture began: its press never went out
            }
            if hid.is_modifier() {
                self.mods.remove(hid);
            }
        }
        out.push(ScanEvent::Key(PhysKey { hid, down, repeat }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(keys: &[(Hid, bool)]) -> Vec<ScanEvent> {
        let mut f = ScanFilter::default();
        let mut out = Vec::new();
        for &(hid, down) in keys {
            f.on_key(hid, down, &mut out);
        }
        out
    }

    fn k(hid: Hid, down: bool, repeat: bool) -> ScanEvent {
        ScanEvent::Key(PhysKey { hid, down, repeat })
    }

    #[test]
    fn presses_releases_and_repeats() {
        assert_eq!(
            run(&[(key::A, true), (key::A, true), (key::A, false)]),
            vec![
                k(key::A, true, false),
                k(key::A, true, true),
                k(key::A, false, false)
            ]
        );
    }

    #[test]
    fn a_key_held_from_before_capture_is_not_released_alone() {
        assert_eq!(run(&[(key::A, false)]), vec![]);
    }

    #[test]
    fn the_host_chord_takes_back_its_modifiers_and_swallows_its_key() {
        let out = run(&[
            (key::LCTRL, true),
            (key::LALT, true),
            (key::LGUI, true),
            (key::G, true),
            (key::G, true),
            (key::G, false),
            (key::LGUI, false),
        ]);
        assert_eq!(
            &out[..3],
            &[
                k(key::LCTRL, true, false),
                k(key::LALT, true, false),
                k(key::LGUI, true, false)
            ]
        );
        let released: Vec<_> = out[3..6].to_vec();
        for m in [key::LCTRL, key::LALT, key::LGUI] {
            assert!(released.contains(&k(m, false, false)), "{m:?} released");
        }
        assert_eq!(out[6], ScanEvent::HostKey);
        // G's repeat and release, and Win's later release, go nowhere.
        assert_eq!(out.len(), 7);
    }

    #[test]
    fn g_with_fewer_modifiers_is_an_ordinary_key() {
        let out = run(&[(key::LCTRL, true), (key::G, true)]);
        assert_eq!(
            out,
            vec![k(key::LCTRL, true, false), k(key::G, true, false)]
        );
    }
}
