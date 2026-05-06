//! Centralized data-path helpers.
//!
//! Historically the validator hardcoded `/mnt/xrpl-data/...` in roughly a
//! dozen places, while `bulk_sync` started respecting `XRPL_DATA_DIR`. The
//! result was a quiet split-brain on m3060: the bulk syncer wrote
//! `dl_done.txt` to `/home/m3060/xrpl-data/sync/` (via `XRPL_DATA_DIR`) but
//! `live_viewer`'s backfill read `/mnt/xrpl-data/sync/dl_done.txt` directly.
//! Backfill found nothing, logged a warning, and silently no-op'd. Nobody
//! noticed because backfill isn't on the FFI verification path.
//!
//! This module is the single source of truth for every disk path the
//! validator touches. The defaults preserve the original `/mnt/xrpl-data`
//! layout, so existing deployments that don't set the env vars are
//! unaffected. Anything that sets `XRPL_DATA_DIR` (like m3060) gets
//! consistent reads and writes.
//!
//! ## Env vars
//!
//! | Variable             | Default                                 | What it controls                |
//! |----------------------|-----------------------------------------|---------------------------------|
//! | `XRPL_DATA_DIR`      | `/mnt/xrpl-data`                        | Root of all validator data      |
//! | `XRPL_ROCKS_PATH`    | `${XRPL_DATA_DIR}/sync/state.rocks`     | RocksDB state directory         |
//! | `XRPL_SEED_PATH`     | `${XRPL_DATA_DIR}/validator_seed.hex`   | Validator's signing key         |
//! | `XRPL_ENGINE_STATE_PATH` | `${XRPL_DATA_DIR}/engine_state.json` | FFI engine state snapshot       |
//!
//! Setting `XRPL_DATA_DIR` cascades through all derived paths unless one of
//! the more specific variables is also set. The more specific variables
//! always win when both are set.

/// Root data directory. Defaults to `/mnt/xrpl-data` for legacy deployments.
pub fn data_dir() -> String {
    std::env::var("XRPL_DATA_DIR").unwrap_or_else(|_| "/mnt/xrpl-data".to_string())
}

/// Directory holding everything the bulk + incremental sync write to disk.
pub fn sync_dir() -> String {
    format!("{}/sync", data_dir())
}

/// RocksDB state directory. Honors `XRPL_ROCKS_PATH` if set, otherwise
/// derives from [`sync_dir`].
pub fn state_rocks_path() -> String {
    std::env::var("XRPL_ROCKS_PATH").unwrap_or_else(|_| format!("{}/state.rocks", sync_dir()))
}

/// File the bulk syncer writes after a verified successful sync, and the
/// backfill / catchup paths read to know how far ahead they need to fill.
pub fn dl_done_path() -> String {
    format!("{}/dl_done.txt", sync_dir())
}

/// File the live loop writes after the first state-hash MATCH so subsequent
/// startups know the database is operational.
pub fn sync_complete_marker_path() -> String {
    format!("{}/sync_complete.marker", sync_dir())
}

/// Validator's signing key file (Ed25519 seed in hex). Honors
/// `XRPL_SEED_PATH` if set.
pub fn seed_path() -> String {
    std::env::var("XRPL_SEED_PATH").unwrap_or_else(|_| format!("{}/validator_seed.hex", data_dir()))
}

/// Load the validator seed from disk, or create one with mode 0600 if absent.
///
/// VALAUDIT Phase 4 (va-04) hardens three failure modes the previous in-line
/// loader silently allowed:
///
/// 1. **Permission check is FATAL on `mode & 0o077 != 0`** instead of warning.
///    A group/other-readable seed file is a key compromise risk; the audit's
///    finding was that the warn-only check let operators ignore the issue.
/// 2. **Atomic create with mode 0600** via `OpenOptions::create_new().mode(0600)`
///    closes the umask-race window between the previous `write()` (which used
///    default umask) and any later `chmod`. The file never exists at any other
///    permission.
/// 3. **Malformed seed (bad hex / wrong length) is FATAL**, not silently
///    regenerated. The previous code called `Seed::generate_with_type()` on any
///    parse failure — catastrophic if a future bug breaks the parser on a valid
///    file (operator's identity silently rotated, validator becomes byzantine
///    fork from its own UNL's perspective).
///
/// Returns `(Seed, was_created)` so the caller can log "Loaded" vs "Generated"
/// without re-checking existence. Errors are formatted strings the caller
/// should print and `exit(1)` on.
#[cfg(unix)]
pub fn load_or_create_seed(seed_path: &str) -> Result<(xrpl_core::crypto::signing::Seed, bool), String> {
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use xrpl_core::address::KeyType;
    use xrpl_core::crypto::signing::Seed;

    match fs::metadata(seed_path) {
        Ok(meta) => {
            // 1. FATAL on bad mode — closes the warn-only audit finding.
            let mode = meta.mode();
            if mode & 0o077 != 0 {
                return Err(format!(
                    "FATAL: seed file {} has mode {:o} — readable by group or others. \
                     Run: chmod 600 {}",
                    seed_path, mode & 0o777, seed_path
                ));
            }
            // 3. FATAL on malformed seed — refuse to silently regenerate identity.
            let hex_str = fs::read_to_string(seed_path)
                .map_err(|e| format!("FATAL: read seed file {}: {e}", seed_path))?;
            let hex_str = hex_str.trim();
            let bytes = hex::decode(hex_str)
                .map_err(|e| format!(
                    "FATAL: seed file {} contains invalid hex ({e}). \
                     Refusing to silently regenerate validator identity.",
                    seed_path
                ))?;
            if bytes.len() != 16 {
                return Err(format!(
                    "FATAL: seed file {} has {} bytes, expected 16. \
                     Refusing to silently regenerate validator identity.",
                    seed_path, bytes.len()
                ));
            }
            let mut arr = [0u8; 16];
            arr.copy_from_slice(&bytes);
            Ok((Seed { bytes: arr, key_type: KeyType::Secp256k1 }, false))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Ensure the parent directory exists so create_new() can succeed.
            if let Some(parent) = std::path::Path::new(seed_path).parent() {
                let _ = fs::create_dir_all(parent);
            }
            // 2. Atomic create with 0600 — no window for the file to exist at
            //    any wider mode.
            let s = Seed::generate_with_type(KeyType::Secp256k1);
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(seed_path)
                .map_err(|e| format!("FATAL: create seed file {}: {e}", seed_path))?;
            f.write_all(hex::encode(s.bytes).as_bytes())
                .map_err(|e| format!("FATAL: write seed {}: {e}", seed_path))?;
            f.sync_all().ok();
            Ok((s, true))
        }
        Err(e) => Err(format!("FATAL: stat seed file {}: {e}", seed_path)),
    }
}

/// FFI engine state snapshot path (JSON). Honors `XRPL_ENGINE_STATE_PATH` if set.
pub fn engine_state_path() -> String {
    std::env::var("XRPL_ENGINE_STATE_PATH").unwrap_or_else(|_| format!("{}/engine_state.json", data_dir()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Cargo runs tests in parallel by default and the helpers below all
    /// poke at the same process-global env vars. Hold this mutex for the
    /// duration of any test that mutates them.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn unset_all() {
        for v in [
            "XRPL_DATA_DIR",
            "XRPL_ROCKS_PATH",
            "XRPL_SEED_PATH",
            "XRPL_ENGINE_STATE_PATH",
        ] {
            std::env::remove_var(v);
        }
    }

    #[test]
    fn defaults_match_legacy_layout() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unset_all();
        assert_eq!(data_dir(), "/mnt/xrpl-data");
        assert_eq!(sync_dir(), "/mnt/xrpl-data/sync");
        assert_eq!(state_rocks_path(), "/mnt/xrpl-data/sync/state.rocks");
        assert_eq!(dl_done_path(), "/mnt/xrpl-data/sync/dl_done.txt");
        assert_eq!(sync_complete_marker_path(), "/mnt/xrpl-data/sync/sync_complete.marker");
        assert_eq!(seed_path(), "/mnt/xrpl-data/validator_seed.hex");
        assert_eq!(engine_state_path(), "/mnt/xrpl-data/engine_state.json");
    }

    #[test]
    fn data_dir_cascades_into_all_derived_paths() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unset_all();
        std::env::set_var("XRPL_DATA_DIR", "/home/m3060/xrpl-data");

        assert_eq!(data_dir(), "/home/m3060/xrpl-data");
        assert_eq!(sync_dir(), "/home/m3060/xrpl-data/sync");
        assert_eq!(state_rocks_path(), "/home/m3060/xrpl-data/sync/state.rocks");
        assert_eq!(dl_done_path(), "/home/m3060/xrpl-data/sync/dl_done.txt");
        assert_eq!(sync_complete_marker_path(), "/home/m3060/xrpl-data/sync/sync_complete.marker");
        assert_eq!(seed_path(), "/home/m3060/xrpl-data/validator_seed.hex");
        assert_eq!(engine_state_path(), "/home/m3060/xrpl-data/engine_state.json");

        unset_all();
    }

    #[test]
    fn specific_overrides_beat_data_dir() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unset_all();
        std::env::set_var("XRPL_DATA_DIR", "/home/m3060/xrpl-data");
        std::env::set_var("XRPL_ROCKS_PATH", "/some/other/place/db");
        std::env::set_var("XRPL_SEED_PATH", "/secrets/seed.hex");

        // Specific overrides win
        assert_eq!(state_rocks_path(), "/some/other/place/db");
        assert_eq!(seed_path(), "/secrets/seed.hex");

        // Non-overridden ones still cascade from data_dir
        assert_eq!(dl_done_path(), "/home/m3060/xrpl-data/sync/dl_done.txt");
        assert_eq!(engine_state_path(), "/home/m3060/xrpl-data/engine_state.json");

        unset_all();
    }

    /// The exact regression case: bulk_sync writes to `XRPL_DATA_DIR/sync/dl_done.txt`,
    /// live_viewer's backfill reads from a hardcoded path. With the helpers,
    /// both end up at the same place when `XRPL_DATA_DIR` is set.
    #[test]
    fn writer_and_reader_agree_under_data_dir_override() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unset_all();
        std::env::set_var("XRPL_DATA_DIR", "/home/m3060/xrpl-data");

        // bulk_sync's writer would call this:
        let writer_target = dl_done_path();
        // live_viewer's backfill reader would call the same:
        let reader_target = dl_done_path();

        assert_eq!(writer_target, reader_target);
        assert_eq!(writer_target, "/home/m3060/xrpl-data/sync/dl_done.txt");

        unset_all();
    }
}
