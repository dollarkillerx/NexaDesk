//! The access secret of a signaling server: a shared password that a self-hosted or
//! published server may ask every registering device for (docs/DEPLOY.md).
//!
//! A device never sends the secret. It sends an HMAC of what the server just told
//! it (the challenge nonce), the name it dialed and its own public key, so that
//! the proof is no use at another server, on another connection or for another
//! device, and a plain `ws://` connection on a loopback address, a log of the
//! traffic or a server that is not the one the person meant to reach learns
//! nothing they can replay. (A server that sees a proof can still try passwords
//! against it offline: pick one that is not guessable, or accept that the secret
//! is only a gate, as when it is published for everyone to use.)
//!
//! It only keeps the server's doors shut to strangers. Who may connect to which
//! device is decided between the two devices (§9), whatever the server says.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Length of a proof.
pub const ACCESS_PROOF_LEN: usize = 32;

const CONTEXT: &[u8] = b"nexa/1 signal access";

fn mac(secret: &str, nonce: &[u8], server_name: &str, pubkey: &[u8]) -> HmacSha256 {
    let mut m = <HmacSha256 as Mac>::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts any key length");
    // Length-prefixed, so that no part can run into the next.
    for part in [
        CONTEXT,
        nonce,
        server_name.to_ascii_lowercase().as_bytes(),
        pubkey,
    ] {
        m.update(&(part.len() as u32).to_be_bytes());
        m.update(part);
    }
    m
}

/// What a device sends in `Register.access_proof`.
pub fn access_proof(
    secret: &str,
    nonce: &[u8],
    server_name: &str,
    pubkey: &[u8],
) -> [u8; ACCESS_PROOF_LEN] {
    mac(secret, nonce, server_name, pubkey)
        .finalize()
        .into_bytes()
        .into()
}

/// Whether `proof` is the proof of `secret` for this registration. In constant time.
pub fn verify_access_proof(
    secret: &str,
    nonce: &[u8],
    server_name: &str,
    pubkey: &[u8],
    proof: &[u8],
) -> bool {
    mac(secret, nonce, server_name, pubkey)
        .verify_slice(proof)
        .is_ok()
}

/// Whether two secrets are the same, in a time that does not depend on how much of
/// them agrees or on their lengths (both are hashed first).
pub fn secrets_equal(a: &str, b: &str) -> bool {
    use sha2::Digest as _;
    use subtle::ConstantTimeEq as _;
    let (a, b) = (Sha256::digest(a.as_bytes()), Sha256::digest(b.as_bytes()));
    a.ct_eq(&b).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE: [u8; 32] = [7; 32];
    const KEY: [u8; 32] = [9; 32];

    #[test]
    fn the_right_secret_proves_and_others_do_not() {
        let proof = access_proof("correct horse", &NONCE, "signal.example.com", &KEY);
        assert!(verify_access_proof(
            "correct horse",
            &NONCE,
            "signal.example.com",
            &KEY,
            &proof
        ));
        for wrong in ["correct hors", "Correct horse", "correct horse ", ""] {
            assert!(
                !verify_access_proof(wrong, &NONCE, "signal.example.com", &KEY, &proof),
                "{wrong:?}"
            );
        }
        assert!(!verify_access_proof(
            "correct horse",
            &NONCE,
            "signal.example.com",
            &KEY,
            &[]
        ));
        assert!(!verify_access_proof(
            "correct horse",
            &NONCE,
            "signal.example.com",
            &KEY,
            &proof[..31]
        ));
    }

    /// A proof made for one connection, server or device is no use for another.
    #[test]
    fn a_proof_is_bound_to_its_registration() {
        let proof = access_proof("s", &NONCE, "signal.example.com", &KEY);
        assert!(!verify_access_proof(
            "s",
            &[8; 32],
            "signal.example.com",
            &KEY,
            &proof
        ));
        assert!(!verify_access_proof(
            "s",
            &NONCE,
            "other.example.com",
            &KEY,
            &proof
        ));
        assert!(!verify_access_proof(
            "s",
            &NONCE,
            "signal.example.com",
            &[1; 32],
            &proof
        ));
        // The name a client dialed may differ in case from the one configured.
        assert!(verify_access_proof(
            "s",
            &NONCE,
            "Signal.Example.COM",
            &KEY,
            &proof
        ));
    }

    #[test]
    fn secrets_compare_by_value() {
        assert!(secrets_equal("open sesame", "open sesame"));
        for other in ["open sesam", "open sesamee", "Open sesame", ""] {
            assert!(!secrets_equal("open sesame", other), "{other:?}");
        }
        assert!(secrets_equal("", ""));
    }

    #[test]
    fn parts_cannot_run_into_each_other() {
        let a = access_proof("s", b"ab", "c", &KEY);
        let b = access_proof("s", b"a", "bc", &KEY);
        assert_ne!(a, b);
    }
}
