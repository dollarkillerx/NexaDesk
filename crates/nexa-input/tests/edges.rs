//! Unusual release orders, repeats, reset, modes, and rule resolution.

mod common;

use common::*;
use nexa_input::hid::key;
use nexa_input::{Chord, ChordKey, FocusClass, Keymap, ModSet, Os, RewriteMode};

fn custom(body: &str) -> Harness {
    let text = format!("[[profile]]\nfrom = \"macos\"\nto = \"windows\"\n{body}");
    Harness::new(
        Keymap::parse(&text)
            .unwrap()
            .for_session(Os::Macos, Os::Windows),
    )
}

// ---- Release orders -----------------------------------------------------------

#[test]
fn shift_released_before_z_in_cmd_shift_z() {
    let mut h = Harness::mac_to_win();
    assert_eq!(h.down("Cmd"), "");
    assert_eq!(h.down("Shift"), "+LShift");
    assert_eq!(h.down("Z"), "-LShift +LCtrl +Y");
    assert_eq!(h.up("Shift"), "", "Shift is already up on the remote");
    assert_eq!(h.up("Z"), "-Y -LCtrl");
    assert_eq!(h.up("Cmd"), "");
    h.assert_idle();
}

#[test]
fn cmd_released_before_z_in_cmd_shift_z() {
    let mut h = Harness::mac_to_win();
    h.down("Cmd");
    h.down("Shift");
    h.down("Z");
    assert_eq!(h.up("Cmd"), "", "the chord owns Ctrl until Z is up");
    assert_eq!(h.up("Z"), "-Y -LCtrl +LShift");
    assert_eq!(h.up("Shift"), "-LShift");
    h.assert_idle();
}

#[test]
fn cmd_released_before_tab() {
    let mut h = Harness::mac_to_win();
    h.down("Cmd");
    assert_eq!(h.down("Tab"), "+LAlt +Tab");
    assert_eq!(h.up("Cmd"), "-LAlt");
    assert_eq!(h.up("Tab"), "-Tab");
    h.assert_idle();
}

#[test]
fn alt_released_before_tab_on_windows_to_mac() {
    let mut h = Harness::win_to_mac();
    h.down("Alt");
    assert_eq!(h.down("Tab"), "-LAlt +LGui +Tab");
    assert_eq!(h.up("Alt"), "-LGui");
    assert_eq!(h.up("Tab"), "-Tab");
    h.assert_idle();
}

#[test]
fn shift_tab_reverses_the_switcher_while_alt_stays_down() {
    let mut h = Harness::mac_to_win();
    h.down("Cmd");
    assert_eq!(h.down("Tab"), "+LAlt +Tab");
    assert_eq!(h.up("Tab"), "-Tab");
    assert_eq!(h.down("Shift"), "+LShift");
    assert_eq!(h.down("Tab"), "+Tab");
    assert_eq!(h.up("Tab"), "-Tab");
    assert_eq!(h.up("Shift"), "-LShift");
    assert_eq!(h.up("Cmd"), "-LAlt");
    h.assert_idle();
}

#[test]
fn modifier_pressed_during_a_rewritten_chord_waits_for_its_release() {
    let mut h = Harness::mac_to_win();
    h.down("Cmd");
    assert_eq!(h.down("C"), "+LCtrl +C");
    assert_eq!(h.down("Shift"), "");
    assert_eq!(h.up("C"), "-C -LCtrl +LShift");
    assert_eq!(h.up("Shift"), "-LShift");
    assert_eq!(h.up("Cmd"), "");
    h.assert_idle();
}

#[test]
fn modifier_pressed_during_a_passed_through_key_is_forwarded() {
    let mut h = Harness::mac_to_win();
    assert_eq!(h.down("W"), "+W");
    assert_eq!(h.down("Shift"), "+LShift");
    assert_eq!(h.up("W"), "-W");
    assert_eq!(h.up("Shift"), "-LShift");
    h.assert_idle();
}

#[test]
fn rollover_between_two_chords_restores_the_later_one() {
    let mut h = Harness::mac_to_win();
    h.down("Cmd");
    assert_eq!(h.down("C"), "+LCtrl +C");
    assert_eq!(h.down("X"), "+X");
    assert_eq!(h.up("C"), "-C", "X still needs Ctrl");
    assert_eq!(h.repeat("X"), "*X");
    assert_eq!(h.up("X"), "-X -LCtrl");
    assert_eq!(h.up("Cmd"), "");
    h.assert_idle();
}

#[test]
fn cmd_released_during_click_drag_keeps_ctrl_until_button_up() {
    let mut h = Harness::mac_to_win();
    h.down("Cmd");
    assert_eq!(h.button(1, true), "+LCtrl +btn1");
    assert_eq!(h.up("Cmd"), "");
    assert_eq!(h.button(1, false), "-btn1 -LCtrl");
    h.assert_idle();
}

// ---- Repeat -------------------------------------------------------------------

#[test]
fn repeat_replays_and_is_never_rematched() {
    let mut h = Harness::mac_to_win();
    h.down("Cmd");
    h.down("Shift");
    assert_eq!(h.down("Z"), "-LShift +LCtrl +Y");
    assert_eq!(h.repeat("Z"), "*Y");
    h.up("Shift");
    assert_eq!(
        h.repeat("Z"),
        "*Y",
        "Shift going up must not turn this into Ctrl+Z"
    );
    h.up("Cmd");
    assert_eq!(h.repeat("Z"), "*Y", "nor Cmd going up into a bare Z");
    assert_eq!(h.up("Z"), "-Y -LCtrl");
    h.assert_idle();
}

#[test]
fn repeat_does_not_emit_another_paste_marker() {
    let mut h = Harness::mac_to_win();
    h.down("Cmd");
    assert_eq!(h.down("V"), "paste +LCtrl +V");
    assert_eq!(h.repeat("V"), "*V");
    h.up("V");
    h.up("Cmd");
    h.assert_idle();
}

#[test]
fn repeat_of_a_sequence_replays_the_whole_sequence() {
    let mut h = Harness::mac_to_win();
    h.down("Cmd");
    let tapped = "+LShift +Home -Home -LShift +Delete -Delete";
    assert_eq!(h.down("Backspace"), tapped);
    assert_eq!(h.repeat("Backspace"), tapped);
    assert_eq!(h.up("Backspace"), "");
    h.up("Cmd");
    h.assert_idle();
}

#[test]
fn repeat_of_tab_during_a_hold() {
    let mut h = Harness::mac_to_win();
    h.down("Cmd");
    assert_eq!(h.down("Tab"), "+LAlt +Tab");
    assert_eq!(h.repeat("Tab"), "*Tab");
    assert_eq!(h.up("Tab"), "-Tab");
    assert_eq!(h.up("Cmd"), "-LAlt");
    h.assert_idle();
}

#[test]
fn modifier_auto_repeat_is_ignored() {
    let mut h = Harness::win_to_mac();
    assert_eq!(h.down("Shift"), "+LShift");
    assert_eq!(h.repeat("Shift"), "");
    assert_eq!(
        h.down("Shift"),
        "",
        "a second down without an up is a repeat"
    );
    assert_eq!(h.up("Shift"), "-LShift");
    h.assert_idle();
}

#[test]
fn stray_events_produce_nothing() {
    let mut h = Harness::mac_to_win();
    assert_eq!(h.up("C"), "");
    assert_eq!(
        h.up("Cmd"),
        "",
        "must not tap Win for a Cmd we never saw go down"
    );
    assert_eq!(h.up("Shift"), "");
    assert_eq!(h.repeat("C"), "");
    assert_eq!(h.button(1, false), "");
    h.assert_idle();
}

// ---- Tap -----------------------------------------------------------------------

#[test]
fn cmd_is_not_alone_if_any_other_key_joined_it() {
    let mut h = Harness::mac_to_win();
    h.down("Cmd");
    assert_eq!(h.down("Shift"), "+LShift");
    assert_eq!(h.up("Shift"), "-LShift");
    assert_eq!(h.up("Cmd"), "");
    h.assert_idle();

    h.down("Shift");
    h.down("Cmd");
    h.up("Cmd");
    assert_eq!(h.up("Shift"), "-LShift");
    h.assert_idle();
}

#[test]
fn cmd_used_in_a_chord_does_not_tap_and_the_next_press_can() {
    let mut h = Harness::mac_to_win();
    h.down("Cmd");
    h.down("C");
    h.up("C");
    assert_eq!(h.up("Cmd"), "");
    h.down("Cmd");
    assert_eq!(h.up("Cmd"), "+LGui -LGui");
    h.assert_idle();
}

#[test]
fn right_hand_modifiers_match_the_same_rules() {
    let mut h = Harness::mac_to_win();
    h.down("RCmd");
    assert_eq!(h.down("C"), "+LCtrl +C");
    h.up("C");
    assert_eq!(h.up("RCmd"), "");
    h.assert_idle();

    h.down("RCmd");
    assert_eq!(h.up("RCmd"), "+LGui -LGui");
    h.assert_idle();

    assert_eq!(h.chord(&["ROption", "Left"]), "+LCtrl +Left -Left -LCtrl");
}

#[test]
fn option_alone_produces_nothing() {
    let mut h = Harness::mac_to_win();
    assert_eq!(h.down("Option"), "");
    assert_eq!(h.up("Option"), "");
    h.assert_idle();
}

#[test]
fn unmatched_key_under_a_deferred_modifier_passes_through_physically() {
    let mut h = Harness::mac_to_win();
    h.down("Option");
    assert_eq!(h.down("A"), "+LAlt +A");
    assert_eq!(h.up("A"), "-A -LAlt");
    assert_eq!(h.up("Option"), "");
    h.assert_idle();
}

// ---- Wildcard ------------------------------------------------------------------

#[test]
fn wildcard_carries_extra_modifiers() {
    assert_eq!(
        Harness::mac_to_win().chord(&["Cmd", "Shift", "P"]),
        "+LShift +LCtrl +P -P -LCtrl -LShift"
    );
}

#[test]
fn wildcard_covers_punctuation_and_enter() {
    for key in ["/", "=", ",", "Enter"] {
        let name = common::hid(key);
        let expected = format!("+LCtrl +{name} -{name} -LCtrl");
        assert_eq!(
            Harness::mac_to_win().chord(&["Cmd", key]),
            expected,
            "Cmd+{key}"
        );
    }
}

#[test]
fn wildcard_does_not_capture_ctrl_tab_or_function_keys() {
    for key in ["Tab", "F5", "Esc", "Space"] {
        let expected = format!("+LCtrl +{key} -{key} -LCtrl");
        assert_eq!(
            Harness::win_to_mac().chord(&["Ctrl", key]),
            expected,
            "Ctrl+{key}"
        );
    }
}

// ---- Rule priority --------------------------------------------------------------

#[test]
fn priority_is_ctx_exact_then_exact_then_wildcard() {
    let body = r#"
        [[profile.rule]]
        when = "Ctrl+*"
        then = "Shift+*"

        [[profile.rule]]
        when = "Ctrl+K"
        then = "F1"

        [[profile.rule]]
        ctx = "terminal"
        when = "Ctrl+K"
        then = "F2"
    "#;
    assert_eq!(
        custom(body).chord(&["Ctrl", "J"]),
        "+LCtrl -LCtrl +LShift +J -J -LShift +LCtrl -LCtrl"
    );
    assert_eq!(
        custom(body).chord(&["Ctrl", "K"]),
        "+LCtrl -LCtrl +F1 -F1 +LCtrl -LCtrl"
    );
    assert_eq!(
        custom(body).terminal().chord(&["Ctrl", "K"]),
        "+LCtrl -LCtrl +F2 -F2 +LCtrl -LCtrl"
    );
}

#[test]
fn ctx_rule_for_another_class_does_not_apply() {
    let body = r#"
        [[profile.rule]]
        ctx = "ide"
        when = "F1"
        then = "F2"
    "#;
    assert_eq!(custom(body).chord(&["F1"]), "+F1 -F1");
    let mut ide = custom(body);
    ide.ctx = FocusClass::Ide;
    assert_eq!(ide.chord(&["F1"]), "+F2 -F2");
}

#[test]
fn later_rule_wins_a_tie() {
    let body = r#"
        [[profile.rule]]
        when = "F1"
        then = "F2"

        [[profile.rule]]
        when = "F1"
        then = "F3"
    "#;
    assert_eq!(custom(body).chord(&["F1"]), "+F3 -F3");
}

#[test]
fn context_is_decided_at_press_time() {
    let mut h = Harness::win_to_mac();
    h.down("Ctrl");
    h.ctx = FocusClass::Terminal;
    assert_eq!(h.down("C"), "+C");
    h.ctx = FocusClass::Default;
    assert_eq!(h.repeat("C"), "*C");
    assert_eq!(h.up("C"), "-C");
    assert_eq!(h.up("Ctrl"), "-LCtrl");
    h.assert_idle();
}

// ---- Reference counting ---------------------------------------------------------

#[test]
fn two_physical_keys_mapping_to_one_output_share_it() {
    let body = r#"
        [[profile.rule]]
        when = "F1"
        then = "Home"
    "#;
    let mut h = custom(body);
    assert_eq!(h.down("F1"), "+Home");
    assert_eq!(h.down("Home"), "");
    assert_eq!(h.up("F1"), "");
    assert_eq!(h.up("Home"), "-Home");
    h.assert_idle();
}

#[test]
fn output_is_not_rematched_as_input() {
    // Win→Mac rewrites physical Home, but Mac→Win emits Home as output only.
    assert_eq!(Harness::mac_to_win().chord(&["Cmd", "Left"]), "+Home -Home");
}

// ---- Reset and modes --------------------------------------------------------------

#[test]
fn reset_releases_keys_then_modifiers_and_ignores_later_releases() {
    let mut h = Harness::mac_to_win();
    h.down("Cmd");
    h.down("V");
    h.button(2, true);
    assert_eq!(h.reset(), "-btn2 -V -LCtrl");
    h.assert_idle();
    assert_eq!(h.up("V"), "");
    assert_eq!(h.up("Cmd"), "", "no tap after a reset");
    assert_eq!(h.button(2, false), "");
    h.assert_idle();
}

#[test]
fn reset_during_a_hold_clears_it() {
    let mut h = Harness::mac_to_win();
    h.down("Cmd");
    h.down("Tab");
    assert_eq!(h.reset(), "-Tab -LAlt");
    assert_eq!(h.up("Tab"), "");
    assert_eq!(h.up("Cmd"), "");
    h.assert_idle();
    assert_eq!(h.chord(&["A"]), "+A -A");
}

#[test]
fn passthrough_mode_turns_all_rewriting_off() {
    let mut h = Harness::mac_to_win();
    h.rewriter.set_mode(RewriteMode::Passthrough);
    assert_eq!(h.chord(&["Cmd", "C"]), "+LGui +C -C -LGui");
    assert_eq!(h.chord(&["Cmd", "V"]), "+LGui +V -V -LGui");
    assert_eq!(h.chord(&["Cmd"]), "+LGui -LGui");
    assert_eq!(h.chord(&["Option", "Left"]), "+LAlt +Left -Left -LAlt");
    h.down("Cmd");
    assert_eq!(h.button(1, true), "+btn1");
    h.button(1, false);
    h.up("Cmd");
    h.assert_idle();
    let mut h = Harness::win_to_mac();
    h.rewriter.set_mode(RewriteMode::Passthrough);
    assert_eq!(h.chord(&["PrintScreen"]), "+PrintScreen -PrintScreen");
}

#[test]
fn mode_change_releases_what_is_held() {
    let mut h = Harness::mac_to_win();
    h.down("Cmd");
    h.down("C");
    let released = render(&h.rewriter.set_mode(RewriteMode::Passthrough));
    assert_eq!(released, "-C -LCtrl");
    assert_eq!(h.up("C"), "");
    assert_eq!(h.rewriter.mode(), RewriteMode::Passthrough);
    assert_eq!(render(&h.rewriter.set_mode(RewriteMode::Passthrough)), "");
}

#[test]
fn pressed_keys_reports_the_remote_state_for_key_state_sync() {
    let mut h = Harness::mac_to_win();
    h.down("Cmd");
    h.down("V");
    let pressed: Vec<_> = h.rewriter.pressed_keys().collect();
    assert_eq!(pressed, vec![key::V, key::LCTRL]);
    assert_eq!(h.rewriter.out_mods(), ModSet::single(key::LCTRL));
}

// ---- Parsing ------------------------------------------------------------------

fn parse(text: &str) -> Chord {
    text.parse().unwrap()
}

#[test]
fn chord_strings_parse_with_every_modifier_spelling() {
    let cmd_shift_z = Chord {
        mods: [key::LGUI, key::LSHIFT].into_iter().collect(),
        key: ChordKey::Key(key::Z),
    };
    assert_eq!(parse("Cmd+Shift+Z"), cmd_shift_z);
    assert_eq!(parse("win + shift + z"), cmd_shift_z);
    assert_eq!(parse("Gui+Shift+Z"), cmd_shift_z);
    assert_eq!(parse("Command+Shift+Z"), cmd_shift_z);
    assert_eq!(parse("Option+Left").mods, parse("Alt+Left").mods);
    assert_eq!(parse("RGui+A").mods, ModSet::single(key::RGUI));
    assert_eq!(parse("Control+A").mods, ModSet::single(key::LCTRL));
}

#[test]
fn chord_strings_parse_keys_wildcards_and_pointer_events() {
    assert_eq!(parse("Ctrl+*").key, ChordKey::Any);
    assert_eq!(
        parse("Shift+Home"),
        Chord {
            mods: ModSet::single(key::LSHIFT),
            key: ChordKey::Key(key::HOME)
        }
    );
    assert_eq!(parse("Cmd").key, ChordKey::None);
    assert_eq!(parse("Cmd+Click").key, ChordKey::Click);
    assert_eq!(parse("Cmd+Wheel").key, ChordKey::Wheel);
    assert_eq!(parse("Cmd+←").key, ChordKey::Key(key::LEFT));
    assert_eq!(parse("Ctrl+F4").key, ChordKey::Key(key::F4));
    assert_eq!(parse("4").key, ChordKey::Key(key::D4));
}

#[test]
fn jis_and_ime_keys_match_section_7_1() {
    let cases = [
        ("Intl1", 0x87),
        ("Ro", 0x87),
        ("Intl2", 0x88),
        ("Intl3", 0x89),
        ("Yen", 0x89),
        ("Intl4", 0x8A),
        ("変換", 0x8A),
        ("Intl5", 0x8B),
        ("無変換", 0x8B),
        ("Lang1", 0x90),
        ("かな", 0x90),
        ("Lang2", 0x91),
        ("英数", 0x91),
    ];
    for (name, usage) in cases {
        assert_eq!(
            parse(name).key,
            ChordKey::Key(nexa_input::Hid::key(usage)),
            "{name}"
        );
    }
}

#[test]
fn wire_format_is_page_shifted_usage() {
    assert_eq!(key::A.0, 0x0007_0004);
    assert_eq!(key::LCTRL.0, 0x0007_00E0);
    assert_eq!(key::RGUI.0, 0x0007_00E7);
    assert!(key::RGUI.is_modifier());
    assert!(!key::A.is_modifier());
}

#[test]
fn modset_collapses_right_onto_left() {
    let set: ModSet = [key::RGUI, key::LSHIFT].into_iter().collect();
    assert_eq!(
        set.collapsed(),
        [key::LGUI, key::LSHIFT].into_iter().collect()
    );
    assert_eq!(set.len(), 2);
}

#[test]
fn bad_chord_strings_are_rejected() {
    assert!("Cmd+".parse::<Chord>().is_err());
    assert!("Cmd+Nope".parse::<Chord>().is_err());
    assert!("A+B".parse::<Chord>().is_err());
}

#[test]
fn invalid_rules_are_rejected() {
    let cases = [
        (
            "no modifier wildcard",
            r#"when = "*"
then = "F1""#,
        ),
        (
            "star without star",
            r#"when = "F1"
then = "Ctrl+*""#,
        ),
        (
            "hold not in then",
            r#"when = "Cmd+Tab"
then = "Alt+Tab"
hold = "Ctrl""#,
        ),
        (
            "empty then",
            r#"when = "F1"
then = []"#,
        ),
        (
            "tap not deferred",
            r#"when = "Ctrl"
then = "Win""#,
        ),
        (
            "click to key",
            r#"when = "Cmd+Click"
then = "Ctrl+A""#,
        ),
        (
            "unknown field",
            r#"when = "F1"
then = "F2"
bogus = 1"#,
        ),
        (
            "unknown tag",
            r#"when = "F1"
then = "F2"
tag = "nope""#,
        ),
    ];
    for (name, rule) in cases {
        let text =
            format!("[[profile]]\nfrom = \"macos\"\nto = \"windows\"\n[[profile.rule]]\n{rule}");
        assert!(Keymap::parse(&text).is_err(), "{name} should be rejected");
    }
    assert!(Keymap::parse("[[profile]]\nfrom = \"beos\"\nto = \"windows\"").is_err());
}

#[test]
fn unknown_os_pairs_fall_back_to_passthrough() {
    let mut h = Harness::session(Os::Linux, Os::Windows);
    assert_eq!(h.chord(&["Cmd", "C"]), "+LGui +C -C -LGui");
}
