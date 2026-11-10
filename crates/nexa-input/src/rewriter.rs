//! The chord rewriter state machine (§7.2).
//!
//! Output modifiers always converge on [`Rewriter::desired`]: instead of the
//! pseudocode's separate `out_down`/`out_up` calls for each case, every event
//! that can change the modifier picture ends by diffing against that one
//! function. Keys released out of order therefore cannot unbalance the state.

use std::collections::BTreeMap;
use std::sync::Arc;

use smallvec::{SmallVec, smallvec};

use crate::FocusClass;
use crate::hid::{ChordKey, Hid, ModSet, key};
use crate::keymap::{Compiled, Rule, Tag};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysKey {
    pub hid: Hid,
    pub down: bool,
    pub repeat: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysButton {
    pub button: u32,
    pub down: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysWheel {
    pub dx: i32,
    pub dy: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Output {
    Key {
        hid: Hid,
        down: bool,
        repeat: bool,
    },
    Button {
        button: u32,
        down: bool,
    },
    Wheel {
        dx: i32,
        dy: i32,
    },
    /// The keys that follow in this batch are a paste; see the gate in §8.3.
    PasteMarker,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RewriteMode {
    #[default]
    Rewrite,
    /// Game mode: rules off, every key forwarded as pressed.
    Passthrough,
}

type Out = SmallVec<[Output; 6]>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Key(Hid),
    Button(u32),
}

#[derive(Clone, Copy, Debug)]
struct Step {
    mods: ModSet,
    key: Option<Hid>,
}

#[derive(Debug)]
struct Active {
    source: Source,
    emitted: SmallVec<[Hid; 2]>,
    mods_for_key: ModSet,
    /// Output modifiers stay at `mods_for_key` while this input is held.
    locked: bool,
    /// Non-empty for a `then` sequence, replayed on repeat.
    sequence: SmallVec<[Step; 2]>,
}

#[derive(Clone, Copy, Debug)]
struct Hold {
    trigger: Hid,
    output: Hid,
}

/// Rewrites physical input into what the remote side should see.
///
/// Each physical key is resolved once, on press; its release and repeats
/// follow from what that press emitted, whatever the modifiers do afterwards.
pub struct Rewriter {
    profile: Arc<Compiled>,
    mode: RewriteMode,
    held_deferred: ModSet,
    used_as_chord: ModSet,
    phys_mods: ModSet,
    /// In press order.
    active: Vec<Active>,
    out_mods: ModSet,
    out_refs: BTreeMap<Hid, u32>,
    hold: Option<Hold>,
}

impl Rewriter {
    pub fn new(profile: Arc<Compiled>) -> Self {
        Self {
            profile,
            mode: RewriteMode::Rewrite,
            held_deferred: ModSet::EMPTY,
            used_as_chord: ModSet::EMPTY,
            phys_mods: ModSet::EMPTY,
            active: Vec::new(),
            out_mods: ModSet::EMPTY,
            out_refs: BTreeMap::new(),
            hold: None,
        }
    }

    pub fn mode(&self) -> RewriteMode {
        self.mode
    }

    /// Switches mode. Anything still held is released first, since the old
    /// mode's mapping would not apply to its release.
    pub fn set_mode(&mut self, mode: RewriteMode) -> SmallVec<[Output; 16]> {
        if mode == self.mode {
            return SmallVec::new();
        }
        let released = self.reset();
        self.mode = mode;
        released
    }

    /// Keys currently down on the remote side, for `KeyStateSync` (§7.5).
    pub fn pressed_keys(&self) -> impl Iterator<Item = Hid> + '_ {
        self.out_refs.keys().copied()
    }

    pub fn out_mods(&self) -> ModSet {
        self.out_mods
    }

    pub fn on_key(&mut self, ev: PhysKey, ctx: FocusClass) -> Out {
        let mut out = Out::new();
        match (ev.down, ev.hid.is_modifier()) {
            (true, true) if !ev.repeat && !self.phys_mods.contains(ev.hid) => {
                self.press_modifier(ev.hid, &mut out);
            }
            (true, true) => {} // auto-repeat of a held modifier
            (true, false) if self.is_active(Source::Key(ev.hid)) => {
                self.replay(ev.hid, &mut out);
            }
            (true, false) if !ev.repeat => self.press_key(ev.hid, ctx, &mut out),
            (true, false) => {} // repeat of a key we never saw go down
            (false, true) => self.release_modifier(ev.hid, &mut out),
            (false, false) => self.release_source(Source::Key(ev.hid), &mut out),
        }
        out
    }

    pub fn on_button(&mut self, ev: PhysButton) -> Out {
        let mut out = Out::new();
        let source = Source::Button(ev.button);
        if !ev.down {
            if self.is_active(source) {
                out.push(Output::Button {
                    button: ev.button,
                    down: false,
                });
                self.release_source(source, &mut out);
            }
            return out;
        }
        if self.is_active(source) {
            return out;
        }
        self.used_as_chord |= self.held_deferred;
        let profile = Arc::clone(&self.profile);
        let (mods, locked) = match self.lookup(&profile, None, ChordKey::Click) {
            Some(rule) => (rule.then[0].mods, true),
            None => (
                self.implied() | self.held_deferred,
                !self.held_deferred.is_empty(),
            ),
        };
        self.sync_mods(mods, &mut out);
        out.push(Output::Button {
            button: ev.button,
            down: true,
        });
        self.active.push(Active {
            source,
            emitted: SmallVec::new(),
            mods_for_key: mods,
            locked,
            sequence: SmallVec::new(),
        });
        out
    }

    /// Wheel events are momentary, so their modifiers are restored at once
    /// unless the rule keeps them with `hold`.
    pub fn on_wheel(&mut self, ev: PhysWheel) -> Out {
        let mut out = Out::new();
        self.used_as_chord |= self.held_deferred;
        let profile = Arc::clone(&self.profile);
        let mods = match self.lookup(&profile, None, ChordKey::Wheel) {
            Some(rule) => {
                if let Some(output) = rule.hold {
                    self.begin_hold(rule, output);
                }
                rule.then[0].mods
            }
            None => self.implied() | self.held_deferred,
        };
        self.sync_mods(mods, &mut out);
        out.push(Output::Wheel {
            dx: ev.dx,
            dy: ev.dy,
        });
        self.sync_mods(self.desired(), &mut out);
        out
    }

    /// Releases everything and forgets all physical state (focus loss).
    /// Physical releases that arrive afterwards are ignored.
    pub fn reset(&mut self) -> SmallVec<[Output; 16]> {
        let mut out: SmallVec<[Output; 16]> = SmallVec::new();
        for active in &self.active {
            if let Source::Button(button) = active.source {
                out.push(Output::Button {
                    button,
                    down: false,
                });
            }
        }
        let (modifiers, keys): (Vec<Hid>, Vec<Hid>) =
            self.out_refs.keys().copied().partition(|h| h.is_modifier());
        for hid in keys.into_iter().chain(modifiers) {
            out.push(Output::Key {
                hid,
                down: false,
                repeat: false,
            });
        }
        self.held_deferred = ModSet::EMPTY;
        self.used_as_chord = ModSet::EMPTY;
        self.phys_mods = ModSet::EMPTY;
        self.active.clear();
        self.out_mods = ModSet::EMPTY;
        self.out_refs.clear();
        self.hold = None;
        out
    }

    fn lookup<'p>(
        &self,
        profile: &'p Compiled,
        ctx: Option<FocusClass>,
        key: ChordKey,
    ) -> Option<&'p Rule> {
        match self.mode {
            RewriteMode::Rewrite => profile.find(ctx, self.phys_mods, key),
            RewriteMode::Passthrough => None,
        }
    }

    fn deferred(&self) -> ModSet {
        match self.mode {
            RewriteMode::Rewrite => self.profile.deferred,
            RewriteMode::Passthrough => ModSet::EMPTY,
        }
    }

    /// The modifiers the physical keys call for when no chord is in play.
    fn implied(&self) -> ModSet {
        let mut mods = self.phys_mods - self.held_deferred;
        if let Some(hold) = self.hold {
            mods.remove(hold.trigger);
            mods.insert(hold.output);
        }
        mods
    }

    fn desired(&self) -> ModSet {
        self.active
            .iter()
            .rev()
            .find(|a| a.locked)
            .map(|a| a.mods_for_key)
            .unwrap_or_else(|| self.implied())
    }

    fn is_active(&self, source: Source) -> bool {
        self.active.iter().any(|a| a.source == source)
    }

    fn press_modifier(&mut self, k: Hid, out: &mut Out) {
        // A modifier joined by another is not "alone" even if no key follows.
        if self.deferred().contains(k) {
            if !self.phys_mods.is_empty() {
                self.used_as_chord |= self.held_deferred | ModSet::single(k);
            }
            self.held_deferred.insert(k);
            self.phys_mods.insert(k);
            return;
        }
        self.used_as_chord |= self.held_deferred;
        self.phys_mods.insert(k);
        self.sync_mods(self.desired(), out);
    }

    fn release_modifier(&mut self, k: Hid, out: &mut Out) {
        if !self.phys_mods.contains(k) {
            return;
        }
        self.phys_mods.remove(k);
        if self.held_deferred.contains(k) {
            self.held_deferred.remove(k);
            if !self.used_as_chord.contains(k)
                && self.mode == RewriteMode::Rewrite
                && let Some(tap) = self.profile.tap_for(k)
            {
                self.tap_mods(tap, out);
            }
            self.used_as_chord.remove(k);
        }
        if self.hold.is_some_and(|h| h.trigger == k) {
            self.hold = None;
        }
        self.sync_mods(self.desired(), out);
    }

    fn press_key(&mut self, k: Hid, ctx: FocusClass, out: &mut Out) {
        let profile = Arc::clone(&self.profile);
        let rule = self.lookup(&profile, Some(ctx), ChordKey::Key(k));
        self.used_as_chord |= self.held_deferred;
        match rule {
            Some(rule) => self.apply_rule(rule, k, out),
            None => {
                let mods = self.implied() | self.held_deferred;
                self.sync_mods(mods, out);
                self.press(k, out);
                self.active.push(Active {
                    source: Source::Key(k),
                    emitted: smallvec![k],
                    mods_for_key: mods,
                    locked: !self.held_deferred.is_empty(),
                    sequence: SmallVec::new(),
                });
            }
        }
    }

    fn apply_rule(&mut self, rule: &Rule, k: Hid, out: &mut Out) {
        // The marker leads the batch: the gate must hold back the chord's keys.
        if rule.tag == Some(Tag::Paste) {
            out.push(Output::PasteMarker);
        }
        let extras = rule.extras(self.phys_mods);
        let steps: SmallVec<[Step; 2]> = rule
            .then
            .iter()
            .map(|chord| Step {
                mods: chord.mods | extras,
                key: match chord.key {
                    ChordKey::Key(h) => Some(h),
                    ChordKey::Any => Some(k),
                    _ => None,
                },
            })
            .collect();

        if steps.len() > 1 {
            self.run_sequence(&steps, out);
            self.active.push(Active {
                source: Source::Key(k),
                emitted: SmallVec::new(),
                mods_for_key: ModSet::EMPTY,
                locked: false,
                sequence: steps,
            });
            return;
        }

        let step = steps[0];
        if let Some(output) = rule.hold {
            self.begin_hold(rule, output);
        }
        self.sync_mods(step.mods, out);
        let mut emitted = SmallVec::new();
        if let Some(hid) = step.key {
            self.press(hid, out);
            emitted.push(hid);
        }
        self.active.push(Active {
            source: Source::Key(k),
            emitted,
            mods_for_key: step.mods,
            locked: rule.hold.is_none(), // a hold rule's modifiers follow the trigger
            sequence: SmallVec::new(),
        });
    }

    fn begin_hold(&mut self, rule: &Rule, output: Hid) {
        let order = [
            key::LGUI,
            key::RGUI,
            key::LALT,
            key::RALT,
            key::LCTRL,
            key::RCTRL,
            key::LSHIFT,
            key::RSHIFT,
        ];
        // Prefers the "leading" modifier: Shift may come and go during a hold.
        let trigger = order
            .into_iter()
            .find(|m| self.phys_mods.contains(*m) && rule.names(*m));
        if let Some(trigger) = trigger {
            self.hold = Some(Hold { trigger, output });
        }
    }

    fn run_sequence(&mut self, steps: &[Step], out: &mut Out) {
        for step in steps {
            self.sync_mods(step.mods, out);
            if let Some(hid) = step.key {
                self.press(hid, out);
                self.release(hid, out);
            }
        }
        self.sync_mods(self.desired(), out);
    }

    fn replay(&mut self, k: Hid, out: &mut Out) {
        let Some(active) = self.active.iter().find(|a| a.source == Source::Key(k)) else {
            return;
        };
        if !active.sequence.is_empty() {
            let steps = active.sequence.clone();
            self.run_sequence(&steps, out);
            return;
        }
        let emitted = active.emitted.clone();
        if active.locked {
            self.sync_mods(active.mods_for_key, out);
        }
        for hid in emitted {
            out.push(Output::Key {
                hid,
                down: true,
                repeat: true,
            });
        }
    }

    fn release_source(&mut self, source: Source, out: &mut Out) {
        let Some(index) = self.active.iter().position(|a| a.source == source) else {
            return;
        };
        let active = self.active.remove(index);
        for hid in active.emitted {
            self.release(hid, out);
        }
        self.sync_mods(self.desired(), out);
    }

    fn tap_mods(&mut self, mods: ModSet, out: &mut Out) {
        for m in mods.iter() {
            self.press(m, out);
        }
        for m in mods.iter() {
            self.release(m, out);
        }
    }

    fn sync_mods(&mut self, want: ModSet, out: &mut Out) {
        for m in (self.out_mods - want).iter() {
            self.release(m, out);
        }
        for m in (want - self.out_mods).iter() {
            self.press(m, out);
        }
    }

    fn press(&mut self, hid: Hid, out: &mut Out) {
        let refs = self.out_refs.entry(hid).or_insert(0);
        *refs += 1;
        if *refs == 1 {
            if hid.is_modifier() {
                self.out_mods.insert(hid);
            }
            out.push(Output::Key {
                hid,
                down: true,
                repeat: false,
            });
        }
    }

    fn release(&mut self, hid: Hid, out: &mut Out) {
        let Some(refs) = self.out_refs.get_mut(&hid) else {
            return;
        };
        *refs -= 1;
        if *refs == 0 {
            self.out_refs.remove(&hid);
            if hid.is_modifier() {
                self.out_mods.remove(hid);
            }
            out.push(Output::Key {
                hid,
                down: false,
                repeat: false,
            });
        }
    }
}
