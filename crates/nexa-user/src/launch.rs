//! Starting a program as SYSTEM inside a user's session (docs/WINDOWS_SERVICE.md §3), so it
//! can see and use that session's secure desktop (UAC prompts, the lock screen). It takes
//! the token of the session's own winlogon.exe, which is SYSTEM and already belongs to the
//! session (as RustDesk's `GetSessionUserTokenWin` does), and starts the program on
//! `winsta0\default` with it.
//!
//! Only a process running as SYSTEM may do this: the NexaDesk service, and the agent's
//! `run-in-session` test subcommand when a scheduled task runs it as SYSTEM.

use std::{
    ffi::OsString,
    fs::File,
    io,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::Path,
    ptr,
};

use windows_sys::Win32::{
    Foundation::{
        GENERIC_WRITE, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
    },
    Security::{
        DuplicateTokenEx, SECURITY_ATTRIBUTES, SecurityImpersonation, TOKEN_ALL_ACCESS,
        TokenPrimary,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_END, FILE_SHARE_READ, OPEN_ALWAYS,
        SetFilePointerEx,
    },
    System::{
        Pipes::CreatePipe,
        RemoteDesktop::{
            WTS_CURRENT_SERVER_HANDLE, WTS_PROCESS_INFOW, WTSEnumerateProcessesW, WTSFreeMemory,
            WTSGetActiveConsoleSessionId,
        },
        Threading::{
            CREATE_NEW_CONSOLE, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
            DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
            InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcess,
            OpenProcessToken, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION,
            PROCESS_QUERY_LIMITED_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
            UpdateProcThreadAttribute,
        },
    },
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// The session the console (the physical screen) is attached to.
pub fn active_console_session() -> Option<u32> {
    // SAFETY: no arguments.
    let id = unsafe { WTSGetActiveConsoleSessionId() };
    (id != u32::MAX).then_some(id)
}

/// The pid of winlogon.exe in `session`.
fn winlogon_in(session: u32) -> io::Result<u32> {
    let mut list: *mut WTS_PROCESS_INFOW = ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: `list` and `count` are written on success; freed below.
    if unsafe { WTSEnumerateProcessesW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut list, &mut count) }
        == 0
    {
        return Err(io::Error::last_os_error());
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
    found.ok_or_else(|| io::Error::other(format!("no winlogon.exe in session {session}")))
}

/// A primary SYSTEM token that belongs to `session`: its winlogon.exe's, duplicated.
fn session_system_token(session: u32) -> io::Result<OwnedHandle> {
    let pid = winlogon_in(session)?;
    // SAFETY: a plain open of another process.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: just opened; owned from here on.
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: an open process handle; `token` is written on success.
    if unsafe { OpenProcessToken(process.as_raw_handle(), TOKEN_ALL_ACCESS, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: just opened; owned from here on.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut primary: HANDLE = ptr::null_mut();
    // SAFETY: duplicating an open token into a primary one; `primary` is written on success.
    if unsafe {
        DuplicateTokenEx(
            token.as_raw_handle(),
            TOKEN_ALL_ACCESS,
            ptr::null(),
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a token handle DuplicateTokenEx returned.
    Ok(unsafe { OwnedHandle::from_raw_handle(primary) })
}

/// `path` opened for appending, for a started program's output.
pub fn open_append(path: &Path) -> io::Result<OwnedHandle> {
    let wide_path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let attrs = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 0,
    };
    // SAFETY: a NUL-terminated path; the handle is owned below.
    let file = unsafe {
        CreateFileW(
            wide_path.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ,
            &attrs,
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if file == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: an open file; moves to its end so what was there is kept.
    unsafe { SetFilePointerEx(file, 0, ptr::null_mut(), FILE_END) };
    // SAFETY: just opened; owned from here on.
    Ok(unsafe { OwnedHandle::from_raw_handle(file) })
}

/// A pipe for a started program's output: this end to read, and the end to give it.
pub fn output_pipe() -> io::Result<(File, OwnedHandle)> {
    let (mut read, mut write): (HANDLE, HANDLE) = (ptr::null_mut(), ptr::null_mut());
    // SAFETY: both ends are written on success and owned below.
    if unsafe { CreatePipe(&mut read, &mut write, ptr::null(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both were just created.
    Ok(unsafe {
        (
            File::from_raw_handle(read),
            OwnedHandle::from_raw_handle(write),
        )
    })
}

/// Where a started program's standard output and error go.
pub enum Output<'a> {
    /// A console window of its own on the session's desktop (to answer prompts there).
    Console,
    /// No window; both go to this file or pipe.
    To(&'a OwnedHandle),
}

/// A started program.
pub struct Child {
    pub process: OwnedHandle,
    pub pid: u32,
}

/// One argument quoted for a Windows command line (as CommandLineToArgvW reads it).
fn quote(arg: &str, line: &mut String) {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\u{b}', '"']) {
        line.push_str(arg);
        return;
    }
    line.push('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                line.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                line.push('"');
                backslashes = 0;
            }
            _ => {
                line.extend(std::iter::repeat_n('\\', backslashes));
                line.push(c);
                backslashes = 0;
            }
        }
    }
    line.extend(std::iter::repeat_n('\\', backslashes * 2));
    line.push('"');
}

/// This process's environment with `extra` set, as a block for CREATE_UNICODE_ENVIRONMENT.
fn environment(extra: &[(&str, &str)]) -> Vec<u16> {
    let mut vars: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(k, _)| {
            !extra
                .iter()
                .any(|(name, _)| k.to_string_lossy().eq_ignore_ascii_case(name))
        })
        .collect();
    vars.extend(extra.iter().map(|(k, v)| ((*k).into(), (*v).into())));
    // The block is sorted by name, ignoring case.
    vars.sort_by_key(|(k, _)| k.to_string_lossy().to_uppercase());
    let mut block = Vec::new();
    for (k, v) in vars {
        block.extend(k.encode_wide());
        block.push(u16::from(b'='));
        block.extend(v.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

/// Starts `exe` with `args` as SYSTEM in `session`, on `winsta0\default`, with this
/// process's environment and `env` added. The caller must run as SYSTEM. Only the
/// output handle is inherited, whatever else this process has open.
pub fn launch_in_session(
    session: u32,
    exe: &Path,
    args: &[String],
    env: &[(&str, &str)],
    output: Output<'_>,
) -> io::Result<Child> {
    let token = session_system_token(session)?;
    let mut line = String::new();
    quote(&exe.to_string_lossy(), &mut line);
    for a in args {
        line.push(' ');
        quote(a, &mut line);
    }
    let mut command = wide(&line);
    let mut desktop = wide("winsta0\\default");
    let mut environment = environment(env);

    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.lpDesktop = desktop.as_mut_ptr();
    let mut flags = CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT;
    let mut inherit: Vec<HANDLE> = Vec::new();
    match output {
        Output::Console => flags |= CREATE_NEW_CONSOLE,
        Output::To(handle) => {
            let h = handle.as_raw_handle();
            // SAFETY: a handle this process owns; inheritable only for this start.
            if unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
                return Err(io::Error::last_os_error());
            }
            inherit.push(h);
            startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            startup.StartupInfo.hStdOutput = h;
            startup.StartupInfo.hStdError = h;
            startup.StartupInfo.hStdInput = ptr::null_mut();
            flags |= CREATE_NO_WINDOW;
        }
    }

    // The list of handles the program inherits: only `inherit`.
    let mut list_size = 0usize;
    // SAFETY: the first call only reports the size needed.
    unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut list_size) };
    let mut list = vec![0u8; list_size];
    let attrs: LPPROC_THREAD_ATTRIBUTE_LIST = list.as_mut_ptr().cast();
    // SAFETY: `list` is as large as asked for and outlives the process creation.
    if unsafe { InitializeProcThreadAttributeList(attrs, 1, 0, &mut list_size) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let result = (|| {
        if !inherit.is_empty() {
            // SAFETY: the handles live through the call; the size is the array's.
            if unsafe {
                UpdateProcThreadAttribute(
                    attrs,
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    inherit.as_ptr().cast(),
                    size_of_val(inherit.as_slice()),
                    ptr::null_mut(),
                    ptr::null(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            startup.lpAttributeList = attrs;
        }
        let mut info = PROCESS_INFORMATION::default();
        // SAFETY: a primary token, a writable NUL-terminated command line, an environment
        // block and startup info whose pointers live through the call.
        let ok = unsafe {
            CreateProcessAsUserW(
                token.as_raw_handle(),
                ptr::null(),
                command.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                i32::from(!inherit.is_empty()),
                flags,
                environment.as_mut_ptr().cast(),
                ptr::null(),
                &startup.StartupInfo,
                &mut info,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(info)
    })();
    // SAFETY: initialized above; not needed once the process exists.
    unsafe { DeleteProcThreadAttributeList(attrs) };
    for h in &inherit {
        // SAFETY: back to not inheritable, so later starts do not hand it on.
        unsafe { SetHandleInformation(*h, HANDLE_FLAG_INHERIT, 0) };
    }
    let info = result?;
    // SAFETY: handles CreateProcessAsUserW returned; the thread's is not needed.
    unsafe {
        drop(OwnedHandle::from_raw_handle(info.hThread));
        Ok(Child {
            process: OwnedHandle::from_raw_handle(info.hProcess),
            pid: info.dwProcessId,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::quote;

    fn q(a: &str) -> String {
        let mut s = String::new();
        quote(a, &mut s);
        s
    }

    #[test]
    fn quoting_matches_command_line_to_argv() {
        assert_eq!(q("plain"), "plain");
        assert_eq!(q(""), "\"\"");
        assert_eq!(q("two words"), "\"two words\"");
        assert_eq!(q(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(q(r"C:\dir with space\"), r#""C:\dir with space\\""#);
        assert_eq!(q(r"C:\no\space"), r"C:\no\space");
    }
}
