//! Holding and transfer helpers shared by the vault and lending transactors
//! (rippled 3.4.1), each over an `Asset` — XRP, an IOU or an MPT:
//!
//! | here | rippled |
//! |---|---|
//! | [`is_frozen`], [`check_frozen`], [`check_deep_frozen`] | TokenHelpers.cpp:78-160 |
//! | [`require_auth`], [`can_transfer`] | TokenHelpers.cpp:649-673 |
//! | [`can_add_holding`], [`holding_exists`] | TokenHelpers.cpp:559-604, MPTokenHelpers.cpp:159-173 |
//! | [`add_empty_holding`], [`remove_empty_holding`] | RippleStateHelpers.cpp:642-791, MPTokenHelpers.cpp:176-197, :332-363 |
//! | [`authorize_mptoken`] | MPTokenHelpers.cpp:200-330 |
//! | [`enforce_mptoken_authorization`] | MPTokenHelpers.cpp:481-578 |
//! | [`create_mpt_issuance`] | MPTokenIssuanceCreate.cpp:123-200 (`MPTokenIssuanceCreate::create`) |
//! | [`account_holds`], [`xrp_liquid`] | TokenHelpers.cpp:366-497, AccountRootHelpers.cpp:269-302 |
//! | [`account_send`] | TokenHelpers.cpp:1552-1570 (`accountSend`) |
//! | [`account_send_multi`] | TokenHelpers.cpp:1573-1590 (`accountSendMulti`) |
//! | [`can_withdraw`], [`do_withdraw`] | View.cpp:438-598 |
//! | [`check_withdraw_freeze`], [`check_deposit_freeze`] | TokenHelpers.cpp:160-262 |
//! | [`can_transfer_waive`] | TokenHelpers.cpp:659-673, MPTokenHelpers.cpp:603-656 |
//! | [`account_holds_full`] | TokenHelpers.cpp:325-476 (`SpendableHandling::FullBalance`) |
//! | [`add_transfer_fee`], [`subtract_transfer_fee`], [`asset_transfer_rate`], [`do_withdraw_split`], [`is_representable`] | 3.5.0-rc1: TokenHelpers.cpp:541-569, View.cpp:507-588, STAmount.cpp:448-462 |
//! | [`decrease_owner_count_for_object`] | AccountRootHelpers.cpp:359-376 |
//! | [`has_expired`] | View.cpp:48-63 |
//!
//! The asset-specific halves that already existed are reused, not rewritten:
//! the MPT freeze / auth / transfer recursion through a vault
//! (`tx::mpt::any_frozen`, `require_auth_id`, `can_transfer_ter`), the IOU
//! line mechanics (`tx::offer::line_adjust`: balance, reserve release,
//! default-state deletion, creation on receipt), the single-leg MPT move
//! (`tx::mpt::direct_send_no_fee`), the credential helpers
//! (`tx::misc::verify_deposit_preauth`, `verify_valid_domain`).
//!
//! Not ported: featureSponsor (reserve sponsors — `getEffectiveTxReserveSponsor`
//! and the sponsor arguments are the no-sponsor case throughout) and
//! MPTokensV2's balance hooks.
//!
//! Only the vault transactors (`tx::vault`, behind SingleAssetVault) call any
//! of this: with the vault amendments off nothing changes.

use crate::flow::steps::Asset as IouAsset;
use crate::ledger::amendments::{self, enabled};
use crate::ledger::directory::{owner_dir_insert, owner_dir_remove};
use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{TxFields, TxResult};
use crate::tx::mpt::{self, AuthType, HeldAsset};
use crate::tx::offer::{json_at, put_json, Leg};
use crate::tx::st_number::Amount;
use xrpl_core::types::Hash256;

const LSF_DEFAULT_RIPPLE: u64 = 0x0080_0000;
const LSF_DEPOSIT_AUTH: u64 = 0x0100_0000;
const LSF_REQUIRE_DEST_TAG: u64 = 0x0002_0000;
const LSF_LOW_RESERVE: u64 = 0x0001_0000;
const LSF_HIGH_RESERVE: u64 = 0x0002_0000;
const LSF_LOW_NO_RIPPLE: u64 = 0x0010_0000;
const LSF_HIGH_NO_RIPPLE: u64 = 0x0020_0000;
/// `tfMPTUnauthorize` (TxFlags.h:150).
pub const TF_MPT_UNAUTHORIZE: u64 = 0x0000_0001;

fn flags(obj: &serde_json::Value) -> u64 {
    obj.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0)
}

fn root(sandbox: &Sandbox, account: &[u8; 20]) -> Option<serde_json::Value> {
    json_at(sandbox, &keylet::account_root_key(account))
}

fn owner_count(root: &serde_json::Value) -> u64 {
    root.get("OwnerCount").and_then(|c| c.as_u64()).unwrap_or(0)
}

fn node_hint(v: Option<&serde_json::Value>) -> Option<u64> {
    let v = v?;
    v.as_u64().or_else(|| v.as_str().and_then(|s| u64::from_str_radix(s, 16).ok()))
}

fn iou_asset(currency: &[u8; 20], issuer: &[u8; 20]) -> IouAsset {
    IouAsset { currency: *currency, issuer: Some(*issuer) }
}

/// The issuer of an asset (`Asset::getIssuer`): the zero account for XRP,
/// the account half of an MPT's id.
pub fn asset_issuer(asset: &HeldAsset) -> [u8; 20] {
    match asset {
        HeldAsset::Xrp => [0u8; 20],
        HeldAsset::Iou { issuer, .. } => *issuer,
        HeldAsset::Mpt(id) => {
            let mut issuer = [0u8; 20];
            issuer.copy_from_slice(&id[4..]);
            issuer
        }
    }
}

// ---------------------------------------------------------------------------
// Expiry, owner counts
// ---------------------------------------------------------------------------

/// `ExpiryComparison` (View.h:42).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExpiryComparison {
    /// Expired once the parent close time has REACHED the time (`>=`).
    Inclusive,
    /// Expired only once the parent close time has PASSED it (`>`).
    Exclusive,
}

/// `hasExpired(view, exp, comparison)` (View.cpp:48-63): never without a
/// time; else `parentCloseTime >= exp` (Inclusive, the default argument) or
/// `parentCloseTime > exp` (Exclusive). The sandbox's base is the parent
/// ledger, so its header's close time is the parent close time — the
/// convention of every expiry test in the engine.
pub fn has_expired(sandbox: &Sandbox, exp: Option<u32>, comparison: ExpiryComparison) -> bool {
    let Some(exp) = exp else { return false };
    let parent_close = sandbox.base().header.close_time as u64;
    match comparison {
        ExpiryComparison::Inclusive => parent_close >= exp as u64,
        ExpiryComparison::Exclusive => parent_close > exp as u64,
    }
}

/// `decreaseOwnerCountForObject(view, accountSle, objectSle, count)`
/// (AccountRootHelpers.cpp:359-376): the owner's OwnerCount falls by
/// `count` for an object that is going away. The object decides only WHO is
/// released — its reserve sponsor (`getLedgerEntryReserveSponsor`) — and
/// without featureSponsor that is nobody: it is `decreaseOwnerCount`. An
/// AccountRoot as the object, or a zero count, is a no-op (:367-371, :346).
pub fn decrease_owner_count_for_object(sandbox: &mut Sandbox, account: &[u8; 20], object: &serde_json::Value, count: u32) {
    if count == 0 || object.get("LedgerEntryType").and_then(|t| t.as_str()) == Some("AccountRoot") {
        return;
    }
    crate::tx::offer::owner_count_add(sandbox, account, -(count as i64));
}

// ---------------------------------------------------------------------------
// Freeze, authorization, transferability
// ---------------------------------------------------------------------------

/// `isFrozen(view, account, asset, depth = 0)` (TokenHelpers.cpp:78-83): XRP
/// never; an IOU by the issuer's global freeze or the issuer's side of the
/// account's line (RippleStateHelpers.cpp:126-146); an MPT by the issuance's
/// or the account's MPToken's lock, or — a vault share — the vault's
/// underlying asset (MPTokenHelpers.cpp:74-83).
pub fn is_frozen(sandbox: &Sandbox, account: &[u8; 20], asset: &HeldAsset) -> bool {
    match asset {
        HeldAsset::Xrp => false,
        HeldAsset::Iou { currency, issuer } => crate::flow::view::is_frozen(sandbox, account, &iou_asset(currency, issuer)),
        HeldAsset::Mpt(id) => {
            let key = keylet::mpt_issuance_key(id);
            mpt::any_frozen_opt(sandbox, &key, json_at(sandbox, &key).as_ref(), &[account])
        }
    }
}

/// `checkFrozen(view, account, asset)` (TokenHelpers.cpp:85-103): tecFROZEN
/// for a frozen IOU, tecLOCKED for a locked MPT.
pub fn check_frozen(sandbox: &Sandbox, account: &[u8; 20], asset: &HeldAsset) -> TxResult {
    match (is_frozen(sandbox, account, asset), asset) {
        (false, _) => TxResult::Success,
        (true, HeldAsset::Mpt(_)) => TxResult::Locked,
        (true, _) => TxResult::Frozen,
    }
}

/// `checkDeepFrozen(view, account, asset)` (TokenHelpers.cpp:148-160): an
/// IOU line deep-frozen by either side is tecFROZEN
/// (RippleStateHelpers.h:128-131, .cpp:149-171 — never for the issuer or
/// without a line); an MPT has no deeper freeze, so it is `isFrozen` again,
/// tecLOCKED (:124-136, :148-152).
pub fn check_deep_frozen(sandbox: &Sandbox, account: &[u8; 20], asset: &HeldAsset) -> TxResult {
    match asset {
        HeldAsset::Xrp => TxResult::Success,
        HeldAsset::Iou { currency, issuer } => {
            if account != issuer && crate::flow::view::is_deep_frozen(sandbox, account, &iou_asset(currency, issuer)) {
                TxResult::Frozen
            } else {
                TxResult::Success
            }
        }
        HeldAsset::Mpt(_) => {
            if is_frozen(sandbox, account, asset) {
                TxResult::Locked
            } else {
                TxResult::Success
            }
        }
    }
}

/// `requireAuth(view, asset, account, authType)` (TokenHelpers.cpp:649-657):
/// the IOU rule (RippleStateHelpers.cpp:570-600 — tecNO_LINE without a line
/// on a strong check or under lsfRequireAuth, tecNO_AUTH on an unauthorized
/// line, a pseudo-account exempt under fixCleanup3_4_0) or the MPT rule
/// (MPTokenHelpers.cpp:374-477, with the vault recursion).
pub fn require_auth(sandbox: &Sandbox, asset: &HeldAsset, account: &[u8; 20], auth: AuthType) -> TxResult {
    match asset {
        HeldAsset::Xrp => TxResult::Success,
        HeldAsset::Iou { currency, issuer } => crate::tx::offer::require_auth_ter(
            sandbox,
            &Leg { xrp: false, cur: *currency, issuer: *issuer },
            account,
            auth == AuthType::StrongAuth,
        )
        // `require_auth_ter` is None only without the issuer's root, where
        // rippled finds no lsfRequireAuth to enforce.
        .unwrap_or(TxResult::Success),
        HeldAsset::Mpt(id) => mpt::require_auth_id(sandbox, id, account, auth, 0),
    }
}

/// `canTransfer(view, asset, from, to)` (TokenHelpers.cpp:659-673), with the
/// default `WaiveMPTCanTransfer::No`: an IOU moves between two non-issuers
/// unless rippling is off on both sides (terNO_RIPPLE,
/// RippleStateHelpers.cpp:603-631); an MPT needs lsfMPTCanTransfer between
/// non-issuers (tecNO_AUTH) and, as a vault share with sfReferenceHolding,
/// its underlying asset's leave (MPTokenHelpers.cpp:603-700);
/// tecOBJECT_NOT_FOUND without the issuance.
pub fn can_transfer(sandbox: &Sandbox, asset: &HeldAsset, from: &[u8; 20], to: &[u8; 20]) -> TxResult {
    match asset {
        HeldAsset::Xrp => TxResult::Success,
        HeldAsset::Iou { currency, issuer } => mpt::iou_can_transfer(sandbox, currency, issuer, from, to),
        HeldAsset::Mpt(id) => match json_at(sandbox, &keylet::mpt_issuance_key(id)) {
            Some(issuance) => mpt::can_transfer_ter(sandbox, &issuance, from, to),
            None => TxResult::ObjectNotFound,
        },
    }
}

// ---------------------------------------------------------------------------
// Holdings
// ---------------------------------------------------------------------------

/// `canAddHolding(view, asset)`: whether an account could be given a
/// holding of the asset at all. XRP always. An IOU (TokenHelpers.cpp:
/// 558-577): terNO_ACCOUNT without the issuer, terNO_RIPPLE unless the
/// issuer has lsfDefaultRipple. An MPT (MPTokenHelpers.cpp:159-173):
/// tecOBJECT_NOT_FOUND without the issuance, tecNO_AUTH unless it is
/// transferable (lsfMPTCanTransfer).
pub fn can_add_holding(sandbox: &Sandbox, asset: &HeldAsset) -> TxResult {
    match asset {
        HeldAsset::Xrp => TxResult::Success,
        HeldAsset::Iou { issuer, .. } => match root(sandbox, issuer) {
            None => TxResult::NoAccount,
            Some(issuer_root) if flags(&issuer_root) & LSF_DEFAULT_RIPPLE == 0 => TxResult::NoRipple,
            Some(_) => TxResult::Success,
        },
        HeldAsset::Mpt(id) => match json_at(sandbox, &keylet::mpt_issuance_key(id)) {
            None => TxResult::ObjectNotFound,
            Some(issuance) if flags(&issuance) & mpt::LSF_MPT_CAN_TRANSFER == 0 => TxResult::NoAuth,
            Some(_) => TxResult::Success,
        },
    }
}

/// `holdingExists(view, account, asset)` (TokenHelpers.cpp:587-613): XRP and
/// the issuer always "hold"; else the trust line or the MPToken exists.
pub fn holding_exists(sandbox: &Sandbox, account: &[u8; 20], asset: &HeldAsset) -> bool {
    match asset {
        HeldAsset::Xrp => true,
        HeldAsset::Iou { currency, issuer } => {
            account == issuer || sandbox.exists(&keylet::ripple_state_key(account, issuer, currency))
        }
        HeldAsset::Mpt(id) => {
            *account == asset_issuer(asset) || sandbox.exists(&keylet::mptoken_key(&keylet::mpt_issuance_key(id), account))
        }
    }
}

/// `addEmptyHolding(ctx, accountID, priorBalance, asset)`
/// (TokenHelpers.cpp:614-627): give `account` an empty holding of the asset
/// — nothing for XRP; a zero-balance, zero-limit trust line; or an MPToken.
/// `prior_balance` is the XRP balance the reserve is judged against
/// (rippled passes the transaction's `preFeeBalance_`).
///
/// IOU (RippleStateHelpers.cpp:642-720), in order: the issuer itself holds
/// nothing (tesSUCCESS); with fixCleanup3_4_0 an existing line is
/// tecDUPLICATE before anything else; a globally frozen issuer is
/// tecFROZEN; a missing root is tefINTERNAL; an issuer without
/// lsfDefaultRipple is terNO_RIPPLE (tecINTERNAL before fixCleanup3_4_0);
/// without the fix the existing line is tecDUPLICATE here; the account must
/// afford one more owned object — tecNO_LINE_INSUF_RESERVE — and
/// `trustCreate` (:182-296) writes the line: the account's side reserved
/// and NoRipple, limit 0, balance 0, in both owner directories (low first),
/// the account's OwnerCount + 1.
///
/// MPT (MPTokenHelpers.cpp:176-197): tefINTERNAL without the issuance or
/// when it is locked; tecDUPLICATE when the MPToken exists; nothing for the
/// issuer; else [`authorize_mptoken`].
pub fn add_empty_holding(sandbox: &mut Sandbox, account: &[u8; 20], prior_balance: u64, asset: &HeldAsset) -> TxResult {
    match asset {
        HeldAsset::Xrp => TxResult::Success,
        HeldAsset::Iou { currency, issuer } => add_empty_line(sandbox, account, prior_balance, currency, issuer),
        HeldAsset::Mpt(id) => {
            let issuance_key = keylet::mpt_issuance_key(id);
            let Some(issuance) = json_at(sandbox, &issuance_key) else { return TxResult::TefInternal };
            if flags(&issuance) & mpt::LSF_MPT_LOCKED != 0 {
                return TxResult::TefInternal;
            }
            if sandbox.exists(&keylet::mptoken_key(&issuance_key, account)) {
                return TxResult::Duplicate;
            }
            if *account == asset_issuer(asset) {
                return TxResult::Success;
            }
            authorize_mptoken(sandbox, prior_balance, id, account, 0, None)
        }
    }
}

fn add_empty_line(sandbox: &mut Sandbox, account: &[u8; 20], prior_balance: u64, currency: &[u8; 20], issuer: &[u8; 20]) -> TxResult {
    if account == issuer {
        return TxResult::Success;
    }
    let key = keylet::ripple_state_key(issuer, account, currency);
    let fix340 = amendments::fix_cleanup_3_4_0(sandbox);
    if fix340 && sandbox.exists(&key) {
        return TxResult::Duplicate;
    }
    if crate::flow::view::is_global_frozen(sandbox, issuer) {
        return TxResult::Frozen;
    }
    let (Some(issuer_root), Some(account_root)) = (root(sandbox, issuer), root(sandbox, account)) else {
        return TxResult::TefInternal;
    };
    if flags(&issuer_root) & LSF_DEFAULT_RIPPLE == 0 {
        return if fix340 { TxResult::NoRipple } else { TxResult::TecInternal };
    }
    if !fix340 && sandbox.exists(&key) {
        return TxResult::Duplicate;
    }
    // checkReserve(…, {.ownerCountDelta = 1}, tecNO_LINE_INSUF_RESERVE)
    // (AccountRootHelpers.cpp:472-476, featureSponsor off).
    if prior_balance < crate::ledger::fees::account_reserve(sandbox, owner_count(&account_root) + 1) {
        return TxResult::NoLineInsufReserve;
    }
    // trustCreate(view, high = issuer > account, src = issuer, dst = account,
    // …, bAuth = false, bNoRipple = true, bFreeze = false, deepFreeze = false,
    // balance 0 (issuer noAccount), limit 0 (issuer = account), 0, 0): the
    // side being set is the account's.
    let account_high = account > issuer;
    let (low, high) = if account_high { (issuer, account) } else { (account, issuer) };
    let low_node = owner_dir_insert(sandbox, low, &key);
    let high_node = owner_dir_insert(sandbox, high, &key);
    let mut line_flags = if account_high { LSF_HIGH_RESERVE | LSF_HIGH_NO_RIPPLE } else { LSF_LOW_RESERVE | LSF_LOW_NO_RIPPLE };
    if flags(&issuer_root) & LSF_DEFAULT_RIPPLE == 0 {
        // "The other side's default is no rippling" (:277-281) — unreachable
        // here (DefaultRipple was required above), kept for the shape.
        line_flags |= if account_high { LSF_LOW_NO_RIPPLE } else { LSF_HIGH_NO_RIPPLE };
    }
    let cur = hex::encode_upper(currency);
    let line = serde_json::json!({
        "LedgerEntryType": "RippleState",
        "Flags": line_flags,
        "Balance": {"currency": cur, "issuer": "0000000000000000000000000000000000000001", "value": "0"},
        "LowLimit": {"currency": cur, "issuer": hex::encode(low), "value": "0"},
        "HighLimit": {"currency": cur, "issuer": hex::encode(high), "value": "0"},
        "LowNode": format!("{low_node:x}"),
        "HighNode": format!("{high_node:x}"),
    });
    put_json(sandbox, key, &line);
    crate::tx::offer::owner_count_add(sandbox, account, 1);
    TxResult::Success
}

/// `removeEmptyHolding(ctx, accountID, asset)` (TokenHelpers.cpp:629-641):
/// take an EMPTY holding away.
///
/// XRP (RippleStateHelpers.cpp:729-740): nothing to remove, but the account
/// must hold no XRP — tecHAS_OBLIGATIONS — and exist (tecINTERNAL).
///
/// IOU (:742-790): no line is tesSUCCESS for the issuer and
/// tecOBJECT_NOT_FOUND for anyone else; a non-issuer's non-zero balance is
/// tecHAS_OBLIGATIONS; each side whose reserve flag is set gives its owner
/// count back (tecINTERNAL without that root) and the flag is cleared;
/// `trustDelete` (:298-325) unlinks the line from both owner directories
/// and erases it.
///
/// MPT (MPTokenHelpers.cpp:332-363): no MPToken is tesSUCCESS for the
/// issuer, tecOBJECT_NOT_FOUND otherwise; an MPTAmount — or, under
/// fixCleanup3_1_3, a LockedAmount — is tecHAS_OBLIGATIONS, as is any
/// confidential balance; else [`authorize_mptoken`] with tfMPTUnauthorize.
pub fn remove_empty_holding(sandbox: &mut Sandbox, account: &[u8; 20], asset: &HeldAsset) -> TxResult {
    match asset {
        HeldAsset::Xrp => {
            let Some(account_root) = root(sandbox, account) else { return TxResult::TecInternal };
            let balance = account_root.get("Balance").and_then(|b| b.as_str()).and_then(|s| s.parse::<i128>().ok()).unwrap_or(0);
            if balance != 0 {
                return TxResult::HasObligations;
            }
            TxResult::Success
        }
        HeldAsset::Iou { currency, issuer } => {
            let account_is_issuer = account == issuer;
            let key = keylet::ripple_state_key(account, issuer, currency);
            let Some(mut line) = json_at(sandbox, &key) else {
                return if account_is_issuer { TxResult::Success } else { TxResult::ObjectNotFound };
            };
            if !account_is_issuer && crate::tx::offer::signed_value(&line["Balance"]).1 .0 != 0 {
                return TxResult::HasObligations;
            }
            let side = |line: &serde_json::Value, field: &str| crate::tx::offer::amount_issuer20(&line[field]);
            let (Some(low), Some(high)) = (side(&line, "LowLimit"), side(&line, "HighLimit")) else {
                return TxResult::TecInternal;
            };
            for (owner, reserve) in [(low, LSF_LOW_RESERVE), (high, LSF_HIGH_RESERVE)] {
                if flags(&line) & reserve != 0 {
                    if root(sandbox, &owner).is_none() {
                        return TxResult::TecInternal;
                    }
                    crate::tx::offer::owner_count_add(sandbox, &owner, -1);
                    line["Flags"] = serde_json::Value::from(flags(&line) & !reserve);
                }
            }
            // The erased version carries the cleared flags ("this will make
            // the metadata reflect an accurate state at the time of
            // deletion", :759-761).
            put_json(sandbox, key, &line);
            sandbox.delete(key);
            owner_dir_remove(sandbox, &low, &key, node_hint(line.get("LowNode")), false);
            owner_dir_remove(sandbox, &high, &key, node_hint(line.get("HighNode")), false);
            TxResult::Success
        }
        HeldAsset::Mpt(id) => {
            let account_is_issuer = *account == asset_issuer(asset);
            let Some(token) = json_at(sandbox, &keylet::mptoken_key(&keylet::mpt_issuance_key(id), account)) else {
                return if account_is_issuer { TxResult::Success } else { TxResult::ObjectNotFound };
            };
            if mpt::dec_field(&token, "MPTAmount") != 0
                || (enabled(sandbox, amendments::FIX_CLEANUP_3_1_3) && mpt::dec_field(&token, "LockedAmount") != 0)
            {
                return TxResult::HasObligations;
            }
            const CONFIDENTIAL: [&str; 4] =
                ["ConfidentialBalanceInbox", "ConfidentialBalanceSpending", "IssuerEncryptedBalance", "AuditorEncryptedBalance"];
            if CONFIDENTIAL.iter().any(|f| token.get(f).is_some()) {
                return TxResult::HasObligations;
            }
            authorize_mptoken(sandbox, 0, id, account, TF_MPT_UNAUTHORIZE, None)
        }
    }
}

/// `authorizeMPToken(ctx, priorBalance, mptIssuanceID, account, journal,
/// flags = 0, holderID = nullopt)` (MPTokenHelpers.cpp:200-330) — what
/// MPTokenAuthorize::doApply runs, callable for any account.
///
/// Without a holder, `account` is the holder:
/// - with tfMPTUnauthorize its MPToken is deleted — unlinked from its owner
///   directory, OwnerCount − 1. A missing token, an MPTAmount, or (under
///   fixCleanup3_1_3) a LockedAmount is tecINTERNAL: the caller checked.
/// - otherwise an MPToken is created — linked into the owner directory,
///   OwnerCount + 1. The reserve is enforced only from the third owned
///   object on (`ownerCount >= 2`): tecINSUFFICIENT_RESERVE when
///   `prior_balance` is below the reserve for one more object. A missing
///   issuance or the issuer itself is tecINTERNAL once LendingProtocol is
///   enabled (before, rippled goes on and creates the token).
///
/// With a holder, `account` is the issuer granting or (tfMPTUnauthorize)
/// revoking lsfMPTAuthorized on the holder's MPToken; a missing issuance,
/// another issuer or a missing token is tecINTERNAL.
///
/// tecINTERNAL also without `account`'s root. The existence and duplicate
/// tests of MPTokenAuthorize::preclaim are the caller's.
pub fn authorize_mptoken(
    sandbox: &mut Sandbox,
    prior_balance: u64,
    mptid: &[u8; 24],
    account: &[u8; 20],
    tx_flags: u64,
    holder: Option<&[u8; 20]>,
) -> TxResult {
    let Some(account_root) = root(sandbox, account) else { return TxResult::TecInternal };
    let issuance_key = keylet::mpt_issuance_key(mptid);
    let Some(holder) = holder else {
        let token_key = keylet::mptoken_key(&issuance_key, account);
        if tx_flags & TF_MPT_UNAUTHORIZE != 0 {
            let Some(token) = json_at(sandbox, &token_key) else { return TxResult::TecInternal };
            if mpt::dec_field(&token, "MPTAmount") != 0
                || (enabled(sandbox, amendments::FIX_CLEANUP_3_1_3) && mpt::dec_field(&token, "LockedAmount") != 0)
            {
                return TxResult::TecInternal;
            }
            owner_dir_remove(sandbox, account, &token_key, node_hint(token.get("OwnerNode")), false);
            decrease_owner_count_for_object(sandbox, account, &token, 1);
            sandbox.delete(token_key);
            return TxResult::Success;
        }
        if owner_count(&account_root) >= 2
            && prior_balance < crate::ledger::fees::account_reserve(sandbox, owner_count(&account_root) + 1)
        {
            return TxResult::InsufficientReserve;
        }
        let issuer = json_at(sandbox, &issuance_key).and_then(|i| mpt::issuance_issuer(&i));
        if (issuer.is_none() || issuer == Some(*account)) && enabled(sandbox, amendments::LENDING_PROTOCOL) {
            return TxResult::TecInternal;
        }
        mpt::create_mptoken(sandbox, &issuance_key, mptid, account);
        crate::tx::offer::owner_count_add(sandbox, account, 1);
        return TxResult::Success;
    };
    let Some(issuance) = json_at(sandbox, &issuance_key) else { return TxResult::TecInternal };
    if mpt::issuance_issuer(&issuance) != Some(*account) {
        return TxResult::TecInternal;
    }
    let token_key = keylet::mptoken_key(&issuance_key, holder);
    let Some(mut token) = json_at(sandbox, &token_key) else { return TxResult::TecInternal };
    let before = flags(&token);
    let after =
        if tx_flags & TF_MPT_UNAUTHORIZE != 0 { before & !mpt::LSF_MPT_AUTHORIZED } else { before | mpt::LSF_MPT_AUTHORIZED };
    if before != after {
        token["Flags"] = serde_json::Value::from(after);
    }
    put_json(sandbox, token_key, &token);
    TxResult::Success
}

/// `enforceMPTokenAuthorization(ctx, mptIssuanceID, account, priorBalance)`
/// (MPTokenHelpers.cpp:481-578), for an issuance with lsfMPTRequireAuth —
/// the do_apply counterpart of `requireAuth`: it deletes the account's
/// expired domain credentials (`verifyValidDomain`) and creates the MPToken
/// a domain member lacks.
///
/// - tefINTERNAL without the issuance, or for the issuer itself;
/// - not authorized by the issuance's domain (no sfDomainID, no accepted
///   credential, or only expired ones) and no MPToken: tecNO_AUTH, or
///   tecEXPIRED when credentials were found expired;
/// - not authorized by a domain the issuance HAS, with an MPToken: the same
///   tecNO_AUTH / tecEXPIRED;
/// - no domain, an MPToken: tesSUCCESS when it carries lsfMPTAuthorized,
///   else tecNO_AUTH;
/// - authorized by the domain with an MPToken: tesSUCCESS, lsfMPTAuthorized
///   ignored;
/// - authorized by the domain without one: [`authorize_mptoken`] creates it
///   (its reserve test applies).
pub fn enforce_mptoken_authorization(sandbox: &mut Sandbox, mptid: &[u8; 24], account: &[u8; 20], prior_balance: u64) -> TxResult {
    let issuance_key = keylet::mpt_issuance_key(mptid);
    let Some(issuance) = json_at(sandbox, &issuance_key) else { return TxResult::TefInternal };
    if mpt::issuance_issuer(&issuance) == Some(*account) {
        return TxResult::TefInternal;
    }
    let token = json_at(sandbox, &keylet::mptoken_key(&issuance_key, account));
    let domain = mpt::key_field(&issuance, "DomainID");
    let mut expired = false;
    let authorized_by_domain = match &domain {
        None => false,
        Some(domain) => match crate::tx::misc::verify_valid_domain(sandbox, account, domain) {
            TxResult::Success => true,
            TxResult::Expired => {
                expired = true;
                false
            }
            _ => false,
        },
    };
    let refusal = if expired { TxResult::Expired } else { TxResult::NoAuth };
    match (authorized_by_domain, token) {
        (false, None) => refusal,
        (false, Some(_)) if domain.is_some() => refusal,
        (false, Some(token)) => {
            if flags(&token) & mpt::LSF_MPT_AUTHORIZED != 0 {
                TxResult::Success
            } else {
                TxResult::NoAuth
            }
        }
        (true, Some(_)) => TxResult::Success,
        (true, None) => authorize_mptoken(sandbox, prior_balance, mptid, account, 0, None),
    }
}

/// `MPTCreateArgs` (MPTokenIssuanceCreate.h:24-43).
#[derive(Clone, Debug, Default)]
pub struct MptCreateArgs {
    /// The balance the reserve is judged against; None skips the test (as
    /// VaultCreate does for the vault's pseudo-account).
    pub prior_balance: Option<u64>,
    pub account: [u8; 20],
    pub sequence: u32,
    pub flags: u64,
    pub max_amount: Option<u64>,
    pub asset_scale: Option<u8>,
    pub transfer_fee: Option<u16>,
    /// sfMPTokenMetadata, hex.
    pub metadata: Option<String>,
    pub domain_id: Option<Hash256>,
    pub immutable_flags: Option<u32>,
    /// sfReferenceHolding: the key of an existing MPToken or RippleState of
    /// `account` — the holding a wrapped asset's issuance stands for. Only
    /// VaultCreate sets it (VaultCreate.cpp:226-232).
    pub reference_holding: Option<Hash256>,
}

/// The static `MPTokenIssuanceCreate::create(ctx, journal, args)`
/// (MPTokenIssuanceCreate.cpp:123-200): tecINTERNAL without the account;
/// with a prior balance, tecINSUFFICIENT_RESERVE when it does not cover one
/// more owned object; then the issuance at `makeMptID(sequence, account)` —
/// linked into the account's owner directory, Flags = `flags & ~tfUniversal`,
/// OutstandingAmount 0, and each optional argument that was given. A
/// reference holding must exist and be an MPToken or a RippleState, else
/// tecINTERNAL (:182-192). OwnerCount + 1. Returns the new MPTID.
///
/// The optional fields are written when given, as rippled's proxy
/// assignments write them: a SoeDefault one (sfAssetScale, sfTransferFee,
/// sfImmutableFlags) given as zero stays absent.
pub fn create_mpt_issuance(sandbox: &mut Sandbox, args: &MptCreateArgs) -> Result<[u8; 24], TxResult> {
    let Some(account_root) = root(sandbox, &args.account) else { return Err(TxResult::TecInternal) };
    if let Some(prior_balance) = args.prior_balance {
        if prior_balance < crate::ledger::fees::account_reserve(sandbox, owner_count(&account_root) + 1) {
            return Err(TxResult::InsufficientReserve);
        }
    }
    let mut mptid = [0u8; 24];
    mptid[..4].copy_from_slice(&args.sequence.to_be_bytes());
    mptid[4..].copy_from_slice(&args.account);
    let issuance_key = keylet::mpt_issuance_key(&mptid);
    let owner_node = owner_dir_insert(sandbox, &args.account, &issuance_key);
    let mut issuance = serde_json::json!({
        "LedgerEntryType": "MPTokenIssuance",
        "Flags": args.flags & !crate::tx::protocol::TF_UNIVERSAL,
        "Issuer": hex::encode(args.account),
        "OutstandingAmount": "0",
        "OwnerNode": format!("{owner_node:x}"),
        "Sequence": args.sequence,
    });
    if let Some(max) = args.max_amount {
        issuance["MaximumAmount"] = serde_json::Value::String(max.to_string());
    }
    if let Some(scale) = args.asset_scale.filter(|s| *s != 0) {
        issuance["AssetScale"] = serde_json::Value::from(scale);
    }
    if let Some(fee) = args.transfer_fee.filter(|f| *f != 0) {
        issuance["TransferFee"] = serde_json::Value::from(fee);
    }
    if let Some(metadata) = &args.metadata {
        issuance["MPTokenMetadata"] = serde_json::Value::String(metadata.clone());
    }
    if let Some(domain) = &args.domain_id {
        issuance["DomainID"] = serde_json::Value::String(hex::encode_upper(domain.0));
    }
    if let Some(immutable) = args.immutable_flags.filter(|f| *f != 0) {
        issuance["ImmutableFlags"] = serde_json::Value::from(immutable);
    }
    if let Some(holding_key) = &args.reference_holding {
        let kind = json_at(sandbox, holding_key).and_then(|h| h.get("LedgerEntryType").and_then(|t| t.as_str()).map(str::to_string));
        if !matches!(kind.as_deref(), Some("MPToken") | Some("RippleState")) {
            return Err(TxResult::TecInternal);
        }
        issuance["ReferenceHolding"] = serde_json::Value::String(hex::encode_upper(holding_key.0));
    }
    put_json(sandbox, issuance_key, &issuance);
    crate::tx::offer::owner_count_add(sandbox, &args.account, 1);
    Ok(mptid)
}

// ---------------------------------------------------------------------------
// Balances and sends
// ---------------------------------------------------------------------------

/// `xrpLiquid(view, id, ownerCountAdj = 0)` (AccountRootHelpers.cpp:269-302):
/// the account's XRP above its reserve — `accountReserve(OwnerCount)`, and
/// zero for a pseudo-account (:280-283) — or zero when the balance is below
/// it or there is no account. (No PaymentSandbox hooks: this is the plain
/// view the vault transactors run on.)
pub fn xrp_liquid(sandbox: &Sandbox, account: &[u8; 20]) -> u64 {
    let Some(account_root) = root(sandbox, account) else { return 0 };
    let balance = account_root.get("Balance").and_then(|b| b.as_str()).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    let reserve = if crate::tx::misc::is_pseudo_account(sandbox, account) {
        0
    } else {
        crate::ledger::fees::account_reserve(sandbox, owner_count(&account_root))
    };
    balance.saturating_sub(reserve)
}

/// `accountHolds(view, account, asset, zeroIfFrozen, zeroIfUnauthorized)`
/// (TokenHelpers.cpp:478-497) with the default `SimpleBalance`:
/// - XRP: [`xrp_liquid`] (:386-389);
/// - an IOU: the line's balance in the account's terms — negative when the
///   account owes — or zero without a line; with ZeroIfFrozen zero when the
///   line is frozen or deep-frozen (:269-291, :391-399). The authorization
///   argument does not reach an IOU;
/// - an MPT: `tx::mpt::account_holds` (:409-476).
///
/// Not ported: `getLineIfUsable`'s LP-token arm (:293-322, an AMM LP token
/// whose pool assets are frozen or non-transferable). A vault's or a loan
/// broker's asset is never an LP token — its issuer may not be a
/// pseudo-account (VaultCreate::preclaim) — so no caller here can meet it.
pub fn account_holds(sandbox: &Sandbox, account: &[u8; 20], asset: &HeldAsset, zero_if_frozen: bool, zero_if_unauthorized: bool) -> Amount {
    match asset {
        HeldAsset::Xrp => Amount { asset: *asset, negative: false, mantissa: xrp_liquid(sandbox, account), exponent: 0 },
        HeldAsset::Iou { currency, issuer } => {
            let view_asset = iou_asset(currency, issuer);
            if !sandbox.exists(&keylet::ripple_state_key(account, issuer, currency))
                || (zero_if_frozen
                    && (crate::flow::view::is_frozen(sandbox, account, &view_asset)
                        || (account != issuer && crate::flow::view::is_deep_frozen(sandbox, account, &view_asset))))
            {
                return Amount::zero(*asset);
            }
            // getTrustLineBalance (:325-357): the balance, negated for the
            // high account. `credit_balance` is that read from the other
            // side (what the account OWES), so its negation is the holding.
            let owed = crate::flow::view::credit_balance(sandbox, account, issuer, currency);
            Amount { asset: *asset, negative: !owed.negative && owed.mantissa != 0, mantissa: owed.mantissa, exponent: owed.exponent }
        }
        HeldAsset::Mpt(id) => Amount {
            asset: *asset,
            negative: false,
            mantissa: mpt::account_holds(sandbox, id, account, zero_if_frozen, zero_if_unauthorized),
            exponent: 0,
        },
    }
}

/// `accountSend(view, from, to, amount, journal, sponsor = {}, waiveFee,
/// allowOverflow = No)` (TokenHelpers.cpp:1552-1570): move `amount` from
/// one account to another on a plain view.
///
/// XRP and IOU — `accountSendIOU` (:968-1082): a negative amount is
/// tecINTERNAL (fixAMMv1_1, on at mainnet); zero, or sender == receiver, is
/// tesSUCCESS with nothing done.
/// - XRP moves between the two roots; a sender holding less than the amount
///   is tecFAILED_PROCESSING. There is no reserve test here.
/// - An IOU is `directSendNoLimitIOU` (:824-885): one line adjustment when
///   either party is the issuer; otherwise issuer → receiver for the amount
///   FIRST, then sender → issuer for the amount — times the issuer's
///   transfer rate unless the fee is waived. Each leg is
///   `directSendNoFeeIOU` (:681-822), the engine's `line_adjust`: the
///   receiver's line is created when missing (reserve charged, no test), and
///   a sender's line spent to zero in its default state gives its reserve
///   back and is deleted.
///
/// MPT — `accountSendMPT` → `directSendNoLimitMPT` (:1489-1512, :1311-1359):
/// tecOBJECT_NOT_FOUND without the issuance; the issuer minting past
/// MaximumAmount is tecPATH_DRY; one `directSendNoFeeMPT` leg with the
/// issuer, or two (issuer → receiver, then sender → issuer with the transfer
/// fee unless waived) between holders — tecNO_AUTH without an MPToken on
/// either side, tecINSUFFICIENT_FUNDS when the sender holds less.
///
/// Authorization and freezes are the caller's, as in rippled.
pub fn account_send(sandbox: &mut Sandbox, from: &[u8; 20], to: &[u8; 20], amount: &Amount, waive_fee: bool) -> TxResult {
    if let HeldAsset::Mpt(id) = &amount.asset {
        if amount.is_zero() || from == to {
            return TxResult::Success;
        }
        return send_mpt(sandbox, from, to, id, amount.mantissa, waive_fee);
    }
    if amount.negative && !amount.is_zero() {
        return TxResult::TecInternal;
    }
    if amount.is_zero() || from == to {
        return TxResult::Success;
    }
    match &amount.asset {
        HeldAsset::Iou { currency, issuer } => {
            let leg = Leg { xrp: false, cur: *currency, issuer: *issuer };
            let value = (amount.mantissa as u128, amount.exponent);
            if from == issuer {
                crate::tx::offer::line_adjust(sandbox, to, &leg, value, true);
            } else if to == issuer {
                crate::tx::offer::line_adjust(sandbox, from, &leg, value, false);
            } else {
                let actual = match crate::tx::offer::transfer_rate(sandbox, &leg) {
                    Some(rate) if !waive_fee && rate != 1_000_000_000 => {
                        // multiply(saAmount, transferRate): the amount times
                        // rate / 1e9 as a Number product at nearest, taken
                        // back to an amount (as `flow::view::account_send_fee`).
                        let product = crate::tx::amm_swap::n_mul(value, (rate as u128, -9), crate::tx::amm_swap::Rnd::Near);
                        crate::flow::amounts::IouAmount::from_me(false, product).magnitude()
                    }
                    _ => value,
                };
                crate::tx::offer::line_adjust(sandbox, to, &leg, value, true);
                crate::tx::offer::line_adjust(sandbox, from, &leg, actual, false);
            }
            TxResult::Success
        }
        _ => {
            // XRP. A party that is the zero account or has no root is
            // skipped, as rippled skips a null SLE (:1014-1017).
            let drops = amount.mantissa;
            let zero = [0u8; 20];
            if *from != zero {
                let key = keylet::account_root_key(from);
                if let Some(mut sender) = json_at(sandbox, &key) {
                    let balance = sender["Balance"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
                    if balance < drops {
                        return TxResult::TecFailedProcessing;
                    }
                    sender["Balance"] = serde_json::Value::String((balance - drops).to_string());
                    put_json(sandbox, key, &sender);
                }
            }
            if *to != zero {
                let key = keylet::account_root_key(to);
                if let Some(mut receiver) = json_at(sandbox, &key) {
                    let balance = receiver["Balance"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
                    receiver["Balance"] = serde_json::Value::String(balance.saturating_add(drops).to_string());
                    put_json(sandbox, key, &receiver);
                }
            }
            TxResult::Success
        }
    }
}

fn send_mpt(sandbox: &mut Sandbox, from: &[u8; 20], to: &[u8; 20], mptid: &[u8; 24], value: u64, waive_fee: bool) -> TxResult {
    let issuance_key = keylet::mpt_issuance_key(mptid);
    let Some(issuance) = json_at(sandbox, &issuance_key) else { return TxResult::ObjectNotFound };
    let Some(issuer) = mpt::issuance_issuer(&issuance) else { return TxResult::ObjectNotFound };
    if *from == issuer || *to == issuer {
        if *from == issuer {
            // isMPTOverflow(sendAmount, outstanding, maxAmount, No)
            // (MPTokenHelpers.cpp:1067-1078).
            let max = issuance.get("MaximumAmount").map(|_| mpt::dec_field(&issuance, "MaximumAmount")).unwrap_or(mpt::MAX_MPT_AMOUNT);
            let outstanding = mpt::dec_field(&issuance, "OutstandingAmount");
            if value > max || outstanding > max - value {
                return TxResult::PathDry;
            }
        }
        return mpt::direct_send_no_fee(sandbox, &issuance_key, from, to, value);
    }
    let actual = if waive_fee {
        value
    } else {
        match mpt::mpt_multiply_rate(value, mpt::issuance_transfer_rate(&issuance)) {
            Some(actual) => actual,
            None => return TxResult::TecInternal,
        }
    };
    let delivered = mpt::direct_send_no_fee(sandbox, &issuance_key, &issuer, to, value);
    if delivered != TxResult::Success {
        return delivered;
    }
    mpt::direct_send_no_fee(sandbox, &issuance_key, from, &issuer, actual)
}

/// `accountSendMulti(view, senderID, asset, receivers, journal, waiveFee)`
/// (TokenHelpers.cpp:1573-1590): one sender paying several receivers of one
/// asset, the sender debited ONCE for what went through the issuer.
/// `receivers` pairs an account with a `Number`, which becomes an amount of
/// the asset under `ambient` (rippled's thread rounding mode) — the `Err` is
/// that conversion's throw. LoanSet and LoanPay are the callers.
///
/// XRP — `accountSendMultiIOU` (:1084-1213): a negative amount is
/// tecINTERNAL; zero, or the sender itself, is skipped; each receiver that
/// exists is credited as the loop goes (a running total past int64 is
/// tecINTERNAL); then the sender is debited the total —
/// tecFAILED_PROCESSING when it holds less. Failures return at once.
///
/// IOU — `directSendNoLimitMultiIOU` (:887-964): a leg with the issuer on
/// either end is one direct line adjustment; any other leg credits the
/// receiver from the issuer and adds the amount (times the transfer rate
/// unless waived) to what the sender owes the issuer, which is settled in
/// ONE adjustment after the loop.
///
/// MPT — `directSendNoLimitMultiMPT` (:1361-1487): tecOBJECT_NOT_FOUND
/// without the issuance; a negative amount is tecINTERNAL. The issuer as
/// sender is held to MaximumAmount — under fixCleanup3_1_3 by the running
/// total of this call (this send, the aggregate, then outstanding plus the
/// aggregate: tecPATH_DRY), before it by each send alone against the
/// outstanding amount read at the start. Legs with the issuer are direct;
/// the others credit the receiver from the issuer and the sender pays the
/// issuer once at the end. A running total past the 64-bit range is
/// tecINTERNAL.
pub fn account_send_multi(
    sandbox: &mut Sandbox,
    sender: &[u8; 20],
    asset: &HeldAsset,
    receivers: &[([u8; 20], crate::tx::number::Number)],
    waive_fee: bool,
    ambient: crate::tx::number::Rounding,
) -> Result<TxResult, crate::flow::st_amount::StError> {
    let negative = |amount: &Amount| amount.negative && !amount.is_zero();
    match asset {
        HeldAsset::Xrp => {
            let zero = [0u8; 20];
            let mut take: u64 = 0;
            for (receiver, value) in receivers {
                let amount = Amount::from_number(*asset, *value, ambient)?;
                if negative(&amount) {
                    return Ok(TxResult::TecInternal);
                }
                if amount.is_zero() || sender == receiver {
                    continue;
                }
                let key = keylet::account_root_key(receiver);
                let Some(mut receiver_root) = (*receiver != zero).then(|| json_at(sandbox, &key)).flatten() else { continue };
                // checkedAdd over int64 drops.
                let Some(total) = take.checked_add(amount.mantissa).filter(|t| *t <= i64::MAX as u64) else {
                    return Ok(TxResult::TecInternal);
                };
                let balance = receiver_root["Balance"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
                receiver_root["Balance"] = serde_json::Value::String(balance.saturating_add(amount.mantissa).to_string());
                put_json(sandbox, key, &receiver_root);
                take = total;
            }
            if *sender != zero {
                let key = keylet::account_root_key(sender);
                if let Some(mut sender_root) = json_at(sandbox, &key) {
                    let balance = sender_root["Balance"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
                    if balance < take {
                        return Ok(TxResult::TecFailedProcessing);
                    }
                    sender_root["Balance"] = serde_json::Value::String((balance - take).to_string());
                    put_json(sandbox, key, &sender_root);
                }
            }
            Ok(TxResult::Success)
        }
        HeldAsset::Iou { currency, issuer } => {
            let leg = Leg { xrp: false, cur: *currency, issuer: *issuer };
            let mut take = Amount::zero(*asset);
            for (receiver, value) in receivers {
                let amount = Amount::from_number(*asset, *value, ambient)?;
                if amount.is_zero() || sender == receiver {
                    continue;
                }
                let magnitude = (amount.mantissa as u128, amount.exponent);
                if sender == issuer {
                    crate::tx::offer::line_adjust(sandbox, receiver, &leg, magnitude, true);
                    continue;
                }
                if receiver == issuer {
                    crate::tx::offer::line_adjust(sandbox, sender, &leg, magnitude, false);
                    continue;
                }
                let actual = match crate::tx::offer::transfer_rate(sandbox, &leg) {
                    Some(rate) if !waive_fee && rate != 1_000_000_000 => {
                        let product = crate::tx::amm_swap::n_mul(magnitude, (rate as u128, -9), crate::tx::amm_swap::Rnd::Near);
                        let fee_paid = crate::flow::amounts::IouAmount::from_me(false, product);
                        Amount { asset: *asset, negative: false, mantissa: fee_paid.mantissa, exponent: fee_paid.exponent }
                    }
                    _ => amount,
                };
                take = take.iou_add(actual, ambient)?;
                crate::tx::offer::line_adjust(sandbox, receiver, &leg, magnitude, true);
            }
            if sender != issuer && !take.is_zero() {
                crate::tx::offer::line_adjust(sandbox, sender, &leg, (take.mantissa as u128, take.exponent), false);
            }
            Ok(TxResult::Success)
        }
        HeldAsset::Mpt(id) => {
            let issuance_key = keylet::mpt_issuance_key(id);
            let Some(issuance) = json_at(sandbox, &issuance_key) else { return Ok(TxResult::ObjectNotFound) };
            let Some(issuer) = mpt::issuance_issuer(&issuance) else { return Ok(TxResult::ObjectNotFound) };
            let maximum = issuance.get("MaximumAmount").map(|_| mpt::dec_field(&issuance, "MaximumAmount")).unwrap_or(mpt::MAX_MPT_AMOUNT);
            let outstanding = mpt::dec_field(&issuance, "OutstandingAmount");
            let aggregate_cap = enabled(sandbox, amendments::FIX_CLEANUP_3_1_3);
            let rate = mpt::issuance_transfer_rate(&issuance);
            let (mut total_sent, mut actual, mut take): (u64, u64, u64) = (0, 0, 0);
            for (receiver, value) in receivers {
                let amount = Amount::from_number(*asset, *value, ambient)?;
                if negative(&amount) {
                    return Ok(TxResult::TecInternal);
                }
                if amount.is_zero() || sender == receiver {
                    continue;
                }
                let send = amount.mantissa;
                if *sender == issuer || *receiver == issuer {
                    if *sender == issuer {
                        if aggregate_cap {
                            // The order guards each subtraction against underflow (:1410-1417).
                            if send > maximum || total_sent > maximum - send || outstanding > maximum - send - total_sent {
                                return Ok(TxResult::PathDry);
                            }
                            total_sent += send;
                        } else if send > maximum || outstanding > maximum - send {
                            return Ok(TxResult::PathDry);
                        }
                    }
                    let Some(sum) = actual.checked_add(send) else { return Ok(TxResult::TecInternal) };
                    let sent = mpt::direct_send_no_fee(sandbox, &issuance_key, sender, receiver, send);
                    if sent != TxResult::Success {
                        return Ok(sent);
                    }
                    actual = sum;
                    continue;
                }
                let actual_send = if waive_fee {
                    send
                } else {
                    match mpt::mpt_multiply_rate(send, rate) {
                        Some(with_fee) => with_fee,
                        None => return Ok(TxResult::TecInternal),
                    }
                };
                let Some(sum) = actual.checked_add(actual_send) else { return Ok(TxResult::TecInternal) };
                actual = sum;
                take += actual_send;
                let sent = mpt::direct_send_no_fee(sandbox, &issuance_key, &issuer, receiver, send);
                if sent != TxResult::Success {
                    return Ok(sent);
                }
            }
            if *sender != issuer && take != 0 {
                let settled = mpt::direct_send_no_fee(sandbox, &issuance_key, sender, &issuer, take);
                if settled != TxResult::Success {
                    return Ok(settled);
                }
            }
            Ok(TxResult::Success)
        }
    }
}

// ---------------------------------------------------------------------------
// Withdrawals
// ---------------------------------------------------------------------------

/// `withdrawToDestExceedsLimit(view, from, to, amount)` (View.cpp:438-463):
/// an IOU withdrawn to a third party must fit that party's trust limit.
/// `owed = creditBalance(to, issuer)` is the line's balance from the
/// issuer's side (RippleStateHelpers.cpp:73-98): the NEGATIVE of what the
/// destination holds. While the destination holds zero or more (`owed <=
/// 0`), holding its limit already (`-owed >= limit`) or an amount above the
/// room left (`amount > limit + owed`) is tecNO_LINE; a destination with no
/// line has limit zero and always fails. A destination that owes the issuer
/// is not tested. Nothing to test when the destination is the sender or the
/// issuer, for XRP, or for an MPT (a withdrawal mints nothing).
fn withdraw_to_dest_exceeds_limit(sandbox: &Sandbox, from: &[u8; 20], to: &[u8; 20], amount: &Amount) -> TxResult {
    let HeldAsset::Iou { currency, issuer } = &amount.asset else { return TxResult::Success };
    if from == to || to == issuer {
        return TxResult::Success;
    }
    let owed = crate::flow::view::credit_balance(sandbox, to, issuer, currency);
    if owed.mantissa == 0 || owed.negative {
        let limit = crate::flow::view::credit_limit(sandbox, to, issuer, currency);
        // -owed >= limit, or amount > limit + owed (an IOU STAmount sum).
        if owed.negated().number() >= limit.number() || amount.number() > limit.add(owed).number() {
            return TxResult::NoLine;
        }
    }
    TxResult::Success
}

/// `canWithdraw(view, from, to, toSle, amount, hasDestinationTag,
/// credentialIDs)` (View.cpp:465-509) — the preclaim test VaultWithdraw and
/// LoanBrokerCoverWithdraw share. `from` is the submitter, `to` the
/// destination (the submitter when the transaction names none).
///
/// - `checkDestinationAndTag` (AccountRootHelpers.cpp:601-613): tecNO_DST
///   without the destination; tecDST_TAG_NEEDED when it requires a tag the
///   transaction lacks;
/// - a withdrawal to oneself passes here;
/// - a destination with lsfDepositAuth that has not preauthorized the
///   submitter needs credentials: tecNO_PERMISSION without any; with them,
///   each must exist (tecINTERNAL — preclaim validated them) and together
///   they must name a DepositPreauth of the destination
///   (`credentials::authorizedDepositPreauth`), else tecNO_PERMISSION;
/// - the trust-limit test above.
///
/// `credential_ids` is the transaction's sfCredentialIDs, None when absent.
pub fn can_withdraw(
    sandbox: &Sandbox,
    from: &[u8; 20],
    to: &[u8; 20],
    amount: &Amount,
    has_destination_tag: bool,
    credential_ids: Option<&[Hash256]>,
) -> TxResult {
    let Some(to_root) = root(sandbox, to) else { return TxResult::NoDst };
    if flags(&to_root) & LSF_REQUIRE_DEST_TAG != 0 && !has_destination_tag {
        return TxResult::DstTagNeeded;
    }
    if from == to {
        return TxResult::Success;
    }
    if flags(&to_root) & LSF_DEPOSIT_AUTH != 0 && !sandbox.exists(&keylet::deposit_preauth_key(to, from)) {
        let Some(ids) = credential_ids else { return TxResult::NoPermission };
        let mut pairs: Vec<([u8; 20], Vec<u8>)> = Vec::new();
        for id in ids {
            let Some(credential) = json_at(sandbox, id) else { return TxResult::TecInternal };
            let (Some(issuer), Some(kind)) = (
                credential.get("Issuer").and_then(|v| v.as_str()).and_then(crate::tx::offer::decode20),
                credential.get("CredentialType").and_then(|v| v.as_str()).and_then(|h| hex::decode(h).ok()),
            ) else {
                return TxResult::TecInternal;
            };
            pairs.push((issuer, kind));
        }
        // authorizedDepositPreauth (CredentialHelpers.cpp): the sorted,
        // unique (Issuer, CredentialType) pairs name the preauth object.
        pairs.sort();
        pairs.dedup();
        if !sandbox.exists(&keylet::deposit_preauth_credentials_key(to, &pairs)) {
            return TxResult::NoPermission;
        }
    }
    withdraw_to_dest_exceeds_limit(sandbox, from, to, amount)
}

/// `canWithdraw(view, tx)` (View.cpp:525-533): the same from a
/// transaction's own fields — sfAccount, sfDestination (default: the
/// account), sfAmount, sfDestinationTag, sfCredentialIDs. temMALFORMED
/// stands in for a transaction whose fields do not parse (rippled's
/// preflight has refused it by then).
pub fn can_withdraw_tx(sandbox: &Sandbox, tx: &TxFields) -> TxResult {
    let Some(amount) = tx.fields.get("Amount").and_then(Amount::from_json) else { return TxResult::Malformed };
    let to = match tx.fields.get("Destination") {
        Some(destination) => match destination.as_str().and_then(crate::tx::offer::decode20) {
            Some(to) => to,
            None => return TxResult::Malformed,
        },
        None => tx.account,
    };
    let ids = credential_ids(tx);
    can_withdraw(sandbox, &tx.account, &to, &amount, tx.fields.get("DestinationTag").is_some(), ids.as_deref())
}

/// The transaction's sfCredentialIDs as keys; None when the field is absent.
pub fn credential_ids(tx: &TxFields) -> Option<Vec<Hash256>> {
    let ids = tx.fields.get("CredentialIDs")?.as_array()?;
    Some(
        ids.iter()
            .filter_map(|v| v.as_str())
            .filter_map(|h| hex::decode(h).ok())
            .filter_map(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
            .map(Hash256)
            .collect(),
    )
}

/// `doWithdraw(ctx, senderAcct, dstAcct, sourceAcct, priorBalance, amount)`
/// (View.cpp:535-598): pay `amount` out of `source` (a vault's or a
/// broker's pseudo-account) to `dst` on behalf of the submitter `sender`.
///
/// - To oneself: an empty holding is added first when there is something to
///   pay — or always, before fixCleanup3_4_0 — and an existing one
///   (tecDUPLICATE) is fine; any other failure is returned.
/// - To someone else: `verifyDepositPreauth(tx, view, sender, dst, dstSle)`
///   — expired credentials are deleted (tecEXPIRED), a DepositAuth
///   destination must have preauthorized the sender or the credentials.
/// - `source` must hold the amount (freezes and authorization ignored):
///   tefINTERNAL otherwise.
/// - `accountSend(source → dst, amount, WaiveTransferFee::Yes)`.
pub fn do_withdraw(
    sandbox: &mut Sandbox,
    tx: &TxFields,
    sender: &[u8; 20],
    dst: &[u8; 20],
    source: &[u8; 20],
    prior_balance: u64,
    amount: &Amount,
) -> TxResult {
    do_withdraw_split(sandbox, tx, sender, dst, source, prior_balance, amount, amount)
}

// ---------------------------------------------------------------------------
// What the vault transactors needed beyond the first pass
// ---------------------------------------------------------------------------

const LSF_LOW_FREEZE: u64 = 0x0040_0000;
const LSF_HIGH_FREEZE: u64 = 0x0080_0000;

fn frozen_code(asset: &HeldAsset) -> TxResult {
    if matches!(asset, HeldAsset::Mpt(_)) {
        TxResult::Locked
    } else {
        TxResult::Frozen
    }
}

/// `isGlobalFrozen(view, asset)` (TokenHelpers.cpp:46-52): the IOU issuer's
/// lsfGlobalFreeze, or the MPT issuance's lsfMPTLocked (MPTokenHelpers.cpp:
/// 41-56; a missing issuance is not frozen). XRP never.
pub fn is_global_frozen(sandbox: &Sandbox, asset: &HeldAsset) -> bool {
    match asset {
        HeldAsset::Xrp => false,
        HeldAsset::Iou { issuer, .. } => crate::flow::view::is_global_frozen(sandbox, issuer),
        HeldAsset::Mpt(id) => {
            json_at(sandbox, &keylet::mpt_issuance_key(id)).is_some_and(|i| flags(&i) & mpt::LSF_MPT_LOCKED != 0)
        }
    }
}

/// `checkGlobalFrozen(view, asset)` (TokenHelpers.cpp:54-60).
pub fn check_global_frozen(sandbox: &Sandbox, asset: &HeldAsset) -> TxResult {
    if is_global_frozen(sandbox, asset) {
        frozen_code(asset)
    } else {
        TxResult::Success
    }
}

/// `isIndividualFrozen(view, account, asset)` (TokenHelpers.cpp:62-67): the
/// ISSUER's side of the account's trust line carries the freeze bit — never
/// for the issuer itself or without a line (RippleStateHelpers.cpp:107-124)
/// — or the account's MPToken is locked (MPTokenHelpers.cpp:58-72).
pub fn is_individual_frozen(sandbox: &Sandbox, account: &[u8; 20], asset: &HeldAsset) -> bool {
    match asset {
        HeldAsset::Xrp => false,
        HeldAsset::Iou { currency, issuer } => {
            account != issuer
                && json_at(sandbox, &keylet::ripple_state_key(account, issuer, currency))
                    .is_some_and(|line| flags(&line) & (if issuer > account { LSF_HIGH_FREEZE } else { LSF_LOW_FREEZE }) != 0)
        }
        HeldAsset::Mpt(id) => json_at(sandbox, &keylet::mptoken_key(&keylet::mpt_issuance_key(id), account))
            .is_some_and(|token| flags(&token) & mpt::LSF_MPT_LOCKED != 0),
    }
}

/// `checkIndividualFrozen(view, account, asset)` (TokenHelpers.cpp:69-75).
pub fn check_individual_frozen(sandbox: &Sandbox, account: &[u8; 20], asset: &HeldAsset) -> TxResult {
    if is_individual_frozen(sandbox, account, asset) {
        frozen_code(asset)
    } else {
        TxResult::Success
    }
}

/// The last arm of both checks below (TokenHelpers.cpp:205-213, :251-259):
/// a pseudo-account whose asset is itself a vault share frozen through its
/// own vault — rippled's UNREACHABLE (a vault may not hold another vault's
/// shares), tecINTERNAL.
fn pseudo_holds_frozen_shares(sandbox: &Sandbox, pseudo: &[u8; 20], asset: &HeldAsset) -> bool {
    match asset {
        HeldAsset::Mpt(id) => mpt::vault_pseudo_account_frozen(sandbox, pseudo, id),
        _ => false,
    }
}

/// `checkWithdrawFreeze(view, pseudoAcct, submitterAcct, dstAcct, asset)`
/// (TokenHelpers.cpp:160-217), a payout from a pseudo-account on the
/// submitter's behalf:
/// - to the asset's issuer: always allowed;
/// - the asset globally frozen;
/// - the pseudo-account individually frozen;
/// - the submitter individually frozen — only when it is not the
///   destination (a plain freeze does not stop a holder taking its own
///   funds out);
/// - the destination deep-frozen;
///
/// each tecFROZEN (IOU) or tecLOCKED (MPT).
pub fn check_withdraw_freeze(sandbox: &Sandbox, pseudo: &[u8; 20], submitter: &[u8; 20], dst: &[u8; 20], asset: &HeldAsset) -> TxResult {
    if *dst == asset_issuer(asset) {
        return TxResult::Success;
    }
    let global = check_global_frozen(sandbox, asset);
    if global != TxResult::Success {
        return global;
    }
    let source = check_individual_frozen(sandbox, pseudo, asset);
    if source != TxResult::Success {
        return source;
    }
    if submitter != dst {
        let own = check_individual_frozen(sandbox, submitter, asset);
        if own != TxResult::Success {
            return own;
        }
    }
    let deep = check_deep_frozen_individual(sandbox, dst, asset);
    if deep != TxResult::Success {
        return deep;
    }
    if pseudo_holds_frozen_shares(sandbox, pseudo, asset) {
        return TxResult::TecInternal;
    }
    TxResult::Success
}

/// `checkDeepFrozen(view, account, asset)` as [`check_deep_frozen`] has it.
fn check_deep_frozen_individual(sandbox: &Sandbox, account: &[u8; 20], asset: &HeldAsset) -> TxResult {
    check_deep_frozen(sandbox, account, asset)
}

/// `checkDepositFreeze(view, srcAcct, pseudoAcct, asset)`
/// (TokenHelpers.cpp:219-262), a deposit into a pseudo-account: the asset
/// globally frozen; the depositor individually frozen unless it is the
/// issuer; the pseudo-account individually frozen ("pseudo-accounts cannot
/// receive deposits under a regular freeze because those funds cannot be
/// later withdrawn") — tecFROZEN (IOU) or tecLOCKED (MPT).
pub fn check_deposit_freeze(sandbox: &Sandbox, src: &[u8; 20], pseudo: &[u8; 20], asset: &HeldAsset) -> TxResult {
    let global = check_global_frozen(sandbox, asset);
    if global != TxResult::Success {
        return global;
    }
    if *src != asset_issuer(asset) {
        let own = check_individual_frozen(sandbox, src, asset);
        if own != TxResult::Success {
            return own;
        }
    }
    let destination = check_individual_frozen(sandbox, pseudo, asset);
    if destination != TxResult::Success {
        return destination;
    }
    if pseudo_holds_frozen_shares(sandbox, pseudo, asset) {
        return TxResult::TecInternal;
    }
    TxResult::Success
}

/// `canTransfer(view, asset, from, to, waive)` (TokenHelpers.cpp:659-673)
/// with an explicit `WaiveMPTCanTransfer`: waived, an MPT moves between
/// holders whatever lsfMPTCanTransfer says and without consulting a
/// reference holding (MPTokenHelpers.cpp:617-618) — the missing issuance is
/// still tecOBJECT_NOT_FOUND. An IOU's rule is never waived.
pub fn can_transfer_waive(sandbox: &Sandbox, asset: &HeldAsset, from: &[u8; 20], to: &[u8; 20], waive: bool) -> TxResult {
    match asset {
        HeldAsset::Mpt(id) if waive => match json_at(sandbox, &keylet::mpt_issuance_key(id)) {
            Some(_) => TxResult::Success,
            None => TxResult::ObjectNotFound,
        },
        _ => can_transfer(sandbox, asset, from, to),
    }
}

/// `accountHolds(…, SpendableHandling::FullBalance)` (TokenHelpers.cpp:
/// 366-497): what the account could spend, not just what it holds.
/// - XRP: [`xrp_liquid`], as ever (:386-389);
/// - the IOU's issuer: `STAmount::kMaxValue` at `kMaxOffset`,
///   9999999999999999e80 (:392-397);
/// - an IOU holder: the line's balance in its terms plus the OPPOSITE limit
///   — the one the issuer set on its side (:343-346) — or zero without a
///   usable line (the limit is not added to a missing or frozen line);
/// - the MPT's issuer: MaximumAmount less OutstandingAmount, zero without
///   the issuance (:423-438);
/// - an MPT holder: the simple balance.
///
/// The `Err` is the IOU addition's overflow (rippled throws).
pub fn account_holds_full(
    sandbox: &Sandbox,
    account: &[u8; 20],
    asset: &HeldAsset,
    zero_if_frozen: bool,
    zero_if_unauthorized: bool,
) -> Result<Amount, crate::flow::st_amount::StError> {
    match asset {
        HeldAsset::Xrp => Ok(account_holds(sandbox, account, asset, zero_if_frozen, zero_if_unauthorized)),
        HeldAsset::Iou { currency, issuer } => {
            if account == issuer {
                return Ok(Amount { asset: *asset, negative: false, mantissa: 9_999_999_999_999_999, exponent: 80 });
            }
            let simple = account_holds(sandbox, account, asset, zero_if_frozen, zero_if_unauthorized);
            let view_asset = iou_asset(currency, issuer);
            let Some(line) = json_at(sandbox, &keylet::ripple_state_key(account, issuer, currency)) else { return Ok(simple) };
            if zero_if_frozen
                && (crate::flow::view::is_frozen(sandbox, account, &view_asset)
                    || crate::flow::view::is_deep_frozen(sandbox, account, &view_asset))
            {
                return Ok(simple);
            }
            let opposite = if account > issuer { "LowLimit" } else { "HighLimit" };
            let (negative, magnitude) = crate::tx::offer::signed_value(&line[opposite]);
            let limit = Amount::new(*asset, magnitude.0, magnitude.1, negative, crate::tx::number::Rounding::ToNearest)?;
            simple.iou_add(limit, crate::tx::number::Rounding::ToNearest)
        }
        HeldAsset::Mpt(id) => {
            if *account != asset_issuer(asset) {
                return Ok(account_holds(sandbox, account, asset, zero_if_frozen, zero_if_unauthorized));
            }
            let Some(issuance) = json_at(sandbox, &keylet::mpt_issuance_key(id)) else { return Ok(Amount::zero(*asset)) };
            let max = issuance.get("MaximumAmount").map(|_| mpt::dec_field(&issuance, "MaximumAmount")).unwrap_or(mpt::MAX_MPT_AMOUNT);
            let available = max.saturating_sub(mpt::dec_field(&issuance, "OutstandingAmount"));
            Ok(Amount { asset: *asset, negative: false, mantissa: available, exponent: 0 })
        }
    }
}

/// `QUALITY_ONE`: a transfer rate of exactly one, no fee.
pub const PARITY_RATE: u64 = 1_000_000_000;

/// `transferRate(view, asset)` (3.5.0-rc1 TokenHelpers.cpp:541-547): the
/// IOU issuer's TransferRate, 1e9 + 10,000 × the MPT issuance's TransferFee,
/// parity for XRP or when neither is set.
pub fn asset_transfer_rate(sandbox: &Sandbox, asset: &HeldAsset) -> u64 {
    match asset {
        HeldAsset::Xrp => PARITY_RATE,
        HeldAsset::Iou { issuer, .. } => crate::flow::view::transfer_rate(sandbox, issuer) as u64,
        HeldAsset::Mpt(id) => json_at(sandbox, &keylet::mpt_issuance_key(id))
            .map(|issuance| mpt::issuance_transfer_rate(&issuance))
            .unwrap_or(PARITY_RATE),
    }
}

/// `mulRatio(STAmount, num, den, rounding)` (3.5.0-rc1 STAmount.cpp:
/// 1766-1782): the amount times `num / den`, exact, rounded up or down —
/// `mulRatio(IOUAmount)` for an IOU, 128-bit integer arithmetic for XRP and
/// MPT (XRPAmount.h:269-292, MPTAmount.h:154-176; past int64 is the throw).
fn mul_ratio(amount: &Amount, num: u64, den: u64, round_up: bool) -> Result<Amount, crate::flow::st_amount::StError> {
    use crate::flow::st_amount::StError;
    if den == 0 {
        return Err(StError::DivideByZero);
    }
    if let HeldAsset::Iou { .. } = amount.asset {
        let iou = crate::flow::amounts::IouAmount { negative: amount.negative, mantissa: amount.mantissa, exponent: amount.exponent };
        let out = crate::flow::view::mul_ratio_iou(iou, num as u32, den as u32, round_up);
        if out.mantissa == 0 {
            return Ok(Amount::zero(amount.asset));
        }
        return Ok(Amount { asset: amount.asset, negative: out.negative, mantissa: out.mantissa, exponent: out.exponent });
    }
    let negative = amount.negative && !amount.is_zero();
    let product = amount.mantissa as u128 * num as u128;
    let mut quotient = product / den as u128;
    // Signed division truncates towards zero: up adds one to a positive
    // quotient, down adds one to a negative one's magnitude.
    if !product.is_multiple_of(den as u128) && (negative != round_up) {
        quotient += 1;
    }
    if quotient > i64::MAX as u128 {
        return Err(StError::Overflow);
    }
    let limit = if matches!(amount.asset, HeldAsset::Xrp) { 100_000_000_000_000_000u128 } else { mpt::MAX_MPT_AMOUNT as u128 };
    if quotient > limit {
        return Err(StError::Overflow);
    }
    Ok(Amount { asset: amount.asset, negative: negative && quotient != 0, mantissa: quotient as u64, exponent: 0 })
}

/// `addTransferFee(amount, rate)` (3.5.0-rc1 TokenHelpers.cpp:555-561): what
/// a sender must part with for the receiver to get `amount` — the amount
/// times rate / 1e9, rounded UP; the amount itself at parity.
pub fn add_transfer_fee(amount: &Amount, rate: u64) -> Result<Amount, crate::flow::st_amount::StError> {
    if rate == PARITY_RATE {
        return Ok(*amount);
    }
    mul_ratio(amount, rate, PARITY_RATE, true)
}

/// `subtractTransferFee(amount, rate)` (3.5.0-rc1 TokenHelpers.cpp:563-569):
/// what arrives of `amount` sent — times 1e9 / rate, rounded DOWN.
pub fn subtract_transfer_fee(amount: &Amount, rate: u64) -> Result<Amount, crate::flow::st_amount::StError> {
    if rate == PARITY_RATE {
        return Ok(*amount);
    }
    mul_ratio(amount, PARITY_RATE, rate, false)
}

/// `isRepresentable(asset, value)` (3.5.0-rc1 STAmount.cpp:448-462):
/// whether `STAmount{asset, value}` constructs at all — false where the
/// Number, the IOU exponent or the XRP / MPT range throws.
pub fn is_representable(asset: &HeldAsset, value: crate::tx::number::Number) -> bool {
    Amount::from_number(*asset, value, crate::tx::number::Rounding::ToNearest).is_ok()
}

/// `doWithdraw(ctx, senderAcct, dstAcct, sourceAcct, priorBalance,
/// destinationAmount, sourceAmount)` (3.5.0-rc1 View.cpp:507-588) — 3.4.1's
/// `doWithdraw` (View.cpp:535-598) is this with the two amounts equal:
///
/// - to oneself: an empty holding is added first when something is
///   DELIVERED — or always, before fixCleanup3_4_0 — an existing one
///   (tecDUPLICATE) being fine;
/// - to someone else: `verifyDepositPreauth`;
/// - `source` must hold the SOURCE amount (freezes and authorization
///   ignored): tefINTERNAL otherwise;
/// - the amounts differ (a transfer fee, fixCleanup3_5_0): the gross goes
///   source → issuer and the net issuer → destination, each one direct leg
///   (`directSendNoFee`; an `account_send` with the issuer at one end is
///   exactly that leg);
/// - else `accountSend(source → dst, amount, WaiveTransferFee::Yes)`.
#[allow(clippy::too_many_arguments)]
pub fn do_withdraw_split(
    sandbox: &mut Sandbox,
    tx: &TxFields,
    sender: &[u8; 20],
    dst: &[u8; 20],
    source: &[u8; 20],
    prior_balance: u64,
    destination_amount: &Amount,
    source_amount: &Amount,
) -> TxResult {
    if dst == sender {
        let positive = !destination_amount.is_zero() && !destination_amount.negative;
        if positive || !amendments::fix_cleanup_3_4_0(sandbox) {
            let added = add_empty_holding(sandbox, sender, prior_balance, &destination_amount.asset);
            if added != TxResult::Success && added != TxResult::Duplicate {
                return added;
            }
        }
    } else {
        // verifyDepositPreauth with a null destination SLE is tecINTERNAL
        // in rippled's callers' terms; canWithdraw answered tecNO_DST in
        // preclaim, so the root is there.
        let Some(dst_root) = root(sandbox, dst) else { return TxResult::TecInternal };
        let verdict = crate::tx::misc::verify_deposit_preauth(sandbox, tx, sender, dst, &dst_root);
        if verdict != TxResult::Success {
            return verdict;
        }
    }
    let held = account_holds(sandbox, source, &destination_amount.asset, false, false);
    if held.number() < source_amount.number() {
        return TxResult::TefInternal;
    }
    if source_amount != destination_amount {
        let issuer = asset_issuer(&destination_amount.asset);
        let redeemed = account_send(sandbox, source, &issuer, source_amount, true);
        if redeemed != TxResult::Success {
            return redeemed;
        }
        return account_send(sandbox, &issuer, dst, destination_amount, true);
    }
    account_send(sandbox, source, dst, destination_amount, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::amendments as am;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::state::LedgerState;
    use crate::tx::number::{MantissaScale, ScaleGuard};
    use serde_json::{json, Value};

    const ALICE: [u8; 20] = [0x11; 20];
    const BOB: [u8; 20] = [0x12; 20];
    /// Issues USD; sorts above ALICE, BOB and below PSEUDO.
    const ISSUER: [u8; 20] = [0x21; 20];
    /// A vault's pseudo-account.
    const PSEUDO: [u8; 20] = [0x31; 20];
    const USD_CODE: [u8; 20] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x55, 0x53, 0x44, 0, 0, 0, 0, 0];
    const USD: HeldAsset = HeldAsset::Iou { currency: USD_CODE, issuer: ISSUER };
    /// Mainnet today plus the vault amendments and fixCleanup3_1_3.
    const ON: [&str; 6] = [
        am::FIX_CLEANUP_3_1_3, am::FIX_CLEANUP_3_2_0, am::FIX_CLEANUP_3_3_0, am::FIX_CLEANUP_3_4_0, am::SINGLE_ASSET_VAULT,
        am::LENDING_PROTOCOL,
    ];
    const BALANCE: u64 = 50_000_000;

    fn put(state: &mut LedgerState, key: Hash256, v: &Value) {
        state.state_map.insert(key, serde_json::to_vec(v).unwrap()).unwrap();
    }

    fn root_in(state: &mut LedgerState, id: [u8; 20], flags: u64, extra: Value) {
        let mut a = json!({
            "LedgerEntryType": "AccountRoot", "Account": hex::encode(id), "Balance": BALANCE.to_string(),
            "Sequence": 1, "OwnerCount": 0, "Flags": flags,
        });
        for (k, v) in extra.as_object().unwrap() {
            a[k] = v.clone();
        }
        put(state, keylet::account_root_key(&id), &a);
    }

    fn state_with(amendments: &[&str]) -> LedgerState {
        let mut state = LedgerState::new_unverified(LedgerHeader {
            sequence: 100, total_coins: 100_000_000_000_000_000,
            parent_hash: Hash256([0; 32]), transaction_hash: Hash256([0; 32]), account_hash: Hash256([0; 32]),
            parent_close_time: 0, close_time: 1000, close_time_resolution: 10, close_flags: 0,
        });
        put(&mut state, keylet::amendments_key(), &json!({"LedgerEntryType": "Amendments", "Flags": 0, "Amendments": amendments}));
        root_in(&mut state, ALICE, 0, json!({}));
        root_in(&mut state, BOB, 0, json!({}));
        root_in(&mut state, ISSUER, LSF_DEFAULT_RIPPLE, json!({}));
        root_in(&mut state, PSEUDO, 0x0190_0000, json!({"Sequence": 0, "Balance": "0", "VaultID": "84".repeat(32)}));
        state
    }

    fn state() -> LedgerState {
        state_with(&ON)
    }

    fn mptid(seq: u32, issuer: [u8; 20]) -> [u8; 24] {
        let mut id = [0u8; 24];
        id[..4].copy_from_slice(&seq.to_be_bytes());
        id[4..].copy_from_slice(&issuer);
        id
    }

    /// An issuance by ISSUER (sequence 7), linked into nothing.
    fn issuance(state: &mut LedgerState, flags: u64, extra: Value) -> [u8; 24] {
        let id = mptid(7, ISSUER);
        let mut o = json!({
            "LedgerEntryType": "MPTokenIssuance", "Issuer": hex::encode(ISSUER), "Flags": flags, "Sequence": 7,
            "OutstandingAmount": "1000", "OwnerNode": "0",
        });
        for (k, v) in extra.as_object().unwrap() {
            o[k] = v.clone();
        }
        put(state, keylet::mpt_issuance_key(&id), &o);
        id
    }

    fn token(state: &mut LedgerState, id: &[u8; 24], holder: [u8; 20], flags: u64, amount: u64) {
        let mut t = json!({
            "LedgerEntryType": "MPToken", "Account": hex::encode(holder), "MPTokenIssuanceID": hex::encode_upper(id),
            "Flags": flags, "OwnerNode": "0",
        });
        if amount != 0 {
            t["MPTAmount"] = json!(amount.to_string());
        }
        put(state, keylet::mptoken_key(&keylet::mpt_issuance_key(id), &holder), &t);
    }

    /// A USD line between `holder` (low: every holder but PSEUDO sorts below
    /// ISSUER) and ISSUER, the holder holding `balance`.
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

    fn get(sb: &Sandbox, key: &Hash256) -> Option<Value> {
        json_at(sb, key)
    }

    fn owners(sb: &Sandbox, id: &[u8; 20]) -> u64 {
        owner_count(&root(sb, id).unwrap())
    }

    fn dir_has(sb: &Sandbox, owner: &[u8; 20], key: &Hash256) -> bool {
        get(sb, &keylet::owner_dir_key(owner))
            .is_some_and(|d| d["Indexes"].as_array().is_some_and(|a| a.iter().any(|k| k == &json!(hex::encode_upper(key.0)))))
    }

    fn usd(value: &str) -> Amount {
        Amount::from_json(&json!({"currency": hex::encode_upper(USD_CODE), "issuer": hex::encode(ISSUER), "value": value})).unwrap()
    }

    fn drops(value: u64) -> Amount {
        Amount { asset: HeldAsset::Xrp, negative: false, mantissa: value, exponent: 0 }
    }

    fn units(id: &[u8; 24], value: u64) -> Amount {
        Amount { asset: HeldAsset::Mpt(*id), negative: false, mantissa: value, exponent: 0 }
    }

    fn large() -> ScaleGuard {
        ScaleGuard::new(MantissaScale::Large330)
    }

    /// View.cpp:48-63, and inventory Q11's building block.
    #[test]
    fn has_expired_is_inclusive_unless_asked() {
        let st = state(); // parent close time 1000
        let sb = Sandbox::new(&st);
        for (exp, inclusive, exclusive) in [(999u32, true, true), (1000, true, false), (1001, false, false)] {
            assert_eq!(has_expired(&sb, Some(exp), ExpiryComparison::Inclusive), inclusive, "{exp}");
            assert_eq!(has_expired(&sb, Some(exp), ExpiryComparison::Exclusive), exclusive, "{exp}");
        }
        assert!(!has_expired(&sb, None, ExpiryComparison::Inclusive));
        assert!(!has_expired(&sb, None, ExpiryComparison::Exclusive));
    }

    /// AccountRootHelpers.cpp:359-376.
    #[test]
    fn decrease_owner_count_for_object_gives_the_count_back() {
        let mut st = state();
        root_in(&mut st, ALICE, 0, json!({"OwnerCount": 5}));
        let mut sb = Sandbox::new(&st);
        decrease_owner_count_for_object(&mut sb, &ALICE, &json!({"LedgerEntryType": "Vault"}), 2);
        assert_eq!(owners(&sb, &ALICE), 3);
        decrease_owner_count_for_object(&mut sb, &ALICE, &json!({"LedgerEntryType": "AccountRoot"}), 1);
        decrease_owner_count_for_object(&mut sb, &ALICE, &json!({"LedgerEntryType": "MPToken"}), 0);
        assert_eq!(owners(&sb, &ALICE), 3);
    }

    /// TokenHelpers.cpp:85-160: tecFROZEN for an IOU, tecLOCKED for an MPT,
    /// never XRP; the deep check is the IOU line's deep freeze and, for an
    /// MPT, the same lock.
    #[test]
    fn check_frozen_and_check_deep_frozen_answer_by_asset() {
        const LSF_HIGH_FREEZE: u64 = 0x0080_0000;
        const LSF_LOW_DEEP_FREEZE: u64 = 0x0200_0000;
        const LSF_HIGH_DEEP_FREEZE: u64 = 0x0400_0000;
        const LSF_GLOBAL_FREEZE: u64 = 0x0040_0000;
        let mut st = state();
        line(&mut st, ALICE, LSF_HIGH_FREEZE, "50", "1000");
        line(&mut st, BOB, 0, "50", "1000");
        let locked = issuance(&mut st, mpt::LSF_MPT_CAN_TRANSFER, json!({}));
        token(&mut st, &locked, ALICE, mpt::LSF_MPT_LOCKED, 5);
        token(&mut st, &locked, BOB, 0, 5);
        let sb = Sandbox::new(&st);
        assert_eq!(check_frozen(&sb, &ALICE, &HeldAsset::Xrp), TxResult::Success);
        assert_eq!(check_deep_frozen(&sb, &ALICE, &HeldAsset::Xrp), TxResult::Success);
        assert_eq!(check_frozen(&sb, &ALICE, &USD), TxResult::Frozen, "the issuer froze ALICE's line");
        assert_eq!(check_frozen(&sb, &BOB, &USD), TxResult::Success);
        assert_eq!(check_frozen(&sb, &ISSUER, &USD), TxResult::Success);
        assert_eq!(check_deep_frozen(&sb, &ALICE, &USD), TxResult::Success, "a plain freeze is not a deep one");
        assert_eq!(check_frozen(&sb, &ALICE, &HeldAsset::Mpt(locked)), TxResult::Locked);
        assert_eq!(check_deep_frozen(&sb, &ALICE, &HeldAsset::Mpt(locked)), TxResult::Locked);
        assert_eq!(check_frozen(&sb, &BOB, &HeldAsset::Mpt(locked)), TxResult::Success);
        assert_eq!(check_deep_frozen(&sb, &BOB, &HeldAsset::Mpt(locked)), TxResult::Success);
        assert!(is_frozen(&sb, &ALICE, &USD) && !is_frozen(&sb, &BOB, &USD));
        // A missing issuance is not frozen (MPTokenHelpers.cpp:42-47).
        assert_eq!(check_frozen(&sb, &ALICE, &HeldAsset::Mpt(mptid(99, ISSUER))), TxResult::Success);

        for deep in [LSF_LOW_DEEP_FREEZE, LSF_HIGH_DEEP_FREEZE] {
            let mut st = state();
            line(&mut st, ALICE, deep, "50", "1000");
            let sb = Sandbox::new(&st);
            assert_eq!(check_deep_frozen(&sb, &ALICE, &USD), TxResult::Frozen);
            assert_eq!(check_deep_frozen(&sb, &ISSUER, &USD), TxResult::Success, "never the issuer");
            assert_eq!(check_deep_frozen(&sb, &BOB, &USD), TxResult::Success, "no line, not deep frozen");
        }
        let mut st = state();
        root_in(&mut st, ISSUER, LSF_DEFAULT_RIPPLE | LSF_GLOBAL_FREEZE, json!({}));
        let sb = Sandbox::new(&st);
        assert_eq!(check_frozen(&sb, &BOB, &USD), TxResult::Frozen, "a global freeze needs no line");
    }

    /// TokenHelpers.cpp:649-673: the asset dispatch of requireAuth and
    /// canTransfer; each arm's own rules are tested where it lives
    /// (`tx::offer`, `tx::mpt`).
    #[test]
    fn require_auth_and_can_transfer_dispatch_on_the_asset() {
        const LSF_REQUIRE_AUTH: u64 = 0x0004_0000;
        const LSF_HIGH_AUTH: u64 = 0x0008_0000;
        let mut st = state();
        root_in(&mut st, ISSUER, LSF_REQUIRE_AUTH, json!({}));
        line(&mut st, ALICE, LSF_HIGH_AUTH, "50", "1000");
        line(&mut st, BOB, LSF_HIGH_NO_RIPPLE, "50", "1000");
        let needs_auth = issuance(&mut st, mpt::LSF_MPT_REQUIRE_AUTH, json!({}));
        token(&mut st, &needs_auth, ALICE, mpt::LSF_MPT_AUTHORIZED, 0);
        token(&mut st, &needs_auth, BOB, 0, 0);
        let sb = Sandbox::new(&st);
        for auth in [AuthType::Legacy, AuthType::WeakAuth, AuthType::StrongAuth] {
            assert_eq!(require_auth(&sb, &HeldAsset::Xrp, &ALICE, auth), TxResult::Success);
            assert_eq!(require_auth(&sb, &USD, &ALICE, auth), TxResult::Success);
            assert_eq!(require_auth(&sb, &USD, &BOB, auth), TxResult::NoAuth);
            assert_eq!(require_auth(&sb, &USD, &ISSUER, auth), TxResult::Success);
            assert_eq!(require_auth(&sb, &HeldAsset::Mpt(needs_auth), &ALICE, auth), TxResult::Success);
            assert_eq!(require_auth(&sb, &HeldAsset::Mpt(needs_auth), &BOB, auth), TxResult::NoAuth);
            assert_eq!(require_auth(&sb, &HeldAsset::Mpt(mptid(99, ISSUER)), &BOB, auth), TxResult::ObjectNotFound);
        }
        // No line at all: tecNO_LINE whatever the strength, the issuer requiring auth.
        assert_eq!(require_auth(&sb, &USD, &PSEUDO, AuthType::StrongAuth), TxResult::NoLine);
        assert_eq!(require_auth(&sb, &USD, &PSEUDO, AuthType::WeakAuth), TxResult::NoLine);

        // canTransfer: the issuer has no DefaultRipple and BOB's line has the
        // issuer's NoRipple; ALICE's does not.
        assert_eq!(can_transfer(&sb, &HeldAsset::Xrp, &ALICE, &BOB), TxResult::Success);
        assert_eq!(can_transfer(&sb, &USD, &ALICE, &BOB), TxResult::Success, "rippling is on at ALICE's line");
        assert_eq!(can_transfer(&sb, &USD, &BOB, &PSEUDO), TxResult::NoRipple, "off on both");
        assert_eq!(can_transfer(&sb, &USD, &BOB, &ISSUER), TxResult::Success);
        // The MPT is not transferable: only the issuer's own moves pass.
        assert_eq!(can_transfer(&sb, &HeldAsset::Mpt(needs_auth), &ALICE, &BOB), TxResult::NoAuth);
        assert_eq!(can_transfer(&sb, &HeldAsset::Mpt(needs_auth), &ISSUER, &BOB), TxResult::Success);
        assert_eq!(can_transfer(&sb, &HeldAsset::Mpt(mptid(99, ISSUER)), &ALICE, &BOB), TxResult::ObjectNotFound);
    }

    /// TokenHelpers.cpp:558-613, MPTokenHelpers.cpp:159-173.
    #[test]
    fn can_add_holding_and_holding_exists() {
        let mut st = state();
        let transferable = issuance(&mut st, mpt::LSF_MPT_CAN_TRANSFER, json!({}));
        line(&mut st, ALICE, 0, "0", "0");
        token(&mut st, &transferable, ALICE, 0, 0);
        let sb = Sandbox::new(&st);
        assert_eq!(can_add_holding(&sb, &HeldAsset::Xrp), TxResult::Success);
        assert_eq!(can_add_holding(&sb, &USD), TxResult::Success);
        assert_eq!(can_add_holding(&sb, &HeldAsset::Iou { currency: USD_CODE, issuer: [0x77; 20] }), TxResult::NoAccount);
        assert_eq!(TxResult::NoAccount.code_str(), "terNO_ACCOUNT");
        assert_eq!(can_add_holding(&sb, &HeldAsset::Iou { currency: USD_CODE, issuer: ALICE }), TxResult::NoRipple, "ALICE has no DefaultRipple");
        assert_eq!(TxResult::NoRipple.code_str(), "terNO_RIPPLE");
        assert_eq!(can_add_holding(&sb, &HeldAsset::Mpt(transferable)), TxResult::Success);
        assert_eq!(can_add_holding(&sb, &HeldAsset::Mpt(mptid(99, ISSUER))), TxResult::ObjectNotFound);
        for (who, usd_holding, mpt_holding) in [(ALICE, true, true), (BOB, false, false), (ISSUER, true, true)] {
            assert!(holding_exists(&sb, &who, &HeldAsset::Xrp));
            assert_eq!(holding_exists(&sb, &who, &USD), usd_holding);
            assert_eq!(holding_exists(&sb, &who, &HeldAsset::Mpt(transferable)), mpt_holding);
        }
        let mut st = state();
        let fixed = issuance(&mut st, 0, json!({}));
        assert_eq!(can_add_holding(&Sandbox::new(&st), &HeldAsset::Mpt(fixed)), TxResult::NoAuth);
    }

    /// RippleStateHelpers.cpp:642-720, every result in rippled's order, and
    /// the line `trustCreate` writes.
    #[test]
    fn add_empty_holding_creates_an_empty_trust_line() {
        // XRP and the issuer itself: nothing to do.
        let st = state();
        let mut sb = Sandbox::new(&st);
        assert_eq!(add_empty_holding(&mut sb, &ALICE, BALANCE, &HeldAsset::Xrp), TxResult::Success);
        assert_eq!(add_empty_holding(&mut sb, &ISSUER, BALANCE, &USD), TxResult::Success);
        assert_eq!(sb.modification_count(), 0);

        // The line: for a holder below the issuer (ALICE, the low side) and
        // above it (PSEUDO, the high side).
        for (holder, reserve, no_ripple) in [(ALICE, LSF_LOW_RESERVE, LSF_LOW_NO_RIPPLE), (PSEUDO, LSF_HIGH_RESERVE, LSF_HIGH_NO_RIPPLE)] {
            let mut sb = Sandbox::new(&st);
            assert_eq!(add_empty_holding(&mut sb, &holder, BALANCE, &USD), TxResult::Success);
            let key = keylet::ripple_state_key(&holder, &ISSUER, &USD_CODE);
            let (low, high) = if holder < ISSUER { (holder, ISSUER) } else { (ISSUER, holder) };
            let cur = hex::encode_upper(USD_CODE);
            assert_eq!(get(&sb, &key).unwrap(), json!({
                "LedgerEntryType": "RippleState", "Flags": reserve | no_ripple,
                "Balance": {"currency": cur, "issuer": "0000000000000000000000000000000000000001", "value": "0"},
                "LowLimit": {"currency": cur, "issuer": hex::encode(low), "value": "0"},
                "HighLimit": {"currency": cur, "issuer": hex::encode(high), "value": "0"},
                "LowNode": "0", "HighNode": "0",
            }));
            assert!(dir_has(&sb, &holder, &key) && dir_has(&sb, &ISSUER, &key));
            assert_eq!((owners(&sb, &holder), owners(&sb, &ISSUER)), (1, 0), "only the holder is charged");
            // Again: the line exists.
            assert_eq!(add_empty_holding(&mut sb, &holder, BALANCE, &USD), TxResult::Duplicate);
        }

        // tecFROZEN: a globally frozen issuer.
        let mut frozen = state();
        root_in(&mut frozen, ISSUER, LSF_DEFAULT_RIPPLE | 0x0040_0000, json!({}));
        assert_eq!(add_empty_holding(&mut Sandbox::new(&frozen), &ALICE, BALANCE, &USD), TxResult::Frozen);
        // …but under fixCleanup3_4_0 an existing line answers first.
        line(&mut frozen, ALICE, 0, "0", "0");
        assert_eq!(add_empty_holding(&mut Sandbox::new(&frozen), &ALICE, BALANCE, &USD), TxResult::Duplicate);
        let mut frozen_old = state_with(&[am::SINGLE_ASSET_VAULT]);
        root_in(&mut frozen_old, ISSUER, LSF_DEFAULT_RIPPLE | 0x0040_0000, json!({}));
        line(&mut frozen_old, ALICE, 0, "0", "0");
        assert_eq!(add_empty_holding(&mut Sandbox::new(&frozen_old), &ALICE, BALANCE, &USD), TxResult::Frozen, "before the fix the freeze is first");

        // tefINTERNAL: a missing root on either side.
        let missing = HeldAsset::Iou { currency: USD_CODE, issuer: [0x77; 20] };
        assert_eq!(add_empty_holding(&mut Sandbox::new(&st), &ALICE, BALANCE, &missing), TxResult::TefInternal);
        assert_eq!(add_empty_holding(&mut Sandbox::new(&st), &[0x78; 20], BALANCE, &USD), TxResult::TefInternal);

        // An issuer without DefaultRipple: terNO_RIPPLE with fixCleanup3_4_0, tecINTERNAL before.
        let bobs = HeldAsset::Iou { currency: USD_CODE, issuer: BOB };
        assert_eq!(add_empty_holding(&mut Sandbox::new(&st), &ALICE, BALANCE, &bobs), TxResult::NoRipple);
        let old = state_with(&[am::SINGLE_ASSET_VAULT]);
        assert_eq!(add_empty_holding(&mut Sandbox::new(&old), &ALICE, BALANCE, &bobs), TxResult::TecInternal);
        // Before the fix an existing line is tecDUPLICATE after those tests.
        let mut old_line = state_with(&[am::SINGLE_ASSET_VAULT]);
        line(&mut old_line, ALICE, 0, "0", "0");
        assert_eq!(add_empty_holding(&mut Sandbox::new(&old_line), &ALICE, BALANCE, &USD), TxResult::Duplicate);

        // tecNO_LINE_INSUF_RESERVE: the reserve for ONE more object, from the first (no free tier here).
        let need = crate::ledger::fees::account_reserve(&Sandbox::new(&st), 1);
        assert_eq!(add_empty_holding(&mut Sandbox::new(&st), &ALICE, need - 1, &USD), TxResult::NoLineInsufReserve);
        assert_eq!(add_empty_holding(&mut Sandbox::new(&st), &ALICE, need, &USD), TxResult::Success);
    }

    /// MPTokenHelpers.cpp:176-197.
    #[test]
    fn add_empty_holding_creates_an_mptoken() {
        let mut st = state();
        let id = issuance(&mut st, mpt::LSF_MPT_CAN_TRANSFER, json!({}));
        let asset = HeldAsset::Mpt(id);
        let mut sb = Sandbox::new(&st);
        assert_eq!(add_empty_holding(&mut sb, &ISSUER, BALANCE, &asset), TxResult::Success, "the issuer holds no token");
        assert_eq!(sb.modification_count(), 0);
        assert_eq!(add_empty_holding(&mut sb, &PSEUDO, 0, &asset), TxResult::Success, "a pseudo-account's first object needs no reserve");
        let key = keylet::mptoken_key(&keylet::mpt_issuance_key(&id), &PSEUDO);
        assert_eq!(get(&sb, &key).unwrap(), json!({
            "LedgerEntryType": "MPToken", "Account": hex::encode(PSEUDO), "MPTokenIssuanceID": hex::encode_upper(id),
            "Flags": 0, "OwnerNode": "0",
        }));
        assert!(dir_has(&sb, &PSEUDO, &key));
        assert_eq!(owners(&sb, &PSEUDO), 1);
        assert_eq!(add_empty_holding(&mut sb, &PSEUDO, 0, &asset), TxResult::Duplicate);

        // tefINTERNAL: no issuance, or a locked one — even with a token already there.
        assert_eq!(add_empty_holding(&mut Sandbox::new(&st), &ALICE, BALANCE, &HeldAsset::Mpt(mptid(99, ISSUER))), TxResult::TefInternal);
        let mut locked = state();
        let lid = issuance(&mut locked, mpt::LSF_MPT_LOCKED, json!({}));
        token(&mut locked, &lid, ALICE, 0, 0);
        assert_eq!(add_empty_holding(&mut Sandbox::new(&locked), &ALICE, BALANCE, &HeldAsset::Mpt(lid)), TxResult::TefInternal);
    }

    /// RippleStateHelpers.cpp:723-791, MPTokenHelpers.cpp:332-363.
    #[test]
    fn remove_empty_holding_takes_an_empty_holding_away() {
        // XRP: the balance must be zero.
        let st = state();
        assert_eq!(remove_empty_holding(&mut Sandbox::new(&st), &ALICE, &HeldAsset::Xrp), TxResult::HasObligations);
        assert_eq!(remove_empty_holding(&mut Sandbox::new(&st), &PSEUDO, &HeldAsset::Xrp), TxResult::Success);
        assert_eq!(remove_empty_holding(&mut Sandbox::new(&st), &[0x78; 20], &HeldAsset::Xrp), TxResult::TecInternal);

        // IOU: what add_empty_holding made comes off again, on either side.
        for holder in [ALICE, PSEUDO] {
            let mut sb = Sandbox::new(&st);
            assert_eq!(add_empty_holding(&mut sb, &holder, BALANCE, &USD), TxResult::Success);
            let key = keylet::ripple_state_key(&holder, &ISSUER, &USD_CODE);
            assert_eq!(remove_empty_holding(&mut sb, &holder, &USD), TxResult::Success);
            assert!(get(&sb, &key).is_none());
            assert!(!dir_has(&sb, &holder, &key) && !dir_has(&sb, &ISSUER, &key));
            assert_eq!((owners(&sb, &holder), owners(&sb, &ISSUER)), (0, 0));
            assert_eq!(remove_empty_holding(&mut sb, &holder, &USD), TxResult::ObjectNotFound);
        }
        assert_eq!(remove_empty_holding(&mut Sandbox::new(&st), &ISSUER, &USD), TxResult::Success, "the issuer has no line to itself");

        // A balance is an obligation; both reserve flags release both owners;
        // the erased line carries the cleared flags.
        let mut st = state();
        root_in(&mut st, ALICE, 0, json!({"OwnerCount": 3}));
        root_in(&mut st, ISSUER, LSF_DEFAULT_RIPPLE, json!({"OwnerCount": 4}));
        let held = line(&mut st, BOB, LSF_LOW_RESERVE, "5", "100");
        let both = line(&mut st, ALICE, LSF_LOW_RESERVE | LSF_HIGH_RESERVE | LSF_LOW_NO_RIPPLE, "0", "100");
        let mut sb = Sandbox::new(&st);
        assert_eq!(remove_empty_holding(&mut sb, &BOB, &USD), TxResult::HasObligations);
        assert!(get(&sb, &held).is_some());
        assert_eq!(remove_empty_holding(&mut sb, &ALICE, &USD), TxResult::Success);
        assert_eq!((owners(&sb, &ALICE), owners(&sb, &ISSUER)), (2, 3));
        let erased: Value = serde_json::from_slice(sb.deleted_content(&both).unwrap()).unwrap();
        assert_eq!(erased["Flags"], json!(LSF_LOW_NO_RIPPLE));

        // MPT.
        let mut st = state();
        let id = issuance(&mut st, mpt::LSF_MPT_CAN_TRANSFER, json!({}));
        let asset = HeldAsset::Mpt(id);
        let mut sb = Sandbox::new(&st);
        assert_eq!(remove_empty_holding(&mut sb, &ALICE, &asset), TxResult::ObjectNotFound);
        assert_eq!(remove_empty_holding(&mut sb, &ISSUER, &asset), TxResult::Success);
        assert_eq!(add_empty_holding(&mut sb, &ALICE, BALANCE, &asset), TxResult::Success);
        let key = keylet::mptoken_key(&keylet::mpt_issuance_key(&id), &ALICE);
        assert_eq!(remove_empty_holding(&mut sb, &ALICE, &asset), TxResult::Success);
        assert!(get(&sb, &key).is_none() && !dir_has(&sb, &ALICE, &key));
        assert_eq!(owners(&sb, &ALICE), 0);

        token(&mut st, &id, ALICE, 0, 9);
        assert_eq!(remove_empty_holding(&mut Sandbox::new(&st), &ALICE, &asset), TxResult::HasObligations);
        // A LockedAmount counts under fixCleanup3_1_3 only.
        for (amendments, want) in [(&ON[..], TxResult::HasObligations), (&[am::SINGLE_ASSET_VAULT][..], TxResult::Success)] {
            let mut st = state_with(amendments);
            let id = issuance(&mut st, 0, json!({}));
            put(&mut st, keylet::mptoken_key(&keylet::mpt_issuance_key(&id), &ALICE), &json!({
                "LedgerEntryType": "MPToken", "Account": hex::encode(ALICE), "MPTokenIssuanceID": hex::encode_upper(id),
                "Flags": 0, "OwnerNode": "0", "LockedAmount": "4",
            }));
            assert_eq!(remove_empty_holding(&mut Sandbox::new(&st), &ALICE, &HeldAsset::Mpt(id)), want);
        }
        // A confidential balance is an obligation.
        let mut st = state();
        let id = issuance(&mut st, 0, json!({}));
        put(&mut st, keylet::mptoken_key(&keylet::mpt_issuance_key(&id), &ALICE), &json!({
            "LedgerEntryType": "MPToken", "Account": hex::encode(ALICE), "MPTokenIssuanceID": hex::encode_upper(id),
            "Flags": 0, "OwnerNode": "0", "ConfidentialBalanceInbox": "AB",
        }));
        assert_eq!(remove_empty_holding(&mut Sandbox::new(&st), &ALICE, &HeldAsset::Mpt(id)), TxResult::HasObligations);
    }

    /// MPTokenHelpers.cpp:200-330.
    #[test]
    fn authorize_mptoken_covers_holder_and_issuer_sides() {
        let mut st = state();
        let id = issuance(&mut st, mpt::LSF_MPT_REQUIRE_AUTH, json!({}));
        let key = keylet::mptoken_key(&keylet::mpt_issuance_key(&id), &ALICE);

        // tecINTERNAL: no such account.
        assert_eq!(authorize_mptoken(&mut Sandbox::new(&st), BALANCE, &id, &[0x78; 20], 0, None), TxResult::TecInternal);

        // The holder opts in; the reserve is only enforced from the third owned object.
        let mut sb = Sandbox::new(&st);
        assert_eq!(authorize_mptoken(&mut sb, 0, &id, &ALICE, 0, None), TxResult::Success, "free below two owned objects");
        assert_eq!(owners(&sb, &ALICE), 1);
        assert!(dir_has(&sb, &ALICE, &key));
        // The issuer authorizes, then revokes.
        assert_eq!(authorize_mptoken(&mut sb, 0, &id, &ISSUER, 0, Some(&ALICE)), TxResult::Success);
        assert_eq!(get(&sb, &key).unwrap()["Flags"], json!(mpt::LSF_MPT_AUTHORIZED));
        assert_eq!(authorize_mptoken(&mut sb, 0, &id, &ISSUER, TF_MPT_UNAUTHORIZE, Some(&ALICE)), TxResult::Success);
        assert_eq!(get(&sb, &key).unwrap()["Flags"], json!(0));
        // tecINTERNAL on the issuer side: not the issuer, no token, no issuance.
        assert_eq!(authorize_mptoken(&mut sb, 0, &id, &BOB, 0, Some(&ALICE)), TxResult::TecInternal);
        assert_eq!(authorize_mptoken(&mut sb, 0, &id, &ISSUER, 0, Some(&BOB)), TxResult::TecInternal);
        assert_eq!(authorize_mptoken(&mut sb, 0, &mptid(99, ISSUER), &ISSUER, 0, Some(&ALICE)), TxResult::TecInternal);
        // The holder opts out.
        assert_eq!(authorize_mptoken(&mut sb, 0, &id, &ALICE, TF_MPT_UNAUTHORIZE, None), TxResult::Success);
        assert!(get(&sb, &key).is_none() && !dir_has(&sb, &ALICE, &key));
        assert_eq!(owners(&sb, &ALICE), 0);
        assert_eq!(authorize_mptoken(&mut sb, 0, &id, &ALICE, TF_MPT_UNAUTHORIZE, None), TxResult::TecInternal, "no token to delete");

        // tecINSUFFICIENT_RESERVE once two objects are owned.
        let mut st2 = st.clone();
        root_in(&mut st2, ALICE, 0, json!({"OwnerCount": 2}));
        let need = crate::ledger::fees::account_reserve(&Sandbox::new(&st2), 3);
        assert_eq!(authorize_mptoken(&mut Sandbox::new(&st2), need - 1, &id, &ALICE, 0, None), TxResult::InsufficientReserve);
        assert_eq!(authorize_mptoken(&mut Sandbox::new(&st2), need, &id, &ALICE, 0, None), TxResult::Success);

        // A token with a balance cannot be deleted here: tecINTERNAL (the caller checked).
        let mut st3 = st.clone();
        token(&mut st3, &id, ALICE, 0, 5);
        assert_eq!(authorize_mptoken(&mut Sandbox::new(&st3), 0, &id, &ALICE, TF_MPT_UNAUTHORIZE, None), TxResult::TecInternal);

        // The issuer's own token, or no issuance: tecINTERNAL under LendingProtocol; before it rippled creates the token.
        assert_eq!(authorize_mptoken(&mut Sandbox::new(&st), 0, &id, &ISSUER, 0, None), TxResult::TecInternal);
        assert_eq!(authorize_mptoken(&mut Sandbox::new(&st), 0, &mptid(99, ISSUER), &ALICE, 0, None), TxResult::TecInternal);
        let mut no_lp = state_with(&[am::SINGLE_ASSET_VAULT]);
        let id2 = issuance(&mut no_lp, 0, json!({}));
        assert_eq!(authorize_mptoken(&mut Sandbox::new(&no_lp), 0, &id2, &ISSUER, 0, None), TxResult::Success);
    }

    fn domain_fixture(st: &mut LedgerState, expiration: Option<u32>, accepted: bool) -> (Hash256, Hash256) {
        let domain = Hash256([0xD0; 32]);
        let kind = "4B5943";
        put(st, domain, &json!({
            "LedgerEntryType": "PermissionedDomain", "Owner": hex::encode(ISSUER), "Flags": 0, "Sequence": 3, "OwnerNode": "0",
            "AcceptedCredentials": [{"Credential": {"Issuer": hex::encode(ISSUER), "CredentialType": kind}}],
        }));
        let cred_key = crate::tx::credential::credential_key(&ALICE, &ISSUER, &hex::decode(kind).unwrap());
        let mut cred = json!({
            "LedgerEntryType": "Credential", "Subject": hex::encode(ALICE), "Issuer": hex::encode(ISSUER), "CredentialType": kind,
            "Flags": if accepted { 0x0001_0000 } else { 0 }, "IssuerNode": "0", "SubjectNode": "0",
        });
        if let Some(exp) = expiration {
            cred["Expiration"] = json!(exp);
        }
        put(st, cred_key, &cred);
        (domain, cred_key)
    }

    /// MPTokenHelpers.cpp:481-578, each of its six outcomes.
    #[test]
    fn enforce_mptoken_authorization_follows_the_domain_or_the_flag() {
        // tefINTERNAL: no issuance; the issuer itself.
        let mut st = state();
        let plain = issuance(&mut st, mpt::LSF_MPT_REQUIRE_AUTH, json!({}));
        assert_eq!(enforce_mptoken_authorization(&mut Sandbox::new(&st), &mptid(99, ISSUER), &ALICE, BALANCE), TxResult::TefInternal);
        assert_eq!(enforce_mptoken_authorization(&mut Sandbox::new(&st), &plain, &ISSUER, BALANCE), TxResult::TefInternal);

        // No domain: no token is tecNO_AUTH; a token needs lsfMPTAuthorized.
        assert_eq!(enforce_mptoken_authorization(&mut Sandbox::new(&st), &plain, &ALICE, BALANCE), TxResult::NoAuth);
        token(&mut st, &plain, ALICE, 0, 0);
        assert_eq!(enforce_mptoken_authorization(&mut Sandbox::new(&st), &plain, &ALICE, BALANCE), TxResult::NoAuth);
        token(&mut st, &plain, ALICE, mpt::LSF_MPT_AUTHORIZED, 0);
        assert_eq!(enforce_mptoken_authorization(&mut Sandbox::new(&st), &plain, &ALICE, BALANCE), TxResult::Success);

        // A domain and an accepted credential: the MPToken is created…
        let mut st = state();
        let (domain, _) = domain_fixture(&mut st, None, true);
        let id = issuance(&mut st, mpt::LSF_MPT_REQUIRE_AUTH, json!({"DomainID": hex::encode_upper(domain.0)}));
        let key = keylet::mptoken_key(&keylet::mpt_issuance_key(&id), &ALICE);
        let mut sb = Sandbox::new(&st);
        assert_eq!(enforce_mptoken_authorization(&mut sb, &id, &ALICE, BALANCE), TxResult::Success);
        assert_eq!(get(&sb, &key).unwrap()["Flags"], json!(0), "created, lsfMPTAuthorized not needed");
        assert_eq!(owners(&sb, &ALICE), 1);
        // …and an existing one is fine without the flag.
        assert_eq!(enforce_mptoken_authorization(&mut sb, &id, &ALICE, BALANCE), TxResult::Success);
        // Creating it is subject to the reserve.
        let mut poor = st.clone();
        root_in(&mut poor, ALICE, 0, json!({"OwnerCount": 2}));
        assert_eq!(enforce_mptoken_authorization(&mut Sandbox::new(&poor), &id, &ALICE, 1), TxResult::InsufficientReserve);
        // BOB has no credential: tecNO_AUTH, with or without a token.
        assert_eq!(enforce_mptoken_authorization(&mut Sandbox::new(&st), &id, &BOB, BALANCE), TxResult::NoAuth);
        let mut with_token = st.clone();
        token(&mut with_token, &id, BOB, mpt::LSF_MPT_AUTHORIZED, 0);
        assert_eq!(
            enforce_mptoken_authorization(&mut Sandbox::new(&with_token), &id, &BOB, BALANCE),
            TxResult::NoAuth,
            "with a domain the flag does not authorize"
        );

        // An expired credential: tecEXPIRED and the credential is deleted — with or without a token.
        for has_token in [false, true] {
            let mut st = state(); // parent close time 1000
            let (domain, cred_key) = domain_fixture(&mut st, Some(999), true);
            let id = issuance(&mut st, mpt::LSF_MPT_REQUIRE_AUTH, json!({"DomainID": hex::encode_upper(domain.0)}));
            if has_token {
                token(&mut st, &id, ALICE, 0, 0);
            }
            let mut sb = Sandbox::new(&st);
            assert_eq!(enforce_mptoken_authorization(&mut sb, &id, &ALICE, BALANCE), TxResult::Expired, "token {has_token}");
            assert!(get(&sb, &cred_key).is_none(), "the expired credential is deleted");
        }
        // A credential not yet accepted does not authorize.
        let mut st = state();
        let (domain, _) = domain_fixture(&mut st, None, false);
        let id = issuance(&mut st, mpt::LSF_MPT_REQUIRE_AUTH, json!({"DomainID": hex::encode_upper(domain.0)}));
        assert_eq!(enforce_mptoken_authorization(&mut Sandbox::new(&st), &id, &ALICE, BALANCE), TxResult::NoAuth);
    }

    /// MPTokenIssuanceCreate.cpp:123-200, as VaultCreate calls it
    /// (VaultCreate.cpp:226-246): the share issuance of a vault of an IOU on
    /// devnet has exactly these fields.
    #[test]
    fn create_mpt_issuance_builds_a_vaults_share_issuance() {
        use crate::ledger::keylet::vault_devnet_fixture::object;
        use crate::tx::protocol::LSF_MPT_CAN_TRADE;
        let st = state();
        let mut sb = Sandbox::new(&st);
        // The pseudo-account's holding of the asset comes first; it is the reference.
        assert_eq!(add_empty_holding(&mut sb, &PSEUDO, BALANCE, &USD), TxResult::Success);
        let holding = keylet::ripple_state_key(&PSEUDO, &ISSUER, &USD_CODE);
        let share_flags = mpt::LSF_MPT_CAN_ESCROW | LSF_MPT_CAN_TRADE | mpt::LSF_MPT_CAN_TRANSFER;
        let args = MptCreateArgs {
            prior_balance: None,
            account: PSEUDO,
            sequence: 1,
            flags: share_flags | 0x8000_0000,
            asset_scale: Some(6),
            reference_holding: Some(holding),
            ..MptCreateArgs::default()
        };
        let id = create_mpt_issuance(&mut sb, &args).expect("created");
        assert_eq!(id, mptid(1, PSEUDO));
        let key = keylet::mpt_issuance_key(&id);
        let ours = get(&sb, &key).unwrap();
        assert_eq!(ours, json!({
            "LedgerEntryType": "MPTokenIssuance", "Flags": share_flags, "Issuer": hex::encode(PSEUDO), "OutstandingAmount": "0",
            "OwnerNode": "0", "Sequence": 1, "AssetScale": 6, "ReferenceHolding": hex::encode_upper(holding.0),
        }));
        assert!(dir_has(&sb, &PSEUDO, &key));
        assert_eq!(owners(&sb, &PSEUDO), 2, "the trust line and the issuance");
        // Devnet's share issuance of the USD vault: the same fields (plus the stamp), the same flags and scale.
        let (theirs, _) = object("share_issuance_usd");
        let mut names: Vec<&str> = theirs.as_object().unwrap().keys().map(String::as_str).collect();
        names.retain(|k| !["index", "mpt_issuance_id", "PreviousTxnID", "PreviousTxnLgrSeq"].contains(k));
        let mut our_names: Vec<&str> = ours.as_object().unwrap().keys().map(String::as_str).collect();
        names.sort_unstable();
        our_names.sort_unstable();
        assert_eq!(our_names, names);
        for same in ["Flags", "Sequence", "AssetScale", "LedgerEntryType"] {
            assert_eq!(ours[same], theirs[same], "{same}");
        }

        // Every optional argument; defaults given as zero stay absent.
        let domain = Hash256([0xD0; 32]);
        let full = MptCreateArgs {
            prior_balance: Some(BALANCE),
            account: ALICE,
            sequence: 9,
            flags: mpt::LSF_MPT_REQUIRE_AUTH | 0x4000_0000,
            max_amount: Some(1_000_000),
            asset_scale: Some(0),
            transfer_fee: Some(250),
            metadata: Some("CAFE".to_string()),
            domain_id: Some(domain),
            immutable_flags: Some(0),
            reference_holding: None,
        };
        let id = create_mpt_issuance(&mut sb, &full).expect("created");
        assert_eq!(get(&sb, &keylet::mpt_issuance_key(&id)).unwrap(), json!({
            "LedgerEntryType": "MPTokenIssuance", "Flags": mpt::LSF_MPT_REQUIRE_AUTH, "Issuer": hex::encode(ALICE),
            "OutstandingAmount": "0", "OwnerNode": "0", "Sequence": 9, "MaximumAmount": "1000000", "TransferFee": 250,
            "MPTokenMetadata": "CAFE", "DomainID": hex::encode_upper(domain.0),
        }));
        assert_eq!(owners(&sb, &ALICE), 1);

        // tecINTERNAL: no account; a reference that is missing or of another type.
        let nobody = MptCreateArgs { account: [0x78; 20], ..MptCreateArgs::default() };
        assert_eq!(create_mpt_issuance(&mut Sandbox::new(&st), &nobody), Err(TxResult::TecInternal));
        for bad in [Hash256([0xEE; 32]), keylet::account_root_key(&ALICE)] {
            let args = MptCreateArgs { account: PSEUDO, sequence: 1, reference_holding: Some(bad), ..MptCreateArgs::default() };
            assert_eq!(create_mpt_issuance(&mut Sandbox::new(&st), &args), Err(TxResult::TecInternal));
        }
        // tecINSUFFICIENT_RESERVE, only when a prior balance is given.
        let need = crate::ledger::fees::account_reserve(&Sandbox::new(&st), 1);
        let short = MptCreateArgs { prior_balance: Some(need - 1), account: ALICE, sequence: 9, ..MptCreateArgs::default() };
        assert_eq!(create_mpt_issuance(&mut Sandbox::new(&st), &short), Err(TxResult::InsufficientReserve));
        let unchecked = MptCreateArgs { prior_balance: None, account: ALICE, sequence: 9, ..MptCreateArgs::default() };
        assert!(create_mpt_issuance(&mut Sandbox::new(&st), &unchecked).is_ok());
    }

    /// TokenHelpers.cpp:366-497, AccountRootHelpers.cpp:269-302.
    #[test]
    fn account_holds_reads_each_asset() {
        let _scale = large();
        let mut st = state();
        root_in(&mut st, ALICE, 0, json!({"OwnerCount": 2}));
        root_in(&mut st, PSEUDO, 0x0190_0000, json!({"Sequence": 0, "Balance": "777", "OwnerCount": 3, "VaultID": "84".repeat(32)}));
        line(&mut st, ALICE, 0, "50", "1000");
        line(&mut st, PSEUDO, 0, "12.5", "0");
        line(&mut st, BOB, 0x0080_0000, "7", "100"); // lsfHighFreeze: the issuer froze BOB
        let id = issuance(&mut st, mpt::LSF_MPT_CAN_TRANSFER, json!({}));
        token(&mut st, &id, ALICE, 0, 40);
        token(&mut st, &id, BOB, mpt::LSF_MPT_LOCKED, 9);
        let sb = Sandbox::new(&st);
        let reserve = crate::ledger::fees::account_reserve(&sb, 2);
        assert_eq!(xrp_liquid(&sb, &ALICE), BALANCE - reserve);
        assert_eq!(xrp_liquid(&sb, &PSEUDO), 777, "a pseudo-account has no reserve");
        assert_eq!(xrp_liquid(&sb, &[0x78; 20]), 0);
        assert_eq!(account_holds(&sb, &ALICE, &HeldAsset::Xrp, true, true), drops(BALANCE - reserve));
        assert_eq!(account_holds(&sb, &ALICE, &USD, true, true), usd("50"));
        assert_eq!(account_holds(&sb, &PSEUDO, &USD, true, true), usd("12.5"), "the high side's balance is stored negated");
        assert_eq!(account_holds(&sb, &[0x78; 20], &USD, false, false), Amount::zero(USD));
        assert_eq!(account_holds(&sb, &BOB, &USD, true, false), Amount::zero(USD), "frozen");
        assert_eq!(account_holds(&sb, &BOB, &USD, false, false), usd("7"), "IgnoreFreeze");
        let asset = HeldAsset::Mpt(id);
        assert_eq!(account_holds(&sb, &ALICE, &asset, true, true), units(&id, 40));
        assert_eq!(account_holds(&sb, &BOB, &asset, true, false), Amount::zero(asset));
        assert_eq!(account_holds(&sb, &BOB, &asset, false, false), units(&id, 9));
        assert_eq!(account_holds(&sb, &PSEUDO, &asset, false, false), Amount::zero(asset));
        // An account that owes shows a negative holding.
        let mut st = state();
        line(&mut st, ALICE, 0, "-3", "0");
        assert_eq!(account_holds(&Sandbox::new(&st), &ALICE, &USD, false, false), usd("-3"));
    }

    /// TokenHelpers.cpp:1552-1570 and the three asset arms.
    #[test]
    fn account_send_moves_each_asset() {
        let _scale = large();
        // XRP: root to root; more than the sender has is tecFAILED_PROCESSING.
        let st = state();
        let mut sb = Sandbox::new(&st);
        assert_eq!(account_send(&mut sb, &ALICE, &PSEUDO, &drops(1_000), true), TxResult::Success);
        assert_eq!(root(&sb, &ALICE).unwrap()["Balance"], json!((BALANCE - 1_000).to_string()));
        assert_eq!(root(&sb, &PSEUDO).unwrap()["Balance"], json!("1000"));
        assert_eq!(account_send(&mut sb, &PSEUDO, &BOB, &drops(1_001), true), TxResult::TecFailedProcessing);
        assert_eq!(TxResult::TecFailedProcessing.code_str(), "tecFAILED_PROCESSING");
        let before = sb.modification_count();
        assert_eq!(account_send(&mut sb, &ALICE, &ALICE, &drops(5), true), TxResult::Success, "to oneself: nothing");
        assert_eq!(account_send(&mut sb, &ALICE, &BOB, &drops(0), true), TxResult::Success, "zero: nothing");
        assert_eq!(sb.modification_count(), before);
        let negative = Amount { negative: true, ..usd("5") };
        assert_eq!(account_send(&mut sb, &ALICE, &BOB, &negative, true), TxResult::TecInternal);

        // IOU, holder → pseudo-account (a deposit): through the issuer, no fee
        // when waived, the pseudo-account's line already there.
        let mut st = state();
        root_in(&mut st, ISSUER, LSF_DEFAULT_RIPPLE, json!({"TransferRate": 1_200_000_000u64}));
        let alice_line = line(&mut st, ALICE, LSF_LOW_RESERVE, "50", "1000");
        let pseudo_line = line(&mut st, PSEUDO, LSF_HIGH_RESERVE | LSF_HIGH_NO_RIPPLE, "0", "0");
        let mut sb = Sandbox::new(&st);
        assert_eq!(account_send(&mut sb, &ALICE, &PSEUDO, &usd("20"), true), TxResult::Success);
        assert_eq!(get(&sb, &alice_line).unwrap()["Balance"]["value"], json!("30"));
        assert_eq!(get(&sb, &pseudo_line).unwrap()["Balance"]["value"], json!("-20"));
        // Not waived: the sender pays the issuer's 20% on top.
        let mut sb = Sandbox::new(&st);
        assert_eq!(account_send(&mut sb, &ALICE, &PSEUDO, &usd("20"), false), TxResult::Success);
        assert_eq!(get(&sb, &alice_line).unwrap()["Balance"]["value"], json!("26"));
        assert_eq!(get(&sb, &pseudo_line).unwrap()["Balance"]["value"], json!("-20"));
        // With the issuer on one end: one line moves, never a fee.
        let mut sb = Sandbox::new(&st);
        assert_eq!(account_send(&mut sb, &ALICE, &ISSUER, &usd("20"), false), TxResult::Success);
        assert_eq!(get(&sb, &alice_line).unwrap()["Balance"]["value"], json!("30"));
        assert_eq!(account_send(&mut sb, &ISSUER, &ALICE, &usd("5"), false), TxResult::Success);
        assert_eq!(get(&sb, &alice_line).unwrap()["Balance"]["value"], json!("35"));
        // A receiver without a line gets one (reserve charged, no test).
        let mut sb = Sandbox::new(&st);
        assert_eq!(account_send(&mut sb, &ALICE, &BOB, &usd("1"), true), TxResult::Success);
        let bob_line = keylet::ripple_state_key(&BOB, &ISSUER, &USD_CODE);
        assert_eq!(get(&sb, &bob_line).unwrap()["Balance"]["value"], json!("1"));
        assert_eq!(owners(&sb, &BOB), 1);

        // MPT.
        let mut st = state();
        let id = issuance(&mut st, mpt::LSF_MPT_CAN_TRANSFER, json!({"MaximumAmount": "1100", "TransferFee": 10_000}));
        token(&mut st, &id, ALICE, 0, 40);
        token(&mut st, &id, PSEUDO, 0, 0);
        let ikey = keylet::mpt_issuance_key(&id);
        let balance = |sb: &Sandbox, who: &[u8; 20]| mpt::dec_field(&get(sb, &keylet::mptoken_key(&ikey, who)).unwrap(), "MPTAmount");
        let outstanding = |sb: &Sandbox| mpt::dec_field(&get(sb, &ikey).unwrap(), "OutstandingAmount");
        let mut sb = Sandbox::new(&st);
        assert_eq!(account_send(&mut sb, &ALICE, &PSEUDO, &units(&id, 30), true), TxResult::Success);
        assert_eq!((balance(&sb, &ALICE), balance(&sb, &PSEUDO), outstanding(&sb)), (10, 30, 1000));
        // Not waived: a 10% transfer fee is burned at the issuer.
        let mut sb = Sandbox::new(&st);
        assert_eq!(account_send(&mut sb, &ALICE, &PSEUDO, &units(&id, 30), false), TxResult::Success);
        assert_eq!((balance(&sb, &ALICE), balance(&sb, &PSEUDO), outstanding(&sb)), (7, 30, 997));
        // More than the sender holds; a party without an MPToken; no issuance.
        assert_eq!(account_send(&mut Sandbox::new(&st), &ALICE, &PSEUDO, &units(&id, 41), true), TxResult::InsufficientFunds);
        assert_eq!(account_send(&mut Sandbox::new(&st), &ALICE, &BOB, &units(&id, 1), true), TxResult::NoAuth);
        assert_eq!(account_send(&mut Sandbox::new(&st), &BOB, &ALICE, &units(&id, 1), true), TxResult::NoAuth);
        assert_eq!(
            account_send(&mut Sandbox::new(&st), &ALICE, &PSEUDO, &units(&mptid(99, ISSUER), 1), true),
            TxResult::ObjectNotFound
        );
        // The issuer mints up to MaximumAmount (1100 against 1000 outstanding), and redeems.
        let mut sb = Sandbox::new(&st);
        assert_eq!(account_send(&mut sb, &ISSUER, &ALICE, &units(&id, 100), true), TxResult::Success);
        assert_eq!((balance(&sb, &ALICE), outstanding(&sb)), (140, 1100));
        assert_eq!(account_send(&mut sb, &ISSUER, &ALICE, &units(&id, 1), true), TxResult::PathDry);
        assert_eq!(account_send(&mut sb, &ALICE, &ISSUER, &units(&id, 140), true), TxResult::Success);
        assert_eq!((balance(&sb, &ALICE), outstanding(&sb)), (0, 960));
        assert_eq!(account_send(&mut sb, &ALICE, &ALICE, &units(&id, 5), true), TxResult::Success);
    }

    /// TokenHelpers.cpp:1573-1590 and its three arms (:887-964, :1084-1213,
    /// :1361-1487).
    #[test]
    fn account_send_multi_debits_the_sender_once() {
        use crate::tx::number::{Number, Rounding};
        let _scale = large();
        let n = |m: i64, e: i32| Number::new(m, e, Rounding::ToNearest).unwrap();
        let near = Rounding::ToNearest;

        // XRP: each receiver credited, the sender debited the total; zero and the sender itself skipped.
        let st = state();
        let mut sb = Sandbox::new(&st);
        let pay = [(BOB, n(300, 0)), (PSEUDO, n(200, 0)), (ALICE, n(999, 0)), (ISSUER, Number::ZERO)];
        assert_eq!(account_send_multi(&mut sb, &ALICE, &HeldAsset::Xrp, &pay, true, near), Ok(TxResult::Success));
        assert_eq!(root(&sb, &ALICE).unwrap()["Balance"], json!((BALANCE - 500).to_string()));
        assert_eq!(root(&sb, &BOB).unwrap()["Balance"], json!((BALANCE + 300).to_string()));
        assert_eq!(root(&sb, &PSEUDO).unwrap()["Balance"], json!("200"));
        assert_eq!(root(&sb, &ISSUER).unwrap()["Balance"], json!(BALANCE.to_string()));
        // More than the sender has: tecFAILED_PROCESSING; a negative amount: tecINTERNAL.
        let too_much = [(BOB, n(BALANCE as i64, 0)), (ISSUER, n(1, 0))];
        assert_eq!(account_send_multi(&mut Sandbox::new(&st), &ALICE, &HeldAsset::Xrp, &too_much, true, near), Ok(TxResult::TecFailedProcessing));
        let negative = [(BOB, n(1, 0)), (ISSUER, n(-1, 0))];
        assert_eq!(account_send_multi(&mut Sandbox::new(&st), &ALICE, &HeldAsset::Xrp, &negative, true, near), Ok(TxResult::TecInternal));
        // A fraction of a drop is rounded as the ambient mode says.
        let fraction = [(BOB, n(15, -1)), (ISSUER, n(25, -1))];
        let mut sb = Sandbox::new(&st);
        assert_eq!(account_send_multi(&mut sb, &ALICE, &HeldAsset::Xrp, &fraction, true, Rounding::Upward), Ok(TxResult::Success));
        assert_eq!(root(&sb, &ALICE).unwrap()["Balance"], json!((BALANCE - 5).to_string()));

        // IOU out of the pseudo-account: ALICE and BOB through the issuer, the issuer directly.
        let mut st = state();
        root_in(&mut st, ISSUER, LSF_DEFAULT_RIPPLE, json!({"TransferRate": 1_200_000_000u64}));
        let pseudo_line = line(&mut st, PSEUDO, LSF_HIGH_RESERVE | LSF_HIGH_NO_RIPPLE, "100", "0");
        let alice_line = line(&mut st, ALICE, LSF_LOW_RESERVE, "1", "1000");
        let bob_line = keylet::ripple_state_key(&BOB, &ISSUER, &USD_CODE);
        let pay = [(ALICE, n(10, 0)), (ISSUER, n(5, 0)), (BOB, n(25, -1)), (PSEUDO, n(50, 0))];
        let value = |sb: &Sandbox, key: &Hash256| get(sb, key).unwrap()["Balance"]["value"].clone();
        let mut sb = Sandbox::new(&st);
        assert_eq!(account_send_multi(&mut sb, &PSEUDO, &USD, &pay, true, near), Ok(TxResult::Success));
        assert_eq!(value(&sb, &alice_line), json!("11"));
        assert_eq!(value(&sb, &bob_line), json!("2.5"), "a line is made for a receiver without one");
        assert_eq!(value(&sb, &pseudo_line), json!("-82.5"), "100 less 5 direct and 12.5 through the issuer");
        // Not waived: the 20% falls on the through-the-issuer legs only.
        let mut sb = Sandbox::new(&st);
        assert_eq!(account_send_multi(&mut sb, &PSEUDO, &USD, &pay, false, near), Ok(TxResult::Success));
        assert_eq!(value(&sb, &alice_line), json!("11"));
        assert_eq!(value(&sb, &pseudo_line), json!("-80"), "100 less 5 and 15");
        // The issuer as sender: every leg direct.
        let mut sb = Sandbox::new(&st);
        assert_eq!(account_send_multi(&mut sb, &ISSUER, &USD, &pay, false, near), Ok(TxResult::Success));
        assert_eq!((value(&sb, &alice_line), value(&sb, &bob_line), value(&sb, &pseudo_line)), (json!("11"), json!("2.5"), json!("-150")));

        // MPT between holders and back to the issuer.
        let mut st = state();
        let id = issuance(&mut st, mpt::LSF_MPT_CAN_TRANSFER, json!({"MaximumAmount": "1100", "TransferFee": 10_000}));
        token(&mut st, &id, PSEUDO, 0, 100);
        token(&mut st, &id, ALICE, 0, 0);
        token(&mut st, &id, BOB, 0, 0);
        let asset = HeldAsset::Mpt(id);
        let ikey = keylet::mpt_issuance_key(&id);
        let balance = |sb: &Sandbox, who: &[u8; 20]| mpt::dec_field(&get(sb, &keylet::mptoken_key(&ikey, who)).unwrap(), "MPTAmount");
        let outstanding = |sb: &Sandbox| mpt::dec_field(&get(sb, &ikey).unwrap(), "OutstandingAmount");
        let pay = [(ALICE, n(10, 0)), (ISSUER, n(5, 0)), (BOB, n(20, 0))];
        let mut sb = Sandbox::new(&st);
        assert_eq!(account_send_multi(&mut sb, &PSEUDO, &asset, &pay, true, near), Ok(TxResult::Success));
        assert_eq!((balance(&sb, &ALICE), balance(&sb, &BOB), balance(&sb, &PSEUDO), outstanding(&sb)), (10, 20, 65, 995));
        let mut sb = Sandbox::new(&st);
        assert_eq!(account_send_multi(&mut sb, &PSEUDO, &asset, &pay, false, near), Ok(TxResult::Success));
        assert_eq!((balance(&sb, &ALICE), balance(&sb, &BOB), balance(&sb, &PSEUDO), outstanding(&sb)), (10, 20, 62, 992), "33 for the 30 in transit");
        // The sender cannot cover the total; a receiver has no MPToken; no issuance; a negative amount.
        let heavy = [(ALICE, n(60, 0)), (BOB, n(41, 0))];
        assert_eq!(account_send_multi(&mut Sandbox::new(&st), &PSEUDO, &asset, &heavy, true, near), Ok(TxResult::InsufficientFunds));
        let stranger = [(ALICE, n(1, 0)), ([0x78; 20], n(1, 0))];
        assert_eq!(account_send_multi(&mut Sandbox::new(&st), &PSEUDO, &asset, &stranger, true, near), Ok(TxResult::NoAuth));
        let missing = HeldAsset::Mpt(mptid(99, ISSUER));
        assert_eq!(account_send_multi(&mut Sandbox::new(&st), &PSEUDO, &missing, &pay, true, near), Ok(TxResult::ObjectNotFound));
        let negative = [(ALICE, n(1, 0)), (BOB, n(-1, 0))];
        assert_eq!(account_send_multi(&mut Sandbox::new(&st), &PSEUDO, &asset, &negative, true, near), Ok(TxResult::TecInternal));

        // The issuer minting to two receivers against MaximumAmount 1100 with 1000 outstanding:
        // 60 + 60 is over the cap in aggregate. fixCleanup3_1_3 sees it; before the fix each
        // send is judged alone against the outstanding amount read at the start.
        let mint = [(ALICE, n(60, 0)), (BOB, n(60, 0))];
        assert_eq!(account_send_multi(&mut Sandbox::new(&st), &ISSUER, &asset, &mint, true, near), Ok(TxResult::PathDry));
        let within = [(ALICE, n(60, 0)), (BOB, n(40, 0))];
        let mut sb = Sandbox::new(&st);
        assert_eq!(account_send_multi(&mut sb, &ISSUER, &asset, &within, true, near), Ok(TxResult::Success));
        assert_eq!(outstanding(&sb), 1100);
        let mut old = state_with(&[am::SINGLE_ASSET_VAULT, am::LENDING_PROTOCOL]);
        let id = issuance(&mut old, mpt::LSF_MPT_CAN_TRANSFER, json!({"MaximumAmount": "1100"}));
        token(&mut old, &id, ALICE, 0, 0);
        token(&mut old, &id, BOB, 0, 0);
        let mut sb = Sandbox::new(&old);
        assert_eq!(account_send_multi(&mut sb, &ISSUER, &HeldAsset::Mpt(id), &mint, true, near), Ok(TxResult::Success));
        assert_eq!(mpt::dec_field(&get(&sb, &keylet::mpt_issuance_key(&id)).unwrap(), "OutstandingAmount"), 1120);
        let alone = [(ALICE, n(101, 0)), (BOB, n(1, 0))];
        assert_eq!(account_send_multi(&mut Sandbox::new(&old), &ISSUER, &HeldAsset::Mpt(id), &alone, true, near), Ok(TxResult::PathDry));
    }

    fn withdraw_tx(fields: Value) -> TxFields {
        TxFields {
            account: ALICE, tx_type: "VaultWithdraw".to_string(), fee: 12, sequence: 5, ticket_seq: None, last_ledger_seq: None,
            fields, inner_batch: false,
        }
    }

    /// View.cpp:438-533.
    #[test]
    fn can_withdraw_checks_the_destination() {
        let _scale = large();
        const LSF_DEPOSIT_AUTH: u64 = 0x0100_0000;
        let mut st = state();
        root_in(&mut st, BOB, LSF_REQUIRE_DEST_TAG, json!({}));
        let sb = Sandbox::new(&st);
        // tecNO_DST, tecDST_TAG_NEEDED; to oneself passes.
        assert_eq!(can_withdraw(&sb, &ALICE, &[0x78; 20], &drops(5), false, None), TxResult::NoDst);
        assert_eq!(can_withdraw(&sb, &ALICE, &BOB, &drops(5), false, None), TxResult::DstTagNeeded);
        assert_eq!(can_withdraw(&sb, &ALICE, &BOB, &drops(5), true, None), TxResult::Success);
        assert_eq!(can_withdraw(&sb, &ALICE, &ALICE, &usd("5"), false, None), TxResult::Success);
        // The transaction form: the destination defaults to the account.
        assert_eq!(can_withdraw_tx(&sb, &withdraw_tx(json!({"Amount": "5"}))), TxResult::Success);
        assert_eq!(can_withdraw_tx(&sb, &withdraw_tx(json!({"Amount": "5", "Destination": hex::encode(BOB)}))), TxResult::DstTagNeeded);
        assert_eq!(
            can_withdraw_tx(&sb, &withdraw_tx(json!({"Amount": "5", "Destination": hex::encode(BOB), "DestinationTag": 0}))),
            TxResult::Success
        );
        assert_eq!(can_withdraw_tx(&sb, &withdraw_tx(json!({}))), TxResult::Malformed);

        // DepositAuth: tecNO_PERMISSION without a preauth or credentials.
        let mut st = state();
        root_in(&mut st, BOB, LSF_DEPOSIT_AUTH, json!({}));
        assert_eq!(can_withdraw(&Sandbox::new(&st), &ALICE, &BOB, &drops(5), false, None), TxResult::NoPermission);
        assert_eq!(can_withdraw(&Sandbox::new(&st), &BOB, &BOB, &drops(5), false, None), TxResult::Success);
        // A credential that does not exist is tecINTERNAL; one that names no preauth is tecNO_PERMISSION.
        let ghost = [Hash256([0xC0; 32])];
        assert_eq!(can_withdraw(&Sandbox::new(&st), &ALICE, &BOB, &drops(5), false, Some(&ghost)), TxResult::TecInternal);
        let (_, cred_key) = domain_fixture(&mut st, None, true);
        let ids = [cred_key];
        assert_eq!(can_withdraw(&Sandbox::new(&st), &ALICE, &BOB, &drops(5), false, Some(&ids)), TxResult::NoPermission);
        let pairs = vec![(ISSUER, hex::decode("4B5943").unwrap())];
        let mut by_credential = st.clone();
        put(&mut by_credential, keylet::deposit_preauth_credentials_key(&BOB, &pairs), &json!({"LedgerEntryType": "DepositPreauth"}));
        assert_eq!(can_withdraw(&Sandbox::new(&by_credential), &ALICE, &BOB, &drops(5), false, Some(&ids)), TxResult::Success);
        // A DepositPreauth of the submitter needs no credentials.
        put(&mut st, keylet::deposit_preauth_key(&BOB, &ALICE), &json!({"LedgerEntryType": "DepositPreauth"}));
        assert_eq!(can_withdraw(&Sandbox::new(&st), &ALICE, &BOB, &drops(5), false, None), TxResult::Success);

        // The trust limit of a third-party destination (IOU only).
        let mut st = state();
        line(&mut st, BOB, 0, "0", "100");
        let id = issuance(&mut st, mpt::LSF_MPT_CAN_TRANSFER, json!({}));
        let sb = Sandbox::new(&st);
        assert_eq!(can_withdraw(&sb, &ALICE, &BOB, &usd("100"), false, None), TxResult::Success);
        assert_eq!(can_withdraw(&sb, &ALICE, &BOB, &usd("100.5"), false, None), TxResult::NoLine);
        assert_eq!(can_withdraw(&sb, &ALICE, &ISSUER, &usd("100.5"), false, None), TxResult::Success, "to the issuer");
        assert_eq!(can_withdraw(&sb, &ALICE, &BOB, &units(&id, 1_000_000), false, None), TxResult::Success, "an MPT has no such limit");
        // No line at all: limit zero, so anything is tecNO_LINE.
        let st = state();
        assert_eq!(can_withdraw(&Sandbox::new(&st), &ALICE, &BOB, &usd("1"), false, None), TxResult::NoLine);
        // A destination holding 3 of a limit of 5 has room for 2.
        let mut st = state();
        line(&mut st, BOB, 0, "3", "5");
        assert_eq!(can_withdraw(&Sandbox::new(&st), &ALICE, &BOB, &usd("2"), false, None), TxResult::Success);
        assert_eq!(can_withdraw(&Sandbox::new(&st), &ALICE, &BOB, &usd("2.5"), false, None), TxResult::NoLine);
        // At or over its limit: nothing fits.
        let mut st = state();
        line(&mut st, BOB, 0, "5", "5");
        assert_eq!(can_withdraw(&Sandbox::new(&st), &ALICE, &BOB, &usd("0.001"), false, None), TxResult::NoLine);
        // A destination that OWES the issuer (owed > 0) is not tested.
        let mut st = state();
        line(&mut st, BOB, 0, "-3", "5");
        assert_eq!(can_withdraw(&Sandbox::new(&st), &ALICE, &BOB, &usd("500"), false, None), TxResult::Success);
    }

    /// View.cpp:535-598.
    #[test]
    fn do_withdraw_pays_out_of_the_pseudo_account() {
        let _scale = large();
        const LSF_DEPOSIT_AUTH: u64 = 0x0100_0000;
        let mut st = state();
        line(&mut st, PSEUDO, LSF_HIGH_RESERVE | LSF_HIGH_NO_RIPPLE, "100", "0");
        let tx = withdraw_tx(json!({}));
        let pseudo_line = keylet::ripple_state_key(&PSEUDO, &ISSUER, &USD_CODE);
        let alice_line = keylet::ripple_state_key(&ALICE, &ISSUER, &USD_CODE);

        // To oneself: the line is made, then paid.
        let mut sb = Sandbox::new(&st);
        assert_eq!(do_withdraw(&mut sb, &tx, &ALICE, &ALICE, &PSEUDO, BALANCE, &usd("40")), TxResult::Success);
        assert_eq!(get(&sb, &alice_line).unwrap()["Balance"]["value"], json!("40"));
        assert_eq!(get(&sb, &pseudo_line).unwrap()["Balance"]["value"], json!("-60"));
        assert_eq!(owners(&sb, &ALICE), 1);
        // Again: the existing line (tecDUPLICATE inside) is fine.
        assert_eq!(do_withdraw(&mut sb, &tx, &ALICE, &ALICE, &PSEUDO, BALANCE, &usd("60")), TxResult::Success);
        assert_eq!(get(&sb, &alice_line).unwrap()["Balance"]["value"], json!("100"));
        // The source is now empty: tefINTERNAL.
        assert_eq!(do_withdraw(&mut sb, &tx, &ALICE, &ALICE, &PSEUDO, BALANCE, &usd("1")), TxResult::TefInternal);

        // The holding cannot be afforded: addEmptyHolding's error comes back.
        assert_eq!(do_withdraw(&mut Sandbox::new(&st), &tx, &ALICE, &ALICE, &PSEUDO, 0, &usd("40")), TxResult::NoLineInsufReserve);

        // A zero withdrawal to oneself: no holding with fixCleanup3_4_0, one before it.
        let mut sb = Sandbox::new(&st);
        assert_eq!(do_withdraw(&mut sb, &tx, &ALICE, &ALICE, &PSEUDO, BALANCE, &usd("0")), TxResult::Success);
        assert!(get(&sb, &alice_line).is_none());
        let mut old = state_with(&[am::SINGLE_ASSET_VAULT]);
        line(&mut old, PSEUDO, LSF_HIGH_RESERVE | LSF_HIGH_NO_RIPPLE, "100", "0");
        let mut sb = Sandbox::new(&old);
        assert_eq!(do_withdraw(&mut sb, &tx, &ALICE, &ALICE, &PSEUDO, BALANCE, &usd("0")), TxResult::Success);
        assert!(get(&sb, &alice_line).is_some());

        // To someone else: DepositAuth needs the preauth; with it the payment creates BOB's line.
        let mut guarded = st.clone();
        root_in(&mut guarded, BOB, LSF_DEPOSIT_AUTH, json!({}));
        assert_eq!(do_withdraw(&mut Sandbox::new(&guarded), &tx, &ALICE, &BOB, &PSEUDO, BALANCE, &usd("40")), TxResult::NoPermission);
        put(&mut guarded, keylet::deposit_preauth_key(&BOB, &ALICE), &json!({"LedgerEntryType": "DepositPreauth"}));
        let mut sb = Sandbox::new(&guarded);
        assert_eq!(do_withdraw(&mut sb, &tx, &ALICE, &BOB, &PSEUDO, BALANCE, &usd("40")), TxResult::Success);
        assert_eq!(get(&sb, &keylet::ripple_state_key(&BOB, &ISSUER, &USD_CODE)).unwrap()["Balance"]["value"], json!("40"));
        assert!(get(&sb, &alice_line).is_none(), "the submitter gets no holding");

        // XRP out of the pseudo-account, and an MPT.
        let mut st = state();
        root_in(&mut st, PSEUDO, 0x0190_0000, json!({"Sequence": 0, "Balance": "900", "VaultID": "84".repeat(32)}));
        let id = issuance(&mut st, mpt::LSF_MPT_CAN_TRANSFER, json!({}));
        token(&mut st, &id, PSEUDO, 0, 25);
        let mut sb = Sandbox::new(&st);
        assert_eq!(do_withdraw(&mut sb, &tx, &ALICE, &ALICE, &PSEUDO, BALANCE, &drops(900)), TxResult::Success);
        assert_eq!(root(&sb, &PSEUDO).unwrap()["Balance"], json!("0"));
        assert_eq!(do_withdraw(&mut sb, &tx, &ALICE, &ALICE, &PSEUDO, BALANCE, &drops(1)), TxResult::TefInternal);
        assert_eq!(do_withdraw(&mut sb, &tx, &ALICE, &ALICE, &PSEUDO, BALANCE, &units(&id, 25)), TxResult::Success);
        let alice_token = get(&sb, &keylet::mptoken_key(&keylet::mpt_issuance_key(&id), &ALICE)).unwrap();
        assert_eq!(alice_token["MPTAmount"], json!("25"), "the MPToken was created and paid");
        assert_eq!(do_withdraw(&mut sb, &tx, &ALICE, &ALICE, &PSEUDO, BALANCE, &units(&id, 1)), TxResult::TefInternal);
    }

    // -----------------------------------------------------------------------
    // The vault transactors' additions
    // -----------------------------------------------------------------------

    const LSF_LOW_FREEZE: u64 = 0x0040_0000;
    const LSF_HIGH_FREEZE: u64 = 0x0080_0000;
    const LSF_GLOBAL_FREEZE: u64 = 0x0040_0000;

    /// TokenHelpers.cpp:219-262 (`checkDepositFreeze`): the issuer's global
    /// freeze, the depositor's individual freeze unless it is the issuer,
    /// and the pseudo-account's individual freeze each refuse — tecFROZEN
    /// for an IOU, tecLOCKED for an MPT. XRP freezes nothing.
    #[test]
    fn check_deposit_freeze_reads_the_three_freezes() {
        let st = state();
        let sb = Sandbox::new(&st);
        assert_eq!(check_deposit_freeze(&sb, &ALICE, &PSEUDO, &HeldAsset::Xrp), TxResult::Success);
        assert_eq!(check_deposit_freeze(&sb, &ALICE, &PSEUDO, &USD), TxResult::Success, "no lines, no freeze");

        // ALICE is the low account of her line: the issuer's side is High.
        let mut st = state();
        line(&mut st, ALICE, LSF_HIGH_FREEZE, "50", "1000");
        let sb = Sandbox::new(&st);
        assert_eq!(check_deposit_freeze(&sb, &ALICE, &PSEUDO, &USD), TxResult::Frozen, "the depositor's line is frozen");
        assert_eq!(check_deposit_freeze(&sb, &BOB, &PSEUDO, &USD), TxResult::Success);
        assert_eq!(check_deposit_freeze(&sb, &ISSUER, &PSEUDO, &USD), TxResult::Success, "the issuer deposits freely");

        // PSEUDO sorts above ISSUER: the issuer's side of its line is Low.
        let mut st = state();
        line(&mut st, PSEUDO, LSF_LOW_FREEZE, "50", "0");
        let sb = Sandbox::new(&st);
        assert_eq!(check_deposit_freeze(&sb, &ALICE, &PSEUDO, &USD), TxResult::Frozen, "the vault's line is frozen");
        assert_eq!(check_deposit_freeze(&sb, &ISSUER, &PSEUDO, &USD), TxResult::Frozen, "even for the issuer");

        let mut st = state();
        root_in(&mut st, ISSUER, LSF_DEFAULT_RIPPLE | LSF_GLOBAL_FREEZE, json!({}));
        let sb = Sandbox::new(&st);
        assert_eq!(check_deposit_freeze(&sb, &ALICE, &PSEUDO, &USD), TxResult::Frozen, "global");
        assert_eq!(check_deposit_freeze(&sb, &ISSUER, &PSEUDO, &USD), TxResult::Frozen, "global, the issuer too");

        let mut st = state();
        let id = issuance(&mut st, mpt::LSF_MPT_CAN_TRANSFER, json!({}));
        token(&mut st, &id, ALICE, mpt::LSF_MPT_LOCKED, 5);
        token(&mut st, &id, BOB, 0, 5);
        token(&mut st, &id, PSEUDO, 0, 5);
        let sb = Sandbox::new(&st);
        assert_eq!(check_deposit_freeze(&sb, &ALICE, &PSEUDO, &HeldAsset::Mpt(id)), TxResult::Locked);
        assert_eq!(check_deposit_freeze(&sb, &BOB, &PSEUDO, &HeldAsset::Mpt(id)), TxResult::Success);
        let mut st = state();
        let id = issuance(&mut st, mpt::LSF_MPT_CAN_TRANSFER | mpt::LSF_MPT_LOCKED, json!({}));
        let sb = Sandbox::new(&st);
        assert_eq!(check_deposit_freeze(&sb, &ISSUER, &PSEUDO, &HeldAsset::Mpt(id)), TxResult::Locked, "a locked issuance");
    }

    /// TokenHelpers.cpp:160-217 (`checkWithdrawFreeze`): to the issuer always;
    /// else the global freeze, the pseudo-account's individual freeze, the
    /// submitter's individual freeze unless it withdraws to itself, and the
    /// destination's deep freeze.
    #[test]
    fn check_withdraw_freeze_lets_a_frozen_holder_withdraw_to_itself() {
        const LSF_LOW_DEEP_FREEZE: u64 = 0x0200_0000;
        let mut st = state();
        line(&mut st, ALICE, LSF_HIGH_FREEZE, "50", "1000");
        line(&mut st, PSEUDO, 0, "50", "0");
        let sb = Sandbox::new(&st);
        assert_eq!(check_withdraw_freeze(&sb, &PSEUDO, &ALICE, &ALICE, &USD), TxResult::Success, "a plain freeze, to oneself");
        assert_eq!(check_withdraw_freeze(&sb, &PSEUDO, &ALICE, &BOB, &USD), TxResult::Frozen, "the submitter is frozen");
        assert_eq!(check_withdraw_freeze(&sb, &PSEUDO, &ALICE, &ISSUER, &USD), TxResult::Success, "to the issuer always");
        assert_eq!(check_withdraw_freeze(&sb, &PSEUDO, &BOB, &ALICE, &USD), TxResult::Success, "a plain freeze still receives");
        assert_eq!(check_withdraw_freeze(&sb, &PSEUDO, &BOB, &BOB, &HeldAsset::Xrp), TxResult::Success);

        let mut st = state();
        line(&mut st, ALICE, LSF_LOW_DEEP_FREEZE | LSF_HIGH_FREEZE, "50", "1000");
        let sb = Sandbox::new(&st);
        assert_eq!(check_withdraw_freeze(&sb, &PSEUDO, &ALICE, &ALICE, &USD), TxResult::Frozen, "deep-frozen destination");
        assert_eq!(check_withdraw_freeze(&sb, &PSEUDO, &BOB, &ALICE, &USD), TxResult::Frozen);

        let mut st = state();
        line(&mut st, PSEUDO, LSF_LOW_FREEZE, "50", "0");
        let sb = Sandbox::new(&st);
        assert_eq!(check_withdraw_freeze(&sb, &PSEUDO, &ALICE, &ALICE, &USD), TxResult::Frozen, "the vault's line is frozen");
        assert_eq!(check_withdraw_freeze(&sb, &PSEUDO, &ALICE, &ISSUER, &USD), TxResult::Success);

        let mut st = state();
        root_in(&mut st, ISSUER, LSF_DEFAULT_RIPPLE | LSF_GLOBAL_FREEZE, json!({}));
        let sb = Sandbox::new(&st);
        assert_eq!(check_withdraw_freeze(&sb, &PSEUDO, &ALICE, &ALICE, &USD), TxResult::Frozen);
        assert_eq!(check_withdraw_freeze(&sb, &PSEUDO, &ALICE, &ISSUER, &USD), TxResult::Success, "the issuer under a global freeze");

        let mut st = state();
        let id = issuance(&mut st, mpt::LSF_MPT_CAN_TRANSFER, json!({}));
        token(&mut st, &id, ALICE, mpt::LSF_MPT_LOCKED, 5);
        token(&mut st, &id, PSEUDO, 0, 5);
        let sb = Sandbox::new(&st);
        // An MPT has no deeper freeze: a locked holder cannot receive either.
        assert_eq!(check_withdraw_freeze(&sb, &PSEUDO, &ALICE, &ALICE, &HeldAsset::Mpt(id)), TxResult::Locked);
        assert_eq!(check_withdraw_freeze(&sb, &PSEUDO, &ALICE, &BOB, &HeldAsset::Mpt(id)), TxResult::Locked);
        assert_eq!(check_withdraw_freeze(&sb, &PSEUDO, &BOB, &BOB, &HeldAsset::Mpt(id)), TxResult::Success);
        assert_eq!(check_withdraw_freeze(&sb, &PSEUDO, &ALICE, &ISSUER, &HeldAsset::Mpt(id)), TxResult::Success);
    }

    /// MPTokenHelpers.cpp:603-656: `WaiveMPTCanTransfer::Yes` passes an MPT
    /// without lsfMPTCanTransfer (VaultWithdraw under fixCleanup3_2_0); a
    /// missing issuance is still tecOBJECT_NOT_FOUND, and an IOU's rippling
    /// rule is not waived.
    #[test]
    fn can_transfer_waived_skips_only_the_mpt_flag() {
        let mut st = state();
        root_in(&mut st, ISSUER, 0, json!({}));
        line(&mut st, BOB, LSF_HIGH_NO_RIPPLE, "50", "1000");
        let fixed = issuance(&mut st, 0, json!({}));
        let sb = Sandbox::new(&st);
        let asset = HeldAsset::Mpt(fixed);
        assert_eq!(can_transfer_waive(&sb, &asset, &ALICE, &BOB, false), TxResult::NoAuth);
        assert_eq!(can_transfer_waive(&sb, &asset, &ALICE, &BOB, true), TxResult::Success);
        assert_eq!(can_transfer_waive(&sb, &HeldAsset::Mpt(mptid(99, ISSUER)), &ALICE, &BOB, true), TxResult::ObjectNotFound);
        assert_eq!(can_transfer_waive(&sb, &USD, &BOB, &PSEUDO, true), TxResult::NoRipple);
        assert_eq!(can_transfer(&sb, &asset, &ALICE, &BOB), TxResult::NoAuth, "the default is not waived");
    }

    /// TokenHelpers.cpp:325-476 with `SpendableHandling::FullBalance`: an
    /// IOU holder's balance PLUS the limit the issuer set on its own side of
    /// the line; the IOU issuer the largest IOU amount; an MPT issuer what it
    /// may still mint; XRP and an MPT holder as the simple balance.
    #[test]
    fn account_holds_full_adds_what_the_issuer_allows() {
        let _scale = large();
        let mut st = state();
        // ALICE is low: her limit is LowLimit, the issuer's is HighLimit.
        let key = line(&mut st, ALICE, 0, "50", "1000");
        let mut l: Value = serde_json::from_slice(&st.state_map.lookup(&key).unwrap()).unwrap();
        l["HighLimit"]["value"] = json!("25");
        put(&mut st, key, &l);
        let id = issuance(&mut st, mpt::LSF_MPT_CAN_TRANSFER, json!({"MaximumAmount": "5000"}));
        token(&mut st, &id, ALICE, 0, 7);
        let sb = Sandbox::new(&st);
        assert_eq!(account_holds_full(&sb, &ALICE, &USD, true, true).unwrap(), usd("75"));
        assert_eq!(account_holds(&sb, &ALICE, &USD, true, true), usd("50"), "SimpleBalance is the line alone");
        let issuer = account_holds_full(&sb, &ISSUER, &USD, true, true).unwrap();
        assert_eq!((issuer.mantissa, issuer.exponent, issuer.negative), (9_999_999_999_999_999, 80, false));
        assert_eq!(account_holds_full(&sb, &BOB, &USD, true, true).unwrap(), Amount::zero(USD), "no line");
        assert_eq!(account_holds_full(&sb, &ISSUER, &HeldAsset::Mpt(id), true, true).unwrap(), units(&id, 4000), "5000 - 1000 outstanding");
        assert_eq!(account_holds_full(&sb, &ALICE, &HeldAsset::Mpt(id), true, true).unwrap(), units(&id, 7));
        assert_eq!(
            account_holds_full(&sb, &ISSUER, &HeldAsset::Mpt(mptid(99, ISSUER)), true, true).unwrap(),
            units(&mptid(99, ISSUER), 0),
            "no issuance"
        );
        assert_eq!(account_holds_full(&sb, &ALICE, &HeldAsset::Xrp, true, true).unwrap(), account_holds(&sb, &ALICE, &HeldAsset::Xrp, true, true));

        // A frozen line reads zero with ZeroIfFrozen — the limit is not added.
        let mut st = state();
        let key = line(&mut st, ALICE, LSF_HIGH_FREEZE, "50", "1000");
        let mut l: Value = serde_json::from_slice(&st.state_map.lookup(&key).unwrap()).unwrap();
        l["HighLimit"]["value"] = json!("25");
        put(&mut st, key, &l);
        let sb = Sandbox::new(&st);
        assert_eq!(account_holds_full(&sb, &ALICE, &USD, true, true).unwrap(), Amount::zero(USD));
        assert_eq!(account_holds_full(&sb, &ALICE, &USD, false, true).unwrap(), usd("75"));
    }

    /// 3.5.0-rc1 TokenHelpers.cpp:541-569 and STAmount.cpp:1766-1782:
    /// `addTransferFee` is the amount times rate / 1e9 rounded UP,
    /// `subtractTransferFee` the amount times 1e9 / rate rounded DOWN, both
    /// exact (`mulRatio`); parity returns the amount.
    #[test]
    fn transfer_fee_is_added_up_and_subtracted_down() {
        let _scale = large();
        let mut st = state();
        root_in(&mut st, ISSUER, LSF_DEFAULT_RIPPLE, json!({"TransferRate": 1_250_000_000u64}));
        let id = issuance(&mut st, mpt::LSF_MPT_CAN_TRANSFER, json!({"TransferFee": 1000}));
        let sb = Sandbox::new(&st);
        assert_eq!(asset_transfer_rate(&sb, &HeldAsset::Xrp), 1_000_000_000);
        assert_eq!(asset_transfer_rate(&sb, &USD), 1_250_000_000);
        assert_eq!(asset_transfer_rate(&sb, &HeldAsset::Mpt(id)), 1_010_000_000, "1000 = 1%");
        assert_eq!(asset_transfer_rate(&sb, &HeldAsset::Mpt(mptid(99, ISSUER))), 1_000_000_000, "no issuance");

        assert_eq!(add_transfer_fee(&usd("100"), 1_250_000_000).unwrap(), usd("125"));
        assert_eq!(subtract_transfer_fee(&usd("125"), 1_250_000_000).unwrap(), usd("100"));
        assert_eq!(add_transfer_fee(&usd("100"), 1_000_000_000).unwrap(), usd("100"));
        // 1/1.25 of 1 = 0.8; of 0.0000000000000001e-80 it underflows towards zero.
        assert_eq!(subtract_transfer_fee(&usd("1"), 1_250_000_000).unwrap(), usd("0.8"));
        // MPT: whole units. 1001 × 1.01 = 1011.01 → 1012 up; 1012 / 1.01 = 1001.98… → 1001 down.
        assert_eq!(add_transfer_fee(&units(&id, 1001), 1_010_000_000).unwrap(), units(&id, 1012));
        assert_eq!(subtract_transfer_fee(&units(&id, 1012), 1_010_000_000).unwrap(), units(&id, 1001));
        assert_eq!(subtract_transfer_fee(&units(&id, 1), 1_010_000_000).unwrap(), units(&id, 0), "the fee eats one unit");
        assert!(add_transfer_fee(&units(&id, mpt::MAX_MPT_AMOUNT), 2_000_000_000).is_err(), "past the largest MPT amount");
    }

    /// 3.5.0-rc1 View.cpp:507-588: with a source amount above the delivered
    /// one the payout goes source → issuer for the gross and issuer →
    /// destination for the net, each a direct leg; equal amounts take the old
    /// path. The source must hold the gross amount.
    #[test]
    fn do_withdraw_split_redeems_the_gross_and_issues_the_net() {
        let _scale = large();
        let mut st = state();
        line(&mut st, PSEUDO, 0, "500", "0");
        line(&mut st, BOB, 0, "0", "1000");
        let tx = withdraw_tx(json!({"Destination": hex::encode(BOB)}));
        let mut sb = Sandbox::new(&st);
        assert_eq!(do_withdraw_split(&mut sb, &tx, &ALICE, &BOB, &PSEUDO, BALANCE, &usd("80"), &usd("100")), TxResult::Success);
        assert_eq!(account_holds(&sb, &PSEUDO, &USD, false, false), usd("400"), "the vault paid the gross 100");
        assert_eq!(account_holds(&sb, &BOB, &USD, false, false), usd("80"), "BOB got the net 80");
        assert_eq!(
            do_withdraw_split(&mut sb, &tx, &ALICE, &BOB, &PSEUDO, BALANCE, &usd("400"), &usd("401")),
            TxResult::TefInternal,
            "the source holds less than the gross"
        );
        // Equal amounts: the plain send.
        assert_eq!(do_withdraw_split(&mut sb, &tx, &ALICE, &BOB, &PSEUDO, BALANCE, &usd("10"), &usd("10")), TxResult::Success);
        assert_eq!(account_holds(&sb, &PSEUDO, &USD, false, false), usd("390"));
        assert_eq!(account_holds(&sb, &BOB, &USD, false, false), usd("90"));
    }

    /// 3.5.0-rc1 STAmount.cpp:448-462 (`isRepresentable`): whether
    /// `STAmount{asset, value}` constructs — XRP up to 1e17 drops, an MPT up
    /// to the largest MPT amount, an IOU up to exponent 80.
    #[test]
    fn is_representable_is_the_assets_range() {
        let _scale = large();
        let n = |m: u128, e: i32| crate::tx::number::Number::from_parts(false, m, e, crate::tx::number::Rounding::ToNearest).unwrap();
        assert!(is_representable(&HeldAsset::Xrp, n(1, 17)));
        assert!(!is_representable(&HeldAsset::Xrp, n(1_000_000_000_000_000_01, 0)), "1e17 + 1 drops");
        assert!(is_representable(&HeldAsset::Xrp, n(0, 0)));
        let id = HeldAsset::Mpt(mptid(7, ISSUER));
        assert!(is_representable(&id, n(mpt::MAX_MPT_AMOUNT as u128, 0)));
        // 2^63 itself is not a Large330 mantissa: Number rounds it to the nearest one, 2^63 - 1
        // (Number.h:367 `kMaxRepUp`), which is the largest MPT amount.
        assert!(is_representable(&id, n(mpt::MAX_MPT_AMOUNT as u128 + 1, 0)));
        assert!(!is_representable(&id, n(mpt::MAX_MPT_AMOUNT as u128 + 4, 0)), "rounds to ...810");
        assert!(!is_representable(&id, n(1, 19)));
        assert!(is_representable(&USD, n(9_999_999_999_999_999, 80)));
        assert!(!is_representable(&USD, n(1, 97)));
        assert!(is_representable(&USD, n(1, -200)), "an IOU underflow is zero, not a throw");
    }
}
