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

/// A 0600 file, replaced atomically. Unix only: on other platforms every operation
/// fails rather than leave a private key under default permissions. §9.1 calls for
/// DPAPI on Windows, which is not implemented here.
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

#[cfg(not(unix))]
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
