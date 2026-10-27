//! `XRPL_OBSERVE=1`: a whole validator that signs and sends nothing, for a twin run beside the live one (Step 4's
//! peer switch is proven on a twin before the live validator takes it).
//!
//! - **No keys.** The validator identity is generated fresh for the run and never written: the seed file is not
//!   opened, read or created.
//! - **Nothing sent as a validator.** No relay hub sessions, no manifest, no validation. At the signing point
//!   everything up to the signature runs (the same checks, the same hash), then the ledger is counted in
//!   [`WOULD_SIGN`] instead.
//!
//! Everything else runs as live: the peers, the engine, our own hash, the independence counters (which count real
//! signatures, so they stay at nought).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use serde_json::{json, Value};

use crate::peer::identity::NodeIdentity;

/// `XRPL_OBSERVE=1`, read once.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("XRPL_OBSERVE").map(|v| v.trim() == "1").unwrap_or(false))
}

/// Ledgers the validator would have signed, and with our own hash.
pub static WOULD_SIGN: AtomicU64 = AtomicU64::new(0);
pub static WOULD_SIGN_OWN: AtomicU64 = AtomicU64::new(0);

/// The validator's identity, and whether its seed file was created. Observing: a fresh identity for this run, the
/// seed file untouched. Else the persistent seed (`paths::load_or_create_seed`).
pub fn validator_identity(observe: bool, seed_path: &str) -> Result<(NodeIdentity, bool), String> {
    if observe {
        return NodeIdentity::generate().map(|i| (i, false)).map_err(|e| format!("identity: {e}"));
    }
    let (seed, created) = crate::paths::load_or_create_seed(seed_path)?;
    let identity = NodeIdentity::from_seed(&seed).map_err(|e| format!("identity from seed: {e}"))?;
    Ok((identity, created))
}

/// The `observe` section of /api/engine.
pub fn json() -> Value {
    json!({
        "observe": enabled(),
        "would_sign": WOULD_SIGN.load(Ordering::Relaxed),
        "would_sign_own": WOULD_SIGN_OWN.load(Ordering::Relaxed),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observe_never_touches_the_seed_file() {
        // Key generation draws on rustls's process-wide provider, which live_viewer installs at start.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = tempfile::tempdir().unwrap();
        let seed = dir.path().join("validator_seed.hex");
        let seed = seed.to_str().unwrap();
        let (_, created) = validator_identity(true, seed).unwrap();
        assert!(!created);
        assert!(!std::path::Path::new(seed).exists(), "observing creates no seed file");
        let (a, created) = validator_identity(false, seed).unwrap();
        assert!(created && std::path::Path::new(seed).exists(), "a validator makes its seed");
        let (b, created) = validator_identity(false, seed).unwrap();
        assert!(!created);
        assert_eq!(a.public_key_hex(), b.public_key_hex(), "and keeps it");
    }
}
