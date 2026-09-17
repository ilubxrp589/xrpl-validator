//! Transaction type dispatcher.
//!
//! Routes decoded transactions to the correct Transactor implementation
//! based on TransactionType string.
//!
//! # DEAD CODE WARNING
//!
//! This module is **not called** by the live validator. Production transaction
//! application is delegated to rippled's C++ engine via FFI — see
//! `crates/xrpl-ffi/src/lib.rs` and `crates/xrpl-node/src/ffi_engine.rs`.
//!
//! This code is retained as a reference implementation / learning artifact.
//! Tests in this module prove the code works in isolation; they do NOT prove
//! the validator is correct.
//!
//! If you are adding a new amendment or tx type: add it to the FFI path,
//! not here. See `ffi/ARCHITECTURE.md` for the architectural decision record.

use super::account::{AccountDeleteTransactor, AccountSetTransactor};
use super::amm::{
    AMMBidTransactor, AMMCreateTransactor, AMMDeleteTransactor, AMMDepositTransactor,
    AMMVoteTransactor, AMMWithdrawTransactor,
};
use super::check::{CheckCancelTransactor, CheckCashTransactor, CheckCreateTransactor};
use super::credential::{
    CredentialAcceptTransactor, CredentialCreateTransactor, CredentialDeleteTransactor,
};
use super::escrow::{EscrowCancelTransactor, EscrowCreateTransactor, EscrowFinishTransactor};
use super::mpt::{
    MPTokenAuthorizeTransactor, MPTokenIssuanceCreateTransactor,
    MPTokenIssuanceDestroyTransactor, MPTokenIssuanceSetTransactor,
};
use super::amm::AMMClawbackTransactor;
use super::xchain::{
    XChainAccountCreateCommitTransactor, XChainAddAccountCreateAttestationTransactor,
    XChainAddClaimAttestationTransactor, XChainClaimTransactor, XChainCommitTransactor,
    XChainCreateBridgeTransactor, XChainCreateClaimIDTransactor, XChainModifyBridgeTransactor,
};
use super::misc::{
    ClawbackTransactor, DIDDeleteTransactor, DIDSetTransactor,
    DepositPreauthTransactor,
    PermissionedDomainDeleteTransactor, PermissionedDomainSetTransactor,
    SetRegularKeyTransactor, SignerListSetTransactor,
};
use super::pseudo::{EnableAmendmentTransactor, SetFeeTransactor, UNLModifyTransactor};
use super::oracle::{OracleDeleteTransactor, OracleSetTransactor};
use super::ticket::TicketCreateTransactor;
use super::nftoken::{
    NFTokenAcceptOfferTransactor, NFTokenBurnTransactor, NFTokenCancelOfferTransactor,
    NFTokenCreateOfferTransactor, NFTokenMintTransactor, NFTokenModifyTransactor,
};
use super::offer::{OfferCancelTransactor, OfferCreateTransactor};
use super::pay_channel::{
    PaymentChannelClaimTransactor, PaymentChannelCreateTransactor, PaymentChannelFundTransactor,
};
use super::payment::PaymentTransactor;
use super::trust_set::TrustSetTransactor;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{
    account_txn_id_armed, apply_common, stamp_account_txn_id, Transactor, TxFields, TxResult,
};

/// Get the Transactor for a given transaction type string.
/// Returns None for unsupported types.
/// Pseudo-transactions: consensus-injected, no Account/Fee/Sequence — the
/// driver skips fee & sequence handling (apply_common) entirely.
pub fn is_pseudo(tx_type: &str) -> bool {
    matches!(tx_type, "UNLModify" | "SetFee" | "EnableAmendment")
}

pub fn get_transactor(tx_type: &str) -> Option<Box<dyn Transactor>> {
    match tx_type {
        "Payment" => Some(Box::new(PaymentTransactor)),
        "OfferCreate" => Some(Box::new(OfferCreateTransactor)),
        "OfferCancel" => Some(Box::new(OfferCancelTransactor)),
        "TrustSet" => Some(Box::new(TrustSetTransactor)),
        "AccountSet" => Some(Box::new(AccountSetTransactor)),
        "AccountDelete" => Some(Box::new(AccountDeleteTransactor)),
        "EscrowCreate" => Some(Box::new(EscrowCreateTransactor)),
        "EscrowFinish" => Some(Box::new(EscrowFinishTransactor)),
        "EscrowCancel" => Some(Box::new(EscrowCancelTransactor)),
        "CheckCreate" => Some(Box::new(CheckCreateTransactor)),
        "CheckCash" => Some(Box::new(CheckCashTransactor)),
        "CheckCancel" => Some(Box::new(CheckCancelTransactor)),
        "PaymentChannelCreate" => Some(Box::new(PaymentChannelCreateTransactor)),
        "PaymentChannelClaim" => Some(Box::new(PaymentChannelClaimTransactor)),
        "PaymentChannelFund" => Some(Box::new(PaymentChannelFundTransactor)),
        "NFTokenMint" => Some(Box::new(NFTokenMintTransactor)),
        "NFTokenBurn" => Some(Box::new(NFTokenBurnTransactor)),
        "NFTokenCreateOffer" => Some(Box::new(NFTokenCreateOfferTransactor)),
        "NFTokenAcceptOffer" => Some(Box::new(NFTokenAcceptOfferTransactor)),
        "NFTokenCancelOffer" => Some(Box::new(NFTokenCancelOfferTransactor)),
        "NFTokenModify" => Some(Box::new(NFTokenModifyTransactor)),
        "SetRegularKey" => Some(Box::new(SetRegularKeyTransactor)),
        "SignerListSet" => Some(Box::new(SignerListSetTransactor)),
        "DepositPreauth" => Some(Box::new(DepositPreauthTransactor)),
        "Clawback" => Some(Box::new(ClawbackTransactor)),
        "CredentialCreate" => Some(Box::new(CredentialCreateTransactor)),
        "CredentialDelete" => Some(Box::new(CredentialDeleteTransactor)),
        "CredentialAccept" => Some(Box::new(CredentialAcceptTransactor)),
        "AMMCreate" => Some(Box::new(AMMCreateTransactor)),
        "AMMDeposit" => Some(Box::new(AMMDepositTransactor)),
        "AMMWithdraw" => Some(Box::new(AMMWithdrawTransactor)),
        "AMMVote" => Some(Box::new(AMMVoteTransactor)),
        "AMMBid" => Some(Box::new(AMMBidTransactor)),
        "AMMDelete" => Some(Box::new(AMMDeleteTransactor)),
        "AMMClawback" => Some(Box::new(AMMClawbackTransactor)),
        "TicketCreate" => Some(Box::new(TicketCreateTransactor)),
        "OracleSet" => Some(Box::new(OracleSetTransactor)),
        "OracleDelete" => Some(Box::new(OracleDeleteTransactor)),
        "DIDSet" => Some(Box::new(DIDSetTransactor)),
        "DIDDelete" => Some(Box::new(DIDDeleteTransactor)),
        "XChainCreateBridge" => Some(Box::new(XChainCreateBridgeTransactor)),
        "XChainCreateClaimID" => Some(Box::new(XChainCreateClaimIDTransactor)),
        "XChainCommit" => Some(Box::new(XChainCommitTransactor)),
        "XChainClaim" => Some(Box::new(XChainClaimTransactor)),
        "XChainModifyBridge" => Some(Box::new(XChainModifyBridgeTransactor)),
        "XChainAccountCreateCommit" => Some(Box::new(XChainAccountCreateCommitTransactor)),
        "XChainAddClaimAttestation" => Some(Box::new(XChainAddClaimAttestationTransactor)),
        "XChainAddAccountCreateAttestation" => Some(Box::new(XChainAddAccountCreateAttestationTransactor)),
        "PermissionedDomainSet" => Some(Box::new(PermissionedDomainSetTransactor)),
        "PermissionedDomainDelete" => Some(Box::new(PermissionedDomainDeleteTransactor)),
        "MPTokenIssuanceCreate" => Some(Box::new(MPTokenIssuanceCreateTransactor)),
        "MPTokenIssuanceDestroy" => Some(Box::new(MPTokenIssuanceDestroyTransactor)),
        "MPTokenIssuanceSet" => Some(Box::new(MPTokenIssuanceSetTransactor)),
        "MPTokenAuthorize" => Some(Box::new(MPTokenAuthorizeTransactor)),
        "UNLModify" => Some(Box::new(UNLModifyTransactor)),
        "SetFee" => Some(Box::new(SetFeeTransactor)),
        "EnableAmendment" => Some(Box::new(EnableAmendmentTransactor)),
        _ => None,
    }
}

/// Check if a transaction type is supported.
pub fn is_supported(tx_type: &str) -> bool {
    get_transactor(tx_type).is_some()
}

/// The transaction pipeline on a caller-supplied sandbox — rippled's
/// `apply(app, view, tx, flags, j)` shape: preflight → preclaim →
/// common (fee, sequence/ticket) → doApply, with a claimed (`tec`)
/// result keeping only the common changes and any other failure leaving
/// the sandbox exactly as it was on entry. Returns `(result, applied)`
/// where `applied` is true for `tes` and claimed `tec` — the pair
/// `applyBatchTransactions` folds by.
///
/// Mirrors `xrpl_node::native_apply::native_apply_one`, which runs the
/// same pipeline on a fresh sandbox over a `LedgerState`.
pub fn apply_on_sandbox(tx: &TxFields, sb: &mut Sandbox) -> (TxResult, bool) {
    let entry = sb.snapshot();
    let Some(transactor) = get_transactor(&tx.tx_type) else {
        // An unsupported type is applied as its common changes only.
        let common = apply_common(tx, sb);
        if common.is_success() {
            return (TxResult::Unsupported, true);
        }
        sb.restore_snapshot(entry);
        return (common, false);
    };
    if is_pseudo(&tx.tx_type) {
        let pf = transactor.preflight(tx);
        if !pf.is_success() {
            return (pf, false);
        }
        let applied = transactor.do_apply(tx, sb);
        if applied.is_success() {
            return (TxResult::Success, true);
        }
        sb.restore_snapshot(entry);
        return (applied, false);
    }
    let preflight = transactor.preflight(tx);
    if !preflight.is_success() {
        if preflight.is_claimed() {
            let common = apply_common(tx, sb);
            if common.is_success() {
                return (preflight, true);
            }
            sb.restore_snapshot(entry);
            return (common, false);
        }
        return (preflight, false);
    }
    let preclaim = transactor.preclaim(tx, sb);
    if !preclaim.is_success() && !preclaim.is_claimed() {
        return (preclaim, false);
    }
    if !preclaim.is_success() {
        let common = apply_common(tx, sb);
        if common.is_success() {
            return (preclaim, true);
        }
        sb.restore_snapshot(entry);
        return (common, false);
    }
    let common = apply_common(tx, sb);
    if !common.is_success() {
        sb.restore_snapshot(entry);
        return (common, false);
    }
    let post_common = sb.snapshot();
    let txn_id_armed = account_txn_id_armed(tx, sb);
    let applied = transactor.do_apply(tx, sb);
    if applied.is_success() {
        stamp_account_txn_id(tx, sb, txn_id_armed);
        (TxResult::Success, true)
    } else if applied.is_claimed() {
        if applied == TxResult::Expired {
            crate::ledger::apply::settle_expired(sb, post_common);
        } else if applied != TxResult::Killed {
            sb.restore_snapshot(post_common);
        }
        (applied, true)
    } else {
        sb.restore_snapshot(entry);
        (applied, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_implemented_types() {
        for ty in [
            "Payment", "OfferCreate", "OfferCancel",
            "TrustSet", "AccountSet", "AccountDelete",
            "EscrowCreate", "EscrowFinish", "EscrowCancel",
            "CheckCreate", "CheckCash", "CheckCancel",
            "PaymentChannelCreate", "PaymentChannelClaim", "PaymentChannelFund",
            "NFTokenMint", "NFTokenBurn", "NFTokenCreateOffer",
            "NFTokenAcceptOffer", "NFTokenCancelOffer",
            "SetRegularKey", "SignerListSet", "DepositPreauth", "Clawback",
            "CredentialCreate", "CredentialDelete", "CredentialAccept",
            "AMMCreate", "AMMDeposit", "AMMWithdraw", "AMMVote", "AMMBid", "AMMDelete",
            "AMMClawback",
            "TicketCreate",
            "OracleSet", "OracleDelete",
            "DIDSet", "DIDDelete",
            "XChainCreateBridge", "XChainCreateClaimID", "XChainCommit",
            "XChainClaim", "XChainModifyBridge", "XChainAccountCreateCommit",
            "XChainAddClaimAttestation", "XChainAddAccountCreateAttestation",
            "PermissionedDomainSet", "PermissionedDomainDelete",
            "MPTokenIssuanceCreate", "MPTokenIssuanceDestroy",
            "MPTokenIssuanceSet", "MPTokenAuthorize",
        ] {
            assert!(is_supported(ty), "{} should be supported", ty);
        }
    }

    #[test]
    fn unknown_is_unsupported() {
        assert!(!is_supported("SomeFutureTxType"));
        assert!(!is_supported("SomeOtherFuture"));
    }
}

#[cfg(test)]
mod apply_on_sandbox_tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::keylet;
    use crate::ledger::sandbox::Sandbox;
    use crate::ledger::state::LedgerState;
    use crate::ledger::transactor::TxFields;
    use xrpl_core::types::Hash256;

    fn state_with_accounts(accts: &[([u8; 20], u64, u32)]) -> LedgerState {
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
        for (id, balance, seq) in accts {
            let acct_json = serde_json::json!({
                "LedgerEntryType": "AccountRoot",
                "Account": hex::encode(id),
                "Balance": balance.to_string(),
                "Sequence": seq,
                "OwnerCount": 0,
                "Flags": 0,
            });
            state.state_map.insert(keylet::account_root_key(id), serde_json::to_vec(&acct_json).unwrap()).unwrap();
        }
        state
    }

    fn acct(n: u8) -> [u8; 20] { let mut a = [0u8; 20]; a[19] = n; a }

    fn payment(from: [u8; 20], to: [u8; 20], drops: u64, fee: u64, seq: u32, inner: bool) -> TxFields {
        let tx = serde_json::json!({
            "TransactionType": "Payment",
            "Account": hex::encode(from),
            "Destination": hex::encode(to),
            "Amount": drops.to_string(),
            "Fee": fee.to_string(),
            "Sequence": seq,
            "Flags": if inner { 0x4000_0000u64 } else { 0 },
        });
        let mut f = TxFields::from_json(&tx).expect("fields");
        f.inner_batch = inner;
        f
    }

    fn balance_of(sb: &Sandbox, id: &[u8; 20]) -> (u64, u32) {
        let v: serde_json::Value = serde_json::from_slice(&sb.read(&keylet::account_root_key(id)).unwrap()).unwrap();
        (v["Balance"].as_str().unwrap().parse().unwrap(), v["Sequence"].as_u64().unwrap() as u32)
    }

    #[test]
    fn a_standalone_payment_moves_xrp_charges_the_fee_and_bumps_the_sequence() {
        let state = state_with_accounts(&[(acct(1), 50_000_000, 5), (acct(2), 20_000_000, 1)]);
        let mut sb = Sandbox::new(&state);
        let (r, applied) = apply_on_sandbox(&payment(acct(1), acct(2), 1_000_000, 12, 5, false), &mut sb);
        assert_eq!(r.code_str(), "tesSUCCESS");
        assert!(applied);
        assert_eq!(balance_of(&sb, &acct(1)), (50_000_000 - 1_000_000 - 12, 6));
        assert_eq!(balance_of(&sb, &acct(2)), (21_000_000, 1));
    }

    #[test]
    fn a_batch_inner_pays_no_fee_but_consumes_its_sequence() {
        let state = state_with_accounts(&[(acct(1), 50_000_000, 5), (acct(2), 20_000_000, 1)]);
        let mut sb = Sandbox::new(&state);
        let (r, applied) = apply_on_sandbox(&payment(acct(1), acct(2), 1_000_000, 0, 5, true), &mut sb);
        assert_eq!(r.code_str(), "tesSUCCESS");
        assert!(applied);
        assert_eq!(balance_of(&sb, &acct(1)), (49_000_000, 6));
    }

    #[test]
    fn a_failed_inner_leaves_the_sandbox_untouched() {
        let state = state_with_accounts(&[(acct(1), 50_000_000, 5), (acct(2), 20_000_000, 1)]);
        let mut sb = Sandbox::new(&state);
        // Wrong sequence: preclaim rejects (tefPAST_SEQ / temBAD_SEQUENCE), not applied.
        let before = sb.snapshot();
        let (r, applied) = apply_on_sandbox(&payment(acct(1), acct(2), 1_000_000, 0, 99, true), &mut sb);
        assert!(!applied, "{}", r.code_str());
        assert_eq!(sb.snapshot().len(), before.len(), "no entries written");
    }

    #[test]
    fn a_tec_inner_is_applied_with_only_its_sequence_consumed() {
        // 1 XRP balance, 0.2 XRP reserve rule aside: sending more than held is tecUNFUNDED_PAYMENT.
        let state = state_with_accounts(&[(acct(1), 1_000_000, 5), (acct(2), 20_000_000, 1)]);
        let mut sb = Sandbox::new(&state);
        let (r, applied) = apply_on_sandbox(&payment(acct(1), acct(2), 900_000_000, 0, 5, true), &mut sb);
        assert!(r.code_str().starts_with("tec"), "{}", r.code_str());
        assert!(applied, "a tec is claimed: applied with the common changes only");
        assert_eq!(balance_of(&sb, &acct(1)), (1_000_000, 6), "fee 0, sequence consumed, nothing moved");
        assert_eq!(balance_of(&sb, &acct(2)), (20_000_000, 1));
    }

    /// tfPartialPayment (0x0002_0000) skips Payment::preclaim's pure-XRP
    /// funding guard (payment.rs:2059-2078 — `pure_xrp && !partial`), so an
    /// unfunded direct-XRP send fails inside do_apply instead
    /// (payment.rs's `sender_balance < amount => TxResult::UnfundedPayment`,
    /// just after the cross-currency/IOU dispatch). That's the branch of
    /// apply_on_sandbox that restores to `post_common` rather than `entry` —
    /// this test is the one that actually exercises it (tests 3 and 4 both
    /// return from the preflight/preclaim stage, before do_apply runs).
    #[test]
    fn a_do_apply_tec_inner_is_applied_with_only_its_sequence_consumed() {
        let state = state_with_accounts(&[(acct(1), 1_000_000, 5), (acct(2), 20_000_000, 1)]);
        let mut sb = Sandbox::new(&state);
        let mut tx = payment(acct(1), acct(2), 900_000_000, 0, 5, true);
        // tfPartialPayment | tfInnerBatchTxn
        tx.fields["Flags"] = serde_json::json!(0x4002_0000u64);
        let (r, applied) = apply_on_sandbox(&tx, &mut sb);
        assert!(r.code_str().starts_with("tec"), "{}", r.code_str());
        assert!(applied, "a tec is claimed: applied with the common changes only");
        assert_eq!(balance_of(&sb, &acct(1)), (1_000_000, 6), "fee 0, sequence consumed, nothing moved");
        assert_eq!(balance_of(&sb, &acct(2)), (20_000_000, 1));
    }
}
