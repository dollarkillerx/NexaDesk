//! Running under the service control manager: report the state, stop when told.

use std::{
    ffi::c_void,
    ptr,
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicPtr, Ordering::SeqCst},
    },
};

use windows_sys::{
    Win32::{
        Foundation::{
            ERROR_CALL_NOT_IMPLEMENTED, ERROR_FAILED_SERVICE_CONTROLLER_CONNECT, NO_ERROR,
        },
        System::Services::{
            RegisterServiceCtrlHandlerExW, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP,
            SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP,
            SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE,
            SERVICE_STOP_PENDING, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
            SetServiceStatus, StartServiceCtrlDispatcherW,
        },
    },
    core::PWSTR,
};

use super::{SERVICE_NAME, log::log, server, wide};

/// Set when the service is told to stop; the server and every host's watcher look at it.
pub static STOPPING: AtomicBool = AtomicBool::new(false);
static STATUS: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

fn name() -> &'static [u16] {
    static NAME: OnceLock<Vec<u16>> = OnceLock::new();
    NAME.get_or_init(|| wide(SERVICE_NAME))
}

fn report(state: SERVICE_STATUS_CURRENT_STATE, exit: u32) {
    let handle = STATUS.load(SeqCst);
    if handle.is_null() {
        return;
    }
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
        } else {
            0
        },
        dwWin32ExitCode: exit,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: if state == SERVICE_RUNNING || state == SERVICE_STOPPED {
            0
        } else {
            10_000
        },
    };
    // SAFETY: the handle RegisterServiceCtrlHandlerExW returned; a complete status.
    unsafe { SetServiceStatus(handle, &status) };
}

unsafe extern "system" fn handler(
    control: u32,
    _event: u32,
    _data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            report(SERVICE_STOP_PENDING, NO_ERROR);
            STOPPING.store(true, SeqCst);
            server::wake();
            NO_ERROR
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR,
        _ => ERROR_CALL_NOT_IMPLEMENTED,
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    // SAFETY: the service's NUL-terminated name, which lives for the process.
    let handle =
        unsafe { RegisterServiceCtrlHandlerExW(name().as_ptr(), Some(handler), ptr::null()) };
    if handle.is_null() {
        log("could not register with the service control manager");
        return;
    }
    STATUS.store(handle, SeqCst);
    report(SERVICE_START_PENDING, NO_ERROR);
    log("started");
    let result = server::serve(|| report(SERVICE_RUNNING, NO_ERROR));
    match &result {
        Ok(()) => log("stopped"),
        Err(e) => log(&format!("stopped: {e}")),
    }
    report(SERVICE_STOPPED, if result.is_ok() { NO_ERROR } else { 1 });
}

/// `nexa-service run`: what the service control manager starts.
pub fn run() -> Result<(), String> {
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: name().as_ptr().cast_mut(),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    // SAFETY: a table ending in an empty entry; returns once the service has stopped.
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT as i32) {
            return Err(
                "this is run by the service control manager; see `nexa-service install`".into(),
            );
        }
        return Err(e.to_string());
    }
    Ok(())
}
