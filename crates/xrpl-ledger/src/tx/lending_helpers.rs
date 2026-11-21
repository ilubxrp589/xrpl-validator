//! The parts of rippled 3.4.1 `LendingHelpers` (include/xrpl/ledger/helpers/
//! LendingHelpers.h, src/libxrpl/ledger/helpers/LendingHelpers.cpp) that the
//! five loan-broker transactors (`tx::loan_broker`) call:
//!
//! | here | rippled |
//! |---|---|
//! | [`check_lending_protocol_dependencies`] | `checkLendingProtocolDependencies`, LendingHelpers.cpp:69-82 |
//! | [`gate`] | `Transactor::invokePreflight` + `preflight1` for a lending type |
//! | [`can_apply_to_broker_cover`] | `canApplyToBrokerCover`, LendingHelpers.cpp:38-67 |
//! | [`is_zero_at_scale`] | `STAmount::isZeroAtScale`, STAmount.cpp:1737-1741 |
//! | [`assets_total_scale`] | `getAssetsTotalScale`, LendingHelpers.h:257-263 |
//! | [`minimum_broker_cover`] | `minimumBrokerCover`, LendingHelpers.h:268-278 |
//!
//! `tenthBipsOfValue` is `tx::protocol::tenth_bips_of_value`; `roundToAsset`,
//! `roundToScale` and `scale` are `tx::st_number`'s.
//!
//! NOT here: the loan maths (LendingHelpers.cpp:150-2315 — periodic rates,
//! payment schedules, `adjustImpreciseNumber`, `checkLoanGuards`, the
//! LoanManage freeze exemption at :84-119). They belong to the four Loan
//! transactors.
//!
//! 3.5.0-rc1: `minimumBrokerCover` moves to LendingHelpers.cpp:212-222 and
//! takes its scale from `getVaultBaseScale`, LoanBrokerDelete from
//! `getVaultScale` (VaultHelpers.cpp:342-378). For a Legacy or CashBasis
//! vault both are `scale(sfAssetsTotal, sfAsset)` — [`assets_total_scale`].
//! The FixedPrecision vault version (LendingProtocolV1_2, `Supported::No`)
//! and the helpers keyed on it (`creditToPosteriorBrokerCoverScale`,
//! `debitToPosteriorBrokerCoverScale`, `checkOptionalBrokerCoverInflow`) are
//! not ported; without that version each of them is the 3.4.1 code or a
//! no-op.

use crate::flow::st_amount::StError;
use crate::ledger::amendments::{self as am, enabled};
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{TxFields, TxResult};
use crate::tx::mpt::HeldAsset;
use crate::tx::number::{Number, Rounding};
use crate::tx::protocol::{invalid_flags, tenth_bips_of_value};
use crate::tx::st_number::{number_field, round_to_asset, round_to_scale, scale, Amount};
use crate::tx::vault_helpers::vault_asset;
use serde_json::Value;

/// `checkLendingProtocolDependencies(rules, tx)` (LendingHelpers.cpp:69-82):
/// every lending transaction needs SingleAssetVault and MPTokensV1, and
/// PermissionedDomains when it carries sfDomainID.
pub fn check_lending_protocol_dependencies(rules: &Sandbox, tx: &TxFields) -> bool {
    if !enabled(rules, am::SINGLE_ASSET_VAULT) {
        return false;
    }
    if !enabled(rules, am::MPTOKENS_V1) {
        return false;
    }
    if tx.fields.get("DomainID").is_some() && !enabled(rules, am::PERMISSIONED_DOMAINS) {
        return false;
    }
    true
}

/// The head of `Transactor::invokePreflight` (Transactor.h:640-653) and
/// `preflight1` / `preflight0` (Transactor.cpp:231-250) for a lending type:
///
/// - the type's amendment (transactions.macro:887-960: featureLendingProtocol
///   for all nine) off: temDISABLED;
/// - `extra` — the type's `checkExtraFeatures` — false: temDISABLED;
/// - sfDelegate: no lending type is delegable (`Delegation::NotDelegable`,
///   the TxSettings default) or has a granular permission, so a delegated
///   one is temINVALID (Transactor.cpp:245-250);
/// - a flag outside the type's mask: temINVALID_FLAG.
pub fn gate(tx: &TxFields, rules: &Sandbox, extra: bool, mask: u64) -> Option<TxResult> {
    if !enabled(rules, am::LENDING_PROTOCOL) || !extra {
        return Some(TxResult::Disabled);
    }
    if tx.delegate().is_some() {
        return Some(TxResult::InvalidTx);
    }
    let flags = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
    if invalid_flags(flags, mask) {
        return Some(TxResult::InvalidFlag);
    }
    None
}

/// `STAmount::isZeroAtScale(scale)` (STAmount.cpp:1737-1741):
/// `roundToScale(*this, scale, ToNearest).signum() == 0`. Never for a
/// non-zero XRP or MPT amount (`roundToScale` leaves an integral amount as
/// it is).
pub fn is_zero_at_scale(amount: &Amount, at_scale: i32) -> Result<bool, StError> {
    Ok(round_to_scale(*amount, at_scale, Rounding::ToNearest)?.is_zero())
}

/// `canApplyToBrokerCover(view, sleBroker, vaultAsset, amount)`
/// (LendingHelpers.cpp:38-67): whether adding `amount` to, or taking it
/// from, the broker's sfCoverAvailable changes it at all.
///
/// - fixCleanup3_2_0 off: always tesSUCCESS;
/// - a zero amount: tecPRECISION_LOSS;
/// - an amount that rounds to zero at the scale of sfCoverAvailable
///   (`scale(CoverAvailable, asset)`, the thread's ToNearest): tecPRECISION_LOSS.
///
/// The `Err` is rippled's STAmount throw (a stored cover out of the asset's
/// range): tefEXCEPTION in the callers' preclaim.
pub fn can_apply_to_broker_cover(sandbox: &Sandbox, broker: &Value, asset: HeldAsset, amount: &Amount) -> Result<TxResult, StError> {
    if !am::fix_cleanup_3_2_0(sandbox) {
        return Ok(TxResult::Success);
    }
    if amount.is_zero() {
        return Ok(TxResult::PrecisionLoss);
    }
    let cover_scale = scale(number_field(broker, "CoverAvailable"), asset, Rounding::ToNearest)?;
    if is_zero_at_scale(amount, cover_scale)? {
        return Ok(TxResult::PrecisionLoss);
    }
    Ok(TxResult::Success)
}

/// `getAssetsTotalScale(vaultSle)` (LendingHelpers.h:257-263):
/// `scale(sfAssetsTotal, sfAsset)` — 0 for an XRP or MPT vault, the
/// sixteen-digit exponent of the total for an IOU vault (-100 while it is
/// empty) — converted under `mode`, the thread's rounding mode at the call.
pub fn assets_total_scale(vault: &Value, mode: Rounding) -> Result<i32, StError> {
    let asset = vault_asset(vault).ok_or(StError::Overflow)?;
    scale(number_field(vault, "AssetsTotal"), asset, mode)
}

/// `minimumBrokerCover(debtTotal, coverRateMinimum, vaultSle)`
/// (LendingHelpers.h:268-278): the cover a broker must keep,
/// `DebtTotal × CoverRateMinimum / 100000` rounded UP to the vault's scale.
/// The whole expression — the product, the quotient, the vault scale and
/// `roundToAsset` (whose rounding argument defaults to the thread's mode) —
/// runs under one `NumberRoundModeGuard(Upward)`.
pub fn minimum_broker_cover(debt_total: Number, cover_rate_minimum: u32, vault: &Value) -> Result<Number, StError> {
    let up = Rounding::Upward;
    let asset = vault_asset(vault).ok_or(StError::Overflow)?;
    let raw = tenth_bips_of_value(debt_total, cover_rate_minimum, up).map_err(|_| StError::Overflow)?;
    round_to_asset(asset, raw, assets_total_scale(vault, up)?, up)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::keylet;
    use crate::ledger::state::LedgerState;
    use crate::tx::st_number::number_from_str;
    use serde_json::json;
    use xrpl_core::types::Hash256;

    const ISSUER: [u8; 20] = [0x21; 20];
    const USD_CODE: [u8; 20] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x55, 0x53, 0x44, 0, 0, 0, 0, 0];
    const USD: HeldAsset = HeldAsset::Iou { currency: USD_CODE, issuer: ISSUER };

    fn state_with(amendments: &[&str]) -> LedgerState {
        let mut state = LedgerState::new_unverified(LedgerHeader {
            sequence: 100, total_coins: 100_000_000_000_000_000,
            parent_hash: Hash256([0x77; 32]), transaction_hash: Hash256([0; 32]), account_hash: Hash256([0; 32]),
            parent_close_time: 0, close_time: 1000, close_time_resolution: 10, close_flags: 0,
        });
        let amendments = json!({"LedgerEntryType": "Amendments", "Flags": 0, "Amendments": amendments});
        state.state_map.insert(keylet::amendments_key(), serde_json::to_vec(&amendments).unwrap()).unwrap();
        state
    }

    const ALL: [&str; 5] =
        [am::FIX_CLEANUP_3_2_0, am::SINGLE_ASSET_VAULT, am::LENDING_PROTOCOL, am::MPTOKENS_V1, am::PERMISSIONED_DOMAINS];

    fn without(off: &[&str]) -> Vec<&'static str> {
        ALL.iter().copied().filter(|a| !off.contains(a)).collect()
    }

    fn tx(fields: Value) -> TxFields {
        let mut all = json!({
            "TransactionType": "LoanBrokerDelete", "Account": hex::encode([0x11u8; 20]), "Fee": "10", "Sequence": 5,
            "hash": "AB".repeat(32),
        });
        for (k, v) in fields.as_object().unwrap() {
            all[k] = v.clone();
        }
        TxFields::from_json(&all).unwrap()
    }

    fn usd(value: &str) -> Amount {
        Amount::from_json(&json!({"currency": hex::encode_upper(USD_CODE), "issuer": hex::encode(ISSUER), "value": value})).unwrap()
    }

    fn usd_vault(assets_total: &str) -> Value {
        json!({
            "LedgerEntryType": "Vault",
            "Asset": {"currency": hex::encode_upper(USD_CODE), "issuer": hex::encode(ISSUER)},
            "AssetsTotal": assets_total,
        })
    }

    fn n(s: &str) -> Number {
        number_from_str(s).unwrap()
    }

    /// LendingHelpers.cpp:69-82 (LoanBroker_test.cpp:84-176 "Disabled": the
    /// lending types are temDISABLED without SingleAssetVault or MPTokensV1
    /// even where LendingProtocol itself is on).
    #[test]
    fn the_lending_types_depend_on_vaults_mpts_and_for_a_domain_on_permissioned_domains() {
        let plain = tx(json!({}));
        let domain = tx(json!({"DomainID": "D0".repeat(32)}));
        let st = state_with(&ALL);
        assert!(check_lending_protocol_dependencies(&Sandbox::new(&st), &plain));
        assert!(check_lending_protocol_dependencies(&Sandbox::new(&st), &domain));
        for off in [am::SINGLE_ASSET_VAULT, am::MPTOKENS_V1] {
            let st = state_with(&without(&[off]));
            assert!(!check_lending_protocol_dependencies(&Sandbox::new(&st), &plain));
        }
        let st = state_with(&without(&[am::PERMISSIONED_DOMAINS]));
        assert!(check_lending_protocol_dependencies(&Sandbox::new(&st), &plain));
        assert!(!check_lending_protocol_dependencies(&Sandbox::new(&st), &domain));
    }

    /// Transactor.h:640-653, Transactor.cpp:231-250: the amendment, the
    /// extra features, delegation, the flag mask — in that order.
    #[test]
    fn the_gate_is_disabled_then_delegation_then_flags() {
        let mask = crate::tx::protocol::TF_UNIVERSAL_MASK;
        let st = state_with(&without(&[am::LENDING_PROTOCOL]));
        let rules = Sandbox::new(&st);
        assert_eq!(gate(&tx(json!({})), &rules, true, mask), Some(TxResult::Disabled));
        // Disabled wins over everything behind it.
        let bad = tx(json!({"Delegate": hex::encode([0x12u8; 20]), "Flags": 1}));
        assert_eq!(gate(&bad, &rules, true, mask), Some(TxResult::Disabled));
        let st = state_with(&ALL);
        let rules = Sandbox::new(&st);
        assert_eq!(gate(&tx(json!({})), &rules, true, mask), None);
        assert_eq!(gate(&tx(json!({})), &rules, false, mask), Some(TxResult::Disabled));
        assert_eq!(gate(&bad, &rules, true, mask), Some(TxResult::InvalidTx));
        assert_eq!(gate(&tx(json!({"Flags": 1})), &rules, true, mask), Some(TxResult::InvalidFlag));
        assert_eq!(gate(&tx(json!({"Flags": 0x8000_0000u64})), &rules, true, mask), None);
    }

    /// STAmount.cpp:1737-1741 with roundToScale (STAmount.cpp:1404-1424).
    #[test]
    fn is_zero_at_scale_rounds_to_nearest() {
        // At scale -10 (one unit of the sixteenth digit of 1e5) 1e-11 is
        // below half a unit, 5e-11 is half a unit and rounds to even/up.
        assert!(is_zero_at_scale(&usd("0.00000000001"), -10).unwrap());
        assert!(is_zero_at_scale(&usd("0.00000000004"), -10).unwrap());
        assert!(!is_zero_at_scale(&usd("0.00000000006"), -10).unwrap());
        assert!(!is_zero_at_scale(&usd("1"), -10).unwrap());
        // Exactly one unit of the scale stays.
        assert!(!is_zero_at_scale(&usd("0.0000000001"), -10).unwrap());
        // Integral assets never round.
        let xrp = Amount::from_json(&json!("1")).unwrap();
        assert!(!is_zero_at_scale(&xrp, 5).unwrap());
    }

    /// LendingHelpers.cpp:38-67 (LoanBroker_test.cpp:2886 "Cover precision
    /// guard: Withdraw", :2903 "Clawback": against a cover of 1e5 with
    /// sixteen digits, an amount of 1e-11 is tecPRECISION_LOSS).
    #[test]
    fn can_apply_to_broker_cover_refuses_zero_and_sub_scale_amounts_under_fix_cleanup_3_2_0() {
        let st = state_with(&ALL);
        let sb = Sandbox::new(&st);
        let broker = json!({"LedgerEntryType": "LoanBroker", "CoverAvailable": "100000"});
        let check = |amount: &Amount, broker: &Value| can_apply_to_broker_cover(&sb, broker, USD, amount).unwrap();
        assert_eq!(check(&usd("0"), &broker), TxResult::PrecisionLoss);
        // scale(100000 USD) = -10.
        assert_eq!(check(&usd("0.00000000001"), &broker), TxResult::PrecisionLoss);
        assert_eq!(check(&usd("0.0000000001"), &broker), TxResult::Success);
        assert_eq!(check(&usd("1"), &broker), TxResult::Success);
        // An empty cover has the zero exponent, -100: nothing is below it.
        let empty = json!({"LedgerEntryType": "LoanBroker"});
        assert_eq!(check(&usd("0.00000000001"), &empty), TxResult::Success);
        assert_eq!(check(&usd("0"), &empty), TxResult::PrecisionLoss);
        // Before the fix there is no guard at all.
        let st = state_with(&without(&[am::FIX_CLEANUP_3_2_0]));
        let sb = Sandbox::new(&st);
        assert_eq!(can_apply_to_broker_cover(&sb, &broker, USD, &usd("0")).unwrap(), TxResult::Success);
        assert_eq!(can_apply_to_broker_cover(&sb, &broker, USD, &usd("0.00000000001")).unwrap(), TxResult::Success);
    }

    /// LendingHelpers.h:257-263.
    #[test]
    fn assets_total_scale_is_the_exponent_of_the_vaults_total() {
        assert_eq!(assets_total_scale(&usd_vault("10000.15696446053"), Rounding::ToNearest).unwrap(), -11);
        assert_eq!(assets_total_scale(&usd_vault("100000"), Rounding::ToNearest).unwrap(), -10);
        // An empty IOU vault: the zero exponent.
        let empty = json!({"LedgerEntryType": "Vault", "Asset": {"currency": hex::encode_upper(USD_CODE), "issuer": hex::encode(ISSUER)}});
        assert_eq!(assets_total_scale(&empty, Rounding::ToNearest).unwrap(), -100);
        let xrp = json!({"LedgerEntryType": "Vault", "Asset": {"currency": "XRP"}, "AssetsTotal": "12345"});
        assert_eq!(assets_total_scale(&xrp, Rounding::ToNearest).unwrap(), 0);
    }

    /// LendingHelpers.h:268-278: rounded up at the vault's scale.
    #[test]
    fn minimum_broker_cover_is_the_rate_of_the_debt_rounded_up_at_the_vault_scale() {
        // 10% of 1000 is exactly 100.
        let vault = usd_vault("100000");
        assert!(minimum_broker_cover(n("1000"), 10_000, &vault).unwrap() == n("100"));
        // 333 tenth-bips of 1 = 0.00333, exact.
        assert!(minimum_broker_cover(n("1"), 333, &vault).unwrap() == n("0.00333"));
        // A debt whose share has digits below the vault scale (-10) is
        // rounded UP there: 10% of 0.00000000011 = 0.000000000011 → 0.0000000001.
        assert!(minimum_broker_cover(n("0.00000000011"), 10_000, &vault).unwrap() == n("0.0000000001"));
        // A zero rate or a zero debt needs no cover.
        assert!(minimum_broker_cover(n("1000"), 0, &vault).unwrap().is_zero());
        assert!(minimum_broker_cover(Number::ZERO, 10_000, &vault).unwrap().is_zero());
        // An XRP vault: whole drops, rounded up. 10% of 15 drops = 1.5 → 2.
        let xrp = json!({"LedgerEntryType": "Vault", "Asset": {"currency": "XRP"}, "AssetsTotal": "12345"});
        assert!(minimum_broker_cover(n("15"), 10_000, &xrp).unwrap() == n("2"));
        assert!(minimum_broker_cover(n("20"), 10_000, &xrp).unwrap() == n("2"));
    }
}
