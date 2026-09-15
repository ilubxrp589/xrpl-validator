//! The ledger reads rippled's path engine makes through `View.cpp` /
//! `RippleStateHelpers.cpp` — `accountHolds`, `accountFunds`, `xrpLiquid`,
//! `transferRate`, `isFrozen`, `isDeepFrozen`, `requireAuth` — and the
//! directory walk `BookTip` relies on (`succ`, `dirFirst`), over the
//! `PaymentSandbox` so every funds question passes through its hooks.
//!
//! The engine in `tx::offer` answers the same questions with `available()`,
//! `require_auth_known()` and the thread-local deferred-credit and
//! owner-count tables; this file answers them the way rippled does, with the
//! sandbox's tables, so the port has no hidden state.
use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::tx::offer::{decode20, json_at, signed_value};
use xrpl_core::types::Hash256;

use super::amounts::{IouAmount, XrpAmount};
use super::payment_sandbox::{OwnerCounts, PaymentSandbox};
use super::steps::Asset;

pub const LSF_GLOBAL_FREEZE: u64 = 0x0040_0000;
pub const LSF_REQUIRE_AUTH: u64 = 0x0004_0000;
const LSF_LOW_FREEZE: u64 = 0x0040_0000;
const LSF_HIGH_FREEZE: u64 = 0x0080_0000;
const LSF_LOW_DEEP_FREEZE: u64 = 0x0200_0000;
const LSF_HIGH_DEEP_FREEZE: u64 = 0x0400_0000;
const LSF_LOW_AUTH: u64 = 0x0004_0000;
const LSF_HIGH_AUTH: u64 = 0x0008_0000;
/// `QUALITY_ONE` / a parity transfer rate.
pub const QUALITY_ONE: u32 = 1_000_000_000;

/// `FreezeHandling` / `AuthHandling` (View.h).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FreezeHandling {
    ZeroIfFrozen,
    IgnoreFreeze,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AuthHandling {
    ZeroIfUnauthorized,
    IgnoreAuth,
}

/// `isGlobalFrozen(view, issuer)`.
pub fn is_global_frozen(sb: &Sandbox, issuer: &[u8; 20]) -> bool {
    json_at(sb, &keylet::account_root_key(issuer))
        .is_some_and(|a| a["Flags"].as_u64().unwrap_or(0) & LSF_GLOBAL_FREEZE != 0)
}

/// `isFrozen(view, account, currency, issuer)` (RippleStateHelpers.cpp):
/// the issuer's global freeze, or the ISSUER's side of the line frozen.
pub fn is_frozen(sb: &Sandbox, account: &[u8; 20], asset: &Asset) -> bool {
    let Some(issuer) = asset.issuer else { return false };
    if is_global_frozen(sb, &issuer) {
        return true;
    }
    let Some(line) = json_at(sb, &keylet::ripple_state_key(account, &issuer, &asset.currency)) else { return false };
    let bit = if issuer > *account { LSF_HIGH_FREEZE } else { LSF_LOW_FREEZE };
    line["Flags"].as_u64().unwrap_or(0) & bit != 0
}

/// `isDeepFrozen(view, account, currency, issuer)`: either side's deep
/// freeze on the line (the holder may neither send nor receive).
pub fn is_deep_frozen(sb: &Sandbox, account: &[u8; 20], asset: &Asset) -> bool {
    let Some(issuer) = asset.issuer else { return false };
    let Some(line) = json_at(sb, &keylet::ripple_state_key(account, &issuer, &asset.currency)) else { return false };
    line["Flags"].as_u64().unwrap_or(0) & (LSF_LOW_DEEP_FREEZE | LSF_HIGH_DEEP_FREEZE) != 0
}

/// `requireAuth(view, issue, account)` as a boolean: true when the holder
/// is authorised (or the issuer requires none, or IS the holder).
pub fn is_authorized(sb: &Sandbox, asset: &Asset, account: &[u8; 20]) -> bool {
    let Some(issuer) = asset.issuer else { return true };
    if *account == issuer {
        return true;
    }
    let Some(iss) = json_at(sb, &keylet::account_root_key(&issuer)) else { return true };
    if iss["Flags"].as_u64().unwrap_or(0) & LSF_REQUIRE_AUTH == 0 {
        return true;
    }
    match json_at(sb, &keylet::ripple_state_key(account, &issuer, &asset.currency)) {
        Some(line) => {
            let bit = if issuer < *account { LSF_LOW_AUTH } else { LSF_HIGH_AUTH };
            line["Flags"].as_u64().unwrap_or(0) & bit != 0
        }
        None => false,
    }
}

/// The holder's balance on a line, from the holder's side (positive = the
/// holder is owed).
pub fn line_balance(sb: &Sandbox, account: &[u8; 20], asset: &Asset) -> IouAmount {
    let Some(issuer) = asset.issuer else { return IouAmount::ZERO };
    let Some(line) = json_at(sb, &keylet::ripple_state_key(account, &issuer, &asset.currency)) else {
        return IouAmount::ZERO;
    };
    let (neg, bal) = signed_value(&line["Balance"]);
    let party_low = *account < issuer;
    // The stored balance is from the LOW account's side.
    let holder_neg = if party_low { neg && bal.0 > 0 } else { !neg && bal.0 > 0 };
    IouAmount::from_me(holder_neg, bal)
}

/// `accountHolds(view, account, currency, issuer, freeze, auth)` for an
/// IOU: the holder's positive balance, zeroed when frozen or unauthorised
/// (per the handling flags), then through `balanceHookIOU`.
pub fn account_holds_iou(ps: &PaymentSandbox, account: &[u8; 20], asset: &Asset, freeze: FreezeHandling, auth: AuthHandling) -> IouAmount {
    let sb = ps.sandbox();
    let Some(issuer) = asset.issuer else { return IouAmount::ZERO };
    if freeze == FreezeHandling::ZeroIfFrozen && is_frozen(sb, account, asset) {
        return IouAmount::ZERO;
    }
    if auth == AuthHandling::ZeroIfUnauthorized && !is_authorized(sb, asset, account) {
        return IouAmount::ZERO;
    }
    let bal = line_balance(sb, account, asset);
    if bal <= IouAmount::ZERO {
        return IouAmount::ZERO;
    }
    ps.balance_hook_iou(account, &issuer, &asset.currency, bal)
}

/// `OwnerCounts` of an account root.
pub fn owner_counts(sb: &Sandbox, account: &[u8; 20]) -> OwnerCounts {
    let Some(root) = json_at(sb, &keylet::account_root_key(account)) else { return OwnerCounts::default() };
    OwnerCounts {
        owner: root["OwnerCount"].as_u64().unwrap_or(0) as u32,
        sponsored: root["SponsoredOwnerCount"].as_u64().unwrap_or(0) as u32,
        sponsoring: root["SponsoringOwnerCount"].as_u64().unwrap_or(0) as u32,
    }
}

/// `xrpLiquid(view, account, ownerCountAdj, j)`: the balance less the
/// reserve at the owner count the sandbox remembers (`ownerCountHook`),
/// floored at zero. The reserve is the ledger's (fees.rs).
pub fn xrp_liquid(ps: &PaymentSandbox, account: &[u8; 20], owner_count_adj: i64) -> XrpAmount {
    let sb = ps.sandbox();
    let Some(root) = json_at(sb, &keylet::account_root_key(account)) else { return 0 };
    let balance: i128 = root["Balance"].as_str().and_then(|s| s.parse::<i128>().ok()).unwrap_or(0);
    let counts = ps.owner_count_hook(account, owner_counts(sb, account));
    let count = (counts.count() as i64 + owner_count_adj).max(0) as u64;
    let reserve = crate::ledger::fees::account_reserve(sb, count) as i128;
    let liquid = balance - reserve;
    if liquid < 0 { 0 } else { liquid }
}

/// `accountFunds` for an offer owner selling `asset` (OfferStream's
/// `accountFundsHelper`): the issuer of an IOU has unlimited funds —
/// `amtDefault` (the offer's own TakerGets) — everyone else `accountHolds`.
pub fn account_funds_iou(ps: &PaymentSandbox, account: &[u8; 20], asset: &Asset, amt_default: IouAmount, freeze: FreezeHandling, auth: AuthHandling) -> IouAmount {
    if asset.issuer == Some(*account) {
        return amt_default;
    }
    account_holds_iou(ps, account, asset, freeze, auth)
}

/// `transferRate(view, issuer)`: the issuer's TransferRate, QUALITY_ONE
/// when absent, zero, or the issuer is XRP.
pub fn transfer_rate(sb: &Sandbox, issuer: &[u8; 20]) -> u32 {
    if issuer == &[0u8; 20] {
        return QUALITY_ONE;
    }
    json_at(sb, &keylet::account_root_key(issuer))
        .and_then(|a| a["TransferRate"].as_u64())
        .filter(|r| *r != 0)
        .map(|r| r as u32)
        .unwrap_or(QUALITY_ONE)
}

/// `getBookBase(book)`: the 24-byte prefix every page of a book shares.
pub fn book_base(input: &Asset, output: &Asset, domain: Option<&Hash256>) -> Hash256 {
    let (pi, gi) = (input.issuer.unwrap_or([0; 20]), output.issuer.unwrap_or([0; 20]));
    match domain {
        Some(d) => keylet::book_base_domain(&input.currency, &output.currency, &pi, &gi, d),
        None => keylet::book_base(&input.currency, &output.currency, &pi, &gi),
    }
}

/// `view.succ(key, last)`: the first existing key strictly greater than
/// `key` and below `last`, within the book's 24-byte prefix. Directory
/// pages are the only keys under that prefix.
pub fn succ_in_book(sb: &Sandbox, base: &Hash256, key: &Hash256, last: &Hash256) -> Option<Hash256> {
    let mut keys = sb.keys_with_prefix(&base.0[..24]);
    keys.sort_by(|a, b| a.0.cmp(&b.0));
    keys.into_iter().find(|k| k.0 > key.0 && k.0 < last.0)
}

/// `getQualityNext(bookBase)`: the first key past this book (prefix + 1).
pub fn quality_next(base: &Hash256) -> Hash256 {
    let mut k = base.0;
    k[24..32].copy_from_slice(&u64::MAX.to_be_bytes());
    // The book's last possible page is prefix||0xFF..; `succ` must stop AT
    // that bound (`< last`), so hand back one past it by carrying into the
    // prefix.
    let mut carry = true;
    for b in k[..24].iter_mut().rev() {
        if !carry {
            break;
        }
        let (v, c) = b.overflowing_add(1);
        *b = v;
        carry = c;
    }
    k[24..32].copy_from_slice(&[0u8; 8]);
    Hash256(k)
}

/// `dirFirst(view, root, page, entry, index)`: the first entry of the
/// directory rooted at `root`, walking IndexNext pages past empty ones.
/// Returns (page key, entry index).
pub fn dir_first(sb: &Sandbox, root: &Hash256) -> Option<(Hash256, Hash256)> {
    let mut page_key = *root;
    for _ in 0..10_000 {
        let page = json_at(sb, &page_key)?;
        if let Some(first) = page.get("Indexes").and_then(|v| v.as_array()).and_then(|a| a.first()) {
            let k = first.as_str().and_then(|s| hex::decode(s).ok()).and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())?;
            return Some((page_key, Hash256(k)));
        }
        let next = page.get("IndexNext").and_then(|v| v.as_str()).and_then(|s| u64::from_str_radix(s, 16).ok()).unwrap_or(0);
        if next == 0 {
            return None;
        }
        page_key = keylet::dir_page_key(root, next);
    }
    None
}

/// `getQuality(dirKey)`: the low 64 bits of a book page key.
pub fn page_quality(dir_key: &Hash256) -> u64 {
    u64::from_be_bytes(dir_key.0[24..32].try_into().unwrap_or_default())
}

/// The fields of an Offer entry the stream needs.
#[derive(Clone, Debug)]
pub struct OfferEntry {
    pub key: Hash256,
    pub owner: [u8; 20],
    pub asset_in: Asset,
    pub asset_out: Asset,
    /// TakerPays (what the owner receives — the step's IN).
    pub taker_pays: (bool, (u128, i32)),
    /// TakerGets (what the owner gives — the step's OUT).
    pub taker_gets: (bool, (u128, i32)),
    pub expiration: Option<u64>,
    pub domain: Option<Hash256>,
    pub json: serde_json::Value,
}

fn amount_asset(v: &serde_json::Value) -> Option<Asset> {
    if v.is_string() {
        return Some(Asset::XRP);
    }
    let cur = v.get("currency").and_then(|c| c.as_str()).and_then(|s| {
        let raw = hex::decode(s).ok()?;
        <[u8; 20]>::try_from(raw.as_slice()).ok()
    })?;
    let issuer = v.get("issuer").and_then(|i| i.as_str()).and_then(decode20)?;
    Some(Asset { currency: cur, issuer: Some(issuer) })
}

/// Read an Offer ledger entry (None when the key holds anything else).
pub fn read_offer(sb: &Sandbox, key: &Hash256) -> Option<OfferEntry> {
    let json = json_at(sb, key)?;
    if json.get("LedgerEntryType").and_then(|t| t.as_str()) != Some("Offer") {
        return None;
    }
    let owner = json.get("Account").and_then(|v| v.as_str()).and_then(decode20)?;
    let tp = json.get("TakerPays")?;
    let tg = json.get("TakerGets")?;
    let asset_in = amount_asset(tp)?;
    let asset_out = amount_asset(tg)?;
    let taker_pays = signed_value(tp);
    let taker_gets = signed_value(tg);
    let expiration = json.get("Expiration").and_then(|v| v.as_u64());
    let domain = json.get("DomainID").and_then(|v| v.as_str()).and_then(|s| hex::decode(s).ok()).and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok()).map(Hash256);
    Some(OfferEntry { key: *key, owner, asset_in, asset_out, taker_pays, taker_gets, expiration, domain, json })
}
