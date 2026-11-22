//! The four Loan transactors of the LendingProtocol amendment, ported from
//! rippled 3.4.1 `src/libxrpl/tx/transactors/lending/`:
//!
//! | here | rippled | type |
//! |---|---|---|
//! | [`LoanSetTransactor`] | LoanSet.cpp | 80 |
//! | [`LoanDeleteTransactor`] | LoanDelete.cpp | 81 |
//! | [`LoanManageTransactor`] | LoanManage.cpp | 82 |
//! | [`LoanPayTransactor`] | LoanPay.cpp | 84 |
//!
//! Each is refused temDISABLED unless LendingProtocol is enabled in the
//! ledger (transactions.macro:963-1024) AND SingleAssetVault and MPTokensV1
//! are (`checkLendingProtocolDependencies`, each type's
//! `checkExtraFeatures`): with the amendment off — mainnet today — nothing
//! here runs past that line. None is delegable (`tx::lending_helpers::gate`).
//!
//! # The objects
//!
//! A Loan (ltLOAN, `tx::protocol::LOAN`) is keyed by its LoanBroker and the
//! broker's sfLoanSequence at the time. It sits in TWO owner directories —
//! the broker pseudo-account's (sfLoanBrokerNode) and the borrower's
//! (sfOwnerNode), linked in that order (LoanSet.cpp:713-718) — and costs
//! the BORROWER one reserve unit (:580, LoanDelete.cpp:131). It has no
//! sfAccount and no sfDestination, so creating or deleting it threads no
//! owner (`threadOwners`, ApplyStateTable.cpp:637-667): the borrower's
//! AccountRoot is threaded because its OwnerCount changes, the broker
//! pseudo-account's root is not touched at all. The LoanBroker's
//! sfOwnerCount counts its live Loans (+1 LoanSet, −1 LoanDelete), its
//! sfDebtTotal what they owe the vault, its sfLoanSequence the next loan's
//! number; the Vault's sfAssetsAvailable / sfAssetsTotal / sfLossUnrealized
//! move with every loan event.
//!
//! # Accounting
//!
//! Two models (`tx::loan_math`): accrual for a Legacy vault (interest enters
//! sfAssetsTotal and sfDebtTotal at origination) and cash basis for a vault
//! created under LendingProtocolV1_1 (`sfLEVersion` 1: principal only,
//! interest as it is paid). The vault's version alone decides.
//!
//! # Amendment arms
//!
//! - fixCleanup3_1_3, 3_2_0, 3_3_0, 3_4_0 (all on at mainnet): both arms of
//!   every branch, as 3.4.1 has them;
//! - LendingProtocolV1_1 (off at mainnet, on at devnet): LoanSet's vault
//!   phase checks (LoanSet.cpp:319-344); everything else of V1_1 here is the
//!   vault's sfLEVersion;
//! - fixCleanup3_5_0 (3.5.0-rc1, off at mainnet, on at devnet): LoanSet's
//!   out-of-range value fields refused tecPRECISION_LOSS instead of thrown
//!   (rc1 LoanSet.cpp:385-395) — and, as OUR proxy for "this ledger is made
//!   by 3.5.x servers", the ungated 3.5.0 change to LoanPay's conservation
//!   balances ([`ConservationModel`]);
//! - featureBatchV1_1: LoanSet's inner-batch rule (LoanSet.cpp:75-82). A
//!   Loan type cannot be a Batch inner in 3.4.1 at all
//!   (`tx::batch::DISABLED_INNER_TYPES`), so the rule is unreachable through
//!   Batch; it is ported for a LoanSet that carries tfInnerBatchTxn.
//!
//! NOT ported: LendingProtocolV1_2 (`Supported::No` in 3.5.0-rc1, on no
//! network). It would hook in at every `VaultVersion::FixedPrecision` branch
//! — LoanSet (rc1 LoanSet.cpp:365-373 the coarsened-vault refusal, :590-602
//! the open-zone limit, :753-765 `adjustVaultBalances`), LoanPay and
//! LoanManage (their FixedPrecision arms) — at LoanSet's private-broker
//! domain check (rc1 :491-507, `lsfLoanBrokerPrivate`), and in Batch (rc1
//! Batch.cpp:312-316 stops refusing vault and lending inners). featureSponsor
//! (reserve sponsors: LoanSet.cpp:68-72 `sfSponsorFlags`, the sponsor arms
//! of `increaseOwnerCount` / `accountReserve`) is not ported either, as in
//! the rest of the family. 3.5.0-rc1's Cosign (`TapProposal` skipping the
//! counterparty-signature presence check, rc1 LoanSet.cpp:96) is
//! `Supported::No` and not ported.
//!
//! # Signatures and fees
//!
//! The engine has no signature-authorization layer: it applies transactions
//! of validated or proposed sets and does not run `Transactor::checkSign`
//! for any type (the node's mempool verifies the primary signature's
//! cryptography only). LoanSet's second signature is treated the same way —
//! see [`LoanSetTransactor`] for exactly what is and is not checked. The two
//! custom base fees ([`loan_set_base_fee`], [`loan_pay_base_fee`]) are ported
//! exactly; rippled compares a fee with the base fee only while the ledger
//! is OPEN (`Transactor::checkFee`, Transactor.cpp:536-547,
//! telINSUF_FEE_P), so in a closed ledger they change no result and the fee
//! charged is the transaction's sfFee. What does reach a closed ledger is an
//! exception thrown while LoanPay's fee is computed — see
//! [`loan_pay_base_fee`].
//!
//! # Time
//!
//! "Now" for lateness, expiry and interest is the PARENT ledger's close time
//! (`view.parentCloseTime()`); a Loan's sfStartDate is the close time of the
//! ledger being built as the open view has it, the parent's close time plus
//! this ledger's close-time resolution (`Ledger(prev, closeTime)`,
//! Ledger.cpp:254-279) — [`ledger_close_time`].
//!
//! # Exceptions
//!
//! An `Err` of a helper is a C++ exception leaving the transactor:
//! tefEXCEPTION (applySteps.cpp:485-488), nothing applied.
//!
//! # Not the transactors' job
//!
//! rippled's invariants are not ported; each of the four has empty
//! `visitInvariantEntry` / `finalizeInvariants`. Where rippled's final
//! result would be an invariant's rather than the transactor's:
//!
//! - ValidLoan (LoanInvariant.cpp): a Loan with sfPaymentRemaining 0 must
//!   have zero balances, a changed Loan must keep its immutable fields,
//!   sfLoanScale within range, periodic payment positive…; the transactors
//!   keep these by construction;
//! - ValidVault with `Privilege::MustModifyVault` (LoanSet, LoanPay): both
//!   always change sfAssetsAvailable on success;
//! - TransfersNotFrozen: LoanManage's default moves the cover from the
//!   broker's pseudo-account to the vault's through `accountSend`, which
//!   does not look at freezes. Before fixCleanup3_4_0 a frozen or locked
//!   holding on that path fails the invariant (tecINVARIANT_FAILED);
//!   fixCleanup3_4_0 exempts the default
//!   (`getLoanDefaultFreezeExemptAccounts`, LendingHelpers.cpp:84-116). This
//!   engine returns the transactor's tesSUCCESS in both cases;
//! - ValidLoanBroker: sfCoverAvailable equal to the pseudo-account's
//!   balance. LoanPay adds the fee to the cover and to the pseudo-account
//!   together, LoanManage's default takes both down together.

use crate::flow::st_amount::StError;
use crate::ledger::amendments::{self as am, enabled};
use crate::ledger::directory::{owner_dir_insert, owner_dir_remove};
use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{Transactor, TxFields, TxResult};
use crate::tx::holding;
use crate::tx::lending_helpers::{self as lh, gate};
use crate::tx::loan_math::{self as lm, LoanPaymentType, LoanRules, M};
use crate::tx::mpt::{self, AuthType, HeldAsset};
use crate::tx::number::{Number, Rounding};
use crate::tx::offer::{json_at, put_json};
use crate::tx::protocol::{
    tenth_bips_of_value, LedgerFormat, Soe, VaultPhase, VaultVersion, LOAN, LOAN_BROKER, LOAN_MAXIMUM_PAYMENTS_PER_TRANSACTION,
    LOAN_PAYMENTS_PER_FEE_INCREMENT, LOAN_REDEMPTION_BUFFER, LSF_LOAN_DEFAULT, LSF_LOAN_IMPAIRED, LSF_LOAN_OVERPAYMENT,
    MAX_CLOSE_INTEREST_RATE, MAX_DATA_PAYLOAD_LENGTH, MAX_INTEREST_RATE, MAX_LATE_INTEREST_RATE, MAX_OVERPAYMENT_FEE,
    MAX_OVERPAYMENT_INTEREST_RATE, TF_LOAN_DEFAULT, TF_LOAN_FULL_PAYMENT, TF_LOAN_IMPAIR, TF_LOAN_LATE_PAYMENT, TF_LOAN_MANAGE_MASK,
    TF_LOAN_OVERPAYMENT, TF_LOAN_PAY_MASK, TF_LOAN_SET_MASK, TF_LOAN_UNIMPAIR, TF_UNIVERSAL_MASK, VAULT,
};
use crate::tx::st_number::{associate_asset, number_field, number_from_json, round_to_asset, round_to_scale, set_number_field, Amount};
use crate::tx::vault::{account_of, blob_len, has, is_zero_key, obj_flags, positive, prior_balance, tx_amount, tx_flags, AMBIENT};
use crate::tx::vault_helpers::{self as vh, vault_asset};
use serde_json::Value;
use xrpl_core::types::Hash256;

// ---------------------------------------------------------------------------
// Shared plumbing
// ---------------------------------------------------------------------------

/// `tfInnerBatchTxn` (TxFlags.h).
const TF_INNER_BATCH_TXN: u64 = 0x4000_0000;

/// `LoanSet::kMinPaymentInterval` / `kDefaultPaymentInterval` (LoanSet.h:68-69).
pub const MIN_PAYMENT_INTERVAL: u32 = 60;
pub const DEFAULT_PAYMENT_INTERVAL: u32 = 60;
/// `LoanSet::kDefaultPaymentTotal` (LoanSet.h:65).
pub const DEFAULT_PAYMENT_TOTAL: u32 = 1;
/// `LoanSet::kDefaultGracePeriod` (LoanSet.h:72).
pub const DEFAULT_GRACE_PERIOD: u32 = 60;

/// `LoanSet::getValueFields()` (LoanSet.cpp:210-223): the Number fields of a
/// LoanSet that are amounts of the vault's asset.
const VALUE_FIELDS: [&str; 5] = ["PrincipalRequested", "LoanOriginationFee", "LoanServiceFee", "LatePaymentFee", "ClosePaymentFee"];

fn arith<T>(r: Result<T, crate::tx::number::NumberError>) -> M<T> {
    r.map_err(|_| StError::Overflow)
}

fn is_negative(n: &Number) -> bool {
    n.negative && !n.is_zero()
}

fn uint(obj: &Value, field: &str) -> u64 {
    obj.get(field).and_then(|v| v.as_u64()).unwrap_or(0)
}

/// A directory page hint (sfOwnerNode, sfLoanBrokerNode): a hex string in
/// the engine's JSON.
fn node_hint(obj: &Value, field: &str) -> Option<u64> {
    obj.get(field).and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| u64::from_str_radix(s, 16).ok())))
}

fn read_typed(sandbox: &Sandbox, key: &Hash256, entry_type: &str) -> Option<Value> {
    json_at(sandbox, key).filter(|o| o.get("LedgerEntryType").and_then(|t| t.as_str()) == Some(entry_type))
}

/// `view.read(keylet::loan(id))`.
fn read_loan(sandbox: &Sandbox, id: &Hash256) -> Option<Value> {
    read_typed(sandbox, id, "Loan")
}

/// `view.read(keylet::loanBroker(id))`.
fn read_broker(sandbox: &Sandbox, id: &Hash256) -> Option<Value> {
    read_typed(sandbox, id, "LoanBroker")
}

/// `view.read(keylet::vault(broker->at(sfVaultID)))`, with its key.
fn vault_of(sandbox: &Sandbox, broker: &Value) -> Option<(Hash256, Value)> {
    let key = mpt::key_field(broker, "VaultID")?;
    Some((key, read_typed(sandbox, &key, "Vault")?))
}

fn root_of(sandbox: &Sandbox, account: &[u8; 20]) -> Option<Value> {
    json_at(sandbox, &keylet::account_root_key(account))
}

fn xrp_balance(root: &Value) -> u64 {
    root.get("Balance").and_then(|b| b.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0)
}

/// `tx[sfLoanID]`.
fn loan_id(tx: &TxFields) -> Option<Hash256> {
    mpt::key_field(&tx.fields, "LoanID")
}

/// `tx[~sfField]` for an STNumber.
fn tx_number(tx: &TxFields, field: &str) -> Option<Number> {
    tx.fields.get(field).and_then(number_from_json)
}

/// `tx[~sfField]` for an unsigned integer.
fn tx_uint(tx: &TxFields, field: &str) -> Option<u64> {
    tx.fields.get(field).and_then(|v| v.as_u64())
}

/// `view.parentCloseTime()`: the sandbox's base is the parent ledger, whose
/// close time every expiry test in the engine reads from `header.close_time`.
fn parent_close_time(sandbox: &Sandbox) -> u32 {
    sandbox.base().header.close_time
}

/// `view.header().closeTime` of the ledger being built (LoanSet.cpp:225-229
/// `getStartDate`): while transactions are applied the open view's close
/// time is `parent.closeTime + closeTimeResolution` (Ledger.cpp:262-278; the
/// agreed close time replaces it only when the ledger is accepted). The
/// resolution is the base header's `close_time_resolution`, as
/// `tx::oracle` reads it — the header the apply pipelines hand the engine
/// must carry the resolution of the ledger being BUILT
/// (`ledger::close_time::next_resolution`), or sfStartDate and every due
/// date derived from it are off by the difference.
pub fn ledger_close_time(sandbox: &Sandbox) -> u32 {
    sandbox.base().header.building_close_time()
}

/// `view.update(sle)` — and, when `asset` is given, `associateAsset(*sle,
/// asset)` (STTakesAsset.cpp:13-50) first: the object's asset-valued Number
/// fields rounded to the asset's precision at the thread's mode. SoeDefault
/// fields at their default are dropped and the template checked.
fn store(sandbox: &mut Sandbox, key: Hash256, obj: &mut Value, format: &LedgerFormat, asset: Option<HeldAsset>) -> M<TxResult> {
    if let Some(asset) = asset {
        associate_asset(obj, asset, AMBIENT)?;
    }
    if format.finish(obj).is_err() {
        return Ok(TxResult::TefInternal);
    }
    put_json(sandbox, key, obj);
    Ok(TxResult::Success)
}

/// Run a doApply body: an `Err` is tefEXCEPTION, and anything but
/// tesSUCCESS leaves the sandbox as it was on entry — rippled's apply view
/// is discarded on every failure, and the dispatcher's own rollback keeps a
/// tecKILLED's writes for OfferCreate's sake.
fn applied(sandbox: &mut Sandbox, body: impl FnOnce(&mut Sandbox) -> M<TxResult>) -> TxResult {
    let entry = sandbox.snapshot();
    let result = body(sandbox).unwrap_or(TxResult::TefException);
    if result != TxResult::Success {
        sandbox.restore_snapshot(entry);
    }
    result
}

/// The cover a broker must keep for `debt_total` as LoanSet and LoanPay
/// compute it: `minimumBrokerCover` under fixCleanup3_2_0 (the rate of the
/// debt rounded UP at the vault's scale); before it the unrounded rate of
/// the debt (LoanSet.cpp:559-572) or that rounded UP at the LOAN's scale
/// (LoanPay.cpp:373-391), all under one Upward guard.
fn minimum_cover(sandbox: &Sandbox, debt_total: Number, rate: u32, vault: &Value, pre_fix_scale: Option<(HeldAsset, i32)>) -> M<Number> {
    if am::fix_cleanup_3_2_0(sandbox) {
        return lh::minimum_broker_cover(debt_total, rate, vault);
    }
    let up = Rounding::Upward;
    let raw = arith(tenth_bips_of_value(debt_total, rate, up))?;
    match pre_fix_scale {
        Some((asset, loan_scale)) => round_to_asset(asset, raw, loan_scale, up),
        None => Ok(raw),
    }
}

// ---------------------------------------------------------------------------
// Base fees
// ---------------------------------------------------------------------------

/// `Transactor::calculateBaseFee(view, tx)` (Transactor.cpp:449-474): one
/// base fee, one more per multi-signer, one more per signer of a sponsor's
/// multi-signature.
pub fn transactor_base_fee(tx_fields: &Value, base_fee: u64) -> u64 {
    let array_len = |v: Option<&Value>| v.and_then(|s| s.as_array()).map_or(0, |a| a.len() as u64);
    let signers = array_len(tx_fields.get("Signers"));
    let sponsor_signers = array_len(tx_fields.get("SponsorSignature").and_then(|s| s.get("Signers")));
    base_fee.saturating_add((signers + sponsor_signers).saturating_mul(base_fee))
}

/// `LoanSet::calculateBaseFee` (LoanSet.cpp:183-208): the normal cost plus
/// one base fee per counterparty signer — the number of sfSigners inside
/// sfCounterpartySignature, or 1 for a single sfTxnSignature there, or 0
/// when it carries neither (or is absent: `getFieldObject` of a missing
/// field is an empty object).
pub fn loan_set_base_fee(tx_fields: &Value, base_fee: u64) -> u64 {
    let normal = transactor_base_fee(tx_fields, base_fee);
    let counter = tx_fields.get("CounterpartySignature");
    let signer_count = match counter.and_then(|c| c.get("Signers")) {
        Some(signers) => signers.as_array().map_or(0, |a| a.len() as u64),
        None if counter.is_some_and(|c| c.get("TxnSignature").is_some()) => 1,
        None => 0,
    };
    normal.saturating_add(signer_count.saturating_mul(base_fee))
}

/// `LoanPay::calculateBaseFee` (LoanPay.cpp:112-226): one base fee per
/// kLoanPaymentsPerFeeIncrement (5) scheduled payments the amount could
/// make, at least one.
///
/// The normal cost for: a non-positive amount (fixCleanup3_4_0), a full or
/// late payment, a missing loan / broker / vault, five or fewer payments
/// remaining, a late loan, an amount of another asset. Otherwise, with
/// `regularPayment` the rounded periodic payment plus the service fee and
/// the thread's rounding mode set Upward for an overpayment and Downward
/// for anything else:
///
/// - fixCleanup3_1_3: an amount of at least 100 regular payments costs
///   20 × the normal cost;
/// - else `amount / regularPayment` taken as an integer (under that mode),
///   then — the mode now Upward — divided by 5 and taken as an integer
///   again, at least 1, times the normal cost.
///
/// rippled calls this from `invokePreclaim` for every LoanPay, open ledger
/// or closed (applySteps.cpp:179-208), although only an open ledger compares
/// the result with the fee paid. So the one thing it can change in a closed
/// ledger is an exception: the integer conversion of an enormous quotient
/// (before fixCleanup3_1_3 caps it) or a division by a zero regular payment
/// throws, and the transaction is tefEXCEPTION — the `Err` here.
pub fn loan_pay_base_fee(sandbox: &Sandbox, tx: &TxFields, base_fee: u64) -> M<u64> {
    let fix313 = enabled(sandbox, am::FIX_CLEANUP_3_1_3);
    let fix340 = am::fix_cleanup_3_4_0(sandbox);
    let normal = transactor_base_fee(&tx.fields, base_fee);
    let Some(amount) = tx.fields.get("Amount").and_then(tx_amount) else { return Ok(normal) };
    if fix340 && !positive(&amount) {
        return Ok(normal);
    }
    let flags = tx_flags(tx);
    if flags & (TF_LOAN_FULL_PAYMENT | TF_LOAN_LATE_PAYMENT) != 0 {
        return Ok(normal);
    }
    let Some(loan) = loan_id(tx).and_then(|id| read_loan(sandbox, &id)) else { return Ok(normal) };
    if uint(&loan, "PaymentRemaining") <= LOAN_PAYMENTS_PER_FEE_INCREMENT as u64 {
        return Ok(normal);
    }
    if lm::is_payment_late(&LoanRules::of(sandbox), parent_close_time(sandbox), &loan) {
        return Ok(normal);
    }
    let Some(broker) = mpt::key_field(&loan, "LoanBrokerID").and_then(|id| read_broker(sandbox, &id)) else { return Ok(normal) };
    let Some((_, vault)) = vault_of(sandbox, &broker) else { return Ok(normal) };
    let Some(asset) = vault_asset(&vault) else { return Ok(normal) };
    if asset != amount.asset {
        return Ok(normal);
    }
    let regular_payment = arith(
        lm::round_periodic_payment(asset, number_field(&loan, "PeriodicPayment"), lm::loan_scale_of(&loan))?
            .add(number_field(&loan, "LoanServiceFee"), AMBIENT),
    )?;
    let mode = if flags & TF_LOAN_OVERPAYMENT != 0 { Rounding::Upward } else { Rounding::Downward };
    let max_increments = (LOAN_MAXIMUM_PAYMENTS_PER_TRANSACTION / LOAN_PAYMENTS_PER_FEE_INCREMENT) as u64;
    let amount = amount.number();
    if fix313 && amount >= arith(regular_payment.mul(lm::num(LOAN_MAXIMUM_PAYMENTS_PER_TRANSACTION as u64), mode))? {
        return Ok(max_increments.saturating_mul(normal));
    }
    let estimate = arith(arith(amount.div(regular_payment, mode))?.to_drops(mode))?;
    let estimate = arith(Number::new(estimate, 0, mode))?;
    let up = Rounding::Upward;
    let increments = arith(arith(estimate.div(lm::num(LOAN_PAYMENTS_PER_FEE_INCREMENT as u64), up))?.to_drops(up))?.max(1);
    Ok((increments as u64).saturating_mul(normal))
}

// ---------------------------------------------------------------------------
// LoanSet (LoanSet.cpp)
// ---------------------------------------------------------------------------

/// LoanSet (type 80): a LoanBroker's owner and a borrower agree on a loan —
/// the principal leaves the Vault for the borrower (less the origination
/// fee, which goes to the broker's owner) and a Loan object records the
/// schedule. One of the two submits; the other is sfCounterparty (the
/// broker's owner when the field is absent) and countersigns.
///
/// **The counterparty signature.** rippled checks it in three places:
/// `preflight` (structure), `STTx::checkSign` through `preflight2` (the
/// cryptography) and `LoanSet::checkSign` (that the key is the
/// counterparty's master or regular key, or its signer list is satisfied —
/// LoanSet.cpp:155-181). This engine checks, as it does for the primary
/// signature, NO cryptography and NO key authorization. What IS checked:
///
/// - a LoanSet that is not a Batch inner without sfCounterpartySignature:
///   temBAD_SIGNER (:85-94);
/// - a Batch inner (tfInnerBatchTxn) without sfCounterparty under
///   BatchV1_1: temBAD_SIGNER (:75-82);
/// - a non-empty sfSigningPubKey inside sfCounterpartySignature that is not
///   a public key (33 bytes, ed25519 `ED…` or secp256k1 `02…` / `03…`):
///   temBAD_SIGNATURE (:96-100, `preflightCheckSigningKey`);
/// - `checkSign`'s one state lookup: no sfCounterparty and no LoanBroker to
///   take the owner from: temBAD_SIGNER (:164-173), before preclaim's own
///   checks.
///
/// Fee: [`loan_set_base_fee`]. Reserve: the borrower's OwnerCount rises by
/// 1 for the Loan, judged against the submitter's balance before the fee
/// when the borrower submits and the borrower's current balance otherwise
/// (:580-587); the borrower's holding of the asset (and, with an origination
/// fee, the broker owner's) is created when missing, at their own reserve.
pub struct LoanSetTransactor;

/// `publicKeyType(slice)` (PublicKey.cpp): 33 bytes starting 0xED (ed25519)
/// or 0x02 / 0x03 (secp256k1).
fn is_public_key(hex_key: &str) -> bool {
    match hex::decode(hex_key) {
        Ok(key) => key.len() == 33 && matches!(key[0], 0xED | 0x02 | 0x03),
        Err(_) => false,
    }
}

impl LoanSetTransactor {
    /// `LoanSet::preflight` (LoanSet.cpp:61-153) under `batch_v1_1`
    /// (featureBatchV1_1).
    fn preflight_rules(tx: &TxFields, batch_v1_1: bool) -> TxResult {
        let inner = tx_flags(tx) & TF_INNER_BATCH_TXN != 0;
        if inner && batch_v1_1 && !has(tx, "Counterparty") {
            return TxResult::BadSigner;
        }
        let counter_signature = tx.fields.get("CounterpartySignature");
        if !inner && counter_signature.is_none() {
            return TxResult::BadSigner;
        }
        if let Some(signature) = counter_signature {
            let key = signature.get("SigningPubKey").and_then(|k| k.as_str()).unwrap_or("");
            if !key.is_empty() && !is_public_key(key) {
                return TxResult::BadSignature;
            }
        }
        // sfData: empty is allowed, else at most 256 bytes.
        if blob_len(tx, "Data").is_some_and(|len| len > MAX_DATA_PAYLOAD_LENGTH) {
            return TxResult::InvalidTx;
        }
        // `validNumericMinimum(value)`: absent, or not negative.
        let number_field_ok = |field: &str, max: Option<Number>| match tx.fields.get(field) {
            None => true,
            Some(value) => match number_from_json(value) {
                Some(n) => !is_negative(&n) && max.is_none_or(|max| n <= max),
                None => false,
            },
        };
        for field in ["LoanServiceFee", "LatePaymentFee", "ClosePaymentFee"] {
            if !number_field_ok(field, None) {
                return TxResult::InvalidTx;
            }
        }
        let Some(principal) = tx_number(tx, "PrincipalRequested") else { return TxResult::InvalidTx };
        if is_negative(&principal) || principal.is_zero() {
            return TxResult::InvalidTx;
        }
        // 0 <= LoanOriginationFee <= PrincipalRequested.
        if !number_field_ok("LoanOriginationFee", Some(principal)) {
            return TxResult::InvalidTx;
        }
        // `validNumericRange(value, max)`: absent, or within [min, max].
        let in_range = |field: &str, min: u64, max: u64| match tx.fields.get(field) {
            None => true,
            Some(value) => value.as_u64().is_some_and(|v| (min..=max).contains(&v)),
        };
        for (field, max) in [
            ("InterestRate", MAX_INTEREST_RATE),
            ("OverpaymentFee", MAX_OVERPAYMENT_FEE),
            ("LateInterestRate", MAX_LATE_INTEREST_RATE),
            ("CloseInterestRate", MAX_CLOSE_INTEREST_RATE),
            ("OverpaymentInterestRate", MAX_OVERPAYMENT_INTEREST_RATE),
        ] {
            if !in_range(field, 0, max as u64) {
                return TxResult::InvalidTx;
            }
        }
        if has(tx, "PaymentTotal") && tx_uint(tx, "PaymentTotal").unwrap_or(0) == 0 {
            return TxResult::InvalidTx;
        }
        if !in_range("PaymentInterval", MIN_PAYMENT_INTERVAL as u64, u32::MAX as u64) {
            return TxResult::InvalidTx;
        }
        // The grace period is between the default and the payment interval.
        let interval = tx_uint(tx, "PaymentInterval").unwrap_or(DEFAULT_PAYMENT_INTERVAL as u64);
        if !in_range("GracePeriod", DEFAULT_GRACE_PERIOD as u64, interval) {
            return TxResult::InvalidTx;
        }
        if mpt::key_field(&tx.fields, "LoanBrokerID").is_none_or(|id| is_zero_key(&id)) {
            return TxResult::InvalidTx;
        }
        TxResult::Success
    }

    /// The borrower and the broker's owner: `counterparty` is sfCounterparty
    /// or the broker's owner; the borrower is the submitter when the
    /// counterparty is the owner, else the counterparty (LoanSet.cpp:293-303).
    fn parties(tx: &TxFields, broker_owner: [u8; 20]) -> ([u8; 20], [u8; 20]) {
        let counterparty = account_of(&tx.fields, "Counterparty").unwrap_or(broker_owner);
        let borrower = if counterparty == broker_owner { tx.account } else { counterparty };
        (counterparty, borrower)
    }

    /// `LoanSet::doApply` (LoanSet.cpp:430-725).
    fn apply(tx: &TxFields, sandbox: &mut Sandbox) -> M<TxResult> {
        let rules = LoanRules::of(sandbox);
        let Some(broker_id) = mpt::key_field(&tx.fields, "LoanBrokerID") else { return Ok(TxResult::TefBadLedger) };
        let Some(mut broker) = read_broker(sandbox, &broker_id) else { return Ok(TxResult::TefBadLedger) };
        let Some(broker_owner) = account_of(&broker, "Owner") else { return Ok(TxResult::TefBadLedger) };
        if root_of(sandbox, &broker_owner).is_none() {
            return Ok(TxResult::TefBadLedger);
        }
        let Some((vault_key, mut vault)) = vault_of(sandbox, &broker) else { return Ok(TxResult::TefBadLedger) };
        let (Some(vault_pseudo), Some(asset)) = (account_of(&vault, "Account"), vault_asset(&vault)) else {
            return Ok(TxResult::TefBadLedger);
        };
        let (_, borrower) = Self::parties(tx, broker_owner);
        if root_of(sandbox, &borrower).is_none() {
            return Ok(TxResult::TefBadLedger);
        }
        let Some(broker_pseudo) = account_of(&broker, "Account") else { return Ok(TxResult::TefBadLedger) };
        if root_of(sandbox, &broker_pseudo).is_none() {
            return Ok(TxResult::TefBadLedger);
        }
        let Some(principal) = tx_number(tx, "PrincipalRequested") else { return Ok(TxResult::TefBadLedger) };

        let vault_available = number_field(&vault, "AssetsAvailable");
        let vault_total = number_field(&vault, "AssetsTotal");
        let vault_scale = lh::assets_total_scale(&vault, AMBIENT)?;
        if vault_available < principal {
            return Ok(TxResult::InsufficientFunds);
        }

        let interest_rate = tx_uint(tx, "InterestRate").unwrap_or(0) as u32;
        let payment_interval = tx_uint(tx, "PaymentInterval").unwrap_or(DEFAULT_PAYMENT_INTERVAL as u64) as u32;
        let payment_total = tx_uint(tx, "PaymentTotal").unwrap_or(DEFAULT_PAYMENT_TOTAL as u64) as u32;
        let management_fee_rate = uint(&broker, "ManagementFeeRate") as u32;
        let properties =
            lm::compute_loan_properties(&rules, asset, principal, interest_rate, payment_interval, payment_total, management_fee_rate, vault_scale)?;
        // The state with the principal as requested (not re-rounded).
        let state = lm::construct_loan_state(properties.loan_state.value_outstanding, principal, properties.loan_state.management_fee_due)?;

        if lm::loan_origination_exceeds_vault_maximum(&vault, vault_total, state.interest_due)? {
            return Ok(TxResult::LimitExceeded);
        }
        // The value fields must fit the loan's scale.
        for field in VALUE_FIELDS {
            if let Some(value) = tx_number(tx, field) {
                if !lm::is_rounded(asset, value, properties.loan_scale)? {
                    return Ok(TxResult::PrecisionLoss);
                }
            }
        }
        let guards = lm::check_loan_guards(asset, principal, interest_rate != 0, payment_total, &properties)?;
        if guards != TxResult::Success {
            return Ok(guards);
        }
        // "Does not compute".
        let positive_number = |n: &Number| !n.negative && !n.is_zero();
        if is_negative(&properties.loan_state.management_fee_due)
            || !positive_number(&properties.loan_state.value_outstanding)
            || !positive_number(&properties.periodic_payment)
        {
            return Ok(TxResult::TecInternal);
        }

        let origination_fee = tx_number(tx, "LoanOriginationFee").unwrap_or(Number::ZERO);
        let to_borrower = arith(principal.sub(origination_fee, AMBIENT))?;

        let deltas = lm::loan_origination_deltas(&vault, principal, state.interest_due)?;
        let new_debt_total = arith(number_field(&broker, "DebtTotal").add(deltas.debt_total_delta, AMBIENT))?;
        let debt_maximum = number_field(&broker, "DebtMaximum");
        if !debt_maximum.is_zero() && debt_maximum < new_debt_total {
            return Ok(TxResult::LimitExceeded);
        }
        let cover_rate_minimum = uint(&broker, "CoverRateMinimum") as u32;
        if number_field(&broker, "CoverAvailable") < minimum_cover(sandbox, new_debt_total, cover_rate_minimum, &vault, None)? {
            return Ok(TxResult::InsufficientFunds);
        }

        // The borrower's reserve for the Loan.
        crate::tx::offer::owner_count_add(sandbox, &borrower, 1);
        let Some(borrower_root) = root_of(sandbox, &borrower) else { return Ok(TxResult::TefBadLedger) };
        let balance = if tx.account == borrower { prior_balance(sandbox, tx) } else { xrp_balance(&borrower_root) };
        if balance < crate::ledger::fees::account_reserve(sandbox, uint(&borrower_root, "OwnerCount")) {
            return Ok(TxResult::InsufficientReserve);
        }

        // The borrower's holding; an existing one is fine.
        let held = holding::add_empty_holding(sandbox, &borrower, xrp_balance(&borrower_root), &asset);
        if held != TxResult::Success && held != TxResult::Duplicate {
            return Ok(held);
        }
        let authorized = holding::require_auth(sandbox, &asset, &borrower, AuthType::StrongAuth);
        if authorized != TxResult::Success {
            return Ok(authorized);
        }
        if !origination_fee.is_zero() {
            // The broker's owner receives the fee: its holding too.
            let Some(owner_root) = root_of(sandbox, &broker_owner) else { return Ok(TxResult::TefBadLedger) };
            let held = holding::add_empty_holding(sandbox, &broker_owner, xrp_balance(&owner_root), &asset);
            if held != TxResult::Success && held != TxResult::Duplicate {
                return Ok(held);
            }
        }
        let authorized = holding::require_auth(sandbox, &asset, &broker_owner, AuthType::StrongAuth);
        if authorized != TxResult::Success {
            return Ok(authorized);
        }

        let sent = holding::account_send_multi(
            sandbox,
            &vault_pseudo,
            &asset,
            &[(borrower, to_borrower), (broker_owner, origination_fee)],
            true,
            AMBIENT,
        )?;
        if sent != TxResult::Success {
            return Ok(sent);
        }

        let start_date = ledger_close_time(sandbox);
        let loan_sequence = uint(&broker, "LoanSequence") as u32;
        let loan_key = keylet::loan_key(&broker_id, loan_sequence);
        let mut loan = serde_json::json!({
            "LedgerEntryType": "Loan",
            "Flags": if tx_flags(tx) & TF_LOAN_OVERPAYMENT != 0 { LSF_LOAN_OVERPAYMENT } else { 0 },
            "LoanScale": properties.loan_scale,
            "StartDate": start_date,
            "PaymentInterval": payment_interval,
            "LoanSequence": loan_sequence,
            "LoanBrokerID": hex::encode_upper(broker_id.0),
            "Borrower": hex::encode(borrower),
            "GracePeriod": tx_uint(tx, "GracePeriod").unwrap_or(DEFAULT_GRACE_PERIOD as u64),
            "PreviousPaymentDueDate": 0,
            "NextPaymentDueDate": start_date.wrapping_add(payment_interval),
            "PaymentRemaining": payment_total,
        });
        for field in ["LoanOriginationFee", "LoanServiceFee", "LatePaymentFee", "ClosePaymentFee"] {
            set_number_field(&mut loan, field, tx_number(tx, field).unwrap_or(Number::ZERO), Soe::Default);
        }
        for field in ["OverpaymentFee", "InterestRate", "LateInterestRate", "CloseInterestRate", "OverpaymentInterestRate"] {
            loan[field] = Value::from(tx_uint(tx, field).unwrap_or(0));
        }
        set_number_field(&mut loan, "PrincipalOutstanding", principal, Soe::Default);
        set_number_field(&mut loan, "PeriodicPayment", properties.periodic_payment, Soe::Required);
        set_number_field(&mut loan, "TotalValueOutstanding", properties.loan_state.value_outstanding, Soe::Default);
        set_number_field(&mut loan, "ManagementFeeOutstanding", properties.loan_state.management_fee_due, Soe::Default);

        // The vault lends the principal.
        set_number_field(&mut vault, "AssetsAvailable", arith(vault_available.sub(principal, AMBIENT))?, Soe::Default);
        set_number_field(&mut vault, "AssetsTotal", arith(vault_total.add(deltas.assets_total_delta, AMBIENT))?, Soe::Default);

        // The broker owes it.
        let debt_total = lm::adjust_imprecise_number(number_field(&broker, "DebtTotal"), deltas.debt_total_delta, asset, vault_scale)?;
        set_number_field(&mut broker, "DebtTotal", debt_total, Soe::Default);
        broker["OwnerCount"] = Value::from((uint(&broker, "OwnerCount") as u32).saturating_add(1));
        let next_sequence = loan_sequence.wrapping_add(1);
        broker["LoanSequence"] = Value::from(next_sequence);
        if next_sequence == 0 {
            return Ok(TxResult::MaxSequenceReached);
        }

        // dirLink: the broker pseudo-account's directory, then the borrower's.
        let broker_node = owner_dir_insert(sandbox, &broker_pseudo, &loan_key);
        loan["LoanBrokerNode"] = Value::String(format!("{broker_node:x}"));
        let owner_node = owner_dir_insert(sandbox, &borrower, &loan_key);
        loan["OwnerNode"] = Value::String(format!("{owner_node:x}"));

        for (key, obj, format) in [(vault_key, &mut vault, &VAULT), (broker_id, &mut broker, &LOAN_BROKER), (loan_key, &mut loan, &LOAN)] {
            let stored = store(sandbox, key, obj, format, Some(asset))?;
            if stored != TxResult::Success {
                return Ok(stored);
            }
        }
        Ok(TxResult::Success)
    }
}

impl Transactor for LoanSetTransactor {
    /// The rules-free form: featureBatchV1_1 off.
    fn preflight(&self, tx: &TxFields) -> TxResult {
        Self::preflight_rules(tx, false)
    }

    fn preflight_in(&self, tx: &TxFields, rules: &Sandbox) -> TxResult {
        // `checkExtraFeatures` (:49-53), `getFlagsMask` (:55-59).
        gate(tx, rules, lh::check_lending_protocol_dependencies(rules, tx), TF_LOAN_SET_MASK)
            .unwrap_or_else(|| Self::preflight_rules(tx, enabled(rules, am::BATCH_V1_1)))
    }

    /// `LoanSet::checkSign`'s state lookup (LoanSet.cpp:155-173), then
    /// `LoanSet::preclaim` (:231-428).
    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let Some(broker_id) = mpt::key_field(&tx.fields, "LoanBrokerID") else { return TxResult::TefInternal };
        let broker = read_broker(sandbox, &broker_id);
        // checkSign: somebody must be there to countersign.
        if !has(tx, "Counterparty") && broker.is_none() {
            return TxResult::BadSigner;
        }

        let interval = tx_uint(tx, "PaymentInterval").unwrap_or(DEFAULT_PAYMENT_INTERVAL as u64) as u32;
        let total = tx_uint(tx, "PaymentTotal").unwrap_or(DEFAULT_PAYMENT_TOTAL as u64) as u32;
        let start_date = ledger_close_time(sandbox);
        {
            // Every due date must fit the 32-bit clock.
            let time_available = u32::MAX - start_date;
            let grace = tx_uint(tx, "GracePeriod").unwrap_or(DEFAULT_GRACE_PERIOD as u64) as u32;
            if grace > time_available || interval > time_available || total > time_available {
                return TxResult::Killed;
            }
            let time_last_payment = time_available - grace;
            if interval == 0 || time_last_payment / interval < total {
                return TxResult::Killed;
            }
        }

        let account = tx.account;
        let Some(broker) = broker else { return TxResult::NoEntry };
        let Some(broker_owner) = account_of(&broker, "Owner") else { return TxResult::TefInternal };
        let (counterparty, borrower) = Self::parties(tx, broker_owner);
        // One of the two parties must own the broker.
        if account != broker_owner && counterparty != broker_owner {
            return TxResult::NoPermission;
        }
        let Some(broker_pseudo) = account_of(&broker, "Account") else { return TxResult::TefInternal };
        if root_of(sandbox, &borrower).is_none() {
            return TxResult::NoAccount;
        }
        let Some((_, vault)) = vault_of(sandbox, &broker) else { return TxResult::TefBadLedger };

        if enabled(sandbox, am::LENDING_PROTOCOL_V1_1) {
            match vh::vault_phase(sandbox, &vault) {
                VaultPhase::Subscription => return TxResult::TooSoon,
                VaultPhase::Redemption => return TxResult::Expired,
                VaultPhase::Investment => {
                    // The last payment must fall due a buffer ahead of redemption.
                    let final_payment = start_date as u64 + interval as u64 * total as u64;
                    let Some(redemption_date) = vault.get("RedemptionDate").and_then(|d| d.as_u64()) else {
                        // `vault->at(sfRedemptionDate)` of a missing optional field throws.
                        return TxResult::TefException;
                    };
                    if final_payment + LOAN_REDEMPTION_BUFFER as u64 > redemption_date {
                        return TxResult::NoPermission;
                    }
                }
                VaultPhase::NoPhase => {}
            }
        }

        // An accrual vault already at its maximum cannot recognize interest.
        let assets_maximum = number_field(&vault, "AssetsMaximum");
        if vh::vault_version(&vault) != VaultVersion::CashBasis
            && !assets_maximum.is_zero()
            && number_field(&vault, "AssetsTotal") >= assets_maximum
        {
            return TxResult::LimitExceeded;
        }

        let (Some(asset), Some(vault_pseudo)) = (vault_asset(&vault), account_of(&vault, "Account")) else {
            return TxResult::TefInternal;
        };
        // The value fields must be representable in the asset (whole units
        // of XRP or an MPT; sixteen digits of an IOU). A value the asset
        // cannot hold at all throws in 3.4.1; fixCleanup3_5_0 refuses it.
        let fix350 = am::fix_cleanup_3_5_0(sandbox);
        for field in VALUE_FIELDS {
            let Some(value) = tx_number(tx, field) else { continue };
            match Amount::from_number(asset, value, AMBIENT) {
                Ok(amount) if amount.number() == value => {}
                Ok(_) => return TxResult::PrecisionLoss,
                Err(_) if fix350 => return TxResult::PrecisionLoss,
                Err(_) => return TxResult::TefException,
            }
        }

        // fixCleanup3_4_0: the asset need not be holdable by a new account
        // when nobody needs a new holding.
        let origination_fee = tx_number(tx, "LoanOriginationFee").unwrap_or(Number::ZERO);
        if !am::fix_cleanup_3_4_0(sandbox)
            || !holding::holding_exists(sandbox, &borrower, &asset)
            || (!origination_fee.is_zero() && !holding::holding_exists(sandbox, &broker_owner, &asset))
        {
            let holdable = holding::can_add_holding(sandbox, &asset);
            if holdable != TxResult::Success {
                return holdable;
            }
        }

        for refused in [
            holding::check_frozen(sandbox, &vault_pseudo, &asset),
            holding::check_deep_frozen(sandbox, &broker_pseudo, &asset),
            holding::check_frozen(sandbox, &borrower, &asset),
            holding::check_deep_frozen(sandbox, &broker_owner, &asset),
        ] {
            if refused != TxResult::Success {
                return refused;
            }
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        applied(sandbox, |sb| Self::apply(tx, sb))
    }
}

// ---------------------------------------------------------------------------
// LoanManage (LoanManage.cpp)
// ---------------------------------------------------------------------------

/// LoanManage (type 82): the broker's owner marks a loan impaired
/// (tfLoanImpair), takes the mark back (tfLoanUnimpair) or declares it in
/// default (tfLoanDefault). With no flag it succeeds and changes nothing.
///
/// Fee: the plain base fee. No reserve change.
pub struct LoanManageTransactor;

/// `LoanManage::unimpairLoan` (LoanManage.cpp:338-390) on the Loan and the
/// Vault in hand — also LoanPay's first step on an impaired loan
/// (LoanPay.cpp:413-419): the loss the impairment booked
/// ([`lm::loan_vault_exposure`]) leaves sfLossUnrealized (tefBAD_LEDGER when
/// more than is booked) and lsfLoanImpaired is cleared. Before
/// fixCleanup3_4_0 the next due date is put back: the normal one
/// (`max(previous, start) + interval`) while that has not passed, else one
/// interval from now.
fn unimpair_loan(sandbox: &Sandbox, loan: &mut Value, vault: &mut Value, asset: HeldAsset) -> M<TxResult> {
    let vault_scale = lh::assets_total_scale(vault, AMBIENT)?;
    let loss_unrealized = number_field(vault, "LossUnrealized");
    let loss_reversed = lm::loan_vault_exposure(vault, loan)?;
    if loss_unrealized < loss_reversed {
        return Ok(TxResult::TefBadLedger);
    }
    let adjusted = lm::adjust_imprecise_number(loss_unrealized, loss_reversed.negated(), asset, vault_scale)?;
    set_number_field(vault, "LossUnrealized", adjusted, Soe::Default);

    loan["Flags"] = Value::from(obj_flags(loan) & !LSF_LOAN_IMPAIRED);
    if !am::fix_cleanup_3_4_0(sandbox) {
        let interval = uint(loan, "PaymentInterval") as u32;
        let normal_due = (uint(loan, "PreviousPaymentDueDate") as u32).max(uint(loan, "StartDate") as u32).wrapping_add(interval);
        let next_due = if !holding::has_expired(sandbox, Some(normal_due), holding::ExpiryComparison::Inclusive) {
            normal_due
        } else {
            parent_close_time(sandbox).wrapping_add(interval)
        };
        loan["NextPaymentDueDate"] = Value::from(next_due);
    }
    Ok(TxResult::Success)
}

impl LoanManageTransactor {
    /// `LoanManage::impairLoan` (LoanManage.cpp:286-336): the vault's
    /// exposure to the loan becomes an unrealized loss — refused
    /// tecLIMIT_EXCEEDED when that is more than the vault has lent out
    /// (`AssetsTotal − AssetsAvailable`). fixCleanup3_4_0: only a late loan
    /// (tecTOO_SOON otherwise); before it a loan not yet late has its due
    /// date pulled to now.
    fn impair_loan(sandbox: &Sandbox, loan: &mut Value, vault: &mut Value, asset: HeldAsset) -> M<TxResult> {
        let rules = LoanRules::of(sandbox);
        let now = parent_close_time(sandbox);
        if rules.fix_cleanup_3_4_0 && !lm::is_payment_late(&rules, now, loan) {
            return Ok(TxResult::TooSoon);
        }
        let loss = lm::loan_vault_exposure(vault, loan)?;
        let vault_scale = lh::assets_total_scale(vault, AMBIENT)?;
        let loss_unrealized = lm::adjust_imprecise_number(number_field(vault, "LossUnrealized"), loss, asset, vault_scale)?;
        set_number_field(vault, "LossUnrealized", loss_unrealized, Soe::Default);
        if arith(number_field(vault, "AssetsTotal").sub(number_field(vault, "AssetsAvailable"), AMBIENT))? < loss_unrealized {
            return Ok(TxResult::LimitExceeded);
        }
        loan["Flags"] = Value::from(obj_flags(loan) | LSF_LOAN_IMPAIRED);
        if !rules.fix_cleanup_3_4_0 && !lm::is_payment_late(&rules, now, loan) {
            loan["NextPaymentDueDate"] = Value::from(now);
        }
        Ok(TxResult::Success)
    }

    /// `LoanManage::defaultLoan` (LoanManage.cpp:134-284): the vault's
    /// exposure to the loan is written off.
    ///
    /// - the first-loss capital that covers it: `DebtTotal ×
    ///   CoverRateMinimum × CoverRateLiquidation`, at most the exposure,
    ///   rounded at the loan's scale — the whole expression under an Upward
    ///   guard — and at most sfCoverAvailable;
    /// - the vault: sfAssetsTotal falls by the uncovered part (rounded DOWN
    ///   at the vault's scale; more than the total is tefBAD_LEDGER),
    ///   sfAssetsAvailable rises by the covered part; an IOU vault whose
    ///   available then exceeds its total by dust (more than thirteen
    ///   orders below it) takes the larger value for both; available above
    ///   total otherwise is tecINTERNAL; an impaired loan's unrealized loss
    ///   is released;
    /// - the broker: sfDebtTotal falls by the exposure, sfCoverAvailable by
    ///   the covered part;
    /// - the loan: lsfLoanDefault, its balances, sfPaymentRemaining and
    ///   sfNextPaymentDueDate zero;
    /// - the covered part moves from the broker's pseudo-account to the
    ///   vault's.
    #[allow(clippy::too_many_arguments)]
    fn default_loan(
        sandbox: &mut Sandbox,
        keys: (Hash256, Hash256, Hash256),
        loan: &mut Value,
        broker: &mut Value,
        vault: &mut Value,
        asset: HeldAsset,
        associate: bool,
    ) -> M<TxResult> {
        let loan_scale = lm::loan_scale_of(loan);
        let debt_total = number_field(broker, "DebtTotal");
        let total_default = lm::loan_vault_exposure(vault, loan)?;
        let cover_available = number_field(broker, "CoverAvailable");
        let default_covered = {
            let up = Rounding::Upward;
            let minimum_cover = arith(tenth_bips_of_value(debt_total, uint(broker, "CoverRateMinimum") as u32, up))?;
            let liquidated = arith(tenth_bips_of_value(minimum_cover, uint(broker, "CoverRateLiquidation") as u32, up))?;
            let covered = round_to_asset(asset, if total_default < liquidated { total_default } else { liquidated }, loan_scale, up)?;
            if cover_available < covered {
                cover_available
            } else {
                covered
            }
        };
        let vault_default = arith(total_default.sub(default_covered, AMBIENT))?;
        let vault_scale = lh::assets_total_scale(vault, AMBIENT)?;

        let mut vault_total = number_field(vault, "AssetsTotal");
        if vault_total < vault_default {
            return Ok(TxResult::TefBadLedger);
        }
        let vault_default_rounded = round_to_asset(asset, vault_default, vault_scale, Rounding::Downward)?;
        vault_total = arith(vault_total.sub(vault_default_rounded, AMBIENT))?;
        let vault_available = arith(number_field(vault, "AssetsAvailable").add(default_covered, AMBIENT))?;
        let integral = !matches!(asset, HeldAsset::Iou { .. });
        if vault_total < vault_available && !integral {
            let difference = arith(vault_available.sub(vault_total, AMBIENT))?;
            if vault_available.external_exponent() - difference.external_exponent() > 13 {
                vault_total = vault_available;
            }
        }
        if vault_total < vault_available {
            return Ok(TxResult::TecInternal);
        }
        set_number_field(vault, "AssetsTotal", vault_total, Soe::Default);
        set_number_field(vault, "AssetsAvailable", vault_available, Soe::Default);
        if obj_flags(loan) & LSF_LOAN_IMPAIRED != 0 {
            let loss_unrealized = number_field(vault, "LossUnrealized");
            if loss_unrealized < total_default {
                return Ok(TxResult::TefBadLedger);
            }
            let adjusted = lm::adjust_imprecise_number(loss_unrealized, total_default.negated(), asset, vault_scale)?;
            set_number_field(vault, "LossUnrealized", adjusted, Soe::Default);
        }

        let debt_total = lm::adjust_imprecise_number(debt_total, total_default.negated(), asset, vault_scale)?;
        set_number_field(broker, "DebtTotal", debt_total, Soe::Default);
        if cover_available < default_covered {
            return Ok(TxResult::TefBadLedger);
        }
        set_number_field(broker, "CoverAvailable", arith(cover_available.sub(default_covered, AMBIENT))?, Soe::Default);

        loan["Flags"] = Value::from(obj_flags(loan) | LSF_LOAN_DEFAULT);
        for field in ["TotalValueOutstanding", "PrincipalOutstanding", "ManagementFeeOutstanding"] {
            set_number_field(loan, field, Number::ZERO, Soe::Default);
        }
        loan["PaymentRemaining"] = Value::from(0u32);
        loan["NextPaymentDueDate"] = Value::from(0u32);

        let (Some(broker_pseudo), Some(vault_pseudo)) = (account_of(broker, "Account"), account_of(vault, "Account")) else {
            return Ok(TxResult::TefBadLedger);
        };
        let asset_if = associate.then_some(asset);
        let (loan_key, broker_key, vault_key) = keys;
        for (key, obj, format) in [(vault_key, vault, &VAULT), (broker_key, broker, &LOAN_BROKER), (loan_key, loan, &LOAN)] {
            let stored = store(sandbox, key, obj, format, asset_if)?;
            if stored != TxResult::Success {
                return Ok(stored);
            }
        }
        let covered = Amount::from_number(asset, default_covered, AMBIENT)?;
        Ok(holding::account_send(sandbox, &broker_pseudo, &vault_pseudo, &covered, true))
    }

    /// `LoanManage::doApply` (LoanManage.cpp:392-436).
    fn apply(tx: &TxFields, sandbox: &mut Sandbox) -> M<TxResult> {
        let Some(loan_key) = loan_id(tx) else { return Ok(TxResult::TefBadLedger) };
        let Some(mut loan) = read_loan(sandbox, &loan_key) else { return Ok(TxResult::TefBadLedger) };
        let Some(broker_key) = mpt::key_field(&loan, "LoanBrokerID") else { return Ok(TxResult::TefBadLedger) };
        let Some(mut broker) = read_broker(sandbox, &broker_key) else { return Ok(TxResult::TefBadLedger) };
        let Some((vault_key, mut vault)) = vault_of(sandbox, &broker) else { return Ok(TxResult::TefBadLedger) };
        let Some(asset) = vault_asset(&vault) else { return Ok(TxResult::TefBadLedger) };
        // fixCleanup3_1_3: the objects written are rounded to the asset
        // (`associateAsset`, :426-433).
        let associate = enabled(sandbox, am::FIX_CLEANUP_3_1_3);
        let asset_if = associate.then_some(asset);
        let flags = tx_flags(tx);
        if flags & TF_LOAN_DEFAULT != 0 {
            return Self::default_loan(sandbox, (loan_key, broker_key, vault_key), &mut loan, &mut broker, &mut vault, asset, associate);
        }
        let changed = if flags & TF_LOAN_IMPAIR != 0 {
            Self::impair_loan(sandbox, &mut loan, &mut vault, asset)?
        } else if flags & TF_LOAN_UNIMPAIR != 0 {
            unimpair_loan(sandbox, &mut loan, &mut vault, asset)?
        } else {
            // No flag: a success that updates nothing.
            return Ok(TxResult::Success);
        };
        if changed != TxResult::Success {
            return Ok(changed);
        }
        // Impairing and unimpairing update the vault and the loan; the
        // broker is only read.
        for (key, obj, format) in [(vault_key, &mut vault, &VAULT), (loan_key, &mut loan, &LOAN)] {
            let stored = store(sandbox, key, obj, format, asset_if)?;
            if stored != TxResult::Success {
                return Ok(stored);
            }
        }
        Ok(TxResult::Success)
    }
}

impl Transactor for LoanManageTransactor {
    /// `LoanManage::preflight` (LoanManage.cpp:43-62): a zero sfLoanID is
    /// temINVALID; more than one of the three flags temINVALID_FLAG.
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if loan_id(tx).is_none_or(|id| is_zero_key(&id)) {
            return TxResult::InvalidTx;
        }
        let flags = tx_flags(tx) & TF_UNIVERSAL_MASK;
        if flags & flags.wrapping_sub(1) != 0 {
            return TxResult::InvalidFlag;
        }
        TxResult::Success
    }

    fn preflight_in(&self, tx: &TxFields, rules: &Sandbox) -> TxResult {
        // `checkExtraFeatures` (:31-35), `getFlagsMask` (:37-41).
        gate(tx, rules, lh::check_lending_protocol_dependencies(rules, tx), TF_LOAN_MANAGE_MASK).unwrap_or_else(|| self.preflight(tx))
    }

    /// `LoanManage::preclaim` (LoanManage.cpp:64-132).
    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let Some(loan) = loan_id(tx).and_then(|id| read_loan(sandbox, &id)) else { return TxResult::NoEntry };
        let loan_flags = obj_flags(&loan);
        let flags = tx_flags(tx);
        // A defaulted loan cannot be changed.
        if loan_flags & LSF_LOAN_DEFAULT != 0 {
            return TxResult::NoPermission;
        }
        if loan_flags & LSF_LOAN_IMPAIRED != 0 && flags & TF_LOAN_IMPAIR != 0 {
            return TxResult::NoPermission;
        }
        if loan_flags & (LSF_LOAN_IMPAIRED | LSF_LOAN_DEFAULT) == 0 && flags & TF_LOAN_UNIMPAIR != 0 {
            return TxResult::NoPermission;
        }
        // Nor one that is fully paid.
        if uint(&loan, "PaymentRemaining") == 0 {
            return TxResult::NoPermission;
        }
        if flags & TF_LOAN_DEFAULT != 0 {
            // Only once the grace period after the due date is over:
            // strictly after it under fixCleanup3_4_0.
            let deadline = (uint(&loan, "NextPaymentDueDate") as u32).wrapping_add(uint(&loan, "GracePeriod") as u32);
            let comparison =
                if am::fix_cleanup_3_4_0(sandbox) { holding::ExpiryComparison::Exclusive } else { holding::ExpiryComparison::Inclusive };
            if !holding::has_expired(sandbox, Some(deadline), comparison) {
                return TxResult::TooSoon;
            }
        }
        let Some(broker) = mpt::key_field(&loan, "LoanBrokerID").and_then(|id| read_broker(sandbox, &id)) else {
            return TxResult::TecInternal;
        };
        if account_of(&broker, "Owner") != Some(tx.account) {
            return TxResult::NoPermission;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        applied(sandbox, |sb| Self::apply(tx, sb))
    }
}

// ---------------------------------------------------------------------------
// LoanPay (LoanPay.cpp)
// ---------------------------------------------------------------------------

/// How LoanPay reads the three balances of its fund-conservation check
/// (`conservationBalance`). The check itself only logs and asserts; what
/// reaches the ledger is an exception while the balances are read or summed
/// (an IOU amount above 9999999999999999e80: tefEXCEPTION).
///
/// 3.4.1 and 3.5.0-rc1 differ here WITHOUT an amendment gate
/// (`xrpl-ops/vaultport/diff-341-vs-350rc.md`, item B1):
///
/// - [`ConservationModel::V341`] (LoanPay.cpp:54-72): every non-XRP
///   balance is `accountHolds(…, SpendableHandling::FullBalance)` — for a
///   trust-line holder the balance PLUS the limit the issuer set on its own
///   side of the line. An issuer who set limits of 5e95 or more toward the
///   borrower and the fee's payee makes the sum overflow: tefEXCEPTION.
/// - [`ConservationModel::V350`] (rc1 LoanPay.cpp:53-90): a holder's
///   balance is the stored one (`SimpleBalance`); only the issuer itself is
///   read with FullBalance. The same LoanPay applies.
///
/// Mainnet validators run 3.4.1, so that is the default. rippled has no
/// rule to tell the two apart; this engine uses fixCleanup3_5_0 being
/// enabled as a PROXY for "this ledger is made by 3.5.x servers" — an
/// amendment only activates when the validators run software that knows it.
/// That is our choice, not a gate in rippled: a 3.5.x network on which
/// fixCleanup3_5_0 is not (yet) enabled behaves as V350 while this engine
/// would still apply V341.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConservationModel {
    V341,
    V350,
}

impl ConservationModel {
    pub fn of(sandbox: &Sandbox) -> ConservationModel {
        if am::fix_cleanup_3_5_0(sandbox) {
            ConservationModel::V350
        } else {
            ConservationModel::V341
        }
    }
}

/// `conservationBalance(view, id, asset)` for an IOU; `None` for XRP and
/// MPT, whose balances and sums cannot throw (an XRP balance is the root's
/// sfBalance; an MPT holder's is its MPToken's amount and the issuer's
/// `MaximumAmount − OutstandingAmount`, so three of them never pass the
/// largest MPT amount).
fn conservation_balance(sandbox: &Sandbox, id: &[u8; 20], asset: &HeldAsset, model: ConservationModel) -> M<Option<Amount>> {
    let HeldAsset::Iou { issuer, .. } = asset else { return Ok(None) };
    if model == ConservationModel::V350 && id != issuer {
        return Ok(Some(holding::account_holds(sandbox, id, asset, false, false)));
    }
    Ok(Some(holding::account_holds_full(sandbox, id, asset, false, false)?))
}

/// The three balances of the check (LoanPay.cpp:634-640, :704-710): the
/// submitter's, the vault pseudo-account's and the fee payee's — the last
/// two zero when they are the submitter.
fn conservation_balances(
    sandbox: &Sandbox,
    account: &[u8; 20],
    vault_pseudo: &[u8; 20],
    broker_payee: &[u8; 20],
    asset: &HeldAsset,
    model: ConservationModel,
) -> M<Option<[Amount; 3]>> {
    let Some(of_account) = conservation_balance(sandbox, account, asset, model)? else { return Ok(None) };
    let other = |id: &[u8; 20]| -> M<Amount> {
        if id == account {
            Ok(Amount::zero(*asset))
        } else {
            Ok(conservation_balance(sandbox, id, asset, model)?.unwrap_or(Amount::zero(*asset)))
        }
    };
    Ok(Some([of_account, other(vault_pseudo)?, other(broker_payee)?]))
}

/// The arithmetic of the check (LoanPay.cpp:711-786), for what it can
/// throw: the scale the balances are compared at, each balance rounded to
/// it, the two totals, the three changes and their total.
fn conservation_arithmetic(before: &[Amount; 3], after: &[Amount; 3]) -> M<()> {
    const MAX_OFFSET: i32 = 80;
    let exponents: Vec<i32> = before.iter().chain(after.iter()).filter(|a| !a.is_zero()).map(|a| a.exponent).collect();
    let balance_scale = match (exponents.iter().min(), exponents.iter().max()) {
        (Some(min), Some(max)) => (if min == max { *max } else { *max + 1 }).min(MAX_OFFSET),
        _ => 0,
    };
    let sum = |a: &[Amount; 3]| -> M<Amount> { a[0].iou_add(a[1], AMBIENT)?.iou_add(a[2], AMBIENT) };
    for amount in before.iter().chain(after.iter()) {
        round_to_scale(*amount, balance_scale, AMBIENT)?;
    }
    round_to_scale(sum(before)?, balance_scale, AMBIENT)?;
    round_to_scale(sum(after)?, balance_scale, AMBIENT)?;
    let mut changes = [Amount::zero(before[0].asset); 3];
    for i in 0..3 {
        changes[i] = after[i].iou_add(before[i].negated(), AMBIENT)?;
    }
    round_to_scale(sum(&changes)?, balance_scale, AMBIENT)?;
    Ok(())
}

/// LoanPay (type 84): the borrower pays — one or more scheduled payments,
/// with tfLoanOverpayment the excess against principal, with
/// tfLoanLatePayment a late one with its penalties, with tfLoanFullPayment
/// the whole loan early (`tx::loan_math::loan_make_payment`). Principal and
/// interest go to the vault's pseudo-account, the fees to the broker's
/// owner — or, when the cover is below its minimum or the owner cannot
/// receive the asset, to the broker's pseudo-account as more cover.
///
/// Fee: [`loan_pay_base_fee`]. Reserve: none, except the holding created
/// for a submitter who is also the fee's payee (LoanPay.cpp:654-669).
pub struct LoanPayTransactor;

impl LoanPayTransactor {
    /// `LoanPay::doApply` (LoanPay.cpp:331-848).
    fn apply(tx: &TxFields, sandbox: &mut Sandbox) -> M<TxResult> {
        let rules = LoanRules::of(sandbox);
        let account = tx.account;
        let Some(amount) = tx.fields.get("Amount").and_then(tx_amount) else { return Ok(TxResult::TefBadLedger) };
        let Some(loan_key) = loan_id(tx) else { return Ok(TxResult::TefBadLedger) };
        let Some(mut loan) = read_loan(sandbox, &loan_key) else { return Ok(TxResult::TefBadLedger) };
        let loan_scale = lm::loan_scale_of(&loan);
        let Some(broker_key) = mpt::key_field(&loan, "LoanBrokerID") else { return Ok(TxResult::TefBadLedger) };
        let Some(mut broker) = read_broker(sandbox, &broker_key) else { return Ok(TxResult::TefBadLedger) };
        let (Some(broker_owner), Some(broker_pseudo)) = (account_of(&broker, "Owner"), account_of(&broker, "Account")) else {
            return Ok(TxResult::TefBadLedger);
        };
        let Some((vault_key, mut vault)) = vault_of(sandbox, &broker) else { return Ok(TxResult::TefBadLedger) };
        let (Some(vault_pseudo), Some(asset)) = (account_of(&vault, "Account"), vault_asset(&vault)) else {
            return Ok(TxResult::TefBadLedger);
        };

        let cover_available = number_field(&broker, "CoverAvailable");
        let debt_total = number_field(&broker, "DebtTotal");
        let vault_scale = lh::assets_total_scale(&vault, AMBIENT)?;

        // The fee goes to the owner while the cover is at its minimum and
        // the owner can receive the asset; else it tops the cover up.
        let send_fee_to_owner = {
            let minimum = minimum_cover(sandbox, debt_total, uint(&broker, "CoverRateMinimum") as u32, &vault, Some((asset, loan_scale)))?;
            cover_available >= minimum
                && holding::check_deep_frozen(sandbox, &broker_owner, &asset) == TxResult::Success
                && holding::require_auth(sandbox, &asset, &broker_owner, AuthType::StrongAuth) == TxResult::Success
        };
        let broker_payee = if send_fee_to_owner { broker_owner } else { broker_pseudo };
        if !send_fee_to_owner {
            let receivable = holding::check_deep_frozen(sandbox, &broker_payee, &asset);
            if receivable != TxResult::Success {
                return Ok(receivable);
            }
        }

        // A payment on an impaired loan unimpairs it first.
        if obj_flags(&loan) & LSF_LOAN_IMPAIRED != 0 {
            let unimpaired = unimpair_loan(sandbox, &mut loan, &mut vault, asset)?;
            if unimpaired != TxResult::Success {
                return Ok(unimpaired);
            }
        }

        let flags = tx_flags(tx);
        let payment_type = if flags & TF_LOAN_LATE_PAYMENT != 0 {
            LoanPaymentType::Late
        } else if flags & TF_LOAN_FULL_PAYMENT != 0 {
            LoanPaymentType::Full
        } else if flags & TF_LOAN_OVERPAYMENT != 0 {
            LoanPaymentType::Overpayment
        } else {
            LoanPaymentType::Regular
        };
        let parts = match lm::loan_make_payment(&rules, parent_close_time(sandbox), asset, &mut loan, &broker, &amount, payment_type)? {
            Ok(parts) => parts,
            Err(refused) => return Ok(refused),
        };
        if is_negative(&parts.principal_paid) || is_negative(&parts.interest_paid) || is_negative(&parts.fee_paid) {
            return Ok(TxResult::LimitExceeded);
        }

        let deltas = lm::loan_payment_deltas(&vault, &parts)?;
        let assets_available_before = number_field(&vault, "AssetsAvailable");
        let assets_total_before = number_field(&vault, "AssetsTotal");
        let paid_to_vault_raw = arith(parts.principal_paid.add(parts.interest_paid, AMBIENT))?;
        let paid_to_vault = round_to_asset(asset, paid_to_vault_raw, vault_scale, Rounding::Downward)?;
        let paid_to_broker = parts.fee_paid;

        let debt_total = lm::adjust_imprecise_number(debt_total, deltas.debt_total_delta.negated(), asset, vault_scale)?;
        set_number_field(&mut broker, "DebtTotal", debt_total, Soe::Default);
        set_number_field(&mut vault, "AssetsAvailable", arith(assets_available_before.add(paid_to_vault, AMBIENT))?, Soe::Default);
        set_number_field(&mut vault, "AssetsTotal", arith(assets_total_before.add(deltas.assets_total_delta, AMBIENT))?, Soe::Default);
        if !send_fee_to_owner {
            set_number_field(&mut broker, "CoverAvailable", arith(cover_available.add(paid_to_broker, AMBIENT))?, Soe::Default);
        }

        // associateAsset on the three (:566-568), then the vault's totals
        // are read back as stored.
        for (key, obj, format) in [(loan_key, &mut loan, &LOAN), (broker_key, &mut broker, &LOAN_BROKER), (vault_key, &mut vault, &VAULT)] {
            let stored = store(sandbox, key, obj, format, Some(asset))?;
            if stored != TxResult::Success {
                return Ok(stored);
            }
        }
        let assets_available_after = number_field(&vault, "AssetsAvailable");
        let assets_total_after = number_field(&vault, "AssetsTotal");
        if assets_available_after == assets_available_before {
            // Too small a payment to move the vault's balance.
            return Ok(TxResult::PrecisionLoss);
        }
        if !deltas.assets_total_delta.is_zero() && assets_total_after == assets_total_before {
            return Ok(TxResult::PrecisionLoss);
        }
        if deltas.assets_total_delta.is_zero() && assets_total_after != assets_total_before {
            return Ok(TxResult::TecInternal);
        }
        if assets_total_after < assets_available_after {
            return Ok(TxResult::TecInternal);
        }

        let model = ConservationModel::of(sandbox);
        let balances_before = conservation_balances(sandbox, &account, &vault_pseudo, &broker_payee, &asset, model)?;

        // fixCleanup3_4_0: the payees are not asked for authorization.
        let skip_payee_auth = rules.fix_cleanup_3_4_0;
        if !skip_payee_auth && !paid_to_vault.is_zero() {
            let authorized = holding::require_auth(sandbox, &asset, &vault_pseudo, AuthType::StrongAuth);
            if authorized != TxResult::Success {
                return Ok(authorized);
            }
        }
        if !paid_to_broker.is_zero() {
            if broker_payee == account {
                // The submitter pays itself the fee: it needs a holding.
                let Some(payee_root) = root_of(sandbox, &broker_payee) else { return Ok(TxResult::TefBadLedger) };
                let held = holding::add_empty_holding(sandbox, &broker_payee, xrp_balance(&payee_root), &asset);
                if held != TxResult::Success && held != TxResult::Duplicate {
                    return Ok(held);
                }
            }
            if !skip_payee_auth {
                let authorized = holding::require_auth(sandbox, &asset, &broker_payee, AuthType::StrongAuth);
                if authorized != TxResult::Success {
                    return Ok(authorized);
                }
            }
        }

        let sent = holding::account_send_multi(
            sandbox,
            &account,
            &asset,
            &[(vault_pseudo, paid_to_vault), (broker_payee, paid_to_broker)],
            true,
            AMBIENT,
        )?;
        if sent != TxResult::Success {
            return Ok(sent);
        }

        if let Some(before) = balances_before {
            if let Some(after) = conservation_balances(sandbox, &account, &vault_pseudo, &broker_payee, &asset, model)? {
                conservation_arithmetic(&before, &after)?;
            }
        }
        Ok(TxResult::Success)
    }
}

impl Transactor for LoanPayTransactor {
    /// `LoanPay::preflight` (LoanPay.cpp:87-110): a zero sfLoanID is
    /// temINVALID, a non-positive amount temBAD_AMOUNT, more than one of
    /// the three payment flags temINVALID_FLAG.
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if loan_id(tx).is_none_or(|id| is_zero_key(&id)) {
            return TxResult::InvalidTx;
        }
        match tx.fields.get("Amount").and_then(tx_amount) {
            Some(amount) if positive(&amount) => {}
            _ => return TxResult::BadAmount,
        }
        let set = tx_flags(tx) & (TF_LOAN_LATE_PAYMENT | TF_LOAN_FULL_PAYMENT | TF_LOAN_OVERPAYMENT);
        if set.count_ones() > 1 {
            return TxResult::InvalidFlag;
        }
        TxResult::Success
    }

    fn preflight_in(&self, tx: &TxFields, rules: &Sandbox) -> TxResult {
        // `checkExtraFeatures` (:75-79), `getFlagsMask` (:81-85).
        gate(tx, rules, lh::check_lending_protocol_dependencies(rules, tx), TF_LOAN_PAY_MASK).unwrap_or_else(|| self.preflight(tx))
    }

    /// `LoanPay::calculateBaseFee`'s throw (see [`loan_pay_base_fee`]), then
    /// `LoanPay::preclaim` (LoanPay.cpp:228-329).
    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        if loan_pay_base_fee(sandbox, tx, 10).is_err() {
            return TxResult::TefException;
        }
        let account = tx.account;
        let Some(amount) = tx.fields.get("Amount").and_then(tx_amount) else { return TxResult::TefInternal };
        let Some(loan) = loan_id(tx).and_then(|id| read_loan(sandbox, &id)) else { return TxResult::NoEntry };
        if account_of(&loan, "Borrower") != Some(account) {
            return TxResult::NoPermission;
        }
        if tx_flags(tx) & TF_LOAN_OVERPAYMENT != 0 && obj_flags(&loan) & LSF_LOAN_OVERPAYMENT == 0 {
            // The loan does not take overpayments.
            return if enabled(sandbox, am::FIX_CLEANUP_3_1_3) { TxResult::NoPermission } else { TxResult::InvalidFlag };
        }
        if uint(&loan, "PaymentRemaining") == 0 || number_field(&loan, "PrincipalOutstanding").is_zero() {
            return TxResult::Killed;
        }
        let Some(broker) = mpt::key_field(&loan, "LoanBrokerID").and_then(|id| read_broker(sandbox, &id)) else {
            return TxResult::TefBadLedger;
        };
        let Some((_, vault)) = vault_of(sandbox, &broker) else { return TxResult::TefBadLedger };
        let (Some(asset), Some(vault_pseudo)) = (vault_asset(&vault), account_of(&vault, "Account")) else {
            return TxResult::TefInternal;
        };
        if amount.asset != asset {
            return TxResult::WrongAsset;
        }
        let frozen = holding::check_frozen(sandbox, &account, &asset);
        if frozen != TxResult::Success {
            return frozen;
        }
        let receivable = holding::check_deep_frozen(sandbox, &vault_pseudo, &asset);
        if receivable != TxResult::Success {
            return receivable;
        }
        let authorized = holding::require_auth(sandbox, &asset, &account, AuthType::Legacy);
        if authorized != TxResult::Success {
            return authorized;
        }
        match holding::account_holds_full(sandbox, &account, &asset, true, true) {
            Ok(balance) if balance.number() < amount.number() => TxResult::InsufficientFunds,
            Ok(_) => TxResult::Success,
            Err(_) => TxResult::TefException,
        }
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        applied(sandbox, |sb| Self::apply(tx, sb))
    }
}

// ---------------------------------------------------------------------------
// LoanDelete (LoanDelete.cpp)
// ---------------------------------------------------------------------------

/// LoanDelete (type 81): the borrower or the broker's owner removes a Loan
/// with no payments remaining (paid off, or defaulted).
///
/// Fee: the plain base fee. Reserve: the borrower's OwnerCount falls by 1.
/// Directories: the Loan leaves the broker pseudo-account's directory
/// (sfLoanBrokerNode), then the borrower's (sfOwnerNode). The broker's own
/// sfOwnerCount falls by 1, and when that was its last loan whatever is
/// left of sfDebtTotal (rounding dust) is cleared.
pub struct LoanDeleteTransactor;

impl LoanDeleteTransactor {
    /// `LoanDelete::doApply` (LoanDelete.cpp:72-136).
    fn apply(tx: &TxFields, sandbox: &mut Sandbox) -> M<TxResult> {
        let Some(loan_key) = loan_id(tx) else { return Ok(TxResult::TefBadLedger) };
        let Some(loan) = read_loan(sandbox, &loan_key) else { return Ok(TxResult::TefBadLedger) };
        let Some(borrower) = account_of(&loan, "Borrower") else { return Ok(TxResult::TefBadLedger) };
        if root_of(sandbox, &borrower).is_none() {
            return Ok(TxResult::TefBadLedger);
        }
        let Some(broker_key) = mpt::key_field(&loan, "LoanBrokerID") else { return Ok(TxResult::TefBadLedger) };
        let Some(mut broker) = read_broker(sandbox, &broker_key) else { return Ok(TxResult::TefBadLedger) };
        let Some(broker_pseudo) = account_of(&broker, "Account") else { return Ok(TxResult::TefBadLedger) };
        if vault_of(sandbox, &broker).is_none() {
            return Ok(TxResult::TefBadLedger);
        }

        owner_dir_remove(sandbox, &broker_pseudo, &loan_key, node_hint(&loan, "LoanBrokerNode"), false);
        owner_dir_remove(sandbox, &borrower, &loan_key, node_hint(&loan, "OwnerNode"), false);
        sandbox.delete(loan_key);

        // `adjustLoanBrokerOwnerCount(view, brokerSle, -1)`.
        let owner_count = (uint(&broker, "OwnerCount") as u32).saturating_sub(1);
        broker["OwnerCount"] = Value::from(owner_count);
        if owner_count == 0 {
            // The last loan: what is left of the debt rounds to zero.
            set_number_field(&mut broker, "DebtTotal", Number::ZERO, Soe::Default);
        }
        // The broker is updated without `associateAsset`; the vault is only
        // peeked (its `associateAsset` at :133 changes nothing that is stored).
        let stored = store(sandbox, broker_key, &mut broker, &LOAN_BROKER, None)?;
        if stored != TxResult::Success {
            return Ok(stored);
        }
        holding::decrease_owner_count_for_object(sandbox, &borrower, &loan, 1);
        Ok(TxResult::Success)
    }
}

impl Transactor for LoanDeleteTransactor {
    /// `LoanDelete::preflight` (LoanDelete.cpp:27-34).
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if loan_id(tx).is_none_or(|id| is_zero_key(&id)) {
            return TxResult::InvalidTx;
        }
        TxResult::Success
    }

    fn preflight_in(&self, tx: &TxFields, rules: &Sandbox) -> TxResult {
        // `checkExtraFeatures` (:21-25); no flags of its own.
        gate(tx, rules, lh::check_lending_protocol_dependencies(rules, tx), TF_UNIVERSAL_MASK).unwrap_or_else(|| self.preflight(tx))
    }

    /// `LoanDelete::preclaim` (LoanDelete.cpp:36-70).
    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let Some(loan) = loan_id(tx).and_then(|id| read_loan(sandbox, &id)) else { return TxResult::NoEntry };
        if uint(&loan, "PaymentRemaining") > 0 {
            return TxResult::HasObligations;
        }
        let Some(broker) = mpt::key_field(&loan, "LoanBrokerID").and_then(|id| read_broker(sandbox, &id)) else {
            return TxResult::TecInternal;
        };
        if account_of(&broker, "Owner") != Some(tx.account) && account_of(&loan, "Borrower") != Some(tx.account) {
            return TxResult::NoPermission;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        applied(sandbox, |sb| Self::apply(tx, sb))
    }
}

#[cfg(test)]
mod tests;
