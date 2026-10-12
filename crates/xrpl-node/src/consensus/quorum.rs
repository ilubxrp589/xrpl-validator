//! The validation quorum: how many trusted validations make a ledger validated (rippled 3.4.1).
//! Built for 5.2(b); nothing calls it yet.
//!
//! ## The formula
//!
//! rippled's `ValidatorList::calculateQuorum` (ValidatorList.cpp:1708-1797) ends with
//! `std::max(std::ceil(effectiveUnlSize * 0.8f), std::ceil(unlSize * 0.6f))` (:1795-1796):
//! - 80% of the effective UNL, the safety bound;
//! - 60% of the whole UNL, a floor when the negative UNL shrinks the effective size.
//!
//! The arithmetic is single-precision float, and [`quorum`] repeats it exactly, because the floats
//! round:
//! - The 80% term always lands on exact maths below 2^20 validators.
//! - The 60% term rounds up at some multiples of 5 (25, 45, 50, 85, …). For example, `45 × 0.6f`
//!   is 27.000002, so its ceiling is 28, not 27.
//! - Today the list has 35 validators and none disabled: max(28, 21) = 28.
//!
//! The rest of `calculateQuorum` is the caller's business, not this function's:
//! - A command-line quorum overrides everything (:1714-1720).
//! - While too many configured publisher lists are unavailable, the quorum is unreachable,
//!   `usize::MAX` (:1723-1758).
//!
//! ## The effective UNL: the negative UNL
//!
//! At the start of every round, `NetworkOPsImp::beginConsensus` (NetworkOPs.cpp:2254-2260):
//! 1. takes the negative UNL of the ledger the round builds on (`Ledger::negativeUNL`,
//!    Ledger.cpp:639-660): the master keys in the `DisabledValidators` array of its NegativeUNL
//!    object;
//! 2. then reruns `ValidatorList::updateTrusted`, which counts the trusted master keys that are
//!    not on that list (ValidatorList.cpp:1939-1948) and recomputes the quorum (:1960).
//!
//! Two limits apply:
//! - **Pending changes:** `ValidatorToDisable` and `ValidatorToReEnable` don't count until a flag
//!   ledger moves them into the array.
//! - **The cap:** `NegativeUNLVote` lists at most `ceil(25%)` of the UNL
//!   (NegativeUNLVote.cpp:244). For today's 35 that's 9, so the effective UNL never drops below
//!   26, and the quorum never below 21.

use std::collections::HashSet;

/// rippled's validation quorum for a UNL of `unl_size` validators, `effective_unl_size` of them not
/// on the negative UNL: `max(ceil(0.8 × effective), ceil(0.6 × UNL))`, in rippled's
/// single-precision arithmetic (ValidatorList.cpp:1795-1796).
pub fn quorum(unl_size: usize, effective_unl_size: usize) -> usize {
    let eighty = (effective_unl_size as f32 * 0.8_f32).ceil();
    let sixty = (unl_size as f32 * 0.6_f32).ceil();
    eighty.max(sixty) as usize
}

/// rippled's effective UNL size (`ValidatorList::updateTrusted`, ValidatorList.cpp:1939-1948):
/// the trusted validators whose master keys are not on the negative UNL.
pub fn effective_unl_size(trusted_masters: &[Vec<u8>], negative_unl: &[Vec<u8>]) -> usize {
    let disabled: HashSet<&[u8]> = negative_unl.iter().map(Vec::as_slice).collect();
    trusted_masters.iter().filter(|k| !disabled.contains(k.as_slice())).count()
}

/// The master keys on a ledger's negative UNL, read from its NegativeUNL object as JSON: the
/// `PublicKey` of each entry in `DisabledValidators`.
/// - It mirrors rippled's `Ledger::negativeUNL` (Ledger.cpp:639-660).
/// - A key that isn't a public key is skipped, as rippled skips one `publicKeyType` rejects.
/// - Pending changes (`ValidatorToDisable`, `ValidatorToReEnable`) are not on the list yet.
pub fn disabled_validators(negative_unl: &serde_json::Value) -> Vec<Vec<u8>> {
    negative_unl["DisabledValidators"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry["DisabledValidator"]["PublicKey"].as_str())
        .filter_map(|key| hex::decode(key).ok())
        .filter(|key| key.len() == 33 && matches!(key[0], 0x02 | 0x03 | 0xED))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todays_list_of_35_needs_28() {
        assert_eq!(quorum(35, 35), 28);
    }

    #[test]
    fn the_negative_unl_lowers_the_quorum_down_to_its_floor() {
        assert_eq!(quorum(35, 34), 28, "ceil(27.2)");
        assert_eq!(quorum(35, 33), 27, "ceil(26.4)");
        // The negative UNL lists at most ceil(25% of 35) = 9: both terms meet at 21.
        assert_eq!(quorum(35, 26), 21);
        // A negative UNL bigger than our own list allows (a UNL that differs from the network's):
        // the 60% floor binds.
        assert_eq!(quorum(20, 10), 12);
    }

    /// rippled's floats, not exact maths: 45 × 0.6f is 27.000002, so a 45-validator list with
    /// 12 disabled needs 28, where exact maths would say max(27, 27) = 27.
    #[test]
    fn the_float_rounding_is_rippleds() {
        assert_eq!(quorum(45, 33), 28);
        assert_eq!(quorum(25, 18), 16, "25 × 0.6f is 15.000001; exact maths says max(15, 15)");
    }

    #[test]
    fn small_and_round_sizes() {
        assert_eq!(quorum(0, 0), 0);
        assert_eq!(quorum(1, 1), 1);
        assert_eq!(quorum(5, 5), 4);
        assert_eq!(quorum(10, 10), 8);
        assert_eq!(quorum(36, 36), 29, "ceil(28.8)");
    }

    fn key(b: u8) -> Vec<u8> {
        let mut k = vec![0xED];
        k.extend_from_slice(&[b; 32]);
        k
    }

    #[test]
    fn the_effective_unl_leaves_out_disabled_validators_on_our_list() {
        let trusted: Vec<Vec<u8>> = (0..35).map(key).collect();
        // Two of ours are disabled, and one key on the negative UNL isn't on our list at all.
        let negative = vec![key(3), key(17), key(200)];
        assert_eq!(effective_unl_size(&trusted, &negative), 33);
        assert_eq!(quorum(trusted.len(), effective_unl_size(&trusted, &negative)), 27);
        assert_eq!(effective_unl_size(&trusted, &[]), 35);
    }

    #[test]
    fn disabled_validators_reads_the_negative_unl_object() {
        let object = serde_json::json!({
            "LedgerEntryType": "NegativeUNL",
            "DisabledValidators": [
                { "DisabledValidator": { "FirstLedgerSequence": 107_599_872, "PublicKey": hex::encode(key(3)) } },
                { "DisabledValidator": { "FirstLedgerSequence": 107_600_128, "PublicKey": hex::encode(key(17)) } },
                // Not a public key: rippled skips it.
                { "DisabledValidator": { "FirstLedgerSequence": 1, "PublicKey": "0412" } },
            ],
            // Pending until the next flag ledger: not on the list yet.
            "ValidatorToDisable": hex::encode(key(9)),
        });
        assert_eq!(disabled_validators(&object), vec![key(3), key(17)]);
        assert!(disabled_validators(&serde_json::json!({"LedgerEntryType": "NegativeUNL"})).is_empty());
    }
}
