//! rippled's vault helpers — `src/libxrpl/ledger/helpers/VaultHelpers.cpp`
//! and `include/xrpl/ledger/helpers/VaultHelpers.h` (3.4.1) — over the
//! engine's JSON objects:
//!
//! | here | VaultHelpers.cpp |
//! |---|---|
//! | [`assets_to_shares_deposit`] | :27-49 |
//! | [`shares_to_assets_deposit`] | :51-72 |
//! | [`clamp_to_assets_total_scale`] | :74-131 |
//! | [`assets_total_for_withdrawal`] | :133-140 |
//! | [`debit_is_non_zero_dust`] | :142-148 |
//! | [`assets_to_shares_withdraw`] | :150-175 |
//! | [`shares_to_assets_withdraw`] | :177-198 |
//! | [`is_sole_shareholder`] | :200-218 |
//! | [`vault_version`] | :220-236 |
//! | [`vault_kind`], [`tx_vault_kind`], [`is_valid_vault_kind`] | :238-271 |
//! | [`is_valid_closed_ended_gap`] | :273-279 |
//! | [`vault_phase`], [`vault_phase_of`] | :281-306 |
//! | [`check_vault_domain`] | :308-327 |
//!
//! # Rounding
//!
//! The conversions are `Number` arithmetic at the thread's mantissa scale
//! (Large330 whenever a vault exists). rippled writes them with no rounding
//! guard of their own, so each `Number` operation and each
//! `STAmount = Number` assignment runs under the caller's thread rounding
//! mode — `ambient` here, ToNearest in every vault transactor (none installs
//! a guard around these calls). What the functions themselves fix:
//!
//! - a deposit's shares are TRUNCATED (`Number::truncate()`, towards zero)
//!   before they become an MPT amount (:44, :47): a depositor never gets a
//!   fraction of a share rounded up;
//! - a withdrawal's shares are truncated only on request (`TruncateShares`,
//!   :171-172); otherwise the Number becomes an MPT amount under the ambient
//!   mode — at ToNearest, half to even (37.5 shares → 38);
//! - assets computed from shares are NOT truncated: they become an amount of
//!   the vault's asset under the ambient mode (sixteen digits for an IOU,
//!   whole units for XRP and MPT);
//! - `clamp_to_assets_total_scale` installs its own modes: ToNearest for the
//!   posterior scale, Downward for the amount (:93, :102, :112, :117).
//!
//! A `Number` or `STAmount` throw (overflow, division by zero) is the `Err`:
//! rippled's transactors catch it where they convert (tecPATH_DRY in
//! VaultDeposit / VaultWithdraw).

use crate::flow::st_amount::StError;
use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::TxResult;
use crate::tx::holding::{has_expired, ExpiryComparison};
use crate::tx::mpt::{self, HeldAsset};
use crate::tx::number::{Number, NumberError, Rounding};
use crate::tx::protocol::{VaultKind, VaultPhase, VaultVersion, MAX_INVESTMENT_PERIOD, MIN_INVESTMENT_PERIOD};
use crate::tx::st_number::{number_field, round_to_asset, round_to_scale, scale, Amount};
use serde_json::Value;

fn arith(_: NumberError) -> StError {
    StError::Overflow
}

fn div(n: Number, d: Number, mode: Rounding) -> Result<Number, StError> {
    n.div(d, mode).map_err(|e| match e {
        NumberError::DivideByZero => StError::DivideByZero,
        NumberError::Overflow => StError::Overflow,
    })
}

/// The vault's `sfAsset`.
pub fn vault_asset(vault: &Value) -> Option<HeldAsset> {
    vault.get("Asset").and_then(mpt::parse_asset)
}

/// The vault's `sfShareMPTID` as an asset.
pub fn vault_share(vault: &Value) -> Option<HeldAsset> {
    let id = hex::decode(vault.get("ShareMPTID")?.as_str()?).ok()?;
    Some(HeldAsset::Mpt(id.as_slice().try_into().ok()?))
}

/// `vault->at(sfScale)`: SoeDefault, zero when absent.
fn vault_scale(vault: &Value) -> i32 {
    vault.get("Scale").and_then(|s| s.as_u64()).unwrap_or(0) as i32
}

/// `issuance->at(sfOutstandingAmount)` as a Number.
fn share_total(issuance: &Value, mode: Rounding) -> Result<Number, StError> {
    Number::from_parts(false, mpt::dec_field(issuance, "OutstandingAmount") as u128, 0, mode).map_err(arith)
}

/// `TruncateShares` (VaultHelpers.h:77).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TruncateShares {
    No,
    Yes,
}

/// `WaiveUnrealizedLoss` (VaultHelpers.h:86).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WaiveUnrealizedLoss {
    No,
    Yes,
}

/// `SuppressExpired` (VaultHelpers.h:272).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SuppressExpired {
    No,
    Yes,
}

/// `assetsToSharesDeposit(vault, issuance, assets)` (:27-49): the shares a
/// deposit of `assets` mints. None (rippled's nullopt) for a negative amount
/// or one that is not of the vault's asset.
///
/// - An empty vault (`sfAssetsTotal` zero): `assets × 10^Scale`, truncated —
///   `Number(assets.mantissa(), assets.exponent() + Scale).truncate()`.
/// - Otherwise `((shareTotal × assets) / assetsTotal).truncate()`, product
///   first.
///
/// The result becomes an MPT amount: past the largest MPT amount is the
/// throw (`Err`).
pub fn assets_to_shares_deposit(vault: &Value, issuance: &Value, assets: &Amount, ambient: Rounding) -> Result<Option<Amount>, StError> {
    let (Some(asset), Some(share)) = (vault_asset(vault), vault_share(vault)) else { return Ok(None) };
    if (assets.negative && !assets.is_zero()) || assets.asset != asset {
        return Ok(None);
    }
    let asset_total = number_field(vault, "AssetsTotal");
    let shares = if asset_total.is_zero() {
        Number::from_parts(false, assets.mantissa as u128, assets.exponent + vault_scale(vault), ambient).map_err(arith)?.truncate()
    } else {
        let product = share_total(issuance, ambient)?.mul(assets.number(), ambient).map_err(arith)?;
        div(product, asset_total, ambient)?.truncate()
    };
    Amount::from_number(share, shares, ambient).map(Some)
}

/// `sharesToAssetsDeposit(vault, issuance, shares)` (:51-72): the assets a
/// deposit minting `shares` takes. None for a negative amount or one that is
/// not of the vault's share.
///
/// - An empty vault: `shares × 10^-Scale` —
///   `STAmount{asset, shares.mantissa(), shares.exponent() - Scale, false}`.
/// - Otherwise `(assetsTotal × shares) / shareTotal`, assigned to an amount
///   of the vault's asset (no truncation).
pub fn shares_to_assets_deposit(vault: &Value, issuance: &Value, shares: &Amount, ambient: Rounding) -> Result<Option<Amount>, StError> {
    let (Some(asset), Some(share)) = (vault_asset(vault), vault_share(vault)) else { return Ok(None) };
    if (shares.negative && !shares.is_zero()) || shares.asset != share {
        return Ok(None);
    }
    let asset_total = number_field(vault, "AssetsTotal");
    if asset_total.is_zero() {
        return Amount::new(asset, shares.mantissa as u128, shares.exponent - vault_scale(vault), false, ambient).map(Some);
    }
    let product = asset_total.mul(shares.number(), ambient).map_err(arith)?;
    let assets = div(product, share_total(issuance, ambient)?, ambient)?;
    Amount::from_number(asset, assets, ambient).map(Some)
}

/// `clampToAssetsTotalScale(vault, delta)` (:74-131): the part of a change
/// to `sfAssetsTotal` that survives at the precision the total will have
/// AFTER the change — so the vault never moves an amount its own total
/// cannot record. `delta` is signed (negative for a withdrawal); the result
/// is its clamped MAGNITUDE, positive. `Err(tecPRECISION_LOSS)` when
/// nothing survives (:126-128).
///
/// - An integral asset (XRP, MPT): the magnitude as it is.
/// - The posterior scale is `scale(assetsTotal + delta, asset)` computed at
///   ToNearest — the scale the vault invariants check against (:92-95).
/// - A debit: the magnitude floored to that scale (`roundToScale`,
///   Downward) (:98-103).
/// - A credit: the posterior `assetsTotal + magnitude` summed Downward,
///   rounded to the asset and that scale Downward, less the total
///   (:104-120) — so a sum that ToNearest would round UP cannot credit the
///   vault more than was deposited.
///
/// Every step that matters carries its own mode; the answer does not depend
/// on `ambient` (rippled's unit test runs it under all four,
/// VaultHelpers_test.cpp:83-116).
pub fn clamp_to_assets_total_scale(vault: &Value, delta: &Amount, ambient: Rounding) -> Result<Result<Amount, TxResult>, StError> {
    let magnitude = Amount { negative: false, ..*delta };
    if magnitude.integral() {
        return Ok(Ok(magnitude));
    }
    let asset = delta.asset;
    let assets_total = number_field(vault, "AssetsTotal");
    let is_debit = delta.negative && !delta.is_zero();
    let post_scale = scale(assets_total.add(delta.number(), Rounding::ToNearest).map_err(arith)?, asset, Rounding::ToNearest)?;
    let actual = if is_debit {
        round_to_scale(magnitude, post_scale, Rounding::Downward)?
    } else {
        let posterior = assets_total.add(magnitude.number(), Rounding::Downward).map_err(arith)?;
        let rounded_posterior = round_to_asset(asset, posterior, post_scale, Rounding::Downward)?;
        Amount::from_number(asset, rounded_posterior.sub(assets_total, ambient).map_err(arith)?, ambient)?
    };
    if actual.is_zero() || actual.negative {
        return Ok(Err(TxResult::PrecisionLoss));
    }
    Ok(Ok(actual))
}

/// `assetsTotalForWithdrawal(vault, waive)` (:133-140): `sfAssetsTotal`,
/// less `sfLossUnrealized` unless the loss is waived — the total a
/// withdrawal's exchange rate is struck against.
pub fn assets_total_for_withdrawal(vault: &Value, waive: WaiveUnrealizedLoss, ambient: Rounding) -> Result<Number, StError> {
    let total = number_field(vault, "AssetsTotal");
    match waive {
        WaiveUnrealizedLoss::Yes => Ok(total),
        WaiveUnrealizedLoss::No => total.sub(number_field(vault, "LossUnrealized"), ambient).map_err(arith),
    }
}

/// `debitIsNonZeroDust(asset, total, amount)` (:142-148): a non-zero amount
/// whose removal leaves the total unchanged at the asset's precision —
/// `STAmount{asset, total - amount} == STAmount{asset, total}`.
pub fn debit_is_non_zero_dust(asset: HeldAsset, total: Number, amount: Number, ambient: Rounding) -> Result<bool, StError> {
    if amount.is_zero() {
        return Ok(false);
    }
    let after = Amount::from_number(asset, total.sub(amount, ambient).map_err(arith)?, ambient)?;
    Ok(after == Amount::from_number(asset, total, ambient)?)
}

/// `assetsToSharesWithdraw(vault, issuance, assets, truncate = No, waive =
/// No)` (:150-175): the shares a withdrawal of `assets` burns. None for a
/// negative amount or one not of the vault's asset. Zero shares when the
/// withdrawal total is zero; else `(shareTotal × assets) / assetsTotal`,
/// truncated on request, then an MPT amount under the ambient mode.
pub fn assets_to_shares_withdraw(
    vault: &Value,
    issuance: &Value,
    assets: &Amount,
    truncate: TruncateShares,
    waive: WaiveUnrealizedLoss,
    ambient: Rounding,
) -> Result<Option<Amount>, StError> {
    let (Some(asset), Some(share)) = (vault_asset(vault), vault_share(vault)) else { return Ok(None) };
    if (assets.negative && !assets.is_zero()) || assets.asset != asset {
        return Ok(None);
    }
    let asset_total = assets_total_for_withdrawal(vault, waive, ambient)?;
    if asset_total.is_zero() {
        return Ok(Some(Amount::zero(share)));
    }
    let product = share_total(issuance, ambient)?.mul(assets.number(), ambient).map_err(arith)?;
    let mut result = div(product, asset_total, ambient)?;
    if truncate == TruncateShares::Yes {
        result = result.truncate();
    }
    Amount::from_number(share, result, ambient).map(Some)
}

/// `sharesToAssetsWithdraw(vault, issuance, shares, waive = No)` (:177-198):
/// the assets a withdrawal burning `shares` pays. None for a negative amount
/// or one not of the vault's share. Zero when the withdrawal total is zero;
/// else `(assetsTotal × shares) / shareTotal` as an amount of the asset.
pub fn shares_to_assets_withdraw(
    vault: &Value,
    issuance: &Value,
    shares: &Amount,
    waive: WaiveUnrealizedLoss,
    ambient: Rounding,
) -> Result<Option<Amount>, StError> {
    let (Some(asset), Some(share)) = (vault_asset(vault), vault_share(vault)) else { return Ok(None) };
    if (shares.negative && !shares.is_zero()) || shares.asset != share {
        return Ok(None);
    }
    let asset_total = assets_total_for_withdrawal(vault, waive, ambient)?;
    if asset_total.is_zero() {
        return Ok(Some(Amount::zero(asset)));
    }
    let product = asset_total.mul(shares.number(), ambient).map_err(arith)?;
    let assets = div(product, share_total(issuance, ambient)?, ambient)?;
    Amount::from_number(asset, assets, ambient).map(Some)
}

/// `isSoleShareholder(view, account, issuance)` (:200-218): the account's
/// MPToken holds every outstanding share. Never with nothing outstanding or
/// without the token.
pub fn is_sole_shareholder(sandbox: &Sandbox, account: &[u8; 20], issuance: &Value) -> bool {
    let outstanding = mpt::dec_field(issuance, "OutstandingAmount");
    if outstanding == 0 {
        return false;
    }
    let (Some(issuer), Some(sequence)) = (mpt::issuance_issuer(issuance), issuance.get("Sequence").and_then(|s| s.as_u64())) else {
        return false;
    };
    let mut share = [0u8; 24];
    share[..4].copy_from_slice(&(sequence as u32).to_be_bytes());
    share[4..].copy_from_slice(&issuer);
    match mpt::json_at(sandbox, &keylet::mptoken_key(&keylet::mpt_issuance_key(&share), account)) {
        Some(token) => mpt::dec_field(&token, "MPTAmount") == outstanding,
        None => false,
    }
}

/// `getVaultVersion(vault)` (:220-236): `sfLEVersion`; absent is Legacy, and
/// so (rippled's unreachable arm) is a value above CashBasis.
pub fn vault_version(vault: &Value) -> VaultVersion {
    match vault.get("LEVersion").and_then(|v| v.as_u64()) {
        Some(1) => VaultVersion::CashBasis,
        _ => VaultVersion::Legacy,
    }
}

fn decode_vault_kind(kind: Option<u64>) -> VaultKind {
    if kind == Some(VaultKind::ClosedEnded as u64) {
        VaultKind::ClosedEnded
    } else {
        VaultKind::OpenEnded
    }
}

/// `getVaultKind(vault)` (:250-255): `sfVaultKind`; absent, or anything but
/// ClosedEnded, is OpenEnded (:240-246).
pub fn vault_kind(vault: &Value) -> VaultKind {
    decode_vault_kind(vault.get("VaultKind").and_then(|k| k.as_u64()))
}

/// `getVaultKind(tx)` (:257-261): the same from a transaction's fields.
pub fn tx_vault_kind(tx_fields: &Value) -> VaultKind {
    vault_kind(tx_fields)
}

/// `isValidVaultKind(tx)` (:263-271): no `sfVaultKind`, or OpenEnded (0) or
/// ClosedEnded (1).
pub fn is_valid_vault_kind(tx_fields: &Value) -> bool {
    match tx_fields.get("VaultKind") {
        None => true,
        Some(kind) => matches!(kind.as_u64(), Some(0) | Some(1)),
    }
}

/// `isValidClosedEndedGap(sub, red)` (:273-279): the investment period
/// `RedemptionDate - SubscriptionDate` is at least kMinInvestmentPeriod
/// (180 s, inclusive) and below kMaxInvestmentPeriod (thirty years,
/// exclusive), in 64-bit arithmetic.
pub fn is_valid_closed_ended_gap(subscription_date: u32, redemption_date: u32) -> bool {
    let (s, r) = (subscription_date as i64, redemption_date as i64);
    r >= s + MIN_INVESTMENT_PERIOD as i64 && r < s + MAX_INVESTMENT_PERIOD as i64
}

/// `getVaultPhase(view, vaultKind, subscriptionDate, redemptionDate)`
/// (:288-306), with `now` the parent ledger's close time:
///
/// - not ClosedEnded: NoPhase;
/// - Subscription while `now <= SubscriptionDate` — the test is
///   `!hasExpired(view, subscriptionDate, Exclusive)`, so the subscription
///   date itself still subscribes ("Investment starts strictly after
///   SubscriptionDate", :299-300);
/// - Investment while `SubscriptionDate < now < RedemptionDate` —
///   `!hasExpired(view, redemptionDate)` with the default Inclusive test;
/// - Redemption from `now >= RedemptionDate` on.
///
/// A missing date never expires (View.cpp:57-58): a closed-ended vault
/// without a SubscriptionDate stays in Subscription, and one without a
/// RedemptionDate never leaves Investment.
pub fn vault_phase_of(sandbox: &Sandbox, vault_kind: Option<u8>, subscription_date: Option<u32>, redemption_date: Option<u32>) -> VaultPhase {
    if vault_kind != Some(VaultKind::ClosedEnded as u8) {
        return VaultPhase::NoPhase;
    }
    if !has_expired(sandbox, subscription_date, ExpiryComparison::Exclusive) {
        return VaultPhase::Subscription;
    }
    if !has_expired(sandbox, redemption_date, ExpiryComparison::Inclusive) {
        return VaultPhase::Investment;
    }
    VaultPhase::Redemption
}

/// `getVaultPhase(view, vault)` (:281-286): the phase from the vault's own
/// `sfVaultKind`, `sfSubscriptionDate` and `sfRedemptionDate`.
pub fn vault_phase(sandbox: &Sandbox, vault: &Value) -> VaultPhase {
    let field = |name: &str| vault.get(name).and_then(|v| v.as_u64());
    vault_phase_of(sandbox, field("VaultKind").map(|k| k as u8), field("SubscriptionDate").map(|d| d as u32), field("RedemptionDate").map(|d| d as u32))
}

/// `checkVaultDomain(view, issuance, subject, suppressExpired)` (:308-327):
/// whether `subject` belongs to the domain of a private vault's share
/// issuance. tecNO_AUTH when the issuance has no `sfDomainID`; otherwise
/// `credentials::validDomain` — tecOBJECT_NOT_FOUND without the domain,
/// tecEXPIRED when only expired credentials match, tecNO_AUTH with none —
/// except that tecEXPIRED is tesSUCCESS when suppressed (preclaim lets it
/// through so that do_apply can delete the expired credentials).
pub fn check_vault_domain(sandbox: &Sandbox, issuance: &Value, subject: &[u8; 20], suppress_expired: SuppressExpired) -> TxResult {
    let Some(domain) = mpt::key_field(issuance, "DomainID") else { return TxResult::NoAuth };
    let verdict = crate::tx::misc::valid_domain(sandbox, &domain, subject);
    if verdict == TxResult::Expired && suppress_expired == SuppressExpired::Yes {
        return TxResult::Success;
    }
    verdict
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::keylet::vault_devnet_fixture::object;
    use crate::ledger::state::LedgerState;
    use crate::tx::number::{MantissaScale, ScaleGuard};
    use serde_json::json;
    use xrpl_core::types::Hash256;

    const ISSUER: [u8; 20] = [0x21; 20];
    const PSEUDO: [u8; 20] = [0x31; 20];
    const HOLDER: [u8; 20] = [0x11; 20];
    const IOU: HeldAsset = HeldAsset::Iou { currency: [0x55; 20], issuer: ISSUER };
    const UNDERLYING_MPT: HeldAsset = HeldAsset::Mpt([0x07; 24]);
    const AMBIENT: Rounding = Rounding::ToNearest;

    fn share_id() -> [u8; 24] {
        let mut id = [0u8; 24];
        id[..4].copy_from_slice(&1u32.to_be_bytes());
        id[4..].copy_from_slice(&PSEUDO);
        id
    }

    fn share() -> HeldAsset {
        HeldAsset::Mpt(share_id())
    }

    fn large() -> ScaleGuard {
        ScaleGuard::new(MantissaScale::Large330)
    }

    fn n(m: i64, e: i32) -> Number {
        Number::new(m, e, Rounding::ToNearest).unwrap()
    }

    /// A vault of `asset` with the given scale and totals, and its share
    /// issuance with `shares` outstanding.
    fn vault(asset: HeldAsset, vault_scale: u8, assets_total: &str, loss: &str, shares: u64) -> (Value, Value) {
        let mut vault = json!({
            "LedgerEntryType": "Vault", "Flags": 0, "Asset": crate::tx::st_number::asset_to_json(&asset),
            "ShareMPTID": hex::encode_upper(share_id()), "Account": hex::encode(PSEUDO), "WithdrawalPolicy": 1,
        });
        if vault_scale != 0 {
            vault["Scale"] = json!(vault_scale);
        }
        for (field, value) in [("AssetsTotal", assets_total), ("AssetsAvailable", assets_total), ("LossUnrealized", loss)] {
            if value != "0" {
                vault[field] = json!(value);
            }
        }
        let issuance = json!({
            "LedgerEntryType": "MPTokenIssuance", "Flags": 56, "Issuer": hex::encode(PSEUDO), "Sequence": 1,
            "OutstandingAmount": shares.to_string(), "OwnerNode": "0",
        });
        (vault, issuance)
    }

    fn iou(m: i64, e: i32) -> Amount {
        Amount::from_number(IOU, n(m, e), Rounding::ToNearest).unwrap()
    }

    fn shares(value: u64) -> Amount {
        Amount { asset: share(), negative: false, mantissa: value, exponent: 0 }
    }

    fn drops(value: u64) -> Amount {
        Amount { asset: HeldAsset::Xrp, negative: false, mantissa: value, exponent: 0 }
    }

    /// The first deposit into an empty vault of an IOU with Scale 1, from
    /// rippled's `VaultScale_test.cpp` (src/test/app/vault/): "Scale deposit
    /// exact" :185-197 (1 IOU → 10 shares), "exact, using full precision"
    /// :210-222 (1.5 → 15), "truncating from .5" :225-240 (1.25 → 12 shares,
    /// which cost 1.2), "insignificant amount" :200-208 (0.09 → no share:
    /// the transactor's tecPRECISION_LOSS).
    #[test]
    fn a_first_deposit_scales_assets_into_shares_and_truncates() {
        let _scale = large();
        let (vault, issuance) = vault(IOU, 1, "0", "0", 0);
        for (assets, minted, cost) in [
            (iou(1, 0), 10u64, iou(1, 0)),
            (iou(15, -1), 15, iou(15, -1)),
            (iou(125, -2), 12, iou(12, -1)),
            (iou(1201, -3), 12, iou(12, -1)),
            (iou(1299, -3), 12, iou(12, -1)),
            (iou(9, -2), 0, Amount::zero(IOU)),
        ] {
            let got = assets_to_shares_deposit(&vault, &issuance, &assets, AMBIENT).unwrap().unwrap();
            assert_eq!(got, if minted == 0 { Amount::zero(share()) } else { shares(minted) }, "{assets:?}");
            assert_eq!(shares_to_assets_deposit(&vault, &issuance, &got, AMBIENT).unwrap().unwrap(), cost, "{assets:?}");
        }
        // The default IOU scale, 6, and no scale (XRP, MPT): one share per unit.
        let (six, issuance) = super::tests::vault(IOU, 6, "0", "0", 0);
        assert_eq!(assets_to_shares_deposit(&six, &issuance, &iou(10_000, 0), AMBIENT), Ok(Some(shares(10_000_000_000))));
        assert_eq!(shares_to_assets_deposit(&six, &issuance, &shares(10_000_000_000), AMBIENT), Ok(Some(iou(10_000, 0))));
        let (xrp, issuance) = super::tests::vault(HeldAsset::Xrp, 0, "0", "0", 0);
        assert_eq!(assets_to_shares_deposit(&xrp, &issuance, &drops(10_000), AMBIENT), Ok(Some(shares(10_000))));
        assert_eq!(shares_to_assets_deposit(&xrp, &issuance, &shares(10_000), AMBIENT), Ok(Some(drops(10_000))));
    }

    /// The same two vaults as they stand on devnet (ledger 7131244): the XRP
    /// vault holds 10000 drops against 10000 shares, and the USD vault's
    /// share issuance has AssetScale 6 — 10,000,000,000 shares.
    #[test]
    fn the_devnet_vaults_convert_at_their_own_rate() {
        let _scale = large();
        let (vault, _) = object("vault_xrp");
        let (issuance, _) = object("share_issuance_xrp");
        let share_asset = vault_share(&vault).unwrap();
        let hundred = Amount { asset: share_asset, negative: false, mantissa: 100, exponent: 0 };
        assert_eq!(vault_asset(&vault), Some(HeldAsset::Xrp));
        assert_eq!(assets_to_shares_deposit(&vault, &issuance, &drops(100), AMBIENT), Ok(Some(hundred)));
        assert_eq!(shares_to_assets_withdraw(&vault, &issuance, &hundred, WaiveUnrealizedLoss::No, AMBIENT), Ok(Some(drops(100))));
        assert_eq!(vault_version(&vault), VaultVersion::Legacy);
        assert_eq!(vault_kind(&vault), VaultKind::OpenEnded);

        let (vault, _) = object("vault_usd_closed_ended");
        let (issuance, _) = object("share_issuance_usd");
        assert_eq!(vault_version(&vault), VaultVersion::CashBasis);
        assert_eq!(vault_kind(&vault), VaultKind::ClosedEnded);
        assert_eq!(mpt::dec_field(&issuance, "OutstandingAmount"), 10_000_000_000);
        let asset = vault_asset(&vault).unwrap();
        // 10000.15696446053 USD back 1e10 shares: one USD buys floor(1e10 / 10000.15696446053) = 999984 shares.
        let one = Amount::from_number(asset, n(1, 0), Rounding::ToNearest).unwrap();
        let minted = assets_to_shares_deposit(&vault, &issuance, &one, AMBIENT).unwrap().unwrap();
        assert_eq!(minted.mantissa, 999_984);
    }

    /// Later deposits strike the vault's rate and truncate
    /// (VaultScale_test.cpp :242-262: at 1.2 IOU / 12 shares, 1.201 and
    /// 1.299 each mint 12 shares for 1.2; :275-284: 0.69 mints 6).
    #[test]
    fn a_later_deposit_uses_the_vaults_rate() {
        let _scale = large();
        let (vault, issuance) = vault(IOU, 1, "1.2", "0", 12);
        for (assets, minted, cost) in [(iou(1201, -3), 12u64, iou(12, -1)), (iou(1299, -3), 12, iou(12, -1)), (iou(69, -2), 6, iou(6, -1))] {
            assert_eq!(assets_to_shares_deposit(&vault, &issuance, &assets, AMBIENT), Ok(Some(shares(minted))), "{assets:?}");
            assert_eq!(shares_to_assets_deposit(&vault, &issuance, &shares(minted), AMBIENT), Ok(Some(cost)), "{assets:?}");
        }
        // A vault that has earned: 150 assets behind 100 shares. 10 assets mint floor(6.66…) = 6 shares, which cost 9.
        let (vault, issuance) = super::tests::vault(HeldAsset::Xrp, 0, "150", "0", 100);
        assert_eq!(assets_to_shares_deposit(&vault, &issuance, &drops(10), AMBIENT), Ok(Some(shares(6))));
        assert_eq!(shares_to_assets_deposit(&vault, &issuance, &shares(6), AMBIENT), Ok(Some(drops(9))));
        // A deposit's rate ignores the unrealized loss.
        let (vault, issuance) = super::tests::vault(HeldAsset::Xrp, 0, "150", "50", 100);
        assert_eq!(assets_to_shares_deposit(&vault, &issuance, &drops(15), AMBIENT), Ok(Some(shares(10))));
    }

    /// Scale 18 (VaultScale_test.cpp :156-198, :377-392): 5 IOU is 5e18
    /// shares; 10 IOU is 1e19, past the largest MPT amount — the throw the
    /// transactor turns into tecPATH_DRY.
    #[test]
    fn shares_past_the_largest_mpt_amount_are_the_throw() {
        let _scale = large();
        let (empty, issuance) = vault(IOU, 18, "0", "0", 0);
        assert_eq!(assets_to_shares_deposit(&empty, &issuance, &iou(5, 0), AMBIENT), Ok(Some(shares(5_000_000_000_000_000_000))));
        assert_eq!(assets_to_shares_deposit(&empty, &issuance, &iou(10, 0), AMBIENT), Err(StError::Overflow));
        let (funded, issuance) = vault(IOU, 18, "5", "0", 5_000_000_000_000_000_000);
        assert_eq!(assets_to_shares_deposit(&funded, &issuance, &iou(10, 0), AMBIENT), Err(StError::Overflow));
        assert_eq!(
            assets_to_shares_withdraw(&funded, &issuance, &iou(10, 0), TruncateShares::No, WaiveUnrealizedLoss::No, AMBIENT),
            Err(StError::Overflow)
        );
        // No shares outstanding behind a non-zero total: Number's division by zero.
        let (broken, issuance) = vault(IOU, 1, "5", "0", 0);
        assert_eq!(shares_to_assets_deposit(&broken, &issuance, &shares(1), AMBIENT), Err(StError::DivideByZero));
    }

    /// Withdrawals (VaultScale_test.cpp): "Scale redeem exact" :309-325
    /// (100 of 1000 shares over 100 IOU → 10), "redeem with rounding"
    /// :327-354 (25 of 900 over 90 → 2.5), :356-375 (21 of 875 over 87.5 →
    /// 2.1); "withdraw exact" :427-447 (10 IOU → 100 shares), "with rounding
    /// assets" :518-551 (2.5 → 25), "rounding shares up" :553-578 (3.75 of
    /// 87.5 / 875 is 37.5 shares: 38 untruncated — to nearest even — and 37
    /// truncated, which pay 3.8 and 3.7), "rounding shares down" :580-…
    /// (3.72 of 83.8 / 838 is 37.199…: 37 either way).
    #[test]
    fn a_withdrawal_converts_both_ways() {
        let _scale = large();
        let no = WaiveUnrealizedLoss::No;
        for (total, outstanding, redeemed, paid) in [("100", 1000u64, 100u64, iou(10, 0)), ("90", 900, 25, iou(25, -1)), ("87.5", 875, 21, iou(21, -1))] {
            let (vault, issuance) = vault(IOU, 1, total, "0", outstanding);
            assert_eq!(shares_to_assets_withdraw(&vault, &issuance, &shares(redeemed), no, AMBIENT), Ok(Some(paid)), "{redeemed} of {outstanding}");
        }
        for (total, outstanding, assets, untruncated, truncated) in [
            ("100", 1000u64, iou(10, 0), 100u64, 100u64),
            ("90", 900, iou(25, -1), 25, 25),
            ("87.5", 875, iou(375, -2), 38, 37),
            ("83.8", 838, iou(372, -2), 37, 37),
            ("87.5", 875, iou(385, -2), 38, 38),
        ] {
            let (vault, issuance) = vault(IOU, 1, total, "0", outstanding);
            assert_eq!(
                assets_to_shares_withdraw(&vault, &issuance, &assets, TruncateShares::No, no, AMBIENT),
                Ok(Some(shares(untruncated))),
                "{assets:?}"
            );
            assert_eq!(
                assets_to_shares_withdraw(&vault, &issuance, &assets, TruncateShares::Yes, no, AMBIENT),
                Ok(Some(shares(truncated))),
                "{assets:?} truncated"
            );
        }
        let (vault, issuance) = vault(IOU, 1, "87.5", "0", 875);
        assert_eq!(shares_to_assets_withdraw(&vault, &issuance, &shares(38), no, AMBIENT), Ok(Some(iou(38, -1))));
        assert_eq!(shares_to_assets_withdraw(&vault, &issuance, &shares(37), no, AMBIENT), Ok(Some(iou(37, -1))));
    }

    /// VaultHelpers.cpp:133-140, :165-166, :192-193: a withdrawal's rate is
    /// struck against the total less the unrealized loss, unless waived; a
    /// total of zero pays nothing and burns nothing.
    #[test]
    fn an_unrealized_loss_lowers_the_withdrawal_rate() {
        let _scale = large();
        let (vault, issuance) = vault(HeldAsset::Xrp, 0, "1000", "200", 1000);
        let (no, yes) = (WaiveUnrealizedLoss::No, WaiveUnrealizedLoss::Yes);
        assert_eq!(assets_total_for_withdrawal(&vault, no, AMBIENT), Ok(n(800, 0)));
        assert_eq!(assets_total_for_withdrawal(&vault, yes, AMBIENT), Ok(n(1000, 0)));
        assert_eq!(shares_to_assets_withdraw(&vault, &issuance, &shares(100), no, AMBIENT), Ok(Some(drops(80))));
        assert_eq!(shares_to_assets_withdraw(&vault, &issuance, &shares(100), yes, AMBIENT), Ok(Some(drops(100))));
        assert_eq!(assets_to_shares_withdraw(&vault, &issuance, &drops(80), TruncateShares::No, no, AMBIENT), Ok(Some(shares(100))));
        assert_eq!(assets_to_shares_withdraw(&vault, &issuance, &drops(80), TruncateShares::No, yes, AMBIENT), Ok(Some(shares(80))));
        // XRP out is a whole number of drops, to nearest even: 3 shares of 800 / 1000 is 2.4 → 2; 5 shares is 4 exactly.
        assert_eq!(shares_to_assets_withdraw(&vault, &issuance, &shares(3), no, AMBIENT), Ok(Some(drops(2))));
        // A vault whose whole total is lost: nothing either way.
        let (lost, issuance) = super::tests::vault(HeldAsset::Xrp, 0, "1000", "1000", 1000);
        assert_eq!(shares_to_assets_withdraw(&lost, &issuance, &shares(100), no, AMBIENT), Ok(Some(Amount::zero(HeldAsset::Xrp))));
        assert_eq!(assets_to_shares_withdraw(&lost, &issuance, &drops(5), TruncateShares::No, no, AMBIENT), Ok(Some(Amount::zero(share()))));
        assert_eq!(shares_to_assets_withdraw(&lost, &issuance, &shares(100), yes, AMBIENT), Ok(Some(drops(100))));
    }

    /// The asset checks every conversion starts with (:31-36, :55-60,
    /// :158-163, :185-190): the wrong asset or a negative amount is nullopt.
    #[test]
    fn a_mismatched_or_negative_amount_is_refused() {
        let _scale = large();
        let (vault, issuance) = vault(IOU, 1, "100", "0", 1000);
        let (no, t) = (WaiveUnrealizedLoss::No, TruncateShares::No);
        let negative = Amount { negative: true, ..iou(1, 0) };
        assert_eq!(assets_to_shares_deposit(&vault, &issuance, &drops(5), AMBIENT), Ok(None));
        assert_eq!(assets_to_shares_deposit(&vault, &issuance, &negative, AMBIENT), Ok(None));
        assert_eq!(assets_to_shares_withdraw(&vault, &issuance, &shares(5), t, no, AMBIENT), Ok(None));
        assert_eq!(assets_to_shares_withdraw(&vault, &issuance, &negative, t, no, AMBIENT), Ok(None));
        assert_eq!(shares_to_assets_deposit(&vault, &issuance, &iou(1, 0), AMBIENT), Ok(None));
        assert_eq!(shares_to_assets_withdraw(&vault, &issuance, &iou(1, 0), no, AMBIENT), Ok(None));
        let other_share = Amount { asset: UNDERLYING_MPT, negative: false, mantissa: 5, exponent: 0 };
        assert_eq!(shares_to_assets_deposit(&vault, &issuance, &other_share, AMBIENT), Ok(None));
        assert_eq!(shares_to_assets_withdraw(&vault, &issuance, &Amount { negative: true, ..shares(5) }, no, AMBIENT), Ok(None));
    }

    /// rippled's own table for `clampToAssetsTotalScale` —
    /// src/test/app/vault/VaultHelpers_test.cpp:216-460 — every row, under
    /// each of the four ambient modes as its `runCases` does (:83-116).
    /// `None` is tecPRECISION_LOSS.
    #[test]
    fn clamp_to_assets_total_scale_matches_rippleds_table() {
        let _scale = large();
        type Row = (&'static str, HeldAsset, (i64, i32), (i64, i32), Option<(i64, i32)>);
        let rows: [Row; 15] = [
            // IOU debits (:216-292).
            ("IOU debit: on-grid, same decade", IOU, (1_000_000_000_000_005, -9), (-1, -9), Some((1, -9))),
            ("IOU debit: sub-ULP at the naive scale, but not at the true postScale", IOU, (1_000_000, 0), (-73, -11), Some((7, -10))),
            ("IOU debit: exact at the true (finer) postScale", IOU, (1_000_000, 0), (-53, -10), Some((53, -10))),
            ("IOU debit: decade-crossing debit, floored (not exact) at finer grid", IOU, (1, 0), (-73, -17), Some((7, -16))),
            (
                "IOU debit: near-total debit, unchanged (finer postScale than magnitude)",
                IOU, (1_000_000, 0), (-9_999_999_999_999_999, -10), Some((9_999_999_999_999_999, -10)),
            ),
            // IOU credits (:297-404).
            ("IOU credit: on-grid", IOU, (1_000_000, 0), (2, -9), Some((2, -9))),
            (
                "IOU credit: overcredit fix across a scale boundary",
                IOU, (9_999_999_999_999_999, -15), (5, 0), Some((4_999_999_999_999_991, -15)),
            ),
            ("IOU credit: Finding-1 regression, ToNearest sum would overcredit", IOU, (1_000_000, 0), (9_999_999_999_999_999, -25), None),
            ("IOU credit: Finding-1 regression, 19-digit half-even tie", IOU, (1_000_000, 0), (9_995, -13), None),
            ("IOU credit: zero-total vault", IOU, (0, 0), (37, -6), Some((37, -6))),
            ("IOU credit: sub-ULP credit", IOU, (1_000_000, 0), (4, -10), None),
            // Integral assets (:412-448).
            ("MPT debit: magnitude is positive, not the signed delta", UNDERLYING_MPT, (1_000_000, 0), (-5, 0), Some((5, 0))),
            ("MPT credit: unchanged", UNDERLYING_MPT, (1_000_000, 0), (7, 0), Some((7, 0))),
            ("XRP debit: magnitude is positive, not the signed delta", HeldAsset::Xrp, (100_000, 0), (-3, 0), Some((3, 0))),
            ("XRP credit: unchanged", HeldAsset::Xrp, (100_000, 0), (10, 0), Some((10, 0))),
        ];
        for (name, asset, total, delta, expected) in rows {
            // makeVault (:65-73): the total is set and asset-rounded; each row's total is on the asset's grid already.
            let total = n(total.0, total.1);
            let mut vault = json!({"LedgerEntryType": "Vault", "Asset": crate::tx::st_number::asset_to_json(&asset)});
            crate::tx::st_number::set_number_field(&mut vault, "AssetsTotal", total, crate::tx::protocol::Soe::Default);
            crate::tx::st_number::associate_asset(&mut vault, asset, Rounding::ToNearest).unwrap();
            assert_eq!(number_field(&vault, "AssetsTotal"), total, "{name}: the total is on the grid");
            let delta_number = n(delta.0, delta.1);
            let delta = Amount::from_number(asset, delta_number, Rounding::ToNearest).unwrap();
            assert_eq!(delta.number(), delta_number, "{name}: the delta is on the grid");
            for ambient in [Rounding::ToNearest, Rounding::Downward, Rounding::Upward, Rounding::TowardsZero] {
                let got = clamp_to_assets_total_scale(&vault, &delta, ambient).unwrap();
                match expected {
                    None => assert_eq!(got, Err(TxResult::PrecisionLoss), "{name} under {ambient:?}"),
                    Some((m, e)) => {
                        let want = Amount::from_number(asset, n(m, e), Rounding::ToNearest).unwrap();
                        assert_eq!(got, Ok(want), "{name} under {ambient:?}");
                    }
                }
            }
        }
        assert_eq!(TxResult::PrecisionLoss.code_str(), "tecPRECISION_LOSS");
    }

    /// VaultHelpers.cpp:142-148.
    #[test]
    fn a_debit_below_the_totals_precision_is_dust() {
        let _scale = large();
        assert_eq!(debit_is_non_zero_dust(IOU, n(1_000_000, 0), Number::ZERO, AMBIENT), Ok(false), "zero is not dust");
        // 1e6 − 4e-11 at sixteen digits is still 1e6; − 1e-9 is not.
        assert_eq!(debit_is_non_zero_dust(IOU, n(1_000_000, 0), n(4, -11), AMBIENT), Ok(true));
        assert_eq!(debit_is_non_zero_dust(IOU, n(1_000_000, 0), n(1, -9), AMBIENT), Ok(false));
        // A fraction of a drop: 0.4 is dust, 0.6 is not.
        assert_eq!(debit_is_non_zero_dust(HeldAsset::Xrp, n(100, 0), n(4, -1), AMBIENT), Ok(true));
        assert_eq!(debit_is_non_zero_dust(HeldAsset::Xrp, n(100, 0), n(6, -1), AMBIENT), Ok(false));
        assert_eq!(debit_is_non_zero_dust(UNDERLYING_MPT, n(100, 0), n(1, 0), AMBIENT), Ok(false));
    }

    fn state(close_time: u32) -> LedgerState {
        LedgerState::new_unverified(LedgerHeader {
            sequence: 100, total_coins: 100_000_000_000_000_000,
            parent_hash: Hash256([0; 32]), transaction_hash: Hash256([0; 32]), account_hash: Hash256([0; 32]),
            parent_close_time: 0, close_time, close_time_resolution: 10, close_flags: 0,
        })
    }

    /// Inventory Q11 — VaultHelpers.cpp:288-306. With SubscriptionDate 1000
    /// and RedemptionDate 2000: Subscription up to AND AT 1000, Investment
    /// from 1001 to 1999, Redemption from 2000 on.
    #[test]
    fn the_phase_boundaries() {
        let closed = Some(VaultKind::ClosedEnded as u8);
        for (now, phase) in [
            (0u32, VaultPhase::Subscription),
            (999, VaultPhase::Subscription),
            (1000, VaultPhase::Subscription),
            (1001, VaultPhase::Investment),
            (1999, VaultPhase::Investment),
            (2000, VaultPhase::Redemption),
            (2001, VaultPhase::Redemption),
        ] {
            let st = state(now);
            let sb = Sandbox::new(&st);
            assert_eq!(vault_phase_of(&sb, closed, Some(1000), Some(2000)), phase, "parent close time {now}");
            let vault = json!({"LedgerEntryType": "Vault", "VaultKind": 1, "SubscriptionDate": 1000, "RedemptionDate": 2000});
            assert_eq!(vault_phase(&sb, &vault), phase, "parent close time {now}");
            // An open-ended vault has no phase, whatever the clock and the dates say.
            assert_eq!(vault_phase_of(&sb, None, Some(1000), Some(2000)), VaultPhase::NoPhase);
            assert_eq!(vault_phase_of(&sb, Some(0), Some(1000), Some(2000)), VaultPhase::NoPhase);
            assert_eq!(vault_phase_of(&sb, Some(2), Some(1000), Some(2000)), VaultPhase::NoPhase);
            assert_eq!(vault_phase(&sb, &json!({"LedgerEntryType": "Vault"})), VaultPhase::NoPhase);
        }
        // A missing date never expires.
        let st = state(u32::MAX);
        let sb = Sandbox::new(&st);
        assert_eq!(vault_phase_of(&sb, closed, None, Some(2000)), VaultPhase::Subscription);
        assert_eq!(vault_phase_of(&sb, closed, Some(1000), None), VaultPhase::Investment);
        // The devnet closed-ended vault (subscription 842769741, redemption 842857741).
        let (vault, _) = object("vault_usd_closed_ended");
        let (sub, red) = (vault["SubscriptionDate"].as_u64().unwrap() as u32, vault["RedemptionDate"].as_u64().unwrap() as u32);
        assert!(is_valid_closed_ended_gap(sub, red));
        for (now, phase) in [(sub, VaultPhase::Subscription), (sub + 1, VaultPhase::Investment), (red - 1, VaultPhase::Investment), (red, VaultPhase::Redemption)] {
            let st = state(now);
            assert_eq!(vault_phase(&Sandbox::new(&st), &vault), phase, "{now}");
        }
    }

    /// VaultHelpers.cpp:220-279.
    #[test]
    fn version_kind_and_the_closed_ended_gap() {
        assert_eq!(vault_version(&json!({})), VaultVersion::Legacy);
        assert_eq!(vault_version(&json!({"LEVersion": 0})), VaultVersion::Legacy);
        assert_eq!(vault_version(&json!({"LEVersion": 1})), VaultVersion::CashBasis);
        assert_eq!(vault_version(&json!({"LEVersion": 2})), VaultVersion::Legacy);
        assert_eq!(vault_kind(&json!({})), VaultKind::OpenEnded);
        assert_eq!(vault_kind(&json!({"VaultKind": 0})), VaultKind::OpenEnded);
        assert_eq!(vault_kind(&json!({"VaultKind": 1})), VaultKind::ClosedEnded);
        assert_eq!(vault_kind(&json!({"VaultKind": 2})), VaultKind::OpenEnded);
        assert_eq!(tx_vault_kind(&json!({"TransactionType": "VaultCreate", "VaultKind": 1})), VaultKind::ClosedEnded);
        assert!(is_valid_vault_kind(&json!({})));
        assert!(is_valid_vault_kind(&json!({"VaultKind": 0})) && is_valid_vault_kind(&json!({"VaultKind": 1})));
        assert!(!is_valid_vault_kind(&json!({"VaultKind": 2})) && !is_valid_vault_kind(&json!({"VaultKind": 255})));
        // 180 s is the least gap (inclusive); thirty years is one too many (exclusive).
        assert!(!is_valid_closed_ended_gap(1000, 1179));
        assert!(is_valid_closed_ended_gap(1000, 1180));
        assert!(is_valid_closed_ended_gap(1000, 1000 + 946_708_559));
        assert!(!is_valid_closed_ended_gap(1000, 1000 + 946_708_560));
        assert!(!is_valid_closed_ended_gap(2000, 1000), "a redemption before the subscription");
        // No 32-bit wrap: the sums are 64-bit.
        assert!(!is_valid_closed_ended_gap(u32::MAX - 10, u32::MAX));
        assert!(is_valid_closed_ended_gap(u32::MAX - 180, u32::MAX));
    }

    /// VaultHelpers.cpp:200-218.
    #[test]
    fn the_sole_shareholder_holds_every_share() {
        let mut st = state(10);
        let (_, issuance) = vault(HeldAsset::Xrp, 0, "1000", "0", 1000);
        let issuance_key = keylet::mpt_issuance_key(&share_id());
        let token = |st: &mut LedgerState, amount: u64| {
            let t = json!({
                "LedgerEntryType": "MPToken", "Account": hex::encode(HOLDER), "MPTokenIssuanceID": hex::encode_upper(share_id()),
                "MPTAmount": amount.to_string(), "Flags": 0, "OwnerNode": "0",
            });
            st.state_map.insert(keylet::mptoken_key(&issuance_key, &HOLDER), serde_json::to_vec(&t).unwrap()).unwrap();
        };
        assert!(!is_sole_shareholder(&Sandbox::new(&st), &HOLDER, &issuance), "no MPToken");
        token(&mut st, 999);
        assert!(!is_sole_shareholder(&Sandbox::new(&st), &HOLDER, &issuance));
        token(&mut st, 1000);
        assert!(is_sole_shareholder(&Sandbox::new(&st), &HOLDER, &issuance));
        assert!(!is_sole_shareholder(&Sandbox::new(&st), &ISSUER, &issuance));
        let (_, empty) = vault(HeldAsset::Xrp, 0, "0", "0", 0);
        assert!(!is_sole_shareholder(&Sandbox::new(&st), &HOLDER, &empty), "nothing outstanding");
    }

    /// VaultHelpers.cpp:308-327.
    #[test]
    fn check_vault_domain_asks_the_share_issuances_domain() {
        let mut st = state(1000);
        let put = |st: &mut LedgerState, key: Hash256, v: &Value| {
            st.state_map.insert(key, serde_json::to_vec(v).unwrap()).unwrap();
        };
        let domain = Hash256([0xD0; 32]);
        let kind = "4B5943";
        put(&mut st, domain, &json!({
            "LedgerEntryType": "PermissionedDomain", "Owner": hex::encode(ISSUER), "Flags": 0, "Sequence": 3, "OwnerNode": "0",
            "AcceptedCredentials": [{"Credential": {"Issuer": hex::encode(ISSUER), "CredentialType": kind}}],
        }));
        let credential = |st: &mut LedgerState, expiration: Option<u32>| {
            let mut c = json!({
                "LedgerEntryType": "Credential", "Subject": hex::encode(HOLDER), "Issuer": hex::encode(ISSUER), "CredentialType": kind,
                "Flags": 0x0001_0000, "IssuerNode": "0", "SubjectNode": "0",
            });
            if let Some(exp) = expiration {
                c["Expiration"] = json!(exp);
            }
            let key = crate::tx::credential::credential_key(&HOLDER, &ISSUER, &hex::decode(kind).unwrap());
            st.state_map.insert(key, serde_json::to_vec(&c).unwrap()).unwrap();
        };
        let private = json!({"LedgerEntryType": "MPTokenIssuance", "Issuer": hex::encode(PSEUDO), "DomainID": hex::encode_upper(domain.0)});
        let public = json!({"LedgerEntryType": "MPTokenIssuance", "Issuer": hex::encode(PSEUDO)});
        let dangling = json!({"LedgerEntryType": "MPTokenIssuance", "Issuer": hex::encode(PSEUDO), "DomainID": "EE".repeat(32)});
        let (no, yes) = (SuppressExpired::No, SuppressExpired::Yes);
        let verdict = |st: &LedgerState, issuance: &Value, suppress| check_vault_domain(&Sandbox::new(st), issuance, &HOLDER, suppress);
        assert_eq!(verdict(&st, &public, no), TxResult::NoAuth, "no domain on the issuance");
        assert_eq!(verdict(&st, &dangling, no), TxResult::ObjectNotFound);
        assert_eq!(verdict(&st, &private, no), TxResult::NoAuth, "no credential");
        credential(&mut st, None);
        assert_eq!(verdict(&st, &private, no), TxResult::Success);
        credential(&mut st, Some(999)); // the parent closed at 1000
        assert_eq!(verdict(&st, &private, no), TxResult::Expired);
        assert_eq!(verdict(&st, &private, yes), TxResult::Success, "preclaim lets an expired credential through");
        assert_eq!(verdict(&st, &public, yes), TxResult::NoAuth);
    }
}
