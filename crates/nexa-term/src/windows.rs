//! PowerShell on a ConPTY. The shell and everything it starts run in a job object that
//! kills them all when the [`Pty`] is dropped, so nothing outlives the session.
//!
//! A host running as SYSTEM starts the shell as the person signed in to its session
//! (`nexa_user`), with their environment and in their folder: whoever connects gets the
//! user's command line, never SYSTEM's. With nobody signed in there is no shell.

use std::{
    ffi::{OsStr, c_void},
    fs::File,
    io::{self, Write},
    os::windows::{
        ffi::OsStrExt,
        io::{FromRawHandle, OwnedHandle},
    },
    path::PathBuf,
    ptr::{null, null_mut},
    sync::Mutex,
};

use windows_sys::Win32::{
    Foundation::{HANDLE, INVALID_HANDLE_VALUE, STILL_ACTIVE},
    System::{
        Console::{COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole},
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        },
        Pipes::CreatePipe,
        Threading::{
            CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, CreateProcessW,
            DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess,
            InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, STARTF_USESTDHANDLES,
            STARTUPINFOEXW, UpdateProcThreadAttribute,
        },
    },
};

use crate::{Spawn, clamp_size};

/// A running shell on a pseudo-console. Read its output from [`Pty::reader`]; write
/// what is typed with [`Pty::write`]. Dropping it ends the shell and everything it
/// started.
pub struct Pty {
    console: HPCON,
    output: File,
    input: Mutex<File>,
    process: OwnedHandle,
    _thread: OwnedHandle,
    /// Closing it kills every process in it.
    _job: OwnedHandle,
}

// SAFETY: the pseudo-console handle is used only through these methods, which the
// console API allows from any thread.
unsafe impl Send for Pty {}
unsafe impl Sync for Pty {}

fn size(cols: u16, rows: u16) -> COORD {
    let (cols, rows) = clamp_size(cols, rows);
    COORD {
        X: cols as i16,
        Y: rows as i16,
    }
}

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

fn check(ok: i32) -> io::Result<()> {
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// A pipe's two ends, owned.
fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let (mut read, mut write): (HANDLE, HANDLE) = (null_mut(), null_mut());
    // SAFETY: CreatePipe fills both handles; no inheritance, default buffer size.
    check(unsafe { CreatePipe(&mut read, &mut write, null(), 0) })?;
    // SAFETY: both were just created and are owned here.
    Ok(unsafe {
        (
            OwnedHandle::from_raw_handle(read),
            OwnedHandle::from_raw_handle(write),
        )
    })
}

impl Pty {
    pub fn spawn(opts: &Spawn) -> io::Result<Self> {
        let shell = opts
            .shell
            .clone()
            .unwrap_or_else(|| PathBuf::from("powershell.exe"));
        // The session's user when this is a SYSTEM host; none when it acts as itself.
        let user = nexa_user::UserToken::of_session()?;
        let environment = user.as_ref().map(|u| u.environment()).transpose()?;
        let cwd = opts.cwd.clone().or_else(|| match &user {
            Some(_) => nexa_user::user_folder(nexa_user::Folder::Profile).ok(),
            None => std::env::var_os("USERPROFILE").map(PathBuf::from),
        });

        let (in_read, in_write) = pipe()?;
        let (out_read, out_write) = pipe()?;
        let mut console: HPCON = 0;
        // SAFETY: the pipe ends are valid; ConPTY duplicates what it needs.
        let hr = unsafe {
            CreatePseudoConsole(
                size(opts.cols, opts.rows),
                raw(&in_read),
                raw(&out_write),
                0,
                &mut console,
            )
        };
        if hr < 0 {
            return Err(io::Error::from_raw_os_error(hr));
        }
        // The console holds its own copies now.
        drop(in_read);
        drop(out_write);

        // The job: every process the shell starts dies with it.
        // SAFETY: an anonymous job with default security.
        let job = unsafe { CreateJobObjectW(null(), null()) };
        if job.is_null() {
            unsafe { ClosePseudoConsole(console) };
            return Err(io::Error::last_os_error());
        }
        // SAFETY: owned from here on.
        let job = unsafe { OwnedHandle::from_raw_handle(job) };
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the structure and its size match the class.
        check(unsafe {
            SetInformationJobObject(
                raw(&job),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        })?;

        // The attribute list that attaches the pseudo-console to the new process.
        let mut list_size = 0usize;
        // SAFETY: the first call only reports the size needed.
        unsafe { InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut list_size) };
        let mut list = vec![0u8; list_size];
        let attrs: LPPROC_THREAD_ATTRIBUTE_LIST = list.as_mut_ptr().cast();
        // SAFETY: `list` is as large as asked for and outlives the process creation.
        check(unsafe { InitializeProcThreadAttributeList(attrs, 1, 0, &mut list_size) })?;
        let result = (|| {
            // SAFETY: the value is the HPCON itself, as the attribute requires.
            check(unsafe {
                UpdateProcThreadAttribute(
                    attrs,
                    0,
                    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                    console as *const c_void,
                    std::mem::size_of::<HPCON>(),
                    null_mut(),
                    null(),
                )
            })?;
            let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
            si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            si.lpAttributeList = attrs;
            // No standard handles of our own: the shell must take them from the
            // pseudo-console. Left unset, it inherits the agent's, which the app
            // redirects to pipes, and everything it prints goes there instead.
            si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
            si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
            si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
            let mut command = wide(OsStr::new(&format!("\"{}\" -NoLogo", shell.display())));
            let dir = cwd.as_ref().map(|d| wide(d.as_os_str()));
            let dir = dir.as_ref().map_or(null(), |d| d.as_ptr());
            let flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT;
            let mut desktop = wide(OsStr::new("winsta0\\default"));
            let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY (both): every pointer is valid for the call; the command line is
            // mutable as required; no handles are inherited.
            check(match (&user, &environment) {
                (Some(user), Some(environment)) => {
                    // The user's own desktop, whatever this thread's is.
                    si.StartupInfo.lpDesktop = desktop.as_mut_ptr();
                    unsafe {
                        CreateProcessAsUserW(
                            user.raw(),
                            null(),
                            command.as_mut_ptr(),
                            null(),
                            null(),
                            0,
                            flags,
                            environment.as_ptr(),
                            dir,
                            &si.StartupInfo,
                            &mut pi,
                        )
                    }
                }
                _ => unsafe {
                    CreateProcessW(
                        null(),
                        command.as_mut_ptr(),
                        null(),
                        null(),
                        0,
                        flags,
                        null(),
                        dir,
                        &si.StartupInfo,
                        &mut pi,
                    )
                },
            })?;
            Ok(pi)
        })();
        // SAFETY: initialized above, no longer needed once the process exists.
        unsafe { DeleteProcThreadAttributeList(attrs) };
        let pi = match result {
            Ok(pi) => pi,
            Err(e) => {
                unsafe { ClosePseudoConsole(console) };
                return Err(e);
            }
        };
        // SAFETY: both handles come from CreateProcessW and are owned here.
        let (process, thread) = unsafe {
            (
                OwnedHandle::from_raw_handle(pi.hProcess),
                OwnedHandle::from_raw_handle(pi.hThread),
            )
        };
        // SAFETY: both handles are valid.
        unsafe { AssignProcessToJobObject(raw(&job), raw(&process)) };

        Ok(Self {
            console,
            output: File::from(out_read),
            input: Mutex::new(File::from(in_write)),
            process,
            _thread: thread,
            _job: job,
        })
    }

    /// A blocking reader of everything the shell writes; it ends once the console closes.
    pub fn reader(&self) -> io::Result<File> {
        self.output.try_clone()
    }

    /// What was typed.
    pub fn write(&self, data: &[u8]) -> io::Result<()> {
        let mut w = self.input.lock().unwrap_or_else(|e| e.into_inner());
        w.write_all(data)
    }

    /// The window was resized.
    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        // SAFETY: the console is open until drop.
        let hr = unsafe { ResizePseudoConsole(self.console, size(cols, rows)) };
        if hr < 0 {
            return Err(io::Error::from_raw_os_error(hr));
        }
        Ok(())
    }

    /// Whether the shell is still running.
    pub fn is_running(&self) -> bool {
        let mut code = 0u32;
        // SAFETY: the process handle is valid until drop.
        let ok = unsafe { GetExitCodeProcess(raw(&self.process), &mut code) } != 0;
        ok && code == STILL_ACTIVE as u32
    }
}

fn raw(h: &OwnedHandle) -> HANDLE {
    use std::os::windows::io::AsRawHandle;
    let r = h.as_raw_handle();
    if r.is_null() { INVALID_HANDLE_VALUE } else { r }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // Closing the console ends the shell's connection to it and the reader's pipe;
        // the job handle closes after this and kills whatever is left.
        // SAFETY: opened in spawn, closed once.
        unsafe { ClosePseudoConsole(self.console) };
    }
}
