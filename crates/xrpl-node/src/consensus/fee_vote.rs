//! Fee and reserve voting on flag ledgers.
//!
//! Validators vote on base fee, account reserve, and owner reserve.
//! The network takes the median of trusted validators' votes.

/// Fee voting preferences.
#[derive(Debug, Clone, Copy)]
pub struct FeeVote {
    /// Base transaction cost in drops (default: 10).
    pub reference_fee: u64,
    /// Base account reserve in drops (default: 10 XRP = 10,000,000).
    pub account_reserve: u64,
    /// Per-object owner reserve in drops (default: 2 XRP = 2,000,000).
    pub owner_reserve: u64,
}

impl Default for FeeVote {
    fn default() -> Self {
        Self {
            reference_fee: 10,
            account_reserve: 10_000_000,
            owner_reserve: 2_000_000,
        }
    }
}

/// Compute the network fee settings from validator votes.
/// Takes the median of each parameter independently.
pub fn compute_network_fees(votes: &[FeeVote]) -> FeeVote {
    if votes.is_empty() {
        return FeeVote::default();
    }

    let median = |values: &mut Vec<u64>| -> u64 {
        values.sort();
        let mid = values.len() / 2;
        #[allow(clippy::manual_is_multiple_of)]
        if values.len() % 2 == 0 {
            // Even count: use lower median (conservative)
            values[mid - 1]
        } else {
            values[mid]
        }
    };

    let mut fees: Vec<u64> = votes.iter().map(|v| v.reference_fee).collect();
    let mut account: Vec<u64> = votes.iter().map(|v| v.account_reserve).collect();
    let mut owner: Vec<u64> = votes.iter().map(|v| v.owner_reserve).collect();

    FeeVote {
        reference_fee: median(&mut fees),
        account_reserve: median(&mut account),
        owner_reserve: median(&mut owner),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_fees() {
        let f = FeeVote::default();
        assert_eq!(f.reference_fee, 10);
        assert_eq!(f.account_reserve, 10_000_000);
        assert_eq!(f.owner_reserve, 2_000_000);
    }

    #[test]
    fn median_odd() {
        let votes = vec![
            FeeVote { reference_fee: 10, account_reserve: 10_000_000, owner_reserve: 2_000_000 },
            FeeVote { reference_fee: 20, account_reserve: 15_000_000, owner_reserve: 3_000_000 },
            FeeVote { reference_fee: 15, account_reserve: 12_000_000, owner_reserve: 2_500_000 },
        ];
        let result = compute_network_fees(&votes);
        assert_eq!(result.reference_fee, 15);
        assert_eq!(result.account_reserve, 12_000_000);
        assert_eq!(result.owner_reserve, 2_500_000);
    }

    #[test]
    fn median_even_uses_lower() {
        let votes = vec![
            FeeVote { reference_fee: 10, ..FeeVote::default() },
            FeeVote { reference_fee: 20, ..FeeVote::default() },
        ];
        let result = compute_network_fees(&votes);
        assert_eq!(result.reference_fee, 10); // lower median
    }

    #[test]
    fn single_voter() {
        let votes = vec![FeeVote { reference_fee: 42, account_reserve: 5_000_000, owner_reserve: 1_000_000 }];
        let result = compute_network_fees(&votes);
        assert_eq!(result.reference_fee, 42);
    }

    #[test]
    fn empty_returns_default() {
        let result = compute_network_fees(&[]);
        assert_eq!(result.reference_fee, 10);
    }
}
