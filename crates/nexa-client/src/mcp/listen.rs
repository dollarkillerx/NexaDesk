//! Where local AI apps reach NexaDesk's MCP server (docs/MCP.md §16): a Unix socket
//! only this user can open (the directory must not be writable by others, the socket
//! is 0600, and each peer's uid is checked) on macOS, a named pipe only this user can
//! open, refusing remote clients, on Windows. As the host's own IPC does it
//! (nexa-agent's `ipc::transport`).

use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering::SeqCst},
    },
    thread::JoinHandle,
};

/// One AI app's connection: read and written as a byte stream.
#[cfg(unix)]
pub type Conn = std::os::unix::net::UnixStream;
#[cfg(windows)]
pub type Conn = std::fs::File;

/// The socket (or pipe) for the app whose data directory is `data_dir`.
pub fn endpoint(data_dir: &Path) -> String {
    #[cfg(unix)]
    {
        data_dir.join("mcp.sock").to_string_lossy().into_owned()
    }
    #[cfg(windows)]
    {
        // FNV-1a of the lower-cased directory, as the host's own pipe is named.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in data_dir.to_string_lossy().to_lowercase().bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!(r"\\.\pipe\nexadesk-mcp-{h:016x}")
    }
}

/// Accepts connections until dropped; each goes to `accept`.
pub struct Listener {
    stop: Arc<AtomicBool>,
    endpoint: String,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, SeqCst);
        // Wake the blocked accept with a connection of our own.
        let _ = connect(&self.endpoint);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.endpoint);
    }
}

/// Ends `conn` from this side: a read blocked on it returns, and its AI app's relay
/// sees the connection close.
pub fn hang_up(conn: &Conn) {
    #[cfg(unix)]
    {
        let _ = conn.shutdown(std::net::Shutdown::Both);
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::{Foundation::HANDLE, System::Pipes::DisconnectNamedPipe};
        // SAFETY: a pipe handle owned by `conn`, which stays open; disconnecting fails
        // the reads and writes pending on it.
        let _ = unsafe { DisconnectNamedPipe(HANDLE(conn.as_raw_handle())) };
    }
}

/// Connects to `endpoint` as an AI app's relay does.
pub fn connect(endpoint: &str) -> std::io::Result<Conn> {
    #[cfg(unix)]
    {
        Conn::connect(endpoint)
    }
    #[cfg(windows)]
    {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(endpoint)
    }
}

#[cfg(unix)]
pub fn listen(endpoint: &str, accept: impl Fn(Conn) + Send + 'static) -> Result<Listener, String> {
    use std::os::unix::{
        fs::{MetadataExt, PermissionsExt},
        io::AsRawFd,
        net::UnixListener,
    };

    let path = Path::new(endpoint);
    let dir = path.parent().ok_or("the socket has no directory")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // SAFETY: plain libc call.
    let me = unsafe { libc::getuid() };
    let meta = std::fs::metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if meta.uid() != me || meta.mode() & 0o022 != 0 {
        return Err(format!(
            "{} is not this user's own directory: AI apps could not be told apart",
            dir.display()
        ));
    }
    if path.exists() {
        if Conn::connect(path).is_ok() {
            return Err("another NexaDesk already serves AI agents".into());
        }
        std::fs::remove_file(path).map_err(|e| format!("{endpoint}: {e}"))?;
    }
    let listener = UnixListener::bind(path).map_err(|e| format!("{endpoint}: {e}"))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("{endpoint}: {e}"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    let thread = std::thread::Builder::new()
        .name("mcp-listen".into())
        .spawn(move || {
            for conn in listener.incoming() {
                if stopping.load(SeqCst) {
                    break;
                }
                let Ok(conn) = conn else { continue };
                // Only this user's own programs: anyone else's connection is dropped
                // unread.
                let (mut uid, mut gid) = (u32::MAX, u32::MAX);
                // SAFETY: a connected socket's descriptor, and two writable ids.
                let ok = unsafe { libc::getpeereid(conn.as_raw_fd(), &mut uid, &mut gid) } == 0;
                if ok && uid == me {
                    accept(conn);
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(Listener {
        stop,
        endpoint: endpoint.to_owned(),
        thread: Some(thread),
    })
}

#[cfg(windows)]
pub fn listen(endpoint: &str, accept: impl Fn(Conn) + Send + 'static) -> Result<Listener, String> {
    use std::os::windows::io::FromRawHandle;

    use windows::{
        Win32::{
            Foundation::{CloseHandle, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL, LocalFree},
            Security::{
                Authorization::{
                    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
                },
                PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
            },
            Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX},
            System::Pipes::{
                ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
            },
        },
        core::{HSTRING, w},
    };

    /// The owner only: full access for whoever created the pipe, nobody else.
    struct OwnerOnly(PSECURITY_DESCRIPTOR);
    impl Drop for OwnerOnly {
        fn drop(&mut self) {
            // SAFETY: allocated by ConvertStringSecurityDescriptor…, freed once.
            unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
        }
    }
    // SAFETY: an immutable descriptor, only read by CreateNamedPipeW.
    unsafe impl Send for OwnerOnly {}

    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: a constant SDDL string and a writable out pointer.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w!("D:P(A;;GA;;;OW)"),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )
    }
    .map_err(|e| format!("pipe security: {e}"))?;
    let sd = OwnerOnly(sd);
    let name = HSTRING::from(endpoint);
    let make = move |first: bool, sd: &OwnerOnly| -> Result<HANDLE, String> {
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0.0,
            bInheritHandle: false.into(),
        };
        let mut open = PIPE_ACCESS_DUPLEX;
        if first {
            // Nobody else may have made it first and be listening in our place.
            open |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        // SAFETY: a valid name and attributes for the call.
        let h = unsafe {
            CreateNamedPipeW(
                &name,
                open,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                64 << 10,
                64 << 10,
                0,
                Some(&attributes),
            )
        };
        if h.is_invalid() {
            Err(if first {
                "another NexaDesk already serves AI agents".to_owned()
            } else {
                format!("pipe: {}", windows::core::Error::from_thread())
            })
        } else {
            Ok(h)
        }
    };
    let first = make(true, &sd)?;
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    struct Raw(HANDLE);
    // SAFETY: a pipe handle, used by one thread at a time.
    unsafe impl Send for Raw {}
    let first = Raw(first);
    let thread = std::thread::Builder::new()
        .name("mcp-listen".into())
        .spawn(move || {
            let mut next = Some(first);
            loop {
                let pipe = match next.take() {
                    Some(p) => p,
                    None => match make(false, &sd) {
                        Ok(h) => Raw(h),
                        Err(_) => break,
                    },
                };
                // SAFETY: a pipe instance made above.
                let connected = unsafe { ConnectNamedPipe(pipe.0, None) };
                let ok = connected.is_ok()
                    || connected
                        .as_ref()
                        .is_err_and(|e| e.code() == ERROR_PIPE_CONNECTED.to_hresult());
                if stopping.load(SeqCst) {
                    // SAFETY: ours, closed once.
                    unsafe {
                        let _ = CloseHandle(pipe.0);
                    }
                    break;
                }
                if ok {
                    // SAFETY: the handle is ours and moves into the File, which closes it.
                    accept(unsafe { std::fs::File::from_raw_handle(pipe.0.0) });
                } else {
                    // SAFETY: ours, closed once.
                    unsafe {
                        let _ = CloseHandle(pipe.0);
                    }
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(Listener {
        stop,
        endpoint: endpoint.to_owned(),
        thread: Some(thread),
    })
}
