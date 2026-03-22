//! Transactor — common transaction application pattern.
//!
//! Every transaction type implements the Transactor trait:
//! - `preflight`: format validation (no state access)
//! - `preclaim`: read-only state checks
//! - `do_apply`: modify state in sandbox
//!
//! Common logic (fee deduction, sequence increment) runs for ALL types.

use super::keylet;
use super::sandbox::Sandbox;

/// Transaction engine result codes matching rippled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxResult {
    // Success
    /// Transaction applied successfully.
    Success,

    // tec — claimed cost, no state change beyond fee
    /// Insufficient XRP to cover fee.
    InsufficientFee,
    /// Insufficient funds for the payment.
    UnfundedPayment,
    /// Destination account doesn't exist.
    NoDst,
    /// Destination needs more XRP to meet reserve.
    NoDstInsufXrp,
    /// Path payment found no liquidity.
    PathDry,
    /// Offer conditions can't be met.
    Unfunded,
    /// No permission for this operation.
    NoPermission,
    /// Object not found.
    NoEntry,

    // tem — malformed, not applied at all
    /// Transaction is malformed.
    Malformed,
    /// Invalid fee.
    BadFee,
    /// Invalid amount.
    BadAmount,
    /// Invalid sequence.
    BadSequence,

    // tef — failed, not applied
    /// Sequence already past.
    PastSeq,
    /// LastLedgerSequence exceeded.
    MaxLedger,
    /// Account not found.
    NoAccount,

    // Unsupported transaction type — deduct fee but skip apply
    Unsupported,
}

impl TxResult {
    /// Whether this result means the transaction was "claimed" (fee deducted).
    pub fn is_claimed(&self) -> bool {
        match self {
            TxResult::Success => true,
            // tec codes: fee is claimed
            TxResult::InsufficientFee
            | TxResult::UnfundedPayment
            | TxResult::NoDst
            | TxResult::NoDstInsufXrp
            | TxResult::PathDry
            | TxResult::Unfunded
            | TxResult::NoPermission
            | TxResult::NoEntry
            | TxResult::Unsupported => true,
            // tem/tef: not claimed
            _ => false,
        }
    }

    /// Whether this result is success.
    pub fn is_success(&self) -> bool {
        *self == TxResult::Success
    }

    /// rippled result code string.
    pub fn code_str(&self) -> &'static str {
        match self {
            TxResult::Success => "tesSUCCESS",
            TxResult::InsufficientFee => "tecINSUFFICIENT_FEE",
            TxResult::UnfundedPayment => "tecUNFUNDED_PAYMENT",
            TxResult::NoDst => "tecNO_DST",
            TxResult::NoDstInsufXrp => "tecNO_DST_INSUF_XRP",
            TxResult::PathDry => "tecPATH_DRY",
            TxResult::Unfunded => "tecUNFUNDED",
            TxResult::NoPermission => "tecNO_PERMISSION",
            TxResult::NoEntry => "tecNO_ENTRY",
            TxResult::Malformed => "temMALFORMED",
            TxResult::BadFee => "temBAD_FEE",
            TxResult::BadAmount => "temBAD_AMOUNT",
            TxResult::BadSequence => "temBAD_SEQUENCE",
            TxResult::PastSeq => "tefPAST_SEQ",
            TxResult::MaxLedger => "tefMAX_LEDGER",
            TxResult::NoAccount => "tefNO_ACCOUNT",
            TxResult::Unsupported => "tecUNSUPPORTED",
        }
    }
}

/// Decoded transaction fields needed for the transaction engine.
#[derive(Debug, Clone)]
pub struct TxFields {
    /// Sender's 20-byte account ID.
    pub account: [u8; 20],
    /// Transaction type string (e.g. "Payment", "OfferCreate").
    pub tx_type: String,
    /// Fee in drops.
    pub fee: u64,
    /// Sequence number (0 if using a Ticket).
    pub sequence: u32,
    /// TicketSequence (if using a ticket instead of sequence).
    pub ticket_seq: Option<u32>,
    /// LastLedgerSequence (optional).
    pub last_ledger_seq: Option<u32>,
    /// Raw JSON for type-specific fields.
    pub fields: serde_json::Value,
}

impl TxFields {
    /// Whether this transaction uses a Ticket instead of a regular Sequence.
    pub fn uses_ticket(&self) -> bool {
        self.sequence == 0 && self.ticket_seq.is_some()
    }
}

/// Trait that every transaction type implements.
pub trait Transactor {
    /// Format validation — no state access.
    fn preflight(&self, tx: &TxFields) -> TxResult;

    /// Read-only state validation.
    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult;

    /// Apply state modifications to the sandbox.
    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult;
}

/// Deduct fee from sender and increment sequence.
/// This runs for EVERY successfully-claimed transaction.
pub fn apply_common(tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
    let acct_key = keylet::account_root_key(&tx.account);

    // Read sender's AccountRoot
    let acct_data = match sandbox.read(&acct_key) {
        Some(data) => data,
        None => return TxResult::NoAccount,
    };

    // Decode the AccountRoot JSON
    let mut acct: serde_json::Value = match serde_json::from_slice(&acct_data) {
        Ok(v) => v,
        Err(_) => return TxResult::Malformed,
    };

    // Check balance >= fee
    let balance = acct["Balance"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    if balance < tx.fee {
        return TxResult::InsufficientFee;
    }

    // Deduct fee
    acct["Balance"] = serde_json::Value::String((balance - tx.fee).to_string());

    // Increment sequence only for non-ticket transactions.
    // Ticket-based txs (Sequence=0, TicketSequence present) don't touch the account sequence.
    if !tx.uses_ticket() {
        let seq = acct["Sequence"].as_u64().unwrap_or(0) as u32;
        let next_seq = match seq.checked_add(1) {
            Some(n) => n,
            None => return TxResult::Malformed,
        };
        acct["Sequence"] = serde_json::Value::Number(next_seq.into());
    }

    // Write back
    let serialized = serde_json::to_vec(&acct).unwrap_or_default();
    sandbox.write(acct_key, serialized);

    TxResult::Success
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::state::LedgerState;
    use xrpl_core::types::Hash256;

    fn test_state_with_account(account_id: &[u8; 20], balance: u64, seq: u32) -> LedgerState {
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
        let mut state = LedgerState::new_unverified(header);

        // Insert account as JSON
        let acct_json = serde_json::json!({
            "LedgerEntryType": "AccountRoot",
            "Account": hex::encode(account_id),
            "Balance": balance.to_string(),
            "Sequence": seq,
            "OwnerCount": 0,
        });
        let key = keylet::account_root_key(account_id);
        state
            .state_map
            .insert(key, serde_json::to_vec(&acct_json).unwrap())
            .unwrap();

        state
    }

    fn test_tx(account: [u8; 20], fee: u64, sequence: u32) -> TxFields {
        TxFields {
            account,
            tx_type: "Payment".to_string(),
            fee,
            sequence,
            last_ledger_seq: None,
            ticket_seq: None,
            fields: serde_json::Value::Null,
        }
    }

    #[test]
    fn apply_common_deducts_fee() {
        let acct = [0x01u8; 20];
        let state = test_state_with_account(&acct, 1_000_000, 1);

        let mut sandbox = Sandbox::new(&state);
        let tx = test_tx(acct, 12, 1);
        let result = apply_common(&tx, &mut sandbox);
        assert_eq!(result, TxResult::Success);

        // Read back and verify
        let key = keylet::account_root_key(&acct);
        let data = sandbox.read(&key).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(v["Balance"].as_str().unwrap(), "999988"); // 1_000_000 - 12
        assert_eq!(v["Sequence"].as_u64().unwrap(), 2);
    }

    #[test]
    fn apply_common_insufficient_fee() {
        let acct = [0x02u8; 20];
        let state = test_state_with_account(&acct, 5, 1); // only 5 drops

        let mut sandbox = Sandbox::new(&state);
        let tx = test_tx(acct, 12, 1); // fee=12 > balance=5
        let result = apply_common(&tx, &mut sandbox);
        assert_eq!(result, TxResult::InsufficientFee);
    }

    #[test]
    fn apply_common_no_account() {
        let acct = [0x03u8; 20];
        let state = test_state_with_account(&[0xFF; 20], 1_000_000, 1); // different account

        let mut sandbox = Sandbox::new(&state);
        let tx = test_tx(acct, 12, 1);
        let result = apply_common(&tx, &mut sandbox);
        assert_eq!(result, TxResult::NoAccount);
    }

    #[test]
    fn tx_result_codes() {
        assert_eq!(TxResult::Success.code_str(), "tesSUCCESS");
        assert!(TxResult::Success.is_success());
        assert!(TxResult::Success.is_claimed());
        assert!(!TxResult::Malformed.is_claimed());
        assert!(!TxResult::PastSeq.is_claimed());
        assert!(TxResult::NoDst.is_claimed()); // tec codes are claimed
    }
}
