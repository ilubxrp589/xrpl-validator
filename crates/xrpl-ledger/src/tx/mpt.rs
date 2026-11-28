//! Multi-Purpose Token (MPT) primitives — the MPTokensV1 subset the
//! differential harness needs: direct MPT Payments and MPT Clawback.
//!
//! Ground truth (3.2.1 vendored sources):
//! - Payment.cpp:516-590 — the direct MPT arm (V1: MPT payments NEVER route
//!   through the flow engine; `ripple` requires `!isDstMPT || mpTokensV2`).
//! - TokenHelpers.cpp:1060-1204 — directSendNoFeeMPT / directSendNoLimitMPT.
//! - MPTokenHelpers.cpp — requireAuth (Legacy), canTransfer, isAnyFrozen,
//!   transferRate.
//! - ledger_entries.macro — sfOutstandingAmount is SoeRequired (a zero stays
//!   written as "0"); sfMPTAmount is SoeDefault (a zero is OMITTED).
//!
//! Out of scope, stated: MPTokensV2 (flow-engine MPT strands, overflow
//! waivers), DynamicMPT's capability mutations, ConfidentialTransfer. Each is
//! amendment-gated territory no specimen exercises; the entry points note
//! them.
//!
//! SingleAssetVault's gates on these helpers (rippled 3.4.1) are ported: a
//! vault share — an MPT whose issuer is a vault's pseudo-account — inherits
//! the freeze, the authorization and the transferability of the vault's
//! underlying asset (`any_frozen`, `require_auth_at`, `can_transfer_ter`),
//! and `sfDomainID` on an issuance authorizes by credential. No vault exists
//! before the amendment, so each is unit-tested against objects seated in
//! their ledger_entries.macro JSON form, not against a ledger specimen.

use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{TxFields, TxResult};
use xrpl_core::types::Hash256;

pub const LSF_MPT_LOCKED: u64 = 0x0000_0001;
pub const LSF_MPT_CAN_TRANSFER: u64 = 0x0000_0020;
pub const LSF_MPT_CAN_CLAWBACK: u64 = 0x0000_0040;
pub const LSF_MPT_REQUIRE_AUTH: u64 = 0x0000_0004;
pub const LSF_MPT_AUTHORIZED: u64 = 0x0000_0002;
/// kMaxMpTokenAmount — MPT values are signed-64 positive.
pub const MAX_MPT_AMOUNT: u64 = 0x7FFF_FFFF_FFFF_FFFF;

/// Parse an MPT amount JSON: `{"mpt_issuance_id": <48 hex>, "value": <dec>}`.
/// Returns the 24-byte MPTID and the value. Values arrive as decimal strings
/// (canonical) — a bare number is tolerated on read.
pub fn parse_mpt_amount(v: &serde_json::Value) -> Option<([u8; 24], u64)> {
    let id_hex = v.get("mpt_issuance_id").and_then(|x| x.as_str())?;
    let bytes = hex::decode(id_hex).ok()?;
    let id: [u8; 24] = bytes.as_slice().try_into().ok()?;
    let val = match v.get("value")? {
        serde_json::Value::String(s) => s.parse::<u64>().ok()?,
        serde_json::Value::Number(n) => n.as_u64()?,
        _ => return None,
    };
    Some((id, val))
}

pub(crate) fn json_at(sandbox: &Sandbox, key: &Hash256) -> Option<serde_json::Value> {
    sandbox.read(key).and_then(|d| serde_json::from_slice(&d).ok())
}

/// Read a u64 ledger field serialized as a DECIMAL string ("281380138") —
/// the MPT amount family — treating an absent field as zero (SoeDefault).
pub(crate) fn dec_field(obj: &serde_json::Value, f: &str) -> u64 {
    match obj.get(f) {
        Some(serde_json::Value::String(s)) => s.parse::<u64>().unwrap_or(0),
        Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(0),
        _ => 0,
    }
}

/// Write a decimal-string u64 field honoring its Soe class: `required` fields
/// always carry the value; default fields are REMOVED at zero (rippled omits
/// a defaulted STUInt64 from the serialization, so it never reaches JSON).
pub(crate) fn set_dec_field(obj: &mut serde_json::Value, f: &str, v: u64, required: bool) {
    if v == 0 && !required {
        if let Some(o) = obj.as_object_mut() {
            o.remove(f);
        }
        return;
    }
    obj[f] = serde_json::Value::String(v.to_string());
}

/// The issuance's Issuer account, decoded from either address form.
pub(crate) fn issuance_issuer(issuance: &serde_json::Value) -> Option<[u8; 20]> {
    issuance.get("Issuer").and_then(|v| v.as_str()).and_then(crate::tx::offer::decode20)
}

/// rippled `AuthType` (TokenHelpers.h): how strict `requireAuth` is about an
/// account that holds no MPToken (or trust line) yet. Legacy is the default
/// argument (MPTokenHelpers.h:148).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AuthType {
    Legacy,
    StrongAuth,
    WeakAuth,
}

/// rippled `kMaxAssetCheckDepth` (Protocol.h:376): how far the freeze, auth
/// and transfer checks follow a share to its vault's underlying asset.
pub const MAX_ASSET_CHECK_DEPTH: u8 = 5;

/// An `Asset` as a Vault's `sfAsset` or a share's reference holding names it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeldAsset {
    Xrp,
    Iou { currency: [u8; 20], issuer: [u8; 20] },
    Mpt([u8; 24]),
}

/// An STIssue in JSON: `{"mpt_issuance_id": …}`, `{"currency": "XRP"}` or
/// `{"currency": …, "issuer": …}`.
pub(crate) fn parse_asset(v: &serde_json::Value) -> Option<HeldAsset> {
    if let Some(id) = v.get("mpt_issuance_id").and_then(|x| x.as_str()) {
        return Some(HeldAsset::Mpt(hex::decode(id).ok()?.as_slice().try_into().ok()?));
    }
    let currency = crate::tx::offer::amount_currency20(v)?;
    match v.get("issuer") {
        None => (currency == [0u8; 20]).then_some(HeldAsset::Xrp),
        Some(_) => Some(HeldAsset::Iou { currency, issuer: crate::tx::offer::amount_issuer20(v)? }),
    }
}

/// A UInt256 field (hex) as a ledger key — `sfVaultID`, `sfReferenceHolding`,
/// `sfDomainID`: `keylet::vault(id)`, `keylet::unchecked(id)` and
/// `keylet::permissionedDomain(id)` are the id itself.
pub(crate) fn key_field(obj: &serde_json::Value, field: &str) -> Option<Hash256> {
    let bytes = hex::decode(obj.get(field)?.as_str()?).ok()?;
    Some(Hash256(bytes.as_slice().try_into().ok()?))
}

/// `assetOfHolding(sleShareIssuance, sleHolding)` (MPTokenHelpers.cpp:580-601):
/// the asset a vault pseudo-account's holding is of — an MPToken's issuance,
/// or for a trust line its currency with the limit issuer that is not the
/// share's issuer (the pseudo-account).
fn asset_of_holding(share_issuance: &serde_json::Value, holding: &serde_json::Value) -> Option<HeldAsset> {
    if holding.get("LedgerEntryType").and_then(|t| t.as_str()) == Some("MPToken") {
        let id = hex::decode(holding.get("MPTokenIssuanceID")?.as_str()?).ok()?;
        return Some(HeldAsset::Mpt(id.as_slice().try_into().ok()?));
    }
    let pseudo = issuance_issuer(share_issuance)?;
    let low = holding.get("LowLimit")?;
    let low_issuer = crate::tx::offer::amount_issuer20(low)?;
    let issuer = if low_issuer != pseudo {
        low_issuer
    } else {
        crate::tx::offer::amount_issuer20(holding.get("HighLimit")?)?
    };
    Some(HeldAsset::Iou { currency: crate::tx::offer::amount_currency20(low)?, issuer })
}

/// `requireAuth(view, mptIssue, account, authType, depth)` by id
/// (MPTokenHelpers.cpp:374-396): tecOBJECT_NOT_FOUND without the issuance.
pub(crate) fn require_auth_id(sandbox: &Sandbox, mptid: &[u8; 24], account: &[u8; 20], auth: AuthType, depth: u8) -> TxResult {
    let issuance_key = keylet::mpt_issuance_key(mptid);
    match json_at(sandbox, &issuance_key) {
        Some(issuance) => require_auth_at(sandbox, &issuance_key, &issuance, account, auth, depth),
        None => TxResult::ObjectNotFound,
    }
}

/// `requireAuth(view, mptIssue, account, authType, depth)`
/// (MPTokenHelpers.cpp:374-477), in rippled's order:
/// - the issuer is always authorized (:399-400);
/// - with fixCleanup3_3_0, a pseudo-account is exempt here (:402-404) — the
///   exemption exists under SingleAssetVault or MPTokensV2 (:388-389);
/// - under SingleAssetVault, when the issuer is a vault pseudo-account
///   (its root carries sfVaultID) authorization is first required on the
///   vault's underlying asset with the same AuthType, recursively to depth
///   five; a missing issuer root or vault is tefINTERNAL (:406-437);
/// - no MPToken fails a Strong or Legacy check (:439-444);
/// - an issuance with sfDomainID asks `credentials::validDomain`: success
///   authorizes (MPToken or not); a failure is returned when there is no
///   MPToken and ignored when there is one (:446-466). Not amendment-gated:
///   the field can only be written once G12/G13 allow it;
/// - without fixCleanup3_3_0, the pseudo-account exemption comes here (:468-470);
/// - lsfMPTRequireAuth wants an lsfMPTAuthorized MPToken (:472-475).
pub(crate) fn require_auth_at(
    sandbox: &Sandbox,
    issuance_key: &Hash256,
    issuance: &serde_json::Value,
    account: &[u8; 20],
    auth: AuthType,
    depth: u8,
) -> TxResult {
    use crate::ledger::amendments::{enabled, fix_cleanup_3_3_0, MPTOKENS_V2, SINGLE_ASSET_VAULT};
    let Some(issuer) = issuance_issuer(issuance) else { return TxResult::Success };
    if issuer == *account {
        return TxResult::Success;
    }
    let sav = enabled(sandbox, SINGLE_ASSET_VAULT);
    let fix330 = fix_cleanup_3_3_0(sandbox);
    let pseudo_exempt =
        || (sav || enabled(sandbox, MPTOKENS_V2)) && crate::tx::misc::is_pseudo_account(sandbox, account);
    if fix330 && pseudo_exempt() {
        return TxResult::Success;
    }
    if sav {
        if depth >= MAX_ASSET_CHECK_DEPTH {
            return TxResult::TecInternal;
        }
        let Some(issuer_root) = json_at(sandbox, &keylet::account_root_key(&issuer)) else {
            return TxResult::TefInternal;
        };
        if issuer_root.get("VaultID").is_some() {
            let vault = key_field(&issuer_root, "VaultID").and_then(|k| json_at(sandbox, &k));
            // sfAsset is SoeRequired on a Vault: one we cannot read is no vault.
            let Some(asset) = vault.as_ref().and_then(|v| v.get("Asset")).and_then(parse_asset) else {
                return TxResult::TefInternal;
            };
            let underlying = match asset {
                HeldAsset::Xrp => TxResult::Success,
                // RippleStateHelpers.cpp:565-598; only a Strong check wants
                // the line to exist. An issuer root we cannot read asks nothing.
                HeldAsset::Iou { currency, issuer } => crate::tx::offer::require_auth_ter(
                    sandbox,
                    &crate::tx::offer::Leg { xrp: false, cur: currency, issuer },
                    account,
                    auth == AuthType::StrongAuth,
                )
                .unwrap_or(TxResult::Success),
                HeldAsset::Mpt(id) => require_auth_id(sandbox, &id, account, auth, depth + 1),
            };
            if underlying != TxResult::Success {
                return underlying;
            }
        }
    }
    let token = json_at(sandbox, &keylet::mptoken_key(issuance_key, account));
    if token.is_none() && auth != AuthType::WeakAuth {
        return TxResult::NoAuth;
    }
    if let Some(domain) = key_field(issuance, "DomainID") {
        let ter = crate::tx::misc::valid_domain(sandbox, &domain, account);
        if ter == TxResult::Success || token.is_none() {
            return ter;
        }
    }
    if !fix330 && pseudo_exempt() {
        return TxResult::Success;
    }
    let iflags = issuance.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
    let authorized =
        token.is_some_and(|t| t.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0) & LSF_MPT_AUTHORIZED != 0);
    if iflags & LSF_MPT_REQUIRE_AUTH != 0 && !authorized {
        return TxResult::NoAuth;
    }
    TxResult::Success
}

/// `requireAuth(view, mptIssue, account)` with the default Legacy type: None
/// when authorized, else the refusal.
pub fn require_auth(
    sandbox: &Sandbox,
    issuance_key: &Hash256,
    issuance: &serde_json::Value,
    account: &[u8; 20],
) -> Option<TxResult> {
    let r = require_auth_at(sandbox, issuance_key, issuance, account, AuthType::Legacy, 0);
    (r != TxResult::Success).then_some(r)
}

/// `isAnyFrozen(view, accounts, mptIssue)` (MPTokenHelpers.cpp:113-140):
/// the issuance's lsfMPTLocked, any party's MPToken lsfMPTLocked, or — under
/// SingleAssetVault, for a vault share — the vault's underlying asset frozen
/// for the share's issuer or for a party (`vault_pseudo_frozen`). With one
/// account it is `isFrozen(view, account, issuance)` (:76-112).
pub(crate) fn any_frozen(
    sandbox: &Sandbox,
    issuance_key: &Hash256,
    issuance: &serde_json::Value,
    accounts: &[&[u8; 20]],
) -> bool {
    any_frozen_at(sandbox, issuance_key, Some(issuance), accounts, 0)
}

/// [`any_frozen`] for an issuance that may be missing — `isFrozen(view,
/// account, mptIssue, depth = 0)` (MPTokenHelpers.cpp:74-83): without the
/// issuance only an account's own MPToken lock, or the vault preconditions
/// (View.cpp:157-171), can freeze.
pub(crate) fn any_frozen_opt(
    sandbox: &Sandbox,
    issuance_key: &Hash256,
    issuance: Option<&serde_json::Value>,
    accounts: &[&[u8; 20]],
) -> bool {
    any_frozen_at(sandbox, issuance_key, issuance, accounts, 0)
}

fn any_frozen_at(
    sandbox: &Sandbox,
    issuance_key: &Hash256,
    issuance: Option<&serde_json::Value>,
    accounts: &[&[u8; 20]],
    depth: u8,
) -> bool {
    let locked = |obj: &serde_json::Value| obj.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0) & LSF_MPT_LOCKED != 0;
    if issuance.is_some_and(locked) {
        return true;
    }
    if accounts.iter().any(|a| json_at(sandbox, &keylet::mptoken_key(issuance_key, a)).is_some_and(|t| locked(&t))) {
        return true;
    }
    match issuance {
        Some(issuance) => accounts.iter().any(|a| vault_pseudo_frozen(sandbox, a, issuance, depth)),
        // View.cpp:157-171: the preconditions still speak for a missing
        // issuance; past them it is not frozen.
        None => !accounts.is_empty() && vault_frozen_preconditions(sandbox, depth) == Some(true),
    }
}

/// `checkVaultPseudoAccountFrozenPreconditions` (View.cpp:66-82): not frozen
/// without SingleAssetVault; frozen at the depth bound; else undecided.
fn vault_frozen_preconditions(sandbox: &Sandbox, depth: u8) -> Option<bool> {
    use crate::ledger::amendments::{enabled, SINGLE_ASSET_VAULT};
    if !enabled(sandbox, SINGLE_ASSET_VAULT) {
        return Some(false);
    }
    if depth >= MAX_ASSET_CHECK_DEPTH {
        return Some(true);
    }
    None
}

/// `isVaultPseudoAccountFrozen(view, account, issuanceSle, depth)`
/// (View.cpp:84-154): a share is frozen for `account` when the vault's
/// underlying asset is frozen for the share's issuer or for `account`. The
/// underlying comes from the share's sfReferenceHolding when it has one
/// (vaults created under fixCleanup3_2_0; a dangling reference is not
/// frozen), else from the issuer's sfVaultID → the Vault's sfAsset; an issuer
/// that is not a vault pseudo-account — the common case — is not frozen.
fn vault_pseudo_frozen(sandbox: &Sandbox, account: &[u8; 20], issuance: &serde_json::Value, depth: u8) -> bool {
    if let Some(decided) = vault_frozen_preconditions(sandbox, depth) {
        return decided;
    }
    let Some(issuer) = issuance_issuer(issuance) else { return false };
    let asset = if issuance.get("ReferenceHolding").is_some() {
        key_field(issuance, "ReferenceHolding")
            .and_then(|k| json_at(sandbox, &k))
            .and_then(|holding| asset_of_holding(issuance, &holding))
    } else {
        json_at(sandbox, &keylet::account_root_key(&issuer))
            .and_then(|root| key_field(&root, "VaultID"))
            .and_then(|k| json_at(sandbox, &k))
            .and_then(|vault| vault.get("Asset").and_then(parse_asset))
    };
    match asset {
        Some(asset) => asset_any_frozen(sandbox, &[&issuer, account], asset, depth + 1),
        None => false,
    }
}

/// `isVaultPseudoAccountFrozen(view, account, mptIssue, 0)` by id: false without the issuance
/// past the preconditions (View.cpp:157-171).
pub(crate) fn vault_pseudo_account_frozen(sandbox: &Sandbox, account: &[u8; 20], mptid: &[u8; 24]) -> bool {
    match json_at(sandbox, &keylet::mpt_issuance_key(mptid)) {
        Some(issuance) => vault_pseudo_frozen(sandbox, account, &issuance, 0),
        None => vault_frozen_preconditions(sandbox, 0) == Some(true),
    }
}

/// `isAnyFrozen(view, accounts, asset, depth)` (TokenHelpers.cpp:103-125):
/// XRP never; an IOU by the issuer's global freeze or the issuer's side of
/// each account's line (RippleStateHelpers.cpp:126-146); an MPT as above.
fn asset_any_frozen(sandbox: &Sandbox, accounts: &[&[u8; 20]], asset: HeldAsset, depth: u8) -> bool {
    match asset {
        HeldAsset::Xrp => false,
        HeldAsset::Iou { currency, issuer } => {
            let asset = crate::flow::steps::Asset { currency, issuer: Some(issuer) };
            accounts.iter().any(|a| crate::flow::view::is_frozen(sandbox, a, &asset))
        }
        HeldAsset::Mpt(id) => {
            let key = keylet::mpt_issuance_key(&id);
            any_frozen_at(sandbox, &key, json_at(sandbox, &key).as_ref(), accounts, depth)
        }
    }
}

pub const LSF_MPT_CAN_ESCROW: u64 = 0x0000_0008;

/// `requireAuth(view, mptIssue, account, AuthType::WeakAuth)` (View.cpp):
/// the issuer is always authorized; a WEAK check lets an account WITHOUT an
/// MPToken through unless the issuance carries lsfMPTRequireAuth, in which
/// case a missing or unauthorized token is tecNO_AUTH — plus the vault and
/// DomainID rules of `require_auth_at`.
pub(crate) fn require_auth_weak(
    sandbox: &Sandbox,
    issuance_key: &Hash256,
    issuance: &serde_json::Value,
    account: &[u8; 20],
) -> Option<TxResult> {
    let r = require_auth_at(sandbox, issuance_key, issuance, account, AuthType::WeakAuth, 0);
    (r != TxResult::Success).then_some(r)
}

/// `canTransfer(view, mptIssue, from, to)` (MPTokenHelpers.cpp:603-656):
/// issuer endpoints always may; third parties need lsfMPTCanTransfer
/// (tecNO_AUTH); and under fixCleanup3_2_0 a share carrying
/// sfReferenceHolding inherits the transferability of the vault's underlying
/// asset — tefINTERNAL on a dangling reference, tecINTERNAL past depth five.
/// (`WaiveMPTCanTransfer::Yes` has no caller among the ported transactors.)
pub(crate) fn can_transfer_ter(
    sandbox: &Sandbox,
    issuance: &serde_json::Value,
    from: &[u8; 20],
    to: &[u8; 20],
) -> TxResult {
    can_transfer_at(sandbox, issuance, from, to, 0)
}

fn can_transfer_at(sandbox: &Sandbox, issuance: &serde_json::Value, from: &[u8; 20], to: &[u8; 20], depth: u8) -> TxResult {
    let Some(issuer) = issuance_issuer(issuance) else { return TxResult::NoAuth };
    if *from == issuer || *to == issuer {
        return TxResult::Success;
    }
    if issuance.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0) & LSF_MPT_CAN_TRANSFER == 0 {
        return TxResult::NoAuth;
    }
    if issuance.get("ReferenceHolding").is_some() && crate::ledger::amendments::fix_cleanup_3_2_0(sandbox) {
        if depth >= MAX_ASSET_CHECK_DEPTH {
            return TxResult::TecInternal;
        }
        let Some(holding) = key_field(issuance, "ReferenceHolding").and_then(|k| json_at(sandbox, &k)) else {
            return TxResult::TefInternal;
        };
        return match asset_of_holding(issuance, &holding) {
            Some(HeldAsset::Mpt(id)) => match json_at(sandbox, &keylet::mpt_issuance_key(&id)) {
                Some(underlying) => can_transfer_at(sandbox, &underlying, from, to, depth + 1),
                None => TxResult::ObjectNotFound,
            },
            Some(HeldAsset::Iou { currency, issuer }) => iou_can_transfer(sandbox, &currency, &issuer, from, to),
            // A holding is an MPToken or a RippleState (assetOfHolding's assert).
            Some(HeldAsset::Xrp) => TxResult::Success,
            None => TxResult::TefInternal,
        };
    }
    TxResult::Success
}

/// `canTransfer(view, issue, from, to)` (RippleStateHelpers.cpp:603-631): an
/// IOU moves between two non-issuers unless rippling is off on BOTH their
/// lines with the issuer (the issuer's side's NoRipple; without a line, the
/// issuer's lsfDefaultRipple stands in) — terNO_RIPPLE. tefINTERNAL without
/// the issuer's root.
pub(crate) fn iou_can_transfer(sandbox: &Sandbox, currency: &[u8; 20], issuer: &[u8; 20], from: &[u8; 20], to: &[u8; 20]) -> TxResult {
    const LSF_DEFAULT_RIPPLE: u64 = 0x0080_0000;
    const LSF_LOW_NO_RIPPLE: u64 = 0x0010_0000;
    const LSF_HIGH_NO_RIPPLE: u64 = 0x0020_0000;
    if issuer == from || issuer == to {
        return TxResult::Success;
    }
    let Some(issuer_root) = json_at(sandbox, &keylet::account_root_key(issuer)) else {
        return TxResult::TefInternal;
    };
    let default_ripple = issuer_root.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0) & LSF_DEFAULT_RIPPLE != 0;
    let ripple_disabled = |account: &[u8; 20]| match json_at(sandbox, &keylet::ripple_state_key(account, issuer, currency)) {
        Some(line) => {
            let bit = if issuer > account { LSF_HIGH_NO_RIPPLE } else { LSF_LOW_NO_RIPPLE };
            line.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0) & bit != 0
        }
        None => !default_ripple,
    };
    if ripple_disabled(from) && ripple_disabled(to) {
        return TxResult::NoRipple;
    }
    TxResult::Success
}

/// `accountHolds(view, account, mptIssue, zeroIfFrozen, zeroIfUnauthorized)`
/// (TokenHelpers.cpp:409-476) — the holder's MPTAmount; zero without an
/// MPToken, zero when frozen (`isFrozen(view, account, sleMpt)`, vault
/// recursion included) under ZeroIfFrozen, and under ZeroIfUnauthorized zero
/// when unauthorized: by the full `requireAuth(StrongAuth)` once
/// SingleAssetVault or ConfidentialTransfer is enabled (:452-459), before
/// that by the issuance's lsfMPTRequireAuth against the token's
/// lsfMPTAuthorized alone (:460-468).
///
/// Not ported: the issuer's own FullBalance arm (:423-438) and MPTokensV2's
/// balance hook (:472-474). No transactor this engine dispatches calls this
/// with ZeroIfUnauthorized on an MPT — in rippled those callers (OfferCreate,
/// the AMM family, CheckCash, the offer stream) only see an MPT under
/// MPTokensV2; the vault and loan transactors are its first users.
pub fn account_holds(
    sandbox: &Sandbox,
    mptid: &[u8; 24],
    account: &[u8; 20],
    zero_if_frozen: bool,
    zero_if_unauthorized: bool,
) -> u64 {
    use crate::ledger::amendments::{enabled, CONFIDENTIAL_TRANSFER, SINGLE_ASSET_VAULT};
    let ikey = keylet::mpt_issuance_key(mptid);
    let Some(token) = json_at(sandbox, &keylet::mptoken_key(&ikey, account)) else { return 0 };
    let issuance = json_at(sandbox, &ikey);
    let flags = |obj: &serde_json::Value| obj.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
    let iflags = issuance.as_ref().map(flags).unwrap_or(0);
    if zero_if_frozen
        && ((iflags | flags(&token)) & LSF_MPT_LOCKED != 0
            || issuance.as_ref().is_some_and(|i| vault_pseudo_frozen(sandbox, account, i, 0)))
    {
        return 0;
    }
    if zero_if_unauthorized {
        if enabled(sandbox, SINGLE_ASSET_VAULT) || enabled(sandbox, CONFIDENTIAL_TRANSFER) {
            if require_auth_id(sandbox, mptid, account, AuthType::StrongAuth, 0) != TxResult::Success {
                return 0;
            }
        } else if issuance.is_some() && iflags & LSF_MPT_REQUIRE_AUTH != 0 && flags(&token) & LSF_MPT_AUTHORIZED == 0 {
            return 0;
        }
    }
    dec_field(&token, "MPTAmount")
}

/// `transferRate(view, MPTID)` (View.cpp:851): 1e9 + 10,000 × TransferFee
/// when the issuance carries the field, parity otherwise.
pub(crate) fn issuance_transfer_rate(issuance: &serde_json::Value) -> u64 {
    match issuance.get("TransferFee") {
        Some(_) => 1_000_000_000 + 10_000 * dec_field(issuance, "TransferFee"),
        None => 1_000_000_000,
    }
}

/// `MPTokenAuthorize::createMPToken(view, mptID, holder, 0)`: the bare
/// holder object linked into the holder's owner directory. The caller
/// adjusts the holder's OwnerCount (rippled's `adjustOwnerCount` follows).
pub(crate) fn create_mptoken(sandbox: &mut Sandbox, issuance_key: &Hash256, mptid: &[u8; 24], holder: &[u8; 20]) {
    let tkey = keylet::mptoken_key(issuance_key, holder);
    let node = crate::ledger::directory::owner_dir_insert(sandbox, holder, &tkey);
    let obj = serde_json::json!({
        "LedgerEntryType": "MPToken",
        "Account": hex::encode(holder),
        "MPTokenIssuanceID": hex::encode_upper(mptid),
        "Flags": 0,
        "OwnerNode": format!("{node:x}"),
    });
    sandbox.write(tkey, serde_json::to_vec(&obj).unwrap_or_default());
}

/// `rippleLockEscrowMPT` (View.cpp:3674): the holder's MPTAmount drops by
/// the escrowed value and its LockedAmount rises by it; the issuance's
/// LockedAmount rises too, its OutstandingAmount untouched. Finding 332
/// (devnet 5418986 161CF6C1, 100 units of 0052AF9C…: holder 500 → 400 +
/// LockedAmount 100, issuance LockedAmount 100).
pub(crate) fn lock_escrow(sandbox: &mut Sandbox, issuance_key: &Hash256, sender: &[u8; 20], amt: u64) -> TxResult {
    let Some(mut issuance) = json_at(sandbox, issuance_key) else { return TxResult::ObjectNotFound };
    if issuance_issuer(&issuance) == Some(*sender) {
        return TxResult::Malformed; // tecINTERNAL
    }
    let tkey = keylet::mptoken_key(issuance_key, sender);
    let Some(mut token) = json_at(sandbox, &tkey) else { return TxResult::ObjectNotFound };
    let bal = dec_field(&token, "MPTAmount");
    if bal < amt {
        return TxResult::Malformed; // tecINTERNAL — preclaim judged the funds
    }
    set_dec_field(&mut token, "MPTAmount", bal - amt, false);
    let tl = dec_field(&token, "LockedAmount") + amt;
    set_dec_field(&mut token, "LockedAmount", tl, false);
    sandbox.write(tkey, serde_json::to_vec(&token).unwrap_or_default());
    let il = dec_field(&issuance, "LockedAmount") + amt;
    set_dec_field(&mut issuance, "LockedAmount", il, false);
    sandbox.write(*issuance_key, serde_json::to_vec(&issuance).unwrap_or_default());
    TxResult::Success
}

/// `rippleUnlockEscrowMPT` (View.cpp:3771, fixTokenEscrowV1 shape — enabled
/// on mainnet): the issuance's LockedAmount drops by the GROSS (absent at
/// zero); the receiver's MPTAmount rises by the NET — or, receiver being the
/// issuer, OutstandingAmount drops by it; the sender's LockedAmount drops by
/// the gross (absent at zero); the gross−net fee leaves OutstandingAmount.
pub(crate) fn unlock_escrow(
    sandbox: &mut Sandbox,
    issuance_key: &Hash256,
    sender: &[u8; 20],
    receiver: &[u8; 20],
    net: u64,
    gross: u64,
) -> TxResult {
    let Some(mut issuance) = json_at(sandbox, issuance_key) else { return TxResult::ObjectNotFound };
    let Some(issuer) = issuance_issuer(&issuance) else { return TxResult::Malformed };
    if issuer == *sender {
        return TxResult::Malformed; // tecINTERNAL
    }
    let locked = dec_field(&issuance, "LockedAmount");
    if issuance.get("LockedAmount").is_none() || locked < gross {
        return TxResult::Malformed; // tecINTERNAL
    }
    set_dec_field(&mut issuance, "LockedAmount", locked - gross, false);
    if issuer != *receiver {
        let rkey = keylet::mptoken_key(issuance_key, receiver);
        let Some(mut rtok) = json_at(sandbox, &rkey) else { return TxResult::ObjectNotFound };
        let rb = dec_field(&rtok, "MPTAmount") + net;
        set_dec_field(&mut rtok, "MPTAmount", rb, false);
        sandbox.write(rkey, serde_json::to_vec(&rtok).unwrap_or_default());
    } else {
        let outstanding = dec_field(&issuance, "OutstandingAmount");
        if outstanding < net {
            return TxResult::Malformed;
        }
        set_dec_field(&mut issuance, "OutstandingAmount", outstanding - net, true);
    }
    {
        let skey = keylet::mptoken_key(issuance_key, sender);
        let Some(mut stok) = json_at(sandbox, &skey) else { return TxResult::ObjectNotFound };
        let slocked = dec_field(&stok, "LockedAmount");
        if stok.get("LockedAmount").is_none() || slocked < gross {
            return TxResult::Malformed;
        }
        set_dec_field(&mut stok, "LockedAmount", slocked - gross, false);
        sandbox.write(skey, serde_json::to_vec(&stok).unwrap_or_default());
    }
    let diff = gross - net;
    if diff != 0 {
        let outstanding = dec_field(&issuance, "OutstandingAmount");
        if outstanding < diff {
            return TxResult::Malformed;
        }
        set_dec_field(&mut issuance, "OutstandingAmount", outstanding - diff, true);
    }
    sandbox.write(*issuance_key, serde_json::to_vec(&issuance).unwrap_or_default());
    TxResult::Success
}

/// `directSendNoFeeMPT` (TokenHelpers.cpp:1060): the single-leg MPT move.
/// Issuer endpoints move OutstandingAmount on the issuance; holder endpoints
/// move MPTAmount on their MPToken. Auth is NOT checked here — callers did.
pub fn direct_send_no_fee(
    sandbox: &mut Sandbox,
    issuance_key: &Hash256,
    sender: &[u8; 20],
    receiver: &[u8; 20],
    amt: u64,
) -> TxResult {
    let Some(mut issuance) = json_at(sandbox, issuance_key) else {
        return TxResult::ObjectNotFound;
    };
    let Some(issuer) = issuance_issuer(&issuance) else { return TxResult::Malformed };
    let outstanding = dec_field(&issuance, "OutstandingAmount");
    let mut issuance_dirty = false;

    if *sender == issuer {
        // V1 has no overflow check HERE (V2-gated); the mint cap lives in
        // directSendNoLimitMPT's issuer branch, which our caller applies.
        set_dec_field(&mut issuance, "OutstandingAmount", outstanding + amt, true);
        issuance_dirty = true;
    } else {
        let tkey = keylet::mptoken_key(issuance_key, sender);
        let Some(mut token) = json_at(sandbox, &tkey) else { return TxResult::NoAuth };
        let bal = dec_field(&token, "MPTAmount");
        if bal < amt {
            return TxResult::InsufficientFunds;
        }
        set_dec_field(&mut token, "MPTAmount", bal - amt, false);
        sandbox.write(tkey, serde_json::to_vec(&token).unwrap_or_default());
    }

    if *receiver == issuer {
        let out_now = dec_field(&issuance, "OutstandingAmount");
        if out_now < amt {
            return TxResult::Malformed; // tecINTERNAL territory — cannot occur
        }
        set_dec_field(&mut issuance, "OutstandingAmount", out_now - amt, true);
        issuance_dirty = true;
    } else {
        let tkey = keylet::mptoken_key(issuance_key, receiver);
        let Some(mut token) = json_at(sandbox, &tkey) else { return TxResult::NoAuth };
        let bal = dec_field(&token, "MPTAmount");
        set_dec_field(&mut token, "MPTAmount", bal + amt, false);
        sandbox.write(tkey, serde_json::to_vec(&token).unwrap_or_default());
    }

    if issuance_dirty {
        sandbox.write(*issuance_key, serde_json::to_vec(&issuance).unwrap_or_default());
    }
    TxResult::Success
}

/// The direct MPT Payment arm (Payment.cpp:516-590, MPTokensV1). Assumes the
/// caller already ran the generic destination checks (pseudo-account, dest
/// exists, dest tag). Returns the final TxResult including rippled's
/// tail mapping of tecINSUFFICIENT_FUNDS / tecPATH_DRY → tecPATH_PARTIAL.
pub fn apply_mpt_payment(
    tx: &TxFields,
    sandbox: &mut Sandbox,
    dest: &[u8; 20],
    mptid: [u8; 24],
    value: u64,
    partial: bool,
) -> TxResult {
    let issuance_key = keylet::mpt_issuance_key(&mptid);
    let Some(issuance) = json_at(sandbox, &issuance_key) else {
        return TxResult::ObjectNotFound; // requireAuth's missing-issuance code
    };
    if let Some(r) = require_auth(sandbox, &issuance_key, &issuance, &tx.account) {
        return r;
    }
    if let Some(r) = require_auth(sandbox, &issuance_key, &issuance, dest) {
        return r;
    }
    let Some(issuer) = issuance_issuer(&issuance) else { return TxResult::Malformed };

    // canTransfer (Payment.cpp:690-692): issuer endpoints always may; third
    // parties need lsfMPTCanTransfer, and a vault share its underlying's.
    let transferable = can_transfer_ter(sandbox, &issuance, &tx.account, dest);
    if transferable != TxResult::Success {
        return transferable;
    }

    // verifyDepositPreauth — unconditional on this arm (Payment.cpp:693).
    // Finding 375 (campaign 20 M-17 / X-3, testnet E33A9793F637,
    // D25509AD2D3D): the WHOLE of it — expired-credential cleanup
    // (tecEXPIRED, deletions kept) and the credential-set authorization —
    // not only the by-account preauth.
    if let Some(dst_root) = json_at(sandbox, &keylet::account_root_key(dest)) {
        let r = crate::tx::misc::verify_deposit_preauth(sandbox, tx, &tx.account, dest, &dst_root);
        if r != TxResult::Success {
            return r;
        }
    }

    // Transfer rate applies only between holders; the same branch owns the
    // lock check (issuer moves stay allowed while locked).
    let mut rate: u64 = 1_000_000_000;
    if tx.account != issuer && *dest != issuer {
        if any_frozen(sandbox, &issuance_key, &issuance, &[&tx.account, dest]) {
            return TxResult::Locked;
        }
        let fee = dec_field(&issuance, "TransferFee");
        rate = 1_000_000_000 + 10_000 * fee;
    }

    // maxSourceAmount: SendMax's value when present (preflight pinned it to
    // the same MPT), else the delivery amount itself (Payment.cpp:73).
    let max_source = tx
        .fields
        .get("SendMax")
        .and_then(parse_mpt_amount)
        .map(|(_, v)| v)
        .unwrap_or(value);

    // Finding 370 (campaign 18, 10 testnet specimens): "No rounding" in the
    // source means no rounding MODE argument — not exact arithmetic. The cost
    // is `multiply(dstAmount, rate)`: a Number product (16-digit mantissa,
    // to nearest) cast to an integer (to nearest, ties even); a partial
    // delivery is `divide(SendMax, rate)` (muldiv + 5, then to nearest); and
    // the sender is debited `multiply(amountDeliver, rate)` recomputed in
    // accountSend — which may exceed SendMax (3-19). We floored all three and
    // debited `required`.
    //
    // 3.4.1 Payment.cpp:720, :729: either one THROWS where its result does
    // not fit — a cost past int64 (`STAmount{asset, Number}`'s
    // `operator rep()`), a divide whose `muldiv` passes 64 bits — and the
    // throw is tefEXCEPTION (applySteps.cpp:485-488), nothing applied. We
    // answered tecPATH_PARTIAL (3.5.0-rc1 MPToken_test.cpp:7918-8059 pins
    // both without fixCleanup3_5_0).
    //
    // fixCleanup3_5_0 (3.5.0-rc1 Payment.cpp:722-749): integer arithmetic.
    // The cost is `tryMulRatio(dstAmount, rate, QUALITY_ONE, roundUp)` — one
    // past int64 exceeds any SendMax — and a partial delivery
    // `mulRatio(SendMax, QUALITY_ONE, rate, roundDown)`; one of zero is
    // tecPATH_PARTIAL. The debit is accountSend's `addTransferFee` below.
    let fix_cleanup_3_5_0 = crate::ledger::amendments::fix_cleanup_3_5_0(sandbox);
    let mut deliver = value;
    let exceeds_send_max = if fix_cleanup_3_5_0 {
        let required = mpt_mul_ratio(value, rate, 1_000_000_000, true);
        let mut exceeds = required.is_none_or(|r| max_source < r);
        if partial && exceeds {
            // A rate is at least parity, so this never passes int64.
            let Some(delivered) = mpt_mul_ratio(max_source, 1_000_000_000, rate, false) else { return TxResult::TefException };
            if delivered == 0 {
                return TxResult::PathPartial;
            }
            deliver = delivered;
            exceeds = false;
        }
        exceeds
    } else {
        let Some(mut required) = mpt_multiply_rate(value, rate) else { return TxResult::TefException };
        if partial && required > max_source {
            required = max_source;
            let Some(d) = mpt_divide_rate(max_source, rate) else { return TxResult::TefException };
            deliver = d;
        }
        required > max_source
    };
    let deliver_min = tx.fields.get("DeliverMin").and_then(parse_mpt_amount).map(|(_, v)| v);
    if exceeds_send_max || deliver_min.is_some_and(|m| deliver < m) {
        return TxResult::PathPartial;
    }

    // accountSend → directSendNoLimitMPT: issuer-adjacent moves are one leg
    // (with the V1 mint cap); holder→holder transits via the issuer bridge —
    // issuer→receiver the delivery, sender→issuer the cost. Nothing to send
    // is tesSUCCESS with nothing touched (accountSendMPT, 3.4.1
    // TokenHelpers.cpp:1503-1504): 3.4.1's divide can deliver zero when its
    // `muldiv(…) + 5` wraps past 2^64.
    let res = if deliver == 0 {
        TxResult::Success
    } else if tx.account == issuer || *dest == issuer {
        if tx.account == issuer {
            let max = issuance
                .get("MaximumAmount")
                .map(|_| dec_field(&issuance, "MaximumAmount"))
                .unwrap_or(MAX_MPT_AMOUNT);
            let outstanding = dec_field(&issuance, "OutstandingAmount");
            if outstanding.checked_add(deliver).map(|s| s > max).unwrap_or(true) {
                return TxResult::PathPartial; // tecPATH_DRY → mapped below anyway
            }
        }
        direct_send_no_fee(sandbox, &issuance_key, &tx.account, dest, deliver)
    } else {
        let leg = direct_send_no_fee(sandbox, &issuance_key, &issuer, dest, deliver);
        if leg != TxResult::Success {
            leg
        } else {
            // accountSend's own cost (directSendNoLimitMPT): 3.4.1's
            // `multiply(saAmount, rate)` (TokenHelpers.cpp:1347), whose
            // throw is tefEXCEPTION too; under fixCleanup3_5_0
            // `addTransferFee(saAmount, rate)`, rounded up (3.5.0-rc1
            // TokenHelpers.cpp:1363-1373, :555-561) — at most SendMax here.
            let debit = if fix_cleanup_3_5_0 { mpt_mul_ratio(deliver, rate, 1_000_000_000, true) } else { mpt_multiply_rate(deliver, rate) };
            let Some(debit) = debit else { return TxResult::TefException };
            direct_send_no_fee(sandbox, &issuance_key, &tx.account, &issuer, debit)
        }
    };

    // fixMPTDeliveredAmount (Payment.cpp:741-746): a delivery other than Amount (partial, or a transfer fee
    // the SendMax covers) is the metadata's DeliveredAmount.
    if res == TxResult::Success && deliver != value && crate::ledger::amendments::enabled(sandbox, FIX_MPT_DELIVERED_AMOUNT) {
        if let Some(mut amount) = tx.fields.get("Amount").cloned() {
            amount["value"] = serde_json::Value::String(deliver.to_string());
            crate::tx::deliver(amount);
        }
    }
    match res {
        TxResult::InsufficientFunds | TxResult::PathDry => TxResult::PathPartial,
        other => other,
    }
}

/// fixMPTDeliveredAmount (enabled on mainnet).
const FIX_MPT_DELIVERED_AMOUNT: &str = "AB8D932A5F338903FE5BCBD80B611FFED70839ABA3170E9CE01D947C0EDEDCF2";

// ---------------------------------------------------------------------------
// MPT issuance lifecycle — MPTokenIssuanceCreate / Destroy / Set, and
// MPTokenAuthorize. Previously fee-only stubs (probe STUB_TYPES).
// Sources: MPTokenIssuanceCreate.cpp::create, MPTokenIssuanceDestroy.cpp,
// MPTokenIssuanceSet.cpp, MPTokenHelpers.cpp::authorizeMPToken (all 3.2.1).
// ---------------------------------------------------------------------------

use crate::ledger::transactor::Transactor;

fn issuance_id_of(tx: &TxFields) -> Option<[u8; 24]> {
    let s = tx.fields.get("MPTokenIssuanceID")?.as_str()?;
    hex::decode(s).ok()?.as_slice().try_into().ok()
}

/// A transfer `Rate` (1e9 = parity) as rippled's `detail::as_amount`:
/// `STAmount(noIssue(), rate, -9)`, IOU-normalised to a mantissa in
/// [1e15, 1e16).
fn rate_as_iou(rate: u64) -> (u128, i32) {
    let (mut m, mut e) = (rate as u128, -9i32);
    while m < 1_000_000_000_000_000 {
        m *= 10;
        e -= 1;
    }
    while m >= 10_000_000_000_000_000 {
        m /= 10;
        e += 1;
    }
    (m, e)
}

/// rippled `multiply(STAmount mpt, Rate)` (Rate2.cpp:34 → STAmount.cpp
/// multiply → `Number{v1} * Number{v2}` → `STAmount{asset, Number}` =
/// fromNumber's `static_cast<int64>`): the product rounded to Number's
/// mantissa at the thread's scale (sixteen digits, nineteen at Large330),
/// then to an integer, both to nearest (ties even). `fromNumber`
/// (STAmount.h:567-583) converts the magnitude and carries the sign; the
/// operands here are never negative, so `to_drops` is that conversion at
/// either scale. Parity returns the amount untouched.
pub(crate) fn mpt_multiply_rate(v: u64, rate: u64) -> Option<u64> {
    use crate::tx::number::{Number, Rounding::ToNearest};
    if rate == 1_000_000_000 || v == 0 {
        return Some(v);
    }
    let (rm, re) = rate_as_iou(rate);
    let a = Number::from_parts(false, v as u128, 0, ToNearest).ok()?;
    let r = Number::from_parts(false, rm, re, ToNearest).ok()?;
    let p = a.mul(r, ToNearest).ok()?.to_drops(ToNearest).ok()?;
    u64::try_from(p).ok()
}

/// rippled `mulRatio(MPTAmount, num, den, roundUp)` (3.5.0-rc1
/// MPTAmount.h:154-176) for a non-negative amount: `v · num / den` exact in
/// 128 bits, rounded up or down. `None` past int64 — the `std::overflow_error`
/// that `tryMulRatio` (:178-189) turns into `std::nullopt`.
pub(crate) fn mpt_mul_ratio(v: u64, num: u64, den: u64, round_up: bool) -> Option<u64> {
    let product = v as u128 * num as u128;
    let mut r = product / den as u128;
    if round_up && !product.is_multiple_of(den as u128) {
        r += 1;
    }
    (r <= i64::MAX as u128).then_some(r as u64)
}

/// rippled `divide(STAmount mpt, Rate)` (Rate2.cpp → STAmount.cpp divide):
/// the integral numerator scaled up to >= 1e15, `muldiv(num, 1e17, den) + 5`
/// at offset `num − den − 17`, canonicalised through
/// `MPTAmount{Number{…, Unchecked}}` — operator rep, to nearest.
///
/// 3.4.1 STAmount.cpp:1226-1269 in 64 bits: `muldiv` throws past
/// `UINT64_MAX` (:1194-1198), and the `+ 5` is uint64 arithmetic, so a
/// quotient within five of `UINT64_MAX` wraps to a mantissa of 0..4 — zero
/// once canonicalised (3.5.0-rc1 MPToken_test.cpp:7899-7966). Off Number's
/// Small scale the STAmount constructor also throws on a mantissa past
/// int64 (STAmount.h:359-370). `None` is every throw.
pub(crate) fn mpt_divide_rate(v: u64, rate: u64) -> Option<u64> {
    use crate::tx::number::{mantissa_scale, MantissaScale, Number, Rounding::ToNearest};
    if rate == 1_000_000_000 || v == 0 {
        return Some(v);
    }
    let (mut nv, mut no) = (v as u128, 0i32);
    while nv < 1_000_000_000_000_000 {
        nv *= 10;
        no -= 1;
    }
    let (dv, doff) = rate_as_iou(rate);
    let quotient = nv * 100_000_000_000_000_000 / dv;
    if quotient > u64::MAX as u128 {
        return None;
    }
    let q = (quotient as u64).wrapping_add(5);
    if mantissa_scale() != MantissaScale::Small && q > i64::MAX as u64 {
        return None;
    }
    let r = Number::rep_from_raw(false, q as u128, no - doff - 17, ToNearest).ok()?;
    u64::try_from(r).ok()
}

/// rippled `divideRound(mpt, Rate, asset, roundUp = true)` on the Number
/// path (3.5.0-rc1 STAmount.cpp:1670-1685, taken with an MPT operand under
/// MPTokensV2 or fixCleanup3_5_0): `Number{amount} / Number{rate}` under
/// `NumberRoundModeGuard(Upward)` — both operands normalised and the quotient
/// rounded upward at the thread's scale — then `roundNumberResult`
/// (:1479-1513) materialises the MPT amount at Upward too (`operator rep()`).
/// Parity returns the amount (Rate2.cpp:94-95).
pub(crate) fn mpt_divide_round_up_number(v: u64, rate: u64) -> Option<u64> {
    use crate::tx::number::{Number, Rounding::Upward};
    if rate == 1_000_000_000 || v == 0 {
        return Some(v);
    }
    let (rm, re) = rate_as_iou(rate);
    let n = Number::from_parts(false, v as u128, 0, Upward).ok()?;
    let d = Number::from_parts(false, rm, re, Upward).ok()?;
    let q = n.div(d, Upward).ok()?.to_drops(Upward).ok()?;
    u64::try_from(q).ok()
}

/// rippled `divideRound(mpt, Rate, asset, roundUp = true)` on the LEGACY
/// path (divRoundImpl without MPTokensV2): `muldivRound` rounding up, then
/// `canonicalizeRound` for an integral asset — cut to one decimal and add 9
/// (two or more digits dropped) or 10 (one dropped) before the last divide.
/// So a tenths digit of 0 does NOT round up, and an exact quotient still
/// gains one when only one digit is dropped. A zero result is 1.
pub(crate) fn mpt_divide_round_up_legacy(v: u64, rate: u64) -> Option<u64> {
    if rate == 1_000_000_000 || v == 0 {
        return Some(v);
    }
    let (mut nv, mut no) = (v as u128, 0i32);
    while nv < 1_000_000_000_000_000 {
        nv *= 10;
        no -= 1;
    }
    let (dv, doff) = rate_as_iou(rate);
    let mut amount = nv.checked_mul(100_000_000_000_000_000)?.div_ceil(dv);
    let mut offset = no - doff - 17;
    if offset < 0 {
        let mut loops = 0;
        while offset < -1 {
            amount /= 10;
            offset += 1;
            loops += 1;
        }
        amount += if loops >= 2 { 9 } else { 10 };
        amount /= 10;
        offset += 1;
    }
    while offset > 0 {
        amount = amount.checked_mul(10)?;
        offset -= 1;
    }
    u64::try_from(amount.max(1)).ok()
}

fn read_json(sandbox: &Sandbox, k: &Hash256) -> Option<serde_json::Value> {
    sandbox.read(k).and_then(|d| serde_json::from_slice(&d).ok())
}

/// Pre-fee balance (do_apply runs after apply_common): rippled's
/// `priorBalance_` — the same convention CheckCash calibrated.
fn prior_balance(sandbox: &Sandbox, tx: &TxFields) -> u64 {
    read_json(sandbox, &keylet::account_root_key(&tx.account))
        .and_then(|a| a["Balance"].as_str().and_then(|s| s.parse::<u64>().ok()))
        .unwrap_or(0)
        .saturating_add(tx.account_fee())
}

fn owner_count(sandbox: &Sandbox, acct: &[u8; 20]) -> u64 {
    read_json(sandbox, &keylet::account_root_key(acct))
        .and_then(|a| a["OwnerCount"].as_u64())
        .unwrap_or(0)
}

/// A UInt256 in JSON that is all zeros (`beast::kZero`).
fn is_zero_hash(v: &serde_json::Value) -> bool {
    v.as_str().is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b == b'0'))
}

/// `sfDomainID` on an MPT issuance transaction needs PermissionedDomains AND
/// SingleAssetVault (`checkExtraFeatures`: MPTokenIssuanceCreate.cpp:33-36,
/// MPTokenIssuanceSet.cpp:29-32).
fn domain_field_enabled(rules: &Sandbox) -> bool {
    use crate::ledger::amendments::{enabled, PERMISSIONED_DOMAINS, SINGLE_ASSET_VAULT};
    enabled(rules, PERMISSIONED_DOMAINS) && enabled(rules, SINGLE_ASSET_VAULT)
}

/// MPTokenIssuanceCreate — mint a new issuance object under the creator.
pub struct MPTokenIssuanceCreateTransactor;

impl Transactor for MPTokenIssuanceCreateTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "MPTokenIssuanceCreate" {
            return TxResult::Malformed;
        }
        if tx.fee_missing() {
            return TxResult::BadFee;
        }
        // A NON-ZERO TransferFee demands tfMPTCanTransfer; the cap is 50000.
        // A8A08D3E (l105864866) carries an explicit "TransferFee": 0 with no
        // CanTransfer flag and mainnet accepts it — the flag requirement is
        // `fee > 0` only (MPTokenIssuanceCreate.cpp preflight). Further tem
        // checks (metadata length) unported — no specimen; the DomainID rules
        // need the ledger's amendments and live in `preflight_in`.
        let flags = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
        if let Some(f) = tx.fields.get("TransferFee").and_then(|v| v.as_u64()) {
            if f > 50_000 || (f > 0 && flags & LSF_MPT_CAN_TRANSFER == 0) {
                return TxResult::Malformed;
            }
        }
        TxResult::Success
    }

    /// G12 — `checkExtraFeatures` (MPTokenIssuanceCreate.cpp:30-36) runs
    /// before every other preflight step: sfDomainID without both
    /// PermissionedDomains and SingleAssetVault is temDISABLED. With them,
    /// :94-102: the domain must be non-zero and the issuance must carry
    /// tfMPTRequireAuth ("domain present implies not public"), else
    /// temMALFORMED.
    fn preflight_in(&self, tx: &TxFields, rules: &Sandbox) -> TxResult {
        let domain = tx.fields.get("DomainID");
        if domain.is_some() && !domain_field_enabled(rules) {
            return TxResult::Disabled;
        }
        let basic = self.preflight(tx);
        if basic != TxResult::Success {
            return basic;
        }
        if let Some(domain) = domain {
            let flags = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
            if is_zero_hash(domain) || flags & LSF_MPT_REQUIRE_AUTH == 0 {
                return TxResult::Malformed;
            }
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        if !sandbox.exists(&keylet::account_root_key(&tx.account)) {
            return TxResult::NoAccount;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let oc = owner_count(sandbox, &tx.account);
        if prior_balance(sandbox, tx) < crate::ledger::fees::account_reserve(sandbox, oc + 1) {
            return TxResult::InsufficientReserve;
        }
        // makeMptID(seq, account): seq_be32 || account — the tx's seq value
        // (ticket seq when ticketed), same rule as offer/check keylets.
        let seq = if tx.uses_ticket() { tx.ticket_seq.unwrap_or(0) } else { tx.sequence };
        let mut mptid = [0u8; 24];
        mptid[..4].copy_from_slice(&seq.to_be_bytes());
        mptid[4..].copy_from_slice(&tx.account);
        let ikey = keylet::mpt_issuance_key(&mptid);
        let node = crate::ledger::directory::owner_dir_insert(sandbox, &tx.account, &ikey);
        // Finding 374: `args.flags & ~tfUniversal` (MPTokenIssuanceCreate.cpp
        // :158), and tfUniversal is tfFullyCanonicalSig | tfInnerBatchTxn —
        // 0xC0000000. Stripping only 0x80000000 would store 0x40000000 on an
        // issuance created inside a Batch (BatchV1_1 activates ~2026-10-09).
        let flags = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0) & !0xC000_0000;
        let mut obj = serde_json::json!({
            "LedgerEntryType": "MPTokenIssuance",
            "Flags": flags,
            "Issuer": hex::encode(tx.account),
            "OutstandingAmount": "0",
            "OwnerNode": format!("{node:x}"),
            "Sequence": seq,
        });
        // Optional passthroughs, in the ledger's own spellings: MaximumAmount
        // is a UInt64 amount-family field (decimal STRING — the l106134471
        // issuance shows "1000000"); AssetScale/TransferFee are numbers;
        // metadata/domain are hex strings.
        if let Some(m) = tx.fields.get("MaximumAmount") {
            obj["MaximumAmount"] = m.clone();
        }
        // AssetScale/TransferFee/ImmutableFlags are soeDEFAULT on the ledger
        // object: an explicit zero in the tx (A8A08D3E carries both) is
        // serialized AWAY by rippled — the stored object omits the field.
        // UINT32 53 is sfImmutableFlags (sfields.macro:98); "MutableFlags"
        // was the SDK codec's stale name for it, gone from SDK branch
        // validator-k256-vault on, where an object key the codec does not
        // know is skipped at encode without an error.
        for f in ["AssetScale", "TransferFee", "ImmutableFlags"] {
            match tx.fields.get(f).and_then(|v| v.as_u64()) {
                Some(v) if v > 0 => obj[f] = serde_json::Value::Number(v.into()),
                _ => {}
            }
        }
        for f in ["MPTokenMetadata", "DomainID"] {
            match tx.fields.get(f).and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => obj[f] = serde_json::Value::String(s.to_string()),
                _ => {}
            }
        }
        sandbox.write(ikey, serde_json::to_vec(&obj).unwrap_or_default());
        crate::tx::offer::owner_count_add(sandbox, &tx.account, 1);
        TxResult::Success
    }
}

/// MPTokenIssuanceDestroy — the issuer retires an empty issuance.
pub struct MPTokenIssuanceDestroyTransactor;

impl Transactor for MPTokenIssuanceDestroyTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "MPTokenIssuanceDestroy" {
            return TxResult::Malformed;
        }
        if tx.fee_missing() {
            return TxResult::BadFee;
        }
        if tx.fields.get("MPTokenIssuanceID").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        if !sandbox.exists(&keylet::account_root_key(&tx.account)) {
            return TxResult::NoAccount;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let Some(id) = issuance_id_of(tx) else { return TxResult::Malformed };
        let ikey = keylet::mpt_issuance_key(&id);
        let Some(iss) = read_json(sandbox, &ikey) else { return TxResult::ObjectNotFound };
        let issuer_ok = iss
            .get("Issuer")
            .and_then(|v| v.as_str())
            .and_then(crate::tx::offer::decode20)
            .map(|i| i == tx.account)
            .unwrap_or(false);
        if !issuer_ok {
            return TxResult::NoPermission;
        }
        if dec_field(&iss, "OutstandingAmount") != 0 || dec_field(&iss, "LockedAmount") != 0 {
            return TxResult::HasObligations;
        }
        let hint = iss
            .get("OwnerNode")
            .and_then(|v| v.as_str())
            .and_then(|s| u64::from_str_radix(s, 16).ok());
        sandbox.delete(ikey);
        crate::ledger::directory::owner_dir_remove(sandbox, &tx.account, &ikey, hint, false);
        crate::tx::offer::owner_count_add(sandbox, &tx.account, -1);
        TxResult::Success
    }
}

/// MPTokenAuthorize — a holder opts in/out of an MPT (creating or deleting
/// their MPToken), or the issuer flips lsfMPTAuthorized on a holder's.
pub struct MPTokenAuthorizeTransactor;

const TF_MPT_UNAUTHORIZE: u64 = 0x0000_0001;

impl Transactor for MPTokenAuthorizeTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "MPTokenAuthorize" {
            return TxResult::Malformed;
        }
        if tx.fee_missing() {
            return TxResult::BadFee;
        }
        if tx.fields.get("MPTokenIssuanceID").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        if !sandbox.exists(&keylet::account_root_key(&tx.account)) {
            return TxResult::NoAccount;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let Some(id) = issuance_id_of(tx) else { return TxResult::Malformed };
        let ikey = keylet::mpt_issuance_key(&id);
        let flags = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
        let holder = tx
            .fields
            .get("Holder")
            .and_then(|h| h.as_str())
            .and_then(crate::tx::offer::decode20);

        let Some(holder_id) = holder else {
            // HOLDER-side (tx account is the holder).
            let tkey = keylet::mptoken_key(&ikey, &tx.account);
            if flags & TF_MPT_UNAUTHORIZE != 0 {
                // Delete own empty MPToken.
                let Some(tok) = read_json(sandbox, &tkey) else {
                    return TxResult::ObjectNotFound;
                };
                if dec_field(&tok, "MPTAmount") != 0 || dec_field(&tok, "LockedAmount") != 0 {
                    return TxResult::HasObligations;
                }
                // fixCleanup3_4_0 (MPTokenAuthorize.cpp:79-83): a holder cannot
                // drop a LOCKED MPToken while the issuance exists —
                // tecNO_PERMISSION (pre-amendment only under SingleAssetVault,
                // which mainnet lacks).
                if crate::ledger::amendments::fix_cleanup_3_4_0(sandbox)
                    && sandbox.exists(&ikey)
                    && tok.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0) & LSF_MPT_LOCKED != 0
                {
                    return TxResult::NoPermission;
                }
                let hint = tok
                    .get("OwnerNode")
                    .and_then(|v| v.as_str())
                    .and_then(|s| u64::from_str_radix(s, 16).ok());
                sandbox.delete(tkey);
                crate::ledger::directory::owner_dir_remove(
                    sandbox, &tx.account, &tkey, hint, false,
                );
                crate::tx::offer::owner_count_add(sandbox, &tx.account, -1);
                return TxResult::Success;
            }
            // Create own MPToken (opt in).
            let Some(iss) = read_json(sandbox, &ikey) else { return TxResult::ObjectNotFound };
            let is_issuer = iss
                .get("Issuer")
                .and_then(|v| v.as_str())
                .and_then(crate::tx::offer::decode20)
                .map(|i| i == tx.account)
                .unwrap_or(false);
            if is_issuer {
                return TxResult::NoPermission;
            }
            if sandbox.exists(&tkey) {
                return TxResult::Duplicate;
            }
            // Trust-line-style reserve: the first two owned items are free
            // (authorizeMPToken: uOwnerCount < 2 ⇒ zero reserve required).
            let oc = owner_count(sandbox, &tx.account);
            let need = if oc < 2 {
                0
            } else {
                crate::ledger::fees::account_reserve(sandbox, oc + 1)
            };
            if prior_balance(sandbox, tx) < need {
                return TxResult::InsufficientReserve;
            }
            let node = crate::ledger::directory::owner_dir_insert(sandbox, &tx.account, &tkey);
            let obj = serde_json::json!({
                "LedgerEntryType": "MPToken",
                "Account": hex::encode(tx.account),
                "MPTokenIssuanceID": hex::encode_upper(id),
                "Flags": 0,
                "OwnerNode": format!("{node:x}"),
            });
            sandbox.write(tkey, serde_json::to_vec(&obj).unwrap_or_default());
            crate::tx::offer::owner_count_add(sandbox, &tx.account, 1);
            return TxResult::Success;
        };

        // ISSUER-side (allowlisting): flip lsfMPTAuthorized on the holder's
        // MPToken. Requires the issuance to enforce lsfMPTRequireAuth.
        if !sandbox.exists(&keylet::account_root_key(&holder_id)) {
            return TxResult::NoDst;
        }
        let Some(iss) = read_json(sandbox, &ikey) else { return TxResult::ObjectNotFound };
        let is_issuer = iss
            .get("Issuer")
            .and_then(|v| v.as_str())
            .and_then(crate::tx::offer::decode20)
            .map(|i| i == tx.account)
            .unwrap_or(false);
        if !is_issuer {
            return TxResult::NoPermission;
        }
        if iss.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0) & LSF_MPT_REQUIRE_AUTH == 0 {
            return TxResult::NoAuth;
        }
        let tkey = keylet::mptoken_key(&ikey, &holder_id);
        let Some(mut tok) = read_json(sandbox, &tkey) else { return TxResult::ObjectNotFound };
        if let Some(root) = read_json(sandbox, &keylet::account_root_key(&holder_id)) {
            if ["AMMID", "VaultID", "LoanBrokerID"].iter().any(|f| root.get(f).is_some()) {
                return TxResult::NoPermission; // pseudo-accounts are implicitly authorized
            }
        }
        let fin = tok.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
        let fout = if flags & TF_MPT_UNAUTHORIZE != 0 {
            fin & !LSF_MPT_AUTHORIZED
        } else {
            fin | LSF_MPT_AUTHORIZED
        };
        tok["Flags"] = serde_json::Value::Number(fout.into());
        sandbox.write(tkey, serde_json::to_vec(&tok).unwrap_or_default());
        TxResult::Success
    }
}

/// MPTokenIssuanceSet — issuer locks/unlocks the issuance (or one holder's
/// MPToken), sets or clears its DomainID (SingleAssetVault with
/// PermissionedDomains) and adjusts mutable metadata. The DynamicMPT
/// capability-flag mutation arms are unported (no specimen) — lock/unlock,
/// DomainID, TransferFee and MPTokenMetadata updates are.
pub struct MPTokenIssuanceSetTransactor;

const TF_MPT_LOCK: u64 = 0x0000_0001;
const TF_MPT_UNLOCK: u64 = 0x0000_0002;
/// `tfMPTokenIssuanceSetEnableFlagMask` (TxFlags.h:383-385): tfMPTSetCanLock
/// 0x04 … tfMPTSetCanHoldConfidentialBalance 0x100.
const TF_MPT_SET_ENABLE_FLAG_MASK: u64 = 0x0000_01FC;
pub const LSF_MPT_CAN_LOCK: u64 = 0x0000_0002;

impl Transactor for MPTokenIssuanceSetTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "MPTokenIssuanceSet" {
            return TxResult::Malformed;
        }
        if tx.fee_missing() {
            return TxResult::BadFee;
        }
        if tx.fields.get("MPTokenIssuanceID").is_none() {
            return TxResult::Malformed;
        }
        let flags = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
        if flags & TF_MPT_LOCK != 0 && flags & TF_MPT_UNLOCK != 0 {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    /// The amendment-gated part of MPTokenIssuanceSet's preflight, in
    /// rippled's order around the checks above:
    /// - G13, `checkExtraFeatures` (MPTokenIssuanceSet.cpp:27-32), first:
    ///   sfDomainID without both PermissionedDomains and SingleAssetVault is
    ///   temDISABLED;
    /// - :68-69 sfDomainID together with sfHolder is temMALFORMED;
    /// - G14, :82-89: once SingleAssetVault, DynamicMPT or
    ///   ConfidentialTransfer is enabled, a transaction that changes nothing
    ///   — sfFlags zero (the raw word: tfFullyCanonicalSig alone is not
    ///   zero), no sfDomainID, no encryption key, no mutation — is
    ///   temMALFORMED. Before, such a set was a successful no-op (testnet
    ///   DE84A64282BE, #20976482).
    fn preflight_in(&self, tx: &TxFields, rules: &Sandbox) -> TxResult {
        use crate::ledger::amendments::{enabled, CONFIDENTIAL_TRANSFER, DYNAMIC_MPT, SINGLE_ASSET_VAULT};
        let has = |f: &str| tx.fields.get(f).is_some();
        let has_domain = has("DomainID");
        if has_domain && !domain_field_enabled(rules) {
            return TxResult::Disabled;
        }
        if has_domain && has("Holder") {
            return TxResult::Malformed;
        }
        let basic = self.preflight(tx);
        if basic != TxResult::Success {
            return basic;
        }
        if enabled(rules, SINGLE_ASSET_VAULT) || enabled(rules, DYNAMIC_MPT) || enabled(rules, CONFIDENTIAL_TRANSFER) {
            let flags = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
            // :45-49 `isMutate`: a capability-enabling flag, or a new
            // metadata, transfer fee or immutable-flags value.
            let is_mutate = flags & TF_MPT_SET_ENABLE_FLAG_MASK != 0
                || has("MPTokenMetadata")
                || has("TransferFee")
                || has("ImmutableFlags");
            let has_key = has("IssuerEncryptionKey") || has("AuditorEncryptionKey");
            if flags == 0 && !has_domain && !has_key && !is_mutate {
                return TxResult::Malformed;
            }
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        if !sandbox.exists(&keylet::account_root_key(&tx.account)) {
            return TxResult::NoAccount;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let Some(id) = issuance_id_of(tx) else { return TxResult::Malformed };
        let ikey = keylet::mpt_issuance_key(&id);
        let Some(iss) = read_json(sandbox, &ikey) else { return TxResult::ObjectNotFound };
        let flags = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
        let iflags = iss.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
        // Finding 372 (campaign 18 4-19, testnet 6F5A4B7BDA4B): on an
        // issuance without lsfMPTCanLock, EVERY IssuanceSet is
        // tecNO_PERMISSION until SingleAssetVault or DynamicMPT is enabled;
        // only then does the refusal narrow to lock/unlock
        // (MPTokenIssuanceSet.cpp:142-154). A Flags-0 set landed tes here.
        if iflags & LSF_MPT_CAN_LOCK == 0 {
            use crate::ledger::amendments::{enabled, DYNAMIC_MPT, SINGLE_ASSET_VAULT};
            let widened = enabled(sandbox, SINGLE_ASSET_VAULT) || enabled(sandbox, DYNAMIC_MPT);
            if !widened || flags & (TF_MPT_LOCK | TF_MPT_UNLOCK) != 0 {
                return TxResult::NoPermission;
            }
        }
        let is_issuer = iss
            .get("Issuer")
            .and_then(|v| v.as_str())
            .and_then(crate::tx::offer::decode20)
            .map(|i| i == tx.account)
            .unwrap_or(false);
        if !is_issuer {
            return TxResult::NoPermission;
        }
        let holder = tx
            .fields
            .get("Holder")
            .and_then(|h| h.as_str())
            .and_then(crate::tx::offer::decode20);
        let (target_key, mut target) = match holder {
            Some(hid) => {
                if !sandbox.exists(&keylet::account_root_key(&hid)) {
                    return TxResult::NoDst;
                }
                let tk = keylet::mptoken_key(&ikey, &hid);
                let Some(t) = read_json(sandbox, &tk) else {
                    return TxResult::ObjectNotFound;
                };
                (tk, t)
            }
            None => (ikey, iss),
        };
        // G13 — preclaim (MPTokenIssuanceSet.cpp:166-177): a DomainID needs
        // lsfMPTRequireAuth on the issuance (tecNO_PERMISSION) and, unless it
        // is the zero that clears the field, an existing PermissionedDomain
        // (tecOBJECT_NOT_FOUND). Preflight refused the field without both
        // amendments, and with a Holder.
        let domain = tx.fields.get("DomainID");
        if let Some(d) = domain {
            if iflags & LSF_MPT_REQUIRE_AUTH == 0 {
                return TxResult::NoPermission;
            }
            if !is_zero_hash(d) {
                let found = key_field(&tx.fields, "DomainID")
                    .and_then(|k| read_json(sandbox, &k))
                    .is_some_and(|pd| pd.get("LedgerEntryType").and_then(|t| t.as_str()) == Some("PermissionedDomain"));
                if !found {
                    return TxResult::ObjectNotFound;
                }
            }
        }
        let fin = target.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
        let mut fout = fin;
        if flags & TF_MPT_LOCK != 0 {
            fout |= LSF_MPT_LOCKED;
        } else if flags & TF_MPT_UNLOCK != 0 {
            fout &= !LSF_MPT_LOCKED;
        }
        if fout != fin {
            target["Flags"] = serde_json::Value::Number(fout.into());
        }
        // TransferFee / MPTokenMetadata / DomainID follow soeDEFAULT-style
        // updates on the ISSUANCE only: zero/empty removes, non-zero sets.
        if holder.is_none() {
            if let Some(f) = tx.fields.get("TransferFee").and_then(|v| v.as_u64()) {
                if f == 0 {
                    target.as_object_mut().map(|o| o.remove("TransferFee"));
                } else {
                    target["TransferFee"] = serde_json::Value::Number(f.into());
                }
            }
            if let Some(m) = tx.fields.get("MPTokenMetadata").and_then(|v| v.as_str()) {
                if m.is_empty() {
                    target.as_object_mut().map(|o| o.remove("MPTokenMetadata"));
                } else {
                    target["MPTokenMetadata"] = serde_json::Value::String(m.to_string());
                }
            }
            // doApply :360-376: a non-zero domain is set, the zero one clears.
            if let Some(d) = domain {
                if is_zero_hash(d) {
                    target.as_object_mut().map(|o| o.remove("DomainID"));
                } else {
                    target["DomainID"] = d.clone();
                }
            }
        }
        sandbox.write(target_key, serde_json::to_vec(&target).unwrap_or_default());
        TxResult::Success
    }
}

#[cfg(test)]
mod fixcleanup340_tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::state::LedgerState;

    /// fixCleanup3_4_0 (MPTokenAuthorize.cpp:79-83): a holder cannot drop a
    /// LOCKED MPToken while the issuance exists — tecNO_PERMISSION; before
    /// the amendment (and without SingleAssetVault) the empty token goes.
    #[test]
    fn a_holder_cannot_unauthorize_a_locked_mptoken_under_fixcleanup340() {
        let holder = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        let header = LedgerHeader {
            sequence: 100, total_coins: 100_000_000_000_000_000,
            parent_hash: Hash256([0; 32]), transaction_hash: Hash256([0; 32]), account_hash: Hash256([0; 32]),
            parent_close_time: 0, close_time: 10, close_time_resolution: 10, close_flags: 0,
        };
        let mut state = LedgerState::new_unverified(header);
        for (id, oc) in [(holder, 1u64), (issuer, 1)] {
            let acct = serde_json::json!({
                "LedgerEntryType": "AccountRoot", "Account": hex::encode(id), "Balance": "50000000",
                "Sequence": 1, "OwnerCount": oc, "Flags": 0,
            });
            state.state_map.insert(keylet::account_root_key(&id), serde_json::to_vec(&acct).unwrap()).unwrap();
        }
        let mut id = [0u8; 24];
        id[..4].copy_from_slice(&1u32.to_be_bytes());
        id[4..].copy_from_slice(&issuer);
        let ikey = keylet::mpt_issuance_key(&id);
        let iss = serde_json::json!({
            "LedgerEntryType": "MPTokenIssuance", "Issuer": hex::encode(issuer), "Flags": 0,
            "Sequence": 1, "OutstandingAmount": "0", "OwnerNode": "0",
        });
        state.state_map.insert(ikey, serde_json::to_vec(&iss).unwrap()).unwrap();
        let tkey = keylet::mptoken_key(&ikey, &holder);
        let tok = serde_json::json!({
            "LedgerEntryType": "MPToken", "Account": hex::encode(holder), "MPTokenIssuanceID": hex::encode_upper(id),
            "MPTAmount": "0", "Flags": LSF_MPT_LOCKED, "OwnerNode": "0",
        });
        state.state_map.insert(tkey, serde_json::to_vec(&tok).unwrap()).unwrap();
        let tx = TxFields {
            account: holder, tx_type: "MPTokenAuthorize".to_string(), fee: 12, sequence: 1,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({"MPTokenIssuanceID": hex::encode_upper(id), "Flags": TF_MPT_UNAUTHORIZE}),
            inner_batch: false,
        };
        let mut sb = Sandbox::new(&state);
        assert_eq!(MPTokenAuthorizeTransactor.do_apply(&tx, &mut sb), TxResult::Success, "pre-amendment the empty token is dropped");
        assert!(!sb.exists(&tkey));
        let am = serde_json::json!({
            "LedgerEntryType": "Amendments", "Flags": 0,
            "Amendments": [crate::ledger::amendments::FIX_CLEANUP_3_4_0],
        });
        state.state_map.insert(keylet::amendments_key(), serde_json::to_vec(&am).unwrap()).unwrap();
        let mut sb = Sandbox::new(&state);
        assert_eq!(MPTokenAuthorizeTransactor.do_apply(&tx, &mut sb), TxResult::NoPermission, "locked: refused under fixCleanup3_4_0");
        assert!(sb.exists(&tkey));
    }
}

#[cfg(test)]
mod f374_tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::state::LedgerState;

    /// Finding 374: an issuance created by a Batch inner (Flags carry
    /// tfInnerBatchTxn 0x40000000) stores its flags without EITHER universal
    /// bit — `args.flags & ~tfUniversal`, tfUniversal = 0xC0000000.
    #[test]
    fn an_issuance_created_inside_a_batch_drops_tf_inner_batch_txn() {
        let issuer = [0x07u8; 20];
        let mut state = LedgerState::new_unverified(LedgerHeader {
            sequence: 100, total_coins: 100_000_000_000_000_000,
            parent_hash: Hash256([0; 32]), transaction_hash: Hash256([0; 32]), account_hash: Hash256([0; 32]),
            parent_close_time: 0, close_time: 10, close_time_resolution: 10, close_flags: 0,
        });
        let acct = serde_json::json!({"LedgerEntryType": "AccountRoot", "Account": hex::encode(issuer),
            "Balance": "50000000", "Sequence": 5, "OwnerCount": 0, "Flags": 0});
        state.state_map.insert(keylet::account_root_key(&issuer), serde_json::to_vec(&acct).unwrap()).unwrap();
        let tx = TxFields {
            account: issuer, tx_type: "MPTokenIssuanceCreate".to_string(), fee: 0, sequence: 5,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({"Flags": 0x4000_0000u64 | 0x8000_0000 | 0x20 | 0x02}),
            inner_batch: true,
        };
        let mut sb = Sandbox::new(&state);
        assert_eq!(MPTokenIssuanceCreateTransactor.do_apply(&tx, &mut sb), TxResult::Success);
        let mut id = [0u8; 24];
        id[..4].copy_from_slice(&5u32.to_be_bytes());
        id[4..].copy_from_slice(&issuer);
        let iss: serde_json::Value = serde_json::from_slice(&sb.read(&keylet::mpt_issuance_key(&id)).unwrap()).unwrap();
        assert_eq!(iss["Flags"].as_u64(), Some(0x22), "only the issuance flags survive: {iss}");
    }

    /// MPTokenIssuanceCreate.cpp:179-180, :226: the transaction's
    /// sfImmutableFlags (UINT32 53) is copied to the issuance under that
    /// name. rippled 3.4.1 has no field called MutableFlags; the SDK codec
    /// knew UINT32 53 by that stale name until branch validator-k256-vault,
    /// and an object key the codec does not know is skipped without an error
    /// when the object is encoded — the issuance would be stored, and
    /// hashed, without the field.
    #[test]
    fn an_issuance_keeps_the_transactions_immutable_flags_under_that_name() {
        let issuer = [0x07u8; 20];
        let mut state = LedgerState::new_unverified(LedgerHeader {
            sequence: 100, total_coins: 100_000_000_000_000_000,
            parent_hash: Hash256([0; 32]), transaction_hash: Hash256([0; 32]), account_hash: Hash256([0; 32]),
            parent_close_time: 0, close_time: 10, close_time_resolution: 10, close_flags: 0,
        });
        let acct = serde_json::json!({"LedgerEntryType": "AccountRoot", "Account": hex::encode(issuer),
            "Balance": "50000000", "Sequence": 5, "OwnerCount": 0, "Flags": 0});
        state.state_map.insert(keylet::account_root_key(&issuer), serde_json::to_vec(&acct).unwrap()).unwrap();
        let mut id = [0u8; 24];
        id[..4].copy_from_slice(&5u32.to_be_bytes());
        id[4..].copy_from_slice(&issuer);
        let created = |fields: serde_json::Value| -> serde_json::Value {
            let tx = TxFields {
                account: issuer, tx_type: "MPTokenIssuanceCreate".to_string(), fee: 12, sequence: 5,
                ticket_seq: None, last_ledger_seq: None, fields, inner_batch: false,
            };
            let mut sb = Sandbox::new(&state);
            assert_eq!(MPTokenIssuanceCreateTransactor.do_apply(&tx, &mut sb), TxResult::Success);
            serde_json::from_slice(&sb.read(&keylet::mpt_issuance_key(&id)).unwrap()).unwrap()
        };
        // tifMPTCanLock (lsifMPTCanLock, 0x02).
        let iss = created(serde_json::json!({"Flags": 0x02, "ImmutableFlags": 0x02}));
        assert_eq!(iss["ImmutableFlags"].as_u64(), Some(0x02), "{iss}");
        assert!(iss.get("MutableFlags").is_none(), "no such field in rippled 3.4.1: {iss}");
        // soeDEFAULT on the object: absent in the transaction, absent on it.
        let iss = created(serde_json::json!({"Flags": 0x02}));
        assert!(iss.get("ImmutableFlags").is_none() && iss.get("MutableFlags").is_none(), "{iss}");
    }
}

/// SingleAssetVault's gates on the MPT helpers every ordinary MPT transaction
/// goes through (inventory G6, G7, G8, G12, G13, G14): the vault objects are
/// seated directly, in the JSON form of rippled's ledger_entries.macro (Vault
/// 0x0084; an AccountRoot's `VaultID` marks the vault's pseudo-account).
#[cfg(test)]
mod vault_gate_tests {
    use super::*;
    use crate::ledger::amendments as am;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::state::LedgerState;
    use serde_json::{json, Value};

    const HOLDER: [u8; 20] = [0x11; 20];
    const HOLDER2: [u8; 20] = [0x12; 20];
    /// Issues the vault's underlying asset.
    const ISSUER: [u8; 20] = [0x21; 20];
    /// The vault's pseudo-account: issues the vault's shares.
    const PSEUDO: [u8; 20] = [0x31; 20];
    const VAULT_KEY: Hash256 = Hash256([0x84; 32]);
    /// Mainnet today: the three cleanups on, no vault amendment.
    const MAINNET: [&str; 3] = [am::FIX_CLEANUP_3_2_0, am::FIX_CLEANUP_3_3_0, am::FIX_CLEANUP_3_4_0];

    const LSF_HIGH_FREEZE: u64 = 0x0080_0000;
    const LSF_HIGH_AUTH: u64 = 0x0008_0000;
    const LSF_HIGH_NO_RIPPLE: u64 = 0x0020_0000;
    const LSF_GLOBAL_FREEZE: u64 = 0x0040_0000;
    const LSF_REQUIRE_AUTH: u64 = 0x0004_0000;
    const LSF_DEFAULT_RIPPLE: u64 = 0x0080_0000;
    const USD: &str = "0000000000000000000000005553440000000000";

    fn state() -> LedgerState {
        let mut state = LedgerState::new_unverified(LedgerHeader {
            sequence: 100, total_coins: 100_000_000_000_000_000,
            parent_hash: Hash256([0; 32]), transaction_hash: Hash256([0; 32]), account_hash: Hash256([0; 32]),
            parent_close_time: 0, close_time: 10, close_time_resolution: 10, close_flags: 0,
        });
        for id in [HOLDER, HOLDER2] {
            root(&mut state, id, 0, json!({}));
        }
        root(&mut state, ISSUER, LSF_DEFAULT_RIPPLE, json!({}));
        state
    }

    fn put(state: &mut LedgerState, key: Hash256, v: &Value) {
        state.state_map.insert(key, serde_json::to_vec(v).unwrap()).unwrap();
    }

    fn with(base: &LedgerState, ids: &[&str]) -> LedgerState {
        let mut state = base.clone();
        put(&mut state, keylet::amendments_key(), &json!({"LedgerEntryType": "Amendments", "Flags": 0, "Amendments": ids}));
        state
    }

    /// Mainnet's cleanups plus the named amendments.
    fn mainnet_plus(base: &LedgerState, ids: &[&str]) -> LedgerState {
        let mut all = MAINNET.to_vec();
        all.extend(ids);
        with(base, &all)
    }

    fn root(state: &mut LedgerState, id: [u8; 20], flags: u64, extra: Value) {
        let mut a = json!({
            "LedgerEntryType": "AccountRoot", "Account": hex::encode(id), "Balance": "50000000",
            "Sequence": 1, "OwnerCount": 0, "Flags": flags,
        });
        for (k, v) in extra.as_object().unwrap() {
            a[k] = v.clone();
        }
        put(state, keylet::account_root_key(&id), &a);
    }

    fn mptid(seq: u32, issuer: [u8; 20]) -> [u8; 24] {
        let mut id = [0u8; 24];
        id[..4].copy_from_slice(&seq.to_be_bytes());
        id[4..].copy_from_slice(&issuer);
        id
    }

    fn issuance(state: &mut LedgerState, id: &[u8; 24], flags: u64, extra: Value) -> Hash256 {
        let issuer: [u8; 20] = id[4..].try_into().unwrap();
        let mut o = json!({
            "LedgerEntryType": "MPTokenIssuance", "Issuer": hex::encode(issuer), "Flags": flags,
            "Sequence": u32::from_be_bytes(id[..4].try_into().unwrap()), "OutstandingAmount": "1000", "OwnerNode": "0",
        });
        for (k, v) in extra.as_object().unwrap() {
            o[k] = v.clone();
        }
        let key = keylet::mpt_issuance_key(id);
        put(state, key, &o);
        key
    }

    fn token(state: &mut LedgerState, id: &[u8; 24], holder: [u8; 20], flags: u64, amount: u64) -> Hash256 {
        let key = keylet::mptoken_key(&keylet::mpt_issuance_key(id), &holder);
        put(state, key, &json!({
            "LedgerEntryType": "MPToken", "Account": hex::encode(holder), "MPTokenIssuanceID": hex::encode_upper(id),
            "MPTAmount": amount.to_string(), "Flags": flags, "OwnerNode": "0",
        }));
        key
    }

    /// A USD line between `holder` and ISSUER. Every holder here sorts below
    /// ISSUER, so ISSUER is the HIGH side: its freeze, its authorization and
    /// its no-ripple are the lsfHigh* bits.
    fn line(state: &mut LedgerState, holder: [u8; 20], flags: u64) -> Hash256 {
        assert!(holder < ISSUER);
        let cur: [u8; 20] = hex::decode(USD).unwrap().try_into().unwrap();
        let key = keylet::ripple_state_key(&holder, &ISSUER, &cur);
        put(state, key, &json!({
            "LedgerEntryType": "RippleState", "Flags": flags,
            "Balance": {"currency": USD, "issuer": "0000000000000000000000000000000000000001", "value": "50"},
            "LowLimit": {"currency": USD, "issuer": hex::encode(holder), "value": "1000"},
            "HighLimit": {"currency": USD, "issuer": hex::encode(ISSUER), "value": "0"},
        }));
        key
    }

    fn usd() -> Value {
        json!({"currency": USD, "issuer": hex::encode(ISSUER)})
    }

    /// The vault on `asset`: PSEUDO's AccountRoot carries `VaultID`, the Vault
    /// sits at that key, and PSEUDO issues the share (transferable, no auth).
    /// Returns the share's id.
    fn vault(state: &mut LedgerState, asset: Value) -> [u8; 24] {
        root(state, PSEUDO, 0x0110_0000 | LSF_DEFAULT_RIPPLE, json!({"Sequence": 0, "VaultID": hex::encode_upper(VAULT_KEY.0)}));
        let share = mptid(1, PSEUDO);
        put(state, VAULT_KEY, &json!({
            "LedgerEntryType": "Vault", "Flags": 0, "Sequence": 7, "OwnerNode": "0", "Owner": hex::encode(HOLDER2),
            "Account": hex::encode(PSEUDO), "Asset": asset, "ShareMPTID": hex::encode_upper(share), "WithdrawalPolicy": 1,
        }));
        issuance(state, &share, LSF_MPT_CAN_TRANSFER, json!({}));
        share
    }

    fn frozen(state: &LedgerState, id: &[u8; 24], who: &[&[u8; 20]]) -> bool {
        let sb = Sandbox::new(state);
        let key = keylet::mpt_issuance_key(id);
        any_frozen(&sb, &key, &json_at(&sb, &key).unwrap(), who)
    }

    fn auth(state: &LedgerState, id: &[u8; 24], who: &[u8; 20], ty: AuthType) -> TxResult {
        let sb = Sandbox::new(state);
        let key = keylet::mpt_issuance_key(id);
        require_auth_at(&sb, &key, &json_at(&sb, &key).unwrap(), who, ty, 0)
    }

    /// G6 — View.cpp:66-139: a vault share is frozen for a holder when the
    /// vault's underlying MPT is locked for that holder or for the share's
    /// issuer (the vault pseudo-account), or locked globally. Only under
    /// SingleAssetVault (:70).
    #[test]
    fn g6_a_vault_share_is_frozen_when_its_underlying_mpt_is_locked() {
        let mut base = state();
        let under = mptid(1, ISSUER);
        let ukey = issuance(&mut base, &under, LSF_MPT_CAN_TRANSFER, json!({}));
        let share = vault(&mut base, json!({"mpt_issuance_id": hex::encode_upper(under)}));
        token(&mut base, &under, PSEUDO, 0, 500);
        token(&mut base, &under, HOLDER, LSF_MPT_LOCKED, 0);
        token(&mut base, &under, HOLDER2, 0, 0);
        token(&mut base, &share, HOLDER, 0, 100);
        token(&mut base, &share, HOLDER2, 0, 100);

        assert!(!frozen(&base, &share, &[&HOLDER]), "no amendments: the share's own locks only");
        assert!(!frozen(&with(&base, &MAINNET), &share, &[&HOLDER]), "mainnet today: unchanged");
        let sav = mainnet_plus(&base, &[am::SINGLE_ASSET_VAULT]);
        assert!(frozen(&sav, &share, &[&HOLDER]), "the underlying is locked for the holder");
        assert!(!frozen(&sav, &share, &[&HOLDER2]), "another holder is not");
        assert!(frozen(&sav, &share, &[&HOLDER2, &HOLDER]), "any of the accounts");
        assert!(!frozen(&mainnet_plus(&base, &[am::LENDING_PROTOCOL]), &share, &[&HOLDER]), "LendingProtocol alone does not gate it");

        // The share's issuer is checked too (View.cpp:138 `{issuer, account}`).
        let mut pseudo_locked = base.clone();
        token(&mut pseudo_locked, &under, PSEUDO, LSF_MPT_LOCKED, 500);
        assert!(frozen(&mainnet_plus(&pseudo_locked, &[am::SINGLE_ASSET_VAULT]), &share, &[&HOLDER2]));
        assert!(!frozen(&with(&pseudo_locked, &MAINNET), &share, &[&HOLDER2]));

        // The underlying's global lock.
        let mut global = base.clone();
        token(&mut global, &under, HOLDER, 0, 0);
        assert!(!frozen(&mainnet_plus(&global, &[am::SINGLE_ASSET_VAULT]), &share, &[&HOLDER]), "nothing locked");
        let mut iss = json_at(&Sandbox::new(&global), &ukey).unwrap();
        iss["Flags"] = json!(LSF_MPT_CAN_TRANSFER | LSF_MPT_LOCKED);
        put(&mut global, ukey, &iss);
        assert!(frozen(&mainnet_plus(&global, &[am::SINGLE_ASSET_VAULT]), &share, &[&HOLDER]));
        assert!(!frozen(&with(&global, &MAINNET), &share, &[&HOLDER]));

        // An ordinary issuance (its issuer is no vault) is untouched.
        assert!(!frozen(&sav, &under, &[&HOLDER2]));
        assert!(frozen(&sav, &under, &[&HOLDER]));
    }

    /// G6 — the underlying may be an IOU (frozen line, global freeze) or XRP
    /// (never frozen): `isAnyFrozen(view, {issuer, account}, vault->at(sfAsset))`
    /// (View.cpp:138, TokenHelpers.cpp:115-125).
    #[test]
    fn g6_a_vault_share_follows_an_iou_underlyings_freeze() {
        let mut base = state();
        let share = vault(&mut base, usd());
        token(&mut base, &share, HOLDER, 0, 100);
        line(&mut base, HOLDER, 0);
        let sav = |s: &LedgerState| mainnet_plus(s, &[am::SINGLE_ASSET_VAULT]);
        assert!(!frozen(&sav(&base), &share, &[&HOLDER]), "nothing frozen");

        let mut line_frozen = base.clone();
        line(&mut line_frozen, HOLDER, LSF_HIGH_FREEZE);
        assert!(frozen(&sav(&line_frozen), &share, &[&HOLDER]), "the issuer froze the holder's line");
        assert!(!frozen(&with(&line_frozen, &MAINNET), &share, &[&HOLDER]), "mainnet today: unchanged");
        assert!(!frozen(&line_frozen, &share, &[&HOLDER]));

        let mut global = base.clone();
        root(&mut global, ISSUER, LSF_DEFAULT_RIPPLE | LSF_GLOBAL_FREEZE, json!({}));
        assert!(frozen(&sav(&global), &share, &[&HOLDER]), "the issuer's global freeze");
        assert!(!frozen(&with(&global, &MAINNET), &share, &[&HOLDER]));

        let mut xrp = state();
        let share = vault(&mut xrp, json!({"currency": "XRP"}));
        token(&mut xrp, &share, HOLDER, 0, 100);
        assert!(!frozen(&sav(&xrp), &share, &[&HOLDER]), "XRP is never frozen");
    }

    /// G6 — a share issuance carrying sfReferenceHolding (vaults created
    /// under fixCleanup3_2_0) names the vault's own MPToken or RippleState for
    /// the underlying; the asset is read from that holding (View.cpp:101-116,
    /// `assetOfHolding` MPTokenHelpers.cpp:580-601) and neither the issuer's
    /// root nor the Vault is consulted. A dangling reference is not frozen.
    #[test]
    fn g6_a_share_with_a_reference_holding_reads_the_underlying_from_it() {
        let sav = |s: &LedgerState| mainnet_plus(s, &[am::SINGLE_ASSET_VAULT]);
        // MPT underlying: the reference is PSEUDO's MPToken of it.
        let mut base = state();
        root(&mut base, PSEUDO, 0x0110_0000, json!({"Sequence": 0, "VaultID": hex::encode_upper(VAULT_KEY.0)}));
        let under = mptid(1, ISSUER);
        issuance(&mut base, &under, LSF_MPT_CAN_TRANSFER, json!({}));
        let holding = token(&mut base, &under, PSEUDO, 0, 500);
        token(&mut base, &under, HOLDER, LSF_MPT_LOCKED, 0);
        let share = mptid(1, PSEUDO);
        issuance(&mut base, &share, LSF_MPT_CAN_TRANSFER, json!({"ReferenceHolding": hex::encode_upper(holding.0)}));
        token(&mut base, &share, HOLDER, 0, 100);
        assert!(frozen(&sav(&base), &share, &[&HOLDER]), "no Vault object is needed: the holding names the asset");
        assert!(!frozen(&sav(&base), &share, &[&HOLDER2]));
        assert!(!frozen(&with(&base, &MAINNET), &share, &[&HOLDER]));

        // IOU underlying: the reference is PSEUDO's trust line; the IOU's
        // issuer is the limit issuer that is not the share's issuer.
        let mut base = state();
        const PSEUDO_L: [u8; 20] = [0x14; 20];
        root(&mut base, PSEUDO_L, 0x0110_0000, json!({"Sequence": 0, "VaultID": hex::encode_upper(VAULT_KEY.0)}));
        let holding = line(&mut base, PSEUDO_L, 0);
        line(&mut base, HOLDER, LSF_HIGH_FREEZE);
        let share = mptid(1, PSEUDO_L);
        issuance(&mut base, &share, LSF_MPT_CAN_TRANSFER, json!({"ReferenceHolding": hex::encode_upper(holding.0)}));
        token(&mut base, &share, HOLDER, 0, 100);
        assert!(frozen(&sav(&base), &share, &[&HOLDER]));
        assert!(!frozen(&sav(&base), &share, &[&HOLDER2]));

        let mut dangling = base.clone();
        issuance(&mut dangling, &share, LSF_MPT_CAN_TRANSFER, json!({"ReferenceHolding": "EE".repeat(32)}));
        assert!(!frozen(&sav(&dangling), &share, &[&HOLDER]), "View.cpp:106-112");
    }

    /// G6 — the recursion stops at kMaxAssetCheckDepth (5) and answers
    /// "frozen" there (View.cpp:73-79). A vault on its own share never ends.
    #[test]
    fn g6_the_asset_recursion_is_frozen_at_depth_five() {
        let mut base = state();
        let share = mptid(1, PSEUDO);
        vault(&mut base, json!({"mpt_issuance_id": hex::encode_upper(share)}));
        token(&mut base, &share, HOLDER, 0, 100);
        assert!(frozen(&mainnet_plus(&base, &[am::SINGLE_ASSET_VAULT]), &share, &[&HOLDER]));
        assert!(!frozen(&with(&base, &MAINNET), &share, &[&HOLDER]));
    }

    fn payment(from: [u8; 20]) -> TxFields {
        TxFields {
            account: from, tx_type: "Payment".to_string(), fee: 12, sequence: 1, ticket_seq: None, last_ledger_seq: None,
            fields: json!({}), inner_batch: false,
        }
    }

    /// G6 through an ordinary transaction — Payment.cpp:709: a holder to
    /// holder payment of a vault share is tecLOCKED while the vault's
    /// underlying asset is frozen for either party.
    #[test]
    fn g6_a_payment_of_vault_shares_is_locked_by_the_underlyings_freeze() {
        let mut base = state();
        let share = vault(&mut base, usd());
        token(&mut base, &share, HOLDER, 0, 100);
        token(&mut base, &share, HOLDER2, 0, 0);
        line(&mut base, HOLDER, 0);
        line(&mut base, HOLDER2, LSF_HIGH_FREEZE);
        let pay = |state: &LedgerState| {
            let mut sb = Sandbox::new(state);
            apply_mpt_payment(&payment(HOLDER), &mut sb, &HOLDER2, share, 10, false)
        };
        assert_eq!(pay(&with(&base, &MAINNET)), TxResult::Success, "mainnet today");
        assert_eq!(pay(&base), TxResult::Success, "no amendments");
        assert_eq!(pay(&mainnet_plus(&base, &[am::SINGLE_ASSET_VAULT])), TxResult::Locked);
    }

    /// G7 (a) — MPTokenHelpers.cpp:388-389, :404, :469: under SingleAssetVault
    /// (or MPTokensV2) a pseudo-account is implicitly authorized. With
    /// fixCleanup3_3_0 the exemption comes before the MPToken test; without
    /// it, after — so a pseudo-account without an MPToken still fails a
    /// Legacy or Strong check there.
    #[test]
    fn g7_a_pseudo_account_is_implicitly_authorized() {
        let mut base = state();
        let id = mptid(1, ISSUER);
        issuance(&mut base, &id, LSF_MPT_REQUIRE_AUTH, json!({}));
        const AMM: [u8; 20] = [0x41; 20];
        const BROKER: [u8; 20] = [0x42; 20];
        root(&mut base, AMM, 0, json!({"AMMID": "AA".repeat(32)}));
        root(&mut base, BROKER, 0, json!({"LoanBrokerID": "BB".repeat(32)}));
        token(&mut base, &id, AMM, 0, 5);
        token(&mut base, &id, HOLDER, 0, 5);

        for ty in [AuthType::Legacy, AuthType::StrongAuth, AuthType::WeakAuth] {
            assert_eq!(auth(&with(&base, &MAINNET), &id, &AMM, ty), TxResult::NoAuth, "mainnet today {ty:?}");
            assert_eq!(auth(&base, &id, &AMM, ty), TxResult::NoAuth, "no amendments {ty:?}");
            for gate in [am::SINGLE_ASSET_VAULT, am::MPTOKENS_V2] {
                let on = mainnet_plus(&base, &[gate]);
                assert_eq!(auth(&on, &id, &AMM, ty), TxResult::Success, "{ty:?}");
                assert_eq!(auth(&on, &id, &BROKER, ty), TxResult::Success, "no MPToken, exempt first {ty:?}");
                assert_eq!(auth(&on, &id, &HOLDER, ty), TxResult::NoAuth, "an ordinary holder is not exempt {ty:?}");
            }
            assert_eq!(auth(&mainnet_plus(&base, &[am::LENDING_PROTOCOL]), &id, &AMM, ty), TxResult::NoAuth, "{ty:?}");
        }
        // Without fixCleanup3_3_0 the MPToken test comes first (:440-443).
        let pre330 = with(&base, &[am::SINGLE_ASSET_VAULT]);
        assert_eq!(auth(&pre330, &id, &AMM, AuthType::Legacy), TxResult::Success);
        assert_eq!(auth(&pre330, &id, &BROKER, AuthType::Legacy), TxResult::NoAuth);
        assert_eq!(auth(&pre330, &id, &BROKER, AuthType::StrongAuth), TxResult::NoAuth);
        assert_eq!(auth(&pre330, &id, &BROKER, AuthType::WeakAuth), TxResult::Success);
    }

    /// G7 (b) — MPTokenHelpers.cpp:407-437: when the issuer is a vault
    /// pseudo-account, authorization is first required on the vault's
    /// underlying asset, with the same AuthType; tefINTERNAL when the issuer's
    /// root or the vault is missing; tecINTERNAL past depth five.
    #[test]
    fn g7_a_vault_share_requires_auth_on_the_underlying_first() {
        let sav = |s: &LedgerState| mainnet_plus(s, &[am::SINGLE_ASSET_VAULT]);
        // IOU underlying whose issuer requires authorization.
        let mut base = state();
        root(&mut base, ISSUER, LSF_DEFAULT_RIPPLE | LSF_REQUIRE_AUTH, json!({}));
        let share = vault(&mut base, usd());
        token(&mut base, &share, HOLDER, 0, 100);
        for ty in [AuthType::Legacy, AuthType::StrongAuth, AuthType::WeakAuth] {
            assert_eq!(auth(&with(&base, &MAINNET), &share, &HOLDER, ty), TxResult::Success, "mainnet today {ty:?}");
            assert_eq!(auth(&base, &share, &HOLDER, ty), TxResult::Success, "no amendments {ty:?}");
            assert_eq!(auth(&sav(&base), &share, &HOLDER, ty), TxResult::NoLine, "no line to a RequireAuth issuer {ty:?}");
        }
        let mut unauthorized = base.clone();
        line(&mut unauthorized, HOLDER, 0);
        assert_eq!(auth(&sav(&unauthorized), &share, &HOLDER, AuthType::Legacy), TxResult::NoAuth);
        assert_eq!(auth(&with(&unauthorized, &MAINNET), &share, &HOLDER, AuthType::Legacy), TxResult::Success);
        let mut authorized = base.clone();
        line(&mut authorized, HOLDER, LSF_HIGH_AUTH);
        assert_eq!(auth(&sav(&authorized), &share, &HOLDER, AuthType::Legacy), TxResult::Success);
        // The share's own rule still applies afterwards: no MPToken of the share.
        assert_eq!(auth(&sav(&authorized), &share, &HOLDER2, AuthType::WeakAuth), TxResult::NoLine);
        line(&mut authorized, HOLDER2, LSF_HIGH_AUTH);
        assert_eq!(auth(&sav(&authorized), &share, &HOLDER2, AuthType::WeakAuth), TxResult::Success);
        assert_eq!(auth(&sav(&authorized), &share, &HOLDER2, AuthType::Legacy), TxResult::NoAuth);
        // The issuer itself is always authorized, before any recursion.
        assert_eq!(auth(&sav(&base), &share, &PSEUDO, AuthType::StrongAuth), TxResult::Success);

        // An issuer that does not require auth: only a Strong check wants the line.
        let mut open = state();
        let share = vault(&mut open, usd());
        token(&mut open, &share, HOLDER, 0, 100);
        assert_eq!(auth(&sav(&open), &share, &HOLDER, AuthType::Legacy), TxResult::Success);
        assert_eq!(auth(&sav(&open), &share, &HOLDER, AuthType::StrongAuth), TxResult::NoLine);

        // MPT underlying with lsfMPTRequireAuth.
        let mut base = state();
        let under = mptid(1, ISSUER);
        issuance(&mut base, &under, LSF_MPT_REQUIRE_AUTH, json!({}));
        let share = vault(&mut base, json!({"mpt_issuance_id": hex::encode_upper(under)}));
        token(&mut base, &share, HOLDER, 0, 100);
        token(&mut base, &under, HOLDER, 0, 0);
        assert_eq!(auth(&sav(&base), &share, &HOLDER, AuthType::Legacy), TxResult::NoAuth);
        assert_eq!(auth(&with(&base, &MAINNET), &share, &HOLDER, AuthType::Legacy), TxResult::Success);
        token(&mut base, &under, HOLDER, LSF_MPT_AUTHORIZED, 0);
        assert_eq!(auth(&sav(&base), &share, &HOLDER, AuthType::Legacy), TxResult::Success);

        // XRP underlying asks nothing.
        let mut xrp = state();
        let share = vault(&mut xrp, json!({"currency": "XRP"}));
        token(&mut xrp, &share, HOLDER, 0, 100);
        assert_eq!(auth(&sav(&xrp), &share, &HOLDER, AuthType::StrongAuth), TxResult::Success);

        // :418-420 the issuer's root is missing; :424-426 its vault is.
        let mut orphan = state();
        let id = mptid(1, [0x77; 20]);
        issuance(&mut orphan, &id, 0, json!({}));
        token(&mut orphan, &id, HOLDER, 0, 1);
        assert_eq!(auth(&sav(&orphan), &id, &HOLDER, AuthType::Legacy), TxResult::TefInternal);
        assert_eq!(auth(&with(&orphan, &MAINNET), &id, &HOLDER, AuthType::Legacy), TxResult::Success);
        let mut no_vault = state();
        root(&mut no_vault, PSEUDO, 0, json!({"VaultID": "CC".repeat(32)}));
        let id = mptid(1, PSEUDO);
        issuance(&mut no_vault, &id, 0, json!({}));
        token(&mut no_vault, &id, HOLDER, 0, 1);
        assert_eq!(auth(&sav(&no_vault), &id, &HOLDER, AuthType::Legacy), TxResult::TefInternal);

        // :407-414 a vault on its own share: tecINTERNAL at depth five.
        let mut cyclic = state();
        let share = mptid(1, PSEUDO);
        vault(&mut cyclic, json!({"mpt_issuance_id": hex::encode_upper(share)}));
        token(&mut cyclic, &share, HOLDER, 0, 1);
        assert_eq!(auth(&sav(&cyclic), &share, &HOLDER, AuthType::Legacy), TxResult::TecInternal);
    }

    /// G7 through an ordinary transaction — Payment.cpp:684-688: paying vault
    /// shares to an account the underlying's issuer has not authorized.
    #[test]
    fn g7_a_payment_of_vault_shares_needs_the_underlyings_authorization() {
        let mut base = state();
        root(&mut base, ISSUER, LSF_DEFAULT_RIPPLE | LSF_REQUIRE_AUTH, json!({}));
        let share = vault(&mut base, usd());
        token(&mut base, &share, HOLDER, 0, 100);
        token(&mut base, &share, HOLDER2, 0, 0);
        line(&mut base, HOLDER, LSF_HIGH_AUTH);
        line(&mut base, HOLDER2, 0);
        let pay = |state: &LedgerState| {
            let mut sb = Sandbox::new(state);
            apply_mpt_payment(&payment(HOLDER), &mut sb, &HOLDER2, share, 10, false)
        };
        assert_eq!(pay(&with(&base, &MAINNET)), TxResult::Success, "mainnet today");
        assert_eq!(pay(&mainnet_plus(&base, &[am::SINGLE_ASSET_VAULT])), TxResult::NoAuth);
        line(&mut base, HOLDER2, LSF_HIGH_AUTH);
        assert_eq!(pay(&mainnet_plus(&base, &[am::SINGLE_ASSET_VAULT])), TxResult::Success);
    }

    /// The DomainID branch of MPT `requireAuth` (MPTokenHelpers.cpp:445-466)
    /// — not amendment-gated in rippled: an issuance can only carry DomainID
    /// once G12/G13 let one be written. An accepted credential of the domain
    /// authorizes, MPToken or not; without one, an existing MPToken falls
    /// through to its own lsfMPTAuthorized, and no MPToken returns
    /// validDomain's verdict.
    #[test]
    fn g7_an_issuance_with_a_domain_authorizes_by_credential() {
        let mut base = state();
        let domain = Hash256([0x6D; 32]);
        const CRED_ISSUER: [u8; 20] = [0x51; 20];
        put(&mut base, domain, &json!({
            "LedgerEntryType": "PermissionedDomain", "Flags": 0, "Owner": hex::encode(ISSUER), "Sequence": 1,
            "AcceptedCredentials": [{"Credential": {"Issuer": hex::encode(CRED_ISSUER), "CredentialType": "ABCD"}}],
        }));
        let cred = |state: &mut LedgerState, who: [u8; 20], flags: u64, expiration: Option<u64>| {
            let mut c = json!({
                "LedgerEntryType": "Credential", "Subject": hex::encode(who), "Issuer": hex::encode(CRED_ISSUER),
                "CredentialType": "ABCD", "Flags": flags,
            });
            if let Some(e) = expiration {
                c["Expiration"] = json!(e);
            }
            put(state, crate::tx::credential::credential_key(&who, &CRED_ISSUER, &[0xAB, 0xCD]), &c);
        };
        const ACCEPTED: u64 = 0x0001_0000;
        let id = mptid(1, ISSUER);
        issuance(&mut base, &id, LSF_MPT_REQUIRE_AUTH, json!({"DomainID": hex::encode_upper(domain.0)}));
        cred(&mut base, HOLDER, ACCEPTED, None);

        for s in [base.clone(), with(&base, &MAINNET), mainnet_plus(&base, &[am::SINGLE_ASSET_VAULT])] {
            // In the domain, no MPToken: a weak check passes, a Legacy or
            // Strong one wants the MPToken first (:440-443).
            assert_eq!(auth(&s, &id, &HOLDER, AuthType::WeakAuth), TxResult::Success);
            assert_eq!(auth(&s, &id, &HOLDER, AuthType::Legacy), TxResult::NoAuth);
            assert_eq!(auth(&s, &id, &HOLDER, AuthType::StrongAuth), TxResult::NoAuth);
            // Outside the domain, no MPToken: validDomain's tecNO_AUTH.
            assert_eq!(auth(&s, &id, &HOLDER2, AuthType::WeakAuth), TxResult::NoAuth);
        }
        // In the domain with an unauthorized MPToken: the credential decides.
        let mut held = base.clone();
        token(&mut held, &id, HOLDER, 0, 5);
        assert_eq!(auth(&held, &id, &HOLDER, AuthType::Legacy), TxResult::Success);
        // Outside it with an MPToken: the token's own flag decides.
        token(&mut held, &id, HOLDER2, 0, 5);
        assert_eq!(auth(&held, &id, &HOLDER2, AuthType::Legacy), TxResult::NoAuth);
        token(&mut held, &id, HOLDER2, LSF_MPT_AUTHORIZED, 5);
        assert_eq!(auth(&held, &id, &HOLDER2, AuthType::Legacy), TxResult::Success);
        // An expired credential and no MPToken: tecEXPIRED; a deleted domain: tecOBJECT_NOT_FOUND.
        let mut expired = base.clone();
        cred(&mut expired, HOLDER2, ACCEPTED, Some(5));
        assert_eq!(auth(&expired, &id, &HOLDER2, AuthType::WeakAuth), TxResult::Expired);
        let mut gone = base.clone();
        issuance(&mut gone, &id, LSF_MPT_REQUIRE_AUTH, json!({"DomainID": "9A".repeat(32)}));
        assert_eq!(auth(&gone, &id, &HOLDER, AuthType::WeakAuth), TxResult::ObjectNotFound);
    }

    /// G8 — TokenHelpers.cpp:449-468: MPT `accountHolds` with
    /// ZeroIfUnauthorized zeroes on the full `requireAuth(StrongAuth)` once
    /// SingleAssetVault (or ConfidentialTransfer) is on; before, only on the
    /// issuance's lsfMPTRequireAuth with the token's lsfMPTAuthorized clear.
    #[test]
    fn g8_account_holds_zeroes_on_the_full_auth_check() {
        // A share whose underlying IOU the holder is not authorized for.
        let mut base = state();
        root(&mut base, ISSUER, LSF_DEFAULT_RIPPLE | LSF_REQUIRE_AUTH, json!({}));
        let share = vault(&mut base, usd());
        token(&mut base, &share, HOLDER, 0, 100);
        line(&mut base, HOLDER, 0);
        let holds = |s: &LedgerState, id: &[u8; 24], frozen: bool, auth: bool| account_holds(&Sandbox::new(s), id, &HOLDER, frozen, auth);
        assert_eq!(holds(&with(&base, &MAINNET), &share, false, true), 100, "mainnet today: the share's own flags pass");
        assert_eq!(holds(&base, &share, false, true), 100);
        let sav = mainnet_plus(&base, &[am::SINGLE_ASSET_VAULT]);
        assert_eq!(holds(&sav, &share, false, true), 0, "requireAuth fails on the underlying");
        assert_eq!(holds(&sav, &share, false, false), 100, "IgnoreAuth");
        assert_eq!(account_holds(&Sandbox::new(&sav), &share, &HOLDER2, false, false), 0, "no MPToken");
        // ZeroIfFrozen under the vault recursion (G6): the holder's line frozen.
        let mut frozen_line = base.clone();
        line(&mut frozen_line, HOLDER, LSF_HIGH_FREEZE | LSF_HIGH_AUTH);
        let sav = mainnet_plus(&frozen_line, &[am::SINGLE_ASSET_VAULT]);
        assert_eq!(holds(&sav, &share, true, false), 0);
        assert_eq!(holds(&sav, &share, false, true), 100, "authorized, and the freeze is ignored");
        assert_eq!(holds(&with(&frozen_line, &MAINNET), &share, true, true), 100);

        // The old flag test, unchanged with the gates off.
        let mut plain = state();
        let id = mptid(1, ISSUER);
        issuance(&mut plain, &id, LSF_MPT_REQUIRE_AUTH, json!({}));
        token(&mut plain, &id, HOLDER, 0, 7);
        for s in [plain.clone(), with(&plain, &MAINNET), mainnet_plus(&plain, &[am::SINGLE_ASSET_VAULT])] {
            assert_eq!(holds(&s, &id, false, true), 0);
            assert_eq!(holds(&s, &id, false, false), 7);
        }
        token(&mut plain, &id, HOLDER, LSF_MPT_AUTHORIZED | LSF_MPT_LOCKED, 7);
        for s in [plain.clone(), with(&plain, &MAINNET), mainnet_plus(&plain, &[am::SINGLE_ASSET_VAULT])] {
            assert_eq!(holds(&s, &id, false, true), 7);
            assert_eq!(holds(&s, &id, true, true), 0, "locked");
        }

        // ConfidentialTransfer alone takes the full check too: a domain
        // credential authorizes an MPToken whose own flag is clear.
        let mut dom = state();
        let domain = Hash256([0x6D; 32]);
        put(&mut dom, domain, &json!({
            "LedgerEntryType": "PermissionedDomain", "Flags": 0, "Owner": hex::encode(HOLDER), "Sequence": 1,
            "AcceptedCredentials": [{"Credential": {"Issuer": hex::encode(ISSUER), "CredentialType": "ABCD"}}],
        }));
        put(&mut dom, crate::tx::credential::credential_key(&HOLDER, &ISSUER, &[0xAB, 0xCD]), &json!({
            "LedgerEntryType": "Credential", "Subject": hex::encode(HOLDER), "Issuer": hex::encode(ISSUER),
            "CredentialType": "ABCD", "Flags": 0x0001_0000u64,
        }));
        issuance(&mut dom, &id, LSF_MPT_REQUIRE_AUTH, json!({"DomainID": hex::encode_upper(domain.0)}));
        token(&mut dom, &id, HOLDER, 0, 9);
        assert_eq!(holds(&with(&dom, &MAINNET), &id, false, true), 0, "the flag test");
        assert_eq!(holds(&mainnet_plus(&dom, &[am::CONFIDENTIAL_TRANSFER]), &id, false, true), 9);
        assert_eq!(holds(&mainnet_plus(&dom, &[am::SINGLE_ASSET_VAULT]), &id, false, true), 9);
    }

    /// `canTransfer` (MPTokenHelpers.cpp:603-656, fixCleanup3_2_0): a share
    /// carrying sfReferenceHolding inherits its underlying's transferability
    /// for a third-party transfer — an MPT without lsfMPTCanTransfer is
    /// tecNO_AUTH, an IOU both of whose lines forbid rippling is terNO_RIPPLE.
    #[test]
    fn can_transfer_follows_the_reference_holding_under_fixcleanup320() {
        let ct = |s: &LedgerState, id: &[u8; 24], from: &[u8; 20], to: &[u8; 20]| {
            let sb = Sandbox::new(s);
            can_transfer_ter(&sb, &json_at(&sb, &keylet::mpt_issuance_key(id)).unwrap(), from, to)
        };
        let mut base = state();
        root(&mut base, PSEUDO, 0x0110_0000, json!({"Sequence": 0, "VaultID": hex::encode_upper(VAULT_KEY.0)}));
        let under = mptid(1, ISSUER);
        issuance(&mut base, &under, 0, json!({}));
        let holding = token(&mut base, &under, PSEUDO, 0, 500);
        let share = mptid(1, PSEUDO);
        issuance(&mut base, &share, LSF_MPT_CAN_TRANSFER, json!({"ReferenceHolding": hex::encode_upper(holding.0)}));
        assert_eq!(ct(&base, &share, &HOLDER, &HOLDER2), TxResult::Success, "before fixCleanup3_2_0");
        let on = with(&base, &MAINNET);
        assert_eq!(ct(&on, &share, &HOLDER, &HOLDER2), TxResult::NoAuth, "the underlying is not transferable");
        assert_eq!(ct(&on, &share, &HOLDER, &PSEUDO), TxResult::Success, "to the share's issuer");
        assert_eq!(ct(&on, &share, &PSEUDO, &HOLDER), TxResult::Success, "from the share's issuer");
        assert_eq!(ct(&on, &under, &HOLDER, &HOLDER2), TxResult::NoAuth);
        assert_eq!(ct(&on, &under, &HOLDER, &ISSUER), TxResult::Success);
        let mut dangling = base.clone();
        issuance(&mut dangling, &share, LSF_MPT_CAN_TRANSFER, json!({"ReferenceHolding": "EE".repeat(32)}));
        assert_eq!(ct(&with(&dangling, &MAINNET), &share, &HOLDER, &HOLDER2), TxResult::TefInternal);
        let mut plain = base.clone();
        issuance(&mut plain, &share, 0, json!({"ReferenceHolding": hex::encode_upper(holding.0)}));
        assert_eq!(ct(&plain, &share, &HOLDER, &HOLDER2), TxResult::NoAuth, "the share's own flag first");

        // IOU underlying (RippleStateHelpers.cpp:603-631).
        let mut iou = state();
        const PSEUDO_L: [u8; 20] = [0x14; 20];
        root(&mut iou, PSEUDO_L, 0x0110_0000, json!({"Sequence": 0, "VaultID": hex::encode_upper(VAULT_KEY.0)}));
        let holding = line(&mut iou, PSEUDO_L, 0);
        let share = mptid(1, PSEUDO_L);
        issuance(&mut iou, &share, LSF_MPT_CAN_TRANSFER, json!({"ReferenceHolding": hex::encode_upper(holding.0)}));
        line(&mut iou, HOLDER, LSF_HIGH_NO_RIPPLE);
        line(&mut iou, HOLDER2, LSF_HIGH_NO_RIPPLE);
        let on = with(&iou, &MAINNET);
        assert_eq!(ct(&on, &share, &HOLDER, &HOLDER2), TxResult::NoRipple, "rippling off on both lines");
        assert_eq!(ct(&iou, &share, &HOLDER, &HOLDER2), TxResult::Success, "before fixCleanup3_2_0");
        line(&mut iou, HOLDER2, 0);
        assert_eq!(ct(&with(&iou, &MAINNET), &share, &HOLDER, &HOLDER2), TxResult::Success, "one line ripples");
        // No line: the issuer's lsfDefaultRipple stands in for it.
        let mut bare = state();
        root(&mut bare, PSEUDO_L, 0x0110_0000, json!({"Sequence": 0, "VaultID": hex::encode_upper(VAULT_KEY.0)}));
        let holding = line(&mut bare, PSEUDO_L, 0);
        issuance(&mut bare, &share, LSF_MPT_CAN_TRANSFER, json!({"ReferenceHolding": hex::encode_upper(holding.0)}));
        assert_eq!(ct(&with(&bare, &MAINNET), &share, &HOLDER, &HOLDER2), TxResult::Success);
        root(&mut bare, ISSUER, 0, json!({}));
        assert_eq!(ct(&with(&bare, &MAINNET), &share, &HOLDER, &HOLDER2), TxResult::NoRipple);
    }

    fn tx(ty: &str, account: [u8; 20], fields: Value) -> TxFields {
        TxFields {
            account, tx_type: ty.to_string(), fee: 12, sequence: 5, ticket_seq: None, last_ledger_seq: None,
            fields, inner_batch: false,
        }
    }

    const TF_MPT_REQUIRE_AUTH: u64 = 0x04;

    /// G12 — MPTokenIssuanceCreate.cpp:30-36 (`checkExtraFeatures`): sfDomainID
    /// is temDISABLED unless PermissionedDomains AND SingleAssetVault are on;
    /// then :94-102 a zero domain or a domain without tfMPTRequireAuth is
    /// temMALFORMED, and the issuance stores the domain (:176-177).
    #[test]
    fn g12_issuance_create_takes_a_domain_only_with_both_amendments() {
        let base = state();
        let domain = "6D".repeat(32);
        let create = tx("MPTokenIssuanceCreate", ISSUER, json!({"Flags": TF_MPT_REQUIRE_AUTH, "DomainID": domain}));
        let plain = tx("MPTokenIssuanceCreate", ISSUER, json!({"Flags": TF_MPT_REQUIRE_AUTH}));
        let pf = |s: &LedgerState, t: &TxFields| MPTokenIssuanceCreateTransactor.preflight_in(t, &Sandbox::new(s));
        for off in [base.clone(), with(&base, &MAINNET), mainnet_plus(&base, &[am::SINGLE_ASSET_VAULT]),
            mainnet_plus(&base, &[am::PERMISSIONED_DOMAINS]), mainnet_plus(&base, &[am::PERMISSIONED_DOMAINS, am::LENDING_PROTOCOL])] {
            assert_eq!(pf(&off, &create), TxResult::Disabled);
            assert_eq!(pf(&off, &plain), TxResult::Success, "without the field nothing changes");
        }
        let on = mainnet_plus(&base, &[am::PERMISSIONED_DOMAINS, am::SINGLE_ASSET_VAULT]);
        assert_eq!(pf(&on, &create), TxResult::Success);
        assert_eq!(pf(&on, &plain), TxResult::Success);
        let zero = tx("MPTokenIssuanceCreate", ISSUER, json!({"Flags": TF_MPT_REQUIRE_AUTH, "DomainID": "00".repeat(32)}));
        assert_eq!(pf(&on, &zero), TxResult::Malformed, "a zero domain");
        let public = tx("MPTokenIssuanceCreate", ISSUER, json!({"Flags": 0, "DomainID": domain}));
        assert_eq!(pf(&on, &public), TxResult::Malformed, "a domain implies tfMPTRequireAuth");

        let mut sb = Sandbox::new(&on);
        assert_eq!(MPTokenIssuanceCreateTransactor.do_apply(&create, &mut sb), TxResult::Success);
        let made = json_at(&sb, &keylet::mpt_issuance_key(&mptid(5, ISSUER))).unwrap();
        assert_eq!(made["DomainID"], json!(domain));
        assert_eq!(made["Flags"], json!(LSF_MPT_REQUIRE_AUTH));
    }

    /// G13 — MPTokenIssuanceSet.cpp:27-32: sfDomainID is temDISABLED unless
    /// PermissionedDomains AND SingleAssetVault are on; :68-69 with sfHolder it
    /// is temMALFORMED; preclaim :166-177 wants lsfMPTRequireAuth on the
    /// issuance (tecNO_PERMISSION) and, for a non-zero domain, the
    /// PermissionedDomain object (tecOBJECT_NOT_FOUND); doApply :360-376 sets
    /// the field, a zero domain clears it.
    #[test]
    fn g13_issuance_set_takes_a_domain_only_with_both_amendments() {
        let mut base = state();
        let id = mptid(1, ISSUER);
        let ikey = issuance(&mut base, &id, LSF_MPT_REQUIRE_AUTH, json!({}));
        let open = mptid(2, ISSUER);
        issuance(&mut base, &open, 0, json!({}));
        let domain = Hash256([0x6D; 32]);
        put(&mut base, domain, &json!({
            "LedgerEntryType": "PermissionedDomain", "Flags": 0, "Owner": hex::encode(ISSUER), "Sequence": 1,
            "AcceptedCredentials": [{"Credential": {"Issuer": hex::encode(ISSUER), "CredentialType": "ABCD"}}],
        }));
        token(&mut base, &id, HOLDER, 0, 0);
        let set = |id: &[u8; 24], d: String| tx("MPTokenIssuanceSet", ISSUER, json!({"MPTokenIssuanceID": hex::encode_upper(id), "DomainID": d}));
        let dhex = hex::encode_upper(domain.0);
        let pf = |s: &LedgerState, t: &TxFields| MPTokenIssuanceSetTransactor.preflight_in(t, &Sandbox::new(s));
        for off in [base.clone(), with(&base, &MAINNET), mainnet_plus(&base, &[am::SINGLE_ASSET_VAULT]),
            mainnet_plus(&base, &[am::PERMISSIONED_DOMAINS])] {
            assert_eq!(pf(&off, &set(&id, dhex.clone())), TxResult::Disabled);
        }
        let on = mainnet_plus(&base, &[am::PERMISSIONED_DOMAINS, am::SINGLE_ASSET_VAULT]);
        assert_eq!(pf(&on, &set(&id, dhex.clone())), TxResult::Success);
        assert_eq!(pf(&on, &set(&id, "00".repeat(32))), TxResult::Success, "a zero domain clears");
        let mut held = set(&id, dhex.clone());
        held.fields["Holder"] = json!(hex::encode(HOLDER));
        assert_eq!(pf(&on, &held), TxResult::Malformed, "DomainID with Holder");

        let apply = |s: &LedgerState, t: &TxFields| {
            let mut sb = Sandbox::new(s);
            let r = MPTokenIssuanceSetTransactor.do_apply(t, &mut sb);
            (r, json_at(&sb, &ikey).unwrap())
        };
        assert_eq!(apply(&on, &set(&open, dhex.clone())).0, TxResult::NoPermission, "the issuance does not require auth");
        assert_eq!(apply(&on, &set(&id, "9A".repeat(32))).0, TxResult::ObjectNotFound, "no such domain");
        let (r, after) = apply(&on, &set(&id, dhex.clone()));
        assert_eq!(r, TxResult::Success);
        assert_eq!(after["DomainID"], json!(dhex));
        let mut with_domain = on.clone();
        put(&mut with_domain, ikey, &after);
        let (r, cleared) = apply(&with_domain, &set(&id, "00".repeat(32)));
        assert_eq!(r, TxResult::Success);
        assert!(cleared.get("DomainID").is_none(), "{cleared}");
        let (r, same) = apply(&on, &set(&id, "00".repeat(32)));
        assert_eq!(r, TxResult::Success, "clearing an absent domain");
        assert!(same.get("DomainID").is_none());
    }

    /// G14 — MPTokenIssuanceSet.cpp:82-89: once SingleAssetVault, DynamicMPT or
    /// ConfidentialTransfer is on, a set that changes nothing (sfFlags zero, no
    /// DomainID, no keys, no mutation) is temMALFORMED. The test is on the raw
    /// flags word: tfFullyCanonicalSig alone is not "zero".
    #[test]
    fn g14_an_issuance_set_that_changes_nothing_is_malformed() {
        let base = state();
        let id = hex::encode_upper(mptid(1, ISSUER));
        let pf = |s: &LedgerState, fields: Value| {
            MPTokenIssuanceSetTransactor.preflight_in(&tx("MPTokenIssuanceSet", ISSUER, fields), &Sandbox::new(s))
        };
        for off in [base.clone(), with(&base, &MAINNET), mainnet_plus(&base, &[am::LENDING_PROTOCOL, am::PERMISSIONED_DOMAINS])] {
            assert_eq!(pf(&off, json!({"MPTokenIssuanceID": id, "Flags": 0})), TxResult::Success, "testnet 6F5A4B7BDA4B's shape");
            assert_eq!(pf(&off, json!({"MPTokenIssuanceID": id})), TxResult::Success);
        }
        for gate in [am::SINGLE_ASSET_VAULT, am::DYNAMIC_MPT, am::CONFIDENTIAL_TRANSFER] {
            let on = mainnet_plus(&base, &[gate, am::PERMISSIONED_DOMAINS]);
            assert_eq!(pf(&on, json!({"MPTokenIssuanceID": id, "Flags": 0})), TxResult::Malformed, "{gate}");
            assert_eq!(pf(&on, json!({"MPTokenIssuanceID": id})), TxResult::Malformed, "{gate}");
            assert_eq!(pf(&on, json!({"MPTokenIssuanceID": id, "Flags": 0, "Holder": hex::encode(HOLDER)})), TxResult::Malformed, "{gate}");
            assert_eq!(pf(&on, json!({"MPTokenIssuanceID": id, "Flags": 1})), TxResult::Success, "a lock changes something");
            assert_eq!(pf(&on, json!({"MPTokenIssuanceID": id, "Flags": 2, "Holder": hex::encode(HOLDER)})), TxResult::Success);
            assert_eq!(pf(&on, json!({"MPTokenIssuanceID": id, "Flags": 0x8000_0000u64})), TxResult::Success, "txFlags is not zero");
        }
        let on = mainnet_plus(&base, &[am::SINGLE_ASSET_VAULT, am::PERMISSIONED_DOMAINS]);
        assert_eq!(pf(&on, json!({"MPTokenIssuanceID": id, "Flags": 0, "DomainID": "6D".repeat(32)})), TxResult::Success, "a domain changes something");
    }
}

#[cfg(test)]
mod payment_fee_tests;
