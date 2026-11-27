//! The control pipe (docs/WINDOWS_SERVICE.md §3). Only SYSTEM and users logged on at this
//! computer (INTERACTIVE) may open it, never over the network. A client gets a host in
//! its own session only: the session is the client process's, not something it names.
//! The host runs while the client keeps the connection: when the client goes (the app
//! quits, crashes, or its user logs off) the service ends the host.

use std::{
    collections::HashSet,
    fs::File,
    io::{self, BufRead, BufReader, Read, Write},
    os::windows::io::{AsRawHandle, FromRawHandle},
    ptr,
    sync::{Mutex, atomic::Ordering::SeqCst, mpsc},
    thread,
    time::{Duration, Instant},
};

use nexa_user::launch::{Output, launch_in_session, output_pipe};
use windows_sys::Win32::{
    Foundation::{ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE, LocalFree, WAIT_OBJECT_0},
    Security::{
        Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
        PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
    },
    Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX},
    System::{
        Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_READMODE_BYTE,
            PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
            PeekNamedPipe,
        },
        RemoteDesktop::ProcessIdToSessionId,
        Threading::{GetExitCodeProcess, INFINITE, TerminateProcess, WaitForSingleObject},
    },
};

use super::{log::log, sas, service::STOPPING, wide};
use crate::proto::{Event, MAX_REQUEST, PIPE, PROTOCOL, Request, check_args};

/// SYSTEM: everything. INTERACTIVE: read and write.
const SDDL: &str = "D:P(A;;GA;;;SY)(A;;GRGW;;;IU)";

/// The pids of the hosts this service started and that still run.
static HOSTS: Mutex<Option<HashSet<u32>>> = Mutex::new(None);

fn hosts<T>(f: impl FnOnce(&mut HashSet<u32>) -> T) -> T {
    let mut guard = HOSTS.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(HashSet::new))
}

/// Ends a wait for the next client, so the server sees [`STOPPING`].
pub fn wake() {
    let _ = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(PIPE);
}

struct Security(PSECURITY_DESCRIPTOR);

impl Drop for Security {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe { LocalFree(self.0) };
    }
}

fn security() -> io::Result<Security> {
    let sddl = wide(SDDL);
    let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: a NUL-terminated SDDL string; `sd` is written on success and freed by Security.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Security(sd))
}

/// A new instance of the pipe. The first one must be the first of its name: nobody else
/// may be serving it.
fn instance(first: bool, security: &Security) -> io::Result<File> {
    let name = wide(PIPE);
    let attrs = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security.0,
        bInheritHandle: 0,
    };
    let open_mode = PIPE_ACCESS_DUPLEX
        | if first {
            FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            0
        };
    // SAFETY: a NUL-terminated name and attributes that live through the call.
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            64 * 1024,
            64 * 1024,
            0,
            &attrs,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a pipe handle just created; owned from here on.
    Ok(unsafe { File::from_raw_handle(handle) })
}

/// Serves clients until the service stops. `running` is called once the pipe is up.
pub fn serve(running: impl FnOnce()) -> Result<(), String> {
    let security = security().map_err(|e| format!("pipe security: {e}"))?;
    let mut next = instance(true, &security).map_err(|e| format!("the control pipe: {e}"))?;
    running();
    loop {
        // SAFETY: a pipe instance; blocks until a client connects.
        let ok = unsafe { ConnectNamedPipe(next.as_raw_handle(), ptr::null_mut()) } != 0;
        let connected =
            ok || io::Error::last_os_error().raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32);
        if STOPPING.load(SeqCst) {
            break;
        }
        // The next instance is there before this one is handed over.
        let fresh = instance(false, &security).map_err(|e| format!("the control pipe: {e}"))?;
        let pipe = std::mem::replace(&mut next, fresh);
        if connected {
            let _ = thread::Builder::new()
                .name("client".into())
                .spawn(move || serve_client(pipe));
        }
    }
    // Every host's watcher ends its host once it sees STOPPING.
    let deadline = Instant::now() + Duration::from_secs(5);
    while hosts(|h| !h.is_empty()) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn send(pipe: &File, event: &Event) -> io::Result<()> {
    let mut line = serde_json::to_string(event).map_err(io::Error::other)?;
    line.push('\n');
    let mut pipe = pipe;
    pipe.write_all(line.as_bytes())?;
    pipe.flush()
}

/// The client's process id and session.
fn client_of(pipe: &File) -> io::Result<(u32, u32)> {
    let mut pid = 0u32;
    // SAFETY: a connected pipe; `pid` is written on success.
    if unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle(), &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut session = 0u32;
    // SAFETY: `session` is written on success.
    if unsafe { ProcessIdToSessionId(pid, &mut session) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((pid, session))
}

/// Whether the client has closed its end.
fn client_gone(pipe: &File) -> bool {
    let mut available = 0u32;
    // SAFETY: a pipe handle; only counts what is waiting, reads nothing.
    unsafe {
        PeekNamedPipe(
            pipe.as_raw_handle(),
            ptr::null_mut(),
            0,
            ptr::null_mut(),
            &mut available,
            ptr::null_mut(),
        ) == 0
    }
}

fn serve_client(pipe: File) {
    let (pid, session) = match client_of(&pipe) {
        Ok(c) => c,
        Err(e) => {
            log(&format!("a client could not be identified: {e}"));
            return;
        }
    };
    let mut line = Vec::new();
    let read = BufReader::new((&pipe).take(MAX_REQUEST as u64)).read_until(b'\n', &mut line);
    if read.is_err() || line.is_empty() {
        return;
    }
    let request = match serde_json::from_slice::<Request>(&line) {
        Ok(r) => r,
        Err(e) => {
            let _ = send(&pipe, &Event::Error(format!("not understood: {e}")));
            return;
        }
    };
    let result = match request {
        Request::Hello => send(&pipe, &Event::Hello { protocol: PROTOCOL }),
        Request::Sas => {
            let event = if !hosts(|h| h.contains(&pid)) {
                Event::Error("only a host this service started may send Ctrl+Alt+Del".into())
            } else {
                log(&format!("Ctrl+Alt+Del for host pid {pid}"));
                match sas::send() {
                    Ok(()) => Event::Done,
                    Err(e) => Event::Error(e),
                }
            };
            send(&pipe, &event)
        }
        Request::Start {
            args,
            signal_secret,
        } => run_host(&pipe, pid, session, &args, signal_secret.as_deref()),
    };
    if let Err(e) = result {
        log(&format!("client pid {pid}: {e}"));
    }
}

/// Starts the host in the client's session and relays its output until it ends; ends it
/// when the client goes or the service stops.
fn run_host(
    pipe: &File,
    client: u32,
    session: u32,
    args: &[String],
    signal_secret: Option<&str>,
) -> io::Result<()> {
    if let Err(e) = check_args(args) {
        return send(pipe, &Event::Error(e));
    }
    if session == 0 {
        return send(pipe, &Event::Error("not from a user's session".into()));
    }
    let exe = std::env::current_exe()?.with_file_name("nexa-agent.exe");
    let (output, give) = output_pipe()?;
    let env: Vec<(&str, &str)> = signal_secret
        .map(|s| ("NEXA_SIGNAL_SECRET", s))
        .into_iter()
        .collect();
    let child = match launch_in_session(session, &exe, args, &env, Output::To(&give)) {
        Ok(c) => c,
        Err(e) => {
            log(&format!(
                "starting the host in session {session} failed: {e}"
            ));
            return send(pipe, &Event::Error(format!("starting the host: {e}")));
        }
    };
    drop(give);
    hosts(|h| h.insert(child.pid));
    log(&format!(
        "host pid {} started in session {session} for client pid {client}",
        child.pid
    ));
    let mut result = send(pipe, &Event::Started { pid: child.pid });

    let (tx, rx) = mpsc::channel::<String>();
    let _ = thread::Builder::new()
        .name("host-output".into())
        .spawn(move || {
            let mut reader = BufReader::new(output);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let line = String::from_utf8_lossy(&buf).trim_end().to_owned();
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                }
            }
        });

    let mut ended = false;
    let mut end = |why: &str| {
        if !ended {
            ended = true;
            log(&format!("ending host pid {}: {why}", child.pid));
            // SAFETY: the host's process handle, owned by `child`.
            unsafe { TerminateProcess(child.process.as_raw_handle(), 1) };
        }
    };
    loop {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(line) => {
                if result.is_ok() {
                    result = send(pipe, &Event::Line(line));
                }
                if result.is_err() {
                    end("the client went away");
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if STOPPING.load(SeqCst) {
                    end("the service is stopping");
                } else if client_gone(pipe) {
                    end("the client went away");
                }
            }
            // Its output closed: it has ended (or is ending).
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let handle = child.process.as_raw_handle();
    let mut code = 0u32;
    // SAFETY: the host's process handle; waits for it to be gone, then reads its code.
    unsafe {
        if WaitForSingleObject(handle, INFINITE) == WAIT_OBJECT_0 {
            GetExitCodeProcess(handle, &mut code);
        }
    }
    hosts(|h| h.remove(&child.pid));
    log(&format!("host pid {} exited with code {code}", child.pid));
    if result.is_ok() {
        let _ = send(
            pipe,
            &Event::Exited {
                code: i64::from(code as i32),
            },
        );
    }
    Ok(())
}
