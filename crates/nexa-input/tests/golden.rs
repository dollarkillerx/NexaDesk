//! One test per row of the DESIGN.md §8.3 tables.

mod common;

use common::*;
use nexa_input::Os;

// ---- Mac controlling Windows -------------------------------------------------

#[test]
fn mw_cmd_letter_or_digit_becomes_ctrl_same_key() {
    for key in LETTERS
        .iter()
        .chain(DIGITS)
        .filter(|k| **k != "Q" && **k != "V")
    {
        let expected = format!("+LCtrl +{key} -{key} -LCtrl");
        assert_eq!(
            Harness::mac_to_win().chord(&["Cmd", key]),
            expected,
            "Cmd+{key}"
        );
    }
}

#[test]
fn mw_cmd_v_is_paste_tagged_and_marker_leads_the_batch() {
    assert_eq!(
        Harness::mac_to_win().chord(&["Cmd", "V"]),
        "paste +LCtrl +V -V -LCtrl"
    );
}

#[test]
fn mw_ctrl_any_key_is_unchanged() {
    for key in ["C", "Z", "Tab", "Left", "Backspace", "F5", "1"] {
        let expected = format!("+LCtrl +{key} -{key} -LCtrl");
        assert_eq!(
            Harness::mac_to_win().chord(&["Ctrl", key]),
            expected,
            "Ctrl+{key}"
        );
    }
}

#[test]
fn mw_cmd_shift_z_becomes_ctrl_y() {
    assert_eq!(
        Harness::mac_to_win().chord(&["Cmd", "Shift", "Z"]),
        "+LShift -LShift +LCtrl +Y -Y -LCtrl +LShift -LShift"
    );
}

#[test]
fn mw_cmd_tab_becomes_alt_tab_and_holds_alt_until_cmd_up() {
    let mut h = Harness::mac_to_win();
    assert_eq!(h.down("Cmd"), "");
    assert_eq!(h.down("Tab"), "+LAlt +Tab");
    assert_eq!(h.up("Tab"), "-Tab");
    assert_eq!(h.down("Tab"), "+Tab");
    assert_eq!(h.up("Tab"), "-Tab");
    assert_eq!(h.up("Cmd"), "-LAlt");
    h.assert_idle();
}

#[test]
fn mw_cmd_q_becomes_alt_f4() {
    assert_eq!(
        Harness::mac_to_win().chord(&["Cmd", "Q"]),
        "+LAlt +F4 -F4 -LAlt"
    );
}

#[test]
fn mw_cmd_left_right_become_home_end() {
    assert_eq!(Harness::mac_to_win().chord(&["Cmd", "Left"]), "+Home -Home");
    assert_eq!(Harness::mac_to_win().chord(&["Cmd", "Right"]), "+End -End");
}

#[test]
fn mw_cmd_up_down_become_ctrl_home_end() {
    assert_eq!(
        Harness::mac_to_win().chord(&["Cmd", "Up"]),
        "+LCtrl +Home -Home -LCtrl"
    );
    assert_eq!(
        Harness::mac_to_win().chord(&["Cmd", "Down"]),
        "+LCtrl +End -End -LCtrl"
    );
}

#[test]
fn mw_option_left_right_become_ctrl_left_right() {
    assert_eq!(
        Harness::mac_to_win().chord(&["Option", "Left"]),
        "+LCtrl +Left -Left -LCtrl"
    );
    assert_eq!(
        Harness::mac_to_win().chord(&["Option", "Right"]),
        "+LCtrl +Right -Right -LCtrl"
    );
}

#[test]
fn mw_option_backspace_becomes_ctrl_backspace() {
    assert_eq!(
        Harness::mac_to_win().chord(&["Option", "Backspace"]),
        "+LCtrl +Backspace -Backspace -LCtrl"
    );
}

#[test]
fn mw_cmd_backspace_is_a_shift_home_delete_sequence() {
    let mut h = Harness::mac_to_win();
    assert_eq!(h.down("Cmd"), "");
    assert_eq!(
        h.down("Backspace"),
        "+LShift +Home -Home -LShift +Delete -Delete"
    );
    assert_eq!(h.up("Backspace"), "");
    assert_eq!(h.up("Cmd"), "");
    h.assert_idle();
}

#[test]
fn mw_cmd_option_esc_becomes_ctrl_shift_esc() {
    assert_eq!(
        Harness::mac_to_win().chord(&["Cmd", "Option", "Esc"]),
        "+LCtrl +LShift +Esc -Esc -LCtrl -LShift"
    );
}

#[test]
fn mw_cmd_shift_4_becomes_win_shift_s() {
    assert_eq!(
        Harness::mac_to_win().chord(&["Cmd", "Shift", "4"]),
        "+LShift +LGui +S -S -LGui -LShift"
    );
}

#[test]
fn mw_cmd_tapped_alone_becomes_win() {
    let mut h = Harness::mac_to_win();
    assert_eq!(h.down("Cmd"), "");
    assert_eq!(h.up("Cmd"), "+LGui -LGui");
    h.assert_idle();
}

#[test]
fn mw_cmd_click_becomes_ctrl_click() {
    let mut h = Harness::mac_to_win();
    assert_eq!(h.down("Cmd"), "");
    assert_eq!(h.button(1, true), "+LCtrl +btn1");
    assert_eq!(h.button(1, false), "-btn1 -LCtrl");
    assert_eq!(h.up("Cmd"), "");
    h.assert_idle();
}

#[test]
fn mw_cmd_wheel_becomes_ctrl_wheel() {
    let mut h = Harness::mac_to_win();
    assert_eq!(h.down("Cmd"), "");
    assert_eq!(h.wheel(), "+LCtrl wheel");
    assert_eq!(h.wheel(), "wheel");
    assert_eq!(h.up("Cmd"), "-LCtrl");
    h.assert_idle();
}

// ---- Windows controlling Mac -------------------------------------------------

#[test]
fn wm_ctrl_letter_or_digit_becomes_cmd_same_key() {
    for key in LETTERS
        .iter()
        .chain(DIGITS)
        .filter(|k| **k != "Y" && **k != "V")
    {
        let expected = format!("+LCtrl -LCtrl +LGui +{key} -{key} -LGui +LCtrl -LCtrl");
        assert_eq!(
            Harness::win_to_mac().chord(&["Ctrl", key]),
            expected,
            "Ctrl+{key}"
        );
    }
}

#[test]
fn wm_ctrl_v_is_paste_tagged() {
    assert_eq!(
        Harness::win_to_mac().chord(&["Ctrl", "V"]),
        "+LCtrl paste -LCtrl +LGui +V -V -LGui +LCtrl -LCtrl"
    );
}

#[test]
fn wm_ctrl_y_becomes_cmd_shift_z() {
    assert_eq!(
        Harness::win_to_mac().chord(&["Ctrl", "Y"]),
        "+LCtrl -LCtrl +LShift +LGui +Z -Z -LShift -LGui +LCtrl -LCtrl"
    );
}

#[test]
fn wm_alt_tab_becomes_cmd_tab_and_keeps_cmd_for_the_switcher() {
    let mut h = Harness::win_to_mac();
    assert_eq!(h.down("Alt"), "+LAlt");
    assert_eq!(h.down("Tab"), "-LAlt +LGui +Tab");
    assert_eq!(h.up("Tab"), "-Tab");
    assert_eq!(h.down("Tab"), "+Tab");
    assert_eq!(h.up("Tab"), "-Tab");
    assert_eq!(h.up("Alt"), "-LGui");
    h.assert_idle();
}

#[test]
fn wm_alt_f4_becomes_cmd_q() {
    assert_eq!(
        Harness::win_to_mac().chord(&["Alt", "F4"]),
        "+LAlt -LAlt +LGui +Q -Q -LGui +LAlt -LAlt"
    );
}

#[test]
fn wm_home_end_become_cmd_left_right() {
    assert_eq!(
        Harness::win_to_mac().chord(&["Home"]),
        "+LGui +Left -Left -LGui"
    );
    assert_eq!(
        Harness::win_to_mac().chord(&["End"]),
        "+LGui +Right -Right -LGui"
    );
}

#[test]
fn wm_ctrl_home_end_become_cmd_up_down() {
    assert_eq!(
        Harness::win_to_mac().chord(&["Ctrl", "Home"]),
        "+LCtrl -LCtrl +LGui +Up -Up -LGui +LCtrl -LCtrl"
    );
    assert_eq!(
        Harness::win_to_mac().chord(&["Ctrl", "End"]),
        "+LCtrl -LCtrl +LGui +Down -Down -LGui +LCtrl -LCtrl"
    );
}

#[test]
fn wm_ctrl_left_right_become_option_left_right() {
    assert_eq!(
        Harness::win_to_mac().chord(&["Ctrl", "Left"]),
        "+LCtrl -LCtrl +LAlt +Left -Left -LAlt +LCtrl -LCtrl"
    );
    assert_eq!(
        Harness::win_to_mac().chord(&["Ctrl", "Right"]),
        "+LCtrl -LCtrl +LAlt +Right -Right -LAlt +LCtrl -LCtrl"
    );
}

#[test]
fn wm_ctrl_backspace_becomes_option_backspace() {
    assert_eq!(
        Harness::win_to_mac().chord(&["Ctrl", "Backspace"]),
        "+LCtrl -LCtrl +LAlt +Backspace -Backspace -LAlt +LCtrl -LCtrl"
    );
}

#[test]
fn wm_ctrl_shift_esc_becomes_cmd_option_esc() {
    assert_eq!(
        Harness::win_to_mac().chord(&["Ctrl", "Shift", "Esc"]),
        "+LCtrl +LShift -LCtrl -LShift +LAlt +LGui +Esc -Esc -LAlt -LGui +LCtrl +LShift -LShift -LCtrl"
    );
}

#[test]
fn wm_win_shift_s_becomes_cmd_shift_4() {
    assert_eq!(
        Harness::win_to_mac().chord(&["Win", "Shift", "S"]),
        "+LGui +LShift +4 -4 -LShift -LGui"
    );
}

#[test]
fn wm_print_screen_becomes_cmd_shift_3() {
    assert_eq!(
        Harness::win_to_mac().chord(&["PrintScreen"]),
        "+LShift +LGui +3 -3 -LShift -LGui"
    );
}

#[test]
fn wm_ctrl_click_becomes_cmd_click() {
    let mut h = Harness::win_to_mac();
    assert_eq!(h.down("Ctrl"), "+LCtrl");
    assert_eq!(h.button(1, true), "-LCtrl +LGui +btn1");
    assert_eq!(h.button(1, false), "-btn1 -LGui +LCtrl");
    assert_eq!(h.up("Ctrl"), "-LCtrl");
    h.assert_idle();
}

// ---- Terminal exception (§8.4) ------------------------------------------------

#[test]
fn wm_terminal_ctrl_c_stays_ctrl_c() {
    assert_eq!(
        Harness::win_to_mac().terminal().chord(&["Ctrl", "C"]),
        "+LCtrl +C -C -LCtrl"
    );
}

#[test]
fn wm_terminal_ctrl_shift_c_maps_to_cmd_c() {
    assert_eq!(
        Harness::win_to_mac()
            .terminal()
            .chord(&["Ctrl", "Shift", "C"]),
        "+LCtrl +LShift -LCtrl -LShift +LGui +C -C -LGui +LCtrl +LShift -LShift -LCtrl"
    );
}

#[test]
fn wm_terminal_ctrl_shift_v_maps_to_cmd_v_with_paste() {
    assert_eq!(
        Harness::win_to_mac()
            .terminal()
            .chord(&["Ctrl", "Shift", "V"]),
        "+LCtrl +LShift paste -LCtrl -LShift +LGui +V -V -LGui +LCtrl +LShift -LShift -LCtrl"
    );
}

#[test]
fn wm_terminal_other_control_keys_are_not_turned_into_cmd() {
    for key in ["D", "W", "Z", "R", "L"] {
        let expected = format!("+LCtrl +{key} -{key} -LCtrl");
        assert_eq!(
            Harness::win_to_mac().terminal().chord(&["Ctrl", key]),
            expected,
            "Ctrl+{key}"
        );
    }
}

#[test]
fn wm_terminal_context_applies_only_in_a_terminal() {
    assert_eq!(
        Harness::win_to_mac().chord(&["Ctrl", "C"]),
        "+LCtrl -LCtrl +LGui +C -C -LGui +LCtrl -LCtrl"
    );
}

// ---- Same-OS sessions ---------------------------------------------------------

#[test]
fn same_os_sessions_forward_everything_unchanged() {
    for os in [Os::Macos, Os::Windows] {
        let mut h = Harness::session(os, os);
        assert_eq!(h.chord(&["Cmd", "C"]), "+LGui +C -C -LGui");
        assert_eq!(h.chord(&["Ctrl", "Y"]), "+LCtrl +Y -Y -LCtrl");
        assert_eq!(h.chord(&["Alt", "Tab"]), "+LAlt +Tab -Tab -LAlt");
        assert_eq!(h.chord(&["Home"]), "+Home -Home");
        assert_eq!(h.chord(&["Cmd"]), "+LGui -LGui");
    }
}

#[test]
fn same_os_sessions_still_tag_paste_for_the_gate() {
    assert_eq!(
        Harness::session(Os::Macos, Os::Macos).chord(&["Cmd", "V"]),
        "+LGui paste +V -V -LGui"
    );
    assert_eq!(
        Harness::session(Os::Windows, Os::Windows).chord(&["Ctrl", "V"]),
        "+LCtrl paste +V -V -LCtrl"
    );
    assert_eq!(
        Harness::session(Os::Windows, Os::Windows).chord(&["Shift", "Insert"]),
        "+LShift paste +Insert -Insert -LShift"
    );
    // Win+V is the Windows clipboard history, not a paste.
    assert_eq!(
        Harness::session(Os::Windows, Os::Windows).chord(&["Cmd", "V"]),
        "+LGui +V -V -LGui"
    );
    assert_eq!(
        Harness::session(Os::Macos, Os::Macos).chord(&["Ctrl", "V"]),
        "+LCtrl +V -V -LCtrl"
    );
}

#[test]
fn navigation_selection_variants_map_both_ways() {
    let mw = |keys: &[&str]| Harness::mac_to_win().chord(keys);
    assert!(
        mw(&["Cmd", "Shift", "Left"]).contains("+LShift +Home"),
        "{}",
        mw(&["Cmd", "Shift", "Left"])
    );
    assert!(mw(&["Cmd", "Shift", "Right"]).contains("+LShift +End"));
    assert!(
        mw(&["Option", "Shift", "Left"]).contains("+Left")
            && mw(&["Option", "Shift", "Left"]).contains("LCtrl")
    );

    let wm = |keys: &[&str]| Harness::session(Os::Windows, Os::Macos).chord(keys);
    assert!(
        wm(&["Shift", "Home"]).contains("+LGui +Left"),
        "{}",
        wm(&["Shift", "Home"])
    );
    assert!(wm(&["Shift", "End"]).contains("+LGui +Right"));
    assert!(wm(&["Ctrl", "Shift", "Home"]).contains("+LGui +Up"));
}
