//! Device identity, pairing and access control. See docs/DETAILED_DESIGN.md §9.
//!
//! The pieces, in the order a connection meets them:
//!
//! 1. [`tls`]: mutual TLS 1.3 over QUIC. Both sides present a self-signed
//!    Ed25519 certificate; the peer's public key is the identity.
//! 2. [`hello_auth`]: what the viewer asked for in `ClientHello`.
//! 3. [`pairing`]: SPAKE2 over `AuthStep`, bound to both public keys and to the
//!    TLS exporter, for peers that are not yet paired.
//! 4. [`store`]: the pinned peers.
//! 5. [`access`]: the decision, a pure function.
//!
//! **Security review:** the `spake2` crate (0.4, pre-1.0) must have an external
//! security review before release. See the note on [`pairing`].

pub mod access;
mod fsio;
pub mod identity;
pub mod keystore;
pub mod pairing;
pub mod secrets;
pub mod store;
pub mod tls;

pub use access::{AccessDecision, AccessPolicy, ConsentPrompt, DenyReason, Evidence, decide};
pub use identity::{DeviceId, DeviceIdentity, DevicePublicKey, IdentityError};
pub use keystore::{FileKeyStore, KeyStore, KeyStoreError};
pub use pairing::{
    AuthMethod, HelloAuth, HostPairing, HostStepOutcome, HostVerified, PROTOCOL_VERSION, PairError,
    PairingContext, ViewerEvent, ViewerPairing, ViewerVerified, hello_auth,
};
pub use secrets::{
    CODE_ATTEMPTS_PER_SOURCE, HostPassword, MAX_ATTEMPTS, OneTimeCode, PASSWORD_FAILURES_GLOBAL,
    PasswordGuard, Source, normalize_code,
};
pub use store::{PairingRecord, PairingStore, Perms, PinnedPeer, StoreError, VerifiedExchange};
pub use tls::{
    ALPN, PeerPolicy, TlsError, client_config, pair_exporter, peer_public_key, server_config,
};

#[cfg(target_os = "macos")]
pub use keystore::KeychainStore;
