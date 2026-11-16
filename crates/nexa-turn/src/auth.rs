//! Credentials. TURN's long-term mechanism, with passwords the server derives from a
//! secret instead of storing them ("TURN REST API" style, as coturn's
//! `use-auth-secret`): the username is `<expiry>:<label>`, the password the base64 of
//! HMAC-SHA1(secret, username). The signaling server hands a pair to each end of one
//! browser session; nobody can make more without the secret, and they expire.

use data_encoding::BASE64;
use hmac::{Hmac, KeyInit, Mac};
use md5::{Digest, Md5};
use sha1::Sha1;

/// A username and password for one use of the TURN server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

/// Credentials valid until `expires` (Unix seconds).
pub fn issue(secret: &[u8], label: &str, expires: u64) -> Credentials {
    let label: String = label
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(32)
        .collect();
    let username = format!("{expires}:{label}");
    let password = password_for(secret, &username);
    Credentials { username, password }
}

pub fn password_for(secret: &[u8], username: &str) -> String {
    let mut mac = <Hmac<Sha1> as KeyInit>::new_from_slice(secret).expect("any key length");
    mac.update(username.as_bytes());
    BASE64.encode(&mac.finalize().into_bytes())
}

/// When a username stops being accepted, if it is one of ours.
pub fn expiry(username: &str) -> Option<u64> {
    username.split(':').next()?.parse().ok()
}

/// RFC 8489 §9.2.2: MD5(username ":" realm ":" password).
pub fn long_term_key(username: &str, realm: &str, password: &str) -> [u8; 16] {
    let mut h = Md5::new();
    h.update(username.as_bytes());
    h.update(b":");
    h.update(realm.as_bytes());
    h.update(b":");
    h.update(password.as_bytes());
    h.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issued_credentials_check_out() {
        let c = issue(b"secret", "browser 7!", 1_800_000_000);
        assert_eq!(c.username, "1800000000:browser7");
        assert_eq!(expiry(&c.username), Some(1_800_000_000));
        assert_eq!(password_for(b"secret", &c.username), c.password);
        assert_ne!(password_for(b"other", &c.username), c.password);
        assert_eq!(expiry("nope"), None);
    }
}
