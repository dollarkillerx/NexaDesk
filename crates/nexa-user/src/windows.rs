use std::{
    cell::Cell,
    ffi::{OsString, c_void},
    io,
    os::windows::{
        ffi::OsStringExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::PathBuf,
    ptr,
    sync::OnceLock,
};

use windows_sys::{
    Win32::{
        Foundation::{ERROR_NO_TOKEN, HANDLE, LocalFree},
        Security::{
            AdjustTokenPrivileges, Authorization::ConvertSidToStringSidW, GetTokenInformation,
            ImpersonateLoggedOnUser, IsWellKnownSid, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW,
            RevertToSelf, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES,
            TOKEN_QUERY, TOKEN_USER, TokenUser, WinLocalSystemSid,
        },
        System::{
            Com::CoTaskMemFree,
            Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock},
            RemoteDesktop::{ProcessIdToSessionId, WTSQueryUserToken},
            Threading::{GetCurrentProcess, GetCurrentProcessId, OpenProcessToken},
        },
        UI::Shell::{
            FOLDERID_Downloads, FOLDERID_LocalAppData, FOLDERID_Profile, FOLDERID_RoamingAppData,
            SHGetKnownFolderPath,
        },
    },
    core::GUID,
};

/// Whether this process runs as SYSTEM (LocalSystem). Looked up once.
pub fn is_system() -> bool {
    static SYSTEM: OnceLock<bool> = OnceLock::new();
    *SYSTEM.get_or_init(|| token_of_this_process(TOKEN_QUERY).is_some_and(|t| is_system_token(&t)))
}

fn token_of_this_process(access: u32) -> Option<OwnedHandle> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: the pseudo handle of this process; `token` is written on success.
    if unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut token) } == 0 {
        return None;
    }
    // SAFETY: a handle just opened and owned from here on.
    Some(unsafe { OwnedHandle::from_raw_handle(token) })
}

fn is_system_token(token: &OwnedHandle) -> bool {
    // TOKEN_USER and the SID after it; u64 for its alignment.
    let mut buf = [0u64; 32];
    let mut len = 0u32;
    // SAFETY: a buffer of the size given; TOKEN_USER and its SID fit in it.
    let ok = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buf.as_mut_ptr().cast(),
            size_of_val(&buf) as u32,
            &mut len,
        )
    };
    if ok == 0 {
        return false;
    }
    // SAFETY: GetTokenInformation wrote a TOKEN_USER at the start of `buf`.
    let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };
    // SAFETY: a SID pointer into `buf`.
    unsafe { IsWellKnownSid(user.User.Sid, WinLocalSystemSid) != 0 }
}

/// WTSQueryUserToken needs SE_TCB_NAME enabled; SYSTEM holds it. Done once.
fn enable_tcb() {
    static DONE: OnceLock<()> = OnceLock::new();
    DONE.get_or_init(|| {
        let Some(token) = token_of_this_process(TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY) else {
            return;
        };
        let name: Vec<u16> = "SeTcbPrivilege".encode_utf16().chain(Some(0)).collect();
        let mut privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: Default::default(),
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        // SAFETY: a NUL-terminated name; the LUID is written on success.
        if unsafe {
            LookupPrivilegeValueW(
                ptr::null(),
                name.as_ptr(),
                &mut privileges.Privileges[0].Luid,
            )
        } == 0
        {
            return;
        }
        // SAFETY: an open token with TOKEN_ADJUST_PRIVILEGES and one privilege to enable.
        unsafe {
            AdjustTokenPrivileges(
                token.as_raw_handle(),
                0,
                &privileges,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
    });
}

/// Nobody is logged in to the session (the sign-in screen): nothing can be done as a user,
/// and nothing is done as SYSTEM instead.
fn nobody() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "nobody is signed in to this computer's session",
    )
}

/// The primary token of the person logged in to this process's session.
pub struct UserToken(OwnedHandle);

impl UserToken {
    /// The session user's token when this process is SYSTEM; `None` otherwise, when it
    /// acts as itself. An error when it is SYSTEM and nobody is signed in.
    pub fn of_session() -> io::Result<Option<Self>> {
        if !is_system() {
            return Ok(None);
        }
        let mut session = 0u32;
        // SAFETY: this process's id; `session` is written on success.
        if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) } == 0 {
            return Err(io::Error::last_os_error());
        }
        enable_tcb();
        let mut token: HANDLE = ptr::null_mut();
        // SAFETY: `token` is written on success and owned from then on.
        if unsafe { WTSQueryUserToken(session, &mut token) } == 0 {
            let e = io::Error::last_os_error();
            return Err(if e.raw_os_error() == Some(ERROR_NO_TOKEN as i32) {
                nobody()
            } else {
                e
            });
        }
        // SAFETY: a token handle WTSQueryUserToken returned; the caller owns it.
        Ok(Some(Self(unsafe { OwnedHandle::from_raw_handle(token) })))
    }

    pub fn raw(&self) -> HANDLE {
        self.0.as_raw_handle()
    }

    /// The user's SID as text (`S-1-5-21-…`), for an access list in SDDL.
    pub fn sid_string(&self) -> io::Result<String> {
        let mut buf = [0u64; 32];
        let mut len = 0u32;
        // SAFETY: a buffer of the size given; TOKEN_USER and its SID fit in it.
        if unsafe {
            GetTokenInformation(
                self.raw(),
                TokenUser,
                buf.as_mut_ptr().cast(),
                size_of_val(&buf) as u32,
                &mut len,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: GetTokenInformation wrote a TOKEN_USER at the start of `buf`.
        let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };
        let mut text: *mut u16 = ptr::null_mut();
        // SAFETY: a SID inside `buf`; `text` is allocated by the call and freed below.
        if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a NUL-terminated string the call returned.
        let sid = unsafe {
            let mut n = 0;
            while *text.add(n) != 0 {
                n += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(text, n))
        };
        // SAFETY: allocated by ConvertSidToStringSidW.
        unsafe { LocalFree(text.cast()) };
        Ok(sid)
    }

    /// The user's environment, as a new logon would have it, for CreateProcessAsUserW
    /// (with CREATE_UNICODE_ENVIRONMENT).
    pub fn environment(&self) -> io::Result<Environment> {
        let mut block: *mut c_void = ptr::null_mut();
        // SAFETY: a valid token; `block` is written on success and freed by `Environment`.
        if unsafe { CreateEnvironmentBlock(&mut block, self.raw(), 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Environment(block))
    }
}

/// An environment block from CreateEnvironmentBlock.
pub struct Environment(*mut c_void);

impl Environment {
    pub fn as_ptr(&self) -> *const c_void {
        self.0
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        // SAFETY: the block CreateEnvironmentBlock returned.
        unsafe { DestroyEnvironmentBlock(self.0) };
    }
}

thread_local! {
    /// Whether this thread is impersonating the user now (for nested calls).
    static AS_USER: Cell<bool> = const { Cell::new(false) };
}

/// Runs `f` as the session's user when this process is SYSTEM: the calling thread
/// impersonates the user for the call, so files `f` opens or creates are checked against
/// the user's rights and created as the user's, and DPAPI uses the user's keys. Handles
/// opened inside stay usable after it. `f` must not hand work to other threads.
///
/// When the process is not SYSTEM, it simply runs `f`. When it is SYSTEM and nobody is
/// signed in, it fails without running `f`.
pub fn as_user<T>(f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    if !is_system() || AS_USER.get() {
        return f();
    }
    let token = UserToken::of_session()?.ok_or_else(nobody)?;
    // SAFETY: a primary token of the user; undone by `Revert`.
    if unsafe { ImpersonateLoggedOnUser(token.raw()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    struct Revert;
    impl Drop for Revert {
        fn drop(&mut self) {
            AS_USER.set(false);
            // SAFETY: ends this thread's impersonation. It does not fail for a thread
            // that impersonates; if it did, the thread would stay the user: less, not more.
            unsafe { RevertToSelf() };
        }
    }
    AS_USER.set(true);
    let _revert = Revert;
    f()
}

/// A folder of the session's user.
#[derive(Clone, Copy, Debug)]
pub enum Folder {
    /// `%USERPROFILE%`
    Profile,
    /// `%APPDATA%`
    RoamingAppData,
    /// `%LOCALAPPDATA%`
    LocalAppData,
    Downloads,
}

/// Where `folder` is for the session's user (this process's own user unless it is
/// SYSTEM). Fails when the process is SYSTEM and nobody is signed in.
pub fn user_folder(folder: Folder) -> io::Result<PathBuf> {
    let id: GUID = match folder {
        Folder::Profile => FOLDERID_Profile,
        Folder::RoamingAppData => FOLDERID_RoamingAppData,
        Folder::LocalAppData => FOLDERID_LocalAppData,
        Folder::Downloads => FOLDERID_Downloads,
    };
    let token = UserToken::of_session()?;
    let mut path: *mut u16 = ptr::null_mut();
    // SAFETY: a known folder id, the user's token or none (this process's user); `path`
    // is allocated by the call, also on failure, and freed below.
    let hr = unsafe {
        SHGetKnownFolderPath(
            &id,
            0,
            token.as_ref().map_or(ptr::null_mut(), UserToken::raw),
            &mut path,
        )
    };
    let result = if hr < 0 {
        Err(io::Error::from_raw_os_error(hr))
    } else {
        // SAFETY: a NUL-terminated path the call returned.
        let wide = unsafe {
            let mut n = 0;
            while *path.add(n) != 0 {
                n += 1;
            }
            std::slice::from_raw_parts(path, n)
        };
        Ok(PathBuf::from(OsString::from_wide(wide)))
    };
    // SAFETY: the buffer SHGetKnownFolderPath allocated (null is allowed).
    unsafe { CoTaskMemFree(path.cast()) };
    result
}
