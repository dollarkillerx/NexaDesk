//! Where the UI's connection comes in, per platform: a Unix socket in the data
//! directory (mode 0600, peer uid checked before a byte is read), or on Windows a
//! named pipe only this user may open, refused to other machines. Above this, the
//! IPC is the same everywhere: frames over a byte stream.

use std::{io, path::Path};

use tokio::io::{AsyncRead, AsyncWrite};

use super::{IpcConfig, IpcError};

pub(super) type Reader = Box<dyn AsyncRead + Unpin + Send>;
pub(super) type Writer = Box<dyn AsyncWrite + Unpin + Send>;

pub(super) fn io_err(path: &Path) -> impl FnOnce(io::Error) -> IpcError + '_ {
    move |source| IpcError::Io {
        path: path.to_owned(),
        source,
    }
}

#[cfg(unix)]
pub(super) use unix::{Cleanup, Listener};
#[cfg(windows)]
pub(super) use windows::{Cleanup, Listener};

#[cfg(unix)]
mod unix {
    use std::{
        io,
        os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
        path::{Path, PathBuf},
    };

    use tokio::net::UnixListener;

    use super::{IpcConfig, IpcError, Reader, Writer, io_err};

    pub(in crate::ipc) struct Listener {
        listener: UnixListener,
        expected_uid: u32,
    }

    /// Removes the socket, if it is still the one this server made.
    pub(in crate::ipc) struct Cleanup {
        path: PathBuf,
        ino: (u64, u64),
    }

    impl Cleanup {
        pub(in crate::ipc) fn remove(&self) {
            if let Ok(m) = std::fs::symlink_metadata(&self.path)
                && (m.dev(), m.ino()) == self.ino
            {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }

    impl Listener {
        /// A socket left by a dead agent is replaced; a live one is an error, as is
        /// anything at the path that is not our socket. Inside a tokio runtime.
        pub(in crate::ipc) fn bind(config: &IpcConfig) -> Result<(Self, Cleanup), IpcError> {
            let path = config.path.clone();
            let dir = path.parent().unwrap_or(Path::new("."));
            if !dir.exists() {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(dir)
                    .map_err(io_err(dir))?;
            }
            let me = super::super::current_uid();
            let meta = std::fs::metadata(dir).map_err(io_err(dir))?;
            if meta.uid() != me || meta.mode() & 0o022 != 0 {
                return Err(IpcError::UnsafeDirectory(dir.to_owned()));
            }
            match std::fs::symlink_metadata(&path) {
                Ok(m) if m.file_type().is_socket() && m.uid() == me => {
                    match std::os::unix::net::UnixStream::connect(&path) {
                        Ok(_) => return Err(IpcError::AlreadyRunning(path)),
                        Err(_) => std::fs::remove_file(&path).map_err(io_err(&path))?,
                    }
                }
                Ok(_) => return Err(IpcError::NotOurSocket(path)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_err(&path)(e)),
            }
            let listener = UnixListener::bind(&path).map_err(io_err(&path))?;
            // The peer uid check in `accept` covers the moment before this applies.
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .map_err(io_err(&path))?;
            let m = std::fs::symlink_metadata(&path).map_err(io_err(&path))?;
            Ok((
                Self {
                    listener,
                    expected_uid: config.expected_uid,
                },
                Cleanup {
                    path,
                    ino: (m.dev(), m.ino()),
                },
            ))
        }

        /// The next connection from this user; anyone else's is dropped unread.
        pub(in crate::ipc) async fn accept(&mut self) -> io::Result<Option<(Reader, Writer)>> {
            let (stream, _) = self.listener.accept().await?;
            match stream.peer_cred() {
                Ok(cred) if cred.uid() == self.expected_uid => {
                    let (rd, wr) = stream.into_split();
                    Ok(Some((Box::new(rd), Box::new(wr))))
                }
                _ => Ok(None),
            }
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::{ffi::c_void, io, path::Path};

    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use windows_sys::Win32::{
        Foundation::{ERROR_ACCESS_DENIED, LocalFree},
        Security::{
            Authorization::{
                ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
            },
            PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
        },
    };

    use super::{IpcConfig, IpcError, Reader, Writer, io_err};

    /// Only the pipe's owner (the user running the agent) may open it.
    const OWNER_ONLY: &str = "D:P(A;;GA;;;OW)";

    /// The pipe for a data directory: `\\.\pipe\nexadesk-<FNV-1a of the path>`. The
    /// app computes the same name from the same directory.
    pub fn pipe_name(dir: &Path) -> String {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in dir.to_string_lossy().to_lowercase().bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!(r"\\.\pipe\nexadesk-{h:016x}")
    }

    pub(in crate::ipc) struct Listener {
        name: String,
        next: NamedPipeServer,
    }

    /// Nothing to remove: a pipe goes when its last handle closes.
    pub(in crate::ipc) struct Cleanup;

    impl Cleanup {
        pub(in crate::ipc) fn remove(&self) {}
    }

    fn create(name: &str, first: bool) -> io::Result<NamedPipeServer> {
        let wide: Vec<u16> = OWNER_ONLY.encode_utf16().chain(Some(0)).collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: a NUL-terminated SDDL string; `descriptor` is written on success.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        // SAFETY: valid security attributes for the duration of the call.
        let server = unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(
                    name,
                    &mut attributes as *mut SECURITY_ATTRIBUTES as *mut c_void,
                )
        };
        // SAFETY: allocated by the conversion above; the pipe keeps its own copy.
        unsafe { LocalFree(descriptor) };
        server
    }

    impl Listener {
        /// A second agent on the same data directory is refused: the pipe exists.
        pub(in crate::ipc) fn bind(config: &IpcConfig) -> Result<(Self, Cleanup), IpcError> {
            let dir = config.path.parent().unwrap_or(Path::new("."));
            std::fs::create_dir_all(dir).map_err(io_err(dir))?;
            let name = pipe_name(dir);
            let next = create(&name, true).map_err(|e| {
                if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
                    IpcError::AlreadyRunning(config.path.clone())
                } else {
                    io_err(&config.path)(e)
                }
            })?;
            Ok((Self { name, next }, Cleanup))
        }

        /// The next connection; the pipe's access list already kept other users out.
        pub(in crate::ipc) async fn accept(&mut self) -> io::Result<Option<(Reader, Writer)>> {
            self.next.connect().await?;
            let fresh = create(&self.name, false)?;
            let connected = std::mem::replace(&mut self.next, fresh);
            let (rd, wr) = tokio::io::split(connected);
            Ok(Some((Box::new(rd), Box::new(wr))))
        }
    }
}

#[cfg(windows)]
pub use windows::pipe_name;
