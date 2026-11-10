//! The access decision (§9.3), a pure function of what has been proven.
//!
//! [`Evidence`] can only be built from a real store lookup or a real exchange, and
//! the result can be [`AccessDecision::Allow`] only for a pinned peer marked
//! unattended. Nothing in [`AccessPolicy`] can remove the consent prompt for an
//! unpaired peer.

use nexa_wire::control::Permissions;

use crate::{
    identity::DevicePublicKey,
    pairing::{AuthMethod, HostVerified},
    store::{Perms, PinnedPeer},
};

/// What the host has established about the peer on this connection.
#[derive(Debug, Clone, Copy)]
pub enum Evidence<'a> {
    /// Nothing, including `ClientHello{Known}` from a key that is not in the store.
    None,
    /// The peer's TLS key is in the pairing store.
    Pinned(PinnedPeer<'a>),
    /// The peer completed a code or password exchange on this connection.
    Exchange(&'a HostVerified),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccessPolicy {
    /// Whether password exchanges are accepted at all. Off means a correct password
    /// is refused.
    pub password_enabled: bool,
    /// §9.3: whether "save as paired" is pre-ticked after a password exchange.
    pub save_password_pairing: bool,
}

impl Default for AccessPolicy {
    fn default() -> Self {
        Self {
            password_enabled: true,
            save_password_pairing: true,
        }
    }
}

/// What to put in the consent dialog (§9.3).
#[derive(Debug, Clone, PartialEq)]
pub struct ConsentPrompt {
    /// Proposed permissions; the dialog may change them.
    pub perms: Permissions,
    /// Offer "save this device" (unpaired peers only).
    pub offer_save: bool,
    /// Whether that box starts ticked.
    pub save_default: bool,
    /// Offer "don't ask next time" (paired peers only).
    pub offer_skip_next_time: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    NoEvidence,
    /// The evidence is about a different key from the connection's TLS peer.
    IdentityMismatch,
    PasswordDisabled,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AccessDecision {
    Allow { perms: Permissions },
    NeedsConsent(ConsentPrompt),
    Deny(DenyReason),
}

/// New devices propose view-only until the person at the host grants more.
fn first_contact_perms() -> Permissions {
    Perms {
        view: true,
        ..Perms::default()
    }
    .into()
}

/// Decides access for the peer authenticated by the TLS handshake as `peer`.
pub fn decide(
    peer: &DevicePublicKey,
    evidence: Evidence<'_>,
    policy: &AccessPolicy,
) -> AccessDecision {
    match evidence {
        Evidence::None => AccessDecision::Deny(DenyReason::NoEvidence),
        Evidence::Pinned(pinned) => {
            let record = pinned.record();
            if record.pubkey != *peer {
                return AccessDecision::Deny(DenyReason::IdentityMismatch);
            }
            if record.unattended {
                AccessDecision::Allow {
                    perms: record.perms.into(),
                }
            } else {
                AccessDecision::NeedsConsent(ConsentPrompt {
                    perms: record.perms.into(),
                    offer_save: false,
                    save_default: false,
                    offer_skip_next_time: true,
                })
            }
        }
        Evidence::Exchange(verified) => {
            if verified.peer() != *peer {
                return AccessDecision::Deny(DenyReason::IdentityMismatch);
            }
            let save_default = match verified.method() {
                AuthMethod::Code => false,
                AuthMethod::Password if policy.password_enabled => policy.save_password_pairing,
                AuthMethod::Password => return AccessDecision::Deny(DenyReason::PasswordDisabled),
            };
            AccessDecision::NeedsConsent(ConsentPrompt {
                perms: first_contact_perms(),
                offer_save: true,
                save_default,
                offer_skip_next_time: false,
            })
        }
    }
}
