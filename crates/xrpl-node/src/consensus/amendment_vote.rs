//! Amendment tracking and voting on flag ledgers.

use std::collections::HashMap;

use xrpl_core::types::Hash256;

/// Every 256th ledger is a flag ledger where amendment votes are tallied.
pub fn is_flag_ledger(seq: u32) -> bool {
    #[allow(clippy::manual_is_multiple_of)]
    { seq > 0 && seq % 256 == 0 }
}

/// Tracks amendment support over time.
pub struct AmendmentTracker {
    /// Amendment hash → our vote (true = support).
    pub vote_preferences: HashMap<Hash256, bool>,
    /// Amendment hash → consecutive flag ledgers with >80% support.
    support_streaks: HashMap<Hash256, u32>,
}

impl AmendmentTracker {
    pub fn new() -> Self {
        Self {
            vote_preferences: HashMap::new(),
            support_streaks: HashMap::new(),
        }
    }

    /// Get our votes for all known amendments.
    pub fn get_votes(&self) -> Vec<(Hash256, bool)> {
        self.vote_preferences.iter().map(|(&h, &v)| (h, v)).collect()
    }

    /// Process a flag ledger: tally support from validator votes.
    /// `votes` maps amendment hash → list of validator keys that support it.
    pub fn process_flag_ledger(
        &mut self,
        votes: &HashMap<Hash256, Vec<String>>,
        trusted_count: usize,
    ) {
        for (&amendment, supporters) in votes {
            let pct = if trusted_count > 0 {
                supporters.len() as f64 / trusted_count as f64
            } else {
                0.0
            };

            let streak = self.support_streaks.entry(amendment).or_default();
            if pct > 0.80 {
                *streak += 1;
            } else {
                *streak = 0; // reset streak
            }
        }
    }

    /// Check if an amendment should activate.
    /// Requires >80% support for approximately 2 weeks of flag ledgers.
    /// At ~15 min per flag ledger (256 ledgers * ~3.5s), 2 weeks ≈ 1344 flag ledgers.
    pub fn should_activate(&self, amendment: &Hash256) -> bool {
        self.support_streaks
            .get(amendment)
            .is_some_and(|&streak| streak >= 1344)
    }

    /// Get the current support streak for an amendment.
    pub fn support_streak(&self, amendment: &Hash256) -> u32 {
        self.support_streaks.get(amendment).copied().unwrap_or(0)
    }
}

impl Default for AmendmentTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(b: u8) -> Hash256 {
        Hash256([b; 32])
    }

    #[test]
    fn flag_ledger_detection() {
        assert!(!is_flag_ledger(0));
        assert!(!is_flag_ledger(1));
        assert!(!is_flag_ledger(255));
        assert!(is_flag_ledger(256));
        assert!(!is_flag_ledger(257));
        assert!(is_flag_ledger(512));
        assert!(is_flag_ledger(15872000)); // real-ish number
    }

    #[test]
    fn track_support() {
        let mut tracker = AmendmentTracker::new();
        let amendment = h(0xAA);

        // 5 of 5 support (100%) → streak increments
        let mut votes = HashMap::new();
        votes.insert(amendment, vec!["V1".into(), "V2".into(), "V3".into(), "V4".into(), "V5".into()]);

        tracker.process_flag_ledger(&votes, 5);
        assert_eq!(tracker.support_streak(&amendment), 1);

        tracker.process_flag_ledger(&votes, 5);
        assert_eq!(tracker.support_streak(&amendment), 2);
    }

    #[test]
    fn streak_resets_below_threshold() {
        let mut tracker = AmendmentTracker::new();
        let amendment = h(0xBB);

        // Build up streak
        let mut votes = HashMap::new();
        votes.insert(amendment, vec!["V1".into(), "V2".into(), "V3".into(), "V4".into(), "V5".into()]);
        tracker.process_flag_ledger(&votes, 5);
        tracker.process_flag_ledger(&votes, 5);
        assert_eq!(tracker.support_streak(&amendment), 2);

        // Drop below 80% → reset
        votes.insert(amendment, vec!["V1".into()]); // 1 of 5 = 20%
        tracker.process_flag_ledger(&votes, 5);
        assert_eq!(tracker.support_streak(&amendment), 0);
    }

    #[test]
    fn activation_requires_long_streak() {
        let tracker = AmendmentTracker::new();
        assert!(!tracker.should_activate(&h(0xCC)));
    }
}
