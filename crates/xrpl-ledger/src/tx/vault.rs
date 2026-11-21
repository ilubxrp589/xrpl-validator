//! The six vault transactors of the SingleAssetVault amendment, ported from
//! rippled 3.4.1 `src/libxrpl/tx/transactors/vault/`:
//!
//! | here | rippled | type |
//! |---|---|---|
//! | [`VaultCreateTransactor`] | VaultCreate.cpp | 65 |
//! | [`VaultSetTransactor`] | VaultSet.cpp | 66 |
//! | [`VaultDeleteTransactor`] | VaultDelete.cpp | 67 |
//! | [`VaultDepositTransactor`] | VaultDeposit.cpp | 68 |
//! | [`VaultWithdrawTransactor`] | VaultWithdraw.cpp | 69 |
//! | [`VaultClawbackTransactor`] | VaultClawback.cpp | 70 |
//!
//! Each is refused temDISABLED unless SingleAssetVault is enabled in the
//! ledger (`Transactor::invokePreflight`, Transactor.h:640-647): with the
//! amendment off — mainnet today — nothing here runs past that line.
//!
//! # What stands under them
//!
//! `tx::vault_helpers` (VaultHelpers.cpp: the assets ↔ shares conversions,
//! kinds, phases, the domain check), `tx::holding` (holdings, sends, freeze
//! and authorization checks), `tx::st_number` (STNumber fields and
//! `associateAsset`), `tx::protocol` (constants, flags, the Vault template),
//! `tx::misc::create_pseudo_account`.
//!
//! # Amendment arms
//!
//! Every `rules().enabled(...)` in the six files is carried, each gated on
//! the ledger's own Amendments object as rippled gates it:
//!
//! - fixCleanup3_1_3, 3_2_0, 3_3_0, 3_4_0 (all on at mainnet): both arms of
//!   every branch;
//! - LendingProtocolV1_1 (off at mainnet): sfVaultKind / sfSubscriptionDate /
//!   sfRedemptionDate, the closed-ended phases, sfLEVersion = CashBasis,
//!   VaultDelete's sfMemoData;
//! - fixCleanup3_5_0 (3.5.0-rc1, off at mainnet, on at devnet): the
//!   AssetsMaximum range checks of VaultCreate and VaultSet, VaultClawback's
//!   missing holder, VaultWithdraw's transfer fee between two holders.
//!
//! NOT ported: LendingProtocolV1_2 (`Supported::No` in 3.5.0-rc1, enabled on
//! no network): the FixedPrecision vault version and everything keyed on it
//! (sfAssetsDeployed, sfYieldUnrealized, the base grid, the open-zone limit).
//! Without it `getVaultDefaultVersion` is Legacy or CashBasis and 3.5.0's
//! refactored helpers (`adjustVaultCash`, `clampVaultOutflow`,
//! `checkDebitNotDust`, `roundToPosteriorVaultScale`, `getAssetsTotal`)
//! reduce to the 3.4.1 code ported here. featureSponsor (reserve sponsors) is
//! not ported either, as in `tx::holding`.
//!
//! # Rounding
//!
//! No vault transactor installs a rounding guard of its own around the
//! conversions: `Number` arithmetic and `STAmount = Number` assignments run
//! at the thread's default, ToNearest ([`AMBIENT`]). The helpers that fix a
//! mode say so themselves.
//!
//! # Exceptions
//!
//! Where rippled catches `std::overflow_error` the `Err` of a helper becomes
//! the code rippled returns there (tecPATH_DRY). An `Err` outside such a
//! `try` is an exception leaving the transactor: tefEXCEPTION
//! (applySteps.cpp:485-488), nothing applied.
//!
//! # Not the transactors' job
//!
//! The vault invariants (ValidVault, VaultInvariant.cpp) are not ported; see
//! the notes at [`VaultWithdrawTransactor`] and [`VaultClawbackTransactor`]
//! for the places where rippled's final result is the invariant's
//! (tecINVARIANT_FAILED) rather than the transactor's.

use crate::flow::st_amount::StError;
use crate::ledger::amendments::{self as am, enabled};
use crate::ledger::directory::{owner_dir_insert, owner_dir_remove};
use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{Transactor, TxFields, TxResult};
use crate::tx::holding::{self, ExpiryComparison};
use crate::tx::mpt::{self, AuthType, HeldAsset};
use crate::tx::number::{Number, Rounding};
use crate::tx::offer::{decode20, json_at, put_json};
use crate::tx::protocol::{
    invalid_flags, Soe, VaultKind, VaultPhase, VaultVersion, LSF_MPT_CAN_TRADE, LSF_VAULT_PRIVATE, MAX_DATA_PAYLOAD_LENGTH,
    MAX_MPTOKEN_METADATA_LENGTH, TF_UNIVERSAL_MASK, TF_VAULT_CREATE_MASK, TF_VAULT_PRIVATE, TF_VAULT_SHARE_NON_TRANSFERABLE, VAULT,
    VAULT_DEFAULT_IOU_SCALE, VAULT_MAXIMUM_IOU_SCALE, VAULT_STRATEGY_FIRST_COME_FIRST_SERVE,
};
use crate::tx::st_number::{
    asset_to_json, associate_asset, number_field, number_from_json, round_to_scale, scale, set_number_field, Amount,
};
use crate::tx::vault_helpers::{
    self as vh, vault_asset, vault_share, SuppressExpired, TruncateShares, WaiveUnrealizedLoss,
};
use serde_json::Value;
use xrpl_core::types::Hash256;

/// The thread's rounding mode in every vault transactor: rippled's default,
/// `Number::RoundingMode::ToNearest` (none of the six installs a guard that
/// outlives a helper call).
const AMBIENT: Rounding = Rounding::ToNearest;

// ---------------------------------------------------------------------------
// Shared plumbing
// ---------------------------------------------------------------------------

fn tx_flags(tx: &TxFields) -> u64 {
    tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0)
}

fn has(tx: &TxFields, field: &str) -> bool {
    tx.fields.get(field).is_some()
}

fn obj_flags(obj: &Value) -> u64 {
    obj.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0)
}

/// The head of `Transactor::invokePreflight` (Transactor.h:640-653) and
/// `preflight1` / `preflight0` (Transactor.cpp:231-250) for a vault type:
///
/// - the type's amendment (transactions.macro: featureSingleAssetVault for
///   all six) off: temDISABLED;
/// - `extra` — the type's `checkExtraFeatures` — false: temDISABLED;
/// - sfDelegate: none of the six is delegable (`Delegation::NotDelegable`,
///   the TxSettings default) or has a granular permission, so a delegated
///   one is temINVALID (Transactor.cpp:245-250);
/// - a flag outside the type's mask: temINVALID_FLAG.
fn gate(tx: &TxFields, rules: &Sandbox, extra: bool, mask: u64) -> Option<TxResult> {
    if !enabled(rules, am::SINGLE_ASSET_VAULT) || !extra {
        return Some(TxResult::Disabled);
    }
    if tx.delegate().is_some() {
        return Some(TxResult::InvalidTx);
    }
    if invalid_flags(tx_flags(tx), mask) {
        return Some(TxResult::InvalidFlag);
    }
    None
}

fn is_zero_key(key: &Hash256) -> bool {
    key.0 == [0u8; 32]
}

/// `tx[sfVaultID]`.
fn vault_id(tx: &TxFields) -> Option<Hash256> {
    mpt::key_field(&tx.fields, "VaultID")
}

/// A VL field's length in bytes (the field is hex in JSON); None when absent.
fn blob_len(tx: &TxFields, field: &str) -> Option<usize> {
    tx.fields.get(field).map(|v| v.as_str().map(|s| s.len() / 2).unwrap_or(0))
}

/// `Transactor::validDataLength(slice, max)` (Transactor.cpp:362-367): absent
/// is fine; present must be non-empty and at most `max` bytes.
fn valid_data_length(tx: &TxFields, field: &str, max: usize) -> bool {
    match blob_len(tx, field) {
        None => true,
        Some(len) => len != 0 && len <= max,
    }
}

/// An amount field of the transaction; a negative MPT value included (the
/// engine's amount reader takes MPT values as unsigned).
fn tx_amount(v: &Value) -> Option<Amount> {
    if let Some(amount) = Amount::from_json(v) {
        return Some(amount);
    }
    let id = hex::decode(v.get("mpt_issuance_id")?.as_str()?).ok()?;
    let id: [u8; 24] = id.as_slice().try_into().ok()?;
    let magnitude: u64 = v.get("value")?.as_str()?.strip_prefix('-')?.parse().ok()?;
    Some(Amount { asset: HeldAsset::Mpt(id), negative: magnitude != 0, mantissa: magnitude, exponent: 0 })
}

/// `tx.getSeqProxy().value()`: the Sequence, or the TicketSequence of a
/// ticketed transaction.
fn seq_proxy_value(tx: &TxFields) -> u32 {
    if tx.uses_ticket() {
        tx.ticket_seq.unwrap_or(0)
    } else {
        tx.sequence
    }
}

/// rippled's `preFeeBalance_`: the account's XRP before this transaction's
/// fee (the fee is already taken when do_apply runs).
fn prior_balance(sandbox: &Sandbox, tx: &TxFields) -> u64 {
    json_at(sandbox, &keylet::account_root_key(&tx.account))
        .and_then(|a| a["Balance"].as_str().and_then(|s| s.parse::<u64>().ok()))
        .unwrap_or(0)
        .saturating_add(tx.account_fee())
}

fn account_of(obj: &Value, field: &str) -> Option<[u8; 20]> {
    obj.get(field).and_then(|v| v.as_str()).and_then(decode20)
}

/// The vault a transaction names, with its key.
fn read_vault(sandbox: &Sandbox, tx: &TxFields) -> Option<(Hash256, Value)> {
    let key = vault_id(tx)?;
    let vault = json_at(sandbox, &key)?;
    (vault.get("LedgerEntryType").and_then(|t| t.as_str()) == Some("Vault")).then_some((key, vault))
}

/// The vault's share: its MPTID and the issuance, when the issuance exists.
fn read_share(sandbox: &Sandbox, vault: &Value) -> Option<([u8; 24], Hash256, Option<Value>)> {
    let HeldAsset::Mpt(id) = vault_share(vault)? else { return None };
    let key = keylet::mpt_issuance_key(&id);
    Some((id, key, json_at(sandbox, &key)))
}

/// `a < b` for two amounts of one asset (`STAmount::operator<`).
fn less(a: &Amount, b: &Amount) -> bool {
    a.number() < b.number()
}

fn positive(a: &Amount) -> bool {
    !a.is_zero() && !a.negative
}

/// `associateAsset(*vault, asset)` and the write: the vault's asset-valued
/// Number fields rounded to the asset's precision (STTakesAsset.cpp:13-50),
/// SoeDefault fields at their default dropped, the object stored. rippled
/// calls `associateAsset` last in each doApply, on the SLE it already
/// handed to the view; the stored object is the rounded one. An amount out
/// of the asset's range is rippled's throw there: tefEXCEPTION.
fn store_vault(sandbox: &mut Sandbox, key: Hash256, vault: &mut Value, asset: HeldAsset) -> TxResult {
    if associate_asset(vault, asset, AMBIENT).is_err() {
        return TxResult::TefException;
    }
    if VAULT.finish(vault).is_err() {
        return TxResult::TefInternal;
    }
    put_json(sandbox, key, vault);
    TxResult::Success
}

fn number_is_negative(n: &Number) -> bool {
    n.negative && !n.is_zero()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::state::LedgerState;
    use crate::tx::dispatch::apply_on_sandbox;
    use serde_json::json;

    /// The vault's owner.
    const OWNER: [u8; 20] = [0x11; 20];
    /// A depositor.
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
    /// The parent ledger's close time in every test state.
    const NOW: u32 = 1000;

    /// Mainnet's rules today plus what a vault needs: every fixCleanup up to
    /// 3.4.0, SingleAssetVault, LendingProtocol, MPTokensV1,
    /// PermissionedDomains, Credentials.
    const MAINNET_WITH_VAULTS: [&str; 9] = [
        am::FIX_CLEANUP_3_1_3, am::FIX_CLEANUP_3_2_0, am::FIX_CLEANUP_3_3_0, am::FIX_CLEANUP_3_4_0, am::SINGLE_ASSET_VAULT,
        am::LENDING_PROTOCOL, am::MPTOKENS_V1, am::PERMISSIONED_DOMAINS, am::CREDENTIALS,
    ];

    fn rules_with(extra: &[&'static str]) -> Vec<&'static str> {
        let mut all = MAINNET_WITH_VAULTS.to_vec();
        all.extend_from_slice(extra);
        all
    }

    fn rules_without(off: &[&str]) -> Vec<&'static str> {
        MAINNET_WITH_VAULTS.iter().copied().filter(|a| !off.contains(a)).collect()
    }

    fn put(state: &mut LedgerState, key: Hash256, v: &Value) {
        state.state_map.insert(key, serde_json::to_vec(v).unwrap()).unwrap();
    }

    fn root_in(state: &mut LedgerState, id: [u8; 20], flags: u64, extra: Value) {
        let mut a = json!({
            "LedgerEntryType": "AccountRoot", "Account": hex::encode(id), "Balance": BALANCE.to_string(),
            "Sequence": 5, "OwnerCount": 0, "Flags": flags,
        });
        for (k, v) in extra.as_object().unwrap() {
            a[k] = v.clone();
        }
        put(state, keylet::account_root_key(&id), &a);
    }

    fn state_with(amendments: &[&str]) -> LedgerState {
        let mut state = LedgerState::new_unverified(LedgerHeader {
            sequence: 100, total_coins: 100_000_000_000_000_000,
            parent_hash: Hash256([0x77; 32]), transaction_hash: Hash256([0; 32]), account_hash: Hash256([0; 32]),
            parent_close_time: 0, close_time: NOW, close_time_resolution: 10, close_flags: 0,
        });
        put(&mut state, keylet::amendments_key(), &json!({"LedgerEntryType": "Amendments", "Flags": 0, "Amendments": amendments}));
        root_in(&mut state, OWNER, 0, json!({}));
        root_in(&mut state, BOB, 0, json!({}));
        root_in(&mut state, CAROL, 0, json!({}));
        root_in(&mut state, ISSUER, LSF_DEFAULT_RIPPLE, json!({}));
        state
    }

    fn state() -> LedgerState {
        state_with(&MAINNET_WITH_VAULTS)
    }

    fn mptid(seq: u32, issuer: [u8; 20]) -> [u8; 24] {
        let mut id = [0u8; 24];
        id[..4].copy_from_slice(&seq.to_be_bytes());
        id[4..].copy_from_slice(&issuer);
        id
    }

    /// An MPT issued by ISSUER (sequence 7).
    fn issuance(state: &mut LedgerState, flags: u64, extra: Value) -> [u8; 24] {
        let id = mptid(7, ISSUER);
        let mut o = json!({
            "LedgerEntryType": "MPTokenIssuance", "Issuer": hex::encode(ISSUER), "Flags": flags, "Sequence": 7,
            "OutstandingAmount": "0", "OwnerNode": "0",
        });
        for (k, v) in extra.as_object().unwrap() {
            o[k] = v.clone();
        }
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

    fn xrp_asset() -> Value {
        json!({"currency": "XRP"})
    }

    fn usd_asset() -> Value {
        json!({"currency": hex::encode_upper(USD_CODE), "issuer": hex::encode(ISSUER)})
    }

    /// The key and pseudo-account a VaultCreate of OWNER at sequence 5 makes
    /// in `state`.
    fn expected_vault(state: &LedgerState) -> (Hash256, [u8; 20]) {
        let key = keylet::vault_key(&OWNER, 5);
        let pseudo = crate::tx::misc::pseudo_account_address(&Sandbox::new(state), &key).unwrap();
        (key, pseudo)
    }

    // -----------------------------------------------------------------------
    // The amendment gate
    // -----------------------------------------------------------------------

    fn one_of_each() -> Vec<TxFields> {
        let vault = "AB".repeat(32);
        let _ = &vault;
        vec![
            tx("VaultCreate", OWNER, json!({"Asset": xrp_asset()})),
        ]
    }

    /// Transactor.h:644-647: a type whose amendment is off is temDISABLED in
    /// preflight — not applied, no fee, no sequence (VaultValidation_test.cpp:93
    /// "disabled single asset vault"). Mainnet's rules today.
    #[test]
    fn every_vault_type_is_tem_disabled_without_the_amendment() {
        for rules in [rules_without(&[am::SINGLE_ASSET_VAULT]), vec![am::FIX_CLEANUP_3_2_0, am::FIX_CLEANUP_3_3_0, am::FIX_CLEANUP_3_4_0], vec![]] {
            let st = state_with(&rules);
            for t in one_of_each() {
                let (result, sb) = apply(&st, &t);
                assert_eq!(result, TxResult::Disabled, "{}", t.tx_type);
                assert_eq!(sb.modification_count(), 0, "{}: nothing is written, the fee included", t.tx_type);
            }
        }
        // LendingProtocol alone does not enable them.
        let st = state_with(&rules_without(&[am::SINGLE_ASSET_VAULT]));
        assert!(enabled(&Sandbox::new(&st), am::LENDING_PROTOCOL));
    }

    /// The six are registered, and each passes the gate once the amendment is on.
    #[test]
    fn every_vault_type_is_dispatched_with_the_amendment() {
        let st = state();
        for t in one_of_each() {
            assert!(crate::tx::dispatch::is_supported(&t.tx_type), "{}", t.tx_type);
            assert_ne!(result(&st, &t), TxResult::Disabled, "{}", t.tx_type);
            assert_ne!(result(&st, &t), TxResult::Unsupported, "{}", t.tx_type);
        }
    }

    /// Transactor.cpp:234-250: none of the six is delegable — temINVALID;
    /// preflight0: a flag outside the type's mask — temINVALID_FLAG
    /// (VaultValidation_test.cpp:162 "invalid flags"). VaultCreate alone has
    /// flags of its own.
    #[test]
    fn a_delegated_or_wrongly_flagged_vault_transaction_is_refused_in_preflight() {
        let st = state();
        for mut t in one_of_each() {
            t.fields["Delegate"] = json!(hex::encode(BOB));
            assert_eq!(result(&st, &t), TxResult::InvalidTx, "{} delegated", t.tx_type);
        }
        for mut t in one_of_each() {
            t.fields["Flags"] = json!(0x0000_0001u64);
            assert_eq!(result(&st, &t), TxResult::InvalidFlag, "{}", t.tx_type);
            // tfFullyCanonicalSig and tfInnerBatchTxn are universal.
            t.fields["Flags"] = json!(0x8000_0000u64);
            assert_ne!(result(&st, &t), TxResult::InvalidFlag, "{}", t.tx_type);
            let own = TF_VAULT_PRIVATE | TF_VAULT_SHARE_NON_TRANSFERABLE;
            t.fields["Flags"] = json!(own);
            let refused = result(&st, &t) == TxResult::InvalidFlag;
            assert_eq!(refused, t.tx_type != "VaultCreate", "{}", t.tx_type);
        }
    }

    // -----------------------------------------------------------------------
    // VaultCreate
    // -----------------------------------------------------------------------

    /// VaultCreate.cpp:38-53 (`checkExtraFeatures`): MPTokensV1; sfDomainID
    /// needs PermissionedDomains (VaultValidation_test.cpp:138); the
    /// closed-ended fields need LendingProtocolV1_1.
    #[test]
    fn vault_create_extra_features() {
        let create = |fields: Value| tx("VaultCreate", OWNER, fields);
        let st = state_with(&rules_without(&[am::MPTOKENS_V1]));
        assert_eq!(result(&st, &create(json!({"Asset": xrp_asset()}))), TxResult::Disabled);
        let st = state_with(&rules_without(&[am::PERMISSIONED_DOMAINS]));
        assert_eq!(result(&st, &create(json!({"Asset": xrp_asset()}))), TxResult::Success);
        let domain = json!({"Asset": xrp_asset(), "DomainID": "D0".repeat(32), "Flags": TF_VAULT_PRIVATE});
        assert_eq!(result(&st, &create(domain)), TxResult::Disabled);
        let st = state();
        for field in ["VaultKind", "SubscriptionDate", "RedemptionDate"] {
            let mut fields = json!({"Asset": xrp_asset()});
            fields[field] = json!(0);
            assert_eq!(result(&st, &create(fields)), TxResult::Disabled, "{field} without LendingProtocolV1_1");
        }
    }

    /// VaultCreate.cpp:61-125, every refusal temMALFORMED
    /// (VaultValidation_test.cpp:329 "create with Scale", :376 "create or set
    /// invalid data", :421 "create with invalid metadata", :442 "set negative
    /// maximum", :533 "invalid create").
    #[test]
    fn vault_create_preflight() {
        let mut st = state_with(&rules_with(&[am::LENDING_PROTOCOL_V1_1]));
        let id = issuance(&mut st, mpt::LSF_MPT_CAN_TRANSFER, json!({}));
        let malformed = |fields: Value, why: &str| {
            assert_eq!(result(&st, &tx("VaultCreate", OWNER, fields)), TxResult::Malformed, "{why}");
        };
        let ok = |fields: Value, why: &str| {
            let r = result(&st, &tx("VaultCreate", OWNER, fields));
            assert!(!matches!(r, TxResult::Malformed | TxResult::Disabled | TxResult::InvalidFlag), "{why}: {r:?}");
        };
        malformed(json!({"Asset": xrp_asset(), "Data": ""}), "empty Data");
        malformed(json!({"Asset": xrp_asset(), "Data": "00".repeat(257)}), "Data over 256 bytes");
        ok(json!({"Asset": xrp_asset(), "Data": "00".repeat(256)}), "256 bytes of Data");
        malformed(json!({"Asset": xrp_asset(), "WithdrawalPolicy": 0}), "an unknown withdrawal policy");
        malformed(json!({"Asset": xrp_asset(), "WithdrawalPolicy": 2}), "an unknown withdrawal policy");
        ok(json!({"Asset": xrp_asset(), "WithdrawalPolicy": 1}), "first come first served");
        malformed(json!({"Asset": xrp_asset(), "DomainID": "00".repeat(32), "Flags": TF_VAULT_PRIVATE}), "a zero DomainID");
        malformed(json!({"Asset": xrp_asset(), "DomainID": "D0".repeat(32)}), "a DomainID on a public vault");
        malformed(json!({"Asset": xrp_asset(), "AssetsMaximum": "-1"}), "a negative AssetsMaximum");
        ok(json!({"Asset": xrp_asset(), "AssetsMaximum": "0"}), "zero is no limit");
        malformed(json!({"Asset": xrp_asset(), "MPTokenMetadata": ""}), "empty metadata");
        malformed(json!({"Asset": xrp_asset(), "MPTokenMetadata": "00".repeat(1025)}), "metadata over 1024 bytes");
        ok(json!({"Asset": xrp_asset(), "MPTokenMetadata": "00".repeat(1024)}), "1024 bytes of metadata");
        malformed(json!({"Asset": xrp_asset(), "Scale": 0}), "Scale on an XRP vault");
        malformed(json!({"Asset": {"mpt_issuance_id": hex::encode_upper(id)}, "Scale": 1}), "Scale on an MPT vault");
        malformed(json!({"Asset": usd_asset(), "Scale": 19}), "Scale above 18");
        ok(json!({"Asset": usd_asset(), "Scale": 18}), "Scale 18");
        ok(json!({"Asset": usd_asset(), "Scale": 0}), "Scale 0 on an IOU vault");
        malformed(json!({"Asset": xrp_asset(), "VaultKind": 2}), "an unknown VaultKind");
        malformed(json!({"Asset": xrp_asset(), "SubscriptionDate": NOW + 10}), "a date on an open-ended vault");
        malformed(json!({"Asset": xrp_asset(), "VaultKind": 0, "RedemptionDate": NOW + 10}), "a date on an open-ended vault");
        malformed(json!({"Asset": xrp_asset(), "VaultKind": 1, "SubscriptionDate": NOW + 10}), "closed-ended without RedemptionDate");
        malformed(json!({"Asset": xrp_asset(), "VaultKind": 1, "RedemptionDate": NOW + 500}), "closed-ended without SubscriptionDate");
        malformed(
            json!({"Asset": xrp_asset(), "VaultKind": 1, "SubscriptionDate": NOW + 10, "RedemptionDate": NOW + 189}),
            "an investment period under 180 s",
        );
        ok(json!({"Asset": xrp_asset(), "VaultKind": 1, "SubscriptionDate": NOW + 10, "RedemptionDate": NOW + 190}), "180 s exactly");
    }

    /// 3.5.0-rc1 VaultCreate.cpp:93-96: with fixCleanup3_5_0 an AssetsMaximum
    /// the asset cannot hold is temMALFORMED; without it the transaction
    /// reaches doApply and rippled throws in associateAsset — tefEXCEPTION,
    /// nothing applied (diff-341-vs-350rc.md A1).
    #[test]
    fn vault_create_assets_maximum_out_of_range() {
        let beyond_xrp = json!({"Asset": xrp_asset(), "AssetsMaximum": "1e18"});
        let st = state_with(&rules_with(&[am::FIX_CLEANUP_3_5_0]));
        assert_eq!(result(&st, &tx("VaultCreate", OWNER, beyond_xrp.clone())), TxResult::Malformed);
        assert_eq!(result(&st, &tx("VaultCreate", OWNER, json!({"Asset": xrp_asset(), "AssetsMaximum": "1e17"}))), TxResult::Success);
        let st = state();
        let (r, sb) = apply(&st, &tx("VaultCreate", OWNER, beyond_xrp));
        assert_eq!(r, TxResult::TefException);
        assert_eq!(sb.modification_count(), 0);
        // An IOU's range is far wider: 1e18 USD is fine either way.
        assert_eq!(result(&st, &tx("VaultCreate", OWNER, json!({"Asset": usd_asset(), "AssetsMaximum": "1e18"}))), TxResult::Success);
    }

    /// VaultCreate.cpp:127-172 (VaultValidation_test.cpp:705 "non-existing
    /// domain", :758 "IOU fail create frozen", :776 "IOU fail create no
    /// ripling", :793 "IOU no issuer", :811 "IOU fail create vault for AMM
    /// LPToken", :896 "MPT no authorization").
    #[test]
    fn vault_create_preclaim() {
        let create = |asset: Value| tx("VaultCreate", OWNER, json!({"Asset": asset}));
        // canAddHolding.
        let mut st = state();
        root_in(&mut st, ISSUER, 0, json!({}));
        assert_eq!(result(&st, &create(usd_asset())), TxResult::NoRipple, "the issuer has no DefaultRipple");
        let gone = json!({"currency": hex::encode_upper(USD_CODE), "issuer": hex::encode([0x99u8; 20])});
        assert_eq!(result(&st, &create(gone)), TxResult::NoAccount, "no issuer");
        let missing = json!({"mpt_issuance_id": hex::encode_upper(mptid(99, ISSUER))});
        assert_eq!(result(&st, &create(missing)), TxResult::ObjectNotFound);
        let fixed = issuance(&mut st, 0, json!({}));
        assert_eq!(result(&st, &create(json!({"mpt_issuance_id": hex::encode_upper(fixed)}))), TxResult::NoAuth, "not transferable");

        // An asset issued by a pseudo-account.
        let mut st = state();
        root_in(&mut st, ISSUER, LSF_DEFAULT_RIPPLE, json!({"AMMID": "A0".repeat(32)}));
        assert_eq!(result(&st, &create(usd_asset())), TxResult::WrongAsset);
        let share = issuance(&mut st, mpt::LSF_MPT_CAN_TRANSFER, json!({}));
        assert_eq!(result(&st, &create(json!({"mpt_issuance_id": hex::encode_upper(share)}))), TxResult::WrongAsset);

        // Frozen for the owner.
        let mut st = state();
        root_in(&mut st, ISSUER, LSF_DEFAULT_RIPPLE | LSF_GLOBAL_FREEZE, json!({}));
        assert_eq!(result(&st, &create(usd_asset())), TxResult::Frozen);
        let mut st = state();
        line(&mut st, OWNER, LSF_HIGH_FREEZE, "10", "100");
        assert_eq!(result(&st, &create(usd_asset())), TxResult::Frozen);
        let mut st = state();
        let locked = issuance(&mut st, mpt::LSF_MPT_CAN_TRANSFER | mpt::LSF_MPT_LOCKED, json!({}));
        assert_eq!(result(&st, &create(json!({"mpt_issuance_id": hex::encode_upper(locked)}))), TxResult::Locked);

        // The domain must exist.
        let st = state();
        let private = tx("VaultCreate", OWNER, json!({"Asset": xrp_asset(), "DomainID": "D0".repeat(32), "Flags": TF_VAULT_PRIVATE}));
        assert_eq!(result(&st, &private), TxResult::ObjectNotFound);

        // A claimed tec keeps the fee and the sequence, nothing else.
        let (r, sb) = apply(&st, &private);
        assert_eq!(r, TxResult::ObjectNotFound);
        assert_eq!(xrp_of(&sb, &OWNER), BALANCE - FEE);
        assert_eq!(root_of(&sb, &OWNER)["Sequence"], json!(6));
        assert_eq!(sb.modification_count(), 1);

        // A closed-ended vault whose subscription has ended.
        let st = state_with(&rules_with(&[am::LENDING_PROTOCOL_V1_1]));
        let closed = |subscription: u32| {
            tx("VaultCreate", OWNER, json!({
                "Asset": xrp_asset(), "VaultKind": 1, "SubscriptionDate": subscription, "RedemptionDate": subscription + 1000,
            }))
        };
        assert_eq!(result(&st, &closed(NOW)), TxResult::Expired, "the parent close time has reached it");
        assert_eq!(result(&st, &closed(NOW - 1)), TxResult::Expired);
        assert_eq!(result(&st, &closed(NOW + 1)), TxResult::Success);
    }

    /// VaultCreate.cpp:156-159: no free pseudo-account address is
    /// terADDRESS_COLLISION — a retry, nothing applied.
    #[test]
    fn vault_create_address_collision() {
        let mut st = state();
        let key = keylet::vault_key(&OWNER, 5);
        // Take every candidate, one after the other.
        for _ in 0..256 {
            let candidate = crate::tx::misc::pseudo_account_address(&Sandbox::new(&st), &key).unwrap();
            root_in(&mut st, candidate, 0, json!({}));
        }
        let (r, sb) = apply(&st, &tx("VaultCreate", OWNER, json!({"Asset": xrp_asset()})));
        assert_eq!(r, TxResult::AddressCollision);
        assert_eq!(sb.modification_count(), 0);
    }

    /// VaultCreate.cpp:174-314, an XRP vault: the Vault (in the owner's
    /// directory, no Scale, WithdrawalPolicy 1, nothing asset-valued), its
    /// pseudo-account (Sequence 0, the three flags, sfVaultID), the share
    /// issuance (sequence 1, transferable, no ReferenceHolding) and the
    /// owner's MPToken. The owner owns three objects more, the
    /// pseudo-account one.
    #[test]
    fn vault_create_xrp() {
        let st = state();
        let (key, pseudo) = expected_vault(&st);
        let create = tx("VaultCreate", OWNER, json!({"Asset": xrp_asset(), "Data": "4d657461", "AssetsMaximum": "1000000"}));
        let (r, sb) = apply(&st, &create);
        assert_eq!(r, TxResult::Success);
        let share = mptid(1, pseudo);
        let vault = get(&sb, &key).unwrap();
        assert_eq!(
            vault,
            json!({
                "LedgerEntryType": "Vault", "Flags": 0, "Sequence": 5, "Owner": hex::encode(OWNER), "Account": hex::encode(pseudo),
                "Asset": {"currency": "XRP"}, "ShareMPTID": hex::encode_upper(share), "OwnerNode": "0", "WithdrawalPolicy": 1,
                "AssetsMaximum": "1000000", "Data": "4D657461",
            })
        );
        assert!(dir_has(&sb, &OWNER, &key));
        let root = root_of(&sb, &pseudo);
        assert_eq!(root["Sequence"], json!(0));
        assert_eq!(root["Balance"], json!("0"));
        assert_eq!(root["Flags"], json!(0x0190_0000u64), "DisableMaster | DefaultRipple | DepositAuth");
        assert_eq!(root["VaultID"], json!(hex::encode_upper(key.0)));
        assert_eq!(root["OwnerCount"], json!(1), "the share issuance");
        let issuance_key = keylet::mpt_issuance_key(&share);
        let issuance = get(&sb, &issuance_key).unwrap();
        assert_eq!(
            issuance,
            json!({
                "LedgerEntryType": "MPTokenIssuance", "Flags": 0x38, "Issuer": hex::encode(pseudo), "OutstandingAmount": "0",
                "OwnerNode": "0", "Sequence": 1,
            }),
            "CanEscrow | CanTrade | CanTransfer; no AssetScale, no ReferenceHolding for XRP"
        );
        assert!(dir_has(&sb, &pseudo, &issuance_key));
        let token_key = keylet::mptoken_key(&issuance_key, &OWNER);
        let token = get(&sb, &token_key).unwrap();
        assert_eq!(token["Flags"], json!(0), "a public vault: not lsfMPTAuthorized");
        assert!(token.get("MPTAmount").is_none());
        assert!(dir_has(&sb, &OWNER, &token_key));
        assert_eq!(owners(&sb, &OWNER), 3, "Vault, pseudo-account, MPToken");
        assert_eq!(xrp_of(&sb, &OWNER), BALANCE - FEE);
    }

    /// An IOU vault: the pseudo-account's trust line (its side reserved and
    /// NoRipple, limit 0), sfScale 6 by default on the vault and as the
    /// share's AssetScale, sfReferenceHolding naming the line
    /// (fixCleanup3_2_0); AssetsMaximum rounded to the asset by
    /// associateAsset.
    #[test]
    fn vault_create_iou() {
        let st = state();
        let (key, pseudo) = expected_vault(&st);
        let create = tx("VaultCreate", OWNER, json!({"Asset": usd_asset(), "AssetsMaximum": "1234567890.123456789"}));
        let (r, sb) = apply(&st, &create);
        assert_eq!(r, TxResult::Success);
        let vault = get(&sb, &key).unwrap();
        assert_eq!(vault["Scale"], json!(6));
        assert_eq!(vault["AssetsMaximum"], json!("1234567890.123457"), "sixteen digits, to nearest");
        assert_eq!(vault["Asset"], json!({"currency": hex::encode_upper(USD_CODE), "issuer": hex::encode(ISSUER)}));
        let line_key = keylet::ripple_state_key(&pseudo, &ISSUER, &USD_CODE);
        let line = get(&sb, &line_key).unwrap();
        assert_eq!(crate::tx::offer::signed_value(&line["Balance"]).1 .0, 0);
        assert!(dir_has(&sb, &pseudo, &line_key) && dir_has(&sb, &ISSUER, &line_key));
        let issuance = get(&sb, &keylet::mpt_issuance_key(&mptid(1, pseudo))).unwrap();
        assert_eq!(issuance["AssetScale"], json!(6));
        assert_eq!(issuance["ReferenceHolding"], json!(hex::encode_upper(line_key.0)));
        assert_eq!(owners(&sb, &pseudo), 2, "the line and the share issuance");
        assert_eq!(owners(&sb, &OWNER), 3);
        assert_eq!(owners(&sb, &ISSUER), 0);

        // An explicit Scale; zero is not stored on the vault (SoeDefault).
        for (scale, stored) in [(0u64, None), (18, Some(18u64))] {
            let (r, sb) = apply(&st, &tx("VaultCreate", OWNER, json!({"Asset": usd_asset(), "Scale": scale})));
            assert_eq!(r, TxResult::Success);
            let vault = get(&sb, &key).unwrap();
            assert_eq!(vault.get("Scale").and_then(|s| s.as_u64()), stored);
            let issuance = get(&sb, &keylet::mpt_issuance_key(&mptid(1, pseudo))).unwrap();
            assert_eq!(issuance.get("AssetScale").and_then(|s| s.as_u64()), stored);
        }

        // Before fixCleanup3_2_0 the share names no holding.
        let st = state_with(&rules_without(&[am::FIX_CLEANUP_3_2_0]));
        let (r, sb) = apply(&st, &tx("VaultCreate", OWNER, json!({"Asset": usd_asset()})));
        assert_eq!(r, TxResult::Success);
        let (_, pseudo) = expected_vault(&st);
        assert!(get(&sb, &keylet::mpt_issuance_key(&mptid(1, pseudo))).unwrap().get("ReferenceHolding").is_none());
    }

    /// An MPT vault: the pseudo-account's MPToken of the asset, named by the
    /// share's sfReferenceHolding; no Scale. Private and non-transferable
    /// flags: lsfVaultPrivate on the vault, lsfMPTRequireAuth alone on the
    /// share, sfDomainID on the share, the owner's MPToken authorized.
    #[test]
    fn vault_create_mpt_private_non_transferable() {
        let mut st = state();
        let id = issuance(&mut st, mpt::LSF_MPT_CAN_TRANSFER, json!({}));
        let domain = Hash256([0xD0; 32]);
        put(&mut st, domain, &json!({"LedgerEntryType": "PermissionedDomain", "Owner": hex::encode(ISSUER), "Flags": 0, "AcceptedCredentials": []}));
        let (key, pseudo) = expected_vault(&st);
        let create = tx("VaultCreate", OWNER, json!({
            "Asset": {"mpt_issuance_id": hex::encode_upper(id)}, "DomainID": hex::encode_upper(domain.0),
            "Flags": TF_VAULT_PRIVATE | TF_VAULT_SHARE_NON_TRANSFERABLE, "MPTokenMetadata": "cafe",
        }));
        let (r, sb) = apply(&st, &create);
        assert_eq!(r, TxResult::Success);
        let vault = get(&sb, &key).unwrap();
        assert_eq!(vault["Flags"], json!(LSF_VAULT_PRIVATE), "tfVaultShareNonTransferable is not kept on the vault");
        assert!(vault.get("Scale").is_none());
        let holding_key = keylet::mptoken_key(&keylet::mpt_issuance_key(&id), &pseudo);
        assert!(get(&sb, &holding_key).is_some());
        let share = mptid(1, pseudo);
        let share_issuance = get(&sb, &keylet::mpt_issuance_key(&share)).unwrap();
        assert_eq!(share_issuance["Flags"], json!(mpt::LSF_MPT_REQUIRE_AUTH));
        assert_eq!(share_issuance["DomainID"], json!(hex::encode_upper(domain.0)));
        assert_eq!(share_issuance["MPTokenMetadata"], json!("CAFE"));
        assert_eq!(share_issuance["ReferenceHolding"], json!(hex::encode_upper(holding_key.0)));
        assert!(share_issuance.get("AssetScale").is_none());
        let token = get(&sb, &keylet::mptoken_key(&keylet::mpt_issuance_key(&share), &OWNER)).unwrap();
        assert_eq!(token["Flags"], json!(mpt::LSF_MPT_AUTHORIZED));
        assert_eq!(owners(&sb, &pseudo), 2);
        assert_eq!(owners(&sb, &OWNER), 3);
        // The underlying issuance itself is untouched.
        assert!(!sb.modifications().contains_key(&keylet::mpt_issuance_key(&id)));

        // A private vault without a domain is legal (the owner alone may deposit).
        let (r, sb) = apply(&st, &tx("VaultCreate", OWNER, json!({"Asset": xrp_asset(), "Flags": TF_VAULT_PRIVATE})));
        assert_eq!(r, TxResult::Success);
        let share_issuance = get(&sb, &keylet::mpt_issuance_key(&share)).unwrap();
        assert_eq!(share_issuance["Flags"], json!(0x3Cu64));
        assert!(share_issuance.get("DomainID").is_none());
    }

    /// VaultCreate.cpp:192-195 (VaultValidation_test.cpp:687 "insufficient
    /// reserve"): the reserve for the Vault and its pseudo-account is judged
    /// against the balance before the fee; the owner's MPToken is its third
    /// object and (MPTokenHelpers.cpp:259-266) is reserve-tested only from an
    /// OwnerCount of two on — which the first two have just made it.
    #[test]
    fn vault_create_reserve() {
        // Base 1 XRP, increment 0.2 XRP: two objects need 1.4 XRP, three 1.6.
        let with_balance = |balance: u64| {
            let mut st = state();
            root_in(&mut st, OWNER, 0, json!({"Balance": balance.to_string()}));
            apply_result(&st)
        };
        fn apply_result(st: &LedgerState) -> TxResult {
            result(st, &tx("VaultCreate", OWNER, json!({"Asset": {"currency": "XRP"}})))
        }
        assert_eq!(with_balance(1_399_999), TxResult::InsufficientReserve, "below the reserve for two");
        assert_eq!(with_balance(1_400_000), TxResult::InsufficientReserve, "two are covered, the MPToken is not");
        assert_eq!(with_balance(1_599_999), TxResult::InsufficientReserve);
        assert_eq!(with_balance(1_600_000), TxResult::Success, "the fee comes out of the reserve");
    }

    /// VaultCreate.cpp:276-287 under LendingProtocolV1_1: sfLEVersion 1
    /// (CashBasis) on every new vault, sfVaultKind and the two dates on a
    /// closed-ended one; without the amendment none of them
    /// (VaultValidation_test.cpp:1005, :1022).
    #[test]
    fn vault_create_under_lending_protocol_v1_1() {
        let st = state_with(&rules_with(&[am::LENDING_PROTOCOL_V1_1]));
        let (key, _) = expected_vault(&st);
        let (r, sb) = apply(&st, &tx("VaultCreate", OWNER, json!({"Asset": xrp_asset()})));
        assert_eq!(r, TxResult::Success);
        let vault = get(&sb, &key).unwrap();
        assert_eq!(vault["LEVersion"], json!(1));
        assert!(vault.get("VaultKind").is_none(), "OpenEnded is the default and is not stored");
        let (r, sb) = apply(&st, &tx("VaultCreate", OWNER, json!({"Asset": xrp_asset(), "VaultKind": 0})));
        assert_eq!(r, TxResult::Success);
        assert!(get(&sb, &key).unwrap().get("VaultKind").is_none());
        let closed = tx("VaultCreate", OWNER, json!({
            "Asset": xrp_asset(), "VaultKind": 1, "SubscriptionDate": NOW + 100, "RedemptionDate": NOW + 5000,
        }));
        let (r, sb) = apply(&st, &closed);
        assert_eq!(r, TxResult::Success);
        let vault = get(&sb, &key).unwrap();
        assert_eq!((&vault["LEVersion"], &vault["VaultKind"]), (&json!(1), &json!(1)));
        assert_eq!((&vault["SubscriptionDate"], &vault["RedemptionDate"]), (&json!(NOW + 100), &json!(NOW + 5000)));

        let st = state();
        let (r, sb) = apply(&st, &tx("VaultCreate", OWNER, json!({"Asset": xrp_asset()})));
        assert_eq!(r, TxResult::Success);
        assert!(get(&sb, &key).unwrap().get("LEVersion").is_none());
    }

    /// A ticketed VaultCreate keys the vault by its TicketSequence
    /// (`getSeqProxy().value()`, VaultCreate.cpp:183, :188).
    #[test]
    fn vault_create_with_a_ticket_is_keyed_by_the_ticket() {
        let mut st = state();
        let ticket = keylet::ticket_key(&OWNER, 3);
        put(&mut st, ticket, &json!({"LedgerEntryType": "Ticket", "Account": hex::encode(OWNER), "Flags": 0, "OwnerNode": "0", "TicketSequence": 3}));
        root_in(&mut st, OWNER, 0, json!({"OwnerCount": 1, "TicketCount": 1}));
        let mut create = tx("VaultCreate", OWNER, json!({"Asset": xrp_asset(), "TicketSequence": 3}));
        create.fields["Sequence"] = json!(0);
        let create = TxFields::from_json(&create.fields).unwrap();
        let (r, sb) = apply(&st, &create);
        assert_eq!(r, TxResult::Success);
        let vault = get(&sb, &keylet::vault_key(&OWNER, 3)).unwrap();
        assert_eq!(vault["Sequence"], json!(3));
        assert_eq!(owners(&sb, &OWNER), 3, "the ticket went, three came");
    }
}
