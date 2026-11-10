//! The secrets a host accepts for pairing, and how many guesses it allows (§9.2).
//!
//! A guess is charged when an exchange *starts*, not when it fails. An attempt that
//! is abandoned, or dropped because the connection died, counts as a failure, and
//! attempts running at the same time cannot exceed the budget between them.

use std::{
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use argon2::{Algorithm, Argon2, Params, Version};
use rand_core::{OsRng, RngCore};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::pairing::PairError;

/// Failed guesses allowed per one-time code (§9.2) and per password lockout window.
pub const MAX_ATTEMPTS: u32 = 5;
pub const CODE_LEN: usize = 8;
pub const SALT_LEN: usize = 16;
const VERIFIER_LEN: usize = 32;
const STORED_PASSWORD_LEN: usize = SALT_LEN + VERIFIER_LEN;
const MAX_PASSWORD_LEN: usize = 1024;

/// RFC 4648 base32, no padding.
const CODE_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

// Fixed in code because `AuthStep` has no room to carry them: both ends must agree.
const ARGON2_MEMORY_KIB: u32 = 64 * 1024;
const ARGON2_PASSES: u32 = 3;
const ARGON2_LANES: u32 = 1;

const PASSWORD_LOCKOUT: Duration = Duration::from_secs(30);
const PASSWORD_LOCKOUT_MAX: Duration = Duration::from_secs(3600);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic elsewhere must not stop attempts being charged.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Resolves an attempt against the limiter that issued it.
trait Settle: Send + Sync {
    /// Returns whether a successful attempt may stand.
    fn settle(&self, generation: u64, success: bool) -> bool;
}

/// One charged guess. Dropping it without [`Attempt::succeed`] records a failure.
#[must_use]
pub struct Attempt {
    owner: Arc<dyn Settle>,
    generation: u64,
    settled: bool,
}

impl Attempt {
    /// Returns false if the secret was already consumed, in which case the
    /// exchange must be treated as failed.
    pub(crate) fn succeed(mut self) -> bool {
        self.settled = true;
        self.owner.settle(self.generation, true)
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        if !self.settled {
            self.owner.settle(self.generation, false);
        }
    }
}

struct CodeState {
    code: Zeroizing<String>,
    generation: u64,
    failures: u32,
    in_flight: u32,
}

impl CodeState {
    fn rotate(&mut self) {
        self.code = random_code();
        self.generation += 1;
        self.failures = 0;
        self.in_flight = 0;
    }
}

struct CodeShared {
    state: Mutex<CodeState>,
}

impl Settle for CodeShared {
    fn settle(&self, generation: u64, success: bool) -> bool {
        let mut s = lock(&self.state);
        if generation != s.generation {
            return false;
        }
        s.in_flight -= 1;
        if success {
            s.rotate();
            return true;
        }
        s.failures += 1;
        if s.failures >= MAX_ATTEMPTS {
            s.rotate();
        }
        false
    }
}

/// The one-time code shown on the host. Single use, and replaced after
/// [`MAX_ATTEMPTS`] failures. Clones share state.
#[derive(Clone)]
pub struct OneTimeCode {
    shared: Arc<CodeShared>,
}

impl Default for OneTimeCode {
    fn default() -> Self {
        Self::generate()
    }
}

impl OneTimeCode {
    pub fn generate() -> Self {
        Self {
            shared: Arc::new(CodeShared {
                state: Mutex::new(CodeState {
                    code: random_code(),
                    generation: 0,
                    failures: 0,
                    in_flight: 0,
                }),
            }),
        }
    }

    /// The code to display. Changes after a success or the failure limit.
    pub fn current(&self) -> Zeroizing<String> {
        lock(&self.shared.state).code.clone()
    }

    /// Replaces the code, invalidating attempts in progress.
    pub fn rotate(&self) {
        lock(&self.shared.state).rotate();
    }

    pub(crate) fn begin(&self) -> Result<(Attempt, Zeroizing<String>), PairError> {
        let mut s = lock(&self.shared.state);
        if s.failures + s.in_flight >= MAX_ATTEMPTS {
            return Err(PairError::TooManyAttempts);
        }
        s.in_flight += 1;
        let attempt = Attempt {
            owner: self.shared.clone(),
            generation: s.generation,
            settled: false,
        };
        Ok((attempt, s.code.clone()))
    }
}

fn random_code() -> Zeroizing<String> {
    let mut raw = Zeroizing::new([0u8; 5]);
    OsRng.fill_bytes(raw.as_mut_slice());
    let bits = Zeroizing::new(raw.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b)));
    let mut code = Zeroizing::new(String::with_capacity(CODE_LEN));
    for i in (0..CODE_LEN).rev() {
        code.push(CODE_ALPHABET[((*bits >> (i * 5)) & 31) as usize] as char);
    }
    code
}

/// Canonical form of a code typed by a person: spaces and hyphens removed, upper case.
pub fn normalize_code(input: &str) -> Result<Zeroizing<String>, PairError> {
    let code: Zeroizing<String> = Zeroizing::new(
        input
            .chars()
            .filter(|c| *c != ' ' && *c != '-')
            .map(|c| c.to_ascii_uppercase())
            .collect(),
    );
    if code.len() != CODE_LEN || !code.bytes().all(|b| CODE_ALPHABET.contains(&b)) {
        return Err(PairError::InvalidCode);
    }
    Ok(code)
}

/// Argon2id(password, salt), the SPAKE2 password for password mode (§9.2).
pub(crate) fn password_verifier(
    password: &str,
    salt: &[u8; SALT_LEN],
) -> Result<Zeroizing<[u8; VERIFIER_LEN]>, PairError> {
    if password.is_empty() || password.len() > MAX_PASSWORD_LEN {
        return Err(PairError::InvalidPassword);
    }
    let params = Params::new(
        ARGON2_MEMORY_KIB,
        ARGON2_PASSES,
        ARGON2_LANES,
        Some(VERIFIER_LEN),
    )
    .map_err(|_| PairError::InvalidPassword)?;
    let mut out = Zeroizing::new([0u8; VERIFIER_LEN]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(password.as_bytes(), salt, out.as_mut_slice())
        .map_err(|_| PairError::InvalidPassword)?;
    Ok(out)
}

/// What the host keeps for password mode: the salt and the Argon2id output, never the
/// password. Persist [`Self::to_bytes`] in a [`crate::KeyStore`]: as SPAKE2 is a
/// balanced PAKE, the output is sufficient to authenticate to this host.
pub struct HostPassword {
    salt: [u8; SALT_LEN],
    verifier: Zeroizing<[u8; VERIFIER_LEN]>,
}

impl HostPassword {
    /// Runs Argon2id, so this takes a noticeable fraction of a second. Keep it off
    /// async worker threads.
    pub fn create(password: &str) -> Result<Self, PairError> {
        let mut salt = [0u8; SALT_LEN];
        OsRng.fill_bytes(&mut salt);
        let verifier = password_verifier(password, &salt)?;
        Ok(Self { salt, verifier })
    }

    pub fn to_bytes(&self) -> Zeroizing<Vec<u8>> {
        let mut out = Zeroizing::new(Vec::with_capacity(STORED_PASSWORD_LEN));
        out.extend_from_slice(&self.salt);
        out.extend_from_slice(self.verifier.as_slice());
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, PairError> {
        if bytes.len() != STORED_PASSWORD_LEN {
            return Err(PairError::InvalidPassword);
        }
        let (salt, verifier) = bytes.split_at(SALT_LEN);
        let mut stored = Self {
            salt: [0; SALT_LEN],
            verifier: Zeroizing::new([0; VERIFIER_LEN]),
        };
        stored.salt.copy_from_slice(salt);
        stored.verifier.copy_from_slice(verifier);
        Ok(stored)
    }

    pub(crate) fn salt(&self) -> &[u8; SALT_LEN] {
        &self.salt
    }

    pub(crate) fn verifier(&self) -> &[u8] {
        self.verifier.as_slice()
    }

    /// Whether `password` is the one this was created from. For local checks such as
    /// "enter the current password to change it"; remote peers go through SPAKE2.
    pub fn matches(&self, password: &str) -> bool {
        password_verifier(password, &self.salt)
            .is_ok_and(|v| bool::from(v.as_slice().ct_eq(self.verifier.as_slice())))
    }
}

struct PasswordState {
    failures: u32,
    in_flight: u32,
    lockouts: u32,
    locked_until: Option<Instant>,
}

struct PasswordShared {
    state: Mutex<PasswordState>,
    base_lockout: Duration,
}

impl Settle for PasswordShared {
    fn settle(&self, _generation: u64, success: bool) -> bool {
        let mut s = lock(&self.state);
        s.in_flight -= 1;
        if success {
            s.failures = 0;
            s.lockouts = 0;
            s.locked_until = None;
            return true;
        }
        s.failures += 1;
        if s.failures >= MAX_ATTEMPTS {
            s.failures = 0;
            let doubling = 1u32 << s.lockouts.min(16);
            s.lockouts += 1;
            let lockout = self
                .base_lockout
                .saturating_mul(doubling)
                .min(PASSWORD_LOCKOUT_MAX);
            s.locked_until = Some(Instant::now() + lockout);
        }
        false
    }
}

/// Online-guessing limit for the unattended password: after [`MAX_ATTEMPTS`] failures
/// the host refuses password exchanges for a period that doubles each time, up to an
/// hour. A password cannot be rotated the way a code can. The state is in memory, so
/// a restart clears it. Clones share state.
#[derive(Clone)]
pub struct PasswordGuard {
    shared: Arc<PasswordShared>,
}

impl Default for PasswordGuard {
    fn default() -> Self {
        Self::with_lockout(PASSWORD_LOCKOUT)
    }
}

impl PasswordGuard {
    /// `base_lockout` is the first lockout period.
    pub fn with_lockout(base_lockout: Duration) -> Self {
        Self {
            shared: Arc::new(PasswordShared {
                state: Mutex::new(PasswordState {
                    failures: 0,
                    in_flight: 0,
                    lockouts: 0,
                    locked_until: None,
                }),
                base_lockout,
            }),
        }
    }

    pub(crate) fn begin(&self) -> Result<Attempt, PairError> {
        let mut s = lock(&self.shared.state);
        if let Some(until) = s.locked_until {
            if Instant::now() < until {
                return Err(PairError::LockedOut);
            }
            s.locked_until = None;
        }
        if s.failures + s.in_flight >= MAX_ATTEMPTS {
            return Err(PairError::TooManyAttempts);
        }
        s.in_flight += 1;
        Ok(Attempt {
            owner: self.shared.clone(),
            generation: 0,
            settled: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_shape() {
        for _ in 0..100 {
            let code = OneTimeCode::generate().current();
            assert_eq!(code.len(), CODE_LEN);
            assert!(code.bytes().all(|b| CODE_ALPHABET.contains(&b)));
        }
    }

    #[test]
    fn normalization() {
        assert_eq!(normalize_code("abcd-234 7").unwrap().as_str(), "ABCD2347");
        assert!(normalize_code("ABCD234").is_err());
        assert!(normalize_code("ABCD23479").is_err());
        assert!(normalize_code("ABCD2341").is_err()); // 1 is not base32
        assert!(normalize_code("ÀBCD2345").is_err());
    }

    #[test]
    fn code_rotates_on_the_fifth_failure() {
        let code = OneTimeCode::generate();
        let first = code.current();
        for _ in 0..MAX_ATTEMPTS - 1 {
            drop(code.begin().unwrap());
            assert_eq!(code.current(), first);
        }
        drop(code.begin().unwrap());
        assert_ne!(code.current(), first);
    }

    #[test]
    fn concurrent_attempts_cannot_exceed_the_budget() {
        let code = OneTimeCode::generate();
        let first = code.current();
        let held: Vec<_> = (0..MAX_ATTEMPTS).map(|_| code.begin().unwrap()).collect();
        assert!(matches!(code.begin(), Err(PairError::TooManyAttempts)));
        assert_eq!(code.current(), first);
        drop(held);
        assert_ne!(code.current(), first);
        assert!(code.begin().is_ok());
    }

    #[test]
    fn a_code_works_once_even_if_two_exchanges_race() {
        let code = OneTimeCode::generate();
        let (a, _) = code.begin().unwrap();
        let (b, _) = code.begin().unwrap();
        assert!(a.succeed());
        assert!(!b.succeed());
    }

    #[test]
    fn manual_rotation_voids_attempts_in_progress() {
        let code = OneTimeCode::generate();
        let (a, _) = code.begin().unwrap();
        code.rotate();
        assert!(!a.succeed());
    }

    #[test]
    fn password_guard_locks_and_unlocks() {
        let guard = PasswordGuard::with_lockout(Duration::from_millis(300));
        for _ in 0..MAX_ATTEMPTS {
            drop(guard.begin().unwrap());
        }
        assert!(matches!(guard.begin(), Err(PairError::LockedOut)));
        std::thread::sleep(Duration::from_millis(350));
        assert!(guard.begin().unwrap().succeed());
    }

    #[test]
    fn password_guard_limits_concurrent_attempts() {
        let guard = PasswordGuard::with_lockout(Duration::from_secs(600));
        let held: Vec<_> = (0..MAX_ATTEMPTS).map(|_| guard.begin().unwrap()).collect();
        assert!(matches!(guard.begin(), Err(PairError::TooManyAttempts)));
        drop(held);
        assert!(matches!(guard.begin(), Err(PairError::LockedOut)));
    }

    #[test]
    fn stored_password_round_trip_and_no_plaintext() {
        let stored = HostPassword::create("correct horse").unwrap();
        let bytes = stored.to_bytes();
        assert_eq!(bytes.len(), STORED_PASSWORD_LEN);
        assert!(!bytes.windows(7).any(|w| w == b"correct"));
        let back = HostPassword::from_bytes(&bytes).unwrap();
        assert!(back.matches("correct horse"));
        assert!(!back.matches("correct horsf"));
        assert!(HostPassword::from_bytes(&bytes[1..]).is_err());
        assert!(HostPassword::create("").is_err());
    }
}
