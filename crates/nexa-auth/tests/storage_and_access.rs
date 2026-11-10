//! Key storage, the pairing store and the access decision. File backend and temp
//! directories only.

#![cfg(unix)]

mod common;

use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use common::pair_in_memory;
use nexa_auth::{
    AccessDecision, AccessPolicy, DenyReason, DeviceIdentity, Evidence, FileKeyStore,
    IdentityError, KeyStore, KeyStoreError, PairingStore, Perms, StoreError, decide,
};

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn store_at(path: &Path, identity: &DeviceIdentity) -> PairingStore {
    PairingStore::open(path, identity.store_mac_key()).unwrap()
}

fn view_only() -> Perms {
    Perms {
        view: true,
        ..Perms::default()
    }
}

/// All combinations of the policy, for "no setting can do this" assertions.
fn every_policy() -> Vec<AccessPolicy> {
    let mut all = Vec::new();
    for password_enabled in [false, true] {
        for save_password_pairing in [false, true] {
            all.push(AccessPolicy {
                password_enabled,
                save_password_pairing,
            });
        }
    }
    all
}

#[test]
fn identity_is_created_once_and_persists() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileKeyStore::new(dir.path().join("sub/device.key"));
    let first = DeviceIdentity::load_or_create(&store).unwrap();
    let second = DeviceIdentity::load_or_create(&store).unwrap();
    assert_eq!(first.public_key(), second.public_key());
    assert_eq!(first.device_id(), second.device_id());
    assert_eq!(
        first.store_mac_key().as_slice(),
        second.store_mac_key().as_slice()
    );
}

#[test]
fn device_id_differs_between_keys() {
    let (a, b) = (DeviceIdentity::generate(), DeviceIdentity::generate());
    assert_ne!(a.device_id(), b.device_id());
    assert_eq!(a.device_id().to_string().len(), 32);
}

#[test]
fn key_file_is_0600_and_its_directory_0700() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("private/device.key");
    let store = FileKeyStore::new(&path);
    DeviceIdentity::load_or_create(&store).unwrap();
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(path.parent().unwrap()), 0o700);
}

#[test]
fn loose_permissions_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("device.key");
    let store = FileKeyStore::new(&path);
    DeviceIdentity::load_or_create(&store).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        store.load(),
        Err(KeyStoreError::InsecurePermissions)
    ));
}

#[test]
fn a_symlinked_key_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real.key");
    fs::write(&real, [0u8; 32]).unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o600)).unwrap();
    let link = dir.path().join("device.key");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(matches!(
        FileKeyStore::new(&link).load(),
        Err(KeyStoreError::Symlink)
    ));
}

#[test]
fn a_key_file_of_the_wrong_length_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("device.key");
    let store = FileKeyStore::new(&path);
    store.store(&[1u8; 31]).unwrap();
    assert!(matches!(
        DeviceIdentity::load_or_create(&store),
        Err(IdentityError::BadStoredKey)
    ));
}

#[test]
fn replacing_a_key_leaves_no_temporary_files() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileKeyStore::new(dir.path().join("device.key"));
    store.store(b"one").unwrap();
    store.store(b"two").unwrap();
    assert_eq!(store.load().unwrap().unwrap().as_slice(), b"two");
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    store.delete().unwrap();
    store.delete().unwrap();
    assert!(store.load().unwrap().is_none());
}

#[test]
fn identity_debug_output_has_no_secret() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("device.key");
    let identity = DeviceIdentity::load_or_create(&FileKeyStore::new(&path)).unwrap();
    let seed = fs::read(&path).unwrap();
    let shown = format!("{identity:?}");
    let seed_hex: String = seed.iter().map(|b| format!("{b:02x}")).collect();
    assert!(!shown.contains(&seed_hex));
}

#[test]
fn pairings_survive_a_reopen() {
    let (host, viewer) = (DeviceIdentity::generate(), DeviceIdentity::generate());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pairings.json");
    let (_, proof) = pair_in_memory(&viewer, &host);
    let mut store = store_at(&path, &host);
    store.add(&proof, "laptop", view_only(), 1000).unwrap();
    store.touch(&viewer.public_key(), 2000).unwrap();
    store.set_unattended(&viewer.public_key(), true).unwrap();

    let reopened = store_at(&path, &host);
    let pinned = reopened.pinned(&viewer.public_key()).unwrap();
    let record = pinned.record();
    assert_eq!(record.name, "laptop");
    assert_eq!(record.perms, view_only());
    assert_eq!(record.added_at, 1000);
    assert_eq!(record.last_seen, 2000);
    assert!(record.unattended);
    assert_eq!(mode(&path), 0o600);
}

#[test]
fn peer_supplied_names_are_cleaned() {
    let (host, viewer) = (DeviceIdentity::generate(), DeviceIdentity::generate());
    let dir = tempfile::tempdir().unwrap();
    let (_, proof) = pair_in_memory(&viewer, &host);
    let mut store = store_at(&dir.path().join("p.json"), &host);
    let long = format!("IT\u{7}\nSupport{}", "x".repeat(500));
    let record = store.add(&proof, &long, view_only(), 1).unwrap();
    assert_eq!(record.name.chars().count(), 128);
    assert!(!record.name.contains('\n'));
}

#[test]
fn revoke_is_durable_and_reports_whether_anything_changed() {
    let (host, viewer) = (DeviceIdentity::generate(), DeviceIdentity::generate());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pairings.json");
    let (_, proof) = pair_in_memory(&viewer, &host);
    let mut store = store_at(&path, &host);
    store.add(&proof, "laptop", view_only(), 1).unwrap();
    assert!(store.revoke(&viewer.public_key()).unwrap());
    assert!(!store.revoke(&viewer.public_key()).unwrap());
    let reopened = store_at(&path, &host);
    assert!(reopened.pinned(&viewer.public_key()).is_none());
}

fn populated(path: &Path, host: &DeviceIdentity) -> String {
    let viewer = DeviceIdentity::generate();
    let (_, proof) = pair_in_memory(&viewer, host);
    store_at(path, host)
        .add(&proof, "laptop", view_only(), 1)
        .unwrap();
    fs::read_to_string(path).unwrap()
}

fn assert_tampered(path: &Path, host: &DeviceIdentity) {
    assert!(matches!(
        PairingStore::open(path, host.store_mac_key()),
        Err(StoreError::Tampered)
    ));
}

#[test]
fn an_edited_body_is_rejected() {
    let host = DeviceIdentity::generate();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pairings.json");
    let original = populated(&path, &host);
    // The obvious attack: mark the device unattended.
    let edited = original.replace(r#"\"unattended\":false"#, r#"\"unattended\":true"#);
    assert_ne!(original, edited);
    fs::write(&path, edited).unwrap();
    assert_tampered(&path, &host);
}

#[test]
fn an_attacker_cannot_add_a_record() {
    let host = DeviceIdentity::generate();
    let attacker = DeviceIdentity::generate();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pairings.json");
    let original = populated(&path, &host);
    // Swap the public key for the attacker's, keeping the old MAC.
    let victim = store_at(&path, &host).records()[0].pubkey.to_hex();
    let edited = original.replace(&victim, &attacker.public_key().to_hex());
    assert_ne!(original, edited);
    fs::write(&path, edited).unwrap();
    assert_tampered(&path, &host);
}

#[test]
fn a_changed_mac_is_rejected() {
    let host = DeviceIdentity::generate();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pairings.json");
    let original = populated(&path, &host);
    let needle = "\"mac\":\"";
    let at = original.find(needle).unwrap() + needle.len();
    let mut edited = original.clone().into_bytes();
    edited[at] = if edited[at] == b'0' { b'1' } else { b'0' };
    fs::write(&path, edited).unwrap();
    assert_tampered(&path, &host);
}

#[test]
fn a_file_signed_by_another_device_is_rejected() {
    let (host, other) = (DeviceIdentity::generate(), DeviceIdentity::generate());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pairings.json");
    populated(&path, &other);
    assert_tampered(&path, &host);
}

#[test]
fn truncated_or_garbage_files_are_rejected() {
    let host = DeviceIdentity::generate();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pairings.json");
    let original = populated(&path, &host);
    fs::write(&path, &original.as_bytes()[..original.len() / 2]).unwrap();
    assert_tampered(&path, &host);
    fs::write(&path, b"{}").unwrap();
    assert_tampered(&path, &host);
    fs::write(&path, b"").unwrap();
    assert_tampered(&path, &host);
}

#[test]
fn a_missing_file_is_an_empty_store() {
    let host = DeviceIdentity::generate();
    let dir = tempfile::tempdir().unwrap();
    assert!(
        store_at(&dir.path().join("none.json"), &host)
            .records()
            .is_empty()
    );
}

#[test]
fn no_evidence_is_always_denied() {
    let peer = DeviceIdentity::generate().public_key();
    for policy in every_policy() {
        assert_eq!(
            decide(&peer, Evidence::None, &policy),
            AccessDecision::Deny(DenyReason::NoEvidence)
        );
    }
}

#[test]
fn an_unattended_pairing_is_allowed_and_other_pairings_ask() {
    let (host, viewer) = (DeviceIdentity::generate(), DeviceIdentity::generate());
    let dir = tempfile::tempdir().unwrap();
    let (_, proof) = pair_in_memory(&viewer, &host);
    let mut store = store_at(&dir.path().join("p.json"), &host);
    store.add(&proof, "laptop", view_only(), 1).unwrap();
    let peer = viewer.public_key();
    for policy in every_policy() {
        let AccessDecision::NeedsConsent(prompt) = decide(
            &peer,
            Evidence::Pinned(store.pinned(&peer).unwrap()),
            &policy,
        ) else {
            panic!("a paired device without unattended access must be asked");
        };
        assert!(prompt.offer_skip_next_time);
        assert!(!prompt.offer_save);
    }
    store.set_unattended(&peer, true).unwrap();
    for policy in every_policy() {
        let decision = decide(
            &peer,
            Evidence::Pinned(store.pinned(&peer).unwrap()),
            &policy,
        );
        assert!(matches!(
            decision,
            AccessDecision::Allow { perms } if perms.view && !perms.input
        ));
    }
}

#[test]
fn a_correct_code_always_needs_consent() {
    let (host, viewer) = (DeviceIdentity::generate(), DeviceIdentity::generate());
    let (_, proof) = pair_in_memory(&viewer, &host);
    let peer = viewer.public_key();
    for policy in every_policy() {
        let AccessDecision::NeedsConsent(prompt) =
            decide(&peer, Evidence::Exchange(&proof), &policy)
        else {
            panic!("a code must never grant access without consent");
        };
        assert!(prompt.offer_save);
        assert!(!prompt.offer_skip_next_time);
        assert!(prompt.perms.view && !prompt.perms.input);
    }
}

#[test]
fn evidence_for_a_different_key_is_denied() {
    let (host, viewer) = (DeviceIdentity::generate(), DeviceIdentity::generate());
    let intruder = DeviceIdentity::generate().public_key();
    let dir = tempfile::tempdir().unwrap();
    let (_, proof) = pair_in_memory(&viewer, &host);
    let mut store = store_at(&dir.path().join("p.json"), &host);
    store.add(&proof, "laptop", view_only(), 1).unwrap();
    store.set_unattended(&viewer.public_key(), true).unwrap();
    let policy = AccessPolicy::default();
    assert_eq!(
        decide(&intruder, Evidence::Exchange(&proof), &policy),
        AccessDecision::Deny(DenyReason::IdentityMismatch)
    );
    let pinned = store.pinned(&viewer.public_key()).unwrap();
    assert_eq!(
        decide(&intruder, Evidence::Pinned(pinned), &policy),
        AccessDecision::Deny(DenyReason::IdentityMismatch)
    );
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "creates and deletes an item in the real login Keychain"]
fn keychain_store_round_trip() {
    use nexa_auth::KeychainStore;

    struct Cleanup(KeychainStore);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.0.delete();
        }
    }

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let service = format!("com.nexadesk.test.{}.{nanos}", std::process::id());
    let cleanup = Cleanup(KeychainStore::new(service, "device-key"));
    let store = &cleanup.0;

    assert!(store.load().unwrap().is_none());
    let identity = DeviceIdentity::load_or_create(store).unwrap();
    let again = DeviceIdentity::load_or_create(store).unwrap();
    assert_eq!(identity.public_key(), again.public_key());
    store.delete().unwrap();
    assert!(store.load().unwrap().is_none());
}
