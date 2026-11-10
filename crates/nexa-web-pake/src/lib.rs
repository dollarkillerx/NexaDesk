//! The browser's half of NexaDesk pairing (docs/WEB_CLIENT.md §8), for WebAssembly.
//!
//! The host's half is `nexa_auth::browser`; read its notes first. This crate computes
//! exactly what `nexa_auth::browser::reference` does, without `nexa-auth`'s networking
//! dependencies, so it builds for `wasm32-unknown-unknown` with no JavaScript glue.
//!
//! Randomness: the page passes 32 bytes from `crypto.getRandomValues`, which seed a
//! ChaCha20 generator used for the one SPAKE2 scalar. Nothing here reads a clock or the OS.
//!
//! The WebAssembly interface is a handful of `extern "C"` functions over linear memory
//! (see [`ffi`]); `web/pake.js` in `nexa-web` wraps them.

use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand_chacha::ChaCha20Rng;
use rand_core::SeedableRng;
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use zeroize::Zeroizing;

// These must match nexa-auth (pairing.rs, browser.rs, secrets.rs); the tests check.
const CODE_LEN: usize = 8;
const CODE_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
pub const SALT_LEN: usize = 16;
pub const SPAKE_MSG_LEN: usize = 33;
pub const CONFIRM_LEN: usize = 32;
const METHOD_CODE: u8 = 1;
const CONFIRM_KEY_INFO: &[u8] = b"nexa/1 pair confirm key";
const TRANSCRIPT_LABEL: &[u8] = b"nexa/1 pair transcript";
const VIEWER_TAG: &[u8] = b"nexa/1 confirm viewer";
const HOST_TAG: &[u8] = b"nexa/1 confirm host";
const BROWSER_BINDING: &[u8] = b"nexa/1 browser dtls-sha256";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Not 8 characters of the code alphabet (spaces and hyphens are ignored).
    InvalidCode,
    /// A message of the wrong length or not a valid point.
    Malformed,
    /// The host could not prove it knows the code for these two fingerprints.
    HostNotVerified,
}

/// Canonical form of a typed code, as `nexa_auth::normalize_code`.
pub fn normalize_code(input: &str) -> Result<Zeroizing<String>, Error> {
    let code: Zeroizing<String> = Zeroizing::new(
        input
            .chars()
            .filter(|c| *c != ' ' && *c != '-')
            .map(|c| c.to_ascii_uppercase())
            .collect(),
    );
    if code.len() != CODE_LEN || !code.bytes().all(|b| CODE_ALPHABET.contains(&b)) {
        return Err(Error::InvalidCode);
    }
    Ok(code)
}

fn transcript(
    browser: &[u8; 32],
    host: &[u8; 32],
    salt: &[u8],
    browser_msg: &[u8],
    host_msg: &[u8],
) -> Vec<u8> {
    let fields: [&[u8]; 7] = [
        TRANSCRIPT_LABEL,
        &[METHOD_CODE],
        salt,
        browser,
        host,
        browser_msg,
        host_msg,
    ];
    let mut out = Vec::new();
    for field in fields {
        out.extend_from_slice(&(field.len() as u32).to_be_bytes());
        out.extend_from_slice(field);
    }
    out
}

fn mac(key: &[u8; 32], role: &[u8], transcript: &[u8]) -> Hmac<Sha256> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(role);
    mac.update(&[0]);
    mac.update(transcript);
    mac
}

/// What the browser keeps between its reply and the host's confirmation.
pub struct Pending {
    key: Zeroizing<[u8; 32]>,
    transcript: Vec<u8>,
}

impl Pending {
    /// Checks the host's confirmation in constant time.
    pub fn check_host(&self, confirm: &[u8]) -> Result<(), Error> {
        mac(&self.key, HOST_TAG, &self.transcript)
            .verify_slice(confirm)
            .map_err(|_| Error::HostNotVerified)
    }
}

/// Answers the host's offer. `browser` and `host` are the SHA-256 DTLS fingerprints as
/// *this* browser sees them: its own local description and the host's answer.
/// Returns `(spake_msg, confirm)` to send, and the state to check the host with.
pub fn answer(
    code: &str,
    browser: &[u8; 32],
    host: &[u8; 32],
    salt: &[u8],
    host_msg: &[u8],
    seed: [u8; 32],
) -> Result<(Vec<u8>, Vec<u8>, Pending), Error> {
    let code = normalize_code(code)?;
    if salt.len() != SALT_LEN || host_msg.len() != SPAKE_MSG_LEN {
        return Err(Error::Malformed);
    }
    let (spake, msg) = Spake2::<Ed25519Group>::start_a_with_rng(
        &Password::new(code.as_bytes()),
        &Identity::new(browser),
        &Identity::new(host),
        ChaCha20Rng::from_seed(seed),
    );
    let spake_key = Zeroizing::new(spake.finish(host_msg).map_err(|_| Error::Malformed)?);
    let binding: [u8; 32] = Sha256::digest(BROWSER_BINDING).into();
    let mut key = Zeroizing::new([0u8; 32]);
    Hkdf::<Sha256>::new(Some(&binding), &spake_key)
        .expand(CONFIRM_KEY_INFO, key.as_mut_slice())
        .expect("32 bytes is a valid HKDF output length");
    let transcript = transcript(browser, host, salt, &msg, host_msg);
    let confirm = mac(&key, VIEWER_TAG, &transcript)
        .finalize()
        .into_bytes()
        .to_vec();
    Ok((msg, confirm, Pending { key, transcript }))
}

/// The WebAssembly interface. One exchange at a time per instance, which is one page.
///
/// ```text
/// p = nx_alloc(n)                       memory for inputs and outputs
/// nx_answer(code, code_len, browser32, host32, salt16, host_msg33, seed32, out65) -> status
///     out65 = spake_msg (33) || confirm (32)
/// nx_check_host(confirm32) -> status    then forgets the exchange
/// status: 0 ok, 1 invalid code, 2 malformed, 3 host not verified, 4 no exchange
/// ```
pub mod ffi {
    use std::sync::Mutex;

    use super::*;

    static PENDING: Mutex<Option<Pending>> = Mutex::new(None);

    fn status(e: Error) -> u32 {
        match e {
            Error::InvalidCode => 1,
            Error::Malformed => 2,
            Error::HostNotVerified => 3,
        }
    }

    /// Allocates `len` bytes for the page to write into. Never freed: a page makes a
    /// handful of small allocations per exchange.
    #[unsafe(no_mangle)]
    pub extern "C" fn nx_alloc(len: usize) -> *mut u8 {
        let mut v = vec![0u8; len].into_boxed_slice();
        let p = v.as_mut_ptr();
        std::mem::forget(v);
        p
    }

    /// # Safety
    /// Every pointer must point at the stated number of bytes in this module's memory.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn nx_answer(
        code: *const u8,
        code_len: usize,
        browser: *const u8,
        host: *const u8,
        salt: *const u8,
        host_msg: *const u8,
        seed: *const u8,
        out: *mut u8,
    ) -> u32 {
        let (code, browser, host, salt, host_msg, seed) = unsafe {
            (
                std::slice::from_raw_parts(code, code_len),
                &*(browser as *const [u8; 32]),
                &*(host as *const [u8; 32]),
                std::slice::from_raw_parts(salt, SALT_LEN),
                std::slice::from_raw_parts(host_msg, SPAKE_MSG_LEN),
                *(seed as *const [u8; 32]),
            )
        };
        let Ok(code) = std::str::from_utf8(code) else {
            return 1;
        };
        match answer(code, browser, host, salt, host_msg, seed) {
            Ok((msg, confirm, pending)) => {
                let out =
                    unsafe { std::slice::from_raw_parts_mut(out, SPAKE_MSG_LEN + CONFIRM_LEN) };
                out[..SPAKE_MSG_LEN].copy_from_slice(&msg);
                out[SPAKE_MSG_LEN..].copy_from_slice(&confirm);
                *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(pending);
                0
            }
            Err(e) => status(e),
        }
    }

    /// # Safety
    /// `confirm` must point at 32 bytes in this module's memory.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn nx_check_host(confirm: *const u8) -> u32 {
        let confirm = unsafe { std::slice::from_raw_parts(confirm, CONFIRM_LEN) };
        match PENDING.lock().unwrap_or_else(|e| e.into_inner()).take() {
            Some(p) => match p.check_host(confirm) {
                Ok(()) => 0,
                Err(e) => status(e),
            },
            None => 4,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use nexa_auth::{
        OneTimeCode, Source,
        browser::{BrowserPairing, DtlsFingerprint, browser_binding},
    };
    use nexa_wire::control::AuthStep;

    use super::*;

    fn source(n: u8) -> Source {
        Source::of(IpAddr::V4(Ipv4Addr::new(192, 168, 9, n)))
    }

    const B: [u8; 32] = [0xB0; 32];
    const H: [u8; 32] = [0x40; 32];

    #[test]
    fn constants_match_the_host() {
        assert_eq!(
            Sha256::digest(BROWSER_BINDING).as_slice(),
            browser_binding().as_slice()
        );
    }

    #[test]
    fn pairs_with_the_real_host() {
        let code = OneTimeCode::generate();
        let shown = code.current();
        let (mut host, offer) =
            BrowserPairing::with_code(DtlsFingerprint(B), DtlsFingerprint(H), &code, source(1))
                .unwrap();
        // A person types it with a hyphen and in lower case.
        let typed = format!("{}-{}", &shown[..4], &shown[4..]).to_lowercase();
        let (msg, confirm, pending) =
            answer(&typed, &B, &H, &offer.salt, &offer.spake_msg, [7; 32]).unwrap();
        let (host_step, verified) = host
            .on_step(&AuthStep {
                spake_msg: msg,
                confirm,
                salt: vec![],
            })
            .unwrap();
        assert_eq!(verified.fingerprint(), DtlsFingerprint(B));
        pending.check_host(&host_step.confirm).unwrap();
    }

    #[test]
    fn the_ffi_does_the_same() {
        let code = OneTimeCode::generate();
        let shown = code.current();
        let (mut host, offer) =
            BrowserPairing::with_code(DtlsFingerprint(B), DtlsFingerprint(H), &code, source(2))
                .unwrap();
        let mut out = [0u8; 65];
        let seed = [9u8; 32];
        let st = unsafe {
            ffi::nx_answer(
                shown.as_ptr(),
                shown.len(),
                B.as_ptr(),
                H.as_ptr(),
                offer.salt.as_ptr(),
                offer.spake_msg.as_ptr(),
                seed.as_ptr(),
                out.as_mut_ptr(),
            )
        };
        assert_eq!(st, 0);
        let (host_step, _) = host
            .on_step(&AuthStep {
                spake_msg: out[..33].to_vec(),
                confirm: out[33..].to_vec(),
                salt: vec![],
            })
            .unwrap();
        assert_eq!(unsafe { ffi::nx_check_host(host_step.confirm.as_ptr()) }, 0);
        // The exchange is forgotten after one check.
        assert_eq!(unsafe { ffi::nx_check_host(host_step.confirm.as_ptr()) }, 4);
    }

    #[test]
    fn a_host_with_other_fingerprints_is_not_believed() {
        let code = OneTimeCode::generate();
        let shown = code.current();
        let mitm = [0x66; 32];
        // The host sees the attacker's fingerprint in place of ours: it rejects us, and
        // even its honest-looking reply could not pass our check.
        let (mut host, offer) =
            BrowserPairing::with_code(DtlsFingerprint(mitm), DtlsFingerprint(H), &code, source(3))
                .unwrap();
        let (msg, confirm, pending) =
            answer(&shown, &B, &H, &offer.salt, &offer.spake_msg, [1; 32]).unwrap();
        assert!(
            host.on_step(&AuthStep {
                spake_msg: msg,
                confirm,
                salt: vec![]
            })
            .is_err()
        );
        assert_eq!(pending.check_host(&[0; 32]), Err(Error::HostNotVerified));
    }

    #[test]
    fn bad_input_is_refused() {
        assert_eq!(
            answer("ABC", &B, &H, &[0; 16], &[0; 33], [0; 32]).err(),
            Some(Error::InvalidCode)
        );
        assert_eq!(
            answer("ABCD-EFG1", &B, &H, &[0; 16], &[0; 33], [0; 32]).err(),
            Some(Error::InvalidCode)
        );
        assert_eq!(
            answer("ABCD-EFGH", &B, &H, &[0; 15], &[0; 33], [0; 32]).err(),
            Some(Error::Malformed)
        );
    }
}
