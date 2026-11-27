//! Starts a program as SYSTEM inside a user's session (docs/WINDOWS_SERVICE.md §3), so it
//! can see and use the secure desktop (UAC prompts, the lock screen) of that session. It
//! takes the token of the session's own winlogon.exe, which is SYSTEM and already belongs
//! to the session (as RustDesk's `GetSessionUserTokenWin` does), and starts the program on
//! `winsta0\default` with it.
//!
//! Only a process running as SYSTEM may do this: the service will, and for now the
//! `run-in-session` subcommand does, started by a scheduled task that runs as SYSTEM.

use std::{ffi::c_void, os::windows::ffi::OsStrExt, path::Path, ptr};

use windows_sys::Win32::{
    Foundation::{CloseHandle, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE},
    Security::{
        DuplicateTokenEx, SECURITY_ATTRIBUTES, SecurityImpersonation, TOKEN_ALL_ACCESS,
        TokenPrimary,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_END, FILE_SHARE_READ, OPEN_ALWAYS,
        SetFilePointerEx,
    },
    System::{
        RemoteDesktop::{
            WTS_CURRENT_SERVER_HANDLE, WTS_PROCESS_INFOW, WTSEnumerateProcessesW, WTSFreeMemory,
            WTSGetActiveConsoleSessionId,
        },
        Threading::{
            CREATE_NEW_CONSOLE, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
            OpenProcess, OpenProcessToken, PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
            STARTF_USESTDHANDLES, STARTUPINFOW,
        },
    },
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Closes a handle when dropped.
struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: a handle this process opened and owns.
            unsafe { CloseHandle(self.0) };
        }
    }
}

fn last_error(what: &str) -> String {
    format!("{what}: {}", std::io::Error::last_os_error())
}

/// The session the console (the physical screen) is attached to.
pub fn active_console_session() -> Option<u32> {
    // SAFETY: no arguments.
    let id = unsafe { WTSGetActiveConsoleSessionId() };
    (id != u32::MAX).then_some(id)
}

/// The pid of winlogon.exe in `session`.
fn winlogon_in(session: u32) -> Result<u32, String> {
    let mut list: *mut WTS_PROCESS_INFOW = ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: `list` and `count` are written on success; freed below.
    if unsafe { WTSEnumerateProcessesW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut list, &mut count) }
        == 0
    {
        return Err(last_error("WTSEnumerateProcesses"));
    }
    // SAFETY: WTSEnumerateProcessesW returned `count` entries at `list`.
    let entries = unsafe { std::slice::from_raw_parts(list, count as usize) };
    let found = entries.iter().find_map(|p| {
        if p.SessionId != session || p.pProcessName.is_null() {
            return None;
        }
        // SAFETY: a NUL-terminated name owned by the list.
        let name = unsafe {
            let mut n = 0;
            while *p.pProcessName.add(n) != 0 {
                n += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(p.pProcessName, n))
        };
        name.eq_ignore_ascii_case("winlogon.exe")
            .then_some(p.ProcessId)
    });
    // SAFETY: the list WTSEnumerateProcessesW allocated.
    unsafe { WTSFreeMemory(list.cast()) };
    found.ok_or_else(|| format!("no winlogon.exe in session {session}"))
}

/// A primary SYSTEM token that belongs to `session`: its winlogon.exe's, duplicated.
fn session_system_token(session: u32) -> Result<Owned, String> {
    let pid = winlogon_in(session)?;
    // SAFETY: a plain open of another process; closed by `Owned`.
    let process = Owned(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) });
    if process.0.is_null() {
        return Err(last_error("OpenProcess(winlogon)"));
    }
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: an open process handle; `token` is written on success.
    if unsafe { OpenProcessToken(process.0, TOKEN_ALL_ACCESS, &mut token) } == 0 {
        return Err(last_error("OpenProcessToken(winlogon)"));
    }
    let token = Owned(token);
    let mut primary: HANDLE = ptr::null_mut();
    // SAFETY: duplicating an open token into a primary one; `primary` is written on success.
    if unsafe {
        DuplicateTokenEx(
            token.0,
            TOKEN_ALL_ACCESS,
            ptr::null(),
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        )
    } == 0
    {
        return Err(last_error("DuplicateTokenEx"));
    }
    Ok(Owned(primary))
}

/// Opens `log` for appending, inheritable, for the started program's output.
fn open_log(log: &Path) -> Result<Owned, String> {
    let path: Vec<u16> = log.as_os_str().encode_wide().chain(Some(0)).collect();
    let attrs = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 1,
    };
    // SAFETY: a NUL-terminated path; the handle is closed by `Owned`.
    let file = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ,
            &attrs,
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if file == INVALID_HANDLE_VALUE {
        return Err(last_error("opening the log"));
    }
    // SAFETY: an open file; moves to its end so earlier runs are kept.
    unsafe { SetFilePointerEx(file, 0, ptr::null_mut(), FILE_END) };
    Ok(Owned(file))
}

/// How the started program shows itself.
pub enum Output<'a> {
    /// A console window of its own on the session's desktop (to answer prompts there).
    Console,
    /// No window; standard output and error appended to this file.
    Log(&'a Path),
}

/// Starts `exe` with `args` as SYSTEM in `session`, on `winsta0\default`. Returns its pid.
/// The caller must run as SYSTEM.
pub fn run_in_session(
    session: u32,
    exe: &Path,
    args: &[String],
    output: Output<'_>,
) -> Result<u32, String> {
    let token = session_system_token(session)?;
    let mut command = format!("\"{}\"", exe.display());
    for a in args {
        command.push(' ');
        if a.is_empty() || a.contains([' ', '\t', '"']) {
            command.push('"');
            command.push_str(&a.replace('"', "\\\""));
            command.push('"');
        } else {
            command.push_str(a);
        }
    }
    let mut command = wide(&command);
    let mut desktop = wide("winsta0\\default");
    let mut startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        lpDesktop: desktop.as_mut_ptr(),
        ..Default::default()
    };
    let log = match output {
        Output::Console => None,
        Output::Log(path) => Some(open_log(path)?),
    };
    let mut flags = CREATE_UNICODE_ENVIRONMENT;
    if let Some(log) = &log {
        startup.dwFlags = STARTF_USESTDHANDLES;
        startup.hStdOutput = log.0;
        startup.hStdError = log.0;
        startup.hStdInput = ptr::null_mut();
        flags |= CREATE_NO_WINDOW;
    } else {
        flags |= CREATE_NEW_CONSOLE;
    }
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: a primary token, a writable NUL-terminated command line, and startup info
    // whose pointers live through the call. The environment is this process's (null).
    let ok = unsafe {
        CreateProcessAsUserW(
            token.0,
            ptr::null(),
            command.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            i32::from(log.is_some()),
            flags,
            ptr::null::<c_void>(),
            ptr::null(),
            &startup,
            &mut info,
        )
    };
    if ok == 0 {
        return Err(last_error("CreateProcessAsUser"));
    }
    // SAFETY: handles CreateProcessAsUserW returned; the process keeps running.
    unsafe {
        CloseHandle(info.hThread);
        CloseHandle(info.hProcess);
    }
    Ok(info.dwProcessId)
}
