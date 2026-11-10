//! `keymap.toml` parsing and rule compilation (§7.2).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::hid::{Chord, ChordError, ChordKey, Hid, ModSet, matches_wildcard, parse_modifier_name};
use crate::{FocusClass, Os};

const DEFAULT_KEYMAP: &str = include_str!("default_keymap.toml");

#[derive(Debug, Error)]
pub enum KeymapError {
    #[error("invalid keymap TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error(transparent)]
    Chord(#[from] ChordError),
    #[error("unknown modifier {0:?}")]
    UnknownModifier(String),
    #[error("unknown OS {0:?}")]
    UnknownOs(String),
    #[error("unknown focus class {0:?}")]
    UnknownContext(String),
    #[error("unknown tag {0:?}")]
    UnknownTag(String),
    #[error("rule {when:?}: {reason}")]
    InvalidRule { when: String, reason: &'static str },
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawKeymap {
    #[serde(default)]
    pub(crate) profile: Vec<RawProfile>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawProfile {
    pub(crate) from: String,
    pub(crate) to: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) deferred: Vec<String>,
    /// User files only: built-in rules of this profile switched off.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) disable: Vec<RawRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) rule: Vec<RawRule>,
}

/// Names a rule by its trigger: `when` and `ctx`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRef {
    pub(crate) when: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) ctx: Option<String>,
}

fn yes() -> bool {
    true
}

fn is_yes(b: &bool) -> bool {
    *b
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) ctx: Option<String>,
    pub(crate) when: String,
    pub(crate) then: RawThen,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) hold: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tag: Option<String>,
    /// User files only: a rule kept but switched off.
    #[serde(default = "yes", skip_serializing_if = "is_yes")]
    pub(crate) enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub(crate) enum RawThen {
    One(String),
    Sequence(Vec<String>),
}

impl RawThen {
    pub(crate) fn items(&self) -> Vec<String> {
        match self {
            Self::One(s) => vec![s.clone()],
            Self::Sequence(v) => v.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tag {
    Paste,
}

#[derive(Clone, Debug)]
pub(crate) struct Rule {
    ctx: Option<FocusClass>,
    /// Collapsed to left-hand modifiers.
    when_mods: ModSet,
    when_key: ChordKey,
    pub(crate) then: Vec<Chord>,
    pub(crate) hold: Option<Hid>,
    pub(crate) tag: Option<Tag>,
}

impl Rule {
    pub(crate) fn is_wildcard(&self) -> bool {
        self.when_key == ChordKey::Any
    }

    /// Held modifiers not named by this rule; only a wildcard carries them over.
    pub(crate) fn extras(&self, phys: ModSet) -> ModSet {
        if !self.is_wildcard() {
            return ModSet::EMPTY;
        }
        phys.iter().filter(|m| !self.names(*m)).collect()
    }

    /// Whether the rule's trigger side includes this physical modifier.
    pub(crate) fn names(&self, modifier: Hid) -> bool {
        self.when_mods
            .is_superset(ModSet::single(modifier).collapsed())
    }
}

/// One direction's rule set, ready for a [`crate::Rewriter`].
#[derive(Clone, Debug, Default)]
pub struct Compiled {
    pub(crate) deferred: ModSet,
    rules: Vec<Rule>,
    /// Keyed by collapsed modifier; value is the modifier set tapped.
    taps: Vec<(ModSet, ModSet)>,
}

impl Compiled {
    /// A profile with no rules: every key is forwarded unchanged.
    pub fn passthrough() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn tap_for(&self, modifier: Hid) -> Option<ModSet> {
        let key = ModSet::single(modifier).collapsed();
        self.taps
            .iter()
            .find(|(m, _)| *m == key)
            .map(|(_, then)| *then)
    }

    /// Picks the winning rule. `ctx` is `None` for events without a focus
    /// class (buttons, wheel), which only rules without `ctx` can match.
    pub(crate) fn find(
        &self,
        ctx: Option<FocusClass>,
        phys: ModSet,
        key: ChordKey,
    ) -> Option<&Rule> {
        let held = phys.collapsed();
        let mut best = None;
        let mut best_score = (0u8, 0u8, 0u32, 0usize);
        for (index, rule) in self.rules.iter().enumerate() {
            if rule.ctx.is_some() && rule.ctx != ctx {
                continue;
            }
            let ctx_rank = u8::from(rule.ctx.is_some());
            let tier = if rule.when_key == key && rule.when_mods == held {
                1 + ctx_rank
            } else if let (true, ChordKey::Key(k)) = (rule.is_wildcard(), key)
                && matches_wildcard(k)
                && held.is_superset(rule.when_mods)
            {
                0
            } else {
                continue;
            };
            // Within a tier: rule with ctx, then more specific, then defined later.
            let score = (tier, ctx_rank, rule.when_mods.len(), index);
            if best.is_none() || score > best_score {
                best = Some(rule);
                best_score = score;
            }
        }
        best
    }

    pub(crate) fn compile(raw: RawProfile) -> Result<Self, KeymapError> {
        let mut deferred = ModSet::EMPTY;
        for name in &raw.deferred {
            let m = parse_modifier_name(name)
                .ok_or_else(|| KeymapError::UnknownModifier(name.clone()))?;
            deferred.insert(m);
        }
        let mut compiled = Compiled {
            deferred,
            ..Default::default()
        };
        for rule in raw.rule.into_iter().filter(|r| r.enabled) {
            compiled.add_rule(rule)?;
        }
        Ok(compiled)
    }

    fn add_rule(&mut self, raw: RawRule) -> Result<(), KeymapError> {
        let when_text = raw.when.clone();
        let invalid = |reason| KeymapError::InvalidRule {
            when: when_text.clone(),
            reason,
        };

        let when: Chord = raw.when.parse()?;
        let ctx = raw.ctx.as_deref().map(parse_ctx).transpose()?;
        let tag = raw.tag.as_deref().map(parse_tag).transpose()?;
        let hold = raw
            .hold
            .as_deref()
            .map(|h| {
                parse_modifier_name(h).ok_or_else(|| KeymapError::UnknownModifier(h.to_owned()))
            })
            .transpose()?;
        let then = match raw.then {
            RawThen::One(text) => vec![text.parse::<Chord>()?],
            RawThen::Sequence(items) => items
                .iter()
                .map(|t| t.parse())
                .collect::<Result<Vec<Chord>, _>>()?,
        };
        if then.is_empty() {
            return Err(invalid("`then` is empty"));
        }

        if when.key == ChordKey::None {
            if when.mods.len() != 1 || then.len() != 1 || then[0].key != ChordKey::None {
                return Err(invalid("a tap rule maps one modifier to modifiers"));
            }
            if ctx.is_some() || hold.is_some() || tag.is_some() {
                return Err(invalid("a tap rule takes no ctx, hold or tag"));
            }
            let side = ModSet::single(when.mods.iter().next().expect("one modifier"));
            if (self.deferred.collapsed() & side.collapsed()).is_empty() {
                return Err(invalid("a tap rule needs its modifier in `deferred`"));
            }
            self.taps.push((side.collapsed(), then[0].mods));
            return Ok(());
        }

        if when.key == ChordKey::Any && when.mods.is_empty() {
            return Err(invalid("a bare `*` would capture all typing"));
        }
        for chord in &then {
            match chord.key {
                ChordKey::Any if when.key != ChordKey::Any => {
                    return Err(invalid("`*` in `then` needs `*` in `when`"));
                }
                ChordKey::Click | ChordKey::Wheel if chord.key != when.key => {
                    return Err(invalid("Click and Wheel map only to themselves"));
                }
                ChordKey::Key(_) | ChordKey::Any
                    if matches!(when.key, ChordKey::Click | ChordKey::Wheel) =>
                {
                    return Err(invalid("Click and Wheel map only to themselves"));
                }
                _ => {}
            }
        }
        if matches!(when.key, ChordKey::Click | ChordKey::Wheel) && then.len() != 1 {
            return Err(invalid("Click and Wheel cannot map to a sequence"));
        }
        if let Some(h) = hold
            && (then.len() != 1 || !then[0].mods.contains(h) || when.mods.is_empty())
        {
            return Err(invalid(
                "`hold` needs a modifier in `when` and in a single-chord `then`",
            ));
        }

        self.rules.push(Rule {
            ctx,
            when_mods: when.mods.collapsed(),
            when_key: when.key,
            then,
            hold,
            tag,
        });
        Ok(())
    }
}

pub(crate) fn parse_os(name: &str) -> Result<Os, KeymapError> {
    match name {
        "macos" => Ok(Os::Macos),
        "windows" => Ok(Os::Windows),
        "linux" => Ok(Os::Linux),
        _ => Err(KeymapError::UnknownOs(name.to_owned())),
    }
}

pub(crate) fn parse_ctx(name: &str) -> Result<FocusClass, KeymapError> {
    match name {
        "default" => Ok(FocusClass::Default),
        "terminal" => Ok(FocusClass::Terminal),
        "ide" => Ok(FocusClass::Ide),
        _ => Err(KeymapError::UnknownContext(name.to_owned())),
    }
}

fn parse_tag(name: &str) -> Result<Tag, KeymapError> {
    match name {
        "paste" => Ok(Tag::Paste),
        _ => Err(KeymapError::UnknownTag(name.to_owned())),
    }
}

/// All profiles from a `keymap.toml` document.
#[derive(Clone, Debug, Default)]
pub struct Keymap {
    profiles: Vec<(Os, Os, Arc<Compiled>)>,
}

impl Keymap {
    pub fn parse(text: &str) -> Result<Self, KeymapError> {
        let raw: RawKeymap = toml::from_str(text)?;
        Self::from_raw(raw)
    }

    pub(crate) fn builtin_raw() -> RawKeymap {
        toml::from_str(DEFAULT_KEYMAP).expect("embedded keymap is valid")
    }

    pub(crate) fn from_raw(raw: RawKeymap) -> Result<Self, KeymapError> {
        let mut profiles = Vec::with_capacity(raw.profile.len());
        for profile in raw.profile {
            let from = parse_os(&profile.from)?;
            let to = parse_os(&profile.to)?;
            profiles.push((from, to, Arc::new(Compiled::compile(profile)?)));
        }
        Ok(Keymap { profiles })
    }

    /// The built-in profiles from DESIGN.md §8.3.
    pub fn builtin() -> Self {
        Self::parse(DEFAULT_KEYMAP).expect("embedded keymap is valid")
    }

    /// The profile for a session. Pairs without a profile forward everything
    /// unchanged. Same-OS profiles only tag paste chords, so the paste gate
    /// still works when no key needs rewriting.
    pub fn for_session(&self, local: Os, remote: Os) -> Arc<Compiled> {
        self.profiles
            .iter()
            .rev()
            .find(|(from, to, _)| *from == local && *to == remote)
            .map(|(_, _, compiled)| Arc::clone(compiled))
            .unwrap_or_else(Compiled::passthrough)
    }
}
