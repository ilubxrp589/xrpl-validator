//! Transaction type dispatcher.
//!
//! Routes decoded transactions to the correct Transactor implementation
//! based on TransactionType string.

use super::account::{AccountDeleteTransactor, AccountSetTransactor};
use super::offer::{OfferCancelTransactor, OfferCreateTransactor};
use super::payment::PaymentTransactor;
use super::trust_set::TrustSetTransactor;
use crate::ledger::transactor::Transactor;

/// Get the Transactor for a given transaction type string.
/// Returns None for unsupported types.
pub fn get_transactor(tx_type: &str) -> Option<Box<dyn Transactor>> {
    match tx_type {
        "Payment" => Some(Box::new(PaymentTransactor)),
        "OfferCreate" => Some(Box::new(OfferCreateTransactor)),
        "OfferCancel" => Some(Box::new(OfferCancelTransactor)),
        "TrustSet" => Some(Box::new(TrustSetTransactor)),
        "AccountSet" => Some(Box::new(AccountSetTransactor)),
        "AccountDelete" => Some(Box::new(AccountDeleteTransactor)),
        _ => None,
    }
}

/// Check if a transaction type is supported.
pub fn is_supported(tx_type: &str) -> bool {
    get_transactor(tx_type).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_implemented_types() {
        for ty in [
            "Payment", "OfferCreate", "OfferCancel",
            "TrustSet", "AccountSet", "AccountDelete",
        ] {
            assert!(is_supported(ty), "{} should be supported", ty);
        }
    }

    #[test]
    fn unknown_is_unsupported() {
        assert!(!is_supported("SomeFutureTxType"));
        assert!(!is_supported("EscrowCreate"));
        assert!(!is_supported("CredentialCreate"));
    }
}
