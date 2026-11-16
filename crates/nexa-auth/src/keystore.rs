//! Where the device's secrets live (§9.1).
//!
//! A store holds one opaque secret. The device key and the host's password
//! verifier each get their own store.

use std::{
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
};

use thiserror::Error;
use zeroize::Zeroizing;

use crate::fsio::atomic_write;

/// Far larger than any secret here; bounds a read of a replaced file.
const MAX_SECRET_LEN: u64 = 4096;

#[derive(Debug, Error)]
pub enum KeyStoreError {
    #[error("key file is a symlink")]
    Symlink,
    #[error("key file is accessible by group or others")]
    InsecurePermissions,
    #[error("key file is implausibly large")]
    TooLarge,
    #[error("this platform has no secure file permissions for key files")]
    UnsupportedPlatform,
    #[error("key file i/o: {0}")]
    Io(#[from] io::Error),
    #[cfg(target_os = "macos")]
    #[error("keychain: {0}")]
    Keychain(#[from] security_framework::base::Error),
}

pub trait KeyStore: Send + Sync {
    /// `Ok(None)` means no secret has been stored yet.
    fn load(&self) -> Result<Option<Zeroizing<Vec<u8>>>, KeyStoreError>;
    fn store(&self, secret: &[u8]) -> Result<(), KeyStoreError>;
    /// Succeeds if there is nothing to delete.
    fn delete(&self) -> Result<(), KeyStoreError>;
}

/// A 0600 file, replaced atomically. On Windows the secret is encrypted with DPAPI for
/// the signed-in user (§9.1) before it is written; elsewhere every operation fails
/// rather than leave a private key under default permissions.
#[derive(Debug, Clone)]
pub struct FileKeyStore {
    path: PathBuf,
}

impl FileKeyStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(unix)]
impl KeyStore for FileKeyStore {
    fn load(&self) -> Result<Option<Zeroizing<Vec<u8>>>, KeyStoreError> {
        use std::os::unix::fs::MetadataExt;

        let meta = match fs::symlink_metadata(&self.path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if meta.file_type().is_symlink() {
            return Err(KeyStoreError::Symlink);
        }
        if meta.mode() & 0o077 != 0 {
            return Err(KeyStoreError::InsecurePermissions);
        }
        if meta.len() > MAX_SECRET_LEN {
            return Err(KeyStoreError::TooLarge);
        }
        let mut data = Zeroizing::new(Vec::new());
        File::open(&self.path)?
            .take(MAX_SECRET_LEN)
            .read_to_end(&mut data)?;
        Ok(Some(data))
    }

    fn store(&self, secret: &[u8]) -> Result<(), KeyStoreError> {
        use std::os::unix::fs::DirBuilderExt;

        if let Some(dir) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        atomic_write(&self.path, secret)?;
        Ok(())
    }

    fn delete(&self) -> Result<(), KeyStoreError> {
        match fs::remove_file(&self.path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}

#[cfg(windows)]
impl KeyStore for FileKeyStore {
    fn load(&self) -> Result<Option<Zeroizing<Vec<u8>>>, KeyStoreError> {
        let meta = match fs::symlink_metadata(&self.path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if meta.file_type().is_symlink() {
            return Err(KeyStoreError::Symlink);
        }
        // DPAPI adds a few hundred bytes around the secret.
        if meta.len() > MAX_SECRET_LEN + 4096 {
            return Err(KeyStoreError::TooLarge);
        }
        let mut sealed = Vec::new();
        File::open(&self.path)?
            .take(MAX_SECRET_LEN + 4096)
            .read_to_end(&mut sealed)?;
        dpapi::unprotect(&sealed).map(Some)
    }

    fn store(&self, secret: &[u8]) -> Result<(), KeyStoreError> {
        if let Some(dir) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(dir)?;
        }
        let sealed = dpapi::protect(secret)?;
        atomic_write(&self.path, &sealed)?;
        Ok(())
    }

    fn delete(&self) -> Result<(), KeyStoreError> {
        match fs::remove_file(&self.path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}

/// Encryption to the signed-in Windows user: only that account on this machine can undo it.
#[cfg(windows)]
mod dpapi {
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
        },
    };
    use zeroize::Zeroizing;

    use super::KeyStoreError;

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        }
    }

    /// Copies DPAPI's output and frees it.
    fn take(out: CRYPT_INTEGER_BLOB) -> Zeroizing<Vec<u8>> {
        // SAFETY: DPAPI returned `cbData` bytes at `pbData`, freed with LocalFree.
        let data = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) };
        let copy = Zeroizing::new(data.to_vec());
        unsafe {
            std::ptr::write_bytes(out.pbData, 0, out.cbData as usize);
            LocalFree(out.pbData.cast());
        }
        copy
    }

    pub fn protect(secret: &[u8]) -> Result<Zeroizing<Vec<u8>>, KeyStoreError> {
        let input = blob(secret);
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: null_mut(),
        };
        // SAFETY: the input blob points at `secret` for the call; no prompt, no entropy.
        let ok = unsafe {
            CryptProtectData(
                &input,
                null(),
                null(),
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(take(out))
    }

    pub fn unprotect(sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>, KeyStoreError> {
        let input = blob(sealed);
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: null_mut(),
        };
        // SAFETY: as in `protect`.
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                null_mut(),
                null(),
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(take(out))
    }
}

#[cfg(not(any(unix, windows)))]
impl KeyStore for FileKeyStore {
    fn load(&self) -> Result<Option<Zeroizing<Vec<u8>>>, KeyStoreError> {
        Err(KeyStoreError::UnsupportedPlatform)
    }
    fn store(&self, _: &[u8]) -> Result<(), KeyStoreError> {
        Err(KeyStoreError::UnsupportedPlatform)
    }
    fn delete(&self) -> Result<(), KeyStoreError> {
        Err(KeyStoreError::UnsupportedPlatform)
    }
}

/// A generic-password item in the user's login Keychain.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone)]
pub struct KeychainStore {
    service: String,
    account: String,
}

#[cfg(target_os = "macos")]
impl KeychainStore {
    /// `errSecItemNotFound`.
    const NOT_FOUND: i32 = -25300;

    pub fn new(service: impl Into<String>, account: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            account: account.into(),
        }
    }
}

#[cfg(target_os = "macos")]
impl KeyStore for KeychainStore {
    fn load(&self) -> Result<Option<Zeroizing<Vec<u8>>>, KeyStoreError> {
        use security_framework::passwords::get_generic_password;
        match get_generic_password(&self.service, &self.account) {
            Ok(v) => Ok(Some(Zeroizing::new(v))),
            Err(e) if e.code() == Self::NOT_FOUND => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn store(&self, secret: &[u8]) -> Result<(), KeyStoreError> {
        security_framework::passwords::set_generic_password(&self.service, &self.account, secret)?;
        Ok(())
    }

    fn delete(&self) -> Result<(), KeyStoreError> {
        use security_framework::passwords::delete_generic_password;
        match delete_generic_password(&self.service, &self.account) {
            Err(e) if e.code() != Self::NOT_FOUND => Err(e.into()),
            _ => Ok(()),
        }
    }
}
