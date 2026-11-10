//! The secrets a host accepts for pairing, and how many guesses it allows (§9.2).
//!
//! A guess is charged when an exchange *starts*, not when it fails. An attempt that
//! is abandoned, or dropped because the connection died, counts as a failure, and
//! attempts running at the same time cannot exceed the budget between them.
//!
//! Every guess is also charged to a [`Source`], so that one sender cannot spend
//! everybody's budget: a source may use only [`CODE_ATTEMPTS_PER_SOURCE`] of a
//! code's guesses, and a source that keeps failing at the password locks out only
//! itself, until failures from all sources together reach
//! [`PASSWORD_FAILURES_GLOBAL`].

use std::{
    collections::HashMap,
    net::IpAddr,
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

/// How many of a code's [`MAX_ATTEMPTS`] guesses one [`Source`] may use. Below the
/// total, so a single sender can neither replace the code by failing nor leave
/// nothing for the person who was given it.
pub const CODE_ATTEMPTS_PER_SOURCE: u32 = 3;

/// Password failures from all sources together, since the last success, that lock
/// password login for everyone. Bounds guessing spread over many addresses.
pub const PASSWORD_FAILURES_GLOBAL: u32 = 25;

/// Sources whose password state is kept. When full, sources with nothing pending
/// are forgotten first; if none can be, a new source is refused.
const PASSWORD_MAX_TRACKED: usize = 4096;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic elsewhere must not stop attempts being charged.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Who a guess is charged to: an IPv4 address, or the /64 an IPv6 address is in,
/// which is what one subscriber is usually given. IPv4-mapped IPv6 addresses count
/// as IPv4.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Source(SourceKey);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum SourceKey {
    V4([u8; 4]),
    V6Prefix([u8; 8]),
}

impl Source {
    pub fn of(ip: IpAddr) -> Self {
        match ip.to_canonical() {
            IpAddr::V4(v4) => Self(SourceKey::V4(v4.octets())),
            IpAddr::V6(v6) => {
                let mut prefix = [0u8; 8];
                prefix.copy_from_slice(&v6.octets()[..8]);
                Self(SourceKey::V6Prefix(prefix))
            }
        }
    }
}

impl From<IpAddr> for Source {
    fn from(ip: IpAddr) -> Self {
        Self::of(ip)
    }
}

/// Resolves an attempt against the limiter that issued it.
trait Settle: Send + Sync {
    /// Returns whether a successful attempt may stand.
    fn settle(&self, generation: u64, source: Source, success: bool) -> bool;
}

/// One charged guess. Dropping it without [`Attempt::succeed`] records a failure.
#[must_use]
pub struct Attempt {
    owner: Arc<dyn Settle>,
    generation: u64,
    source: Source,
    settled: bool,
}

impl Attempt {
    /// Returns false if the secret was already consumed, in which case the
    /// exchange must be treated as failed.
    pub(crate) fn succeed(mut self) -> bool {
        self.settled = true;
        self.owner.settle(self.generation, self.source, true)
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        if !self.settled {
            self.owner.settle(self.generation, self.source, false);
        }
    }
}

struct CodeState {
    code: Zeroizing<String>,
    generation: u64,
    failures: u32,
    in_flight: u32,
    /// Guesses each source has started at this code, failed or still running. At
    /// most [`MAX_ATTEMPTS`] entries, as every guess also counts towards the total.
    per_source: HashMap<Source, u32>,
}

impl CodeState {
    fn rotate(&mut self) {
        self.code = random_code();
        self.generation += 1;
        self.failures = 0;
        self.in_flight = 0;
        self.per_source.clear();
    }
}

struct CodeShared {
    state: Mutex<CodeState>,
}

impl Settle for CodeShared {
    // A source's share stays used after a failure: it is only refilled by a new code.
    fn settle(&self, generation: u64, _source: Source, success: bool) -> bool {
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
/// [`MAX_ATTEMPTS`] failures, of which one [`Source`] may cause at most
/// [`CODE_ATTEMPTS_PER_SOURCE`]. Clones share state, so native viewers and browsers
/// (`nexa-web`) draw on the same limits.
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
                    per_source: HashMap::new(),
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

    pub(crate) fn begin(&self, source: Source) -> Result<(Attempt, Zeroizing<String>), PairError> {
        let mut s = lock(&self.shared.state);
        if s.failures + s.in_flight >= MAX_ATTEMPTS {
            return Err(PairError::TooManyAttempts);
        }
        let used = s.per_source.get(&source).copied().unwrap_or(0);
        if used >= CODE_ATTEMPTS_PER_SOURCE {
            return Err(PairError::TooManyAttempts);
        }
        s.per_source.insert(source, used + 1);
        s.in_flight += 1;
        let attempt = Attempt {
            owner: self.shared.clone(),
            generation: s.generation,
            source,
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

/// Failures and lockout for one scope: a single source, or all of them together.
#[derive(Default)]
struct Strikes {
    failures: u32,
    in_flight: u32,
    lockouts: u32,
    locked_until: Option<Instant>,
}

impl Strikes {
    fn admit(&mut self, now: Instant, limit: u32) -> Result<(), PairError> {
        if let Some(until) = self.locked_until {
            if now < until {
                return Err(PairError::LockedOut);
            }
            self.locked_until = None;
        }
        if self.failures + self.in_flight >= limit {
            return Err(PairError::TooManyAttempts);
        }
        Ok(())
    }

    fn fail(&mut self, now: Instant, limit: u32, base_lockout: Duration) {
        self.failures += 1;
        if self.failures >= limit {
            self.failures = 0;
            let doubling = 1u32 << self.lockouts.min(16);
            self.lockouts += 1;
            let lockout = base_lockout
                .saturating_mul(doubling)
                .min(PASSWORD_LOCKOUT_MAX);
            self.locked_until = Some(now + lockout);
        }
    }

    fn clear(&mut self) {
        self.failures = 0;
        self.lockouts = 0;
        self.locked_until = None;
    }

    /// Nothing would be lost by forgetting it, except how long its next lockout is.
    fn idle(&self, now: Instant) -> bool {
        self.in_flight == 0 && self.failures == 0 && self.locked_until.is_none_or(|u| now >= u)
    }
}

struct PasswordState {
    global: Strikes,
    per_source: HashMap<Source, Strikes>,
}

struct PasswordShared {
    state: Mutex<PasswordState>,
    base_lockout: Duration,
}

impl Settle for PasswordShared {
    fn settle(&self, _generation: u64, source: Source, success: bool) -> bool {
        let now = Instant::now();
        let mut guard = lock(&self.state);
        let s = &mut *guard;
        s.global.in_flight -= 1;
        // Present: entries with an attempt running are never forgotten.
        let mine = s.per_source.entry(source).or_default();
        mine.in_flight = mine.in_flight.saturating_sub(1);
        if success {
            mine.clear();
            s.global.clear();
            return true;
        }
        mine.fail(now, MAX_ATTEMPTS, self.base_lockout);
        s.global
            .fail(now, PASSWORD_FAILURES_GLOBAL, self.base_lockout);
        false
    }
}

/// Online-guessing limit for the unattended password. After [`MAX_ATTEMPTS`]
/// failures a [`Source`] is refused for a period that doubles each time, up to an
/// hour; other sources are not affected until [`PASSWORD_FAILURES_GLOBAL`] failures
/// from all of them together, which locks everyone out the same way. A password
/// cannot be rotated the way a code can. The state is in memory, so a restart
/// clears it. Clones share state.
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
    /// `base_lockout` is the first lockout period, for a source and for everyone.
    pub fn with_lockout(base_lockout: Duration) -> Self {
        Self {
            shared: Arc::new(PasswordShared {
                state: Mutex::new(PasswordState {
                    global: Strikes::default(),
                    per_source: HashMap::new(),
                }),
                base_lockout,
            }),
        }
    }

    pub(crate) fn begin(&self, source: Source) -> Result<Attempt, PairError> {
        let now = Instant::now();
        let mut guard = lock(&self.shared.state);
        let s = &mut *guard;
        s.global.admit(now, PASSWORD_FAILURES_GLOBAL)?;
        if !s.per_source.contains_key(&source) && s.per_source.len() >= PASSWORD_MAX_TRACKED {
            s.per_source.retain(|_, st| !st.idle(now));
            if s.per_source.len() >= PASSWORD_MAX_TRACKED {
                return Err(PairError::TooManyAttempts);
            }
        }
        let mine = s.per_source.entry(source).or_default();
        mine.admit(now, MAX_ATTEMPTS)?;
        mine.in_flight += 1;
        s.global.in_flight += 1;
        Ok(Attempt {
            owner: self.shared.clone(),
            generation: 0,
            source,
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

    fn src(n: u8) -> Source {
        Source::of(IpAddr::from([192, 0, 2, n]))
    }

    #[test]
    fn sources_group_ipv6_by_slash_64_and_unmap_ipv4() {
        let a: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap();
        let c: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(Source::of(a), Source::of(b));
        assert_ne!(Source::of(a), Source::of(c));
        let mapped: IpAddr = "::ffff:192.0.2.7".parse().unwrap();
        assert_eq!(Source::of(mapped), src(7));
        assert_ne!(src(7), src(8));
    }

    #[test]
    fn code_rotates_on_the_fifth_failure() {
        let code = OneTimeCode::generate();
        let first = code.current();
        for i in 0..MAX_ATTEMPTS - 1 {
            drop(code.begin(src(i as u8 % 2)).unwrap());
            assert_eq!(code.current(), first);
        }
        drop(code.begin(src(2)).unwrap());
        assert_ne!(code.current(), first);
    }

    #[test]
    fn one_source_cannot_use_up_a_code() {
        let code = OneTimeCode::generate();
        let first = code.current();
        for _ in 0..CODE_ATTEMPTS_PER_SOURCE {
            drop(code.begin(src(1)).unwrap());
        }
        assert!(matches!(
            code.begin(src(1)),
            Err(PairError::TooManyAttempts)
        ));
        // The code is unchanged and the person it was given to can still use it.
        let (attempt, secret) = code.begin(src(9)).unwrap();
        assert_eq!(secret, first);
        assert!(attempt.succeed());
        // A new code gives every source its share back.
        assert!(code.begin(src(1)).is_ok());
    }

    #[test]
    fn addresses_in_one_slash_64_share_a_codes_share() {
        let code = OneTimeCode::generate();
        let v6 = |s: &str| Source::of(s.parse::<IpAddr>().unwrap());
        for _ in 0..CODE_ATTEMPTS_PER_SOURCE {
            drop(code.begin(v6("2001:db8::1")).unwrap());
        }
        assert!(matches!(
            code.begin(v6("2001:db8::ffff")),
            Err(PairError::TooManyAttempts)
        ));
        assert!(code.begin(v6("2001:db8:0:1::1")).is_ok());
    }

    #[test]
    fn concurrent_attempts_cannot_exceed_the_budget() {
        let code = OneTimeCode::generate();
        let first = code.current();
        let held: Vec<_> = (0..MAX_ATTEMPTS)
            .map(|i| code.begin(src(i as u8)).unwrap())
            .collect();
        assert!(matches!(
            code.begin(src(100)),
            Err(PairError::TooManyAttempts)
        ));
        assert_eq!(code.current(), first);
        drop(held);
        assert_ne!(code.current(), first);
        assert!(code.begin(src(0)).is_ok());
    }

    #[test]
    fn a_code_works_once_even_if_two_exchanges_race() {
        let code = OneTimeCode::generate();
        let (a, _) = code.begin(src(1)).unwrap();
        let (b, _) = code.begin(src(2)).unwrap();
        assert!(a.succeed());
        assert!(!b.succeed());
    }

    #[test]
    fn manual_rotation_voids_attempts_in_progress() {
        let code = OneTimeCode::generate();
        let (a, _) = code.begin(src(1)).unwrap();
        code.rotate();
        assert!(!a.succeed());
    }

    #[test]
    fn password_guard_locks_and_unlocks() {
        let guard = PasswordGuard::with_lockout(Duration::from_millis(300));
        for _ in 0..MAX_ATTEMPTS {
            drop(guard.begin(src(1)).unwrap());
        }
        assert!(matches!(guard.begin(src(1)), Err(PairError::LockedOut)));
        std::thread::sleep(Duration::from_millis(350));
        assert!(guard.begin(src(1)).unwrap().succeed());
    }

    #[test]
    fn password_lockout_is_per_source() {
        let guard = PasswordGuard::with_lockout(Duration::from_secs(600));
        for _ in 0..MAX_ATTEMPTS {
            drop(guard.begin(src(1)).unwrap());
        }
        assert!(matches!(guard.begin(src(1)), Err(PairError::LockedOut)));
        assert!(guard.begin(src(2)).unwrap().succeed());
        // Someone else's success does not lift this source's lockout.
        assert!(matches!(guard.begin(src(1)), Err(PairError::LockedOut)));
    }

    #[test]
    fn password_failures_from_many_sources_lock_everyone() {
        let guard = PasswordGuard::with_lockout(Duration::from_secs(600));
        for i in 0..PASSWORD_FAILURES_GLOBAL {
            // A few per source, so no single source is locked.
            drop(guard.begin(src((i / 2) as u8)).unwrap());
        }
        assert!(matches!(guard.begin(src(200)), Err(PairError::LockedOut)));
    }

    #[test]
    fn a_success_resets_the_global_count() {
        let guard = PasswordGuard::with_lockout(Duration::from_secs(600));
        for i in 0..PASSWORD_FAILURES_GLOBAL - 1 {
            drop(guard.begin(src((i / 2) as u8)).unwrap());
        }
        assert!(guard.begin(src(250)).unwrap().succeed());
        for i in 0..PASSWORD_FAILURES_GLOBAL - 1 {
            drop(guard.begin(src(100 + (i / 2) as u8)).unwrap());
        }
        assert!(guard.begin(src(251)).is_ok());
    }

    #[test]
    fn password_guard_limits_concurrent_attempts() {
        let guard = PasswordGuard::with_lockout(Duration::from_secs(600));
        let held: Vec<_> = (0..MAX_ATTEMPTS)
            .map(|_| guard.begin(src(1)).unwrap())
            .collect();
        assert!(matches!(
            guard.begin(src(1)),
            Err(PairError::TooManyAttempts)
        ));
        drop(held);
        assert!(matches!(guard.begin(src(1)), Err(PairError::LockedOut)));
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
