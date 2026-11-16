//! What the agent's entry point needs from macOS: the Keychain, the data directory,
//! permissions, the media capabilities, and the main thread's run loop.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering::Relaxed},
    },
};

use nexa_agent::{Agent, Platform};
use nexa_auth::{KeyStore, KeychainStore};
use nexa_wire::control::{Caps, FocusContext, Os};

pub const OS: Os = Os::Macos;

/// Where the fixed password's Argon2id verifier is kept: never the password itself.
pub fn password_store() -> Box<dyn KeyStore> {
    Box::new(KeychainStore::new("app.nexadesk", "host-password"))
}

pub fn device_key_store(_data_dir: &Path) -> Box<dyn KeyStore> {
    Box::new(KeychainStore::new("app.nexadesk", "agent-device-key"))
}

pub fn default_data_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(Path::new(&home).join("Library/Application Support/NexaDesk"))
}

pub fn default_downloads() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(Path::new(&home).join("Downloads/NexaDesk"))
}

fn scutil(key: &str) -> Option<String> {
    std::process::Command::new("scutil")
        .args(["--get", key])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

pub fn computer_name() -> String {
    scutil("ComputerName").unwrap_or_else(|| "Mac".to_owned())
}

/// Names for the browser certificate beyond the addresses: this Mac's `.local` name.
pub fn local_names() -> Vec<String> {
    scutil("LocalHostName")
        .map(|n| format!("{n}.local"))
        .into_iter()
        .collect()
}

/// The agent runs from inside the app's bundle, so its first AppKit call (the pasteboard,
/// screen capture) would give it a Dock icon of its own beside the app's. As an accessory
/// it stays out of the Dock and the app switcher.
pub fn prepare() {
    use objc2::{
        msg_send,
        runtime::{AnyClass, AnyObject},
    };
    #[link(name = "AppKit", kind = "framework")]
    unsafe extern "C" {}
    /// NSApplicationActivationPolicyAccessory.
    const ACCESSORY: isize = 1;
    let Some(class) = AnyClass::get(c"NSApplication") else {
        return;
    };
    // SAFETY: called on the main thread before anything else touches AppKit;
    // `sharedApplication` and `setActivationPolicy:` take and return what is declared.
    unsafe {
        let app: *mut AnyObject = msg_send![class, sharedApplication];
        if !app.is_null() {
            let _: bool = msg_send![app, setActivationPolicy: ACCESSORY];
        }
    }
}

pub fn check_permissions() -> Result<(), String> {
    use nexa_input::platform::macos::accessibility_granted;
    use nexa_media::macos::has_screen_capture_permission;
    if !has_screen_capture_permission() {
        return Err(
            "Screen Recording permission is missing. Grant it to this program (or the terminal \
             running it) in System Settings > Privacy & Security > Screen Recording, then start again."
                .to_owned(),
        );
    }
    if !accessibility_granted() {
        eprintln!(
            "nexa-agent: Accessibility permission is missing, so viewers cannot be given keyboard or mouse control."
        );
    }
    Ok(())
}

/// §3.5: probed with real sessions once per OS build and GPU, then cached.
pub fn caps() -> Caps {
    use nexa_media::macos::{cached_caps, probe_caps};
    match nexa_media::caps_cache::default_path() {
        Some(path) => cached_caps(&path),
        None => probe_caps(),
    }
}

pub fn platform() -> Arc<dyn Platform> {
    Arc::new(crate::real::RealPlatform)
}

/// Resolves on SIGTERM or SIGINT.
pub async fn stop_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

/// The main thread belongs to AppKit: FocusWatcher notifications and pasteboard
/// promise callbacks both arrive through its run loop. Returns once `quit` is set.
pub fn main_loop(agent: &Arc<Agent>, quit: &AtomicBool) {
    use nexa_input::platform::macos::FocusWatcher;
    use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSRunLoop};
    let extra_terminals: Vec<String> = Vec::new();
    if let Some((class, id)) = FocusWatcher::current(&extra_terminals) {
        agent.set_focus(FocusContext {
            cls: class as i32,
            app_id: id,
        });
    }
    let watcher = {
        let agent = agent.clone();
        FocusWatcher::start(extra_terminals, move |class, id| {
            agent.set_focus(FocusContext {
                cls: class as i32,
                app_id: id.to_owned(),
            });
        })
    };
    let run_loop = NSRunLoop::currentRunLoop();
    while !quit.load(Relaxed) {
        let until = NSDate::dateWithTimeIntervalSinceNow(0.2);
        // SAFETY: the mode is a Foundation constant and the date is valid.
        unsafe {
            run_loop.runMode_beforeDate(NSDefaultRunLoopMode, &until);
        }
    }
    drop(watcher);
}
