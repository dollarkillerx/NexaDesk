use std::{
    ffi::{CString, OsStr},
    fs::File,
    io::{self, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::ffi::OsStrExt,
    },
    path::PathBuf,
    sync::Mutex,
    thread,
    time::Duration,
};

use crate::{Spawn, clamp_size};

/// How long the shell has to leave after the hang-up before it is killed.
const HANGUP_GRACE: Duration = Duration::from_millis(1500);

/// A running shell on a pseudo-terminal. Read its output from [`Pty::reader`]; write
/// what is typed with [`Pty::write`]. Dropping it ends the shell and everything it
/// started.
pub struct Pty {
    master: OwnedFd,
    writer: Mutex<File>,
    pid: libc::pid_t,
}

fn cstring(s: &OsStr) -> io::Result<CString> {
    CString::new(s.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in argument"))
}

fn winsize(cols: u16, rows: u16) -> libc::winsize {
    let (cols, rows) = clamp_size(cols, rows);
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

impl Pty {
    pub fn spawn(opts: &Spawn) -> io::Result<Self> {
        let shell = opts
            .shell
            .clone()
            .or_else(|| std::env::var_os("SHELL").map(PathBuf::from))
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| PathBuf::from("/bin/sh"));
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let cwd = opts
            .cwd
            .clone()
            .or(home)
            .unwrap_or_else(|| PathBuf::from("/"));

        // Everything the child needs is made before fork: after it, only
        // async-signal-safe calls are allowed.
        let program = cstring(shell.as_os_str())?;
        let name = shell.file_name().unwrap_or(shell.as_os_str());
        let mut argv0 = b"-".to_vec();
        argv0.extend_from_slice(name.as_bytes());
        let argv0 = CString::new(argv0).map_err(|_| io::Error::other("bad shell name"))?;
        let argv = [argv0.as_ptr(), std::ptr::null()];
        let dir = cstring(cwd.as_os_str())?;
        let mut env: Vec<CString> = std::env::vars_os()
            .filter(|(k, _)| k != "TERM" && k != "COLORTERM")
            .filter_map(|(k, v)| {
                let mut kv = k.as_bytes().to_vec();
                kv.push(b'=');
                kv.extend_from_slice(v.as_bytes());
                CString::new(kv).ok()
            })
            .collect();
        env.push(CString::new("TERM=xterm-256color").expect("no NUL"));
        env.push(CString::new("COLORTERM=truecolor").expect("no NUL"));
        let mut envp: Vec<*const libc::c_char> = env.iter().map(|c| c.as_ptr()).collect();
        envp.push(std::ptr::null());

        let mut ws = winsize(opts.cols, opts.rows);
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: openpty fills the two descriptors; the name and termios are not asked for.
        let r = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut ws,
            )
        };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: both were just opened and are owned here.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        set_cloexec(master.as_raw_fd())?;

        // SAFETY: the child only calls async-signal-safe functions before exec, on
        // memory prepared above, and exits if exec fails.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(io::Error::last_os_error());
        }
        if pid == 0 {
            unsafe {
                let s = slave.as_raw_fd();
                libc::setsid();
                libc::ioctl(s, libc::TIOCSCTTY as _, 0);
                libc::dup2(s, 0);
                libc::dup2(s, 1);
                libc::dup2(s, 2);
                if s > 2 {
                    libc::close(s);
                }
                libc::close(master.as_raw_fd());
                // Signals the agent ignores or blocks must not stay that way in the shell.
                for sig in [
                    libc::SIGINT,
                    libc::SIGQUIT,
                    libc::SIGPIPE,
                    libc::SIGTERM,
                    libc::SIGHUP,
                    libc::SIGCHLD,
                ] {
                    libc::signal(sig, libc::SIG_DFL);
                }
                let mut none: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut none);
                libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
                libc::chdir(dir.as_ptr());
                libc::execve(program.as_ptr(), argv.as_ptr(), envp.as_ptr());
                libc::_exit(127);
            }
        }
        drop(slave);
        let writer = File::from(master.try_clone()?);
        Ok(Self {
            master,
            writer: Mutex::new(writer),
            pid,
        })
    }

    /// A blocking reader of everything the shell writes. It ends (EOF or an error) once
    /// the shell and whatever it started have gone.
    pub fn reader(&self) -> io::Result<File> {
        Ok(File::from(self.master.try_clone()?))
    }

    /// What was typed.
    pub fn write(&self, data: &[u8]) -> io::Result<()> {
        let mut w = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        w.write_all(data)
    }

    /// The window was resized; the shell gets SIGWINCH.
    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let ws = winsize(cols, rows);
        // SAFETY: TIOCSWINSZ reads one winsize.
        let r = unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Whether the shell is still running.
    pub fn is_running(&self) -> bool {
        // SAFETY: signal 0 only checks that the process exists.
        unsafe { libc::kill(self.pid, 0) == 0 }
    }
}

fn set_cloexec(fd: libc::c_int) -> io::Result<()> {
    // SAFETY: plain fcntl on a descriptor owned by the caller.
    let r = unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl Drop for Pty {
    fn drop(&mut self) {
        let pid = self.pid;
        // Hang up on the whole session, as closing a terminal window does; whatever is
        // still there after the grace period is killed. Reaped either way.
        unsafe {
            libc::kill(-pid, libc::SIGHUP);
            libc::kill(pid, libc::SIGHUP);
        }
        thread::spawn(move || {
            let deadline = std::time::Instant::now() + HANGUP_GRACE;
            loop {
                let mut status = 0;
                // SAFETY: waits only for our own child.
                let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
                if r == pid || r < 0 {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    unsafe {
                        libc::kill(-pid, libc::SIGKILL);
                        libc::kill(pid, libc::SIGKILL);
                        libc::waitpid(pid, &mut status, 0);
                    }
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            // The rest of its process group, if any stayed behind.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    fn read_until(r: &mut File, needle: &str) -> String {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        let end = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < end {
            let n = r.read(&mut buf).unwrap_or(0);
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
            if String::from_utf8_lossy(&out).contains(needle) {
                break;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    #[test]
    fn a_shell_runs_commands_sees_its_size_and_ends_with_the_pty() {
        let pty = Pty::spawn(&Spawn {
            shell: Some("/bin/sh".into()),
            cwd: Some("/".into()),
            cols: 100,
            rows: 30,
        })
        .unwrap();
        let mut r = pty.reader().unwrap();
        pty.write(b"echo hel''lo $TERM; stty size\n").unwrap();
        let out = read_until(&mut r, "30 100");
        assert!(out.contains("hello xterm-256color"), "{out}");
        assert!(out.contains("30 100"), "{out}");
        pty.resize(120, 40).unwrap();
        pty.write(b"stty size\n").unwrap();
        let out = read_until(&mut r, "40 120");
        assert!(out.contains("40 120"), "{out}");

        // A child that would linger is ended with the shell.
        pty.write(b"sleep 300 &\necho started\n").unwrap();
        read_until(&mut r, "started");
        let pid = pty.pid;
        drop(pty);
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < end && unsafe { libc::kill(pid, 0) } == 0 {
            thread::sleep(Duration::from_millis(50));
        }
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "the shell is gone");
        // The reader ends too.
        let mut rest = Vec::new();
        let _ = r.read_to_end(&mut rest);
    }
}
