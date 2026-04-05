//! Transaction type dispatcher.
//!
//! Routes decoded transactions to the correct Transactor implementation
//! based on TransactionType string.

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
use super::misc::{
    AMMClawbackTransactor, ClawbackTransactor, DIDDeleteTransactor, DIDSetTransactor,
    DepositPreauthTransactor, MPTokenAuthorizeTransactor, MPTokenIssuanceCreateTransactor,
    MPTokenIssuanceDestroyTransactor, MPTokenIssuanceSetTransactor, OracleDeleteTransactor,
    OracleSetTransactor, PermissionedDomainDeleteTransactor, PermissionedDomainSetTransactor,
    SetRegularKeyTransactor, SignerListSetTransactor, TicketCreateTransactor,
    XChainAccountCreateCommitTransactor, XChainAddAccountCreateAttestationTransactor,
    XChainAddClaimAttestationTransactor, XChainClaimTransactor, XChainCommitTransactor,
    XChainCreateBridgeTransactor, XChainCreateClaimIDTransactor, XChainModifyBridgeTransactor,
};
use super::nftoken::{
    NFTokenAcceptOfferTransactor, NFTokenBurnTransactor, NFTokenCancelOfferTransactor,
    NFTokenCreateOfferTransactor, NFTokenMintTransactor,
};
use super::offer::{OfferCancelTransactor, OfferCreateTransactor};
use super::pay_channel::{
    PaymentChannelClaimTransactor, PaymentChannelCreateTransactor, PaymentChannelFundTransactor,
};
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
