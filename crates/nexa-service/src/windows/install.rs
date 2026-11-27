//! `install` and `uninstall` (docs/WINDOWS_SERVICE.md §3), run as administrator.
//!
//! The service runs only the copies in `C:\Program Files\NexaDesk Service\`, which only
//! administrators can change: never a program in a user's folder (where NexaDesk itself is
//! installed), which any program of that user could replace to become SYSTEM.

use std::{
    io,
    path::{Path, PathBuf},
    ptr, thread,
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::{ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_DOES_NOT_EXIST},
    System::{
        Registry::{HKEY_LOCAL_MACHINE, RegDeleteKeyW},
        Services::{
            ChangeServiceConfig2W, CloseServiceHandle, ControlService, CreateServiceW,
            DeleteService, OpenSCManagerW, OpenServiceW, QueryServiceStatus, SC_ACTION,
            SC_ACTION_NONE, SC_ACTION_RESTART, SC_HANDLE, SC_MANAGER_ALL_ACCESS,
            SERVICE_ALL_ACCESS, SERVICE_AUTO_START, SERVICE_CONFIG_DESCRIPTION,
            SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONTROL_STOP, SERVICE_DESCRIPTIONW,
            SERVICE_ERROR_NORMAL, SERVICE_FAILURE_ACTIONSW, SERVICE_RUNNING, SERVICE_STATUS,
            SERVICE_STOPPED, SERVICE_WIN32_OWN_PROCESS, StartServiceW,
        },
    },
};

use super::{
    SERVICE_NAME,
    sas::{POLICY_KEY, POLICY_VALUE, delete_value, read_dword, write_dword},
    wide,
};

/// Where install remembers that it set SoftwareSASGeneration, so uninstall removes only
/// what it added.
const OWN_KEY: &str = r"SOFTWARE\NexaDesk\Service";
const SET_SAS: &str = "SetSoftwareSASGeneration";

struct Sc(SC_HANDLE);

impl Drop for Sc {
    fn drop(&mut self) {
        // SAFETY: a handle from OpenSCManagerW, OpenServiceW or CreateServiceW.
        unsafe { CloseServiceHandle(self.0) };
    }
}

fn manager() -> Result<Sc, String> {
    // SAFETY: the local computer's service manager.
    let h = unsafe { OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_ALL_ACCESS) };
    if h.is_null() {
        let e = io::Error::last_os_error();
        return Err(if e.kind() == io::ErrorKind::PermissionDenied {
            "run this as administrator".into()
        } else {
            format!("the service manager: {e}")
        });
    }
    Ok(Sc(h))
}

fn open(manager: &Sc) -> Result<Option<Sc>, String> {
    let name = wide(SERVICE_NAME);
    // SAFETY: an open manager and a NUL-terminated name.
    let h = unsafe { OpenServiceW(manager.0, name.as_ptr(), SERVICE_ALL_ACCESS) };
    if h.is_null() {
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST as i32) {
            return Ok(None);
        }
        return Err(format!("the service: {e}"));
    }
    Ok(Some(Sc(h)))
}

fn state(service: &Sc) -> Option<u32> {
    let mut status = SERVICE_STATUS::default();
    // SAFETY: an open service; `status` is written.
    (unsafe { QueryServiceStatus(service.0, &mut status) } != 0).then_some(status.dwCurrentState)
}

fn wait_for(service: &Sc, wanted: u32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if state(service) == Some(wanted) {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn stop(service: &Sc) -> Result<(), String> {
    if state(service) == Some(SERVICE_STOPPED) {
        return Ok(());
    }
    let mut status = SERVICE_STATUS::default();
    // SAFETY: an open service; `status` is written.
    unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut status) };
    if !wait_for(service, SERVICE_STOPPED, Duration::from_secs(20)) {
        return Err("the service did not stop".into());
    }
    // The process may still be leaving after it reported stopped.
    thread::sleep(Duration::from_millis(500));
    Ok(())
}

pub fn install_dir() -> PathBuf {
    let base = std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
    base.join("NexaDesk Service")
}

/// Copies `from` to `to`, a few times if the old one is still in use.
fn copy(from: &Path, to: &Path) -> Result<(), String> {
    let mut tries = 0;
    loop {
        match std::fs::copy(from, to) {
            Ok(_) => return Ok(()),
            Err(_) if tries < 10 => {
                tries += 1;
                thread::sleep(Duration::from_millis(500));
            }
            Err(e) => {
                return Err(format!(
                    "copying {} to {}: {e}",
                    from.display(),
                    to.display()
                ));
            }
        }
    }
}

/// `nexa-service install`: copies, registers, starts. Run again to update.
pub fn install() -> Result<(), String> {
    let manager = manager()?;
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let agent = me.with_file_name("nexa-agent.exe");
    if !agent.exists() {
        return Err(format!("{} is missing", agent.display()));
    }
    let dir = install_dir();
    let target = dir.join("nexa-service.exe");
    let existing = open(&manager)?;
    if let Some(service) = &existing {
        stop(service)?;
    }
    if me.parent() != Some(dir.as_path()) {
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        copy(&me, &target)?;
        copy(&agent, &dir.join("nexa-agent.exe"))?;
    }

    let service = match existing {
        Some(s) => s,
        None => {
            let (name, display) = (wide(SERVICE_NAME), wide("NexaDesk Service"));
            let command = wide(&format!("\"{}\" run", target.display()));
            // SAFETY: NUL-terminated strings; runs as LocalSystem (no account given).
            let h = unsafe {
                CreateServiceW(
                    manager.0,
                    name.as_ptr(),
                    display.as_ptr(),
                    SERVICE_ALL_ACCESS,
                    SERVICE_WIN32_OWN_PROCESS,
                    SERVICE_AUTO_START,
                    SERVICE_ERROR_NORMAL,
                    command.as_ptr(),
                    ptr::null(),
                    ptr::null_mut(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                )
            };
            if h.is_null() {
                return Err(format!(
                    "creating the service: {}",
                    io::Error::last_os_error()
                ));
            }
            Sc(h)
        }
    };
    let mut text = wide(
        "Lets NexaDesk show and control UAC prompts and the lock screen when you allow \
         someone to control this computer.",
    );
    let description = SERVICE_DESCRIPTIONW {
        lpDescription: text.as_mut_ptr(),
    };
    // Restarted after 3 s the first two times, then left alone; counts reset after a day.
    let mut actions = [
        SC_ACTION {
            Type: SC_ACTION_RESTART,
            Delay: 3000,
        },
        SC_ACTION {
            Type: SC_ACTION_RESTART,
            Delay: 3000,
        },
        SC_ACTION {
            Type: SC_ACTION_NONE,
            Delay: 0,
        },
    ];
    let failure = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: 24 * 60 * 60,
        lpRebootMsg: ptr::null_mut(),
        lpCommand: ptr::null_mut(),
        cActions: actions.len() as u32,
        lpsaActions: actions.as_mut_ptr(),
    };
    // SAFETY: an open service and structures that live through the calls.
    unsafe {
        ChangeServiceConfig2W(
            service.0,
            SERVICE_CONFIG_DESCRIPTION,
            (&description as *const SERVICE_DESCRIPTIONW).cast(),
        );
        ChangeServiceConfig2W(
            service.0,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            (&failure as *const SERVICE_FAILURE_ACTIONSW).cast(),
        );
    }

    // Services may send Ctrl+Alt+Del, unless a policy already says something.
    if read_dword(POLICY_KEY, POLICY_VALUE).is_none() {
        write_dword(POLICY_KEY, POLICY_VALUE, 1)?;
        write_dword(OWN_KEY, SET_SAS, 1)?;
    }

    // SAFETY: an open service; no arguments.
    if unsafe { StartServiceW(service.0, 0, ptr::null()) } == 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(ERROR_SERVICE_ALREADY_RUNNING as i32) {
            return Err(format!("starting the service: {e}"));
        }
    }
    if !wait_for(&service, SERVICE_RUNNING, Duration::from_secs(20)) {
        return Err("the service did not start; see service.log beside it".into());
    }
    println!("installed and running: {}", dir.display());
    Ok(())
}

/// `nexa-service uninstall`: stops and removes the service, its files, and the
/// Ctrl+Alt+Del policy if install set it.
pub fn uninstall() -> Result<(), String> {
    let manager = manager()?;
    if let Some(service) = open(&manager)? {
        stop(&service)?;
        // SAFETY: an open service.
        if unsafe { DeleteService(service.0) } == 0 {
            return Err(format!(
                "removing the service: {}",
                io::Error::last_os_error()
            ));
        }
    }
    if read_dword(OWN_KEY, SET_SAS) == Some(1) {
        // Only if nobody changed it since.
        if read_dword(POLICY_KEY, POLICY_VALUE) == Some(1) {
            delete_value(POLICY_KEY, POLICY_VALUE);
        }
        delete_value(OWN_KEY, SET_SAS);
        for key in [OWN_KEY, r"SOFTWARE\NexaDesk"] {
            let k = wide(key);
            // SAFETY: a NUL-terminated name; fails harmlessly while it has subkeys.
            unsafe { RegDeleteKeyW(HKEY_LOCAL_MACHINE, k.as_ptr()) };
        }
    }
    let dir = install_dir();
    if dir.exists()
        && let Err(e) = std::fs::remove_dir_all(&dir)
    {
        return Err(format!(
            "the service is removed, but {} could not be deleted: {e}",
            dir.display()
        ));
    }
    println!("removed");
    Ok(())
}
