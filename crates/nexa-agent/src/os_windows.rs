//! What the agent's entry point needs from Windows (docs/WINDOWS.md): secrets in
//! DPAPI-protected files, %APPDATA% for data, physical pixels everywhere, the media
//! capabilities, and a main thread that only waits.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering::Relaxed},
    },
    time::Duration,
};

use nexa_agent::{Agent, Platform};
use nexa_auth::{FileKeyStore, KeyStore};
use nexa_wire::control::{Caps, Os};

pub const OS: Os = Os::Windows;

/// The fixed password's Argon2id verifier, encrypted for this Windows user (DPAPI).
pub fn password_store() -> Box<dyn KeyStore> {
    let dir = default_data_dir().unwrap_or_else(|_| PathBuf::from("."));
    Box::new(FileKeyStore::new(dir.join("host-password")))
}

/// This device's key, encrypted for this Windows user (DPAPI).
pub fn device_key_store(data_dir: &Path) -> Box<dyn KeyStore> {
    Box::new(FileKeyStore::new(data_dir.join("device-key")))
}

pub fn default_data_dir() -> Result<PathBuf, String> {
    let appdata = std::env::var_os("APPDATA").ok_or("APPDATA is not set")?;
    Ok(Path::new(&appdata).join("NexaDesk"))
}

pub fn default_downloads() -> Result<PathBuf, String> {
    let profile = std::env::var_os("USERPROFILE").ok_or("USERPROFILE is not set")?;
    Ok(Path::new(&profile).join("Downloads").join("NexaDesk"))
}

pub fn computer_name() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| "Windows PC".to_owned())
}

/// Names for the browser certificate beyond the addresses: none needed.
pub fn local_names() -> Vec<String> {
    Vec::new()
}

/// Physical pixels everywhere: desktop duplication, the cursor and SendInput agree only
/// when the process is per-monitor DPI aware.
pub fn prepare() {
    use windows_sys::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    // SAFETY: called once, before any window or display call.
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

/// Nothing to grant on Windows: capture and input need no permission. A process can
/// still not reach the secure desktop (UAC, the lock screen): docs/WINDOWS.md stage 6.
pub fn check_permissions() -> Result<(), String> {
    Ok(())
}

pub fn caps() -> Caps {
    let caps = nexa_media::windows::caps();
    // Which encoder this PC got, for picture problems (docs/WINDOWS.md stage 2).
    eprintln!(
        "nexa-agent: video encoder: {}",
        if nexa_media::windows::hardware_h264() {
            "GPU H.264, up to 4096x2304 at 120 fps"
        } else {
            "software H.264 (no working GPU encoder), up to 1920x1080 at 60 fps"
        }
    );
    caps
}

pub fn platform() -> Arc<dyn Platform> {
    Arc::new(crate::real::RealPlatform)
}

/// Resolves on Ctrl+C (or the console closing).
pub async fn stop_signal() {
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Nothing runs on the main thread: it waits for `quit`.
pub fn main_loop(_agent: &Arc<Agent>, quit: &AtomicBool) {
    while !quit.load(Relaxed) {
        std::thread::sleep(Duration::from_millis(200));
    }
}
