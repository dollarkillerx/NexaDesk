//! `nexa-service`: the NexaDesk Windows service (docs/WINDOWS_SERVICE.md §3).
//!
//! Installed in `C:\Program Files\NexaDesk Service\` beside a copy of `nexa-agent.exe`, it
//! runs as LocalSystem and starts that host, as SYSTEM, in the session of whoever asks on
//! its control pipe, so the host can see and use UAC prompts and the lock screen. It is a
//! launcher only: what the host does is the host's own business.
//!
//! ```text
//! nexa-service install     copy this program and nexa-agent.exe beside it to Program Files,
//!                          register and start the service (as administrator)
//! nexa-service uninstall   stop and remove it (as administrator)
//! nexa-service hello       whether the service is running
//! nexa-service start -- <host arguments>
//!                          start the host in this session through the service, and show
//!                          its output until it ends (for trying it; the app does this)
//! ```

#[cfg_attr(not(windows), allow(dead_code))]
mod proto;
#[cfg(windows)]
mod windows;

fn main() {
    #[cfg(windows)]
    let result = windows::main();
    #[cfg(not(windows))]
    let result: Result<(), String> = Err("the service is for Windows".into());
    if let Err(message) = result {
        eprintln!("nexa-service: {message}");
        std::process::exit(1);
    }
}
