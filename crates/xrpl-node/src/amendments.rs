//! Amendment-set helpers for runtime refresh (reaudit 2026-06-10 finding F6/va-07).
//!
//! Pure and non-FFI so they unit-test without libxrpl. The FFI verifier fetches
//! the active mainnet amendment set once at startup; an amendment that reaches
//! 2-week majority and activates *while the validator runs* would be absent from
//! that frozen list, so every tx exercising its behavior diverges → 3 strikes →
//! halt. These helpers drive a flag-ledger refresh of the live set.
//!
//! SCOPING (reaudit §6): refreshing the ID list only rescues amendments whose
//! semantics are already compiled into the linked libxrpl. An amendment introduced
//! *in* a newer rippled needs the rebuild — runtime IDs cannot supply missing
//! engine semantics.

/// XRPL flag ledgers (where amendment voting/activation is tallied) occur every
/// 256 sequences. Refresh the active amendment set on each one.
pub fn is_flag_ledger(seq: u32) -> bool {
    seq != 0 && seq % 256 == 0
}

/// Difference between the current active amendment set and a freshly fetched one.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct AmendmentDelta {
    pub added: Vec<[u8; 32]>,
    pub removed: Vec<[u8; 32]>,
}

impl AmendmentDelta {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

/// Amendments present in `fetched` but not `current` (added) and in `current` but
/// not `fetched` (removed). Order-independent; output preserves input order.
pub fn diff_amendment_sets(current: &[[u8; 32]], fetched: &[[u8; 32]]) -> AmendmentDelta {
    use std::collections::HashSet;
    let cur: HashSet<[u8; 32]> = current.iter().copied().collect();
    let new: HashSet<[u8; 32]> = fetched.iter().copied().collect();
    AmendmentDelta {
        added: fetched.iter().copied().filter(|a| !cur.contains(a)).collect(),
        removed: current.iter().copied().filter(|a| !new.contains(a)).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn amd(n: u8) -> [u8; 32] {
        [n; 32]
    }

    #[test]
    fn flag_ledger_detection() {
        assert!(!is_flag_ledger(0), "ledger 0 is not a flag ledger");
        assert!(!is_flag_ledger(1));
        assert!(!is_flag_ledger(255));
        assert!(is_flag_ledger(256));
        assert!(!is_flag_ledger(257));
        assert!(is_flag_ledger(512));
        assert!(is_flag_ledger(256 * 1000));
    }

    #[test]
    fn identical_sets_have_no_delta() {
        let set = [amd(1), amd(2), amd(3)];
        assert!(diff_amendment_sets(&set, &set).is_empty());
    }

    #[test]
    fn newly_activated_amendment_is_added() {
        let current = [amd(1), amd(2)];
        let fetched = [amd(1), amd(2), amd(3)];
        let d = diff_amendment_sets(&current, &fetched);
        assert_eq!(d.added, vec![amd(3)]);
        assert!(d.removed.is_empty());
    }

    #[test]
    fn dropped_amendment_is_removed() {
        let current = [amd(1), amd(2), amd(3)];
        let fetched = [amd(1), amd(3)];
        let d = diff_amendment_sets(&current, &fetched);
        assert!(d.added.is_empty());
        assert_eq!(d.removed, vec![amd(2)]);
    }

    #[test]
    fn added_and_removed_together_order_independent() {
        let current = [amd(3), amd(1), amd(2)];
        let fetched = [amd(2), amd(4), amd(1)];
        let d = diff_amendment_sets(&current, &fetched);
        assert_eq!(d.added, vec![amd(4)]);
        assert_eq!(d.removed, vec![amd(3)]);
    }
}
