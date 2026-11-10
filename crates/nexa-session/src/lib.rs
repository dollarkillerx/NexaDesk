//! Wires capture, encode, transport, decode and present together (§1, §6), and owns
//! the session protocol on top of QUIC (§3, §9.2). Contains no platform code: every
//! backend arrives through a trait.

pub mod clip;
pub mod feedback;
pub mod framed;
pub mod handshake;
pub mod host;
pub mod input;
pub mod mailbox;
pub mod negotiate;
pub mod proto;
pub mod rendezvous;
pub mod viewer;

use std::time::{Duration, Instant};

use thiserror::Error;

/// Spins for the last stretch, since OS sleeps overshoot by more than the pacing step.
pub(crate) fn wait_until(t: Instant) {
    loop {
        let now = Instant::now();
        if now >= t {
            return;
        }
        let left = t - now;
        if left > Duration::from_millis(2) {
            std::thread::sleep(left - Duration::from_millis(1));
        } else {
            std::hint::spin_loop();
        }
    }
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("timed out")]
    Timeout,
    #[error("protocol error: {0}")]
    Protocol(&'static str),
    #[error(transparent)]
    Connection(#[from] quinn::ConnectionError),
    #[error("stream error: {0}")]
    Stream(String),
    /// The other side refused the session.
    #[error("refused by peer ({reason:?}): {message}")]
    Rejected {
        reason: proto::RejectReason,
        message: String,
    },
    /// This side refused the session.
    #[error("refused ({0:?})")]
    Refused(proto::RejectReason),
    #[error(transparent)]
    Auth(nexa_auth::PairError),
    #[error(transparent)]
    Tls(nexa_auth::TlsError),
}
