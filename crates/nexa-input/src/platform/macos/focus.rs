//! Frontmost-application tracking (§7.3).

use std::ptr::NonNull;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_app_kit::{
    NSRunningApplication, NSWorkspace, NSWorkspaceApplicationKey,
    NSWorkspaceDidActivateApplicationNotification,
};
use objc2_foundation::{NSNotification, NSNotificationCenter};

use crate::FocusClass;

/// Built-in terminals (§7.3). Alacritty has shipped under two identifiers.
pub const TERMINAL_BUNDLE_IDS: &[&str] = &[
    "com.apple.Terminal",
    "com.googlecode.iterm2",
    "net.kovidgoyal.kitty",
    "com.mitchellh.ghostty",
    "org.alacritty",
    "io.alacritty",
    "com.github.wez.wezterm",
];

/// Classifies an application. `extra` is the user's additions to the list.
pub fn classify_bundle_id<S: AsRef<str>>(bundle_id: &str, extra: &[S]) -> FocusClass {
    let is_terminal =
        TERMINAL_BUNDLE_IDS.contains(&bundle_id) || extra.iter().any(|id| id.as_ref() == bundle_id);
    if is_terminal {
        FocusClass::Terminal
    } else {
        FocusClass::Default
    }
}

fn bundle_id_of(app: &NSRunningApplication) -> String {
    app.bundleIdentifier()
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn app_from_note(note: &NSNotification) -> Option<Retained<NSRunningApplication>> {
    let info = note.userInfo()?;
    // SAFETY: the value for this key is documented to be an NSRunningApplication.
    let value = info.objectForKey(unsafe { NSWorkspaceApplicationKey })?;
    value.downcast::<NSRunningApplication>().ok()
}

/// Reports the frontmost app's class on every change.
///
/// Notifications arrive on the main thread, so that thread's run loop has to be
/// running.
pub struct FocusWatcher {
    center: Retained<NSNotificationCenter>,
    observer: Retained<ProtocolObject<dyn NSObjectProtocol>>,
}

impl FocusWatcher {
    /// `on_change` receives the class and the bundle id of each newly active
    /// application.
    pub fn start(
        extra_terminals: Vec<String>,
        on_change: impl Fn(FocusClass, &str) + 'static,
    ) -> Self {
        let workspace = NSWorkspace::sharedWorkspace();
        let center = workspace.notificationCenter();
        let block = RcBlock::new(move |note: NonNull<NSNotification>| {
            // SAFETY: the center passes a valid notification for the call.
            let Some(app) = app_from_note(unsafe { note.as_ref() }) else {
                return;
            };
            let id = bundle_id_of(&app);
            on_change(classify_bundle_id(&id, &extra_terminals), &id);
        });
        // SAFETY: the block has the documented signature; a nil queue runs it
        // on the posting thread.
        let observer = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(NSWorkspaceDidActivateApplicationNotification),
                None,
                None,
                &block,
            )
        };
        Self { center, observer }
    }

    /// The application that is frontmost right now, for the initial state.
    pub fn current(extra_terminals: &[String]) -> Option<(FocusClass, String)> {
        let app = NSWorkspace::sharedWorkspace().frontmostApplication()?;
        let id = bundle_id_of(&app);
        let class = classify_bundle_id(&id, extra_terminals);
        Some((class, id))
    }
}

impl Drop for FocusWatcher {
    fn drop(&mut self) {
        // SAFETY: `observer` is the token the center returned when it was added.
        unsafe { self.center.removeObserver(self.observer.as_ref()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_terminals_are_terminals() {
        for id in TERMINAL_BUNDLE_IDS {
            assert_eq!(
                classify_bundle_id::<&str>(id, &[]),
                FocusClass::Terminal,
                "{id}"
            );
        }
    }

    #[test]
    fn other_apps_are_default() {
        for id in ["com.apple.Safari", "com.microsoft.VSCode", ""] {
            assert_eq!(
                classify_bundle_id::<&str>(id, &[]),
                FocusClass::Default,
                "{id}"
            );
        }
    }

    #[test]
    fn user_additions_count() {
        assert_eq!(
            classify_bundle_id("com.microsoft.VSCode", &["com.microsoft.VSCode"]),
            FocusClass::Terminal
        );
    }

    #[test]
    fn matching_is_exact() {
        assert_eq!(
            classify_bundle_id::<&str>("com.apple.Terminal.helper", &[]),
            FocusClass::Default
        );
    }
}
