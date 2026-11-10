//! Pairing a web browser (docs/WEB_CLIENT.md §8): the host's side.
//!
//! A browser has no device key and no mutual TLS. What it does have is WebRTC's DTLS
//! certificate, whose SHA-256 fingerprint each side writes into its SDP. The exchange is
//! the native one ([`crate::pairing`]) with those two fingerprints as the SPAKE2 and
//! transcript identities:
//!
//! ```text
//! browser                                     host
//!   offer SDP (its fingerprint)        ─────▶
//!                                      ◀─────  answer SDP (host fingerprint),
//!                                              AuthStep{salt, spake_msg}
//!   AuthStep{spake_msg, confirm}       ─────▶  verifies the browser
//!                                      ◀─────  AuthStep{confirm}
//! ```
//!
//! Each side uses the fingerprints *it* sees: its own, and the one in the other's SDP. If
//! the signaling path rewrote either SDP to put itself in the middle of DTLS, the two
//! transcripts differ and neither confirmation verifies, so knowing the one-time code is
//! not enough without also being the DTLS endpoint the other side checks.
//!
//! The confirmation key is salted with [`BROWSER_BINDING`] instead of a TLS exporter, so a
//! browser exchange can never yield a key valid for a native one, or the reverse.
//!
//! The browser half is `crates/nexa-web-pake` (WebAssembly), tested against this module.
//! Only one-time codes are accepted: a browser cannot run Argon2 at an acceptable cost.
//!
//! **Security review:** everything said on [`crate::pairing`] applies here too.

use std::mem;

use nexa_wire::control::AuthStep;
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use zeroize::Zeroizing;

use crate::{
    pairing::{
        AuthMethod, CONFIRM_LEN, HOST_TAG, PairError, SPAKE_MSG_LEN, VIEWER_TAG, confirm_key,
        confirm_ok, confirm_tag, transcript_of,
    },
    secrets::{Attempt, OneTimeCode, SALT_LEN, Source},
};

/// Domain separation for browser exchanges; its SHA-256 salts the confirmation key.
pub const BROWSER_BINDING: &[u8] = b"nexa/1 browser dtls-sha256";

/// The HKDF salt for browser confirmation keys.
pub fn browser_binding() -> [u8; 32] {
    Sha256::digest(BROWSER_BINDING).into()
}

/// A SHA-256 DTLS certificate fingerprint, as written in SDP (`a=fingerprint:sha-256`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtlsFingerprint(pub [u8; 32]);

impl DtlsFingerprint {
    /// Parses `AB:CD:...` (32 colon-separated hex bytes, either case).
    pub fn parse_sdp(value: &str) -> Option<Self> {
        let mut out = [0u8; 32];
        let mut parts = value.trim().split(':');
        for byte in &mut out {
            let part = parts.next()?;
            if part.len() != 2 {
                return None;
            }
            *byte = u8::from_str_radix(part, 16).ok()?;
        }
        parts.next().is_none().then_some(Self(out))
    }

    /// The `sha-256` fingerprint in an SDP. Every media section of a WebRTC SDP shares
    /// one DTLS transport (BUNDLE), so differing fingerprints are refused.
    pub fn from_sdp(sdp: &str) -> Option<Self> {
        let mut found: Option<Self> = None;
        for line in sdp.lines() {
            let Some(rest) = line.trim().strip_prefix("a=fingerprint:") else {
                continue;
            };
            let (algo, value) = rest.split_once(' ')?;
            if !algo.eq_ignore_ascii_case("sha-256") {
                continue;
            }
            let fp = Self::parse_sdp(value)?;
            match found {
                Some(prev) if prev != fp => return None,
                _ => found = Some(fp),
            }
        }
        found
    }
}

enum State {
    AwaitingBrowser {
        spake: Box<Spake2<Ed25519Group>>,
        host_msg: Vec<u8>,
        salt: [u8; SALT_LEN],
        attempt: Attempt,
    },
    Finished,
}

/// The host's side of a browser exchange. Any error is terminal; dropping it
/// mid-exchange charges a failed attempt, as for native viewers.
pub struct BrowserPairing {
    browser: DtlsFingerprint,
    host: DtlsFingerprint,
    state: State,
}

/// Proof that the browser holding the DTLS certificate `fingerprint` knew the code.
/// Constructed only here.
#[derive(Debug)]
pub struct BrowserVerified {
    fingerprint: DtlsFingerprint,
}

impl BrowserVerified {
    pub fn fingerprint(&self) -> DtlsFingerprint {
        self.fingerprint
    }
}

fn identities(browser: &DtlsFingerprint, host: &DtlsFingerprint) -> (Identity, Identity) {
    (Identity::new(&browser.0), Identity::new(&host.0))
}

impl BrowserPairing {
    /// Charges an attempt against `code` and `source`, and returns the first message.
    /// `browser` is the fingerprint in the browser's offer, `host` our own.
    pub fn with_code(
        browser: DtlsFingerprint,
        host: DtlsFingerprint,
        code: &OneTimeCode,
        source: Source,
    ) -> Result<(Self, AuthStep), PairError> {
        if browser == host {
            return Err(PairError::SameIdentity);
        }
        let (attempt, secret) = code.begin(source)?;
        let mut salt = [0u8; SALT_LEN];
        OsRng.fill_bytes(&mut salt);
        let (id_browser, id_host) = identities(&browser, &host);
        let (spake, host_msg) = Spake2::<Ed25519Group>::start_b(
            &Password::new(secret.as_bytes()),
            &id_browser,
            &id_host,
        );
        let offer = AuthStep {
            spake_msg: host_msg.clone(),
            confirm: Vec::new(),
            salt: salt.to_vec(),
        };
        let state = State::AwaitingBrowser {
            spake: Box::new(spake),
            host_msg,
            salt,
            attempt,
        };
        Ok((
            Self {
                browser,
                host,
                state,
            },
            offer,
        ))
    }

    /// Feeds the browser's message. On success, send the returned step back; on any error
    /// the attempt counts as failed.
    pub fn on_step(&mut self, step: &AuthStep) -> Result<(AuthStep, BrowserVerified), PairError> {
        let State::AwaitingBrowser {
            spake,
            host_msg,
            salt,
            attempt,
        } = mem::replace(&mut self.state, State::Finished)
        else {
            return Err(PairError::WrongState);
        };
        if !step.salt.is_empty() {
            return Err(PairError::Malformed("salt in browser step"));
        }
        if step.spake_msg.len() != SPAKE_MSG_LEN {
            return Err(PairError::Malformed("spake message length"));
        }
        if step.confirm.len() != CONFIRM_LEN {
            return Err(PairError::Malformed("confirm length"));
        }
        let spake_key = Zeroizing::new(
            spake
                .finish(&step.spake_msg)
                .map_err(|_| PairError::Malformed("spake message"))?,
        );
        let key = confirm_key(&spake_key, &browser_binding());
        let transcript = transcript_of(
            &self.browser.0,
            &self.host.0,
            AuthMethod::Code,
            &salt,
            &step.spake_msg,
            &host_msg,
        );
        if !confirm_ok(&key, VIEWER_TAG, &transcript, &step.confirm) {
            return Err(PairError::ConfirmationFailed);
        }
        if !attempt.succeed() {
            return Err(PairError::SecretConsumed);
        }
        Ok((
            AuthStep {
                spake_msg: Vec::new(),
                confirm: confirm_tag(&key, HOST_TAG, &transcript),
                salt: Vec::new(),
            },
            BrowserVerified {
                fingerprint: self.browser,
            },
        ))
    }
}

/// The browser's side, for tests here and in `nexa-web-pake`, which must compute the
/// same values. Not used by the host.
#[doc(hidden)]
pub mod reference {
    use super::*;

    pub struct Viewer {
        pub key: Zeroizing<[u8; 32]>,
        pub transcript: Vec<u8>,
    }

    /// Answers the host's offer: returns the reply step and what is needed to check
    /// the host's confirmation.
    pub fn answer(
        code: &str,
        browser: &DtlsFingerprint,
        host: &DtlsFingerprint,
        offer: &AuthStep,
    ) -> Result<(AuthStep, Viewer), PairError> {
        let code = crate::secrets::normalize_code(code)?;
        let (id_browser, id_host) = identities(browser, host);
        let (spake, msg) =
            Spake2::<Ed25519Group>::start_a(&Password::new(code.as_bytes()), &id_browser, &id_host);
        let spake_key = spake
            .finish(&offer.spake_msg)
            .map_err(|_| PairError::Malformed("spake message"))?;
        let key = confirm_key(&spake_key, &browser_binding());
        let transcript = transcript_of(
            &browser.0,
            &host.0,
            AuthMethod::Code,
            &offer.salt,
            &msg,
            &offer.spake_msg,
        );
        let confirm = confirm_tag(&key, VIEWER_TAG, &transcript);
        Ok((
            AuthStep {
                spake_msg: msg,
                confirm,
                salt: Vec::new(),
            },
            Viewer { key, transcript },
        ))
    }

    pub fn host_ok(viewer: &Viewer, host_step: &AuthStep) -> bool {
        confirm_ok(
            &viewer.key,
            HOST_TAG,
            &viewer.transcript,
            &host_step.confirm,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;
    use crate::pairing::PairError;

    fn source(n: u8) -> Source {
        Source::of(IpAddr::V4(Ipv4Addr::new(10, 0, 0, n)))
    }

    const B: DtlsFingerprint = DtlsFingerprint([1; 32]);
    const H: DtlsFingerprint = DtlsFingerprint([2; 32]);

    #[test]
    fn a_browser_with_the_code_pairs_and_checks_the_host() {
        let code = OneTimeCode::generate();
        let (mut host, offer) = BrowserPairing::with_code(B, H, &code, source(1)).unwrap();
        let (reply, viewer) = reference::answer(&code.current(), &B, &H, &offer).unwrap();
        let (host_step, verified) = host.on_step(&reply).unwrap();
        assert_eq!(verified.fingerprint(), B);
        assert!(reference::host_ok(&viewer, &host_step));
    }

    #[test]
    fn a_wrong_code_fails_and_is_charged() {
        let code = OneTimeCode::generate();
        let (mut host, offer) = BrowserPairing::with_code(B, H, &code, source(2)).unwrap();
        let (reply, _) = reference::answer("AAAA-AAAA", &B, &H, &offer).unwrap();
        assert_eq!(
            host.on_step(&reply).unwrap_err(),
            PairError::ConfirmationFailed
        );
        // Three per source: two more, then refused.
        for _ in 0..2 {
            BrowserPairing::with_code(B, H, &code, source(2)).unwrap();
        }
        assert!(matches!(
            BrowserPairing::with_code(B, H, &code, source(2)),
            Err(PairError::TooManyAttempts)
        ));
    }

    /// Signaling in the middle swapped a fingerprint for its own: the host computes with
    /// the attacker's, the browser with its own, so the browser's proof does not verify
    /// even though it used the right code.
    #[test]
    fn a_rewritten_fingerprint_fails() {
        let mitm = DtlsFingerprint([3; 32]);
        for (host_view_browser, host_view_host, n) in [(mitm, H, 3), (B, mitm, 4)] {
            let code = OneTimeCode::generate();
            let (mut host, offer) =
                BrowserPairing::with_code(host_view_browser, host_view_host, &code, source(n))
                    .unwrap();
            let (reply, _) = reference::answer(&code.current(), &B, &H, &offer).unwrap();
            assert_eq!(
                host.on_step(&reply).unwrap_err(),
                PairError::ConfirmationFailed
            );
        }
    }

    #[test]
    fn the_code_works_once() {
        let code = OneTimeCode::generate();
        let shown = code.current();
        let (mut host, offer) = BrowserPairing::with_code(B, H, &code, source(5)).unwrap();
        let (reply, _) = reference::answer(&shown, &B, &H, &offer).unwrap();
        host.on_step(&reply).unwrap();
        // The same code again, even with a correct exchange, is consumed.
        match BrowserPairing::with_code(B, H, &code, source(6)) {
            Ok((mut again, offer)) => {
                let (reply, _) = reference::answer(&shown, &B, &H, &offer).unwrap();
                assert!(again.on_step(&reply).is_err());
            }
            Err(_) => {}
        }
    }

    #[test]
    fn identical_fingerprints_are_refused() {
        let code = OneTimeCode::generate();
        assert!(matches!(
            BrowserPairing::with_code(B, B, &code, source(7)),
            Err(PairError::SameIdentity)
        ));
    }

    #[test]
    fn fingerprints_are_read_from_sdp() {
        let hex: Vec<String> = (0..32).map(|i| format!("{:02X}", i * 7)).collect();
        let line = format!("a=fingerprint:sha-256 {}", hex.join(":"));
        let sdp = format!("v=0\r\n{line}\r\nm=video 9\r\n{line}\r\n");
        let fp = DtlsFingerprint::from_sdp(&sdp).unwrap();
        assert_eq!(fp.0[1], 7);
        let other = line.replace("00:07", "00:08");
        assert!(DtlsFingerprint::from_sdp(&format!("{line}\r\n{other}\r\n")).is_none());
        assert!(DtlsFingerprint::from_sdp("a=fingerprint:sha-1 00:11").is_none());
        assert!(DtlsFingerprint::parse_sdp("00:11").is_none());
    }
}
