//! The signaling server's access key, kept in the system's secret store rather than
//! in `config.json` (docs/MCP.md §3.4): the Keychain on a Mac, a file encrypted with
//! DPAPI for the signed-in user on Windows (`nexa_auth::FileKeyStore`).
//!
//! `nexa-mcp set-signal-secret` reads it from standard input, so it is never on a
//! command line or in a shell's history; `clear-signal-secret` forgets it.

use std::path::Path;

use nexa_auth::KeyStore;

/// The store for this agent's access key. Apart from the app's: the app keeps its
/// own for its own server.
pub fn store(data_dir: &Path) -> Box<dyn KeyStore> {
    #[cfg(target_os = "macos")]
    {
        let _ = data_dir;
        Box::new(nexa_auth::KeychainStore::new(
            "app.nexadesk",
            "mcp-signal-secret",
        ))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Box::new(nexa_auth::FileKeyStore::new(data_dir.join("signal-secret")))
    }
}

/// The key, or empty when none is kept (or it cannot be read).
pub fn load(store: &dyn KeyStore) -> String {
    match store.load() {
        Ok(Some(bytes)) => String::from_utf8_lossy(&bytes).trim().to_owned(),
        _ => String::new(),
    }
}

/// Keeps `key`, trimmed. An empty key is refused: `clear` forgets one.
pub fn save(store: &dyn KeyStore, key: &str) -> Result<(), String> {
    let key = key.trim();
    if key.is_empty() {
        return Err("no key given".into());
    }
    store.store(key.as_bytes()).map_err(|e| e.to_string())
}

pub fn clear(store: &dyn KeyStore) -> Result<(), String> {
    store.delete().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_kept_trimmed_read_back_and_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let s = nexa_auth::FileKeyStore::new(dir.path().join("signal-secret"));
        assert_eq!(load(&s), "");
        assert!(save(&s, "  \n").is_err());
        save(&s, " open sesame\n").unwrap();
        assert_eq!(load(&s), "open sesame");
        clear(&s).unwrap();
        assert_eq!(load(&s), "");
    }
}
