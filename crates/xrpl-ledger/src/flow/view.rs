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

use super::amounts::{EitherAmount, IouAmount, XrpAmount};
use super::steps::DebtDirection;
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
    if freeze == FreezeHandling::ZeroIfFrozen && (is_frozen(sb, account, asset) || is_deep_frozen(sb, account, asset)) {
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

/// `accountHolds` with its sign: `getTrustLineBalance` (the holder's side
/// of the line, no opposite limit) through `balanceHook`. A missing line
/// is zero; `ZeroIfFrozen` drops a frozen or deep-frozen line.
pub fn account_holds_signed_iou(ps: &PaymentSandbox, account: &[u8; 20], asset: &Asset, freeze: FreezeHandling) -> IouAmount {
    let sb = ps.sandbox();
    let Some(issuer) = asset.issuer else { return IouAmount::ZERO };
    if json_at(sb, &keylet::ripple_state_key(account, &issuer, &asset.currency)).is_none() {
        return ps.balance_hook_iou(account, &issuer, &asset.currency, IouAmount::ZERO);
    }
    if freeze == FreezeHandling::ZeroIfFrozen && (is_frozen(sb, account, asset) || is_deep_frozen(sb, account, asset)) {
        return ps.balance_hook_iou(account, &issuer, &asset.currency, IouAmount::ZERO);
    }
    let bal = line_balance(sb, account, asset);
    ps.balance_hook_iou(account, &issuer, &asset.currency, bal)
}

/// `QualityDirection`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum QualityDirection {
    In,
    Out,
}

/// `DirectIPaymentStep::quality`: the line `dst`–`src`'s QualityIn (from
/// the destination's side) or QualityOut (from the source's side);
/// QUALITY_ONE when absent or zero.
pub fn line_quality(sb: &Sandbox, src: &[u8; 20], dst: &[u8; 20], currency: &[u8; 20], dir: QualityDirection) -> u32 {
    let Some(line) = json_at(sb, &keylet::ripple_state_key(dst, src, currency)) else { return QUALITY_ONE };
    let field = match dir {
        QualityDirection::In => {
            if dst < src { "LowQualityIn" } else { "HighQualityIn" }
        }
        QualityDirection::Out => {
            if src < dst { "LowQualityOut" } else { "HighQualityOut" }
        }
    };
    match line.get(field).and_then(|v| v.as_u64()) {
        Some(q) if q != 0 => q as u32,
        _ => QUALITY_ONE,
    }
}

/// `creditLimit2(view, account, issuer, currency)`: the limit `account`
/// extends to `issuer` on their line (zero without a line).
pub fn credit_limit(sb: &Sandbox, account: &[u8; 20], issuer: &[u8; 20], currency: &[u8; 20]) -> IouAmount {
    let Some(line) = json_at(sb, &keylet::ripple_state_key(account, issuer, currency)) else { return IouAmount::ZERO };
    let field = if account < issuer { "LowLimit" } else { "HighLimit" };
    let (neg, m) = signed_value(&line[field]);
    IouAmount::from_me(neg && m.0 > 0, m)
}

/// `creditBalance(view, account, issuer, currency)`: the line's balance
/// from `account`'s side.
pub fn credit_balance(sb: &Sandbox, account: &[u8; 20], issuer: &[u8; 20], currency: &[u8; 20]) -> IouAmount {
    let Some(line) = json_at(sb, &keylet::ripple_state_key(account, issuer, currency)) else { return IouAmount::ZERO };
    let (neg, m) = signed_value(&line["Balance"]);
    // Stored from the low account's side; `account < issuer` negates.
    let account_neg = if account < issuer { !neg && m.0 > 0 } else { neg && m.0 > 0 };
    IouAmount::from_me(account_neg, m)
}

/// The step checks' error codes (`StepChecks.h`, `DirectStep.cpp`,
/// `XRPEndpointStep.cpp`, `BookStep.cpp`): the strand builder maps them
/// to their TERs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StepCheckError {
    /// temBAD_PATH
    BadPath,
    /// temBAD_PATH_LOOP
    BadPathLoop,
    /// terNO_LINE
    NoLine,
    /// terNO_RIPPLE
    NoRipple,
    /// terNO_ACCOUNT
    NoAccount,
    /// terNO_AUTH
    NoAuth,
    /// tecPATH_DRY
    PathDry,
    /// tecNO_ISSUER
    NoIssuer,
}

/// `checkFreeze(view, src, dst, currency)`: the destination's global
/// freeze, or its side of the `src`–`dst` line frozen, or either side
/// deep-frozen → terNO_LINE.
pub fn check_freeze(sb: &Sandbox, src: &[u8; 20], dst: &[u8; 20], currency: &[u8; 20]) -> Result<(), StepCheckError> {
    if dst != &[0u8; 20] {
        if let Some(root) = json_at(sb, &keylet::account_root_key(dst)) {
            if root["Flags"].as_u64().unwrap_or(0) & LSF_GLOBAL_FREEZE != 0 {
                return Err(StepCheckError::NoLine);
            }
        }
    }
    if src != &[0u8; 20] && dst != &[0u8; 20] {
        if let Some(line) = json_at(sb, &keylet::ripple_state_key(src, dst, currency)) {
            let flags = line["Flags"].as_u64().unwrap_or(0);
            let bit = if dst > src { LSF_HIGH_FREEZE } else { LSF_LOW_FREEZE };
            if flags & bit != 0 {
                return Err(StepCheckError::NoLine);
            }
            if flags & (LSF_LOW_DEEP_FREEZE | LSF_HIGH_DEEP_FREEZE) != 0 {
                return Err(StepCheckError::NoLine);
            }
        }
    }
    Ok(())
}

/// `checkNoRipple(view, prev, cur, next, currency)`: both of `cur`'s lines
/// flagged NoRipple on its side → terNO_RIPPLE; a missing line → terNO_LINE.
pub fn check_no_ripple(sb: &Sandbox, prev: &[u8; 20], cur: &[u8; 20], next: &[u8; 20], currency: &[u8; 20]) -> Result<(), StepCheckError> {
    let Some(line_in) = json_at(sb, &keylet::ripple_state_key(prev, cur, currency)) else { return Err(StepCheckError::NoLine) };
    let Some(line_out) = json_at(sb, &keylet::ripple_state_key(cur, next, currency)) else { return Err(StepCheckError::NoLine) };
    let bit_in: u64 = if cur > prev { 0x0020_0000 } else { 0x0010_0000 };
    let bit_out: u64 = if cur > next { 0x0020_0000 } else { 0x0010_0000 };
    if line_in["Flags"].as_u64().unwrap_or(0) & bit_in != 0 && line_out["Flags"].as_u64().unwrap_or(0) & bit_out != 0 {
        return Err(StepCheckError::NoRipple);
    }
    Ok(())
}

/// The `authField` test of `DirectIPaymentStep::check`: the line's auth
/// bit on the SOURCE's (issuer's) side.
pub fn is_authorized_line_flag(line: &serde_json::Value, src: &[u8; 20], dst: &[u8; 20]) -> bool {
    let bit = if src > dst { LSF_HIGH_AUTH } else { LSF_LOW_AUTH };
    line["Flags"].as_u64().unwrap_or(0) & bit != 0
}

/// `mulRatio(IOUAmount, num, den, roundUp)`.
pub fn mul_ratio_iou(a: IouAmount, num: u32, den: u32, round_up: bool) -> IouAmount {
    match mul_ratio_either(EitherAmount::Iou(a), num, den, round_up) {
        EitherAmount::Iou(v) => v,
        EitherAmount::Xrp(_) => a,
    }
}

/// `rippleCredit` for the steps (the DirectStep's transfer).
pub fn ripple_credit_pub(ps: &mut PaymentSandbox, sender: &[u8; 20], receiver: &[u8; 20], asset: &Asset, amount: IouAmount) {
    ripple_credit(ps, sender, receiver, asset, amount)
}

#[allow(dead_code)]
fn _dir(_: DebtDirection) {}

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
    // `xrpLiquid`: fullBalance − reserve, then `balanceHook(account,
    // xrpAccount(), balance)` clamps by the deferred credits, floor zero.
    let liquid = balance - reserve;
    let hooked = ps.balance_hook_xrp(account, liquid);
    if hooked < 0 { 0 } else { hooked }
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
    let mut k = match domain {
        Some(d) => keylet::book_base_domain(&input.currency, &output.currency, &pi, &gi, d),
        None => keylet::book_base(&input.currency, &output.currency, &pi, &gi),
    };
    // `getBookBase` returns `getQualityIndex(hash)`: the low 64 bits — the
    // quality — zeroed, so `succ(base, next)` starts BEFORE the best page.
    k.0[24..32].copy_from_slice(&[0u8; 8]);
    k
}

/// `view.succ(key, last)`: the first existing key strictly greater than
/// `key` and below `last`, within the book's 24-byte prefix. Directory
/// pages are the only keys under that prefix.
pub fn succ_in_book(sb: &Sandbox, base: &Hash256, key: &Hash256, last: &Hash256) -> Option<Hash256> {
    let mut keys = sb.keys_with_prefix(&base.0[..24]);
    if std::env::var("XRPL_FLOW_TRACE").is_ok() {
        eprintln!("FLOW   succ base={} keys={} after={} last={} first_key={}", hex::encode(base.0), keys.len(), hex::encode(key.0), hex::encode(last.0), keys.first().map(|k| hex::encode(k.0)).unwrap_or_default());
    }
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
        let page = json_at(sb, &page_key);
        if std::env::var("XRPL_FLOW_TRACE").is_ok() {
            eprintln!("FLOW   dir_first page={} present={} indexes={:?}", hex::encode(&page_key.0[24..]), page.is_some(), page.as_ref().and_then(|p| p.get("Indexes")).map(|v| v.to_string().chars().take(80).collect::<String>()));
        }
        let page = page?;
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
        if s.len() == 40 {
            let raw = hex::decode(s).ok()?;
            <[u8; 20]>::try_from(raw.as_slice()).ok()
        } else if s.len() == 3 && s != "XRP" {
            // A standard three-letter code: bytes 12..15 of the 20-byte form.
            let mut c = [0u8; 20];
            c[12..15].copy_from_slice(s.as_bytes());
            Some(c)
        } else {
            None
        }
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

/// `mulRatio(amt, num, den, roundUp)` for either amount: XRP is the exact
/// 128-bit quotient rounded away from zero (roundUp, positive) or toward
/// it; an IOU is IOUAmount.cpp's scaled quotient — the engine's
/// `mul_ratio` port (nearest-16 normalisation, then the one-ulp bump).
pub fn mul_ratio_either(amt: EitherAmount, num: u32, den: u32, round_up: bool) -> EitherAmount {
    match amt {
        EitherAmount::Xrp(d) => {
            let neg = d < 0;
            let m = d * num as i128;
            let mut r = m / den as i128;
            if m % den as i128 != 0 {
                if !neg && round_up {
                    r += 1;
                }
                if neg && !round_up {
                    r -= 1;
                }
            }
            EitherAmount::Xrp(r)
        }
        EitherAmount::Iou(a) => {
            if a.mantissa == 0 {
                return EitherAmount::Iou(IouAmount::ZERO);
            }
            let r = crate::tx::offer::mul_ratio((a.mantissa as u128, a.exponent), num as u128, den as u128, round_up != a.negative);
            EitherAmount::Iou(IouAmount::from_me(a.negative, r))
        }
    }
}

/// `accountSendIOU`'s XRP legs: the sender's root falls (creditHook
/// with its balance), the receiver's rises (creditHook with the negated
/// balance). A sender below the amount is tecFAILED_PROCESSING; either
/// party may be the XRP account (all zero) and is then skipped.
fn xrp_send(ps: &mut PaymentSandbox, from: &[u8; 20], to: &[u8; 20], amount: i128) -> Result<(), crate::ledger::transactor::TxResult> {
    let zero = [0u8; 20];
    if from != &zero {
        let key = keylet::account_root_key(from);
        let Some(mut root) = json_at(ps.sandbox(), &key) else { return Err(crate::ledger::transactor::TxResult::NoDst) };
        let bal: i128 = root["Balance"].as_str().and_then(|s| s.parse::<i128>().ok()).unwrap_or(0);
        if bal < amount {
            return Err(crate::ledger::transactor::TxResult::Unfunded);
        }
        ps.credit_hook_xrp(from, &zero, amount, bal);
        root["Balance"] = serde_json::Value::String((bal - amount).to_string());
        crate::tx::offer::put_json(ps.view(), key, &root);
    }
    if to != &zero {
        let key = keylet::account_root_key(to);
        let Some(mut root) = json_at(ps.sandbox(), &key) else { return Err(crate::ledger::transactor::TxResult::NoDst) };
        let bal: i128 = root["Balance"].as_str().and_then(|s| s.parse::<i128>().ok()).unwrap_or(0);
        root["Balance"] = serde_json::Value::String((bal + amount).to_string());
        ps.credit_hook_xrp(&zero, to, amount, -bal);
        crate::tx::offer::put_json(ps.view(), key, &root);
    }
    Ok(())
}

/// `rippleCreditIOU(view, sender, receiver, amount)`: the line between
/// them moves by `amount` in the sender's terms, `creditHook` first with
/// the sender-side balance. The engine's `line_adjust` carries the line
/// mechanics (reserve flag, deletion, creation) for the non-issuer party.
fn ripple_credit(ps: &mut PaymentSandbox, sender: &[u8; 20], receiver: &[u8; 20], asset: &Asset, amount: IouAmount) {
    let issuer = asset.issuer.unwrap_or([0; 20]);
    let leg = crate::tx::offer::Leg { xrp: false, cur: asset.currency, issuer };
    let magnitude = (amount.mantissa as u128, amount.exponent);
    // The sender-side balance before the credit: the balance of the line
    // as the SENDER sees it (positive = the sender is owed).
    let pre = {
        let lk = keylet::ripple_state_key(sender, receiver, &asset.currency);
        match json_at(ps.sandbox(), &lk) {
            Some(line) => {
                let (neg, bal) = signed_value(&line["Balance"]);
                let sender_low = sender < receiver;
                let sender_neg = if sender_low { neg && bal.0 > 0 } else { !neg && bal.0 > 0 };
                IouAmount::from_me(sender_neg, bal)
            }
            None => IouAmount::ZERO,
        }
    };
    ps.credit_hook_iou(sender, receiver, &asset.currency, amount, pre);
    if sender == &issuer {
        crate::tx::offer::line_adjust(ps.view(), receiver, &leg, magnitude, true);
    } else if receiver == &issuer {
        crate::tx::offer::line_adjust(ps.view(), sender, &leg, magnitude, false);
    } else {
        // A line between two non-issuers (checkIssuer = false): both
        // sides move on the one line.
        crate::tx::offer::line_adjust(ps.view(), sender, &leg, magnitude, false);
        crate::tx::offer::line_adjust(ps.view(), receiver, &leg, magnitude, true);
    }
}

/// `accountSend(view, from, to, amount, waiveFee)` — `accountSendIOU`:
/// nothing for a zero amount or `from == to`; XRP moves between roots;
/// an IOU goes `rippleSendIOU`: straight `rippleCredit` when either party
/// is the issuer, otherwise through the issuer with the sender paying
/// `multiply(amount, transferRate(issuer))` unless the fee is waived.
pub fn account_send(ps: &mut PaymentSandbox, from: &[u8; 20], to: &[u8; 20], asset: &Asset, amount: EitherAmount) -> Result<(), crate::ledger::transactor::TxResult> {
    account_send_fee(ps, from, to, asset, amount, false)
}

pub fn account_send_fee(ps: &mut PaymentSandbox, from: &[u8; 20], to: &[u8; 20], asset: &Asset, amount: EitherAmount, waive_fee: bool) -> Result<(), crate::ledger::transactor::TxResult> {
    if amount.is_zero() || from == to {
        return Ok(());
    }
    match amount {
        EitherAmount::Xrp(d) => xrp_send(ps, from, to, d),
        EitherAmount::Iou(a) => {
            let issuer = asset.issuer.unwrap_or([0; 20]);
            if from == &issuer || to == &issuer {
                ripple_credit(ps, from, to, asset, a);
                return Ok(());
            }
            let actual = if waive_fee {
                a
            } else {
                let rate = transfer_rate(ps.sandbox(), &issuer);
                if rate == QUALITY_ONE {
                    a
                } else {
                    // `multiply(saAmount, transferRate)`: STAmount × the
                    // rate as an IOU value (rate / 1e9), Number nearest.
                    let r = crate::tx::amm_swap::n_mul((a.mantissa as u128, a.exponent), (rate as u128, -9), crate::tx::amm_swap::Rnd::Near);
                    IouAmount::from_me(a.negative, r)
                }
            };
            ripple_credit(ps, &issuer, to, asset, a);
            ripple_credit(ps, from, &issuer, asset, actual);
            Ok(())
        }
    }
}
