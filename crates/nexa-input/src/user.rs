//! The user's `keymap.toml` (§7.2, DESIGN §8.3 "可逐条编辑"): built-in rules switched
//! off, and rules of their own, merged over the built-in profiles.
//!
//! ```toml
//! [[profile]]
//! from = "macos"
//! to = "windows"
//! disable = [{ when = "Cmd+Q" }, { when = "Ctrl+C", ctx = "terminal" }]
//!
//! [[profile.rule]]          # added after the built-in ones, so it wins a tie
//! when = "Cmd+K"
//! then = "Ctrl+K"
//! enabled = false           # kept, but off
//! ```
//!
//! A built-in rule is named by its trigger: `when` (compared as a chord, so
//! `Shift+Cmd+Z` names `Cmd+Shift+Z`) and `ctx`.

use std::{io, path::Path};

use crate::{
    FocusClass, Os,
    hid::{Chord, ChordKey, Hid, ModSet, key},
    keymap::{
        Compiled, Keymap, KeymapError, RawKeymap, RawProfile, RawRef, RawRule, RawThen, parse_ctx,
        parse_os,
    },
    rewriter::{Output, PhysKey, Rewriter},
};

pub const FILE_NAME: &str = "keymap.toml";
/// A file larger than this is not a keymap anyone wrote by hand or in the UI.
const MAX_FILE: u64 = 1 << 20;

fn os_name(os: Os) -> &'static str {
    match os {
        Os::Windows => "windows",
        Os::Linux => "linux",
        _ => "macos",
    }
}

/// One rule as the settings view lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleView {
    /// From the built-in keymap; otherwise the user's own.
    pub builtin: bool,
    pub ctx: Option<String>,
    pub when: String,
    pub then: Vec<String>,
    pub hold: Option<String>,
    pub tag: Option<String>,
    pub enabled: bool,
}

impl RuleView {
    fn of(r: &RawRule, builtin: bool, enabled: bool) -> Self {
        Self {
            builtin,
            ctx: r.ctx.clone(),
            when: r.when.clone(),
            then: r.then.items(),
            hold: r.hold.clone(),
            tag: r.tag.clone(),
            enabled,
        }
    }

    fn raw(&self) -> RawRule {
        RawRule {
            ctx: self.ctx.clone().filter(|c| !c.is_empty()),
            when: self.when.trim().to_owned(),
            then: if self.then.len() == 1 {
                RawThen::One(self.then[0].trim().to_owned())
            } else {
                RawThen::Sequence(self.then.iter().map(|t| t.trim().to_owned()).collect())
            },
            hold: self.hold.clone().filter(|h| !h.is_empty()),
            tag: self.tag.clone().filter(|t| !t.is_empty()),
            enabled: self.enabled,
        }
    }
}

/// `(ctx, modifiers, key)` of a rule's trigger, for naming rules.
fn trigger(when: &str, ctx: Option<&str>) -> Option<(Option<FocusClass>, ModSet, ChordKey)> {
    let chord: Chord = when.parse().ok()?;
    let ctx = match ctx {
        Some(c) => Some(parse_ctx(c).ok()?),
        None => None,
    };
    Some((ctx, chord.mods.collapsed(), chord.key))
}

fn same_trigger(a: &RawRule, b: &RawRef) -> bool {
    match (
        trigger(&a.when, a.ctx.as_deref()),
        trigger(&b.when, b.ctx.as_deref()),
    ) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// The user's keymap file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UserKeymap {
    raw: RawKeymap,
}

impl UserKeymap {
    /// Parses and checks a user file: every rule must compile.
    pub fn parse(text: &str) -> Result<Self, KeymapError> {
        let raw: RawKeymap = toml::from_str(text)?;
        let me = Self { raw };
        me.keymap()?;
        Ok(me)
    }

    /// `dir/keymap.toml`, or nothing if there is none.
    pub fn load(dir: &Path) -> Result<Self, KeymapError> {
        let path = dir.join(FILE_NAME);
        match std::fs::metadata(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Ok(m) if m.len() > MAX_FILE => {
                return Err(KeymapError::InvalidRule {
                    when: FILE_NAME.into(),
                    reason: "file too large",
                });
            }
            _ => {}
        }
        let text = std::fs::read_to_string(&path).map_err(|_| KeymapError::InvalidRule {
            when: FILE_NAME.into(),
            reason: "cannot be read",
        })?;
        Self::parse(&text)
    }

    pub fn to_toml(&self) -> String {
        let mut out = String::from(
            "# NexaDesk keyboard rules (DESIGN §8.3). Built-in rules live in the app;\n\
             # this file only switches some off and adds your own.\n\n",
        );
        out.push_str(&toml::to_string(&self.raw).unwrap_or_default());
        out
    }

    /// Writes `dir/keymap.toml` through a temporary file and a rename (§11).
    pub fn save(&self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(FILE_NAME);
        let tmp = dir.join(format!(".{FILE_NAME}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, self.to_toml())?;
        std::fs::rename(&tmp, &path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }

    fn profile(&self, from: Os, to: Os) -> Option<&RawProfile> {
        self.raw
            .profile
            .iter()
            .rev()
            .find(|p| p.from == os_name(from) && p.to == os_name(to))
    }

    /// The rules in force for `from → to`: built-in ones first (switched off if the
    /// file says so), then the user's.
    pub fn rules(&self, from: Os, to: Os) -> Vec<RuleView> {
        let user = self.profile(from, to);
        let disabled =
            |r: &RawRule| user.is_some_and(|u| u.disable.iter().any(|d| same_trigger(r, d)));
        let mut out: Vec<RuleView> = Keymap::builtin_raw()
            .profile
            .iter()
            .filter(|p| p.from == os_name(from) && p.to == os_name(to))
            .flat_map(|p| p.rule.iter())
            .map(|r| RuleView::of(r, true, !disabled(r)))
            .collect();
        if let Some(u) = user {
            out.extend(u.rule.iter().map(|r| RuleView::of(r, false, r.enabled)));
        }
        out
    }

    /// Replaces the user's part of `from → to` with what the settings view shows:
    /// `rules` is the whole list, built-in rules included (only their `enabled`
    /// counts). Refused, unchanged, if any rule is invalid.
    pub fn set_rules(&mut self, from: Os, to: Os, rules: &[RuleView]) -> Result<(), KeymapError> {
        let disable = rules
            .iter()
            .filter(|r| r.builtin && !r.enabled)
            .map(|r| RawRef {
                when: r.when.clone(),
                ctx: r.ctx.clone(),
            })
            .collect();
        let rule: Vec<RawRule> = rules
            .iter()
            .filter(|r| !r.builtin)
            .map(RuleView::raw)
            .collect();
        let mut next = self.clone();
        next.raw
            .profile
            .retain(|p| !(p.from == os_name(from) && p.to == os_name(to)));
        let profile = RawProfile {
            from: os_name(from).into(),
            to: os_name(to).into(),
            deferred: Vec::new(),
            disable,
            rule,
        };
        if !(profile.disable.is_empty() && profile.rule.is_empty()) {
            next.raw.profile.push(profile);
        }
        // Each user rule on its own, switched on, so a rule that is off is still checked.
        for r in rules.iter().filter(|r| !r.builtin) {
            let mut alone = r.raw();
            alone.enabled = true;
            check_rule(from, to, alone)?;
        }
        next.keymap()?;
        *self = next;
        Ok(())
    }

    /// The built-in keymap with this file applied.
    pub fn keymap(&self) -> Result<Keymap, KeymapError> {
        let mut merged = Keymap::builtin_raw();
        for user in &self.raw.profile {
            parse_os(&user.from)?;
            parse_os(&user.to)?;
            match merged
                .profile
                .iter_mut()
                .find(|p| p.from == user.from && p.to == user.to)
            {
                Some(base) => {
                    base.rule
                        .retain(|r| !user.disable.iter().any(|d| same_trigger(r, d)));
                    base.rule.extend(user.rule.iter().cloned());
                    for d in &user.deferred {
                        if !base.deferred.contains(d) {
                            base.deferred.push(d.clone());
                        }
                    }
                }
                None => merged.profile.push(RawProfile {
                    disable: Vec::new(),
                    ..user.clone()
                }),
            }
        }
        Keymap::from_raw(merged)
    }
}

/// Whether one rule would compile in the `from → to` profile.
fn check_rule(from: Os, to: Os, rule: RawRule) -> Result<(), KeymapError> {
    let deferred = Keymap::builtin_raw()
        .profile
        .into_iter()
        .find(|p| p.from == os_name(from) && p.to == os_name(to))
        .map(|p| p.deferred)
        .unwrap_or_default();
    Compiled::compile(RawProfile {
        from: os_name(from).into(),
        to: os_name(to).into(),
        deferred,
        disable: Vec::new(),
        rule: vec![rule],
    })
    .map(|_| ())
}

/// What one chord turns into, without sending anything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Preview {
    /// Every key event the remote side would get, in order: (name, down).
    pub events: Vec<(String, bool)>,
    /// The chords those events make, e.g. `["Ctrl+Y"]` or `["Shift+Home", "Delete"]`.
    pub sent: Vec<String>,
    /// The paste gate (§8.3) would hold these keys until the clipboard is there.
    pub paste: bool,
}

/// A key's name as the remote OS calls it.
pub fn key_name(hid: Hid, remote: Os) -> String {
    let mac = !matches!(remote, Os::Windows | Os::Linux);
    let name = match (hid, mac) {
        (key::LGUI | key::RGUI, true) => "Cmd",
        (key::LGUI | key::RGUI, false) => "Win",
        (key::LALT | key::RALT, true) => "Option",
        (key::LALT | key::RALT, false) => "Alt",
        (key::LCTRL | key::RCTRL, _) => "Ctrl",
        (key::LSHIFT | key::RSHIFT, _) => "Shift",
        _ => return hid.to_string(),
    };
    name.to_owned()
}

/// Runs the real [`Rewriter`] for `from → to` on `chord` pressed and released:
/// modifiers down in order, the key down and up, modifiers up in reverse. A chord
/// of modifiers alone is a tap (`Cmd` → `Win`).
pub fn preview(
    keymap: &Keymap,
    from: Os,
    to: Os,
    chord: &str,
    ctx: FocusClass,
) -> Result<Preview, KeymapError> {
    let parsed: Chord = chord.parse()?;
    let key = match parsed.key {
        ChordKey::Key(k) => Some(k),
        ChordKey::None if !parsed.mods.is_empty() => None,
        _ => {
            return Err(KeymapError::InvalidRule {
                when: chord.to_owned(),
                reason: "press one key, optionally with modifiers",
            });
        }
    };
    let mut rw = Rewriter::new(keymap.for_session(from, to));
    let mods: Vec<Hid> = parsed.mods.iter().collect();
    let mut outputs = Vec::new();
    let mut feed = |hid, down| {
        outputs.extend(rw.on_key(
            PhysKey {
                hid,
                down,
                repeat: false,
            },
            ctx,
        ));
    };
    for &m in &mods {
        feed(m, true);
    }
    if let Some(k) = key {
        feed(k, true);
        feed(k, false);
    }
    for &m in mods.iter().rev() {
        feed(m, false);
    }

    let mut p = Preview::default();
    let mut down = ModSet::EMPTY;
    let mut tapped = ModSet::EMPTY;
    for o in outputs {
        match o {
            Output::Key { hid, down: d, .. } => {
                p.events.push((key_name(hid, to), d));
                match (hid.is_modifier(), d) {
                    (true, true) => {
                        down.insert(hid);
                        tapped.insert(hid);
                    }
                    (true, false) => down.remove(hid),
                    (false, true) => p.sent.push(chord_text(down, Some(hid), to)),
                    (false, false) => {}
                }
            }
            Output::PasteMarker => p.paste = true,
            Output::Button { .. } | Output::Wheel { .. } => {}
        }
    }
    // Modifiers alone (a tap rule): the chord is what went down.
    if p.sent.is_empty() && !tapped.is_empty() {
        p.sent.push(chord_text(tapped, None, to));
    }
    Ok(p)
}

fn chord_text(mods: ModSet, key: Option<Hid>, remote: Os) -> String {
    let order = [key::LCTRL, key::LALT, key::LSHIFT, key::LGUI];
    let mut parts: Vec<String> = order
        .iter()
        .filter(|m| !(mods.collapsed() & ModSet::single(**m)).is_empty())
        .map(|m| key_name(*m, remote))
        .collect();
    if let Some(k) = key {
        parts.push(key_name(k, remote));
    }
    parts.join("+")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(when: &str, then: &str) -> RuleView {
        RuleView {
            builtin: false,
            ctx: None,
            when: when.into(),
            then: vec![then.into()],
            hold: None,
            tag: None,
            enabled: true,
        }
    }

    #[test]
    fn previews_run_the_real_rewriter() {
        let k = Keymap::builtin();
        let p = preview(
            &k,
            Os::Macos,
            Os::Windows,
            "Cmd+Shift+Z",
            FocusClass::Default,
        )
        .unwrap();
        assert_eq!(p.sent, vec!["Ctrl+Y"]);
        assert!(!p.paste);
        let p = preview(&k, Os::Macos, Os::Windows, "Cmd+V", FocusClass::Default).unwrap();
        assert_eq!(p.sent, vec!["Ctrl+V"]);
        assert!(p.paste);
        let p = preview(&k, Os::Macos, Os::Windows, "Cmd", FocusClass::Default).unwrap();
        assert_eq!(p.sent, vec!["Win"]);
        let p = preview(&k, Os::Macos, Os::Macos, "Cmd+Q", FocusClass::Default).unwrap();
        assert_eq!(p.sent, vec!["Cmd+Q"]);
        assert!(preview(&k, Os::Macos, Os::Windows, "Cmd+Nope", FocusClass::Default).is_err());
        assert!(preview(&k, Os::Macos, Os::Windows, "Click", FocusClass::Default).is_err());
    }

    #[test]
    fn switching_off_and_adding_rules_changes_what_is_sent_and_survives_a_save() {
        let dir = tempfile::tempdir().unwrap();
        let mut user = UserKeymap::load(dir.path()).unwrap();
        assert_eq!(user, UserKeymap::default());
        let mut rules = user.rules(Os::Macos, Os::Windows);
        assert!(rules.iter().all(|r| r.builtin && r.enabled));
        let quit = rules.iter_mut().find(|r| r.when == "Cmd+Q").unwrap();
        quit.enabled = false;
        rules.push(view("Cmd+K", "Ctrl+Shift+K"));
        user.set_rules(Os::Macos, Os::Windows, &rules).unwrap();
        user.save(dir.path()).unwrap();

        let loaded = UserKeymap::load(dir.path()).unwrap();
        assert_eq!(loaded, user);
        let k = loaded.keymap().unwrap();
        let sent = |c| {
            preview(&k, Os::Macos, Os::Windows, c, FocusClass::Default)
                .unwrap()
                .sent
        };
        assert_eq!(
            sent("Cmd+Q"),
            vec!["Ctrl+Q"],
            "the general Cmd+* rule applies now"
        );
        assert_eq!(sent("Cmd+K"), vec!["Ctrl+Shift+K"]);
        let listed = loaded.rules(Os::Macos, Os::Windows);
        assert!(!listed.iter().find(|r| r.when == "Cmd+Q").unwrap().enabled);
        assert!(
            listed
                .last()
                .is_some_and(|r| !r.builtin && r.when == "Cmd+K")
        );
        // Other profiles are untouched.
        assert!(
            loaded
                .rules(Os::Windows, Os::Macos)
                .iter()
                .all(|r| r.enabled)
        );
    }

    #[test]
    fn a_user_rule_that_is_off_is_kept_but_not_applied() {
        let mut user = UserKeymap::default();
        let mut rule = view("Cmd+K", "Ctrl+Shift+K");
        rule.enabled = false;
        user.set_rules(Os::Macos, Os::Windows, &[rule]).unwrap();
        let text = user.to_toml();
        assert!(text.contains("enabled = false"), "{text}");
        let k = UserKeymap::parse(&text).unwrap().keymap().unwrap();
        let p = preview(&k, Os::Macos, Os::Windows, "Cmd+K", FocusClass::Default).unwrap();
        assert_eq!(p.sent, vec!["Ctrl+K"]);
    }

    #[test]
    fn invalid_rules_are_refused_and_nothing_changes() {
        let mut user = UserKeymap::default();
        for bad in [
            view("Cmd+Nope", "Ctrl+K"),
            view("*", "Ctrl+*"),
            view("Cmd+K", ""),
        ] {
            assert!(user.set_rules(Os::Macos, Os::Windows, &[bad]).is_err());
        }
        let mut off = view("Cmd+Nope", "Ctrl+K");
        off.enabled = false;
        assert!(user.set_rules(Os::Macos, Os::Windows, &[off]).is_err());
        assert_eq!(user, UserKeymap::default());
        assert!(UserKeymap::parse("[[profile]]\nfrom = \"amiga\"\nto = \"macos\"\n").is_err());
        assert!(UserKeymap::parse("garbage = [").is_err());
    }

    #[test]
    fn rules_are_named_by_chord_not_by_spelling() {
        let user = UserKeymap::parse(
            "[[profile]]\nfrom = \"macos\"\nto = \"windows\"\ndisable = [{ when = \"Shift+Command+z\" }]\n",
        )
        .unwrap();
        let rules = user.rules(Os::Macos, Os::Windows);
        assert!(
            !rules
                .iter()
                .find(|r| r.when == "Cmd+Shift+Z")
                .unwrap()
                .enabled
        );
        let k = user.keymap().unwrap();
        let p = preview(
            &k,
            Os::Macos,
            Os::Windows,
            "Cmd+Shift+Z",
            FocusClass::Default,
        )
        .unwrap();
        assert_eq!(p.sent, vec!["Ctrl+Shift+Z"]);
    }
}
