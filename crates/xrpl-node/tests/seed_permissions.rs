//! VALAUDIT Phase 4 (va-04) — seed hygiene tests.
//!
//! Verifies `paths::load_or_create_seed`'s three audit-mandated behaviors:
//!
//! 1. Fatal on bad mode (mode & 0o077 != 0)
//! 2. Atomic create with mode 0600 when the file doesn't exist
//! 3. Fatal on malformed seed (no silent identity regeneration)
//!
//! These run in tempfile-isolated directories — they MUST NOT touch the
//! production seed at /home/m3060/xrpl-data/validator_seed.hex.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;

use xrpl_node::paths::load_or_create_seed;

/// Create a unique tempdir for each test (avoids cross-test contamination
/// since tests run in parallel).
fn tempdir() -> PathBuf {
    let base = std::env::temp_dir();
    let id = format!(
        "xrpl-seed-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let dir = base.join(id);
    fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

#[test]
fn fresh_create_uses_mode_0600() {
    let dir = tempdir();
    let path = dir.join("seed.hex");
    let path_str = path.to_str().unwrap();

    let (_seed, was_created) = load_or_create_seed(path_str).expect("create OK");
    assert!(was_created);

    let mode = fs::metadata(&path).unwrap().mode() & 0o777;
    assert_eq!(mode, 0o600, "fresh seed must be mode 0600, got {:o}", mode);

    // Sanity-check content: 32 hex chars = 16 bytes.
    let content = fs::read_to_string(&path).unwrap();
    assert_eq!(content.trim().len(), 32);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn loads_existing_with_correct_mode() {
    let dir = tempdir();
    let path = dir.join("seed.hex");
    let path_str = path.to_str().unwrap();

    // Create a valid seed file with proper mode.
    fs::write(&path, "00112233445566778899aabbccddeeff").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

    let (seed, was_created) = load_or_create_seed(path_str).expect("load OK");
    assert!(!was_created);
    assert_eq!(&seed.bytes[..], &hex::decode("00112233445566778899aabbccddeeff").unwrap()[..]);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn fatal_on_group_readable_mode() {
    let dir = tempdir();
    let path = dir.join("seed.hex");
    let path_str = path.to_str().unwrap();

    fs::write(&path, "00112233445566778899aabbccddeeff").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

    let err = load_or_create_seed(path_str).unwrap_err();
    assert!(err.contains("FATAL"), "expected FATAL prefix, got: {err}");
    assert!(err.contains("mode 644"), "expected mode 644 in message, got: {err}");
    assert!(err.contains("chmod 600"), "expected chmod hint, got: {err}");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn fatal_on_world_readable_mode() {
    let dir = tempdir();
    let path = dir.join("seed.hex");
    let path_str = path.to_str().unwrap();

    fs::write(&path, "00112233445566778899aabbccddeeff").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o604)).unwrap();

    let err = load_or_create_seed(path_str).unwrap_err();
    assert!(err.contains("FATAL"));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn fatal_on_invalid_hex() {
    let dir = tempdir();
    let path = dir.join("seed.hex");
    let path_str = path.to_str().unwrap();

    fs::write(&path, "not-valid-hex-not-valid-hex-XXXX").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

    let err = load_or_create_seed(path_str).unwrap_err();
    assert!(err.contains("FATAL"), "expected FATAL prefix, got: {err}");
    assert!(err.contains("invalid hex"), "expected invalid hex msg, got: {err}");
    assert!(
        err.contains("Refusing to silently regenerate"),
        "must refuse silent regeneration, got: {err}"
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn fatal_on_wrong_byte_length() {
    let dir = tempdir();
    let path = dir.join("seed.hex");
    let path_str = path.to_str().unwrap();

    // Valid hex, wrong length (8 bytes instead of 16).
    fs::write(&path, "0011223344556677").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

    let err = load_or_create_seed(path_str).unwrap_err();
    assert!(err.contains("FATAL"));
    assert!(err.contains("8 bytes"), "expected '8 bytes', got: {err}");
    assert!(err.contains("expected 16"));
    assert!(err.contains("Refusing to silently regenerate"));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn round_trip_load_after_create() {
    let dir = tempdir();
    let path = dir.join("seed.hex");
    let path_str = path.to_str().unwrap();

    let (s1, was_created) = load_or_create_seed(path_str).expect("create");
    assert!(was_created);

    let (s2, was_created2) = load_or_create_seed(path_str).expect("load back");
    assert!(!was_created2, "second call must not re-create");
    assert_eq!(s1.bytes, s2.bytes, "round-trip must yield identical seed");

    fs::remove_dir_all(&dir).ok();
}
