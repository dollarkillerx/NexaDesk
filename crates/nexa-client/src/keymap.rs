//! The keyboard settings behind `nexa_keymap_*` (§11.2): the rules of a profile, saving
//! the user's changes to keymap.toml, and a preview that runs the real rewriter on a
//! chord without a session and without sending anything.

use std::path::{Path, PathBuf};

use nexa_input::{
    FocusClass, Os,
    user::{RuleView, UserKeymap, preview},
};

use crate::proto::{KeyOut, KeyPreview, KeymapRequest, KeymapRule, KeymapView};

fn os_of(n: u32) -> Result<Os, String> {
    match n {
        1 => Ok(Os::Windows),
        2 => Ok(Os::Macos),
        3 => Ok(Os::Linux),
        _ => Err(format!("unknown OS {n}")),
    }
}

fn opt(s: &str) -> Option<String> {
    Some(s.trim().to_owned()).filter(|s| !s.is_empty())
}

fn view_of(r: &KeymapRule) -> RuleView {
    RuleView {
        builtin: r.builtin,
        ctx: opt(&r.ctx),
        when: r.when.clone(),
        then: r.then.clone(),
        hold: opt(&r.hold),
        tag: opt(&r.tag),
        enabled: r.enabled,
    }
}

fn rule_of(v: RuleView) -> KeymapRule {
    KeymapRule {
        builtin: v.builtin,
        ctx: v.ctx.unwrap_or_default(),
        when: v.when,
        then: v.then,
        hold: v.hold.unwrap_or_default(),
        tag: v.tag.unwrap_or_default(),
        enabled: v.enabled,
    }
}

fn dir_of(req: &KeymapRequest, default_dir: &Path) -> PathBuf {
    if req.data_dir.is_empty() {
        default_dir.to_path_buf()
    } else {
        PathBuf::from(&req.data_dir)
    }
}

/// The user's file, or nothing and the reason if it cannot be used.
fn load(dir: &Path) -> (UserKeymap, String) {
    match UserKeymap::load(dir) {
        Ok(u) => (u, String::new()),
        Err(e) => (UserKeymap::default(), e.to_string()),
    }
}

pub fn rules(req: &KeymapRequest, default_dir: &Path) -> Result<KeymapView, String> {
    let (from, to) = (os_of(req.local_os)?, os_of(req.remote_os)?);
    let (user, reason) = load(&dir_of(req, default_dir));
    Ok(KeymapView {
        rules: user.rules(from, to).into_iter().map(rule_of).collect(),
        error: if reason.is_empty() {
            String::new()
        } else {
            format!("keymap.toml cannot be used, so the built-in rules are shown: {reason}")
        },
        file_error: reason,
    })
}

pub fn save(req: &KeymapRequest, default_dir: &Path) -> Result<(), String> {
    let (from, to) = (os_of(req.local_os)?, os_of(req.remote_os)?);
    let dir = dir_of(req, default_dir);
    // A damaged file is replaced by what the person now sees.
    let (mut user, _) = load(&dir);
    let views: Vec<RuleView> = req.rules.iter().map(view_of).collect();
    user.set_rules(from, to, &views)
        .map_err(|e| e.to_string())?;
    user.save(&dir).map_err(|e| format!("keymap.toml: {e}"))
}

pub fn preview_chord(req: &KeymapRequest, default_dir: &Path) -> Result<KeyPreview, String> {
    let (from, to) = (os_of(req.local_os)?, os_of(req.remote_os)?);
    let ctx = match req.ctx {
        1 => FocusClass::Terminal,
        2 => FocusClass::Ide,
        _ => FocusClass::Default,
    };
    let (mut user, _) = load(&dir_of(req, default_dir));
    if req.edited {
        let views: Vec<RuleView> = req.rules.iter().map(view_of).collect();
        if let Err(e) = user.set_rules(from, to, &views) {
            return Ok(KeyPreview {
                error: e.to_string(),
                ..Default::default()
            });
        }
    }
    let keymap = user.keymap().map_err(|e| e.to_string())?;
    Ok(match preview(&keymap, from, to, &req.chord, ctx) {
        Ok(p) => KeyPreview {
            events: p
                .events
                .into_iter()
                .map(|(key, down)| KeyOut { key, down })
                .collect(),
            sent: p.sent,
            paste: p.paste,
            error: String::new(),
        },
        Err(e) => KeyPreview {
            error: e.to_string(),
            ..Default::default()
        },
    })
}

/// The keymap a session uses: built-in rules with the user's file applied. A file
/// that cannot be used is reported and ignored.
pub fn for_sessions(dir: &Path) -> (nexa_input::Keymap, Option<String>) {
    match UserKeymap::load(dir).and_then(|u| u.keymap()) {
        Ok(k) => (k, None),
        // The reason alone: the caller words it (a warning with a code).
        Err(e) => (nexa_input::Keymap::builtin(), Some(e.to_string())),
    }
}
