//! Property test for the §7.2 invariant.

mod common;

use std::collections::BTreeSet;

use common::hid;
use nexa_input::{
    FocusClass, Hid, Keymap, Os, Output, PhysButton, PhysKey, PhysWheel, RewriteMode, Rewriter,
};
use proptest::prelude::*;

const KEYS: &[&str] = &[
    "LGui",
    "RGui",
    "LAlt",
    "RAlt",
    "LCtrl",
    "RCtrl",
    "LShift",
    "RShift",
    "Tab",
    "C",
    "V",
    "Y",
    "Z",
    "Q",
    "4",
    "S",
    "Left",
    "Up",
    "Home",
    "End",
    "Backspace",
    "Esc",
    "F4",
    "PrintScreen",
];

#[derive(Clone, Debug)]
enum Action {
    Key {
        index: usize,
        down: bool,
        repeat: bool,
    },
    Button {
        button: u32,
        down: bool,
    },
    Wheel,
    Reset,
    Ctx(FocusClass),
    Mode(RewriteMode),
}

fn action() -> impl Strategy<Value = Action> {
    let key = (0..KEYS.len(), any::<bool>(), prop::bool::weighted(0.15)).prop_map(
        |(index, down, repeat)| Action::Key {
            index,
            down,
            repeat,
        },
    );
    prop_oneof![
        30 => key,
        4 => (1u32..=2, any::<bool>()).prop_map(|(button, down)| Action::Button { button, down }),
        2 => Just(Action::Wheel),
        1 => Just(Action::Reset),
        2 => prop_oneof![
            Just(FocusClass::Default),
            Just(FocusClass::Terminal),
            Just(FocusClass::Ide),
        ].prop_map(Action::Ctx),
        1 => prop_oneof![
            Just(RewriteMode::Rewrite),
            Just(RewriteMode::Passthrough),
        ].prop_map(Action::Mode),
    ]
}

/// What the remote side would see; panics on a malformed stream.
#[derive(Default)]
struct Remote {
    keys: BTreeSet<Hid>,
    buttons: BTreeSet<u32>,
}

impl Remote {
    fn apply(&mut self, outputs: &[Output]) {
        for output in outputs {
            match *output {
                Output::Key {
                    hid,
                    down: true,
                    repeat: false,
                } => {
                    assert!(self.keys.insert(hid), "{hid} pressed twice");
                }
                Output::Key {
                    hid,
                    down: true,
                    repeat: true,
                } => {
                    assert!(self.keys.contains(&hid), "{hid} repeated while up");
                }
                Output::Key {
                    hid, down: false, ..
                } => {
                    assert!(self.keys.remove(&hid), "{hid} released while up");
                }
                Output::Button { button, down: true } => {
                    assert!(self.buttons.insert(button), "button {button} pressed twice");
                }
                Output::Button {
                    button,
                    down: false,
                } => {
                    assert!(
                        self.buttons.remove(&button),
                        "button {button} released while up"
                    );
                }
                Output::Wheel { .. } | Output::PasteMarker => {}
            }
        }
    }
}

fn run(profile: (Os, Os), actions: &[Action], release_order: &[u64]) {
    let mut rewriter = Rewriter::new(Keymap::builtin().for_session(profile.0, profile.1));
    let mut remote = Remote::default();
    let mut ctx = FocusClass::Default;
    let mut phys_keys: BTreeSet<usize> = BTreeSet::new();
    let mut phys_buttons: BTreeSet<u32> = BTreeSet::new();

    for action in actions {
        match *action {
            Action::Key {
                index,
                down,
                repeat,
            } => {
                let hid = hid(KEYS[index]);
                if down {
                    phys_keys.insert(index);
                } else {
                    phys_keys.remove(&index);
                }
                let output = rewriter.on_key(
                    PhysKey {
                        hid,
                        down,
                        repeat: repeat && down,
                    },
                    ctx,
                );
                remote.apply(&output);
            }
            Action::Button { button, down } => {
                if down {
                    phys_buttons.insert(button);
                } else {
                    phys_buttons.remove(&button);
                }
                remote.apply(&rewriter.on_button(PhysButton { button, down }));
            }
            Action::Wheel => remote.apply(&rewriter.on_wheel(PhysWheel { dx: 0, dy: 120 })),
            Action::Reset => {
                let output = rewriter.reset();
                remote.apply(&output);
                assert!(
                    remote.keys.is_empty() && remote.buttons.is_empty(),
                    "reset left keys down"
                );
            }
            Action::Ctx(next) => ctx = next,
            Action::Mode(mode) => {
                let output = rewriter.set_mode(mode);
                remote.apply(&output);
            }
        }
        assert_eq!(
            rewriter.pressed_keys().collect::<BTreeSet<_>>(),
            remote.keys
        );
    }

    // Release whatever is still physically down, in an arbitrary order.
    let mut remaining: Vec<(u64, Option<usize>, Option<u32>)> = phys_keys
        .into_iter()
        .map(|i| (Some(i), None))
        .chain(phys_buttons.into_iter().map(|b| (None, Some(b))))
        .enumerate()
        .map(|(n, (k, b))| (release_order.get(n).copied().unwrap_or(0), k, b))
        .collect();
    remaining.sort_by_key(|(order, _, _)| *order);
    for (_, key, button) in remaining {
        if let Some(index) = key {
            let event = PhysKey {
                hid: hid(KEYS[index]),
                down: false,
                repeat: false,
            };
            remote.apply(&rewriter.on_key(event, ctx));
        }
        if let Some(button) = button {
            remote.apply(&rewriter.on_button(PhysButton {
                button,
                down: false,
            }));
        }
    }

    assert_eq!(rewriter.pressed_keys().count(), 0, "out_refs not empty");
    assert!(rewriter.out_mods().is_empty(), "out_mods not empty");
    assert!(
        remote.keys.is_empty(),
        "remote keys stuck: {:?}",
        remote.keys
    );
    assert!(
        remote.buttons.is_empty(),
        "remote buttons stuck: {:?}",
        remote.buttons
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn nothing_sticks_and_no_key_is_released_unpressed(
        actions in prop::collection::vec(action(), 0..120),
        release_order in prop::collection::vec(any::<u64>(), 32),
        which in 0..4usize,
    ) {
        let profile = [
            (Os::Macos, Os::Windows),
            (Os::Windows, Os::Macos),
            (Os::Macos, Os::Macos),
            (Os::Linux, Os::Windows),
        ][which];
        run(profile, &actions, &release_order);
    }
}
