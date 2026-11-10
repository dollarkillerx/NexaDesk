//! The pairing state machine, driven directly with no I/O.

mod common;

use std::time::Duration;

use nexa_auth::{
    AuthMethod, DeviceIdentity, HostPairing, HostPassword, MAX_ATTEMPTS, OneTimeCode, PairError,
    PairingContext, PasswordGuard, ViewerEvent, ViewerPairing,
};
use nexa_wire::control::AuthStep;
use prost::Message;

struct Pair {
    viewer: DeviceIdentity,
    host: DeviceIdentity,
}

impl Pair {
    fn new() -> Self {
        Self {
            viewer: DeviceIdentity::generate(),
            host: DeviceIdentity::generate(),
        }
    }

    fn ctx(&self, exporter: u8) -> PairingContext {
        PairingContext::new(
            self.viewer.public_key(),
            self.host.public_key(),
            [exporter; 32],
        )
        .unwrap()
    }
}

fn viewer_reply(v: &mut ViewerPairing, offer: &AuthStep) -> Result<AuthStep, PairError> {
    match v.on_step(offer)? {
        ViewerEvent::Send(s) => Ok(s),
        ViewerEvent::Complete(_) => panic!("viewer completed too early"),
    }
}

/// Runs one exchange where the viewer may hold a different secret from the host.
fn exchange(
    v_ctx: PairingContext,
    h_ctx: PairingContext,
    code: &OneTimeCode,
    typed: &str,
) -> Result<AuthMethod, PairError> {
    let (mut h, offer) = HostPairing::with_code(h_ctx, code)?;
    let mut v = ViewerPairing::with_code(v_ctx, typed)?;
    let reply = viewer_reply(&mut v, &offer)?;
    let done = h.on_step(&reply)?;
    match v.on_step(&done.reply)? {
        ViewerEvent::Complete(p) => Ok(p.method()),
        ViewerEvent::Send(_) => panic!("viewer should have completed"),
    }
}

fn other_code(code: &str) -> String {
    let first = if code.starts_with('A') { 'B' } else { 'A' };
    format!("{first}{}", &code[1..])
}

#[test]
fn code_exchange_completes_both_ways() {
    let p = Pair::new();
    let code = OneTimeCode::generate();
    let shown = code.current();
    let (mut h, offer) = HostPairing::with_code(p.ctx(1), &code).unwrap();
    let mut v = ViewerPairing::with_code(p.ctx(1), &shown).unwrap();
    let reply = viewer_reply(&mut v, &offer).unwrap();
    let done = h.on_step(&reply).unwrap();
    assert_eq!(done.verified.peer(), p.viewer.public_key());
    assert_eq!(done.verified.method(), AuthMethod::Code);
    let ViewerEvent::Complete(proof) = v.on_step(&done.reply).unwrap() else {
        panic!("viewer should complete");
    };
    assert_eq!(proof.peer(), p.host.public_key());
}

#[test]
fn typed_code_may_be_lower_case_and_grouped() {
    let p = Pair::new();
    let code = OneTimeCode::generate();
    let shown = code.current();
    let typed = format!("{}-{}", shown[..4].to_lowercase(), &shown[4..]);
    exchange(p.ctx(1), p.ctx(1), &code, &typed).unwrap();
}

#[test]
fn password_exchange_completes() {
    let p = Pair::new();
    let stored = HostPassword::create("hunter2 hunter2").unwrap();
    let guard = PasswordGuard::default();
    let (mut h, offer) = HostPairing::with_password(p.ctx(1), &stored, &guard).unwrap();
    let mut v = ViewerPairing::with_password(p.ctx(1), "hunter2 hunter2").unwrap();
    let reply = viewer_reply(&mut v, &offer).unwrap();
    let done = h.on_step(&reply).unwrap();
    assert_eq!(done.verified.method(), AuthMethod::Password);
    assert!(matches!(
        v.on_step(&done.reply).unwrap(),
        ViewerEvent::Complete(_)
    ));
}

#[test]
fn wrong_code_is_rejected_by_the_host() {
    let p = Pair::new();
    let code = OneTimeCode::generate();
    let wrong = other_code(&code.current());
    assert_eq!(
        exchange(p.ctx(1), p.ctx(1), &code, &wrong),
        Err(PairError::ConfirmationFailed)
    );
}

#[test]
fn differing_exporters_fail_even_with_the_right_code_and_keys() {
    let p = Pair::new();
    let code = OneTimeCode::generate();
    let shown = code.current();
    assert_eq!(
        exchange(p.ctx(1), p.ctx(2), &code, &shown),
        Err(PairError::ConfirmationFailed)
    );
}

#[test]
fn differing_viewer_keys_fail() {
    let p = Pair::new();
    let attacker = DeviceIdentity::generate();
    let code = OneTimeCode::generate();
    let shown = code.current();
    let host_view =
        PairingContext::new(attacker.public_key(), p.host.public_key(), [1; 32]).unwrap();
    assert_eq!(
        exchange(p.ctx(1), host_view, &code, &shown),
        Err(PairError::ConfirmationFailed)
    );
}

#[test]
fn differing_host_keys_fail() {
    let p = Pair::new();
    let attacker = DeviceIdentity::generate();
    let code = OneTimeCode::generate();
    let shown = code.current();
    let viewer_view =
        PairingContext::new(p.viewer.public_key(), attacker.public_key(), [1; 32]).unwrap();
    assert_eq!(
        exchange(viewer_view, p.ctx(1), &code, &shown),
        Err(PairError::ConfirmationFailed)
    );
}

#[test]
fn a_changed_salt_fails_confirmation() {
    let p = Pair::new();
    let code = OneTimeCode::generate();
    let shown = code.current();
    let (mut h, mut offer) = HostPairing::with_code(p.ctx(1), &code).unwrap();
    offer.salt[0] ^= 1;
    let mut v = ViewerPairing::with_code(p.ctx(1), &shown).unwrap();
    let reply = viewer_reply(&mut v, &offer).unwrap();
    assert_eq!(h.on_step(&reply).err(), Some(PairError::ConfirmationFailed));
}

#[test]
fn a_tampered_viewer_confirm_fails() {
    let p = Pair::new();
    let code = OneTimeCode::generate();
    let shown = code.current();
    let (mut h, offer) = HostPairing::with_code(p.ctx(1), &code).unwrap();
    let mut v = ViewerPairing::with_code(p.ctx(1), &shown).unwrap();
    let mut reply = viewer_reply(&mut v, &offer).unwrap();
    reply.confirm[31] ^= 1;
    assert_eq!(h.on_step(&reply).err(), Some(PairError::ConfirmationFailed));
}

#[test]
fn a_tampered_host_confirm_fails() {
    let p = Pair::new();
    let code = OneTimeCode::generate();
    let shown = code.current();
    let (mut h, offer) = HostPairing::with_code(p.ctx(1), &code).unwrap();
    let mut v = ViewerPairing::with_code(p.ctx(1), &shown).unwrap();
    let reply = viewer_reply(&mut v, &offer).unwrap();
    let mut done = h.on_step(&reply).unwrap();
    done.reply.confirm[0] ^= 1;
    assert!(matches!(
        v.on_step(&done.reply),
        Err(PairError::ConfirmationFailed)
    ));
}

#[test]
fn a_viewer_step_cannot_be_replayed_into_a_new_exchange() {
    let p = Pair::new();
    let code = OneTimeCode::generate();
    let shown = code.current();
    let (_h1, offer) = HostPairing::with_code(p.ctx(1), &code).unwrap();
    let mut v = ViewerPairing::with_code(p.ctx(1), &shown).unwrap();
    let captured = viewer_reply(&mut v, &offer).unwrap();
    // The exchange it was made for never saw it; a new one gets the same bytes.
    let (mut h2, _) = HostPairing::with_code(p.ctx(1), &code).unwrap();
    assert_eq!(
        h2.on_step(&captured).err(),
        Some(PairError::ConfirmationFailed)
    );
}

#[test]
fn the_machines_refuse_steps_after_finishing() {
    let p = Pair::new();
    let code = OneTimeCode::generate();
    let shown = code.current();
    let (mut h, offer) = HostPairing::with_code(p.ctx(1), &code).unwrap();
    let mut v = ViewerPairing::with_code(p.ctx(1), &shown).unwrap();
    let reply = viewer_reply(&mut v, &offer).unwrap();
    let done = h.on_step(&reply).unwrap();
    assert!(matches!(h.on_step(&reply), Err(PairError::WrongState)));
    v.on_step(&done.reply).unwrap();
    assert!(matches!(v.on_step(&done.reply), Err(PairError::WrongState)));
}

#[test]
fn an_error_is_terminal() {
    let p = Pair::new();
    let code = OneTimeCode::generate();
    let shown = code.current();
    let (mut h, offer) = HostPairing::with_code(p.ctx(1), &code).unwrap();
    let mut v = ViewerPairing::with_code(p.ctx(1), &shown).unwrap();
    let good = viewer_reply(&mut v, &offer).unwrap();
    assert!(h.on_step(&AuthStep::default()).is_err());
    // The genuine message now arrives too late.
    assert_eq!(h.on_step(&good).err(), Some(PairError::WrongState));
}

#[test]
fn the_code_rotates_after_five_failures_and_the_old_one_stops_working() {
    let p = Pair::new();
    let code = OneTimeCode::generate();
    let first = code.current();
    let wrong = other_code(&first);
    for _ in 0..MAX_ATTEMPTS {
        assert_eq!(code.current(), first);
        assert_eq!(
            exchange(p.ctx(1), p.ctx(1), &code, &wrong),
            Err(PairError::ConfirmationFailed)
        );
    }
    assert_ne!(code.current(), first);
    assert_eq!(
        exchange(p.ctx(1), p.ctx(1), &code, &first),
        Err(PairError::ConfirmationFailed)
    );
    // The new code works.
    exchange(p.ctx(1), p.ctx(1), &code, &code.current()).unwrap();
}

#[test]
fn abandoned_exchanges_count_as_failures() {
    let p = Pair::new();
    let code = OneTimeCode::generate();
    let first = code.current();
    for _ in 0..MAX_ATTEMPTS {
        let (h, _offer) = HostPairing::with_code(p.ctx(1), &code).unwrap();
        drop(h); // Connection died, or the attacker walked away.
    }
    assert_ne!(code.current(), first);
}

#[test]
fn a_code_is_single_use() {
    let p = Pair::new();
    let code = OneTimeCode::generate();
    let shown = code.current();
    exchange(p.ctx(1), p.ctx(1), &code, &shown).unwrap();
    assert_ne!(code.current(), shown);
    assert_eq!(
        exchange(p.ctx(1), p.ctx(1), &code, &shown),
        Err(PairError::ConfirmationFailed)
    );
}

#[test]
fn two_viewers_racing_on_one_code_cannot_both_win() {
    let p = Pair::new();
    let code = OneTimeCode::generate();
    let shown = code.current();
    let (mut h1, offer1) = HostPairing::with_code(p.ctx(1), &code).unwrap();
    let (mut h2, offer2) = HostPairing::with_code(p.ctx(2), &code).unwrap();
    let mut v1 = ViewerPairing::with_code(p.ctx(1), &shown).unwrap();
    let mut v2 = ViewerPairing::with_code(p.ctx(2), &shown).unwrap();
    let r1 = viewer_reply(&mut v1, &offer1).unwrap();
    let r2 = viewer_reply(&mut v2, &offer2).unwrap();
    assert!(h1.on_step(&r1).is_ok());
    assert_eq!(h2.on_step(&r2).err(), Some(PairError::SecretConsumed));
}

#[test]
fn password_attempts_lock_out_at_the_host() {
    let p = Pair::new();
    // Skips Argon2: the guard does not depend on the stored value.
    let stored = HostPassword::from_bytes(&[3u8; 48]).unwrap();
    let guard = PasswordGuard::with_lockout(Duration::from_secs(600));
    for _ in 0..MAX_ATTEMPTS {
        let (mut h, _offer) = HostPairing::with_password(p.ctx(1), &stored, &guard).unwrap();
        assert!(h.on_step(&AuthStep::default()).is_err());
    }
    assert!(matches!(
        HostPairing::with_password(p.ctx(1), &stored, &guard).map(|_| ()),
        Err(PairError::LockedOut)
    ));
}

/// A small deterministic generator, so a failing fuzz case reproduces.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }

    fn field(&mut self) -> Vec<u8> {
        // Mostly the lengths the parser cares about.
        let n = match self.next() % 12 {
            0 => 0,
            1 => 1,
            2 | 3 => 16,
            4 => 15,
            5 | 6 => 32,
            7..=9 => 33,
            10 => 34,
            _ => (self.next() % 512) as usize,
        };
        let mut out = self.bytes(n);
        if n == 33 && self.next().is_multiple_of(2) {
            // The correct side byte for the host, to reach point decoding.
            out[0] = if self.next().is_multiple_of(2) {
                b'A'
            } else {
                b'B'
            };
        }
        out
    }

    fn step(&mut self) -> AuthStep {
        AuthStep {
            spake_msg: self.field(),
            confirm: self.field(),
            salt: self.field(),
        }
    }
}

#[test]
fn malformed_auth_steps_never_panic_and_never_complete() {
    let p = Pair::new();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let code = OneTimeCode::generate();
    for _ in 0..400 {
        let (mut h, _offer) = HostPairing::with_code(p.ctx(1), &code).unwrap();
        let first = h.on_step(&rng.step());
        assert!(first.is_err(), "a random step must not complete pairing");
        assert!(h.on_step(&rng.step()).is_err());

        let mut v = ViewerPairing::with_code(p.ctx(1), "ABCDEFGH").unwrap();
        for _ in 0..3 {
            if let Ok(ViewerEvent::Complete(_)) = v.on_step(&rng.step()) {
                panic!("a random step must not complete pairing");
            }
        }
    }
}

#[test]
fn malformed_steps_reach_a_password_viewer_without_panicking() {
    // Slow per case, as a well-shaped offer costs an Argon2 run.
    let p = Pair::new();
    let mut rng = Rng(0xDEAD_BEEF_F00D_1234);
    for _ in 0..4 {
        let mut v = ViewerPairing::with_password(p.ctx(1), "pw").unwrap();
        let step = AuthStep {
            spake_msg: rng.bytes(33),
            confirm: vec![],
            salt: rng.bytes(16),
        };
        let _ = v.on_step(&step);
        let _ = v.on_step(&rng.step());
    }
}

#[test]
fn random_bytes_on_the_wire_never_panic() {
    let p = Pair::new();
    let mut rng = Rng(0x0123_4567_89AB_CDEF);
    let code = OneTimeCode::generate();
    let mut decoded = 0;
    for _ in 0..2000 {
        let n = (rng.next() % 96) as usize;
        let bytes = rng.bytes(n);
        if let Ok(step) = AuthStep::decode(bytes.as_slice()) {
            decoded += 1;
            let (mut h, _) = HostPairing::with_code(p.ctx(1), &code).unwrap();
            assert!(h.on_step(&step).is_err());
        }
    }
    assert!(decoded > 0, "the fuzz never exercised the state machine");
}

#[test]
fn special_curve_points_do_not_complete_or_panic() {
    let p = Pair::new();
    let code = OneTimeCode::generate();
    let shown = code.current();
    let mut identity = [0u8; 32];
    identity[0] = 1;
    let mut order_two = [0xFFu8; 32];
    order_two[0] = 0xEC;
    order_two[31] = 0x7F;
    let mut not_a_point = [0xFFu8; 32];
    not_a_point[0] = 0xED;
    for point in [identity, [0; 32], order_two, not_a_point] {
        let (mut h, _offer) = HostPairing::with_code(p.ctx(1), &code).unwrap();
        let mut spake_msg = vec![b'A'];
        spake_msg.extend_from_slice(&point);
        let step = AuthStep {
            spake_msg,
            confirm: vec![0; 32],
            salt: vec![],
        };
        assert!(h.on_step(&step).is_err());

        let mut v = ViewerPairing::with_code(p.ctx(1), &shown).unwrap();
        let mut spake_msg = vec![b'B'];
        spake_msg.extend_from_slice(&point);
        let offer = AuthStep {
            spake_msg,
            confirm: vec![],
            salt: vec![0; 16],
        };
        // The viewer has no secret-dependent answer to give, so either outcome is
        // fine here. What matters is that the state machine doesn't panic and that
        // it cannot complete without a valid confirmation.
        if let Ok(ViewerEvent::Send(_)) = v.on_step(&offer) {
            assert!(
                v.on_step(&AuthStep {
                    spake_msg: vec![],
                    confirm: vec![0; 32],
                    salt: vec![],
                })
                .is_err()
            );
        }
    }
}

#[test]
fn right_length_wrong_content_is_rejected() {
    let p = Pair::new();
    let code = OneTimeCode::generate();
    // Wrong SPAKE side byte: a viewer message claiming to be from the host.
    let (mut h, offer) = HostPairing::with_code(p.ctx(1), &code).unwrap();
    assert_eq!(offer.spake_msg[0], b'B');
    let reflected = AuthStep {
        spake_msg: offer.spake_msg.clone(),
        confirm: vec![0; 32],
        salt: vec![],
    };
    assert!(matches!(
        h.on_step(&reflected),
        Err(PairError::Malformed(_))
    ));
}
