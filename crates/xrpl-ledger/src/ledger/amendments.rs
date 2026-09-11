//! Amendment state, read from the ledger's own Amendments singleton
//! (keylet 7DB0788C…, `sfAmendments` = the enabled IDs) — rippled's
//! `Rules::enabled`. A state that carries no Amendments object (a bundle
//! drill's seated pre-images, a synthetic test ledger) answers "not
//! enabled", so every pre-amendment specimen keeps the rules it was pinned
//! under and the live mirror applies the amendment from the flag ledger
//! that enabled it.
//!
//! Finding 259: fixCleanup3_3_0 went live on mainnet at flag ledger
//! 106912768 (2026-09-11 ~12:51 UTC) with deploy117 already running. Until
//! then the engine hard-coded the pre-amendment rules it had specimens for.

use super::keylet;
use super::sandbox::Sandbox;

/// fixCleanup3_3_0 — AMM deposit/withdraw freeze scope, AMM precision-loss
/// guard, CheckID zero, pseudo-account signers, domain-book validation.
pub const FIX_CLEANUP_3_3_0: &str =
    "3298D47E1F3A8A24FECAA30F699B8FE1DD234E072834BA099AD8180FFCE0FEC4";

/// Whether the amendment with this 256-bit ID (upper-case hex) is enabled
/// in the ledger the sandbox is built on.
pub fn enabled(sandbox: &Sandbox, id_hex: &str) -> bool {
    let Some(bytes) = sandbox.read(&keylet::amendments_key()) else { return false };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return false };
    v.get("Amendments")
        .and_then(|a| a.as_array())
        .is_some_and(|a| a.iter().any(|x| x.as_str().is_some_and(|s| s.eq_ignore_ascii_case(id_hex))))
}

pub fn fix_cleanup_3_3_0(sandbox: &Sandbox) -> bool {
    enabled(sandbox, FIX_CLEANUP_3_3_0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::state::LedgerState;
    use xrpl_core::types::Hash256;

    fn test_state() -> LedgerState {
        let header = LedgerHeader {
            sequence: 1,
            total_coins: 100_000_000_000_000_000,
            parent_hash: Hash256([0; 32]),
            transaction_hash: Hash256([0; 32]),
            account_hash: Hash256([0; 32]),
            parent_close_time: 0,
            close_time: 10,
            close_time_resolution: 10,
            close_flags: 0,
        };
        LedgerState::new_unverified(header)
    }

    #[test]
    fn reads_the_amendments_singleton_and_defaults_to_off() {
        let mut state = test_state();
        assert!(!fix_cleanup_3_3_0(&Sandbox::new(&state)), "no singleton → off");
        let obj = serde_json::json!({
            "LedgerEntryType": "Amendments",
            "Flags": 0,
            "Amendments": ["00000000000000000000000000000000000000000000000000000000000000AA", FIX_CLEANUP_3_3_0.to_lowercase()],
        });
        state.state_map.insert(keylet::amendments_key(), serde_json::to_vec(&obj).unwrap()).unwrap();
        assert!(fix_cleanup_3_3_0(&Sandbox::new(&state)));
        assert!(!enabled(&Sandbox::new(&state), "00000000000000000000000000000000000000000000000000000000000000BB"));
    }
}
