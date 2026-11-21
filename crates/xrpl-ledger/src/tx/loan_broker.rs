//! The five loan-broker transactors of the LendingProtocol amendment, ported
//! from rippled 3.4.1 `src/libxrpl/tx/transactors/lending/`:
//!
//! | here | rippled | type |
//! |---|---|---|
//! | [`LoanBrokerSetTransactor`] | LoanBrokerSet.cpp | 74 |
//! | [`LoanBrokerDeleteTransactor`] | LoanBrokerDelete.cpp | 75 |
//! | [`LoanBrokerCoverDepositTransactor`] | LoanBrokerCoverDeposit.cpp | 76 |
//! | [`LoanBrokerCoverWithdrawTransactor`] | LoanBrokerCoverWithdraw.cpp | 77 |
//! | [`LoanBrokerCoverClawbackTransactor`] | LoanBrokerCoverClawback.cpp | 78 |
//!
//! Each is refused temDISABLED unless LendingProtocol is enabled in the
//! ledger (transactions.macro:887-960, `Transactor::invokePreflight`,
//! Transactor.h:640-647) AND SingleAssetVault and MPTokensV1 are
//! (`checkLendingProtocolDependencies`, each type's `checkExtraFeatures`):
//! with the amendment off — mainnet today — nothing here runs past that line.
//!
//! # The objects
//!
//! A LoanBroker (ltLOAN_BROKER, `tx::protocol::LOAN_BROKER`) hangs off one
//! Vault. It sits in TWO owner directories — its owner's (sfOwnerNode) and
//! the vault pseudo-account's (sfVaultNode) — and has a pseudo-account of
//! its own (sfAccount, an AccountRoot carrying sfLoanBrokerID) that holds
//! the first-loss capital: sfCoverAvailable is that pseudo-account's holding
//! of the vault's asset (its XRP balance, trust line or MPToken). The owner
//! is charged two reserve units — the LoanBroker and its pseudo-account
//! (LoanBrokerSet.cpp:255-259, LoanBrokerDelete.cpp:193-195). The broker's
//! own sfOwnerCount counts its live Loans and is written by the Loan
//! transactors only; here it is read (LoanBrokerDelete refuses while it is
//! not zero).
//!
//! # What stands under them
//!
//! `tx::lending_helpers` (the LendingHelpers parts the brokers call),
//! `tx::holding` (holdings, sends, freeze and authorization checks,
//! `canWithdraw` / `doWithdraw`), `tx::st_number` (STNumber fields,
//! `associateAsset`), `tx::protocol` (limits, the LoanBroker template),
//! `tx::misc::create_pseudo_account`, `tx::vault_helpers` (the vault's kind).
//!
//! # Amendment arms
//!
//! - fixCleanup3_2_0, 3_3_0, 3_4_0 (all on at mainnet): both arms of every
//!   branch, as rippled 3.4.1 has them;
//! - LendingProtocolV1_1 (off at mainnet): LoanBrokerSet attaches a broker to
//!   a closed-ended vault only (LoanBrokerSet.cpp:149-161);
//! - fixCleanup3_5_0 (3.5.0-rc1, off at mainnet, on at devnet):
//!   LoanBrokerSet's sfDebtMaximum range check, LoanBrokerDelete's transfer /
//!   authorization checks on the cover payout, its owner count released
//!   before the payout, and the owner's holding created for it.
//!
//! NOT ported: LendingProtocolV1_2 (`Supported::No` in 3.5.0-rc1, enabled on
//! no network). It would hook in at: LoanBrokerSet's sfDomainID /
//! tfLoanBrokerPrivate / lsfLoanBrokerPrivate (3.5.0-rc1
//! LoanBrokerSet.cpp:34-38, :64-94, :123-132, :187-188, :216-223, :277-288,
//! :360-366 — without V1_2 a LoanBrokerSet carrying sfDomainID is
//! temDISABLED and its flag mask is the universal one), and at every
//! `getVaultVersion(vault) == VaultVersion::FixedPrecision` branch of
//! LoanBrokerSet (:233-235), LoanBrokerCoverDeposit (:108-114, :135-137,
//! :172-179), LoanBrokerCoverWithdraw (:107-128, :253-256) and
//! LoanBrokerCoverClawback (:202-209, :322-329). featureSponsor (reserve
//! sponsors) is not ported either, as in `tx::holding`.
//!
//! # Rounding
//!
//! Outside the guards the helpers install themselves, `Number` arithmetic
//! and `STAmount = Number` conversions run at the thread's default,
//! ToNearest ([`AMBIENT`]).
//!
//! # Exceptions
//!
//! An `Err` of a helper is a C++ exception leaving the transactor:
//! tefEXCEPTION (applySteps.cpp:485-488), nothing applied.
//!
//! # Not the transactors' job
//!
//! The loan-broker invariant (ValidLoanBroker, LoanBrokerInvariant.cpp) is
//! not ported; each of the five has empty `visitInvariantEntry` /
//! `finalizeInvariants` of its own. What it enforces after every transaction
//! that touches a broker, its pseudo-account or the pseudo-account's holding:
//! sfCoverAvailable equal to the pseudo-account's balance of the vault's
//! asset (:262-289; the upper bound under fixCleanup3_1_3 and not on
//! LoanBrokerDelete), sfDebtTotal and sfCoverAvailable not negative, a
//! sfLoanSequence that never falls, a zero-OwnerCount broker whose
//! pseudo-account directory is one page with at most one trust line or
//! MPToken, and — LendingProtocolV1_1 — a broker deleted only by
//! LoanBrokerDelete with zero OwnerCount and a DebtTotal that rounds to zero.
//! The five transactors keep all of that by construction (cover and holding
//! always move by the same amount), so no path was found here where
//! rippled's final result is the invariant's; were the two ever to differ
//! on a ledger (a holding changed behind the broker's back), rippled would
//! answer tecINVARIANT_FAILED where this engine returns the transactor's
//! result.

use crate::flow::st_amount::StError;
use crate::ledger::amendments::{self as am, enabled};
use crate::ledger::directory::{owner_dir_insert, owner_dir_remove};
use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{Transactor, TxFields, TxResult};
use crate::tx::holding;
use crate::tx::lending_helpers::{self as lh, gate};
use crate::tx::mpt::{self, AuthType, HeldAsset};
use crate::tx::number::{Number, Rounding};
use crate::tx::offer::{json_at, put_json};
use crate::tx::protocol::{
    tenth_bips_of_value, Soe, VaultKind, LOAN_BROKER, MAX_COVER_RATE, MAX_DATA_PAYLOAD_LENGTH, MAX_MANAGEMENT_FEE_RATE,
    TF_UNIVERSAL_MASK,
};
use crate::tx::st_number::{associate_asset, number_field, number_from_json, round_to_asset, round_to_scale, scale, set_number_field, Amount};
use crate::tx::vault::{
    account_of, blob_len, credential_fields_ok, has, is_zero_key, less, obj_flags, positive, prior_balance, seq_proxy_value,
    tx_amount, withdraw_destination, AMBIENT,
};
use crate::tx::vault_helpers::{self as vh, vault_asset};
use serde_json::Value;
use xrpl_core::types::Hash256;

// ---------------------------------------------------------------------------
// Shared plumbing
// ---------------------------------------------------------------------------

const LSF_ALLOW_TRUST_LINE_CLAWBACK: u64 = 0x8000_0000;
const LSF_NO_FREEZE: u64 = 0x0020_0000;

/// `tx[~sfLoanBrokerID]`.
fn broker_id(tx: &TxFields) -> Option<Hash256> {
    mpt::key_field(&tx.fields, "LoanBrokerID")
}

/// `view.read(keylet::loanBroker(id))`: the object at the key, when it is a
/// LoanBroker (a keylet read of another type is null).
fn read_broker(sandbox: &Sandbox, id: &Hash256) -> Option<Value> {
    json_at(sandbox, id).filter(|o| o.get("LedgerEntryType").and_then(|t| t.as_str()) == Some("LoanBroker"))
}

/// `view.read(keylet::vault(id))`.
fn read_vault(sandbox: &Sandbox, id: &Hash256) -> Option<Value> {
    json_at(sandbox, id).filter(|o| o.get("LedgerEntryType").and_then(|t| t.as_str()) == Some("Vault"))
}

/// `view.read(keylet::vault(broker->at(sfVaultID)))`.
fn vault_of(sandbox: &Sandbox, broker: &Value) -> Option<Value> {
    read_vault(sandbox, &mpt::key_field(broker, "VaultID")?)
}

/// An unsigned integer field of a ledger object, zero when absent.
fn uint_field(obj: &Value, field: &str) -> u64 {
    obj.get(field).and_then(|v| v.as_u64()).unwrap_or(0)
}

/// A directory page hint (sfOwnerNode, sfVaultNode): a hex string in the
/// engine's JSON.
fn node_hint(obj: &Value, field: &str) -> Option<u64> {
    obj.get(field).and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| u64::from_str_radix(s, 16).ok())))
}

/// `view.update(broker)` and `associateAsset(*broker, vaultAsset)`
/// (STTakesAsset.cpp:13-50): the broker's asset-valued Number fields
/// (sfDebtTotal, sfDebtMaximum, sfCoverAvailable) rounded to the asset's
/// precision, SoeDefault fields at their default dropped, the object stored.
/// rippled calls `associateAsset` on the SLE it has already handed to the
/// view; the stored object is the rounded one. An amount out of the asset's
/// range is rippled's throw there: tefEXCEPTION.
fn store_broker(sandbox: &mut Sandbox, key: Hash256, broker: &mut Value, asset: HeldAsset) -> TxResult {
    if associate_asset(broker, asset, AMBIENT).is_err() {
        return TxResult::TefException;
    }
    if LOAN_BROKER.finish(broker).is_err() {
        return TxResult::TefInternal;
    }
    put_json(sandbox, key, broker);
    TxResult::Success
}

/// `STAmount{asset, number}` at the thread's rounding mode.
fn amount_of(asset: HeldAsset, n: Number) -> Result<Amount, StError> {
    Amount::from_number(asset, n, AMBIENT)
}

fn arith<T>(r: Result<T, crate::tx::number::NumberError>) -> Result<T, StError> {
    r.map_err(|_| StError::Overflow)
}

// ---------------------------------------------------------------------------
// LoanBrokerSet (LoanBrokerSet.cpp)
// ---------------------------------------------------------------------------

/// LoanBrokerSet (type 74): the owner of a Vault creates a LoanBroker on it
/// — the broker, its pseudo-account and the pseudo-account's holding of the
/// vault's asset — or, naming sfLoanBrokerID, changes an existing broker's
/// sfData and sfDebtMaximum.
///
/// Fee: the plain base fee (no `calculateBaseFee` override). Reserve: a new
/// broker raises the owner's OwnerCount by 2, for the LoanBroker and its
/// pseudo-account (LoanBrokerSet.cpp:255-259); the pseudo-account's trust
/// line or MPToken is counted on the pseudo-account. Directories: the broker
/// is linked into the owner's directory (sfOwnerNode) and then into the
/// vault pseudo-account's (sfVaultNode) (:250-253).
pub struct LoanBrokerSetTransactor;

impl LoanBrokerSetTransactor {
    /// `LoanBrokerSet::checkExtraFeatures` (LoanBrokerSet.cpp:30-34; the
    /// sfDomainID line is 3.5.0-rc1's :34-38). sfDomainID is a
    /// LendingProtocolV1_2 field: 3.4.1's transaction format does not have
    /// it at all, and 3.5.0-rc1 refuses it while V1_2 is off — which it is
    /// on every network.
    fn extra_features(tx: &TxFields, rules: &Sandbox) -> bool {
        if has(tx, "DomainID") {
            return false;
        }
        lh::check_lending_protocol_dependencies(rules, tx)
    }

    /// `getValueFields()` (LoanBrokerSet.cpp:85-91): sfDebtMaximum must be
    /// representable in the vault's asset — whole units for XRP and MPT,
    /// sixteen digits for an IOU: tecPRECISION_LOSS. A value the asset
    /// cannot hold at all (XRP above 1e17 drops) throws in 3.4.1
    /// (tefEXCEPTION); fixCleanup3_5_0 refuses it tecPRECISION_LOSS
    /// (3.5.0-rc1 :225-242).
    fn check_value_fields(tx: &TxFields, sandbox: &Sandbox, asset: HeldAsset) -> TxResult {
        let Some(value) = tx.fields.get("DebtMaximum").and_then(number_from_json) else { return TxResult::Success };
        match amount_of(asset, value) {
            Ok(amount) if amount.number() == value => TxResult::Success,
            Ok(_) => TxResult::PrecisionLoss,
            Err(_) if am::fix_cleanup_3_5_0(sandbox) => TxResult::PrecisionLoss,
            Err(_) => TxResult::TefException,
        }
    }
}

impl Transactor for LoanBrokerSetTransactor {
    /// `LoanBrokerSet::preflight` (LoanBrokerSet.cpp:36-83); every refusal
    /// is temINVALID.
    fn preflight(&self, tx: &TxFields) -> TxResult {
        // sfData: empty is allowed (it clears the field), else at most 256 bytes.
        if blob_len(tx, "Data").is_some_and(|len| len > MAX_DATA_PAYLOAD_LENGTH) {
            return TxResult::InvalidTx;
        }
        let uint = |field: &str| tx.fields.get(field).map(|v| v.as_u64());
        // `validNumericRange(value, max)`: absent is fine.
        let in_range = |field: &str, max: u64| match uint(field) {
            None => true,
            Some(value) => value.is_some_and(|v| v <= max),
        };
        if !in_range("ManagementFeeRate", MAX_MANAGEMENT_FEE_RATE as u64) {
            return TxResult::InvalidTx;
        }
        if !in_range("CoverRateMinimum", MAX_COVER_RATE as u64) {
            return TxResult::InvalidTx;
        }
        if !in_range("CoverRateLiquidation", MAX_COVER_RATE as u64) {
            return TxResult::InvalidTx;
        }
        // 0 <= DebtMaximum <= kMaxMpTokenAmount.
        if let Some(maximum) = tx.fields.get("DebtMaximum") {
            let Some(maximum) = number_from_json(maximum) else { return TxResult::InvalidTx };
            let Ok(largest) = Number::from_parts(false, mpt::MAX_MPT_AMOUNT as u128, 0, AMBIENT) else { return TxResult::InvalidTx };
            if (maximum.negative && !maximum.is_zero()) || maximum > largest {
                return TxResult::InvalidTx;
            }
        }
        if has(tx, "LoanBrokerID") {
            // The fixed fields cannot be given when changing a broker.
            if has(tx, "ManagementFeeRate") || has(tx, "CoverRateMinimum") || has(tx, "CoverRateLiquidation") {
                return TxResult::InvalidTx;
            }
            if broker_id(tx).is_none_or(|id| is_zero_key(&id)) {
                return TxResult::InvalidTx;
            }
        }
        // sfVaultID is required by the format; zero is refused here.
        if mpt::key_field(&tx.fields, "VaultID").is_none_or(|id| is_zero_key(&id)) {
            return TxResult::InvalidTx;
        }
        // Both cover rates zero, or neither.
        let zero = |field: &str| uint(field).flatten().unwrap_or(0) == 0;
        if zero("CoverRateMinimum") != zero("CoverRateLiquidation") {
            return TxResult::InvalidTx;
        }
        TxResult::Success
    }

    fn preflight_in(&self, tx: &TxFields, rules: &Sandbox) -> TxResult {
        // 3.5.0-rc1's `getFlagsMask` (:123-132) is tfLoanBrokerSetMask only
        // under LendingProtocolV1_2; otherwise the universal mask, as 3.4.1.
        gate(tx, rules, Self::extra_features(tx, rules), TF_UNIVERSAL_MASK).unwrap_or_else(|| self.preflight(tx))
    }

    /// `LoanBrokerSet::preclaim` (LoanBrokerSet.cpp:93-186).
    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let account = tx.account;
        let Some(vault_id) = mpt::key_field(&tx.fields, "VaultID") else { return TxResult::TefInternal };
        let Some(vault) = read_vault(sandbox, &vault_id) else { return TxResult::NoEntry };
        let Some(asset) = vault_asset(&vault) else { return TxResult::TefInternal };
        if account_of(&vault, "Owner") != Some(account) {
            return TxResult::NoPermission;
        }
        if let Some(id) = broker_id(tx) {
            // Changing an existing broker.
            let Some(broker) = read_broker(sandbox, &id) else { return TxResult::NoEntry };
            // The vault of a broker cannot be changed.
            if mpt::key_field(&broker, "VaultID") != Some(vault_id) {
                return TxResult::NoPermission;
            }
            if account_of(&broker, "Owner") != Some(account) {
                return TxResult::NoPermission;
            }
            if let Some(maximum) = tx.fields.get("DebtMaximum").and_then(number_from_json) {
                // Not below what is already lent; zero means no limit.
                if !maximum.is_zero() && maximum < number_field(&broker, "DebtTotal") {
                    return TxResult::LimitExceeded;
                }
            }
        } else {
            // LendingProtocolV1_1: only a closed-ended vault may host a
            // broker — the lending protocol relies on its Subscription /
            // Investment / Redemption phases.
            if enabled(sandbox, am::LENDING_PROTOCOL_V1_1) && vh::vault_kind(&vault) != VaultKind::ClosedEnded {
                return TxResult::NoPermission;
            }
            let holdable = holding::can_add_holding(sandbox, &asset);
            if holdable != TxResult::Success {
                return holdable;
            }
            let Some(vault_account) = account_of(&vault, "Account") else { return TxResult::TefInternal };
            let frozen = holding::check_frozen(sandbox, &vault_account, &asset);
            if frozen != TxResult::Success {
                return frozen;
            }
        }
        Self::check_value_fields(tx, sandbox, asset)
    }

    /// `LoanBrokerSet::doApply` (LoanBrokerSet.cpp:188-295).
    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let data = tx.fields.get("Data").and_then(|d| d.as_str()).map(|d| d.to_uppercase());
        let maximum = tx.fields.get("DebtMaximum").and_then(number_from_json);
        if let Some(id) = broker_id(tx) {
            // Change an existing broker.
            let Some(mut broker) = read_broker(sandbox, &id) else { return TxResult::TefBadLedger };
            let Some(vault) = vault_of(sandbox, &broker) else { return TxResult::TecInternal };
            let Some(asset) = vault_asset(&vault) else { return TxResult::TecInternal };
            // An empty sfData / a zero sfDebtMaximum is the default: the
            // field goes (SoeDefault).
            if let Some(data) = data {
                broker["Data"] = Value::String(data);
            }
            if let Some(maximum) = maximum {
                set_number_field(&mut broker, "DebtMaximum", maximum, Soe::Default);
            }
            return store_broker(sandbox, id, &mut broker, asset);
        }

        // Create a broker on the vault.
        let Some(vault_id) = mpt::key_field(&tx.fields, "VaultID") else { return TxResult::TefBadLedger };
        let Some(vault) = read_vault(sandbox, &vault_id) else { return TxResult::TefBadLedger };
        let (Some(vault_account), Some(asset)) = (account_of(&vault, "Account"), vault_asset(&vault)) else {
            return TxResult::TefBadLedger;
        };
        let account = tx.account;
        let sequence = seq_proxy_value(tx);
        let Some(owner) = json_at(sandbox, &keylet::account_root_key(&account)) else { return TxResult::TefBadLedger };
        let prior = prior_balance(sandbox, tx);
        let broker_key = keylet::loan_broker_key(&account, sequence);

        // dirLink: the owner's directory, then the vault pseudo-account's.
        let owner_node = owner_dir_insert(sandbox, &account, &broker_key);
        let vault_node = owner_dir_insert(sandbox, &vault_account, &broker_key);

        // The LoanBroker and its pseudo-account count as two owned objects;
        // the reserve is judged with both already counted.
        let owner_count = uint_field(&owner, "OwnerCount") + 2;
        crate::tx::offer::owner_count_add(sandbox, &account, 2);
        if prior < crate::ledger::fees::account_reserve(sandbox, owner_count) {
            return TxResult::InsufficientReserve;
        }

        let pseudo = match crate::tx::misc::create_pseudo_account(sandbox, &broker_key, crate::tx::misc::PseudoOwner::LoanBroker) {
            Ok(pseudo) => pseudo,
            Err(refused) => return refused,
        };
        let held = holding::add_empty_holding(sandbox, &pseudo, prior, &asset);
        if held != TxResult::Success {
            return held;
        }

        let mut broker = serde_json::json!({
            "LedgerEntryType": "LoanBroker",
            "Flags": 0,
            "Sequence": sequence,
            "VaultID": hex::encode_upper(vault_id.0),
            "Owner": hex::encode(account),
            "Account": hex::encode(pseudo),
            "OwnerNode": format!("{owner_node:x}"),
            "VaultNode": format!("{vault_node:x}"),
            // The broker's loans are numbered from 1.
            "LoanSequence": 1,
        });
        if let Some(data) = data {
            broker["Data"] = Value::String(data);
        }
        for field in ["ManagementFeeRate", "CoverRateMinimum", "CoverRateLiquidation"] {
            if let Some(rate) = tx.fields.get(field) {
                broker[field] = rate.clone();
            }
        }
        if let Some(maximum) = maximum {
            set_number_field(&mut broker, "DebtMaximum", maximum, Soe::Default);
        }
        store_broker(sandbox, broker_key, &mut broker, asset)
    }
}

// ---------------------------------------------------------------------------
// LoanBrokerCoverDeposit (LoanBrokerCoverDeposit.cpp)
// ---------------------------------------------------------------------------

/// LoanBrokerCoverDeposit (type 76): the broker's owner adds first-loss
/// capital — the vault's asset moves from the owner to the broker's
/// pseudo-account and sfCoverAvailable rises by the same amount.
///
/// Fee: the plain base fee. No reserve change (the pseudo-account's holding
/// exists since LoanBrokerSet).
pub struct LoanBrokerCoverDepositTransactor;

/// The amount a cover deposit moves (LoanBrokerCoverDeposit.cpp:106-114,
/// :157-165): under fixCleanup3_2_0 sfAmount floored (Downward) to the scale
/// of sfCoverAvailable, so that `sfCoverAvailable += amount` cannot round to
/// more than the depositor paid; before the fix sfAmount as it is.
fn cover_deposit_amount(sandbox: &Sandbox, broker: &Value, asset: HeldAsset, amount: Amount) -> Result<Amount, StError> {
    if !am::fix_cleanup_3_2_0(sandbox) {
        return Ok(amount);
    }
    let cover_scale = scale(number_field(broker, "CoverAvailable"), asset, AMBIENT)?;
    round_to_scale(amount, cover_scale, Rounding::Downward)
}

impl Transactor for LoanBrokerCoverDepositTransactor {
    /// `LoanBrokerCoverDeposit::preflight` (LoanBrokerCoverDeposit.cpp:27-41).
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if broker_id(tx).is_none_or(|id| is_zero_key(&id)) {
            return TxResult::InvalidTx;
        }
        // Positive, and a legal amount (`isLegalNet`: an XRP amount in range).
        match tx.fields.get("Amount").and_then(tx_amount) {
            Some(amount) if positive(&amount) => TxResult::Success,
            _ => TxResult::BadAmount,
        }
    }

    fn preflight_in(&self, tx: &TxFields, rules: &Sandbox) -> TxResult {
        // `checkExtraFeatures` (:21-25).
        gate(tx, rules, lh::check_lending_protocol_dependencies(rules, tx), TF_UNIVERSAL_MASK).unwrap_or_else(|| self.preflight(tx))
    }

    /// `LoanBrokerCoverDeposit::preclaim` (LoanBrokerCoverDeposit.cpp:43-134).
    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let fix320 = am::fix_cleanup_3_2_0(sandbox);
        let fix330 = am::fix_cleanup_3_3_0(sandbox);
        let account = tx.account;
        let (Some(id), Some(amount)) = (broker_id(tx), tx.fields.get("Amount").and_then(tx_amount)) else {
            return TxResult::TefInternal;
        };
        let Some(broker) = read_broker(sandbox, &id) else { return TxResult::NoEntry };
        if account_of(&broker, "Owner") != Some(account) {
            return TxResult::NoPermission;
        }
        let Some(vault) = vault_of(sandbox, &broker) else { return TxResult::TefBadLedger };
        let Some(asset) = vault_asset(&vault) else { return TxResult::TefInternal };
        if amount.asset != asset {
            return TxResult::WrongAsset;
        }
        let Some(pseudo) = account_of(&broker, "Account") else { return TxResult::TefInternal };
        // A non-transferable asset cannot be deposited.
        let transferable = holding::can_transfer(sandbox, &asset, &account, &pseudo);
        if transferable != TxResult::Success {
            return transferable;
        }
        if fix330 {
            let frozen = holding::check_deposit_freeze(sandbox, &account, &pseudo, &asset);
            if frozen != TxResult::Success {
                return frozen;
            }
        } else {
            let source = holding::check_frozen(sandbox, &account, &asset);
            if source != TxResult::Success {
                return source;
            }
            let destination = holding::check_deep_frozen(sandbox, &pseudo, &asset);
            if destination != TxResult::Success {
                return destination;
            }
        }
        // An asset the depositor is not authorized to hold cannot be deposited.
        let authorized = holding::require_auth(sandbox, &asset, &account, AuthType::StrongAuth);
        if authorized != TxResult::Success {
            return authorized;
        }
        // Dust below the cover's scale is refused here rather than in do_apply.
        let Ok(rounded) = cover_deposit_amount(sandbox, &broker, asset, amount) else { return TxResult::TefException };
        if fix320 && rounded.is_zero() {
            return TxResult::PrecisionLoss;
        }
        let Ok(balance) = holding::account_holds_full(sandbox, &account, &asset, true, true) else {
            return TxResult::TefException;
        };
        if less(&balance, &rounded) {
            return TxResult::InsufficientFunds;
        }
        TxResult::Success
    }

    /// `LoanBrokerCoverDeposit::doApply` (LoanBrokerCoverDeposit.cpp:136-189).
    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let (Some(id), Some(requested)) = (broker_id(tx), tx.fields.get("Amount").and_then(tx_amount)) else {
            return TxResult::TecInternal;
        };
        let Some(mut broker) = read_broker(sandbox, &id) else { return TxResult::TecInternal };
        let Some(vault) = vault_of(sandbox, &broker) else { return TxResult::TecInternal };
        let (Some(asset), Some(pseudo)) = (vault_asset(&vault), account_of(&broker, "Account")) else {
            return TxResult::TecInternal;
        };
        // Rounded as in preclaim: the same amount drives the transfer and
        // the cover.
        let Ok(amount) = cover_deposit_amount(sandbox, &broker, asset, requested) else { return TxResult::TefException };
        // Preclaim refused a zero; one here is tecINTERNAL.
        if amount.is_zero() {
            return TxResult::TecInternal;
        }
        let paid = holding::account_send(sandbox, &tx.account, &pseudo, &amount, true);
        if paid != TxResult::Success {
            return paid;
        }
        let Ok(cover) = number_field(&broker, "CoverAvailable").add(amount.number(), AMBIENT) else {
            return TxResult::TefException;
        };
        set_number_field(&mut broker, "CoverAvailable", cover, Soe::Default);
        store_broker(sandbox, id, &mut broker, asset)
    }
}

// ---------------------------------------------------------------------------
// LoanBrokerCoverWithdraw (LoanBrokerCoverWithdraw.cpp)
// ---------------------------------------------------------------------------

/// LoanBrokerCoverWithdraw (type 77): the broker's owner takes first-loss
/// capital out, to itself or to sfDestination, down to the minimum cover the
/// broker's debt requires (sfDebtTotal × sfCoverRateMinimum).
///
/// Fee: the plain base fee. Reserve: a withdrawal to oneself may create the
/// holding the assets land in (`doWithdraw` → `addEmptyHolding`).
pub struct LoanBrokerCoverWithdrawTransactor;

/// The cover a broker must keep (LoanBrokerCoverWithdraw.cpp:172-186):
/// under fixCleanup3_2_0 `minimumBrokerCover` — rounded up at the VAULT's
/// scale; before it the same product rounded up at the scale of sfDebtTotal
/// itself, all under one Upward guard.
fn withdraw_minimum_cover(sandbox: &Sandbox, broker: &Value, vault: &Value, asset: HeldAsset) -> Result<Number, StError> {
    let debt_total = number_field(broker, "DebtTotal");
    let rate = uint_field(broker, "CoverRateMinimum") as u32;
    if am::fix_cleanup_3_2_0(sandbox) {
        return lh::minimum_broker_cover(debt_total, rate, vault);
    }
    let up = Rounding::Upward;
    let raw = arith(tenth_bips_of_value(debt_total, rate, up))?;
    round_to_asset(asset, raw, scale(debt_total, asset, up)?, up)
}

impl Transactor for LoanBrokerCoverWithdrawTransactor {
    /// `LoanBrokerCoverWithdraw::preflight` (LoanBrokerCoverWithdraw.cpp:36-61),
    /// less the credential fields (they need the rules:
    /// [`Self::preflight_in`]).
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if broker_id(tx).is_none_or(|id| is_zero_key(&id)) {
            return TxResult::InvalidTx;
        }
        match tx.fields.get("Amount").and_then(tx_amount) {
            Some(amount) if positive(&amount) => {}
            _ => return TxResult::BadAmount,
        }
        if has(tx, "Destination") && withdraw_destination(tx).is_none_or(|d| d == [0u8; 20]) {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preflight_in(&self, tx: &TxFields, rules: &Sandbox) -> TxResult {
        // `checkExtraFeatures` (:26-34): sfCredentialIDs needs Credentials
        // and fixCleanup3_4_0.
        let extra = lh::check_lending_protocol_dependencies(rules, tx)
            && (!has(tx, "CredentialIDs") || (enabled(rules, am::CREDENTIALS) && am::fix_cleanup_3_4_0(rules)));
        if let Some(refused) = gate(tx, rules, extra, TF_UNIVERSAL_MASK) {
            return refused;
        }
        let basic = self.preflight(tx);
        if basic != TxResult::Success {
            return basic;
        }
        // `credentials::checkFields` (:57-58).
        if !credential_fields_ok(tx, rules) {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    /// `LoanBrokerCoverWithdraw::preclaim` (LoanBrokerCoverWithdraw.cpp:63-206).
    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let fix320 = am::fix_cleanup_3_2_0(sandbox);
        let fix330 = am::fix_cleanup_3_3_0(sandbox);
        let fix340 = am::fix_cleanup_3_4_0(sandbox);
        let account = tx.account;
        let (Some(id), Some(amount), Some(dst)) =
            (broker_id(tx), tx.fields.get("Amount").and_then(tx_amount), withdraw_destination(tx))
        else {
            return TxResult::TefInternal;
        };
        // Before anything else: never into a pseudo-account.
        if crate::tx::misc::is_pseudo_account(sandbox, &dst) {
            return TxResult::TecPseudoAccount;
        }
        let Some(broker) = read_broker(sandbox, &id) else { return TxResult::NoEntry };
        if account_of(&broker, "Owner") != Some(account) {
            return TxResult::NoPermission;
        }
        let Some(vault) = vault_of(sandbox, &broker) else { return TxResult::TefBadLedger };
        let Some(asset) = vault_asset(&vault) else { return TxResult::TefInternal };
        if amount.asset != asset {
            return TxResult::WrongAsset;
        }
        match lh::can_apply_to_broker_cover(sandbox, &broker, asset, &amount) {
            Ok(TxResult::Success) => {}
            Ok(refused) => return refused,
            Err(_) => return TxResult::TefException,
        }
        // The broker's pseudo-account is the source of funds.
        let Some(pseudo) = account_of(&broker, "Account") else { return TxResult::TefInternal };
        // fixCleanup3_2_0: a cover withdrawal is a recovery path — the
        // asset's lsfMPTCanTransfer is not consulted, so an issuer cannot
        // trap a broker's first-loss capital. IOU rippling, freezes and
        // authorization still apply.
        let transferable = holding::can_transfer_waive(sandbox, &asset, &pseudo, &dst, fix320);
        if transferable != TxResult::Success {
            return transferable;
        }
        // Credentials first: canWithdraw assumes they exist.
        let credentials = crate::tx::credential::credentials_valid(sandbox, tx, &account);
        if credentials != TxResult::Success {
            return credentials;
        }
        // To a third party it is a transfer: destination, tag, deposit
        // authorization, trust limit — and the destination must already hold
        // the asset's line or MPToken.
        let mut auth = AuthType::WeakAuth;
        if account != dst {
            let allowed = holding::can_withdraw_tx(sandbox, tx);
            if allowed != TxResult::Success {
                return allowed;
            }
            auth = AuthType::StrongAuth;
        }
        let authorized = holding::require_auth(sandbox, &asset, &dst, auth);
        if authorized != TxResult::Success {
            return authorized;
        }
        // fixCleanup3_4_0: fail early when the self-withdrawal would have to
        // create a holding that cannot be created.
        if fix340 && account == dst && !holding::holding_exists(sandbox, &dst, &asset) {
            let holdable = holding::can_add_holding(sandbox, &asset);
            if holdable != TxResult::Success {
                return holdable;
            }
        }
        let issuer = holding::asset_issuer(&asset);
        if fix330 {
            let frozen = holding::check_withdraw_freeze(sandbox, &pseudo, &account, &dst, &asset);
            if frozen != TxResult::Success {
                return frozen;
            }
        } else if dst != issuer {
            // Unless sending straight to the issuer.
            let source = holding::check_frozen(sandbox, &pseudo, &asset);
            if source != TxResult::Success {
                return source;
            }
            let destination = holding::check_deep_frozen(sandbox, &dst, &asset);
            if destination != TxResult::Success {
                return destination;
            }
        }

        let cover = number_field(&broker, "CoverAvailable");
        let Ok(minimum) = withdraw_minimum_cover(sandbox, &broker, &vault, asset) else { return TxResult::TefException };
        if cover < amount.number() {
            return TxResult::InsufficientFunds;
        }
        let Ok(left) = cover.sub(amount.number(), AMBIENT) else { return TxResult::TefException };
        if left < minimum {
            return TxResult::InsufficientFunds;
        }
        // fixCleanup3_3_0: a payout to the issuer ignores freezes.
        let zero_if_frozen = !(fix330 && dst == issuer);
        if less(&holding::account_holds(sandbox, &pseudo, &asset, zero_if_frozen, true), &amount) {
            return TxResult::InsufficientFunds;
        }
        TxResult::Success
    }

    /// `LoanBrokerCoverWithdraw::doApply` (LoanBrokerCoverWithdraw.cpp:208-243).
    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let (Some(id), Some(amount), Some(dst)) =
            (broker_id(tx), tx.fields.get("Amount").and_then(tx_amount), withdraw_destination(tx))
        else {
            return TxResult::TecInternal;
        };
        let Some(mut broker) = read_broker(sandbox, &id) else { return TxResult::TecInternal };
        let Some(vault) = vault_of(sandbox, &broker) else { return TxResult::TecInternal };
        let (Some(asset), Some(pseudo)) = (vault_asset(&vault), account_of(&broker, "Account")) else {
            return TxResult::TecInternal;
        };
        let prior = prior_balance(sandbox, tx);
        let Ok(cover) = number_field(&broker, "CoverAvailable").sub(amount.number(), AMBIENT) else {
            return TxResult::TefException;
        };
        set_number_field(&mut broker, "CoverAvailable", cover, Soe::Default);
        let stored = store_broker(sandbox, id, &mut broker, asset);
        if stored != TxResult::Success {
            return stored;
        }
        holding::do_withdraw(sandbox, tx, &tx.account, &dst, &pseudo, prior, &amount)
    }
}

// ---------------------------------------------------------------------------
// LoanBrokerCoverClawback (LoanBrokerCoverClawback.cpp)
// ---------------------------------------------------------------------------

/// LoanBrokerCoverClawback (type 78): the ISSUER of the vault's asset takes
/// first-loss capital back from a broker's pseudo-account, down to the
/// minimum cover the broker's debt requires. The broker is named by
/// sfLoanBrokerID, or — for an IOU — by an sfAmount whose `issuer` is the
/// broker's pseudo-account (the trust line seen from the issuer's side). No
/// amount, or zero, means everything above the minimum; a larger amount is
/// clamped to it.
///
/// Fee: the plain base fee. No reserve change.
pub struct LoanBrokerCoverClawbackTransactor;

/// `determineBrokerID(view, tx)` (LoanBrokerCoverClawback.cpp:86-127):
/// sfLoanBrokerID when given; else the sfLoanBrokerID of the account the IOU
/// sfAmount names as issuer — tecNO_ENTRY without that account,
/// tecOBJECT_NOT_FOUND when it is not a broker's pseudo-account (another
/// kind of pseudo-account included).
fn determine_broker_id(sandbox: &Sandbox, tx: &TxFields) -> Result<Hash256, TxResult> {
    if let Some(id) = broker_id(tx) {
        return Ok(id);
    }
    // Preflight refused a missing or non-IOU amount without a broker id.
    let Some(Amount { asset: HeldAsset::Iou { issuer: maybe_pseudo, .. }, .. }) = tx.fields.get("Amount").and_then(tx_amount) else {
        return Err(TxResult::TecInternal);
    };
    let Some(root) = json_at(sandbox, &keylet::account_root_key(&maybe_pseudo)) else { return Err(TxResult::NoEntry) };
    mpt::key_field(&root, "LoanBrokerID").ok_or(TxResult::ObjectNotFound)
}

/// `determineAsset(view, account, brokerPseudoAccountID, amount)`
/// (LoanBrokerCoverClawback.cpp:129-157): the asset the transaction means.
/// An MPT amount is its own asset. An IOU amount names one end of the trust
/// line as issuer: the submitter (the real issuer) — the asset as written —
/// or the broker's pseudo-account — the same currency issued by the
/// submitter. Anyone else: tecWRONG_ASSET.
fn determine_asset(account: &[u8; 20], broker_pseudo: &[u8; 20], amount: &Amount) -> Result<HeldAsset, TxResult> {
    match amount.asset {
        HeldAsset::Iou { currency, issuer: holder } => {
            if holder == *account {
                Ok(amount.asset)
            } else if holder == *broker_pseudo {
                Ok(HeldAsset::Iou { currency, issuer: *account })
            } else {
                Err(TxResult::WrongAsset)
            }
        }
        other => Ok(other),
    }
}

/// `determineClawAmount(sleBroker, vaultAsset, amount, vaultSle, rules)`
/// (LoanBrokerCoverClawback.cpp:159-196): the most that may be clawed back
/// is sfCoverAvailable less the minimum cover — `minimumBrokerCover` under
/// fixCleanup3_2_0, before it the unrounded rate of the debt taken Upward —
/// the subtraction rounded Downward. Nothing above the minimum:
/// tecINSUFFICIENT_FUNDS. No amount or zero takes all of it; a larger amount
/// is clamped to it. The result is in the VAULT's asset whatever issuer the
/// transaction's amount named.
fn determine_claw_amount(
    sandbox: &Sandbox,
    broker: &Value,
    vault: &Value,
    asset: HeldAsset,
    amount: Option<&Amount>,
) -> Result<Result<Amount, TxResult>, StError> {
    let debt_total = number_field(broker, "DebtTotal");
    let rate = uint_field(broker, "CoverRateMinimum") as u32;
    let min_required = if am::fix_cleanup_3_2_0(sandbox) {
        lh::minimum_broker_cover(debt_total, rate, vault)?
    } else {
        arith(tenth_bips_of_value(debt_total, rate, Rounding::Upward))?
    };
    let max_claw = arith(number_field(broker, "CoverAvailable").sub(min_required, Rounding::Downward))?;
    if max_claw.is_zero() || max_claw.negative {
        return Ok(Err(TxResult::InsufficientFunds));
    }
    let claw = match amount {
        Some(amount) if !amount.is_zero() && amount.number() <= max_claw => amount.number(),
        _ => max_claw,
    };
    Ok(Ok(amount_of(asset, claw)?))
}

impl Transactor for LoanBrokerCoverClawbackTransactor {
    /// `LoanBrokerCoverClawback::preflight` (LoanBrokerCoverClawback.cpp:42-84).
    fn preflight(&self, tx: &TxFields) -> TxResult {
        // One of the two is needed.
        if !has(tx, "LoanBrokerID") && !has(tx, "Amount") {
            return TxResult::InvalidTx;
        }
        if has(tx, "LoanBrokerID") && broker_id(tx).is_none_or(|id| is_zero_key(&id)) {
            return TxResult::InvalidTx;
        }
        if let Some(amount) = tx.fields.get("Amount") {
            let Some(amount) = tx_amount(amount) else { return TxResult::BadAmount };
            // XRP has no issuer: nobody can claw it back.
            if amount.asset == HeldAsset::Xrp {
                return TxResult::BadAmount;
            }
            // Zero is fine: "all of it", down to the minimum cover.
            if amount.negative && !amount.is_zero() {
                return TxResult::BadAmount;
            }
            if !has(tx, "LoanBrokerID") {
                // Without a broker id the amount must be an IOU naming the
                // broker's pseudo-account as its issuer — not the submitter,
                // not nobody.
                let HeldAsset::Iou { issuer: holder, .. } = amount.asset else { return TxResult::InvalidTx };
                if holder == tx.account || holder == [0u8; 20] {
                    return TxResult::InvalidTx;
                }
            }
        }
        TxResult::Success
    }

    fn preflight_in(&self, tx: &TxFields, rules: &Sandbox) -> TxResult {
        // `checkExtraFeatures` (:36-40).
        gate(tx, rules, lh::check_lending_protocol_dependencies(rules, tx), TF_UNIVERSAL_MASK).unwrap_or_else(|| self.preflight(tx))
    }

    /// `LoanBrokerCoverClawback::preclaim` (LoanBrokerCoverClawback.cpp:236-334).
    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let account = tx.account;
        let id = match determine_broker_id(sandbox, tx) {
            Ok(id) => id,
            Err(refused) => return refused,
        };
        let amount = tx.fields.get("Amount").and_then(tx_amount);
        let Some(broker) = read_broker(sandbox, &id) else { return TxResult::NoEntry };
        let Some(pseudo) = account_of(&broker, "Account") else { return TxResult::TefInternal };
        let Some(vault) = vault_of(sandbox, &broker) else { return TxResult::TefBadLedger };
        let Some(asset) = vault_asset(&vault) else { return TxResult::TefInternal };
        if asset == HeldAsset::Xrp {
            return TxResult::NoPermission;
        }
        // Only the asset's issuer claws back.
        let issuer = holding::asset_issuer(&asset);
        if issuer != account {
            return TxResult::NoPermission;
        }
        if let Some(amount) = &amount {
            match determine_asset(&account, &pseudo, amount) {
                // The right issuer, the wrong asset.
                Ok(tx_asset) if tx_asset != asset => return TxResult::WrongAsset,
                Ok(_) => {}
                Err(refused) => return refused,
            }
        }
        let claw = match determine_claw_amount(sandbox, &broker, &vault, asset, amount.as_ref()) {
            // The cover is already at its minimum.
            Ok(Err(refused)) => return refused,
            Ok(Ok(claw)) => claw,
            Err(_) => return TxResult::TefException,
        };
        match lh::can_apply_to_broker_cover(sandbox, &broker, asset, &claw) {
            Ok(TxResult::Success) => {}
            Ok(refused) => return refused,
            Err(_) => return TxResult::TefException,
        }
        // The balance must really be there; it always equals sfCoverAvailable.
        if less(&holding::account_holds(sandbox, &pseudo, &asset, false, false), &claw) {
            return TxResult::TecInternal;
        }
        let Some(issuer_root) = json_at(sandbox, &keylet::account_root_key(&issuer)) else { return TxResult::TefBadLedger };
        match asset {
            // `preclaimHelper<MPTIssue>` (:215-234).
            HeldAsset::Mpt(mpt_id) => match json_at(sandbox, &keylet::mpt_issuance_key(&mpt_id)) {
                None => TxResult::ObjectNotFound,
                Some(issuance) if obj_flags(&issuance) & mpt::LSF_MPT_CAN_CLAWBACK == 0 => TxResult::NoPermission,
                Some(issuance) if mpt::issuance_issuer(&issuance) != Some(issuer) => TxResult::TecInternal,
                Some(_) => TxResult::Success,
            },
            // `preclaimHelper<Issue>` (:203-212): lsfAllowTrustLineClawback
            // and not lsfNoFreeze.
            _ => {
                let flags = obj_flags(&issuer_root);
                if flags & LSF_ALLOW_TRUST_LINE_CLAWBACK == 0 || flags & LSF_NO_FREEZE != 0 {
                    TxResult::NoPermission
                } else {
                    TxResult::Success
                }
            }
        }
    }

    /// `LoanBrokerCoverClawback::doApply` (LoanBrokerCoverClawback.cpp:336-376).
    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let Ok(id) = determine_broker_id(sandbox, tx) else { return TxResult::TecInternal };
        let amount = tx.fields.get("Amount").and_then(tx_amount);
        let Some(mut broker) = read_broker(sandbox, &id) else { return TxResult::TecInternal };
        let Some(vault) = vault_of(sandbox, &broker) else { return TxResult::TecInternal };
        let (Some(asset), Some(pseudo)) = (vault_asset(&vault), account_of(&broker, "Account")) else {
            return TxResult::TecInternal;
        };
        let claw = match determine_claw_amount(sandbox, &broker, &vault, asset, amount.as_ref()) {
            Ok(Ok(claw)) => claw,
            Ok(Err(_)) => return TxResult::TecInternal,
            Err(_) => return TxResult::TefException,
        };
        if claw.asset == HeldAsset::Xrp {
            return TxResult::TecInternal;
        }
        let Ok(cover) = number_field(&broker, "CoverAvailable").sub(claw.number(), AMBIENT) else {
            return TxResult::TefException;
        };
        set_number_field(&mut broker, "CoverAvailable", cover, Soe::Default);
        let stored = store_broker(sandbox, id, &mut broker, asset);
        if stored != TxResult::Success {
            return stored;
        }
        // From the pseudo-account to the issuer.
        holding::account_send(sandbox, &pseudo, &tx.account, &claw, true)
    }
}

// ---------------------------------------------------------------------------
// LoanBrokerDelete (LoanBrokerDelete.cpp)
// ---------------------------------------------------------------------------

/// LoanBrokerDelete (type 75): the owner removes a broker with no loans and
/// no debt — whatever cover is left goes back to the owner, then the
/// pseudo-account's holding, the pseudo-account and the LoanBroker go.
///
/// Fee: the plain base fee. Reserve: the owner's OwnerCount falls by 2, the
/// LoanBroker and its pseudo-account (LoanBrokerDelete.cpp:189-196) —
/// after the pseudo-account is erased in 3.4.1, BEFORE the cover is paid out
/// under fixCleanup3_5_0, so that the holding it may then create for the
/// owner can use the reserve the broker frees. Directories: the broker
/// leaves the owner's directory (sfOwnerNode) and the vault
/// pseudo-account's (sfVaultNode).
pub struct LoanBrokerDeleteTransactor;

impl Transactor for LoanBrokerDeleteTransactor {
    fn preflight(&self, _tx: &TxFields) -> TxResult {
        TxResult::Unsupported
    }

    fn preclaim(&self, _tx: &TxFields, _sandbox: &Sandbox) -> TxResult {
        TxResult::Unsupported
    }

    fn do_apply(&self, _tx: &TxFields, _sandbox: &mut Sandbox) -> TxResult {
        TxResult::Unsupported
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::state::LedgerState;
    use crate::tx::dispatch::apply_on_sandbox;
    use crate::tx::st_number::number_from_str;
    use serde_json::json;

    /// Owns the vault and the broker.
    const OWNER: [u8; 20] = [0x11; 20];
    /// Somebody else.
    const BOB: [u8; 20] = [0x12; 20];
    /// A third party.
    const CAROL: [u8; 20] = [0x13; 20];
    /// Issues USD and the MPT.
    const ISSUER: [u8; 20] = [0x21; 20];
    const USD_CODE: [u8; 20] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x55, 0x53, 0x44, 0, 0, 0, 0, 0];
    const USD: HeldAsset = HeldAsset::Iou { currency: USD_CODE, issuer: ISSUER };
    const BALANCE: u64 = 50_000_000;
    const FEE: u64 = 10;
    const LSF_DEFAULT_RIPPLE: u64 = 0x0080_0000;
    const LSF_GLOBAL_FREEZE: u64 = 0x0040_0000;
    const LSF_LOW_FREEZE: u64 = 0x0040_0000;
    const LSF_HIGH_FREEZE: u64 = 0x0080_0000;
    const LSF_REQUIRE_DEST_TAG: u64 = 0x0002_0000;
    /// The parent ledger's close time in every test state.
    const NOW: u32 = 1000;

    /// Mainnet's rules today plus what a loan broker needs: every
    /// fixCleanup up to 3.4.0, SingleAssetVault, LendingProtocol,
    /// MPTokensV1, PermissionedDomains, Credentials.
    const MAINNET_WITH_LENDING: [&str; 9] = [
        am::FIX_CLEANUP_3_1_3, am::FIX_CLEANUP_3_2_0, am::FIX_CLEANUP_3_3_0, am::FIX_CLEANUP_3_4_0, am::SINGLE_ASSET_VAULT,
        am::LENDING_PROTOCOL, am::MPTOKENS_V1, am::PERMISSIONED_DOMAINS, am::CREDENTIALS,
    ];

    fn rules_with(extra: &[&'static str]) -> Vec<&'static str> {
        let mut all = MAINNET_WITH_LENDING.to_vec();
        all.extend_from_slice(extra);
        all
    }

    fn rules_without(off: &[&str]) -> Vec<&'static str> {
        MAINNET_WITH_LENDING.iter().copied().filter(|a| !off.contains(a)).collect()
    }

    fn put(state: &mut LedgerState, key: Hash256, v: &Value) {
        state.state_map.insert(key, serde_json::to_vec(v).unwrap()).unwrap();
    }

    fn root_in(state: &mut LedgerState, id: [u8; 20], flags: u64) {
        let a = json!({
            "LedgerEntryType": "AccountRoot", "Account": hex::encode(id), "Balance": BALANCE.to_string(),
            "Sequence": 5, "OwnerCount": 0, "Flags": flags,
        });
        put(state, keylet::account_root_key(&id), &a);
    }

    fn state_with(amendments: &[&str]) -> LedgerState {
        let mut state = LedgerState::new_unverified(LedgerHeader {
            sequence: 100, total_coins: 100_000_000_000_000_000,
            parent_hash: Hash256([0x77; 32]), transaction_hash: Hash256([0; 32]), account_hash: Hash256([0; 32]),
            parent_close_time: 0, close_time: NOW, close_time_resolution: 10, close_flags: 0,
        });
        put(&mut state, keylet::amendments_key(), &json!({"LedgerEntryType": "Amendments", "Flags": 0, "Amendments": amendments}));
        root_in(&mut state, OWNER, 0);
        root_in(&mut state, BOB, 0);
        root_in(&mut state, CAROL, 0);
        root_in(&mut state, ISSUER, LSF_DEFAULT_RIPPLE | LSF_ALLOW_TRUST_LINE_CLAWBACK);
        state
    }

    fn state() -> LedgerState {
        state_with(&MAINNET_WITH_LENDING)
    }

    fn mptid(seq: u32, issuer: [u8; 20]) -> [u8; 24] {
        let mut id = [0u8; 24];
        id[..4].copy_from_slice(&seq.to_be_bytes());
        id[4..].copy_from_slice(&issuer);
        id
    }

    /// An MPT issued by ISSUER (sequence 7).
    fn issuance(state: &mut LedgerState, flags: u64) -> [u8; 24] {
        let id = mptid(7, ISSUER);
        let o = json!({
            "LedgerEntryType": "MPTokenIssuance", "Issuer": hex::encode(ISSUER), "Flags": flags, "Sequence": 7,
            "OutstandingAmount": "0", "OwnerNode": "0",
        });
        put(state, keylet::mpt_issuance_key(&id), &o);
        id
    }

    /// `holder`'s MPToken of `id` holding `amount`; the issuance's
    /// OutstandingAmount grows by it.
    fn token(state: &mut LedgerState, id: &[u8; 24], holder: [u8; 20], flags: u64, amount: u64) {
        let ikey = keylet::mpt_issuance_key(id);
        let mut t = json!({
            "LedgerEntryType": "MPToken", "Account": hex::encode(holder), "MPTokenIssuanceID": hex::encode_upper(id),
            "Flags": flags, "OwnerNode": "0",
        });
        if amount != 0 {
            t["MPTAmount"] = json!(amount.to_string());
            let mut i: Value = serde_json::from_slice(&state.state_map.lookup(&ikey).unwrap()).unwrap();
            let outstanding = mpt::dec_field(&i, "OutstandingAmount") + amount;
            i["OutstandingAmount"] = json!(outstanding.to_string());
            put(state, ikey, &i);
        }
        put(state, keylet::mptoken_key(&ikey, &holder), &t);
    }

    /// A USD line between `holder` and ISSUER, the holder holding `balance`
    /// with its own limit `limit`.
    fn line(state: &mut LedgerState, holder: [u8; 20], flags: u64, balance: &str, limit: &str) -> Hash256 {
        let key = keylet::ripple_state_key(&holder, &ISSUER, &USD_CODE);
        let cur = hex::encode_upper(USD_CODE);
        let holder_low = holder < ISSUER;
        let (low, high) = if holder_low { (holder, ISSUER) } else { (ISSUER, holder) };
        let stored = if holder_low || balance == "0" { balance.to_string() } else { format!("-{balance}") };
        let (low_limit, high_limit) = if holder_low { (limit, "0") } else { ("0", limit) };
        put(state, key, &json!({
            "LedgerEntryType": "RippleState", "Flags": flags,
            "Balance": {"currency": cur, "issuer": "0000000000000000000000000000000000000001", "value": stored},
            "LowLimit": {"currency": cur, "issuer": hex::encode(low), "value": low_limit},
            "HighLimit": {"currency": cur, "issuer": hex::encode(high), "value": high_limit},
            "LowNode": "0", "HighNode": "0",
        }));
        key
    }

    fn usd_json(value: &str) -> Value {
        json!({"currency": hex::encode_upper(USD_CODE), "issuer": hex::encode(ISSUER), "value": value})
    }

    fn usd(value: &str) -> Amount {
        Amount::from_json(&usd_json(value)).unwrap()
    }

    fn mpt_json(id: &[u8; 24], value: u64) -> Value {
        json!({"mpt_issuance_id": hex::encode_upper(id), "value": value.to_string()})
    }

    fn get(sb: &Sandbox, key: &Hash256) -> Option<Value> {
        json_at(sb, key)
    }

    fn root_of(sb: &Sandbox, id: &[u8; 20]) -> Value {
        get(sb, &keylet::account_root_key(id)).unwrap()
    }

    fn owners(sb: &Sandbox, id: &[u8; 20]) -> u64 {
        root_of(sb, id)["OwnerCount"].as_u64().unwrap()
    }

    fn xrp_of(sb: &Sandbox, id: &[u8; 20]) -> u64 {
        root_of(sb, id)["Balance"].as_str().unwrap().parse().unwrap()
    }

    fn dir_has(sb: &Sandbox, owner: &[u8; 20], key: &Hash256) -> bool {
        get(sb, &keylet::owner_dir_key(owner))
            .is_some_and(|d| d["Indexes"].as_array().is_some_and(|a| a.iter().any(|k| k == &json!(hex::encode_upper(key.0)))))
    }

    /// What `holder` holds of `asset` (freezes and authorization ignored).
    fn holds(sb: &Sandbox, holder: &[u8; 20], asset: &HeldAsset) -> Amount {
        holding::account_holds(sb, holder, asset, false, false)
    }

    fn n(s: &str) -> Number {
        number_from_str(s).unwrap()
    }

    /// A transaction of `account` at the account's sequence in the test
    /// state (5), with the type's own fields.
    fn tx(tx_type: &str, account: [u8; 20], fields: Value) -> TxFields {
        let mut all = json!({
            "TransactionType": tx_type, "Account": hex::encode(account), "Fee": FEE.to_string(), "Sequence": 5,
            "hash": "AB".repeat(32),
        });
        for (k, v) in fields.as_object().unwrap() {
            all[k] = v.clone();
        }
        TxFields::from_json(&all).unwrap()
    }

    /// Apply on a fresh sandbox, the whole pipeline; the sandbox comes back
    /// for inspection.
    fn apply<'a>(state: &'a LedgerState, tx: &TxFields) -> (TxResult, Sandbox<'a>) {
        let mut sb = Sandbox::new(state);
        let (result, _) = apply_on_sandbox(tx, &mut sb);
        (result, sb)
    }

    fn result(state: &LedgerState, tx: &TxFields) -> TxResult {
        apply(state, tx).0
    }

    /// Apply and keep: the state after a transaction that must succeed.
    fn commit(state: &mut LedgerState, tx: &TxFields) {
        let (result, sb) = apply(state, tx);
        assert_eq!(result, TxResult::Success, "{} must apply", tx.tx_type);
        let mods = sb.into_modifications();
        crate::ledger::sandbox::apply_modifications(state, mods).unwrap();
        // The next transaction of this account in a test uses the same
        // Sequence field; put the root's back.
        let key = keylet::account_root_key(&tx.account);
        let mut root: Value = serde_json::from_slice(&state.state_map.lookup(&key).unwrap()).unwrap();
        root["Sequence"] = json!(5);
        put(state, key, &root);
    }

    fn edit(state: &mut LedgerState, key: Hash256, change: impl FnOnce(&mut Value)) {
        let mut v: Value = serde_json::from_slice(&state.state_map.lookup(&key).unwrap()).unwrap();
        change(&mut v);
        put(state, key, &v);
    }

    fn key_hex(key: &Hash256) -> String {
        hex::encode_upper(key.0)
    }

    fn xrp_asset() -> Value {
        json!({"currency": "XRP"})
    }

    fn usd_asset() -> Value {
        json!({"currency": hex::encode_upper(USD_CODE), "issuer": hex::encode(ISSUER)})
    }

    /// A vault made by OWNER (sequence 5), committed: its key and
    /// pseudo-account.
    fn make_vault(state: &mut LedgerState, fields: Value) -> (Hash256, [u8; 20]) {
        let key = keylet::vault_key(&OWNER, 5);
        commit(state, &tx("VaultCreate", OWNER, fields));
        let vault: Value = serde_json::from_slice(&state.state_map.lookup(&key).unwrap()).unwrap();
        (key, account_of(&vault, "Account").unwrap())
    }

    /// An open-ended XRP vault of OWNER.
    fn xrp_vault(state: &mut LedgerState) -> (Hash256, [u8; 20]) {
        make_vault(state, json!({"Asset": xrp_asset()}))
    }

    /// An open-ended USD vault of OWNER, who holds 1000 USD (limit 100000).
    fn usd_vault(state: &mut LedgerState) -> (Hash256, [u8; 20]) {
        line(state, OWNER, 0, "1000", "100000");
        make_vault(state, json!({"Asset": usd_asset()}))
    }

    /// An open-ended vault of ISSUER's MPT (transferable, clawable), of
    /// which OWNER holds 1000.
    fn mpt_vault(state: &mut LedgerState) -> (Hash256, [u8; 20], [u8; 24]) {
        let id = issuance(state, mpt::LSF_MPT_CAN_TRANSFER | mpt::LSF_MPT_CAN_CLAWBACK);
        token(state, &id, OWNER, 0, 1000);
        let (key, pseudo) = make_vault(state, json!({"Asset": {"mpt_issuance_id": hex::encode_upper(id)}}));
        (key, pseudo, id)
    }

    fn broker_set(account: [u8; 20], vault: &Hash256, fields: Value) -> TxFields {
        let mut all = json!({"VaultID": key_hex(vault)});
        for (k, v) in fields.as_object().unwrap() {
            all[k] = v.clone();
        }
        tx("LoanBrokerSet", account, all)
    }

    /// The key of the broker a LoanBrokerSet of OWNER at sequence 5 makes.
    fn broker_key() -> Hash256 {
        keylet::loan_broker_key(&OWNER, 5)
    }

    /// A broker of OWNER on `vault`, committed: its key and pseudo-account.
    fn make_broker(state: &mut LedgerState, vault: &Hash256, fields: Value) -> (Hash256, [u8; 20]) {
        commit(state, &broker_set(OWNER, vault, fields));
        let broker: Value = serde_json::from_slice(&state.state_map.lookup(&broker_key()).unwrap()).unwrap();
        (broker_key(), account_of(&broker, "Account").unwrap())
    }

    fn broker_of(sb: &Sandbox) -> Value {
        get(sb, &broker_key()).unwrap()
    }

    // -----------------------------------------------------------------------
    // LoanBrokerSet
    // -----------------------------------------------------------------------

    /// LoanBrokerSet.cpp:36-83, every refusal temINVALID
    /// (LoanBroker_test.cpp:1199 "Invalid LoanBrokerSet", :872 testLoanBroker's
    /// field-range cases).
    #[test]
    fn loan_broker_set_preflight() {
        let st = state();
        let vault = Hash256([0xAB; 32]);
        let invalid = |fields: Value, why: &str| {
            let (r, sb) = apply(&st, &broker_set(OWNER, &vault, fields));
            assert_eq!(r, TxResult::InvalidTx, "{why}");
            assert_eq!(sb.modification_count(), 0, "{why}: a tem charges nothing");
        };
        let passes = |fields: Value, why: &str| {
            // Past preflight: the vault does not exist.
            assert_eq!(result(&st, &broker_set(OWNER, &vault, fields)), TxResult::NoEntry, "{why}");
        };
        invalid(json!({"Data": "AB".repeat(257)}), "sfData over 256 bytes");
        passes(json!({"Data": "AB".repeat(256)}), "sfData of 256 bytes");
        passes(json!({"Data": ""}), "an empty sfData clears the field");
        invalid(json!({"ManagementFeeRate": 10_001}), "a management fee above 10%");
        passes(json!({"ManagementFeeRate": 10_000}), "a management fee of 10%");
        invalid(json!({"CoverRateMinimum": 100_001, "CoverRateLiquidation": 1}), "a minimum cover rate above 100%");
        invalid(json!({"CoverRateMinimum": 1, "CoverRateLiquidation": 100_001}), "a liquidation cover rate above 100%");
        passes(json!({"CoverRateMinimum": 100_000, "CoverRateLiquidation": 100_000}), "both cover rates at 100%");
        invalid(json!({"DebtMaximum": "-1"}), "a negative sfDebtMaximum");
        invalid(json!({"DebtMaximum": "9223372036854775808"}), "sfDebtMaximum above the largest MPT amount");
        passes(json!({"DebtMaximum": "9223372036854775807"}), "sfDebtMaximum at the largest MPT amount");
        passes(json!({"DebtMaximum": "0"}), "a zero sfDebtMaximum");
        // The fixed fields cannot accompany sfLoanBrokerID.
        let broker = "CD".repeat(32);
        for fixed in ["ManagementFeeRate", "CoverRateMinimum", "CoverRateLiquidation"] {
            let mut fields = json!({"LoanBrokerID": broker});
            fields[fixed] = json!(0);
            invalid(fields, fixed);
        }
        invalid(json!({"LoanBrokerID": "00".repeat(32)}), "a zero sfLoanBrokerID");
        let (r, _) = apply(&st, &broker_set(OWNER, &Hash256([0; 32]), json!({})));
        assert_eq!(r, TxResult::InvalidTx, "a zero sfVaultID");
        // Both cover rates zero, or neither.
        invalid(json!({"CoverRateMinimum": 1}), "a minimum cover rate without a liquidation rate");
        invalid(json!({"CoverRateLiquidation": 1}), "a liquidation rate without a minimum cover rate");
        invalid(json!({"CoverRateMinimum": 1, "CoverRateLiquidation": 0}), "one of the two rates zero");
        passes(json!({"CoverRateMinimum": 0, "CoverRateLiquidation": 0}), "both rates zero");
    }

    /// LoanBrokerSet.cpp:30-34 with LendingHelpers.cpp:69-82
    /// (LoanBroker_test.cpp:84 "Disabled"); sfDomainID is 3.5.0-rc1
    /// LoanBrokerSet.cpp:34-38 — LendingProtocolV1_2, not ported.
    #[test]
    fn loan_broker_set_extra_features() {
        let vault = Hash256([0xAB; 32]);
        for off in [am::LENDING_PROTOCOL, am::SINGLE_ASSET_VAULT, am::MPTOKENS_V1] {
            let st = state_with(&rules_without(&[off]));
            assert_eq!(result(&st, &broker_set(OWNER, &vault, json!({}))), TxResult::Disabled, "{off}");
        }
        let st = state();
        assert_eq!(result(&st, &broker_set(OWNER, &vault, json!({"DomainID": "D0".repeat(32)}))), TxResult::Disabled);
    }

    /// LoanBrokerSet.cpp:93-146: the vault, its owner; changing a broker —
    /// the broker, its vault, its owner, sfDebtMaximum against sfDebtTotal
    /// (LoanBroker_test.cpp:1379 "testLoanBrokerSetDebtMaximum").
    #[test]
    fn loan_broker_set_preclaim_vault_and_broker() {
        let mut st = state();
        let (vault, _) = xrp_vault(&mut st);
        assert_eq!(result(&st, &broker_set(OWNER, &Hash256([0xAB; 32]), json!({}))), TxResult::NoEntry, "no such vault");
        assert_eq!(result(&st, &broker_set(BOB, &vault, json!({}))), TxResult::NoPermission, "not the vault's owner");
        // A broker id naming nothing, or something that is not a broker.
        assert_eq!(result(&st, &broker_set(OWNER, &vault, json!({"LoanBrokerID": "CD".repeat(32)}))), TxResult::NoEntry);
        assert_eq!(result(&st, &broker_set(OWNER, &vault, json!({"LoanBrokerID": key_hex(&vault)}))), TxResult::NoEntry);

        let (broker, _) = make_broker(&mut st, &vault, json!({}));
        let update = |st: &LedgerState, fields: Value| {
            let mut all = json!({"LoanBrokerID": key_hex(&broker)});
            for (k, v) in fields.as_object().unwrap() {
                all[k] = v.clone();
            }
            result(st, &broker_set(OWNER, &vault, all))
        };
        assert_eq!(update(&st, json!({})), TxResult::Success, "a no-op change");
        // The broker's vault cannot be changed: a second vault of OWNER's.
        let mut other = st.clone();
        let second = keylet::vault_key(&OWNER, 6);
        let mut copy: Value = serde_json::from_slice(&other.state_map.lookup(&vault).unwrap()).unwrap();
        copy["Sequence"] = json!(6);
        put(&mut other, second, &copy);
        assert_eq!(
            result(&other, &broker_set(OWNER, &second, json!({"LoanBrokerID": key_hex(&broker)}))),
            TxResult::NoPermission,
            "another vault than the broker's"
        );
        // The broker of someone else, on a vault that is the submitter's.
        let mut foreign = st.clone();
        edit(&mut foreign, broker, |b| b["Owner"] = json!(hex::encode(BOB)));
        assert_eq!(
            result(&foreign, &broker_set(OWNER, &vault, json!({"LoanBrokerID": key_hex(&broker)}))),
            TxResult::NoPermission,
            "not the broker's owner"
        );
        // sfDebtMaximum may not fall below what is lent; zero lifts the limit.
        let mut lent = st.clone();
        edit(&mut lent, broker, |b| b["DebtTotal"] = json!("500"));
        assert_eq!(update(&lent, json!({"DebtMaximum": "499"})), TxResult::LimitExceeded);
        assert_eq!(update(&lent, json!({"DebtMaximum": "500"})), TxResult::Success);
        assert_eq!(update(&lent, json!({"DebtMaximum": "0"})), TxResult::Success);
    }

    /// LoanBrokerSet.cpp:149-171: a new broker — LendingProtocolV1_1's
    /// closed-ended vault, an asset the pseudo-account can hold, a vault
    /// pseudo-account that is not frozen (LoanBroker_test.cpp:1623 "RIPD-4466
    /// - LoanBrokerSet disallows frozen vaults").
    #[test]
    fn loan_broker_set_preclaim_new_broker() {
        // LendingProtocolV1_1: open-ended is refused, closed-ended passes.
        let mut st = state_with(&rules_with(&[am::LENDING_PROTOCOL_V1_1]));
        let (vault, _) = xrp_vault(&mut st);
        assert_eq!(result(&st, &broker_set(OWNER, &vault, json!({}))), TxResult::NoPermission, "an open-ended vault under V1_1");
        let mut st = state_with(&rules_with(&[am::LENDING_PROTOCOL_V1_1]));
        let (vault, _) = make_vault(
            &mut st,
            json!({"Asset": xrp_asset(), "VaultKind": 1, "SubscriptionDate": NOW + 100, "RedemptionDate": NOW + 5000}),
        );
        assert_eq!(result(&st, &broker_set(OWNER, &vault, json!({}))), TxResult::Success, "a closed-ended vault under V1_1");
        // Without V1_1 an open-ended vault hosts a broker.
        let mut st = state();
        let (vault, _) = xrp_vault(&mut st);
        assert_eq!(result(&st, &broker_set(OWNER, &vault, json!({}))), TxResult::Success);

        // canAddHolding: the MPT lost lsfMPTCanTransfer; the IOU's issuer
        // lost lsfDefaultRipple.
        let mut st = state();
        let (vault, _, id) = mpt_vault(&mut st);
        edit(&mut st, keylet::mpt_issuance_key(&id), |i| i["Flags"] = json!(mpt::LSF_MPT_CAN_CLAWBACK));
        assert_eq!(result(&st, &broker_set(OWNER, &vault, json!({}))), TxResult::NoAuth);
        let mut st = state();
        let (vault, _) = usd_vault(&mut st);
        edit(&mut st, keylet::account_root_key(&ISSUER), |a| a["Flags"] = json!(0));
        assert_eq!(result(&st, &broker_set(OWNER, &vault, json!({}))), TxResult::NoRipple);

        // The vault pseudo-account frozen: globally, or on its own line; an
        // MPT locked.
        let mut st = state();
        let (vault, _) = usd_vault(&mut st);
        edit(&mut st, keylet::account_root_key(&ISSUER), |a| a["Flags"] = json!(LSF_DEFAULT_RIPPLE | LSF_GLOBAL_FREEZE));
        assert_eq!(result(&st, &broker_set(OWNER, &vault, json!({}))), TxResult::Frozen);
        let mut st = state();
        let (vault, vault_pseudo) = usd_vault(&mut st);
        let issuer_side = if ISSUER < vault_pseudo { LSF_LOW_FREEZE } else { LSF_HIGH_FREEZE };
        edit(&mut st, keylet::ripple_state_key(&vault_pseudo, &ISSUER, &USD_CODE), |l| {
            l["Flags"] = json!(l["Flags"].as_u64().unwrap() | issuer_side)
        });
        assert_eq!(result(&st, &broker_set(OWNER, &vault, json!({}))), TxResult::Frozen);
        let mut st = state();
        let (vault, _, id) = mpt_vault(&mut st);
        edit(&mut st, keylet::mpt_issuance_key(&id), |i| i["Flags"] = json!(i["Flags"].as_u64().unwrap() | mpt::LSF_MPT_LOCKED));
        assert_eq!(result(&st, &broker_set(OWNER, &vault, json!({}))), TxResult::Locked);
    }

    /// LoanBrokerSet.cpp:173-183 (`getValueFields`): sfDebtMaximum must be
    /// exact in the vault's asset; fixCleanup3_5_0 (3.5.0-rc1 :225-242)
    /// turns the out-of-range throw into tecPRECISION_LOSS.
    #[test]
    fn loan_broker_set_debt_maximum_must_be_representable_in_the_asset() {
        let mut st = state();
        let (vault, _) = xrp_vault(&mut st);
        let with = |st: &LedgerState, maximum: &str| result(st, &broker_set(OWNER, &vault, json!({"DebtMaximum": maximum})));
        assert_eq!(with(&st, "1000"), TxResult::Success);
        assert_eq!(with(&st, "1000.5"), TxResult::PrecisionLoss, "half a drop");
        // Above the XRP range (1e17 drops): STAmount throws.
        assert_eq!(with(&st, "200000000000000000"), TxResult::TefException);
        let mut st = state_with(&rules_with(&[am::FIX_CLEANUP_3_5_0]));
        let (vault, _) = xrp_vault(&mut st);
        let with = |st: &LedgerState, maximum: &str| result(st, &broker_set(OWNER, &vault, json!({"DebtMaximum": maximum})));
        assert_eq!(with(&st, "200000000000000000"), TxResult::PrecisionLoss);
        assert_eq!(with(&st, "1000.5"), TxResult::PrecisionLoss);
        assert_eq!(with(&st, "1000"), TxResult::Success);
        // An IOU holds sixteen digits; an MPT whole units.
        let mut st = state();
        let (vault, _) = usd_vault(&mut st);
        let with = |st: &LedgerState, maximum: &str| result(st, &broker_set(OWNER, &vault, json!({"DebtMaximum": maximum})));
        assert_eq!(with(&st, "1000.5"), TxResult::Success);
        assert_eq!(with(&st, "1234567890123456"), TxResult::Success);
        assert_eq!(with(&st, "12345678901234567"), TxResult::PrecisionLoss, "seventeen digits");
        let mut st = state();
        let (vault, _, _) = mpt_vault(&mut st);
        assert_eq!(result(&st, &broker_set(OWNER, &vault, json!({"DebtMaximum": "0.5"}))), TxResult::PrecisionLoss);
    }

    /// LoanBrokerSet.cpp:222-292 on an XRP vault (LoanBroker_test.cpp:216-270,
    /// the broker and its pseudo-account after create): the LoanBroker, its
    /// pseudo-account, both directory links, two reserve units.
    #[test]
    fn loan_broker_set_creates_the_broker_its_pseudo_account_and_both_directory_links() {
        let mut st = state();
        let (vault, vault_pseudo) = xrp_vault(&mut st);
        let owners_before = owners(&Sandbox::new(&st), &OWNER);
        let (r, sb) = apply(&st, &broker_set(OWNER, &vault, json!({})));
        assert_eq!(r, TxResult::Success);
        let key = broker_key();
        let broker = get(&sb, &key).expect("the LoanBroker");
        let pseudo = account_of(&broker, "Account").unwrap();
        assert_eq!(
            broker,
            json!({
                "LedgerEntryType": "LoanBroker", "Flags": 0, "Sequence": 5, "LoanSequence": 1,
                "VaultID": key_hex(&vault), "Owner": hex::encode(OWNER), "Account": hex::encode(pseudo),
                "OwnerNode": "0", "VaultNode": "0",
            }),
            "sfOwnerCount, sfDebtTotal, sfCoverAvailable and the optional fields are at their defaults: absent"
        );
        assert_eq!(Some(pseudo), crate::tx::misc::pseudo_account_address(&Sandbox::new(&st), &key));
        assert_ne!(pseudo, vault_pseudo);
        // The pseudo-account: no key, deposits by authorization only,
        // sequence 0, nothing owned (XRP needs no holding).
        let root = root_of(&sb, &pseudo);
        assert_eq!(root["Flags"], json!(0x0010_0000u64 | 0x0080_0000 | 0x0100_0000));
        assert_eq!((root["Sequence"].as_u64(), root["Balance"].as_str(), root["OwnerCount"].as_u64()), (Some(0), Some("0"), Some(0)));
        assert_eq!(root["LoanBrokerID"], json!(key_hex(&key)));
        assert!(root.get("VaultID").is_none() && root.get("AMMID").is_none());
        assert!(!sb.exists(&keylet::owner_dir_key(&pseudo)));
        // Two directories; two reserve units on the owner; the fee.
        assert!(dir_has(&sb, &OWNER, &key), "the owner's directory");
        assert!(dir_has(&sb, &vault_pseudo, &key), "the vault pseudo-account's directory");
        assert_eq!(owners(&sb, &OWNER), owners_before + 2);
        assert_eq!(owners(&sb, &vault_pseudo), owners(&Sandbox::new(&st), &vault_pseudo), "the vault pseudo-account is charged nothing");
        assert_eq!(xrp_of(&sb, &OWNER), xrp_of(&Sandbox::new(&st), &OWNER) - FEE);
        // The vault itself is not written.
        assert!(sb.modifications().get(&vault).is_none());
    }

    /// LoanBrokerSet.cpp:267-287: the optional fields are stored, zeros are
    /// not (SoeDefault); an IOU or MPT vault's broker pseudo-account gets
    /// its holding (LoanBroker_test.cpp:253: OwnerCount 1 unless XRP).
    #[test]
    fn loan_broker_set_stores_the_optional_fields_and_gives_the_pseudo_account_its_holding() {
        let mut st = state();
        let (vault, _) = usd_vault(&mut st);
        let fields = json!({
            "Data": "abcdef", "ManagementFeeRate": 123, "DebtMaximum": "5000.25", "CoverRateMinimum": 1000, "CoverRateLiquidation": 500,
        });
        let (r, sb) = apply(&st, &broker_set(OWNER, &vault, fields));
        assert_eq!(r, TxResult::Success);
        let broker = broker_of(&sb);
        assert_eq!(broker["Data"], json!("ABCDEF"));
        assert_eq!((&broker["ManagementFeeRate"], &broker["CoverRateMinimum"], &broker["CoverRateLiquidation"]), (&json!(123), &json!(1000), &json!(500)));
        assert!(number_field(&broker, "DebtMaximum") == n("5000.25"));
        let pseudo = account_of(&broker, "Account").unwrap();
        // A zero-balance, zero-limit line of the pseudo-account, counted on it.
        let line_key = keylet::ripple_state_key(&pseudo, &ISSUER, &USD_CODE);
        assert!(sb.exists(&line_key), "the pseudo-account's trust line");
        assert!(holds(&sb, &pseudo, &USD).is_zero());
        assert_eq!(owners(&sb, &pseudo), 1);
        assert!(dir_has(&sb, &pseudo, &line_key));

        // Zeros and an empty sfData are the defaults: not stored.
        let fields = json!({"Data": "", "ManagementFeeRate": 0, "DebtMaximum": "0", "CoverRateMinimum": 0, "CoverRateLiquidation": 0});
        let (r, sb) = apply(&st, &broker_set(OWNER, &vault, fields));
        assert_eq!(r, TxResult::Success);
        let broker = broker_of(&sb);
        for field in ["Data", "ManagementFeeRate", "DebtMaximum", "CoverRateMinimum", "CoverRateLiquidation"] {
            assert!(broker.get(field).is_none(), "{field}");
        }

        // An MPT vault: the pseudo-account's MPToken.
        let mut st = state();
        let (vault, _, id) = mpt_vault(&mut st);
        let (r, sb) = apply(&st, &broker_set(OWNER, &vault, json!({})));
        assert_eq!(r, TxResult::Success);
        let pseudo = account_of(&broker_of(&sb), "Account").unwrap();
        assert!(sb.exists(&keylet::mptoken_key(&keylet::mpt_issuance_key(&id), &pseudo)), "the pseudo-account's MPToken");
        assert_eq!(owners(&sb, &pseudo), 1);
    }

    /// LoanBrokerSet.cpp:255-259: the two reserve units are judged against
    /// the balance before the fee; a failure keeps only the fee.
    #[test]
    fn loan_broker_set_needs_the_reserve_for_two_more_objects() {
        let mut st = state();
        let (vault, _) = xrp_vault(&mut st);
        let sb = Sandbox::new(&st);
        let count = owners(&sb, &OWNER);
        let needed = crate::ledger::fees::account_reserve(&sb, count + 2);
        let poor = |st: &LedgerState, balance: u64| {
            let mut st = st.clone();
            edit(&mut st, keylet::account_root_key(&OWNER), |a| a["Balance"] = json!(balance.to_string()));
            st
        };
        let short = poor(&st, needed - 1);
        let (r, sb) = apply(&short, &broker_set(OWNER, &vault, json!({})));
        assert_eq!(r, TxResult::InsufficientReserve);
        assert!(!sb.exists(&broker_key()), "nothing of the broker stays");
        assert_eq!(owners(&sb, &OWNER), count);
        assert_eq!(xrp_of(&sb, &OWNER), needed - 1 - FEE, "the fee is charged");
        let enough = poor(&st, needed);
        assert_eq!(result(&enough, &broker_set(OWNER, &vault, json!({}))), TxResult::Success, "exactly the reserve, before the fee");
    }

    /// LoanBrokerSet.cpp:194-221 (LoanBroker_test.cpp:480-497: "Verify that
    /// fields get removed when set to default values"): sfData and
    /// sfDebtMaximum change; nothing else does.
    #[test]
    fn loan_broker_set_changes_data_and_debt_maximum_of_an_existing_broker() {
        let mut st = state();
        let (vault, _) = usd_vault(&mut st);
        let (broker, pseudo) = make_broker(&mut st, &vault, json!({"ManagementFeeRate": 7, "Data": "AA", "DebtMaximum": "100"}));
        let before = broker_of(&Sandbox::new(&st));
        let owners_before = owners(&Sandbox::new(&st), &OWNER);
        let update = |fields: Value| {
            let mut all = json!({"LoanBrokerID": key_hex(&broker)});
            for (k, v) in fields.as_object().unwrap() {
                all[k] = v.clone();
            }
            apply(&st, &broker_set(OWNER, &vault, all))
        };
        let (r, sb) = update(json!({"Data": "bbcc", "DebtMaximum": "250.5"}));
        assert_eq!(r, TxResult::Success);
        let after = broker_of(&sb);
        assert_eq!(after["Data"], json!("BBCC"));
        assert!(number_field(&after, "DebtMaximum") == n("250.5"));
        for field in ["ManagementFeeRate", "Sequence", "LoanSequence", "VaultID", "Owner", "Account", "OwnerNode", "VaultNode", "Flags"] {
            assert_eq!(after[field], before[field], "{field}");
        }
        assert_eq!(owners(&sb, &OWNER), owners_before, "no reserve change");
        assert_eq!(account_of(&after, "Account"), Some(pseudo));
        // Only one of the two.
        let (_, sb) = update(json!({"Data": "DD"}));
        assert!(number_field(&broker_of(&sb), "DebtMaximum") == n("100"));
        // The defaults remove the fields.
        let (r, sb) = update(json!({"Data": "", "DebtMaximum": "0"}));
        assert_eq!(r, TxResult::Success);
        let after = broker_of(&sb);
        assert!(after.get("Data").is_none() && after.get("DebtMaximum").is_none());
        // A no-op still rewrites (and re-threads) the broker.
        let (r, sb) = update(json!({}));
        assert_eq!(r, TxResult::Success);
        assert_eq!(broker_of(&sb), before);
    }

    // -----------------------------------------------------------------------
    // LoanBrokerCoverDeposit
    // -----------------------------------------------------------------------

    fn cover_deposit(account: [u8; 20], broker: &Hash256, amount: Value) -> TxFields {
        tx("LoanBrokerCoverDeposit", account, json!({"LoanBrokerID": key_hex(broker), "Amount": amount}))
    }

    /// An XRP vault of OWNER with a broker on it.
    fn xrp_broker(state: &mut LedgerState) -> (Hash256, [u8; 20], Hash256) {
        let (vault, _) = xrp_vault(state);
        let (broker, pseudo) = make_broker(state, &vault, json!({}));
        (broker, pseudo, vault)
    }

    /// A USD vault of OWNER (holding 1000 USD) with a broker on it.
    fn usd_broker(state: &mut LedgerState) -> (Hash256, [u8; 20], Hash256) {
        let (vault, _) = usd_vault(state);
        let (broker, pseudo) = make_broker(state, &vault, json!({}));
        (broker, pseudo, vault)
    }

    /// A vault of ISSUER's MPT (OWNER holding 1000) with a broker on it.
    fn mpt_broker(state: &mut LedgerState) -> (Hash256, [u8; 20], [u8; 24]) {
        let (vault, _, id) = mpt_vault(state);
        let (broker, pseudo) = make_broker(state, &vault, json!({}));
        (broker, pseudo, id)
    }

    /// LoanBrokerCoverDeposit.cpp:27-41 (LoanBroker_test.cpp:323-326:
    /// a zero broker id temINVALID; a zero or negative amount temBAD_AMOUNT).
    #[test]
    fn cover_deposit_preflight() {
        let st = state();
        let broker = Hash256([0xCD; 32]);
        assert_eq!(result(&st, &cover_deposit(OWNER, &Hash256([0; 32]), json!("10"))), TxResult::InvalidTx);
        for amount in [json!("0"), json!("-10"), usd_json("0"), usd_json("-10")] {
            assert_eq!(result(&st, &cover_deposit(OWNER, &broker, amount.clone())), TxResult::BadAmount, "{amount}");
        }
        // The broker id is judged before the amount.
        assert_eq!(result(&st, &cover_deposit(OWNER, &Hash256([0; 32]), json!("0"))), TxResult::InvalidTx);
        // XRP beyond the legal range.
        assert_eq!(result(&st, &cover_deposit(OWNER, &broker, json!("100000000000000001"))), TxResult::BadAmount);
        assert_eq!(result(&st, &cover_deposit(OWNER, &broker, json!("10"))), TxResult::NoEntry, "past preflight");
    }

    /// LoanBrokerCoverDeposit.cpp:43-76 (LoanBroker_test.cpp:324, :327: not
    /// the owner tecNO_PERMISSION; an id that is not a broker tecNO_ENTRY).
    #[test]
    fn cover_deposit_preclaim_broker_owner_and_asset() {
        let mut st = state();
        let (broker, _, vault) = xrp_broker(&mut st);
        assert_eq!(result(&st, &cover_deposit(OWNER, &vault, json!("10"))), TxResult::NoEntry, "a vault's id");
        assert_eq!(result(&st, &cover_deposit(BOB, &broker, json!("10"))), TxResult::NoPermission);
        assert_eq!(result(&st, &cover_deposit(OWNER, &broker, usd_json("10"))), TxResult::WrongAsset);
        // The vault gone from under the broker.
        let mut orphan = st.clone();
        edit(&mut orphan, broker, |b| b["VaultID"] = json!("EE".repeat(32)));
        assert_eq!(result(&orphan, &cover_deposit(OWNER, &broker, json!("10"))), TxResult::TefBadLedger);
        // More XRP than the owner can spare above its reserve.
        assert_eq!(result(&st, &cover_deposit(OWNER, &broker, json!(BALANCE.to_string()))), TxResult::InsufficientFunds);
        assert_eq!(result(&st, &cover_deposit(OWNER, &broker, json!("10"))), TxResult::Success);
    }

    /// LoanBrokerCoverDeposit.cpp:78-98 (LoanBroker_test.cpp:1953
    /// "LoanBrokerCoverDeposit IOU freeze checks", :2020 "MPT lock checks").
    #[test]
    fn cover_deposit_freezes_and_authorization() {
        // IOU: a global freeze; the depositor's line frozen; the
        // pseudo-account's line frozen.
        let mut st = state();
        let (broker, pseudo, _) = usd_broker(&mut st);
        let deposit = cover_deposit(OWNER, &broker, usd_json("10"));
        assert_eq!(result(&st, &deposit), TxResult::Success);
        let mut global = st.clone();
        edit(&mut global, keylet::account_root_key(&ISSUER), |a| a["Flags"] = json!(a["Flags"].as_u64().unwrap() | LSF_GLOBAL_FREEZE));
        assert_eq!(result(&global, &deposit), TxResult::Frozen);
        let freeze = |st: &LedgerState, holder: [u8; 20]| {
            let mut st = st.clone();
            let issuer_side = if ISSUER < holder { LSF_LOW_FREEZE } else { LSF_HIGH_FREEZE };
            edit(&mut st, keylet::ripple_state_key(&holder, &ISSUER, &USD_CODE), |l| {
                l["Flags"] = json!(l["Flags"].as_u64().unwrap() | issuer_side)
            });
            st
        };
        assert_eq!(result(&freeze(&st, OWNER), &deposit), TxResult::Frozen, "the depositor frozen");
        assert_eq!(result(&freeze(&st, pseudo), &deposit), TxResult::Frozen, "the pseudo-account frozen");
        // Before fixCleanup3_3_0 a plain freeze of the pseudo-account's
        // line did not stop a deposit (only a deep freeze did).
        let mut st = state_with(&rules_without(&[am::FIX_CLEANUP_3_3_0]));
        let (broker, pseudo, _) = usd_broker(&mut st);
        let deposit = cover_deposit(OWNER, &broker, usd_json("10"));
        assert_eq!(result(&freeze(&st, pseudo), &deposit), TxResult::Success);
        assert_eq!(result(&freeze(&st, OWNER), &deposit), TxResult::Frozen);

        // MPT: locked for the depositor; an issuance that is no longer
        // transferable.
        let mut st = state();
        let (broker, _, id) = mpt_broker(&mut st);
        let deposit = cover_deposit(OWNER, &broker, mpt_json(&id, 10));
        assert_eq!(result(&st, &deposit), TxResult::Success);
        let ikey = keylet::mpt_issuance_key(&id);
        let mut locked = st.clone();
        edit(&mut locked, keylet::mptoken_key(&ikey, &OWNER), |t| t["Flags"] = json!(mpt::LSF_MPT_LOCKED));
        assert_eq!(result(&locked, &deposit), TxResult::Locked);
        let mut not_transferable = st.clone();
        edit(&mut not_transferable, ikey, |i| i["Flags"] = json!(mpt::LSF_MPT_CAN_CLAWBACK));
        assert_eq!(result(&not_transferable, &deposit), TxResult::NoAuth, "lsfMPTCanTransfer cleared");
    }

    /// LoanBrokerCoverDeposit.cpp:100-131 (LoanBroker_test.cpp:2813 "Cover
    /// precision guard: Deposit zero-at-scale", :2828 "Deposit rounds down"):
    /// under fixCleanup3_2_0 the amount is floored at the cover's scale.
    #[test]
    fn cover_deposit_is_floored_at_the_scale_of_the_cover() {
        let mut st = state();
        let (broker, pseudo, _) = usd_broker(&mut st);
        // A cover of 100000 has its sixteenth digit at 1e-10.
        line(&mut st, OWNER, 0, "200000", "1000000");
        commit(&mut st, &cover_deposit(OWNER, &broker, usd_json("100000")));
        assert_eq!(result(&st, &cover_deposit(OWNER, &broker, usd_json("0.00000000001"))), TxResult::PrecisionLoss, "dust");
        let (r, sb) = apply(&st, &cover_deposit(OWNER, &broker, usd_json("1.00000000019")));
        assert_eq!(r, TxResult::Success);
        assert!(number_field(&broker_of(&sb), "CoverAvailable") == n("100001.0000000001"), "floored to 1.0000000001");
        assert_eq!(holds(&sb, &pseudo, &USD), usd("100001.0000000001"), "the same amount moved");
        assert_eq!(holds(&sb, &OWNER, &USD), usd("99998.9999999999"));
        // More than the owner holds or could draw: the line's balance plus
        // the issuer's limit (zero).
        assert_eq!(result(&st, &cover_deposit(OWNER, &broker, usd_json("100001"))), TxResult::InsufficientFunds);

        // Before the fix the amount goes in as asked.
        let mut st = state_with(&rules_without(&[am::FIX_CLEANUP_3_2_0]));
        let (broker, _, _) = usd_broker(&mut st);
        line(&mut st, OWNER, 0, "200000", "1000000");
        commit(&mut st, &cover_deposit(OWNER, &broker, usd_json("100000")));
        assert_eq!(result(&st, &cover_deposit(OWNER, &broker, usd_json("0.00000000001"))), TxResult::Success);
    }

    /// LoanBrokerCoverDeposit.cpp:136-189 (LoanBroker_test.cpp:383-386
    /// `verifyCoverAmount`: sfCoverAvailable and the pseudo-account's
    /// balance both show the deposit) — XRP, IOU and MPT.
    #[test]
    fn cover_deposit_moves_the_asset_to_the_pseudo_account_and_raises_the_cover() {
        // XRP.
        let mut st = state();
        let (broker, pseudo, _) = xrp_broker(&mut st);
        let before = Sandbox::new(&st);
        let (r, sb) = apply(&st, &cover_deposit(OWNER, &broker, json!("10000")));
        assert_eq!(r, TxResult::Success);
        assert!(number_field(&broker_of(&sb), "CoverAvailable") == n("10000"));
        assert_eq!(xrp_of(&sb, &pseudo), 10_000);
        assert_eq!(xrp_of(&sb, &OWNER), xrp_of(&before, &OWNER) - 10_000 - FEE);
        assert_eq!(owners(&sb, &OWNER), owners(&before, &OWNER), "no reserve change");
        // A second deposit adds.
        commit(&mut st, &cover_deposit(OWNER, &broker, json!("10000")));
        let (_, sb) = apply(&st, &cover_deposit(OWNER, &broker, json!("5")));
        assert!(number_field(&broker_of(&sb), "CoverAvailable") == n("10005"));
        assert_eq!(xrp_of(&sb, &pseudo), 10_005);

        // IOU.
        let mut st = state();
        let (broker, pseudo, _) = usd_broker(&mut st);
        let (r, sb) = apply(&st, &cover_deposit(OWNER, &broker, usd_json("250.5")));
        assert_eq!(r, TxResult::Success);
        assert!(number_field(&broker_of(&sb), "CoverAvailable") == n("250.5"));
        assert_eq!(holds(&sb, &pseudo, &USD), usd("250.5"));
        assert_eq!(holds(&sb, &OWNER, &USD), usd("749.5"));

        // MPT.
        let mut st = state();
        let (broker, pseudo, id) = mpt_broker(&mut st);
        let asset = HeldAsset::Mpt(id);
        let (r, sb) = apply(&st, &cover_deposit(OWNER, &broker, mpt_json(&id, 400)));
        assert_eq!(r, TxResult::Success);
        assert!(number_field(&broker_of(&sb), "CoverAvailable") == n("400"));
        assert_eq!(holds(&sb, &pseudo, &asset).mantissa, 400);
        assert_eq!(holds(&sb, &OWNER, &asset).mantissa, 600);
        assert_eq!(result(&st, &cover_deposit(OWNER, &broker, mpt_json(&id, 1001))), TxResult::InsufficientFunds);
    }

    /// The asset's issuer as broker owner deposits what it issues
    /// (`accountHolds(…, FullBalance)`, LoanBrokerCoverDeposit.cpp:123-131:
    /// an issuer's spendable balance is not its line).
    #[test]
    fn cover_deposit_by_the_issuer_of_the_asset_mints_into_the_cover() {
        let mut st = state();
        // ISSUER owns the vault and the broker.
        let vault = keylet::vault_key(&ISSUER, 5);
        commit(&mut st, &tx("VaultCreate", ISSUER, json!({"Asset": usd_asset()})));
        commit(&mut st, &broker_set(ISSUER, &vault, json!({})));
        let broker = keylet::loan_broker_key(&ISSUER, 5);
        let pseudo = account_of(&get(&Sandbox::new(&st), &broker).unwrap(), "Account").unwrap();
        let (r, sb) = apply(&st, &cover_deposit(ISSUER, &broker, usd_json("77")));
        assert_eq!(r, TxResult::Success);
        assert_eq!(holds(&sb, &pseudo, &USD), usd("77"));
        assert!(number_field(&get(&sb, &broker).unwrap(), "CoverAvailable") == n("77"));
    }

    // -----------------------------------------------------------------------
    // LoanBrokerCoverWithdraw
    // -----------------------------------------------------------------------

    fn cover_withdraw(account: [u8; 20], broker: &Hash256, amount: Value) -> TxFields {
        tx("LoanBrokerCoverWithdraw", account, json!({"LoanBrokerID": key_hex(broker), "Amount": amount}))
    }

    fn cover_withdraw_to(account: [u8; 20], broker: &Hash256, amount: Value, destination: [u8; 20]) -> TxFields {
        tx(
            "LoanBrokerCoverWithdraw",
            account,
            json!({"LoanBrokerID": key_hex(broker), "Amount": amount, "Destination": hex::encode(destination)}),
        )
    }

    /// An XRP broker of OWNER holding a cover of `drops`.
    fn xrp_broker_with_cover(state: &mut LedgerState, drops: u64) -> (Hash256, [u8; 20], Hash256) {
        let made = xrp_broker(state);
        commit(state, &cover_deposit(OWNER, &made.0, json!(drops.to_string())));
        made
    }

    /// A USD broker of OWNER holding a cover of `value` (OWNER keeps the rest
    /// of its 1000 USD).
    fn usd_broker_with_cover(state: &mut LedgerState, value: &str) -> (Hash256, [u8; 20], Hash256) {
        let made = usd_broker(state);
        commit(state, &cover_deposit(OWNER, &made.0, usd_json(value)));
        made
    }

    /// An MPT broker of OWNER holding a cover of `value`.
    fn mpt_broker_with_cover(state: &mut LedgerState, value: u64) -> (Hash256, [u8; 20], [u8; 24]) {
        let made = mpt_broker(state);
        commit(state, &cover_deposit(OWNER, &made.0, mpt_json(&made.2, value)));
        made
    }

    /// LoanBrokerCoverWithdraw.cpp:26-61 (LoanBroker_test.cpp:417-421, :434-437:
    /// a zero broker id temINVALID, a zero or negative amount temBAD_AMOUNT,
    /// the zero destination temMALFORMED).
    #[test]
    fn cover_withdraw_preflight() {
        let st = state();
        let broker = Hash256([0xCD; 32]);
        assert_eq!(result(&st, &cover_withdraw(OWNER, &Hash256([0; 32]), json!("10"))), TxResult::InvalidTx);
        for amount in [json!("0"), json!("-10"), usd_json("0"), usd_json("-10")] {
            assert_eq!(result(&st, &cover_withdraw(OWNER, &broker, amount.clone())), TxResult::BadAmount, "{amount}");
        }
        assert_eq!(result(&st, &cover_withdraw_to(OWNER, &broker, json!("10"), [0u8; 20])), TxResult::Malformed);
        // sfCredentialIDs: needs Credentials and fixCleanup3_4_0; then the
        // array's own rules (credentials::checkFields).
        let with_credentials = |ids: Value| {
            let mut t = cover_withdraw(OWNER, &broker, json!("10"));
            t.fields["CredentialIDs"] = ids;
            t
        };
        for off in [am::CREDENTIALS, am::FIX_CLEANUP_3_4_0] {
            let st = state_with(&rules_without(&[off]));
            assert_eq!(result(&st, &with_credentials(json!(["A1".repeat(32)]))), TxResult::Disabled, "{off}");
            assert_ne!(result(&st, &cover_withdraw(OWNER, &broker, json!("10"))), TxResult::Disabled);
        }
        assert_eq!(result(&st, &with_credentials(json!([]))), TxResult::Malformed, "an empty array");
        assert_eq!(result(&st, &with_credentials(json!(["A1".repeat(32), "A1".repeat(32)]))), TxResult::Malformed, "a repeated id");
        assert_eq!(result(&st, &cover_withdraw(OWNER, &broker, json!("10"))), TxResult::NoEntry, "past preflight");
    }

    /// LoanBrokerCoverWithdraw.cpp:75-104 (LoanBroker_test.cpp:418, :421):
    /// the pseudo-account destination comes first, then the broker, its
    /// owner, the asset.
    #[test]
    fn cover_withdraw_preclaim_destination_broker_owner_and_asset() {
        let mut st = state();
        let (broker, pseudo, vault) = xrp_broker_with_cover(&mut st, 10_000);
        let vault_pseudo = account_of(&get(&Sandbox::new(&st), &vault).unwrap(), "Account").unwrap();
        for dst in [pseudo, vault_pseudo] {
            assert_eq!(result(&st, &cover_withdraw_to(OWNER, &broker, json!("10"), dst)), TxResult::TecPseudoAccount);
            // Before the broker is even looked up.
            assert_eq!(result(&st, &cover_withdraw_to(OWNER, &Hash256([0xEE; 32]), json!("10"), dst)), TxResult::TecPseudoAccount);
        }
        assert_eq!(result(&st, &cover_withdraw(OWNER, &vault, json!("10"))), TxResult::NoEntry, "a vault's id");
        assert_eq!(result(&st, &cover_withdraw(BOB, &broker, json!("10"))), TxResult::NoPermission);
        assert_eq!(result(&st, &cover_withdraw(OWNER, &broker, usd_json("10"))), TxResult::WrongAsset);
        assert_eq!(result(&st, &cover_withdraw(OWNER, &broker, json!("10"))), TxResult::Success);
    }

    /// LoanBrokerCoverWithdraw.cpp:127-148 (LoanBroker_test.cpp:424-432: to a
    /// bystander with no line tecNO_LINE, with no MPToken tecNO_AUTH): a
    /// payout to someone else is a transfer.
    #[test]
    fn cover_withdraw_to_a_third_party_is_checked_as_a_transfer() {
        // XRP: the destination must exist; a required tag must be given.
        let mut st = state();
        let (broker, _, _) = xrp_broker_with_cover(&mut st, 10_000);
        assert_eq!(result(&st, &cover_withdraw_to(OWNER, &broker, json!("10"), [0x99; 20])), TxResult::NoDst);
        let mut tagged = st.clone();
        edit(&mut tagged, keylet::account_root_key(&BOB), |a| a["Flags"] = json!(LSF_REQUIRE_DEST_TAG));
        let to_bob = cover_withdraw_to(OWNER, &broker, json!("10"), BOB);
        assert_eq!(result(&tagged, &to_bob), TxResult::DstTagNeeded);
        let mut with_tag = to_bob.clone();
        with_tag.fields["DestinationTag"] = json!(3);
        assert_eq!(result(&tagged, &with_tag), TxResult::Success);
        // A destination with lsfDepositAuth that has not preauthorized the owner.
        let mut guarded = st.clone();
        edit(&mut guarded, keylet::account_root_key(&BOB), |a| a["Flags"] = json!(0x0100_0000u64));
        assert_eq!(result(&guarded, &to_bob), TxResult::NoPermission);
        assert_eq!(result(&st, &to_bob), TxResult::Success);

        // IOU: no line at the destination.
        let mut st = state();
        let (broker, _, _) = usd_broker_with_cover(&mut st, "100");
        let to_bob = cover_withdraw_to(OWNER, &broker, usd_json("1"), BOB);
        assert_eq!(result(&st, &to_bob), TxResult::NoLine);
        // A line with room takes it; one without room does not.
        let mut roomy = st.clone();
        line(&mut roomy, BOB, 0, "0", "50");
        assert_eq!(result(&roomy, &to_bob), TxResult::Success);
        assert_eq!(result(&roomy, &cover_withdraw_to(OWNER, &broker, usd_json("51"), BOB)), TxResult::NoLine, "above the limit");

        // MPT: no MPToken at the destination.
        let mut st = state();
        let (broker, _, id) = mpt_broker_with_cover(&mut st, 100);
        let to_bob = cover_withdraw_to(OWNER, &broker, mpt_json(&id, 1), BOB);
        assert_eq!(result(&st, &to_bob), TxResult::NoAuth);
        let mut holder = st.clone();
        token(&mut holder, &id, BOB, 0, 0);
        assert_eq!(result(&holder, &to_bob), TxResult::Success);
    }

    /// LoanBrokerCoverWithdraw.cpp:150-167 (LoanBroker_test.cpp:2094
    /// "LoanBrokerCoverWithdraw IOU self-withdrawal while individually
    /// frozen", :2169 "IOU freeze checks", :2281 "MPT lock checks").
    #[test]
    fn cover_withdraw_freezes() {
        let mut st = state();
        let (broker, pseudo, _) = usd_broker_with_cover(&mut st, "100");
        line(&mut st, BOB, 0, "0", "50");
        let to_self = cover_withdraw(OWNER, &broker, usd_json("1"));
        let to_bob = cover_withdraw_to(OWNER, &broker, usd_json("1"), BOB);
        let to_issuer = cover_withdraw_to(OWNER, &broker, usd_json("1"), ISSUER);
        let freeze = |st: &LedgerState, holder: [u8; 20]| {
            let mut st = st.clone();
            let issuer_side = if ISSUER < holder { LSF_LOW_FREEZE } else { LSF_HIGH_FREEZE };
            edit(&mut st, keylet::ripple_state_key(&holder, &ISSUER, &USD_CODE), |l| {
                l["Flags"] = json!(l["Flags"].as_u64().unwrap() | issuer_side)
            });
            st
        };
        // A global freeze stops everything but a payout to the issuer.
        let mut global = st.clone();
        edit(&mut global, keylet::account_root_key(&ISSUER), |a| a["Flags"] = json!(a["Flags"].as_u64().unwrap() | LSF_GLOBAL_FREEZE));
        assert_eq!(result(&global, &to_self), TxResult::Frozen);
        assert_eq!(result(&global, &to_bob), TxResult::Frozen);
        assert_eq!(result(&global, &to_issuer), TxResult::Success);
        // The pseudo-account frozen: nothing leaves, except to the issuer.
        let frozen_pseudo = freeze(&st, pseudo);
        assert_eq!(result(&frozen_pseudo, &to_self), TxResult::Frozen);
        assert_eq!(result(&frozen_pseudo, &to_issuer), TxResult::Success);
        // The owner individually frozen: it may still take its own funds
        // out, but not send them on.
        let frozen_owner = freeze(&st, OWNER);
        assert_eq!(result(&frozen_owner, &to_self), TxResult::Success);
        assert_eq!(result(&frozen_owner, &to_bob), TxResult::Frozen);

        // MPT: the issuance locked.
        let mut st = state();
        let (broker, _, id) = mpt_broker_with_cover(&mut st, 100);
        edit(&mut st, keylet::mpt_issuance_key(&id), |i| i["Flags"] = json!(i["Flags"].as_u64().unwrap() | mpt::LSF_MPT_LOCKED));
        assert_eq!(result(&st, &cover_withdraw(OWNER, &broker, mpt_json(&id, 1))), TxResult::Locked);
        assert_eq!(result(&st, &cover_withdraw_to(OWNER, &broker, mpt_json(&id, 1), ISSUER)), TxResult::Success);
    }

    /// LoanBrokerCoverWithdraw.cpp:106-119 with fixCleanup3_2_0: a cover
    /// withdrawal does not consult lsfMPTCanTransfer ("an issuer cannot trap
    /// a broker's first-loss capital"); before the fix it did.
    #[test]
    fn cover_withdraw_waives_mpt_can_transfer_under_fix_cleanup_3_2_0() {
        let build = |rules: &[&'static str]| {
            let mut st = state_with(rules);
            let (broker, _, id) = mpt_broker_with_cover(&mut st, 100);
            token(&mut st, &id, BOB, 0, 0);
            edit(&mut st, keylet::mpt_issuance_key(&id), |i| {
                i["Flags"] = json!(i["Flags"].as_u64().unwrap() & !mpt::LSF_MPT_CAN_TRANSFER)
            });
            (st, broker, id)
        };
        let (st, broker, id) = build(&MAINNET_WITH_LENDING);
        assert_eq!(result(&st, &cover_withdraw_to(OWNER, &broker, mpt_json(&id, 1), BOB)), TxResult::Success);
        let (st, broker, id) = build(&rules_without(&[am::FIX_CLEANUP_3_2_0]));
        assert_eq!(result(&st, &cover_withdraw_to(OWNER, &broker, mpt_json(&id, 1), BOB)), TxResult::NoAuth);
    }

    /// LoanBrokerCoverWithdraw.cpp:169-203 (LoanBroker_test.cpp:422: more
    /// than the cover tecINSUFFICIENT_FUNDS): the cover may not fall below
    /// sfDebtTotal × sfCoverRateMinimum, rounded up at the vault's scale.
    #[test]
    fn cover_withdraw_keeps_the_minimum_cover() {
        let mut st = state();
        let (broker, _, _) = xrp_broker_with_cover(&mut st, 10_000);
        assert_eq!(result(&st, &cover_withdraw(OWNER, &broker, json!("10001"))), TxResult::InsufficientFunds, "more than the cover");
        assert_eq!(result(&st, &cover_withdraw(OWNER, &broker, json!("10000"))), TxResult::Success, "all of it, with no debt");
        // A debt of 15 drops at 10%: the minimum is 1.5 → 2 drops.
        edit(&mut st, broker, |b| {
            b["DebtTotal"] = json!("15");
            b["CoverRateMinimum"] = json!(10_000);
            b["CoverRateLiquidation"] = json!(5_000);
        });
        assert_eq!(result(&st, &cover_withdraw(OWNER, &broker, json!("9999"))), TxResult::InsufficientFunds, "leaves 1 < 2");
        assert_eq!(result(&st, &cover_withdraw(OWNER, &broker, json!("9998"))), TxResult::Success, "leaves exactly the minimum");
        // The pseudo-account holding less than the cover says: refused.
        let mut short = st.clone();
        edit(&mut short, broker, |b| b["CoverAvailable"] = json!("20000"));
        assert_eq!(result(&short, &cover_withdraw(OWNER, &broker, json!("15000"))), TxResult::InsufficientFunds);
    }

    /// LoanBrokerCoverWithdraw.cpp:106-109 → LendingHelpers.cpp:38-67
    /// (LoanBroker_test.cpp:2886 "Cover precision guard: Withdraw").
    #[test]
    fn cover_withdraw_of_dust_below_the_cover_scale_is_precision_loss() {
        let mut st = state();
        line(&mut st, OWNER, 0, "200000", "1000000");
        let (vault, _) = make_vault(&mut st, json!({"Asset": usd_asset()}));
        let (broker, _) = make_broker(&mut st, &vault, json!({}));
        commit(&mut st, &cover_deposit(OWNER, &broker, usd_json("100000")));
        assert_eq!(result(&st, &cover_withdraw(OWNER, &broker, usd_json("0.00000000001"))), TxResult::PrecisionLoss);
        assert_eq!(result(&st, &cover_withdraw(OWNER, &broker, usd_json("0.0000000001"))), TxResult::Success);
    }

    /// LoanBrokerCoverWithdraw.cpp:208-243 (LoanBroker_test.cpp:438-455:
    /// withdraw 7 of 10, then 1 to evan, then 1 to evan with a tag): the
    /// cover falls and the asset arrives — XRP, IOU, MPT; self and third party.
    #[test]
    fn cover_withdraw_pays_out_of_the_pseudo_account_and_lowers_the_cover() {
        // XRP to oneself.
        let mut st = state();
        let (broker, pseudo, _) = xrp_broker_with_cover(&mut st, 10_000);
        let before = Sandbox::new(&st);
        let (r, sb) = apply(&st, &cover_withdraw(OWNER, &broker, json!("7000")));
        assert_eq!(r, TxResult::Success);
        assert!(number_field(&broker_of(&sb), "CoverAvailable") == n("3000"));
        assert_eq!(xrp_of(&sb, &pseudo), 3_000);
        assert_eq!(xrp_of(&sb, &OWNER), xrp_of(&before, &OWNER) + 7_000 - FEE);
        // XRP to BOB.
        let (r, sb) = apply(&st, &cover_withdraw_to(OWNER, &broker, json!("1000"), BOB));
        assert_eq!(r, TxResult::Success);
        assert_eq!(xrp_of(&sb, &BOB), BALANCE + 1_000);
        assert_eq!(xrp_of(&sb, &OWNER), xrp_of(&before, &OWNER) - FEE);
        assert!(number_field(&broker_of(&sb), "CoverAvailable") == n("9000"));
        // All of it: the field goes (SoeDefault).
        let (r, sb) = apply(&st, &cover_withdraw(OWNER, &broker, json!("10000")));
        assert_eq!(r, TxResult::Success);
        assert!(broker_of(&sb).get("CoverAvailable").is_none());
        assert_eq!(xrp_of(&sb, &pseudo), 0);

        // IOU, to oneself and to BOB.
        let mut st = state();
        let (broker, pseudo, _) = usd_broker_with_cover(&mut st, "100");
        line(&mut st, BOB, 0, "0", "50");
        let (r, sb) = apply(&st, &cover_withdraw(OWNER, &broker, usd_json("70.5")));
        assert_eq!(r, TxResult::Success);
        assert!(number_field(&broker_of(&sb), "CoverAvailable") == n("29.5"));
        assert_eq!(holds(&sb, &pseudo, &USD), usd("29.5"));
        assert_eq!(holds(&sb, &OWNER, &USD), usd("970.5"));
        let (r, sb) = apply(&st, &cover_withdraw_to(OWNER, &broker, usd_json("10"), BOB));
        assert_eq!(r, TxResult::Success);
        assert_eq!(holds(&sb, &BOB, &USD), usd("10"));
        assert_eq!(holds(&sb, &pseudo, &USD), usd("90"));
        assert_eq!(holds(&sb, &OWNER, &USD), usd("900"), "the owner's own balance is untouched");

        // MPT.
        let mut st = state();
        let (broker, pseudo, id) = mpt_broker_with_cover(&mut st, 100);
        let asset = HeldAsset::Mpt(id);
        let (r, sb) = apply(&st, &cover_withdraw(OWNER, &broker, mpt_json(&id, 60)));
        assert_eq!(r, TxResult::Success);
        assert!(number_field(&broker_of(&sb), "CoverAvailable") == n("40"));
        assert_eq!(holds(&sb, &pseudo, &asset).mantissa, 40);
        assert_eq!(holds(&sb, &OWNER, &asset).mantissa, 960);
    }

    /// LoanBrokerCoverWithdraw.cpp:144-148 and View.cpp:535-598 (`doWithdraw`):
    /// an owner that no longer has the holding gets one with the payout.
    #[test]
    fn cover_withdraw_to_oneself_creates_the_missing_holding() {
        let mut st = state();
        let (broker, _, id) = mpt_broker_with_cover(&mut st, 100);
        let asset = HeldAsset::Mpt(id);
        let ikey = keylet::mpt_issuance_key(&id);
        // The owner's MPToken emptied and gone.
        edit(&mut st, ikey, |i| i["OutstandingAmount"] = json!("100"));
        st.state_map.delete(&keylet::mptoken_key(&ikey, &OWNER)).unwrap();
        let before = owners(&Sandbox::new(&st), &OWNER);
        let (r, sb) = apply(&st, &cover_withdraw(OWNER, &broker, mpt_json(&id, 60)));
        assert_eq!(r, TxResult::Success);
        assert_eq!(holds(&sb, &OWNER, &asset).mantissa, 60);
        assert_eq!(owners(&sb, &OWNER), before + 1, "the new MPToken is the owner's");
        // An issuance that takes no new holders: refused in preclaim under
        // fixCleanup3_4_0 (canAddHolding), not half-way through do_apply.
        edit(&mut st, ikey, |i| i["Flags"] = json!(i["Flags"].as_u64().unwrap() & !mpt::LSF_MPT_CAN_TRANSFER));
        assert_eq!(result(&st, &cover_withdraw(OWNER, &broker, mpt_json(&id, 60))), TxResult::NoAuth);
    }

    // -----------------------------------------------------------------------
    // LoanBrokerCoverClawback
    // -----------------------------------------------------------------------

    /// A LoanBrokerCoverClawback with whichever of the two fields is given.
    fn cover_clawback(account: [u8; 20], broker: Option<&Hash256>, amount: Option<Value>) -> TxFields {
        let mut fields = json!({});
        if let Some(broker) = broker {
            fields["LoanBrokerID"] = json!(key_hex(broker));
        }
        if let Some(amount) = amount {
            fields["Amount"] = amount;
        }
        tx("LoanBrokerCoverClawback", account, fields)
    }

    /// An IOU amount naming `holder` as its issuer: the trust line seen from
    /// the real issuer's side.
    fn held_by(holder: &[u8; 20], value: &str) -> Value {
        json!({"currency": hex::encode_upper(USD_CODE), "issuer": hex::encode(holder), "value": value})
    }

    /// LoanBrokerCoverClawback.cpp:42-84 (LoanBroker_test.cpp:331-338: neither
    /// field, a zero broker id, an XRP amount, a negative amount, an MPT
    /// amount without a broker id).
    #[test]
    fn cover_clawback_preflight() {
        let st = state();
        let broker = Hash256([0xCD; 32]);
        let id = mptid(7, ISSUER);
        assert_eq!(result(&st, &cover_clawback(ISSUER, None, None)), TxResult::InvalidTx, "one of the two is needed");
        assert_eq!(result(&st, &cover_clawback(ISSUER, Some(&Hash256([0; 32])), None)), TxResult::InvalidTx);
        assert_eq!(result(&st, &cover_clawback(ISSUER, Some(&broker), Some(json!("1000")))), TxResult::BadAmount, "XRP");
        assert_eq!(result(&st, &cover_clawback(ISSUER, None, Some(json!("1000")))), TxResult::BadAmount, "XRP, no broker id");
        assert_eq!(result(&st, &cover_clawback(ISSUER, Some(&broker), Some(usd_json("-10")))), TxResult::BadAmount);
        assert_eq!(result(&st, &cover_clawback(ISSUER, None, Some(mpt_json(&id, 1)))), TxResult::InvalidTx, "an MPT needs the broker id");
        // Without a broker id the amount's issuer is the holder: not the
        // submitter itself.
        assert_eq!(result(&st, &cover_clawback(ISSUER, None, Some(usd_json("1")))), TxResult::InvalidTx);
        // Zero is "all"; past preflight.
        assert_eq!(result(&st, &cover_clawback(ISSUER, Some(&broker), Some(usd_json("0")))), TxResult::NoEntry);
        assert_eq!(result(&st, &cover_clawback(ISSUER, Some(&broker), None)), TxResult::NoEntry);
    }

    /// LoanBrokerCoverClawback.cpp:86-127, :236-294 (LoanBroker_test.cpp:339-370):
    /// finding the broker, who may claw back, which asset.
    #[test]
    fn cover_clawback_preclaim_broker_issuer_and_asset() {
        let mut st = state();
        let (broker, pseudo, vault) = usd_broker_with_cover(&mut st, "100");
        let vault_pseudo = account_of(&get(&Sandbox::new(&st), &vault).unwrap(), "Account").unwrap();
        // No broker id: the amount's issuer must be a broker's pseudo-account.
        assert_eq!(result(&st, &cover_clawback(ISSUER, None, Some(held_by(&[0x99; 20], "1")))), TxResult::NoEntry, "no such account");
        assert_eq!(result(&st, &cover_clawback(ISSUER, None, Some(held_by(&BOB, "1")))), TxResult::ObjectNotFound, "an ordinary account");
        assert_eq!(
            result(&st, &cover_clawback(ISSUER, None, Some(held_by(&vault_pseudo, "1")))),
            TxResult::ObjectNotFound,
            "a pseudo-account, but a vault's"
        );
        assert_eq!(result(&st, &cover_clawback(ISSUER, None, Some(held_by(&pseudo, "1")))), TxResult::Success);
        // A broker id that is not a broker.
        assert_eq!(result(&st, &cover_clawback(ISSUER, Some(&vault), None)), TxResult::NoEntry);
        // Only the issuer.
        assert_eq!(result(&st, &cover_clawback(OWNER, Some(&broker), None)), TxResult::NoPermission);
        assert_eq!(result(&st, &cover_clawback(BOB, Some(&broker), Some(usd_json("1")))), TxResult::NoPermission);
        // With a broker id the amount may name either end of the line —
        // but no third account and no other currency.
        assert_eq!(result(&st, &cover_clawback(ISSUER, Some(&broker), Some(usd_json("1")))), TxResult::Success);
        assert_eq!(result(&st, &cover_clawback(ISSUER, Some(&broker), Some(held_by(&pseudo, "1")))), TxResult::Success);
        assert_eq!(result(&st, &cover_clawback(ISSUER, Some(&broker), Some(held_by(&vault_pseudo, "1")))), TxResult::WrongAsset);
        let wat = hex::encode_upper([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x57, 0x41, 0x54, 0, 0, 0, 0, 0]);
        let other_currency = json!({"currency": wat, "issuer": hex::encode(pseudo), "value": "1"});
        assert_eq!(result(&st, &cover_clawback(ISSUER, Some(&broker), Some(other_currency))), TxResult::WrongAsset);
        let mpt_amount = mpt_json(&mptid(7, ISSUER), 1);
        assert_eq!(result(&st, &cover_clawback(ISSUER, Some(&broker), Some(mpt_amount))), TxResult::WrongAsset);

        // XRP is never clawed back.
        let mut st = state();
        let (broker, _, _) = xrp_broker_with_cover(&mut st, 10_000);
        assert_eq!(result(&st, &cover_clawback(ISSUER, Some(&broker), None)), TxResult::NoPermission);
        assert_eq!(result(&st, &cover_clawback(OWNER, Some(&broker), None)), TxResult::NoPermission);
    }

    /// LoanBrokerCoverClawback.cpp:203-234, :296-334 (LoanBroker_test.cpp:371-377:
    /// nothing deposited tecINSUFFICIENT_FUNDS): the issuer's clawback
    /// permission, and something above the minimum cover to take.
    #[test]
    fn cover_clawback_preclaim_permission_and_funds() {
        // No cover at all.
        let mut st = state();
        let (broker, _, _) = usd_broker(&mut st);
        assert_eq!(result(&st, &cover_clawback(ISSUER, Some(&broker), Some(usd_json("10")))), TxResult::InsufficientFunds);
        // A cover exactly at its minimum.
        let mut st = state();
        let (broker, _, _) = usd_broker_with_cover(&mut st, "100");
        let mut at_minimum = st.clone();
        edit(&mut at_minimum, broker, |b| {
            b["DebtTotal"] = json!("1000");
            b["CoverRateMinimum"] = json!(10_000);
            b["CoverRateLiquidation"] = json!(5_000);
        });
        assert_eq!(result(&at_minimum, &cover_clawback(ISSUER, Some(&broker), None)), TxResult::InsufficientFunds);
        // The IOU issuer must allow clawback and must not have given up freezing.
        let mut no_clawback = st.clone();
        edit(&mut no_clawback, keylet::account_root_key(&ISSUER), |a| a["Flags"] = json!(LSF_DEFAULT_RIPPLE));
        assert_eq!(result(&no_clawback, &cover_clawback(ISSUER, Some(&broker), None)), TxResult::NoPermission);
        let mut no_freeze = st.clone();
        edit(&mut no_freeze, keylet::account_root_key(&ISSUER), |a| {
            a["Flags"] = json!(LSF_DEFAULT_RIPPLE | LSF_ALLOW_TRUST_LINE_CLAWBACK | LSF_NO_FREEZE)
        });
        assert_eq!(result(&no_freeze, &cover_clawback(ISSUER, Some(&broker), None)), TxResult::NoPermission);
        assert_eq!(result(&st, &cover_clawback(ISSUER, Some(&broker), None)), TxResult::Success);
        // The pseudo-account holding less than the cover says: tecINTERNAL.
        let mut short = st.clone();
        edit(&mut short, broker, |b| b["CoverAvailable"] = json!("200"));
        assert_eq!(result(&short, &cover_clawback(ISSUER, Some(&broker), None)), TxResult::TecInternal);

        // An MPT must be clawable.
        let mut st = state();
        let (broker, _, id) = mpt_broker_with_cover(&mut st, 100);
        assert_eq!(result(&st, &cover_clawback(ISSUER, Some(&broker), None)), TxResult::Success);
        edit(&mut st, keylet::mpt_issuance_key(&id), |i| i["Flags"] = json!(mpt::LSF_MPT_CAN_TRANSFER));
        assert_eq!(result(&st, &cover_clawback(ISSUER, Some(&broker), None)), TxResult::NoPermission);
    }

    /// LoanBrokerCoverClawback.cpp:159-196, :336-376 (LoanBroker_test.cpp:457-
    /// 508: 2 of 6 clawed back; then all of it four ways — no amount, zero,
    /// the exact amount, more than there is).
    #[test]
    fn cover_clawback_takes_the_amount_or_everything_above_the_minimum() {
        let mut st = state();
        let (broker, pseudo, _) = usd_broker_with_cover(&mut st, "100");
        let before = Sandbox::new(&st);
        assert_eq!(holds(&before, &pseudo, &USD), usd("100"));
        let (r, sb) = apply(&st, &cover_clawback(ISSUER, Some(&broker), Some(usd_json("30"))));
        assert_eq!(r, TxResult::Success);
        assert!(number_field(&broker_of(&sb), "CoverAvailable") == n("70"));
        assert_eq!(holds(&sb, &pseudo, &USD), usd("70"), "the issuer redeemed 30");
        assert_eq!(holds(&sb, &OWNER, &USD), holds(&before, &OWNER, &USD), "the owner's own balance is not touched");
        assert_eq!(xrp_of(&sb, &ISSUER), BALANCE - FEE);
        // Everything, four ways.
        for amount in [None, Some(usd_json("0")), Some(usd_json("100")), Some(usd_json("5000")), Some(held_by(&pseudo, "0"))] {
            let (r, sb) = apply(&st, &cover_clawback(ISSUER, Some(&broker), amount.clone()));
            assert_eq!(r, TxResult::Success, "{amount:?}");
            assert!(broker_of(&sb).get("CoverAvailable").is_none(), "{amount:?}: the cover is gone (SoeDefault)");
            assert!(holds(&sb, &pseudo, &USD).is_zero(), "{amount:?}");
            assert!(sb.exists(&keylet::ripple_state_key(&pseudo, &ISSUER, &USD_CODE)), "the pseudo-account's line stays");
        }
        // Without the broker id, through the pseudo-account's side of the line.
        let (r, sb) = apply(&st, &cover_clawback(ISSUER, None, Some(held_by(&pseudo, "12.5"))));
        assert_eq!(r, TxResult::Success);
        assert!(number_field(&broker_of(&sb), "CoverAvailable") == n("87.5"));
        assert_eq!(holds(&sb, &pseudo, &USD), usd("87.5"));

        // A debt keeps its minimum cover: 10% of 250 = 25 stays.
        edit(&mut st, broker, |b| {
            b["DebtTotal"] = json!("250");
            b["CoverRateMinimum"] = json!(10_000);
            b["CoverRateLiquidation"] = json!(5_000);
        });
        for amount in [None, Some(usd_json("5000")), Some(usd_json("75"))] {
            let (r, sb) = apply(&st, &cover_clawback(ISSUER, Some(&broker), amount.clone()));
            assert_eq!(r, TxResult::Success, "{amount:?}");
            assert!(number_field(&broker_of(&sb), "CoverAvailable") == n("25"), "{amount:?}");
            assert_eq!(holds(&sb, &pseudo, &USD), usd("25"), "{amount:?}");
        }
        let (_, sb) = apply(&st, &cover_clawback(ISSUER, Some(&broker), Some(usd_json("74"))));
        assert!(number_field(&broker_of(&sb), "CoverAvailable") == n("26"));

        // MPT.
        let mut st = state();
        let (broker, pseudo, id) = mpt_broker_with_cover(&mut st, 100);
        let asset = HeldAsset::Mpt(id);
        let (r, sb) = apply(&st, &cover_clawback(ISSUER, Some(&broker), Some(mpt_json(&id, 40))));
        assert_eq!(r, TxResult::Success);
        assert!(number_field(&broker_of(&sb), "CoverAvailable") == n("60"));
        assert_eq!(holds(&sb, &pseudo, &asset).mantissa, 60);
        let issuance = get(&sb, &keylet::mpt_issuance_key(&id)).unwrap();
        assert_eq!(mpt::dec_field(&issuance, "OutstandingAmount"), 1000 - 40, "clawed-back MPT is redeemed");
        let (r, sb) = apply(&st, &cover_clawback(ISSUER, Some(&broker), None));
        assert_eq!(r, TxResult::Success);
        assert!(broker_of(&sb).get("CoverAvailable").is_none());
        assert_eq!(holds(&sb, &pseudo, &asset).mantissa, 0);
    }

    /// LoanBrokerCoverClawback.cpp:311-313 → LendingHelpers.cpp:38-67
    /// (LoanBroker_test.cpp:2903 "Cover precision guard: Clawback").
    #[test]
    fn cover_clawback_of_dust_below_the_cover_scale_is_precision_loss() {
        let mut st = state();
        line(&mut st, OWNER, 0, "200000", "1000000");
        let (vault, _) = make_vault(&mut st, json!({"Asset": usd_asset()}));
        let (broker, _) = make_broker(&mut st, &vault, json!({}));
        commit(&mut st, &cover_deposit(OWNER, &broker, usd_json("100000")));
        assert_eq!(result(&st, &cover_clawback(ISSUER, Some(&broker), Some(usd_json("0.00000000001")))), TxResult::PrecisionLoss);
        assert_eq!(result(&st, &cover_clawback(ISSUER, Some(&broker), Some(usd_json("0.0000000001")))), TxResult::Success);
    }

    // -----------------------------------------------------------------------
    // LoanBrokerDelete
    // -----------------------------------------------------------------------

    fn broker_delete(account: [u8; 20], broker: &Hash256) -> TxFields {
        tx("LoanBrokerDelete", account, json!({"LoanBrokerID": key_hex(broker)}))
    }

    /// LoanBrokerDelete.cpp:28-35, :37-93 (LoanBroker_test.cpp:501-503: a
    /// vault's id tecNO_ENTRY, not the owner tecNO_PERMISSION; :1185 "Invalid
    /// LoanBrokerDelete").
    #[test]
    fn loan_broker_delete_preflight_and_preclaim() {
        let mut st = state();
        let (broker, _, vault) = xrp_broker(&mut st);
        assert_eq!(result(&st, &broker_delete(OWNER, &Hash256([0; 32]))), TxResult::InvalidTx);
        assert_eq!(result(&st, &broker_delete(OWNER, &vault)), TxResult::NoEntry, "a vault's id");
        assert_eq!(result(&st, &broker_delete(BOB, &broker)), TxResult::NoPermission);
        // Live loans.
        let mut with_loans = st.clone();
        edit(&mut with_loans, broker, |b| b["OwnerCount"] = json!(1));
        assert_eq!(result(&with_loans, &broker_delete(OWNER, &broker)), TxResult::HasObligations);
        // Debt left.
        let mut in_debt = st.clone();
        edit(&mut in_debt, broker, |b| b["DebtTotal"] = json!("1"));
        assert_eq!(result(&in_debt, &broker_delete(OWNER, &broker)), TxResult::HasObligations);
        // The vault gone from under the broker.
        let mut orphan = st.clone();
        edit(&mut orphan, broker, |b| b["VaultID"] = json!("EE".repeat(32)));
        assert_eq!(result(&orphan, &broker_delete(OWNER, &broker)), TxResult::TefBadLedger);
        assert_eq!(result(&st, &broker_delete(OWNER, &broker)), TxResult::Success);
    }

    /// LoanBrokerDelete.cpp:75-92: a sfDebtTotal that is dust below the
    /// vault's scale does not hold the broker back.
    #[test]
    fn loan_broker_delete_ignores_debt_that_rounds_to_zero_at_the_vault_scale() {
        let mut st = state();
        let (broker, _, vault) = usd_broker(&mut st);
        // A vault total of 100000 has its sixteenth digit at 1e-10.
        edit(&mut st, vault, |v| {
            v["AssetsTotal"] = json!("100000");
            v["AssetsAvailable"] = json!("100000");
        });
        let with_debt = |debt: &str| {
            let mut st = st.clone();
            edit(&mut st, broker, |b| b["DebtTotal"] = json!(debt));
            result(&st, &broker_delete(OWNER, &broker))
        };
        assert_eq!(with_debt("0.00000000009"), TxResult::Success, "below one unit of the scale: truncated to zero");
        assert_eq!(with_debt("0.0000000001"), TxResult::HasObligations, "one unit of the scale");
        assert_eq!(with_debt("5"), TxResult::HasObligations);
    }

    /// LoanBrokerDelete.cpp:95-118 (LoanBroker_test.cpp:1646 "LoanBrokerDelete
    /// - locked broker pseudo-account MPT", :1767 "frozen broker
    /// pseudo-account IOU"): with cover to pay out, the owner must be able to
    /// receive it and — fixCleanup3_2_0 — the pseudo-account to send it.
    #[test]
    fn loan_broker_delete_freezes() {
        let mut st = state();
        let (broker, pseudo, _) = usd_broker_with_cover(&mut st, "100");
        let delete = broker_delete(OWNER, &broker);
        let set_flags = |st: &LedgerState, holder: [u8; 20], deep: bool| {
            let mut st = st.clone();
            let holder_low = holder < ISSUER;
            // The issuer's freeze; a deep freeze adds lsfLow/HighDeepFreeze.
            let (freeze, deep_freeze) = if holder_low { (LSF_HIGH_FREEZE, 0x0400_0000u64) } else { (LSF_LOW_FREEZE, 0x0200_0000u64) };
            edit(&mut st, keylet::ripple_state_key(&holder, &ISSUER, &USD_CODE), |l| {
                l["Flags"] = json!(l["Flags"].as_u64().unwrap() | freeze | if deep { deep_freeze } else { 0 })
            });
            st
        };
        assert_eq!(result(&set_flags(&st, OWNER, true), &delete), TxResult::Frozen, "the owner deep-frozen");
        assert_eq!(result(&set_flags(&st, OWNER, false), &delete), TxResult::Success, "a plain freeze still receives");
        assert_eq!(result(&set_flags(&st, pseudo, false), &delete), TxResult::Frozen, "the pseudo-account frozen");
        // Nothing to pay out: no freeze matters.
        let mut st = state();
        let (broker, pseudo, _) = usd_broker(&mut st);
        let delete = broker_delete(OWNER, &broker);
        assert_eq!(result(&set_flags(&st, pseudo, true), &delete), TxResult::Success);
        // Before fixCleanup3_2_0 the pseudo-account's freeze was not asked.
        let mut st = state_with(&rules_without(&[am::FIX_CLEANUP_3_2_0]));
        let (broker, pseudo, _) = usd_broker_with_cover(&mut st, "100");
        let delete = broker_delete(OWNER, &broker);
        assert_eq!(result(&set_flags(&st, pseudo, false), &delete), TxResult::Success);
        assert_eq!(result(&set_flags(&st, OWNER, true), &delete), TxResult::Frozen);

        // MPT: the issuance locked.
        let mut st = state();
        let (broker, _, id) = mpt_broker_with_cover(&mut st, 100);
        edit(&mut st, keylet::mpt_issuance_key(&id), |i| i["Flags"] = json!(i["Flags"].as_u64().unwrap() | mpt::LSF_MPT_LOCKED));
        assert_eq!(result(&st, &broker_delete(OWNER, &broker)), TxResult::Locked);
    }

    /// LoanBrokerDelete.cpp:123-200 (LoanBroker_test.cpp:509-548: the broker
    /// and its pseudo-account are gone, the owner has the cover back): XRP,
    /// IOU and MPT, with and without cover.
    #[test]
    fn loan_broker_delete_returns_the_cover_and_removes_everything_the_set_made() {
        // XRP, with cover.
        let mut st = state();
        let (vault, vault_pseudo) = xrp_vault(&mut st);
        let before_broker = Sandbox::new(&st);
        let (owners_before, xrp_before) = (owners(&before_broker, &OWNER), xrp_of(&before_broker, &OWNER));
        let (broker, pseudo) = make_broker(&mut st, &vault, json!({}));
        commit(&mut st, &cover_deposit(OWNER, &broker, json!("10000")));
        let (r, sb) = apply(&st, &broker_delete(OWNER, &broker));
        assert_eq!(r, TxResult::Success);
        assert!(!sb.exists(&broker), "the LoanBroker");
        assert!(!sb.exists(&keylet::account_root_key(&pseudo)), "its pseudo-account");
        assert!(!dir_has(&sb, &OWNER, &broker) && !dir_has(&sb, &vault_pseudo, &broker), "both directory links");
        assert_eq!(owners(&sb, &OWNER), owners_before, "the two reserve units are back");
        assert_eq!(xrp_of(&sb, &OWNER), xrp_before - 3 * FEE, "the cover is back; three fees were paid");
        assert!(sb.exists(&vault), "the vault stays");

        // IOU, with cover: the pseudo-account's line goes too.
        let mut st = state();
        let (broker, pseudo, vault) = usd_broker_with_cover(&mut st, "100");
        let vault_pseudo = account_of(&get(&Sandbox::new(&st), &vault).unwrap(), "Account").unwrap();
        let (r, sb) = apply(&st, &broker_delete(OWNER, &broker));
        assert_eq!(r, TxResult::Success);
        assert_eq!(holds(&sb, &OWNER, &USD), usd("1000"));
        assert!(!sb.exists(&keylet::ripple_state_key(&pseudo, &ISSUER, &USD_CODE)), "the pseudo-account's line");
        assert!(!sb.exists(&keylet::owner_dir_key(&pseudo)), "the pseudo-account's directory");
        assert!(!sb.exists(&keylet::account_root_key(&pseudo)) && !sb.exists(&broker));
        assert!(!dir_has(&sb, &vault_pseudo, &broker));
        // Without cover.
        let mut st = state();
        let (broker, pseudo, _) = usd_broker(&mut st);
        let (r, sb) = apply(&st, &broker_delete(OWNER, &broker));
        assert_eq!(r, TxResult::Success);
        assert!(!sb.exists(&keylet::account_root_key(&pseudo)) && !sb.exists(&broker));
        assert_eq!(holds(&sb, &OWNER, &USD), usd("1000"));

        // MPT, with cover.
        let mut st = state();
        let (broker, pseudo, id) = mpt_broker_with_cover(&mut st, 100);
        let asset = HeldAsset::Mpt(id);
        let (r, sb) = apply(&st, &broker_delete(OWNER, &broker));
        assert_eq!(r, TxResult::Success);
        assert_eq!(holds(&sb, &OWNER, &asset).mantissa, 1000);
        assert!(!sb.exists(&keylet::mptoken_key(&keylet::mpt_issuance_key(&id), &pseudo)), "the pseudo-account's MPToken");
        assert!(!sb.exists(&keylet::account_root_key(&pseudo)) && !sb.exists(&broker));
    }

    /// 3.5.0-rc1 LoanBrokerDelete.cpp:100-119, :172-200 (fixCleanup3_5_0): an
    /// owner without a holding of the asset gets one for the cover payout,
    /// out of the reserve the broker frees. Before the fix the payout goes
    /// straight to accountSend: tecNO_AUTH for an MPT.
    #[test]
    fn loan_broker_delete_under_fix_cleanup_3_5_0_creates_the_owners_holding_for_the_payout() {
        let build = |rules: &[&'static str]| {
            let mut st = state_with(rules);
            let (broker, _, id) = mpt_broker_with_cover(&mut st, 100);
            let ikey = keylet::mpt_issuance_key(&id);
            // The owner's MPToken emptied and gone.
            edit(&mut st, ikey, |i| i["OutstandingAmount"] = json!("100"));
            st.state_map.delete(&keylet::mptoken_key(&ikey, &OWNER)).unwrap();
            (st, broker, id)
        };
        let (st, broker, _) = build(&MAINNET_WITH_LENDING);
        let (r, sb) = apply(&st, &broker_delete(OWNER, &broker));
        assert_eq!(r, TxResult::NoAuth, "3.4.1: no MPToken to pay into");
        assert!(sb.exists(&broker), "a tec keeps the broker");

        let (st, broker, id) = build(&rules_with(&[am::FIX_CLEANUP_3_5_0]));
        let owners_before = owners(&Sandbox::new(&st), &OWNER);
        let (r, sb) = apply(&st, &broker_delete(OWNER, &broker));
        assert_eq!(r, TxResult::Success);
        assert_eq!(holds(&sb, &OWNER, &HeldAsset::Mpt(id)).mantissa, 100);
        assert_eq!(owners(&sb, &OWNER), owners_before - 2 + 1, "two units freed, one taken by the new MPToken");
        assert!(!sb.exists(&broker));
        // The reserve for the new holding is judged AFTER the broker's two
        // units are released: an owner exactly at its reserve still succeeds.
        let mut tight = st.clone();
        let reserve = crate::ledger::fees::account_reserve(&Sandbox::new(&st), owners_before - 1);
        edit(&mut tight, keylet::account_root_key(&OWNER), |a| a["Balance"] = json!(reserve.to_string()));
        assert_eq!(result(&tight, &broker_delete(OWNER, &broker)), TxResult::Success);
        // fixCleanup3_5_0's preclaim: an issuance that takes no new holders.
        let mut closed = st.clone();
        edit(&mut closed, keylet::mpt_issuance_key(&id), |i| {
            i["Flags"] = json!(i["Flags"].as_u64().unwrap() & !mpt::LSF_MPT_CAN_TRANSFER)
        });
        assert_eq!(result(&closed, &broker_delete(OWNER, &broker)), TxResult::NoAuth);
    }

    /// VaultDelete.cpp:160-162 with a LoanBroker in the vault
    /// pseudo-account's directory (LoanBrokerSet.cpp:252): an empty vault
    /// that still hosts a broker cannot be deleted; once the broker is gone
    /// it can.
    #[test]
    fn a_vault_cannot_be_deleted_while_a_broker_is_attached() {
        let mut st = state();
        let (broker, _, vault) = xrp_broker(&mut st);
        let vault_delete = tx("VaultDelete", OWNER, json!({"VaultID": key_hex(&vault)}));
        let (r, sb) = apply(&st, &vault_delete);
        assert_eq!(r, TxResult::HasObligations);
        assert!(sb.exists(&vault) && sb.exists(&broker));
        commit(&mut st, &broker_delete(OWNER, &broker));
        let (r, sb) = apply(&st, &vault_delete);
        assert_eq!(r, TxResult::Success);
        assert!(!sb.exists(&vault));
        assert_eq!(owners(&sb, &OWNER), 0, "everything the vault and the broker charged is released");
    }

    // -----------------------------------------------------------------------
    // The amendment gate, all five
    // -----------------------------------------------------------------------

    fn one_of_each() -> Vec<TxFields> {
        let vault = Hash256([0xAB; 32]);
        let broker = Hash256([0xCD; 32]);
        vec![
            broker_set(OWNER, &vault, json!({})),
            broker_delete(OWNER, &broker),
            cover_deposit(OWNER, &broker, json!("100")),
            cover_withdraw(OWNER, &broker, json!("100")),
            cover_clawback(OWNER, Some(&broker), None),
        ]
    }

    /// Transactor.h:644-647 with transactions.macro:887-960
    /// (featureLendingProtocol) and each type's `checkExtraFeatures`
    /// (LoanBroker_test.cpp:84 "Disabled"): a loan-broker type is temDISABLED
    /// in preflight — not applied, no fee, no sequence — unless
    /// LendingProtocol, SingleAssetVault and MPTokensV1 are all enabled.
    /// Mainnet's rules today have none of the first two.
    #[test]
    fn every_loan_broker_type_is_tem_disabled_without_the_amendments() {
        let mainnet_today = vec![am::FIX_CLEANUP_3_1_3, am::FIX_CLEANUP_3_2_0, am::FIX_CLEANUP_3_3_0, am::FIX_CLEANUP_3_4_0, am::MPTOKENS_V1, am::PERMISSIONED_DOMAINS, am::CREDENTIALS];
        for rules in [
            mainnet_today,
            rules_without(&[am::LENDING_PROTOCOL]),
            rules_without(&[am::SINGLE_ASSET_VAULT]),
            rules_without(&[am::MPTOKENS_V1]),
            vec![am::LENDING_PROTOCOL],
            vec![],
        ] {
            let st = state_with(&rules);
            for t in one_of_each() {
                let (result, sb) = apply(&st, &t);
                assert_eq!(result, TxResult::Disabled, "{}", t.tx_type);
                assert_eq!(sb.modification_count(), 0, "{}: nothing is written, the fee included", t.tx_type);
            }
        }
    }

    /// The five are registered, and each passes the gate once the
    /// amendments are on.
    #[test]
    fn every_loan_broker_type_is_dispatched_with_the_amendments() {
        let st = state();
        for t in one_of_each() {
            assert!(crate::tx::dispatch::is_supported(&t.tx_type), "{}", t.tx_type);
            assert_eq!(result(&st, &t), TxResult::NoEntry, "{}: past preflight, nothing there", t.tx_type);
        }
    }

    /// Transactor.cpp:234-250: none of the five is delegable — temINVALID;
    /// preflight0: none has a flag of its own — temINVALID_FLAG. Batch.h:60-76:
    /// none may be a Batch inner.
    #[test]
    fn a_delegated_flagged_or_batched_loan_broker_transaction_is_refused() {
        let st = state();
        for mut t in one_of_each() {
            t.fields["Delegate"] = json!(hex::encode(BOB));
            assert_eq!(result(&st, &t), TxResult::InvalidTx, "{} delegated", t.tx_type);
        }
        for mut t in one_of_each() {
            t.fields["Flags"] = json!(0x0001_0000u64);
            assert_eq!(result(&st, &t), TxResult::InvalidFlag, "{}", t.tx_type);
            // tfFullyCanonicalSig is universal.
            t.fields["Flags"] = json!(0x8000_0000u64);
            assert_ne!(result(&st, &t), TxResult::InvalidFlag, "{}", t.tx_type);
            assert!(crate::tx::batch::DISABLED_INNER_TYPES.contains(&t.tx_type.as_str()), "{}", t.tx_type);
        }
    }
}
