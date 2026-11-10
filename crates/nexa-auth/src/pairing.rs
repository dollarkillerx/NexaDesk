//! The pairing handshake (§9.2 step 3b) as an I/O-free state machine over `AuthStep`.
//!
//! ```text
//! viewer                                    host
//!   ClientHello{auth: Code|Password}  ─────▶
//!                                     ◀─────  AuthStep{salt, spake_msg}
//!   AuthStep{spake_msg, confirm}      ─────▶  verifies the viewer
//!                                     ◀─────  AuthStep{confirm}
//! ```
//!
//! Both SPAKE2 identities are the devices' TLS public keys. The confirmation key is
//! `HKDF(ikm = spake_key, salt = TLS exporter)`, so a relay that terminates TLS
//! separately to each side leaves the two ends with different keys.
//!
//! The viewer confirms first, so a host never reveals a value that tests a guess
//! until the viewer has already proven it knows the secret.
//!
//! The messages are not encrypted here; they travel inside the QUIC control stream.
//!
//! **Security review:** `spake2` 0.4 is pre-1.0 and not known to be audited. It does
//! not reject identity or low-order peer elements, and it copies the password into
//! buffers it does not zeroize. It must have an external security review before release.

use std::mem;

use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use nexa_wire::control::{Auth, AuthStep, ClientHello};
use rand_core::{OsRng, RngCore};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};
use thiserror::Error;
use zeroize::Zeroizing;

use crate::{
    identity::DevicePublicKey,
    secrets::{
        Attempt, HostPassword, OneTimeCode, PasswordGuard, SALT_LEN, Source, normalize_code,
        password_verifier,
    },
};

/// `ClientHello.proto` this build speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// One SPAKE2 side byte plus one Ed25519 point.
pub(crate) const SPAKE_MSG_LEN: usize = 33;
pub(crate) const CONFIRM_LEN: usize = 32;
const CONFIRM_KEY_INFO: &[u8] = b"nexa/1 pair confirm key";
const TRANSCRIPT_LABEL: &[u8] = b"nexa/1 pair transcript";
pub(crate) const VIEWER_TAG: &[u8] = b"nexa/1 confirm viewer";
pub(crate) const HOST_TAG: &[u8] = b"nexa/1 confirm host";

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PairError {
    #[error("malformed pairing message: {0}")]
    Malformed(&'static str),
    #[error("confirmation failed: wrong secret or a machine-in-the-middle")]
    ConfirmationFailed,
    #[error("pairing message arrived in the wrong state")]
    WrongState,
    #[error("this secret has been used or replaced")]
    SecretConsumed,
    #[error("too many pairing attempts in progress")]
    TooManyAttempts,
    #[error("password attempts are locked out for now")]
    LockedOut,
    #[error("not a valid one-time code")]
    InvalidCode,
    #[error("password is empty or too long")]
    InvalidPassword,
    #[error("peer presented this device's own identity")]
    SameIdentity,
    #[error("unsupported protocol version {0}")]
    UnsupportedProtocol(u32),
}

/// What the viewer asks for in `ClientHello` (§9.2 step 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelloAuth {
    Known,
    Code,
    Password,
}

/// Reads the requested authentication from a `ClientHello`.
pub fn hello_auth(hello: &ClientHello) -> Result<HelloAuth, PairError> {
    if hello.proto != PROTOCOL_VERSION {
        return Err(PairError::UnsupportedProtocol(hello.proto));
    }
    match hello.auth {
        Some(Auth::Known(true)) => Ok(HelloAuth::Known),
        Some(Auth::Code(true)) => Ok(HelloAuth::Code),
        Some(Auth::Password(true)) => Ok(HelloAuth::Password),
        _ => Err(PairError::Malformed("no authentication method")),
    }
}

/// How a pairing was authenticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    Code = 1,
    Password = 2,
}

/// Facts from the TLS layer that both ends must agree on.
pub struct PairingContext {
    viewer: DevicePublicKey,
    host: DevicePublicKey,
    exporter: Zeroizing<[u8; 32]>,
}

impl PairingContext {
    /// `viewer` and `host` are the keys the TLS handshake authenticated.
    pub fn new(
        viewer: DevicePublicKey,
        host: DevicePublicKey,
        exporter: [u8; 32],
    ) -> Result<Self, PairError> {
        if viewer == host {
            return Err(PairError::SameIdentity);
        }
        Ok(Self {
            viewer,
            host,
            exporter: Zeroizing::new(exporter),
        })
    }

    fn identities(&self) -> (Identity, Identity) {
        (
            Identity::new(self.viewer.as_bytes()),
            Identity::new(self.host.as_bytes()),
        )
    }
}

pub(crate) fn confirm_key(spake_key: &[u8], exporter: &[u8; 32]) -> Zeroizing<[u8; 32]> {
    let mut key = Zeroizing::new([0u8; 32]);
    Hkdf::<Sha256>::new(Some(exporter), spake_key)
        .expand(CONFIRM_KEY_INFO, key.as_mut_slice())
        .expect("32 bytes is a valid HKDF output length");
    key
}

fn transcript(
    ctx: &PairingContext,
    method: AuthMethod,
    salt: &[u8],
    viewer_msg: &[u8],
    host_msg: &[u8],
) -> Vec<u8> {
    transcript_of(
        ctx.viewer.as_bytes(),
        ctx.host.as_bytes(),
        method,
        salt,
        viewer_msg,
        host_msg,
    )
}

/// The transcript both confirmations cover, for any pair of 32-byte identities: device
/// keys here, DTLS certificate fingerprints for a browser ([`crate::browser`]).
pub(crate) fn transcript_of(
    viewer_id: &[u8],
    host_id: &[u8],
    method: AuthMethod,
    salt: &[u8],
    viewer_msg: &[u8],
    host_msg: &[u8],
) -> Vec<u8> {
    let fields: [&[u8]; 7] = [
        TRANSCRIPT_LABEL,
        &[method as u8],
        salt,
        viewer_id,
        host_id,
        viewer_msg,
        host_msg,
    ];
    let mut out = Vec::new();
    for field in fields {
        out.extend_from_slice(&(field.len() as u32).to_be_bytes());
        out.extend_from_slice(field);
    }
    out
}

fn confirm_mac(key: &[u8; 32], role: &[u8], transcript: &[u8]) -> Hmac<Sha256> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(role);
    mac.update(&[0]);
    mac.update(transcript);
    mac
}

pub(crate) fn confirm_tag(key: &[u8; 32], role: &[u8], transcript: &[u8]) -> Vec<u8> {
    confirm_mac(key, role, transcript)
        .finalize()
        .into_bytes()
        .to_vec()
}

/// Constant-time: `verify_slice` compares with `subtle`.
pub(crate) fn confirm_ok(key: &[u8; 32], role: &[u8], transcript: &[u8], tag: &[u8]) -> bool {
    confirm_mac(key, role, transcript).verify_slice(tag).is_ok()
}

enum ViewerSecret {
    Code(Zeroizing<String>),
    Password(Zeroizing<String>),
}

enum ViewerState {
    AwaitingOffer(ViewerSecret),
    AwaitingConfirm {
        key: Zeroizing<[u8; 32]>,
        transcript: Vec<u8>,
        method: AuthMethod,
    },
    Finished,
}

/// The viewer's side. Any error is terminal.
pub struct ViewerPairing {
    ctx: PairingContext,
    state: ViewerState,
}

/// What a viewer does after a step.
pub enum ViewerEvent {
    /// Send this to the host and wait for its reply.
    Send(AuthStep),
    /// The host is verified.
    Complete(ViewerVerified),
}

/// Proof that this viewer completed an exchange with `peer`. Constructed only here.
#[derive(Debug)]
pub struct ViewerVerified {
    peer: DevicePublicKey,
    method: AuthMethod,
}

impl ViewerVerified {
    pub fn peer(&self) -> DevicePublicKey {
        self.peer
    }
    pub fn method(&self) -> AuthMethod {
        self.method
    }
}

impl ViewerPairing {
    pub fn with_code(ctx: PairingContext, typed_code: &str) -> Result<Self, PairError> {
        let code = normalize_code(typed_code)?;
        Ok(Self {
            ctx,
            state: ViewerState::AwaitingOffer(ViewerSecret::Code(code)),
        })
    }

    pub fn with_password(ctx: PairingContext, password: &str) -> Result<Self, PairError> {
        if password.is_empty() {
            return Err(PairError::InvalidPassword);
        }
        Ok(Self {
            ctx,
            state: ViewerState::AwaitingOffer(ViewerSecret::Password(Zeroizing::new(
                password.to_string(),
            ))),
        })
    }

    /// Feeds one message from the host. In password mode the first call runs
    /// Argon2id, so keep it off async worker threads.
    pub fn on_step(&mut self, step: &AuthStep) -> Result<ViewerEvent, PairError> {
        match mem::replace(&mut self.state, ViewerState::Finished) {
            ViewerState::AwaitingOffer(secret) => self.on_offer(secret, step),
            ViewerState::AwaitingConfirm {
                key,
                transcript,
                method,
            } => {
                if !step.spake_msg.is_empty() || !step.salt.is_empty() {
                    return Err(PairError::Malformed("unexpected fields in host confirm"));
                }
                if step.confirm.len() != CONFIRM_LEN {
                    return Err(PairError::Malformed("confirm length"));
                }
                if !confirm_ok(&key, HOST_TAG, &transcript, &step.confirm) {
                    return Err(PairError::ConfirmationFailed);
                }
                Ok(ViewerEvent::Complete(ViewerVerified {
                    peer: self.ctx.host,
                    method,
                }))
            }
            ViewerState::Finished => Err(PairError::WrongState),
        }
    }

    fn on_offer(
        &mut self,
        secret: ViewerSecret,
        step: &AuthStep,
    ) -> Result<ViewerEvent, PairError> {
        if !step.confirm.is_empty() {
            return Err(PairError::Malformed("confirm in host offer"));
        }
        let salt: &[u8; SALT_LEN] = step
            .salt
            .as_slice()
            .try_into()
            .map_err(|_| PairError::Malformed("salt length"))?;
        if step.spake_msg.len() != SPAKE_MSG_LEN {
            return Err(PairError::Malformed("spake message length"));
        }
        let (pw, method) = match secret {
            ViewerSecret::Code(code) => {
                (Zeroizing::new(code.as_bytes().to_vec()), AuthMethod::Code)
            }
            ViewerSecret::Password(password) => (
                Zeroizing::new(password_verifier(&password, salt)?.to_vec()),
                AuthMethod::Password,
            ),
        };
        let (id_viewer, id_host) = self.ctx.identities();
        let (spake, viewer_msg) =
            Spake2::<Ed25519Group>::start_a(&Password::new(pw.as_slice()), &id_viewer, &id_host);
        let spake_key = Zeroizing::new(
            spake
                .finish(&step.spake_msg)
                .map_err(|_| PairError::Malformed("spake message"))?,
        );
        let key = confirm_key(&spake_key, &self.ctx.exporter);
        let transcript = transcript(&self.ctx, method, salt, &viewer_msg, &step.spake_msg);
        let confirm = confirm_tag(&key, VIEWER_TAG, &transcript);
        self.state = ViewerState::AwaitingConfirm {
            key,
            transcript,
            method,
        };
        Ok(ViewerEvent::Send(AuthStep {
            spake_msg: viewer_msg,
            confirm,
            salt: Vec::new(),
        }))
    }
}

enum HostState {
    AwaitingViewer {
        spake: Box<Spake2<Ed25519Group>>,
        host_msg: Vec<u8>,
        salt: [u8; SALT_LEN],
        attempt: Attempt,
    },
    Finished,
}

/// The host's side. Any error is terminal. Dropping it mid-exchange charges a failed attempt.
pub struct HostPairing {
    ctx: PairingContext,
    method: AuthMethod,
    state: HostState,
}

/// Proof that this host completed an exchange with `peer`. Constructed only here;
/// [`crate::access::decide`] and [`crate::store::PairingStore::add`] require it.
#[derive(Debug)]
pub struct HostVerified {
    peer: DevicePublicKey,
    method: AuthMethod,
}

impl HostVerified {
    pub fn peer(&self) -> DevicePublicKey {
        self.peer
    }
    pub fn method(&self) -> AuthMethod {
        self.method
    }
}

/// A successful exchange: send `reply` and then run the access decision.
#[derive(Debug)]
pub struct HostStepOutcome {
    pub reply: AuthStep,
    pub verified: HostVerified,
}

impl HostPairing {
    /// Charges an attempt against `code` and `source`, and returns the first message
    /// for the viewer. `source` is where the viewer really is (see [`Source`]).
    pub fn with_code(
        ctx: PairingContext,
        code: &OneTimeCode,
        source: Source,
    ) -> Result<(Self, AuthStep), PairError> {
        let (attempt, secret) = code.begin(source)?;
        let mut salt = [0u8; SALT_LEN];
        OsRng.fill_bytes(&mut salt);
        Ok(Self::start(
            ctx,
            AuthMethod::Code,
            secret.as_bytes(),
            salt,
            attempt,
        ))
    }

    /// Charges an attempt against `guard` and `source`, and returns the first message
    /// for the viewer.
    pub fn with_password(
        ctx: PairingContext,
        password: &HostPassword,
        guard: &PasswordGuard,
        source: Source,
    ) -> Result<(Self, AuthStep), PairError> {
        let attempt = guard.begin(source)?;
        Ok(Self::start(
            ctx,
            AuthMethod::Password,
            password.verifier(),
            *password.salt(),
            attempt,
        ))
    }

    fn start(
        ctx: PairingContext,
        method: AuthMethod,
        secret: &[u8],
        salt: [u8; SALT_LEN],
        attempt: Attempt,
    ) -> (Self, AuthStep) {
        let (id_viewer, id_host) = ctx.identities();
        let (spake, host_msg) =
            Spake2::<Ed25519Group>::start_b(&Password::new(secret), &id_viewer, &id_host);
        let offer = AuthStep {
            spake_msg: host_msg.clone(),
            confirm: Vec::new(),
            salt: salt.to_vec(),
        };
        let state = HostState::AwaitingViewer {
            spake: Box::new(spake),
            host_msg,
            salt,
            attempt,
        };
        (Self { ctx, method, state }, offer)
    }

    /// Feeds the viewer's message. On any error the attempt counts as failed.
    pub fn on_step(&mut self, step: &AuthStep) -> Result<HostStepOutcome, PairError> {
        let HostState::AwaitingViewer {
            spake,
            host_msg,
            salt,
            attempt,
        } = mem::replace(&mut self.state, HostState::Finished)
        else {
            return Err(PairError::WrongState);
        };
        if !step.salt.is_empty() {
            return Err(PairError::Malformed("salt in viewer step"));
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
        let key = confirm_key(&spake_key, &self.ctx.exporter);
        let transcript = transcript(&self.ctx, self.method, &salt, &step.spake_msg, &host_msg);
        if !confirm_ok(&key, VIEWER_TAG, &transcript, &step.confirm) {
            return Err(PairError::ConfirmationFailed);
        }
        if !attempt.succeed() {
            return Err(PairError::SecretConsumed);
        }
        Ok(HostStepOutcome {
            reply: AuthStep {
                spake_msg: Vec::new(),
                confirm: confirm_tag(&key, HOST_TAG, &transcript),
                salt: Vec::new(),
            },
            verified: HostVerified {
                peer: self.ctx.viewer,
                method: self.method,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::DeviceIdentity;

    fn ctx(viewer: &DeviceIdentity, host: &DeviceIdentity, exporter: u8) -> PairingContext {
        PairingContext::new(viewer.public_key(), host.public_key(), [exporter; 32]).unwrap()
    }

    /// A host that skips verifying the viewer and answers honestly from *its own* view
    /// of the exchange, which is the best a relay in the middle can do. The viewer's
    /// check must not pass. This is the viewer half of the MITM property.
    #[test]
    fn viewer_rejects_a_host_with_a_different_exporter() {
        let (viewer, host) = (DeviceIdentity::generate(), DeviceIdentity::generate());
        let code = OneTimeCode::generate();
        let shown = code.current();

        let host_ctx = ctx(&viewer, &host, 2);
        let (id_v, id_h) = host_ctx.identities();
        let (spake, host_msg) =
            Spake2::<Ed25519Group>::start_b(&Password::new(shown.as_bytes()), &id_v, &id_h);
        let salt = [9u8; SALT_LEN];

        let mut v = ViewerPairing::with_code(ctx(&viewer, &host, 1), &shown).unwrap();
        let ViewerEvent::Send(viewer_step) = v
            .on_step(&AuthStep {
                spake_msg: host_msg.clone(),
                confirm: vec![],
                salt: salt.to_vec(),
            })
            .unwrap()
        else {
            panic!("viewer should reply");
        };

        let spake_key = spake.finish(&viewer_step.spake_msg).unwrap();
        let key = confirm_key(&spake_key, &host_ctx.exporter);
        let t = transcript(
            &host_ctx,
            AuthMethod::Code,
            &salt,
            &viewer_step.spake_msg,
            &host_msg,
        );
        let forged = AuthStep {
            spake_msg: vec![],
            confirm: confirm_tag(&key, HOST_TAG, &t),
            salt: vec![],
        };
        assert_eq!(
            v.on_step(&forged).err(),
            Some(PairError::ConfirmationFailed)
        );
    }

    #[test]
    fn a_host_tag_is_not_a_viewer_tag() {
        // Without role separation a host could reflect the viewer's own confirmation.
        let key = [5u8; 32];
        let tag = confirm_tag(&key, VIEWER_TAG, b"t");
        assert!(confirm_ok(&key, VIEWER_TAG, b"t", &tag));
        assert!(!confirm_ok(&key, HOST_TAG, b"t", &tag));
    }

    #[test]
    fn the_exporter_changes_the_confirmation_key() {
        assert_ne!(*confirm_key(b"s", &[1; 32]), *confirm_key(b"s", &[2; 32]));
    }

    #[test]
    fn a_peer_with_our_own_key_is_refused() {
        let id = DeviceIdentity::generate();
        assert_eq!(
            PairingContext::new(id.public_key(), id.public_key(), [0; 32]).err(),
            Some(PairError::SameIdentity)
        );
    }

    #[test]
    fn hello_parsing() {
        let hello = |proto, auth| ClientHello {
            proto,
            device_name: String::new(),
            os: 0,
            caps: None,
            auth,
        };
        assert_eq!(
            hello_auth(&hello(1, Some(Auth::Known(true)))),
            Ok(HelloAuth::Known)
        );
        assert_eq!(
            hello_auth(&hello(1, Some(Auth::Code(true)))),
            Ok(HelloAuth::Code)
        );
        assert_eq!(
            hello_auth(&hello(1, Some(Auth::Password(true)))),
            Ok(HelloAuth::Password)
        );
        assert!(hello_auth(&hello(1, None)).is_err());
        assert!(hello_auth(&hello(1, Some(Auth::Known(false)))).is_err());
        assert_eq!(
            hello_auth(&hello(2, Some(Auth::Known(true)))),
            Err(PairError::UnsupportedProtocol(2))
        );
    }
}
