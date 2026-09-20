//! Transactor — common transaction application pattern.
//!
//! Every transaction type implements the Transactor trait:
//! - `preflight`: format validation (no state access)
//! - `preclaim`: read-only state checks
//! - `do_apply`: modify state in sandbox
//!
//! Common logic (fee deduction, sequence increment) runs for ALL types.
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
    /// tecBAD_CREDENTIALS — a CredentialID that is missing, not the sender's, or not accepted (finding 286).
    BadCredentials,
    NoIssuer,
    /// Object not found.
    NoEntry,
    /// The named target object does not exist (PaymentChannelClaim's missing
    /// channel is tecNO_TARGET where PaymentChannelFund's is tecNO_ENTRY —
    /// rippled keeps the two codes distinct per transactor).
    NoTarget,
    /// IoC/FoK offer crossed nothing (or FoK not fully filled).
    Killed,
    /// tecINVARIANT_FAILED — an AMM offer whose pool product would fall (fixAMMOverflowOffer).
    InvariantFailed,
    /// temBAD_PATH
    BadPath,
    /// terNO_RIPPLE
    NoRipple,
    /// Placement would exceed the owner reserve.
    InsufReserveOffer,
    /// Offer is unfunded at apply time.
    UnfundedOffer,
    /// The transaction carries an `Expiration` that the parent close time has
    /// already reached. rippled: `hasExpired(view, exp)` is
    /// `parentCloseTime() >= exp` (View.cpp:48-54).
    Expired,
    /// AccountDelete: the account's Sequence is too recent —
    /// `sequence + 255 > view.seq()` (AccountDelete.cpp kSeqDelta).
    TooSoon,
    /// Path delivered something but less than required (no partial flag).
    PathPartial,
    /// Destination requires a DestinationTag and the tx has none.
    DstTagNeeded,
    /// Resulting array exceeds its ledger-object maximum (e.g. an oracle's
    /// merged PriceDataSeries over ten entries).
    ArrayTooLarge,
    /// The object this transaction would create already exists.
    Duplicate,
    /// A referenced object does not exist (NFT offers use this rather than
    /// the older tecNO_ENTRY).
    ObjectNotFound,
    /// NFTokenAcceptOffer: the two brokered offers belong to one account (a
    /// loop), or the acceptor placed the offer it is accepting
    /// (NFTokenAcceptOffer.cpp:105-108, :165-167, :224-226).
    CantAcceptOwnNftOffer,
    /// The account that must pay for an NFT offer cannot cover it.
    /// `NFTokenAcceptOffer::preclaim` compares `accountFunds(offer owner)`
    /// against the offer's own Amount.
    InsufficientFunds,
    /// Brokered NFT accept: the buy offer does not cover the sell offer's ask
    /// (or does not once the broker's fee is taken) — tecINSUFFICIENT_PAYMENT.
    InsufficientPayment,
    /// Brokered NFT accept: the two offers name different tokens or assets,
    /// or the broker fee is in another asset — tecNFTOKEN_BUY_SELL_MISMATCH.
    NftokenBuySellMismatch,
    /// An AMM pool cannot cover the requested amount, or the account holds no
    /// LP tokens in it.
    AmmBalance,
    /// The account still owns something it cannot abandon, so it cannot be
    /// deleted. `AccountDelete::preclaim` walks the owner DIRECTORY and refuses
    /// on any entry whose type has no `nonObligationDeleter` — an Escrow, a
    /// PayChannel, a Check, a trust line, an NFT page. NOT an OwnerCount test:
    /// an escrow named to this account as DESTINATION sits in its directory
    /// while leaving OwnerCount at zero.
    HasObligations,
    /// The LP tokens offered exceed what the account actually holds.
    /// `AMMWithdraw::preclaim` splits this from `tecAMM_BALANCE`: holding NONE
    /// is a balance failure, holding SOME BUT TOO FEW is an invalid-tokens one.
    AmmInvalidTokens,
    /// The pool's LPTokenBalance is zero — an emptied AMM awaiting deletion
    /// refuses votes/deposits-without-tfTwoAssetIfEmpty with tecAMM_EMPTY.
    AmmEmpty,
    /// tecAMM_NOT_EMPTY — AMMDelete on a pool whose LPTokenBalance is not zero (finding 347).
    AmmNotEmpty,
    /// terNO_AMM — an AMM transaction naming a pool that does not exist (retry class, never in a ledger).
    NoAmm,
    /// OracleSet's LastUpdateTime is below the ripple epoch, outside the
    /// ±300s window around the last close, or not newer than the stored one.
    InvalidUpdateTime,
    /// The sender has no trust line at all for the IOU it is escrowing
    /// (escrowCreatePreclaimHelper<Issue>) — tecNO_LINE.
    NoLine,
    /// The issuer froze the sender's or destination's line (or everything) —
    /// tecFROZEN.
    Frozen,
    /// Directory is full (e.g. > 250 outstanding tickets).
    DirFull,
    /// Creating the owned object(s) would breach the account's owner reserve.
    InsufficientReserve,
    /// Modifying a trust line into a reserved state, but the owner can't afford
    /// the incremental reserve.
    InsufReserveLine,
    /// Depositing into an AMM more than the depositor can actually fund.
    UnfundedAmm,
    /// An AMM operation whose amounts cannot satisfy the pool's constraints —
    /// notably a two-asset deposit where NEITHER side's proportional partner
    /// fits inside what the transaction offered.
    AmmFailed,
    /// An AMM withdrawal that would leave sqrt(pool1·pool2) below the LPToken
    /// balance beyond the invariant's tolerance — tecPRECISION_LOSS
    /// (fixCleanup3_3_0 with fixAMMv1_3, finding 259).
    PrecisionLoss,
    /// Creating a trust line, but the owner can't afford the incremental reserve.
    NoLineInsufReserve,
    /// The credit would push the receiver's trust line past its limit.
    LimitExceeded,
    /// Setting a non-existent trust line to defaults — nothing to do.
    NoLineRedundant,
    /// The party lacks authorization for the asset — an MPT holder without an
    /// MPToken (or unauthorized under lsfMPTRequireAuth), or a third-party
    /// transfer of an MPT without lsfMPTCanTransfer.
    NoAuth,
    /// The MPT is locked — globally (issuance lsfMPTLocked) or individually
    /// (either holder's MPToken lsfMPTLocked) — for a holder→holder payment.
    Locked,
    /// A DIDSet whose result would carry none of URI/DIDDocument/Data.
    EmptyDid,
    /// XChain: the tx account is the bridge door itself.
    XChainSelfCommit,
    /// XChain: the committed asset is not the bridge's chain-side issue.
    XChainBadTransferIssue,
    /// XChain: SignatureReward differs from the bridge's.
    XChainRewardMismatch,
    /// XChain: the referenced claim id does not exist.
    XChainNoClaimId,
    /// XChain: no submitted attestation is signed by a door signer.
    XChainProofUnknownKey,
    /// XChain: attestation's sending account differs from the claim's.
    XChainSendingAccountMismatch,
    /// XChain: attestation names the wrong destination chain.
    XChainWrongChain,
    /// XChain: an explicit claim without an attested quorum.
    XChainClaimNoQuorum,
    /// XChain: the door account has no signer list to attest against.
    XChainNoSignersList,
    /// Clearing the RegularKey with the master key disabled and no signer
    /// list to fall back on (tecNO_ALTERNATIVE_KEY, SetRegularKey.cpp:83).
    NoAlternativeKey,
    /// tecNEED_MASTER_KEY — asfDisableMaster / asfNoFreeze not signed with the master key.
    NeedMasterKey,
    /// Turning on RequireAuth while the account already owns objects
    /// (tecOWNERS, SetAccount.cpp preclaim: `!dirIsEmpty(ownerDir)`).
    Owners,

    // tem — malformed, not applied at all
    /// Transaction is malformed.
    Malformed,
    /// Invalid fee.
    BadFee,
    /// Invalid amount.
    BadAmount,
    /// Invalid sequence.
    BadSequence,
    /// temINVALID_FLAG — Batch: not exactly one mode flag, or tfInnerBatchTxn on the outer.
    InvalidFlag,
    /// temREDUNDANT — Batch: duplicate inner, or duplicate (account, sequence) under AllOrNothing/UntilFailure;
    /// Payment: destination is the sender, same asset both ends, no Paths (Payment.cpp:171).
    Redundant,
    /// temBAD_SEND_XRP_MAX — XRP-to-XRP payment carrying SendMax (finding 299).
    BadSendXrpMax,
    /// temBAD_SEND_XRP_PATHS — XRP-to-XRP (or MPT) payment carrying Paths.
    BadSendXrpPaths,
    /// temBAD_SEND_XRP_PARTIAL — XRP-to-XRP payment with tfPartialPayment.
    BadSendXrpPartial,
    /// temBAD_SEND_XRP_LIMIT — XRP-to-XRP (or MPT) payment with tfLimitQuality.
    BadSendXrpLimit,
    /// temBAD_SEND_XRP_NO_DIRECT — XRP-to-XRP (or MPT) payment with tfNoRippleDirect.
    BadSendXrpNoDirect,
    /// temRIPPLE_EMPTY — tfNoRippleDirect with no Paths: no strand can exist (PaySteps.cpp:542, finding 300).
    RippleEmpty,
    /// temBAD_CURRENCY — the reserved "XRP" currency code on an IOU amount.
    BadCurrency,
    /// temDST_NEEDED — Payment without a Destination.
    DstNeeded,
    /// temBAD_SIGNER — Batch: BatchSigners not sorted/unique, missing or
    /// spurious signer, a signer that is the outer account; also an inner
    /// carrying a Signers field.
    BadSigner,
    /// temINVALID_INNER_BATCH — Batch: an inner of a disabled/pseudo type,
    /// or an inner that otherwise fails its own preflight (rippled's
    /// `xrpl::preflight(..., TapBatch, ...)` call on the inner).
    InvalidInnerBatch,
    /// temARRAY_EMPTY — Batch: RawTransactions absent or empty.
    ArrayEmpty,
    /// temARRAY_TOO_LARGE — Batch: more than 8 inners or signers (the tem code; `ArrayTooLarge` is the tec).
    TemArrayTooLarge,
    /// temSEQ_AND_TICKET — an inner with both or neither of Sequence / TicketSequence.
    SeqAndTicket,
    /// temBAD_SIGNATURE — an inner carrying TxnSignature.
    BadSignature,
    /// temBAD_REGKEY — an inner carrying a non-empty SigningPubKey.
    BadRegKey,
    /// temBAD_TRANSFER_RATE — AccountSet TransferRate outside [1e9, 2e9] (0 clears).
    BadTransferRate,
    /// temBAD_EXPIRATION — EscrowCreate without any timeout, or CancelAfter <= FinishAfter.
    BadExpiration,
    /// temBAD_QUORUM — SignerListSet quorum of zero or beyond the weights' sum.
    BadQuorum,
    /// temINVALID — an inner of a disallowed type (Batch inside Batch, pseudo types).
    InvalidTx,

    // tef — failed, not applied
    /// Sequence already past.
    PastSeq,
    /// LastLedgerSequence exceeded.
    MaxLedger,
    /// tefNO_TICKET — the TicketSequence names a ticket that was already used.
    NoTicket,
    /// tefWRONG_PRIOR — AccountTxnID does not match the account's.
    WrongPrior,
    /// tefNO_AUTH_REQUIRED — tfSetfAuth on an account without lsfRequireAuth.
    NoAuthRequired,
    // ter — retry, not applied
    /// terPRE_SEQ — a future Sequence.
    PreSeq,
    /// terPRE_TICKET — a TicketSequence not yet created.
    PreTicket,
    /// Account not found.
    NoAccount,
    /// Pseudo-transaction internal failure (tefFAILURE).
    Failure,

    // tel — local failure, never in a validated ledger
    /// The payment driver hit its safety bound of 1000 iterations
    /// (telFAILED_PROCESSING, StrandFlow.h:655).
    FailedProcessing,

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
            | TxResult::BadCredentials
            | TxResult::NoIssuer
            | TxResult::NoEntry
            | TxResult::NoTarget
            | TxResult::Killed
            | TxResult::InsufReserveOffer
            | TxResult::UnfundedOffer
            | TxResult::Expired
            | TxResult::TooSoon
            | TxResult::PathPartial
            | TxResult::DstTagNeeded
            | TxResult::ArrayTooLarge
            | TxResult::Duplicate
            | TxResult::ObjectNotFound
            | TxResult::CantAcceptOwnNftOffer
            | TxResult::InsufficientFunds
            | TxResult::InsufficientPayment
            | TxResult::NftokenBuySellMismatch
            | TxResult::AmmBalance
            | TxResult::AmmInvalidTokens
            | TxResult::AmmEmpty
            | TxResult::AmmNotEmpty
            | TxResult::InvalidUpdateTime
            | TxResult::NoLine
            | TxResult::Frozen
            | TxResult::LimitExceeded
            | TxResult::HasObligations
            | TxResult::DirFull
            | TxResult::InsufficientReserve
            | TxResult::InsufReserveLine
            | TxResult::UnfundedAmm
            | TxResult::AmmFailed
                | TxResult::PrecisionLoss
            | TxResult::NoLineInsufReserve
            | TxResult::NoLineRedundant
            | TxResult::NoAuth
            | TxResult::Locked
            | TxResult::EmptyDid
            | TxResult::XChainSelfCommit
            | TxResult::XChainBadTransferIssue
            | TxResult::XChainRewardMismatch
            | TxResult::XChainNoClaimId
            | TxResult::XChainProofUnknownKey
            | TxResult::XChainSendingAccountMismatch
            | TxResult::XChainWrongChain
            | TxResult::XChainClaimNoQuorum
            | TxResult::XChainNoSignersList
            | TxResult::NoAlternativeKey
            | TxResult::NeedMasterKey
            | TxResult::Owners
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
            TxResult::BadCredentials => "tecBAD_CREDENTIALS",
            TxResult::TooSoon => "tecTOO_SOON",
            TxResult::NoIssuer => "tecNO_ISSUER",
            TxResult::NoEntry => "tecNO_ENTRY",
            TxResult::NoTarget => "tecNO_TARGET",
            TxResult::Killed => "tecKILLED",
            TxResult::InvariantFailed => "tecINVARIANT_FAILED",
            TxResult::BadPath => "temBAD_PATH",
            TxResult::NoRipple => "terNO_RIPPLE",
            TxResult::RippleEmpty => "temRIPPLE_EMPTY",
            TxResult::InsufReserveOffer => "tecINSUF_RESERVE_OFFER",
            TxResult::UnfundedOffer => "tecUNFUNDED_OFFER",
            TxResult::Expired => "tecEXPIRED",
            TxResult::PathPartial => "tecPATH_PARTIAL",
            TxResult::DstTagNeeded => "tecDST_TAG_NEEDED",
            TxResult::ArrayTooLarge => "tecARRAY_TOO_LARGE",
            TxResult::Duplicate => "tecDUPLICATE",
            TxResult::ObjectNotFound => "tecOBJECT_NOT_FOUND",
            TxResult::CantAcceptOwnNftOffer => "tecCANT_ACCEPT_OWN_NFTOKEN_OFFER",
            TxResult::InsufficientFunds => "tecINSUFFICIENT_FUNDS",
            TxResult::InsufficientPayment => "tecINSUFFICIENT_PAYMENT",
            TxResult::NftokenBuySellMismatch => "tecNFTOKEN_BUY_SELL_MISMATCH",
            TxResult::AmmBalance => "tecAMM_BALANCE",
            TxResult::AmmInvalidTokens => "tecAMM_INVALID_TOKENS",
            TxResult::AmmEmpty => "tecAMM_EMPTY",
            TxResult::AmmNotEmpty => "tecAMM_NOT_EMPTY",
            TxResult::NoAmm => "terNO_AMM",
            TxResult::InvalidUpdateTime => "tecINVALID_UPDATE_TIME",
            TxResult::NoLine => "tecNO_LINE",
            TxResult::Frozen => "tecFROZEN",
            TxResult::HasObligations => "tecHAS_OBLIGATIONS",
            TxResult::DirFull => "tecDIR_FULL",
            TxResult::InsufficientReserve => "tecINSUFFICIENT_RESERVE",
            TxResult::InsufReserveLine => "tecINSUF_RESERVE_LINE",
            TxResult::UnfundedAmm => "tecUNFUNDED_AMM",
            TxResult::AmmFailed => "tecAMM_FAILED",
            TxResult::PrecisionLoss => "tecPRECISION_LOSS",
            TxResult::NoLineInsufReserve => "tecNO_LINE_INSUF_RESERVE",
            TxResult::LimitExceeded => "tecLIMIT_EXCEEDED",
            TxResult::NoLineRedundant => "tecNO_LINE_REDUNDANT",
            TxResult::NoAuth => "tecNO_AUTH",
            TxResult::Locked => "tecLOCKED",
            TxResult::EmptyDid => "tecEMPTY_DID",
            TxResult::XChainSelfCommit => "tecXCHAIN_SELF_COMMIT",
            TxResult::XChainBadTransferIssue => "tecXCHAIN_BAD_TRANSFER_ISSUE",
            TxResult::XChainRewardMismatch => "tecXCHAIN_REWARD_MISMATCH",
            TxResult::XChainNoClaimId => "tecXCHAIN_NO_CLAIM_ID",
            TxResult::XChainProofUnknownKey => "tecXCHAIN_PROOF_UNKNOWN_KEY",
            TxResult::XChainSendingAccountMismatch => "tecXCHAIN_SENDING_ACCOUNT_MISMATCH",
            TxResult::XChainWrongChain => "tecXCHAIN_WRONG_CHAIN",
            TxResult::XChainClaimNoQuorum => "tecXCHAIN_CLAIM_NO_QUORUM",
            TxResult::XChainNoSignersList => "tecXCHAIN_NO_SIGNERS_LIST",
            TxResult::NoAlternativeKey => "tecNO_ALTERNATIVE_KEY",
            TxResult::NeedMasterKey => "tecNEED_MASTER_KEY",
            TxResult::Owners => "tecOWNERS",
            TxResult::Malformed => "temMALFORMED",
            TxResult::BadFee => "temBAD_FEE",
            TxResult::BadAmount => "temBAD_AMOUNT",
            TxResult::BadSequence => "temBAD_SEQUENCE",
            TxResult::BadTransferRate => "temBAD_TRANSFER_RATE",
            TxResult::BadExpiration => "temBAD_EXPIRATION",
            TxResult::BadQuorum => "temBAD_QUORUM",
            TxResult::InvalidFlag => "temINVALID_FLAG",
            TxResult::Redundant => "temREDUNDANT",
            TxResult::BadSendXrpMax => "temBAD_SEND_XRP_MAX",
            TxResult::BadSendXrpPaths => "temBAD_SEND_XRP_PATHS",
            TxResult::BadSendXrpPartial => "temBAD_SEND_XRP_PARTIAL",
            TxResult::BadSendXrpLimit => "temBAD_SEND_XRP_LIMIT",
            TxResult::BadSendXrpNoDirect => "temBAD_SEND_XRP_NO_DIRECT",
            TxResult::BadCurrency => "temBAD_CURRENCY",
            TxResult::DstNeeded => "temDST_NEEDED",
            TxResult::BadSigner => "temBAD_SIGNER",
            TxResult::InvalidInnerBatch => "temINVALID_INNER_BATCH",
            TxResult::ArrayEmpty => "temARRAY_EMPTY",
            TxResult::TemArrayTooLarge => "temARRAY_TOO_LARGE",
            TxResult::SeqAndTicket => "temSEQ_AND_TICKET",
            TxResult::BadSignature => "temBAD_SIGNATURE",
            TxResult::BadRegKey => "temBAD_REGKEY",
            TxResult::InvalidTx => "temINVALID",
            TxResult::PastSeq => "tefPAST_SEQ",
            TxResult::MaxLedger => "tefMAX_LEDGER",
            TxResult::NoTicket => "tefNO_TICKET",
            TxResult::WrongPrior => "tefWRONG_PRIOR",
            TxResult::NoAuthRequired => "tefNO_AUTH_REQUIRED",
            TxResult::PreSeq => "terPRE_SEQ",
            TxResult::PreTicket => "terPRE_TICKET",
            TxResult::NoAccount => "tefNO_ACCOUNT",
            TxResult::Failure => "tefFAILURE",
            TxResult::FailedProcessing => "telFAILED_PROCESSING",
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
    /// Set by `BatchTransactor` for the inner transactions it applies
    /// (rippled's `tapBATCH`): the inner carries `Fee: "0"` by rule, so
    /// the fee-zero gate every transactor enforces is waived, and no
    /// signature is expected.
    pub inner_batch: bool,
}

impl TxFields {
    /// Whether this transaction uses a Ticket instead of a regular Sequence.
    pub fn uses_ticket(&self) -> bool {
        self.sequence == 0 && self.ticket_seq.is_some()
    }

    /// The preflight fee gate: a standalone transaction with `Fee: "0"`
    /// is malformed (`temBAD_FEE`); a batch inner carries `Fee: "0"` by
    /// rule (rippled preflight1 under `tapBATCH`).
    pub fn fee_missing(&self) -> bool {
        // Finding 313 (fuzz fee:zero, 31 mutants on 107060755): rippled's
        // preflight1 rejects only a non-native or NEGATIVE fee (temBAD_FEE);
        // the fee LEVEL is judged in checkFee only while the ledger is open
        // (telINSUF_FEE_P), never for a closed-ledger application. `Fee: "0"`
        // applies — libxrpl returned tesSUCCESS where we said temBAD_FEE.
        let _ = self;
        false
    }

    /// The common-field reader that used to live in
    /// `xrpl_node::native_apply::build_txfields`. Accepts the engine's hex
    /// account dialect (via `tx::offer::decode20`, which tries hex first and
    /// falls back to base58) and the pseudo-transaction zero account.
    pub fn from_json(txjson: &serde_json::Value) -> Option<TxFields> {
        // Pseudo-transactions carry Account: "" and Fee: "0" — the zero account.
        let account = match txjson["Account"].as_str()? {
            "" => [0u8; 20],
            a => crate::tx::offer::decode20(a)?,
        };
        let tx_type = txjson["TransactionType"].as_str()?.to_string();
        let fee = txjson["Fee"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
        let sequence = txjson["Sequence"].as_u64().unwrap_or(0) as u32;
        let ticket_seq = txjson.get("TicketSequence").and_then(|v| v.as_u64()).map(|v| v as u32);
        let last_ledger_seq = txjson.get("LastLedgerSequence").and_then(|v| v.as_u64()).map(|v| v as u32);
        let mut fields = txjson.clone();
        for k in ACCOUNT_FIELDS {
            if let Some(a) = fields.get(*k).and_then(|v| v.as_str()) {
                if a.starts_with('r') {
                    if let Some(id) = crate::tx::offer::decode20(a) {
                        fields[*k] = serde_json::json!(hex::encode(id));
                    }
                }
            }
        }
        Some(TxFields {
            account,
            tx_type,
            fee,
            sequence,
            ticket_seq,
            last_ledger_seq,
            fields,
            inner_batch: false,
        })
    }
}

/// Account-bearing fields (other than `Account` itself) that get rewritten
/// from base58 to hex by `TxFields::from_json` — mirrors
/// `differential_probe`'s list of the same name, which hex-normalises a
/// fixture's transaction JSON before the engine ever sees it.
const ACCOUNT_FIELDS: &[&str] = &["Destination", "Owner", "Authorize", "Unauthorize", "RegularKey"];

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
/// Transactor.cpp:660 — an account carrying sfAccountTxnID (armed via
/// asfAccountTxnID) gets it rewritten to the CURRENT tx's hash — but the
/// stamp sits in `apply()` and a tec result ROLLS THE VIEW BACK to fee +
/// sequence only, discarding it. Call this from the SUCCESS arm only.
/// #106455052 BFF40FB0 (full-ledger replay): rapido's two #52 payments are
/// tecPATH_PARTIAL / tecPATH_DRY, and mainnet's final AccountTxnID is
/// still #51's C428A7A6 while PreviousTxnID threads to the tec fee write.
/// (#106455036 E7C799A8 calibrated the success side: the second payment of
/// the ledger leaves ITS hash.)
///
/// `armed_before` is the field's presence BEFORE do_apply ran
/// (`account_txn_id_armed`, read where the pipeline takes its post-common
/// snapshot). rippled's stamp sits in `Transactor::apply` ahead of doApply
/// (Transactor.cpp:906, `if (sle->isFieldPresent(sfAccountTxnID))`), so the
/// AccountSet that ARMS the field — doApply's `makeFieldPresent`, the zero
/// hash (AccountSet.cpp:380-383) — is never itself recorded; the next tx is.
/// Finding 253, #106906126 85C26301282E: rKRQCkc9's SetFlag 5 lands a root
/// with AccountTxnID 0000…0000 on mainnet; stamping after do_apply saw the
/// freshly created field and wrote the arming tx's own hash.
pub fn stamp_account_txn_id(tx: &TxFields, sandbox: &mut Sandbox, armed_before: bool) {
    if !armed_before {
        return;
    }
    let key = keylet::account_root_key(&tx.account);
    let Some(data) = sandbox.read(&key) else { return };
    let Ok(mut acct) = serde_json::from_slice::<serde_json::Value>(&data) else { return };
    // Still present: a ClearFlag 5 in the same tx removed it (AccountSet.cpp:386).
    if acct.get("AccountTxnID").is_none() {
        return;
    }
    let Some(h) = tx.fields.get("hash").and_then(|v| v.as_str()) else { return };
    acct["AccountTxnID"] = serde_json::Value::String(h.to_uppercase());
    if let Ok(bytes) = serde_json::to_vec(&acct) {
        sandbox.write(key, bytes);
    }
}

/// Whether the sender's root carries AccountTxnID right now — read before
/// do_apply, it is what rippled's pre-doApply stamp would have seen (finding
/// 253).
pub fn account_txn_id_armed(tx: &TxFields, sandbox: &Sandbox) -> bool {
    let key = keylet::account_root_key(&tx.account);
    sandbox
        .read(&key)
        .and_then(|data| serde_json::from_slice::<serde_json::Value>(&data).ok())
        .is_some_and(|acct| acct.get("AccountTxnID").is_some())
}

/// The checks every transaction passes between preflight and its own
/// preclaim (rippled Transactor::preclaim → checkSeqProxy,
/// checkPriorTxAndLastLedger), for the ledger `ledger_seq` being built.
/// Finding 312 (fuzz seq:±1 / lls:past, 81 mutants on 107060755): we
/// incremented the account's Sequence without ever comparing it — a future
/// sequence applied (or, in the Payment path, read temBAD_SEQUENCE), a past
/// one applied, a LastLedgerSequence behind the ledger applied.
pub fn preclaim_common(tx: &TxFields, sandbox: &Sandbox, ledger_seq: u32) -> TxResult {
    let acct_key = keylet::account_root_key(&tx.account);
    let Some(data) = sandbox.read(&acct_key) else {
        return TxResult::NoAccount;
    };
    let Ok(acct) = serde_json::from_slice::<serde_json::Value>(&data) else {
        return TxResult::Malformed;
    };
    let acct_seq = acct["Sequence"].as_u64().unwrap_or(0) as u32;
    if !tx.uses_ticket() {
        if tx.sequence != acct_seq {
            return if acct_seq < tx.sequence { TxResult::PreSeq } else { TxResult::PastSeq };
        }
    } else {
        let tseq = tx.ticket_seq.unwrap_or(0);
        if !sandbox.exists(&keylet::ticket_key(&tx.account, tseq)) {
            return if tseq >= acct_seq { TxResult::PreTicket } else { TxResult::NoTicket };
        }
    }
    if let Some(want) = tx.fields.get("AccountTxnID").and_then(|v| v.as_str()) {
        let have = acct.get("AccountTxnID").and_then(|v| v.as_str()).unwrap_or("");
        if !have.eq_ignore_ascii_case(want) {
            return TxResult::WrongPrior;
        }
    }
    if let Some(lls) = tx.last_ledger_seq {
        if ledger_seq > lls {
            return TxResult::MaxLedger;
        }
    }
    TxResult::Success
}

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
    } else {
        // Consuming a Ticket (rippled SeqProxy consumption): delete the Ticket
        // object, unlink it from the owner directory via its OwnerNode hint,
        // and decrement OwnerCount + TicketCount. Mainnet-verified shape
        // (#105663160 ticketed cancels): Ticket Deleted + its dir page
        // Modified + AccountRoot {Balance, OwnerCount, TicketCount}.
        let tk = keylet::ticket_key(&tx.account, tx.ticket_seq.unwrap_or(0));
        if let Some(td) = sandbox.read(&tk) {
            let hint = serde_json::from_slice::<serde_json::Value>(&td)
                .ok()
                .and_then(|t| {
                    t.get("OwnerNode").and_then(|v| {
                        v.as_u64()
                            .or_else(|| v.as_str().and_then(|s| u64::from_str_radix(s, 16).ok()))
                    })
                });
            sandbox.delete(tk);
            crate::ledger::directory::owner_dir_remove(sandbox, &tx.account, &tk, hint, true);
            let oc = acct["OwnerCount"].as_u64().unwrap_or(0);
            acct["OwnerCount"] = serde_json::Value::Number(oc.saturating_sub(1).into());
            // sfTicketCount is soeOPTIONAL: at zero the field is REMOVED,
            // not stored as 0 (byte census: net omits, we kept 2028 00000000).
            let tc = acct["TicketCount"].as_u64().unwrap_or(0).saturating_sub(1);
            if tc == 0 {
                if let Some(o) = acct.as_object_mut() {
                    o.remove("TicketCount");
                }
            } else {
                acct["TicketCount"] = serde_json::Value::Number(tc.into());
            }
        }
    }

    // Write back
    let serialized = serde_json::to_vec(&acct).unwrap_or_default();
    sandbox.write(acct_key, serialized);

    TxResult::Success
}

#[cfg(test)]
mod claim_tests {
    use super::*;

    /// Finding 343 (#107103593 350D8E96B831, a ticketed EscrowFinish into a
    /// line at its limit): every tec claims the fee and consumes the
    /// sequence or ticket; tecLIMIT_EXCEEDED was the one code missing from
    /// the list, so the transaction wrote nothing — no fee, ticket left.
    #[test]
    fn every_tec_code_is_claimed() {
        let all = [
            TxResult::LimitExceeded, TxResult::NoLine, TxResult::Frozen, TxResult::NoPermission,
            TxResult::PathDry, TxResult::PathPartial, TxResult::Killed, TxResult::Expired,
            TxResult::AmmNotEmpty,
        ];
        for r in all {
            assert!(r.code_str().starts_with("tec"), "{:?}", r);
            assert!(r.is_claimed(), "{:?} must claim", r);
        }
    }
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
            inner_batch: false,
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

    #[test]
    fn txfields_from_json_reads_the_common_fields_and_defaults_inner_batch_off() {
        let tx = serde_json::json!({
            "TransactionType": "Payment",
            "Account": "0000000000000000000000000000000000000001",
            "Fee": "12",
            "Sequence": 7,
            "LastLedgerSequence": 99,
            "Amount": "1000000",
        });
        let f = TxFields::from_json(&tx).expect("fields");
        assert_eq!(f.tx_type, "Payment");
        assert_eq!(f.fee, 12);
        assert_eq!(f.sequence, 7);
        assert_eq!(f.ticket_seq, None);
        assert_eq!(f.last_ledger_seq, Some(99));
        assert!(!f.inner_batch);
        assert!(!f.fee_missing());
    }

    #[test]
    fn fee_missing_is_waived_for_a_batch_inner() {
        let tx = serde_json::json!({
            "TransactionType": "Payment",
            "Account": "0000000000000000000000000000000000000001",
            "Fee": "0",
            "Sequence": 7,
        });
        let mut f = TxFields::from_json(&tx).expect("fields");
        assert!(!f.fee_missing(), "finding 313: a standalone zero-fee tx is valid");
        f.inner_batch = true;
        assert!(!f.fee_missing(), "a batch inner carries Fee 0 by rule");
    }

    #[test]
    fn batch_result_codes_have_their_rippled_names() {
        assert_eq!(TxResult::InvalidFlag.code_str(), "temINVALID_FLAG");
        assert_eq!(TxResult::Redundant.code_str(), "temREDUNDANT");
        assert_eq!(TxResult::BadSigner.code_str(), "temBAD_SIGNER");
        assert_eq!(TxResult::InvalidInnerBatch.code_str(), "temINVALID_INNER_BATCH");
        assert_eq!(TxResult::ArrayEmpty.code_str(), "temARRAY_EMPTY");
        assert_eq!(TxResult::TemArrayTooLarge.code_str(), "temARRAY_TOO_LARGE");
        assert_eq!(TxResult::SeqAndTicket.code_str(), "temSEQ_AND_TICKET");
        assert_eq!(TxResult::BadSignature.code_str(), "temBAD_SIGNATURE");
        assert_eq!(TxResult::BadRegKey.code_str(), "temBAD_REGKEY");
        assert_eq!(TxResult::InvalidTx.code_str(), "temINVALID");
        for r in [TxResult::InvalidFlag, TxResult::Redundant, TxResult::BadSigner,
                  TxResult::InvalidInnerBatch, TxResult::ArrayEmpty, TxResult::TemArrayTooLarge,
                  TxResult::SeqAndTicket, TxResult::BadSignature, TxResult::BadRegKey, TxResult::InvalidTx] {
            assert!(!r.is_claimed(), "{:?} is a tem code, never claimed", r);
            assert!(!r.is_success());
        }
    }
}
