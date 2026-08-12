//! AMM (Automated Market Maker) transaction types.
//!
//! AMMCreate, AMMDeposit, AMMWithdraw, AMMVote, AMMBid, AMMDelete.
//! XRPL's native AMM allows liquidity provision for any token pair.
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

use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{Transactor, TxFields, TxResult};
use crate::shamap::hash::sha512_half;

/// Compute a deterministic key for an AMM instance.
/// AMM key = SHA512Half(0x0041 || issue1_currency(20) || issue1_issuer(20) || issue2_currency(20) || issue2_issuer(20))
/// For XRP, issuer is all zeros and currency is all zeros.
fn amm_key(tx: &TxFields) -> Option<[u8; 32]> {
    let asset1 = tx.fields.get("Asset")?;
    let asset2 = tx.fields.get("Asset2")?;
    // rippled keylet::amm orders the issues and serializes account-first —
    // delegate to the verified keylet implementation.
    let mut b1 = Vec::with_capacity(40);
    encode_asset_to_buf(&mut b1, asset1);
    let mut b2 = Vec::with_capacity(40);
    encode_asset_to_buf(&mut b2, asset2);
    let (c1, i1): ([u8; 20], [u8; 20]) =
        (b1[..20].try_into().ok()?, b1[20..40].try_into().ok()?);
    let (c2, i2): ([u8; 20], [u8; 20]) =
        (b2[..20].try_into().ok()?, b2[20..40].try_into().ok()?);
    Some(crate::ledger::keylet::amm_key(&c1, &i1, &c2, &i2).0)
}

fn encode_asset_to_buf(buf: &mut Vec<u8>, asset: &serde_json::Value) {
    if asset.is_object() {
        // IOU: currency(20 bytes) + issuer(20 bytes)
        let currency_str = asset.get("currency").and_then(|c| c.as_str()).unwrap_or("");
        let issuer_str = asset.get("issuer").and_then(|i| i.as_str()).unwrap_or("");

        // XRP asset is encoded even inside an object as all zeros (the currency
        // code "XRP" is the zero currency, NOT the ASCII bytes X/R/P). Mainnet
        // sends XRP as {"currency":"XRP"} with no issuer.
        if currency_str == "XRP" && issuer_str.is_empty() {
            buf.extend_from_slice(&[0u8; 40]);
            return;
        }

        // Currency code: 3-char ISO -> 20 bytes (zeros + 3 chars at offset 12)
        let mut currency = [0u8; 20];
        if currency_str.len() == 3 {
            currency[12] = currency_str.as_bytes()[0];
            currency[13] = currency_str.as_bytes()[1];
            currency[14] = currency_str.as_bytes()[2];
        } else if currency_str.len() == 40 {
            if let Ok(bytes) = hex::decode(currency_str) {
                if bytes.len() == 20 { currency.copy_from_slice(&bytes); }
            }
        }
        buf.extend_from_slice(&currency);

        // Issuer: hex (internal) OR base58 classic address (mainnet JSON).
        let mut issuer = [0u8; 20];
        if let Ok(bytes) = hex::decode(issuer_str) {
            if bytes.len() == 20 { issuer.copy_from_slice(&bytes); }
        } else if let Ok(id) = xrpl_core::types::AccountId::from_address(issuer_str) {
            issuer = id.0;
        }
        buf.extend_from_slice(&issuer);
    } else {
        // XRP: 20 zero bytes (currency) + 20 zero bytes (issuer)
        buf.extend_from_slice(&[0u8; 40]);
    }
}

fn read_amm_key_from_fields(tx: &TxFields) -> Option<xrpl_core::types::Hash256> {
    let key_bytes = amm_key(tx)?;
    Some(xrpl_core::types::Hash256(key_bytes))
}

/// For AMMDeposit/Withdraw/Vote/Bid/Delete — read the AMM key from Asset+Asset2
fn amm_key_from_asset_fields(tx: &TxFields) -> Option<xrpl_core::types::Hash256> {
    read_amm_key_from_fields(tx)
}

// ─── AMMCreate ───

pub struct AMMCreateTransactor;

impl Transactor for AMMCreateTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "AMMCreate" { return TxResult::Malformed; }
        if tx.fee == 0 { return TxResult::BadFee; }
        if tx.fields.get("Amount").is_none() || tx.fields.get("Amount2").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) { return TxResult::NoAccount; }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        // Compute AMM key from the asset pair
        // AMMCreate uses Amount/Amount2, but the AMM object key uses Asset/Asset2
        // We derive asset info from the Amount fields
        let amount1 = &tx.fields["Amount"];
        let amount2 = &tx.fields["Amount2"];

        // Build asset pair for key computation
        let asset1 = if amount1.is_string() {
            serde_json::json!({"currency": "XRP"})
        } else {
            serde_json::json!({
                "currency": amount1.get("currency").unwrap_or(&serde_json::Value::Null),
                "issuer": amount1.get("issuer").unwrap_or(&serde_json::Value::Null),
            })
        };
        let asset2 = if amount2.is_string() {
            serde_json::json!({"currency": "XRP"})
        } else {
            serde_json::json!({
                "currency": amount2.get("currency").unwrap_or(&serde_json::Value::Null),
                "issuer": amount2.get("issuer").unwrap_or(&serde_json::Value::Null),
            })
        };

        // Compute AMM key (rippled keylet::amm: ordered issues, account-first)
        let mut b1 = Vec::with_capacity(40);
        encode_asset_to_buf(&mut b1, &asset1);
        let mut b2 = Vec::with_capacity(40);
        encode_asset_to_buf(&mut b2, &asset2);
        let (Ok(c1), Ok(i1), Ok(c2), Ok(i2)) = (
            <[u8; 20]>::try_from(&b1[..20]),
            <[u8; 20]>::try_from(&b1[20..40]),
            <[u8; 20]>::try_from(&b2[..20]),
            <[u8; 20]>::try_from(&b2[20..40]),
        ) else {
            return TxResult::Malformed;
        };
        let amm_hash = crate::ledger::keylet::amm_key(&c1, &i1, &c2, &i2);

        // Bug 4: Check for duplicate AMM
        if sandbox.exists(&amm_hash) {
            return TxResult::NoPermission;
        }

        use crate::tx::offer as ox;
        // The AMM operating account is derived ripesha-style from the PARENT
        // ledger hash and the AMM keylet (rippled ammAccountID, prefix 0;
        // higher prefixes only on address collision). Mainnet-verified
        // against #105666951 (rNgZoYRTk…).
        let parent = sandbox.base().header.parent_hash;
        let mut pre = [0u8; 66];
        pre[2..34].copy_from_slice(&parent.0);
        pre[34..66].copy_from_slice(&amm_hash.0);
        let seed = crate::shamap::hash::sha512_half(&pre);
        let amm_acct = xrpl_core::crypto::signing::public_key_to_account_id(&seed.0);
        let amm_acct_key = keylet::account_root_key(&amm_acct);

        // AMM AccountRoot: lsfDisableMaster | lsfDepositAuth | lsfDefaultRipple.
        let amm_root = serde_json::json!({
            "LedgerEntryType": "AccountRoot",
            "Account": hex::encode(amm_acct),
            "Balance": "0",
            "Sequence": sandbox.base().header.sequence,
            "OwnerCount": 0,
            "Flags": 0x0110_0000u64 | 0x0080_0000,
            "AMMID": hex::encode_upper(amm_hash.0),
        });
        sandbox.write(amm_acct_key, serde_json::to_vec(&amm_root).unwrap_or_default());

        // Fund the pool: both amounts creator → AMM account (XRP moves the
        // Balance, IOUs create the AMM's trust lines + directory entries).
        for v in [amount1, amount2] {
            if let (Some(leg), Some(amt)) = (ox::leg_of(v), keylet::amount_mant_exp(v)) {
                if amt.0 > 0 {
                    ox::move_leg(sandbox, &tx.account, &amm_acct, &leg, amt);
                }
            }
        }

        // Mint the creator's LP tokens: `ammLPTokens` = sqrt(Amount * Amount2),
        // XRP counted in DROPS. This was a hardcoded 1e7 placeholder, which
        // every later deposit and withdrawal on the pool then inherited — 23 of
        // the 83 value divergences at gate 46, and the largest by magnitude
        // (#105666951 minted 10000000 against mainnet's 31622.77660168379).
        let lpt = keylet::amm_lpt_currency(&c1, &c2);
        let lp_leg = crate::tx::offer::Leg { xrp: false, cur: lpt, issuer: amm_acct };
        let minted: crate::tx::offer::Me =
            match (keylet::amount_mant_exp(amount1), keylet::amount_mant_exp(amount2)) {
                (Some(a), Some(b)) => crate::tx::amm_swap::amm_lp_tokens(a, b),
                _ => (0, 0),
            };
        ox::move_leg(sandbox, &amm_acct, &tx.account, &lp_leg, minted);

        // Create AMM ledger entry
        let amm_obj = serde_json::json!({
            "LedgerEntryType": "AMM",
            "Account": hex::encode(amm_acct),
            "Asset": asset1,
            "Asset2": asset2,
            "LPTokenBalance": {
                "currency": hex::encode_upper(lpt),
                "issuer": hex::encode(amm_acct),
                "value": ox::me_to_value_string(minted),
            },
            "TradingFee": tx.fields.get("TradingFee").and_then(|f| f.as_u64()).unwrap_or(0),
            "AuctionSlot": {},
            "VoteSlots": [],
        });
        sandbox.write(amm_hash, serde_json::to_vec(&amm_obj).unwrap());

        TxResult::Success
    }
}

/// 20-byte currency from an Asset spec (`{"currency": "XRP" | ISO | hex40}`).
fn asset_currency20(v: &serde_json::Value) -> [u8; 20] {
    let mut c = [0u8; 20];
    let Some(s) = v.get("currency").and_then(|x| x.as_str()) else { return c };
    if s == "XRP" {
        return c;
    }
    if s.len() == 40 {
        if let Ok(b) = hex::decode(s) {
            c.copy_from_slice(&b);
        }
    } else if s.len() == 3 {
        c[12..15].copy_from_slice(s.as_bytes());
    }
    c
}

/// Resolve the AMM object key, its operating account, and the LP-token leg
/// (0x03-currency issued by the AMM account) for a Deposit/Withdraw.
fn amm_ctx(
    tx: &TxFields,
    sandbox: &Sandbox,
) -> Option<(xrpl_core::types::Hash256, [u8; 20], crate::tx::offer::Leg)> {
    let key = amm_key_from_asset_fields(tx)?;
    let obj: serde_json::Value = serde_json::from_slice(&sandbox.read(&key)?).ok()?;
    let acct_hex = obj.get("Account").and_then(|v| v.as_str())?;
    let acct_b = hex::decode(acct_hex).ok()?;
    let acct = <[u8; 20]>::try_from(acct_b.as_slice()).ok()?;
    let cur_a = asset_currency20(tx.fields.get("Asset")?);
    let cur_b = asset_currency20(tx.fields.get("Asset2")?);
    let lpt = keylet::amm_lpt_currency(&cur_a, &cur_b);
    Some((key, acct, crate::tx::offer::Leg { xrp: false, cur: lpt, issuer: acct }))
}

/// Adjust the AMM object's LPTokenBalance value (magnitude only — parity
/// compares keys; the oracle corrects the number downstream).
fn bump_lp_balance(
    sandbox: &mut Sandbox,
    amm_key: &xrpl_core::types::Hash256,
    delta: crate::tx::offer::Me,
    add: bool,
) {
    use crate::tx::offer as ox;
    let Some(mut obj) = ox::json_at(sandbox, amm_key) else { return };
    let cur = obj["LPTokenBalance"]
        .as_object()
        .and_then(|o| o.get("value"))
        .and_then(|v| v.as_str())
        .and_then(|s| keylet::amount_mant_exp(&serde_json::Value::String(s.to_string())))
        .unwrap_or((0, 0));
    let (neg, mag) = ox::signed_add(false, cur, !add, delta);
    let sign = if neg && mag.0 > 0 { "-" } else { "" };
    obj["LPTokenBalance"]["value"] =
        serde_json::Value::String(format!("{}{}", sign, ox::me_to_value_string(mag)));
    ox::put_json(sandbox, *amm_key, &obj);
}


use crate::tx::offer as ox;

/// rippled `multiply(balance, frac, rm)` — the exact product rounded to 16
/// significant digits in ONE direction (`Number::upward` / `downward`), as
/// opposed to `st_multiply`'s half-even. XRP is integral, so it rounds to whole
/// drops instead.
fn mul_directed(a: ox::Me, b: ox::Me, up: bool, xrp: bool) -> ox::Me {
    if a.0 == 0 || b.0 == 0 {
        return (0, 0);
    }
    let (am, ae) = ox::norm16(a);
    let (bm, be) = ox::norm16(b);
    // Both mantissas are < 1e16, so the product is < 1e32 and fits a u128.
    let prod = am * bm;
    let e = ae + be;
    if xrp {
        if e >= 0 {
            return (prod.saturating_mul(10u128.saturating_pow(e.min(38) as u32)), 0);
        }
        let d = 10u128.saturating_pow(((-e).min(38)) as u32);
        let (q, r) = (prod / d, prod % d);
        return (if up && r != 0 { q + 1 } else { q }, 0);
    }
    let mut k = 0u32;
    let mut t = prod;
    while t >= 10_000_000_000_000_000 {
        t /= 10;
        k += 1;
    }
    let d = 10u128.pow(k);
    let (q, r) = (prod / d, prod % d);
    let m = if up && r != 0 { q + 1 } else { q };
    ox::norm16((m, e + k as i32))
}

/// rippled `adjustLPTokens(lptAMMBalance, lpTokens, IsDeposit::Yes)` —
/// `(lptAMMBalance + lpTokens) - lptAMMBalance` with rounding forced DOWNWARD,
/// which quantises the token count to what is actually representable once it
/// joins the pool balance (AMMHelpers.cpp:173-184).
/// `adjustLPTokens` — see `amm_swap::adjust_lp_tokens`. rippled forces
/// DOWNWARD on both steps (`SaveNumberRoundMode`); this used to route through
/// `stamount_signed_add`, which is half-even, and so could land a ulp high.
fn adjust_lp_tokens(lpt_balance: ox::Me, tokens: ox::Me) -> ox::Me {
    crate::tx::amm_swap::adjust_lp_tokens(lpt_balance, tokens, true)
}

/// rippled `AMMDeposit::equalDepositLimit` (AMMDeposit.cpp:721-787), the
/// tfTwoAsset path. Both Amount and Amount2 are MAXIMA, not the amounts
/// deposited: the pool ratio decides one side from the other.
///
///   frac      = amount / amountBalance                      (Number, 16 digits)
///   tokensAdj = adjustLPTokens(lpt, multiply(lpt, frac, DOWNWARD))
///   frac      = tokensAdj / lpt                             (adjustFracByTokens)
///   amt2Dep   = multiply(amount2Balance, frac, UPWARD)
///   if amt2Dep <= amount2      -> deposit (amount, amt2Dep)
///   ...otherwise repeat led by amount2, and if THAT overshoots amount too,
///   the transaction fails with tecAMM_FAILED.
///
/// The rounding directions are not incidental — LP tokens minimize on deposit
/// and assets maximize, "to ensure AMM invariant sqrt(poolAsset1 * poolAsset2)
/// >= LPTokensBalance" (AMMHelpers.h:651-668). Nor is the 16-digit
/// quantisation of `frac` itself: `Number` IS a 16-digit type, and computing
/// frac exactly instead lands #105869720 878CD973C64F exactly ON the boundary
/// and admits it. rippled needs 3.235503279094233 QQ1 where the transaction
/// offered 3.235503279094231 (2 ulp short), and 10000001 drops where it
/// offered 10000000 (1 drop short), so BOTH directions miss.
///
/// Returns the (Amount, Amount2) actually to be moved, or None for
/// tecAMM_FAILED.
fn equal_deposit_limit(
    amount_balance: ox::Me,
    amount2_balance: ox::Me,
    lpt_balance: ox::Me,
    amount: ox::Me,
    amount2: ox::Me,
    amount_xrp: bool,
    amount2_xrp: bool,
) -> Option<(ox::Me, ox::Me, ox::Me)> {
    if amount_balance.0 == 0 || amount2_balance.0 == 0 || lpt_balance.0 == 0 {
        return None;
    }
    let led = |num: ox::Me, den: ox::Me, out_balance: ox::Me, out_xrp: bool|
     -> Option<(ox::Me, ox::Me)> {
        let frac = ox::st_divide(num, den, false);
        let tokens = adjust_lp_tokens(lpt_balance, mul_directed(lpt_balance, frac, false, false));
        if tokens.0 == 0 {
            return None;
        }
        let frac = ox::st_divide(tokens, lpt_balance, false);
        Some((mul_directed(out_balance, frac, true, out_xrp), tokens))
    };
    if let Some((a2, t)) = led(amount, amount_balance, amount2_balance, amount2_xrp) {
        if ox::me_cmp(a2, amount2).is_le() {
            return Some((amount, a2, t));
        }
    }
    if let Some((a1, t)) = led(amount2, amount2_balance, amount_balance, amount_xrp) {
        if ox::me_cmp(a1, amount).is_le() {
            return Some((a1, amount2, t));
        }
    }
    None
}

/// rippled `AMMWithdraw::equalWithdrawLimit` (AMMWithdraw.cpp:899), the
/// tfTwoAsset path — the MIRROR of `equal_deposit_limit`, and the rounding
/// mirrors with it:
///
///   frac      = amount / amountBalance
///   tokensAdj = adjustLPTokens(lpt, multiply(lpt, frac, UPWARD), IsDeposit::No)
///   frac      = tokensAdj / lpt
///   amt2Out   = multiply(amount2Balance, frac, DOWNWARD)
///   if amt2Out <= amount2   -> withdraw (amount, amt2Out)
///   else re-lead from amount2 and require the derived side to fit.
///
/// LP tokens round UP on a withdrawal and assets DOWN — the opposite of a
/// deposit, both directions chosen to hold `sqrt(a*b) >= LPTokensBalance`
/// (`getLPTokenRounding`/`getAssetRounding`, AMMHelpers.h:595-612).
#[allow(clippy::too_many_arguments)]
fn equal_withdraw_limit(
    amount_balance: ox::Me,
    amount2_balance: ox::Me,
    lpt_balance: ox::Me,
    amount: ox::Me,
    amount2: ox::Me,
    amount_xrp: bool,
    amount2_xrp: bool,
) -> Option<(ox::Me, ox::Me, ox::Me)> {
    if amount_balance.0 == 0 || amount2_balance.0 == 0 || lpt_balance.0 == 0 {
        return None;
    }
    let led = |num: ox::Me, den: ox::Me, out_balance: ox::Me, out_xrp: bool|
     -> Option<(ox::Me, ox::Me)> {
        let frac = ox::st_divide(num, den, false);
        let tokens = crate::tx::amm_swap::adjust_lp_tokens(
            lpt_balance,
            mul_directed(lpt_balance, frac, true, false),
            false,
        );
        if tokens.0 == 0 {
            return None;
        }
        let frac = ox::st_divide(tokens, lpt_balance, false);
        Some((mul_directed(out_balance, frac, false, out_xrp), tokens))
    };
    if let Some((a2, t)) = led(amount, amount_balance, amount2_balance, amount2_xrp) {
        if ox::me_cmp(a2, amount2).is_le() {
            return Some((amount, a2, t));
        }
    }
    if let Some((a1, t)) = led(amount2, amount2_balance, amount_balance, amount_xrp) {
        if ox::me_cmp(a1, amount).is_le() {
            return Some((a1, amount2, t));
        }
    }
    None
}

// ─── AMMDeposit ───

pub struct AMMDepositTransactor;

impl Transactor for AMMDepositTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "AMMDeposit" { return TxResult::Malformed; }
        if tx.fee == 0 { return TxResult::BadFee; }
        if tx.fields.get("Asset").is_none() || tx.fields.get("Asset2").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        use crate::tx::offer as ox;
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) { return TxResult::NoAccount; }
        // Check AMM exists
        match amm_key_from_asset_fields(tx) {
            Some(key) => {
                if !sandbox.exists(&key) { return TxResult::NoEntry; }
            }
            None => return TxResult::Malformed,
        }
        // The depositor must be able to FUND an XRP side, and rippled measures
        // that against the reserve it will owe AFTER the deposit: `xrpLiquid`
        // is taken with the owner count bumped by one when the depositor has no
        // LPToken line yet, because the deposit is about to open one
        // (AMMDeposit.cpp:230-244 `balance`). Falling short is tecUNFUNDED_AMM
        // when that line already exists and tecINSUF_RESERVE_LINE when it does
        // not — the shortfall is a missing reserve, not missing funds.
        //
        // This is a different rule from the reserve guard in do_apply below,
        // which only asks whether the depositor clears the reserve at all.
        // #105893158 85C32164 deposits 446527 drops holding 5646527 at
        // OwnerCount 21: the reserve guard passes (liquid 246527 > 0) but the
        // deposit needs 446527, and with no LP line mainnet claims the fee and
        // returns tecINSUF_RESERVE_LINE.
        if let Some((_, amm_acct, lp_leg)) = amm_ctx(tx, sandbox) {
            let lp_line_exists =
                sandbox.exists(&keylet::ripple_state_key(&tx.account, &amm_acct, &lp_leg.cur));
            let adj = u64::from(!lp_line_exists);
            for f in ["Amount", "Amount2"] {
                let Some(v) = tx.fields.get(f) else { continue };
                let (Some(leg), Some(amt)) = (ox::leg_of(v), keylet::amount_mant_exp(v)) else {
                    continue;
                };
                if !leg.xrp || amt.0 == 0 {
                    continue;
                }
                let Some(acct) = ox::json_at(sandbox, &acct_key) else { continue };
                let oc = acct["OwnerCount"].as_u64().unwrap_or(0);
                let bal = acct["Balance"].as_str().and_then(|s| s.parse::<u128>().ok()).unwrap_or(0);
                let liquid = bal.saturating_sub(
                    crate::ledger::fees::account_reserve(sandbox, oc + adj) as u128,
                );
                let want = ox::me_rescale(amt, 0, false);
                if liquid < want {
                    return if lp_line_exists {
                        TxResult::UnfundedAmm
                    } else {
                        TxResult::InsufReserveLine
                    };
                }
            }
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        use crate::tx::offer as ox;
        let Some((amm_key, amm_acct, lp_leg)) = amm_ctx(tx, sandbox) else {
            return TxResult::NoEntry;
        };

        // rippled AMMDeposit apply-side reserve guard (AMMDeposit.cpp): a
        // depositor who holds ZERO LPTokens is about to open (or fund) an
        // LPToken trust line, so it must keep XRP above accountReserve(oc + 1).
        // Note this keys off the LP *balance*, not line existence — a line that
        // already exists with a zero balance (a prior full withdraw) still
        // counts as holding none, so the +1 reserve still applies (#105770848,
        // #105783986: bal 2529949 <= reserve(8) 2600000 → tecINSUF_RESERVE_LINE
        // even though the zero-balance LP line is present). Runs in do_apply on
        // the post-fee balance, exactly as rippled does; the claimed tec rolls
        // back to just the fee mutation (net_muts=1).
        let lp_line = keylet::ripple_state_key(&tx.account, &amm_acct, &lp_leg.cur);
        let lp_balance_zero = match sandbox.read(&lp_line) {
            None => true,
            Some(d) => serde_json::from_slice::<serde_json::Value>(&d)
                .ok()
                .and_then(|l| l["Balance"]["value"].as_str().map(|s| s == "0" || s == "-0"))
                .unwrap_or(true),
        };
        if lp_balance_zero {
            if let Some(acct) = ox::json_at(sandbox, &keylet::account_root_key(&tx.account)) {
                let oc = acct["OwnerCount"].as_u64().unwrap_or(0);
                let bal = acct["Balance"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
                if bal <= crate::ledger::fees::account_reserve(sandbox, oc + 1) {
                    return TxResult::InsufReserveLine;
                }
            }
        }

        // tfTwoAsset: Amount and Amount2 are MAXIMA, not the amounts deposited.
        // rippled derives one side from the other at the pool's ratio and fails
        // the whole transaction when NEITHER derivation fits inside what was
        // offered — see `equal_deposit_limit`. We moved both maxima verbatim and
        // never failed, so #105869720 878CD973C64F returned tesSUCCESS against
        // mainnet's tecAMM_FAILED.
        const TF_TWO_ASSET: u64 = 0x0010_0000;
        let mut sized: Option<(ox::Me, ox::Me, ox::Me)> = None;
        if tx.fields.get("Flags").and_then(|v| v.as_u64()).unwrap_or(0) & TF_TWO_ASSET != 0 {
            if let (Some(av), Some(bv)) = (tx.fields.get("Amount"), tx.fields.get("Amount2")) {
                if let (Some(aleg), Some(amt), Some(bleg), Some(amt2)) = (
                    ox::leg_of(av),
                    keylet::amount_mant_exp(av),
                    ox::leg_of(bv),
                    keylet::amount_mant_exp(bv),
                ) {
                    let lpt = ox::json_at(sandbox, &amm_key)
                        .and_then(|o| o["LPTokenBalance"]["value"].as_str().map(str::to_string))
                        .and_then(|t| keylet::amount_mant_exp(&serde_json::Value::String(t)));
                    if let Some(lpt) = lpt {
                        match equal_deposit_limit(
                            crate::tx::amm_swap::holds(sandbox, &amm_acct, &aleg),
                            crate::tx::amm_swap::holds(sandbox, &amm_acct, &bleg),
                            lpt,
                            amt,
                            amt2,
                            aleg.xrp,
                            bleg.xrp,
                        ) {
                            Some(triple) => sized = Some(triple),
                            None => return TxResult::AmmFailed,
                        }
                    }
                }
            }
        }

        // `lpTokensOut` prices the deposit against the pool as it stood BEFORE
        // the assets landed, so capture that side here — below the move it is
        // already inflated by the deposit itself.
        const TF_SINGLE_ASSET: u64 = 0x0008_0000;
        let single_pre = if tx.fields.get("Flags").and_then(|v| v.as_u64()).unwrap_or(0)
            & TF_SINGLE_ASSET
            != 0
        {
            tx.fields.get("Amount").and_then(|v| {
                let leg = ox::leg_of(v)?;
                let amt = keylet::amount_mant_exp(v)?;
                Some((crate::tx::amm_swap::holds(sandbox, &amm_acct, &leg), amt))
            })
        } else {
            None
        };
        // A single-asset deposit pays what the ADJUSTED tokens are worth, not
        // what was asked for — so this must be derived BEFORE anything moves.
        let single_adj: Option<(ox::Me, ox::Me)> = single_pre.and_then(|(pool_pre, amt)| {
            let obj = ox::json_at(sandbox, &amm_key)?;
            let lpt = keylet::amount_mant_exp(&serde_json::Value::String(
                obj["LPTokenBalance"]["value"].as_str()?.to_string(),
            ))?;
            let tfee = obj["TradingFee"].as_u64().unwrap_or(0) as u16;
            let xrp = tx.fields.get("Amount").and_then(ox::leg_of).map(|l| l.xrp)?;
            let t0 = crate::tx::amm_swap::adjust_lp_tokens(
                lpt,
                crate::tx::amm_swap::lp_tokens_out(pool_pre, amt, lpt, tfee),
                true,
            );
            if t0.0 == 0 {
                return None;
            }
            let (tokens, deposited) = crate::tx::amm_swap::adjust_asset_in_by_tokens(
                pool_pre, amt, lpt, t0, tfee, xrp,
            );
            (tokens.0 > 0 && deposited.0 > 0).then_some((deposited, tokens))
        });

        // Move the deposited side(s) depositor → AMM account (XRP or IOU
        // lines — move_leg handles both).
        for (i, f) in ["Amount", "Amount2"].iter().enumerate() {
            if let Some(v) = tx.fields.get(*f) {
                if let (Some(leg), Some(amt0)) = (ox::leg_of(v), keylet::amount_mant_exp(v)) {
                    let amt = match (sized, single_adj) {
                        (Some((a, _, _)), _) if i == 0 => a,
                        (Some((_, b, _)), _) => b,
                        (None, Some((a, _))) if i == 0 => a,
                        _ => amt0,
                    };
                    if amt.0 > 0 {
                        ox::move_leg(sandbox, &tx.account, &amm_acct, &leg, amt);
                    }
                }
            }
        }
        // Mint LP tokens to the depositor: an explicit `LPTokenOut` when the
        // sender named one, else `lpTokensOut` (Equation 3) for a single-asset
        // deposit.
        //
        // ⚠ The remaining modes — two-asset without LPTokenOut, tfOneAssetLPToken
        // — still fall back to the 1e7 PLACEHOLDER this used to use
        // unconditionally. That is deliberate: the line KEY is what the
        // key-level gate needs, and the magnitude shows up under DX_VALCHECK
        // until each mode's formula lands.
        let minted = tx
            .fields
            .get("LPTokenOut")
            .and_then(keylet::amount_mant_exp)
            .filter(|m| m.0 > 0)
            .or_else(|| sized.map(|(_, _, t)| t).filter(|t| t.0 > 0))
            .or_else(|| single_adj.map(|(_, t)| t))
            .unwrap_or((1_000_000_000_000_000, -8));
        ox::move_leg(sandbox, &amm_acct, &tx.account, &lp_leg, minted);
        bump_lp_balance(sandbox, &amm_key, minted, true);
        TxResult::Success
    }
}

/// Tear down an emptied LPToken trust line: delete it, drop it from BOTH owner
/// directories, and release the reserve on each side.
///
/// The AMM side is the ISSUER of the LPToken, so `move_leg` never touches it —
/// the teardown has to be explicit. rippled gets this for free because burning
/// the tokens runs the ordinary trust-line delete, which fires the moment the
/// balance reaches zero and both sides are default.
fn tear_down_lp_line(
    sandbox: &mut Sandbox,
    who: &[u8; 20],
    amm_acct: &[u8; 20],
    lp_key: xrpl_core::types::Hash256,
    lp_line: &serde_json::Value,
) {
    let node = |field: &str| {
        lp_line.get(field).and_then(|v| v.as_str()).and_then(|s| u64::from_str_radix(s, 16).ok())
    };
    let (w_node, a_node) = if who < amm_acct { ("LowNode", "HighNode") } else { ("HighNode", "LowNode") };
    sandbox.delete(lp_key);
    crate::ledger::directory::owner_dir_remove(sandbox, who, &lp_key, node(w_node), false);
    crate::ledger::directory::owner_dir_remove(sandbox, amm_acct, &lp_key, node(a_node), false);
    crate::tx::offer::owner_count_add(sandbox, who, -1);
    crate::tx::offer::owner_count_add(sandbox, amm_acct, -1);
}

/// Pay out BOTH pool assets in proportion to `tokens / total_lp`.
///
/// rippled's `equalWithdrawTokens` (AMMWithdraw.cpp:790-850):
///     frac            = tokensAdj / lptAMMBalance
///     amountWithdraw  = getRoundedAsset(amountBalance,  frac, IsDeposit::No)
///     amount2Withdraw = getRoundedAsset(amount2Balance, frac, IsDeposit::No)
/// IsDeposit::No rounds DOWN, which is what `me_muldiv(.., false)` does.
///
/// Shared by the tfWithdrawAll path (tokens = the LP's whole balance) and the
/// tfLPToken path (tokens = the requested LPTokenIn).
fn payout_proportional(
    sandbox: &mut Sandbox,
    tx: &TxFields,
    amm_acct: &[u8; 20],
    tokens: (u128, i32),
    total_lp: (u128, i32),
) {
    use crate::tx::offer as ox;
    let asset_leg = |v: &serde_json::Value| -> Option<ox::Leg> {
        if v.get("currency").and_then(|c| c.as_str()) == Some("XRP") {
            return Some(ox::Leg { xrp: true, cur: [0u8; 20], issuer: [0u8; 20] });
        }
        let mut amt = v.clone();
        amt["value"] = serde_json::json!("0");
        ox::leg_of(&amt)
    };
    for f in ["Asset", "Asset2"] {
        let Some(v) = tx.fields.get(f) else { continue };
        let Some(leg) = asset_leg(v) else { continue };
        let pool = crate::tx::amm_swap::holds(sandbox, amm_acct, &leg);
        // TWO separate 16-digit steps, not a fused muldiv. rippled's
        // `equalWithdrawTokens` computes `frac = tokens / lptAMMBalance` and
        // then `getRoundedAsset(balance, frac, IsDeposit::No)` =
        // `multiply(balance, frac, Downward)`, so `frac` is rounded to 16
        // digits BEFORE it multiplies. A fused muldiv keeps the intermediate
        // exact — more accurate than rippled, and therefore wrong.
        //
        // #105796380 2437575D (tfWithdrawAll) is the specimen: the withdrawer's
        // Jocker line lands …890599 against mainnet's …890600, one ulp.
        let frac = ox::st_divide(tokens, total_lp, false);
        let share = mul_directed(pool, frac, false, leg.xrp);
        if share.0 > 0 {
            ox::move_leg(sandbox, amm_acct, &tx.account, &leg, share);
        }
    }
}

// ─── AMMWithdraw ───

pub struct AMMWithdrawTransactor;

impl Transactor for AMMWithdrawTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "AMMWithdraw" { return TxResult::Malformed; }
        if tx.fee == 0 { return TxResult::BadFee; }
        if tx.fields.get("Asset").is_none() || tx.fields.get("Asset2").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) { return TxResult::NoAccount; }
        match amm_key_from_asset_fields(tx) {
            Some(key) => {
                if !sandbox.exists(&key) { return TxResult::NoEntry; }
            }
            None => return TxResult::Malformed,
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        use crate::tx::offer as ox;
        let Some((amm_key, amm_acct, lp_leg)) = amm_ctx(tx, sandbox) else {
            return TxResult::NoEntry;
        };
        // A pool cannot pay out more of an asset than it holds:
        // AMMWithdraw::preclaim's checkAmount rejects amount > balance with
        // tecAMM_BALANCE before anything moves (AMMWithdraw.cpp:232).
        // #105763689 740D41D6 asks for 50950 drops from a pool holding 43921.
        for f in ["Amount", "Amount2"] {
            if let Some(v) = tx.fields.get(f) {
                if let (Some(leg), Some(amt)) = (ox::leg_of(v), keylet::amount_mant_exp(v)) {
                    let held = crate::tx::amm_swap::holds(sandbox, &amm_acct, &leg);
                    if ox::me_cmp(amt, held).is_gt() {
                        return TxResult::AmmBalance;
                    }
                }
            }
        }
        // tfWithdrawAll: redeem the LP's ENTIRE position — both pool assets out
        // in proportion to their LPToken share, ALL their LPTokens burned, and
        // the LPToken trust line torn down (deleted, dropped from BOTH owner
        // directories, a reserve released on each side). rippled AMMWithdraw
        // apply-side under tfWithdrawAll. #105787513 EDD6BA97 / #105796380
        // 2437575D: mainnet emits eight nodes; we emitted three (a fixed default
        // burn, no asset payout, the LP line only Modified to zero not deleted).
        // rippled's isWithdrawAll covers BOTH tfWithdrawAll (0x00020000) and
        // tfOneAssetWithdrawAll (0x00040000) — AMMWithdraw.cpp:1133-1138. We
        // only checked the first. #105929166 39E145693A80 carries Flags 262144
        // = tfOneAssetWithdrawAll, which I first misread as tfSingleAsset
        // (0x00080000). Under it the `Amount` is a MINIMUM, not the size of the
        // withdrawal: the LP burns their ENTIRE token balance and takes it out
        // in that one asset, which is why mainnet Deletes the LP line.
        const TF_WITHDRAW_ALL: u64 = 0x0002_0000;
        const TF_ONE_ASSET_WITHDRAW_ALL: u64 = 0x0004_0000;
        let wd_flags = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
        if wd_flags & (TF_WITHDRAW_ALL | TF_ONE_ASSET_WITHDRAW_ALL) != 0 {
            let lp_key = keylet::ripple_state_key(&tx.account, &amm_acct, &lp_leg.cur);
            let Some(lp_line) = ox::json_at(sandbox, &lp_key) else { return TxResult::Success };
            let (_neg, lp_bal) = ox::signed_value(&lp_line["Balance"]);
            if lp_bal.0 == 0 {
                return TxResult::Success;
            }
            let total_lp = ox::json_at(sandbox, &amm_key)
                .and_then(|o| o["LPTokenBalance"]["value"].as_str().map(str::to_string))
                .and_then(|s| keylet::amount_mant_exp(&serde_json::Value::String(s)))
                .unwrap_or(lp_bal);
            if wd_flags & TF_ONE_ASSET_WITHDRAW_ALL != 0 {
                // ONE asset out, sized by rippled's ammAssetOut against the
                // LP's whole token balance. `Amount` names the side (its value
                // is only a floor).
                if let Some(v) = tx.fields.get("Amount") {
                    if let Some(leg) = ox::leg_of(v) {
                        let bal = crate::tx::amm_swap::holds(sandbox, &amm_acct, &leg);
                        let tfee = ox::json_at(sandbox, &amm_key)
                            .and_then(|o| o["TradingFee"].as_u64())
                            .unwrap_or(0) as u16;
                        if let Some(out) =
                            crate::tx::amm_swap::amm_asset_out(bal, total_lp, lp_bal, tfee, leg.xrp)
                        {
                            if out.0 > 0 {
                                ox::move_leg(sandbox, &amm_acct, &tx.account, &leg, out);
                            }
                        }
                    }
                }
            } else {
                // Both assets out, proportional to the redeemed LPToken share.
                payout_proportional(sandbox, tx, &amm_acct, lp_bal, total_lp);
            }
            tear_down_lp_line(sandbox, &tx.account, &amm_acct, lp_key, &lp_line);
            bump_lp_balance(sandbox, &amm_key, lp_bal, false);
            return TxResult::Success;
        }
        // tfLPToken (0x00010000): the LP names only how many LPTokens to redeem
        // and receives BOTH pool assets in proportion — there is no Amount /
        // Amount2 on the transaction at all. rippled's equalWithdrawTokens
        // (AMMWithdraw.cpp:790-850) pays out amountBalance*frac and
        // amount2Balance*frac where frac = LPTokenIn / lptAMMBalance.
        //
        // We only ever moved Amount/Amount2, found neither, and so paid out
        // nothing while still burning the tokens. #105880685 F2CCA2BD6FA4,
        // #105840045 41110275D9B7 (same XRP/FARM pool) and #105877543
        // 331E54E698CA: all our_muts=5 vs net_muts=8, the three missing nodes
        // being the two RippleStates and the AccountRoot the payout touches.
        //
        // Not modelled: rippled returns tecAMM_FAILED when either side rounds
        // to zero, and treats LPTokenIn == lptAMMBalance as a full withdrawal.
        // Neither is exercised by a known case; left with the other deferred
        // AMM rounding question rather than guessed at.
        let lp_token_in = tx.fields.get("LPTokenIn").and_then(keylet::amount_mant_exp);
        let has_explicit_amount =
            tx.fields.get("Amount").is_some() || tx.fields.get("Amount2").is_some();
        if let Some(tokens) = lp_token_in.filter(|t| t.0 > 0) {
            if !has_explicit_amount {
                if let Some(total_lp) = ox::json_at(sandbox, &amm_key)
                    .and_then(|o| o["LPTokenBalance"]["value"].as_str().map(str::to_string))
                    .and_then(|s| keylet::amount_mant_exp(&serde_json::Value::String(s)))
                {
                    payout_proportional(sandbox, tx, &amm_acct, tokens, total_lp);
                }
            }
        }

        // tfSingleAsset withdrawals name an Amount and NO LPTokenIn: the LP
        // tokens burned are DERIVED from what is taken out, and the pool
        // balance that derivation needs is the one BEFORE the payout below.
        let single_asset_burn = if tx.fields.get("LPTokenIn").is_none() {
            (|| {
                let v = tx.fields.get("Amount")?;
                let leg = ox::leg_of(v)?;
                let withdraw = keylet::amount_mant_exp(v)?;
                let balance = crate::tx::amm_swap::holds(sandbox, &amm_acct, &leg);
                let total_lp = ox::json_at(sandbox, &amm_key)
                    .and_then(|o| o["LPTokenBalance"]["value"].as_str().map(str::to_string))
                    .and_then(|t| keylet::amount_mant_exp(&serde_json::Value::String(t)))?;
                let tfee = ox::json_at(sandbox, &amm_key)
                    .and_then(|o| o["TradingFee"].as_u64())
                    .unwrap_or(0) as u16;
                // `singleWithdraw` (AMMWithdraw.cpp:969) does NOT burn what
                // `lpTokensIn` returns, nor pay out the requested `Amount`:
                //   tokens = adjustLPTokensIn(lpt, lpTokensIn(...), withdrawAll)
                //   (tokensAdj, amountAdj) = adjustAssetOutByTokens(...)
                // and it withdraws `amountAdj`, burning `tokensAdj`. The
                // adjustment quantises the burn to the POOL BALANCE's ulp,
                // which is why mainnet's LP line lands on a coarse grid where
                // ours carried full 16-digit precision — #105816437 AC7713F9
                // stores 97379042.34212 against our 97379042.34211386.
                //
                // tfWithdrawAll skips the adjustment (`isWithdrawAll`), and it
                // returns earlier, so this path never sees it.
                let t0 = crate::tx::amm_swap::lp_tokens_in(balance, withdraw, total_lp, tfee)?;
                let t0 = crate::tx::amm_swap::adjust_lp_tokens(total_lp, t0, false);
                if t0.0 == 0 {
                    return None;
                }
                let (tokens, out) = crate::tx::amm_swap::adjust_asset_out_by_tokens(
                    balance, withdraw, total_lp, t0, tfee, leg.xrp,
                );
                (tokens.0 > 0 && out.0 > 0).then_some((out, tokens))
            })()
        } else {
            None
        };

        let pool_lpt = |sandbox: &Sandbox| -> Option<ox::Me> {
            let o = ox::json_at(sandbox, &amm_key)?;
            keylet::amount_mant_exp(&serde_json::Value::String(
                o["LPTokenBalance"]["value"].as_str()?.to_string(),
            ))
        };
        // tfTwoAsset: Amount/Amount2 are MAXIMA and the pool ratio picks the
        // pair — moving them verbatim withdraws whatever was asked for.
        const TF_TWO_ASSET_W: u64 = 0x0010_0000;
        let wd_sized: Option<(ox::Me, ox::Me, ox::Me)> = if wd_flags & TF_TWO_ASSET_W != 0 {
            (|| {
                let (av, bv) = (tx.fields.get("Amount")?, tx.fields.get("Amount2")?);
                let (aleg, amt) = (ox::leg_of(av)?, keylet::amount_mant_exp(av)?);
                let (bleg, amt2) = (ox::leg_of(bv)?, keylet::amount_mant_exp(bv)?);
                equal_withdraw_limit(
                    crate::tx::amm_swap::holds(sandbox, &amm_acct, &aleg),
                    crate::tx::amm_swap::holds(sandbox, &amm_acct, &bleg),
                    pool_lpt(sandbox)?,
                    amt,
                    amt2,
                    aleg.xrp,
                    bleg.xrp,
                )
            })()
        } else {
            None
        };
        // tfOneAssetLPToken: LPTokenIn is what is burned and the AMOUNT is
        // DERIVED from it via `ammAssetOut` (`Amount` is only a minimum) —
        // rippled `singleWithdrawTokens` (AMMWithdraw.cpp:1020).
        let one_asset: Option<(ox::Me, ox::Me)> = if wd_sized.is_none() {
            (|| {
                let lp_in = tx
                    .fields
                    .get("LPTokenIn")
                    .and_then(keylet::amount_mant_exp)
                    .filter(|m| m.0 > 0)?;
                let v = tx.fields.get("Amount")?;
                let leg = ox::leg_of(v)?;
                let lpt = pool_lpt(sandbox)?;
                let tfee = ox::json_at(sandbox, &amm_key)
                    .and_then(|o| o["TradingFee"].as_u64())
                    .unwrap_or(0) as u16;
                let tokens = crate::tx::amm_swap::adjust_lp_tokens(lpt, lp_in, false);
                let out = crate::tx::amm_swap::amm_asset_out(
                    crate::tx::amm_swap::holds(sandbox, &amm_acct, &leg),
                    lpt,
                    tokens,
                    tfee,
                    leg.xrp,
                )?;
                Some((out, tokens))
            })()
        } else {
            None
        };

        // Move the withdrawn side(s) AMM account → withdrawer.
        for (i, f) in ["Amount", "Amount2"].iter().enumerate() {
            if let Some(v) = tx.fields.get(*f) {
                if let (Some(leg), Some(amt0)) = (ox::leg_of(v), keylet::amount_mant_exp(v)) {
                    let amt = match (wd_sized, one_asset, single_asset_burn) {
                        (Some((a, _, _)), _, _) if i == 0 => a,
                        (Some((_, b, _)), _, _) => b,
                        (None, Some((a, _)), _) if i == 0 => a,
                        // tfSingleAsset pays what the ADJUSTED tokens are worth.
                        (None, None, Some((a, _))) if i == 0 => a,
                        _ => amt0,
                    };
                    if amt.0 > 0 {
                        ox::move_leg(sandbox, &amm_acct, &tx.account, &leg, amt);
                    }
                }
            }
        }
        // Burn the withdrawer's LP tokens. LPTokenIn when named; otherwise the
        // single-asset derivation above. The old fallback here was a FIXED
        // placeholder (1e7 tokens) that bore no relation to the withdrawal —
        // #105929166 39E145693A80 is tfSingleAsset taking 353,745,926 drops,
        // which consumed the LP's ENTIRE position on mainnet (the LP line is
        // Deleted); we burned the placeholder and left the line at a nonzero
        // balance.
        let burned = wd_sized
            .map(|(_, _, t)| t)
            .or(one_asset.map(|(_, t)| t))
            .or_else(|| {
                tx.fields
                    .get("LPTokenIn")
                    .and_then(keylet::amount_mant_exp)
                    .filter(|m| m.0 > 0)
            })
            .or(single_asset_burn.map(|(_, t)| t))
            .unwrap_or((1_000_000_000_000_000, -8));
        ox::move_leg(sandbox, &tx.account, &amm_acct, &lp_leg, burned);
        bump_lp_balance(sandbox, &amm_key, burned, false);

        // A burn that empties the LP's position TEARS THE LINE DOWN — it is not
        // left sitting at zero. rippled gets this from the ordinary trust-line
        // delete that runs when the tokens are burned; we have to do it
        // explicitly because the AMM is the LPToken's issuer and `move_leg`
        // never touches the issuer side.
        //
        // #105929166 39E145693A80 and #105922945 15FAEA4CC56D: mainnet DELETES
        // the LP line (`:2`) and Modifies both owner directories; we emitted it
        // Modified (`:1`) and left the directories alone — the same key showing
        // up as missing-Deleted and extra-Modified is the whole signature.
        let lp_key = keylet::ripple_state_key(&tx.account, &amm_acct, &lp_leg.cur);
        if let Some(line) = ox::json_at(sandbox, &lp_key) {
            let (_neg, mag) = ox::signed_value(&line["Balance"]);
            if mag.0 == 0 {
                tear_down_lp_line(sandbox, &tx.account, &amm_acct, lp_key, &line);
            }
        }
        TxResult::Success
    }
}

// ─── AMMVote ───

pub struct AMMVoteTransactor;

impl Transactor for AMMVoteTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "AMMVote" { return TxResult::Malformed; }
        if tx.fee == 0 { return TxResult::BadFee; }
        if tx.fields.get("Asset").is_none() || tx.fields.get("Asset2").is_none() {
            return TxResult::Malformed;
        }
        if tx.fields.get("TradingFee").is_none() { return TxResult::Malformed; }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) { return TxResult::NoAccount; }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        // Update AMM's VoteSlots with this account's fee vote
        if let Some(key) = amm_key_from_asset_fields(tx) {
            if let Some(data) = sandbox.read(&key) {
                if let Ok(mut amm) = serde_json::from_slice::<serde_json::Value>(&data) {
                    let fee = tx.fields.get("TradingFee").and_then(|f| f.as_u64()).unwrap_or(0);
                    let vote = serde_json::json!({
                        "Account": hex::encode(tx.account),
                        "TradingFee": fee,
                    });

                    // Add/replace vote in VoteSlots
                    let slots = amm.get_mut("VoteSlots")
                        .and_then(|s| s.as_array_mut());
                    if let Some(slots) = slots {
                        // Remove existing vote from this account
                        let acct_hex = hex::encode(tx.account);
                        slots.retain(|v| v.get("Account").and_then(|a| a.as_str()) != Some(&acct_hex));
                        slots.push(vote);
                        // Limit to 8 vote slots (rippled maximum)
                        if slots.len() > 8 { slots.remove(0); }
                    }

                    sandbox.write(key, serde_json::to_vec(&amm).unwrap());
                }
            }
        }
        TxResult::Success
    }
}

// ─── AMMBid ───

pub struct AMMBidTransactor;

impl Transactor for AMMBidTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "AMMBid" { return TxResult::Malformed; }
        if tx.fee == 0 { return TxResult::BadFee; }
        if tx.fields.get("Asset").is_none() || tx.fields.get("Asset2").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) { return TxResult::NoAccount; }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        // Bid for the AMM's auction slot (discounted trading fee)
        if let Some(key) = amm_key_from_asset_fields(tx) {
            if let Some(data) = sandbox.read(&key) {
                if let Ok(mut amm) = serde_json::from_slice::<serde_json::Value>(&data) {
                    amm["AuctionSlot"] = serde_json::json!({
                        "Account": hex::encode(tx.account),
                        "Expiration": 0,
                        "Price": tx.fields.get("BidMin").cloned().unwrap_or(serde_json::json!("0")),
                    });
                    sandbox.write(key, serde_json::to_vec(&amm).unwrap());
                }
            }
        }
        TxResult::Success
    }
}

// ─── AMMDelete ───

pub struct AMMDeleteTransactor;

impl Transactor for AMMDeleteTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "AMMDelete" { return TxResult::Malformed; }
        if tx.fee == 0 { return TxResult::BadFee; }
        if tx.fields.get("Asset").is_none() || tx.fields.get("Asset2").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) { return TxResult::NoAccount; }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        if let Some(key) = amm_key_from_asset_fields(tx) {
            if sandbox.exists(&key) {
                // Read AMM to find the creator for OwnerCount
                if let Some(data) = sandbox.read(&key) {
                    if let Ok(amm) = serde_json::from_slice::<serde_json::Value>(&data) {
                        if let Some(creator_hex) = amm.get("Account").and_then(|a| a.as_str()) {
                            if let Ok(creator_bytes) = hex::decode(creator_hex) {
                                if creator_bytes.len() == 20 {
                                    let mut creator = [0u8; 20];
                                    creator.copy_from_slice(&creator_bytes);
                                    decrement_owner_count(sandbox, &creator);
                                }
                            }
                        }
                    }
                }
                sandbox.delete(key);
            }
        }
        TxResult::Success
    }
}

// ─── Helpers ───

fn deduct_xrp(sandbox: &mut Sandbox, account: &[u8; 20], drops: u64) -> bool {
    let key = keylet::account_root_key(account);
    if let Some(data) = sandbox.read(&key) {
        if let Ok(mut acct) = serde_json::from_slice::<serde_json::Value>(&data) {
            let balance = acct["Balance"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            if balance < drops {
                return false;
            }
            let new_balance = balance - drops;
            acct["Balance"] = serde_json::Value::String(new_balance.to_string());
            sandbox.write(key, serde_json::to_vec(&acct).unwrap());
            return true;
        }
    }
    false
}

fn credit_xrp(sandbox: &mut Sandbox, account: &[u8; 20], drops: u64) {
    let key = keylet::account_root_key(account);
    if let Some(data) = sandbox.read(&key) {
        if let Ok(mut acct) = serde_json::from_slice::<serde_json::Value>(&data) {
            let balance = acct["Balance"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let new_balance = balance.saturating_add(drops);
            acct["Balance"] = serde_json::Value::String(new_balance.to_string());
            sandbox.write(key, serde_json::to_vec(&acct).unwrap());
        }
    }
}

/// Update the AMM pool's XRP balance.
/// If `is_deposit` is true, adds `drops` to the pool.
/// If `is_deposit` is false (withdraw), checks the pool has sufficient funds and subtracts.
/// Returns true on success, false if the pool has insufficient funds (withdraw only).
fn update_amm_pool_balance(
    sandbox: &mut Sandbox,
    amm_key: &xrpl_core::types::Hash256,
    drops: u64,
    is_deposit: bool,
) -> bool {
    if let Some(data) = sandbox.read(amm_key) {
        if let Ok(mut amm) = serde_json::from_slice::<serde_json::Value>(&data) {
            let pool_balance = amm
                .get("PoolBalance")
                .and_then(|b| b.as_str())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);

            let new_balance = if is_deposit {
                pool_balance.saturating_add(drops)
            } else {
                if pool_balance < drops {
                    return false;
                }
                pool_balance - drops
            };

            amm["PoolBalance"] = serde_json::Value::String(new_balance.to_string());
            sandbox.write(*amm_key, serde_json::to_vec(&amm).unwrap());
            return true;
        }
    }
    // AMM not found — shouldn't happen if preclaim passed, but treat as failure for withdrawals
    is_deposit
}

fn increment_owner_count(sandbox: &mut Sandbox, account: &[u8; 20]) {
    let key = keylet::account_root_key(account);
    if let Some(data) = sandbox.read(&key) {
        if let Ok(mut acct) = serde_json::from_slice::<serde_json::Value>(&data) {
            let count = acct["OwnerCount"].as_u64().unwrap_or(0);
            acct["OwnerCount"] = serde_json::Value::Number((count + 1).into());
            sandbox.write(key, serde_json::to_vec(&acct).unwrap());
        }
    }
}

fn decrement_owner_count(sandbox: &mut Sandbox, account: &[u8; 20]) {
    let key = keylet::account_root_key(account);
    if let Some(data) = sandbox.read(&key) {
        if let Ok(mut acct) = serde_json::from_slice::<serde_json::Value>(&data) {
            let count = acct["OwnerCount"].as_u64().unwrap_or(0);
            if count > 0 {
                acct["OwnerCount"] = serde_json::Value::Number((count - 1).into());
            }
            sandbox.write(key, serde_json::to_vec(&acct).unwrap());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::sandbox::Sandbox;
    use crate::ledger::state::LedgerState;
    use xrpl_core::types::Hash256;

    fn make_state(id: &[u8; 20], balance: u64) -> LedgerState {
        let header = LedgerHeader {
            sequence: 100,
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
        let acct = serde_json::json!({
            "LedgerEntryType": "AccountRoot",
            "Account": hex::encode(id),
            "Balance": balance.to_string(),
            "Sequence": 1,
            "OwnerCount": 0,
        });
        let key = keylet::account_root_key(id);
        state.state_map.insert(key, serde_json::to_vec(&acct).unwrap()).unwrap();
        state
    }

    /// An XRP side of an AMM deposit has to be FUNDABLE, and rippled measures
    /// that against the reserve the depositor will owe afterwards: `xrpLiquid`
    /// with the owner count bumped by one when there is no LPToken line yet,
    /// because the deposit is about to open one (AMMDeposit.cpp:230-244). Short
    /// of it is tecUNFUNDED_AMM when that line exists and tecINSUF_RESERVE_LINE
    /// when it does not. #105893158 85C32164 deposits 446527 drops holding
    /// 5646527 at OwnerCount 21 — the separate reserve guard passes (liquid
    /// 246527 > 0) but the deposit needs 446527, and mainnet claims the fee.
    /// A tfTwoAsset deposit whose BOTH directions overshoot is tecAMM_FAILED —
    /// rippled `equalDepositLimit` (AMMDeposit.cpp:721-787). #105869720
    /// 878CD973C64F against the XRP/QQ1 pool: 7326949 drops / 2.37063675152562
    /// QQ1 / 4167.465608078962 LP, offering 10 XRP and 3.235503279094231 QQ1.
    ///
    /// The margin is ONE ulp, which is what makes this a real test of the
    /// rounding rather than of the algorithm: assets round UP on deposit and LP
    /// tokens DOWN, and `frac` is itself quantised to 16 digits because Number
    /// IS a 16-digit type. Compute frac exactly instead and the XRP-led
    /// direction lands precisely ON 3.235503279094231 and is admitted — which
    /// is exactly the tesSUCCESS we used to return.
    #[test]
    fn a_two_asset_deposit_that_overshoots_both_ways_fails() {
        let amount_balance = (7_326_949u128, 0i32); // drops
        let amount2_balance = (237_063_675_152_562u128, -14);
        let lpt_balance = (4_167_465_608_078_962u128, -12);

        assert_eq!(
            equal_deposit_limit(
                amount_balance, amount2_balance, lpt_balance,
                (10_000_000, 0), (3_235_503_279_094_231, -15), true, false,
            ),
            None,
            "XRP-led needs 3.235503279094233 QQ1 and QQ1-led needs 10000001 drops",
        );

        // One more drop of headroom on the XRP side and the QQ1-led direction
        // fits, so the deposit goes through at the pool's ratio.
        let ok = equal_deposit_limit(
            amount_balance, amount2_balance, lpt_balance,
            (10_000_001, 0), (3_235_503_279_094_231, -15), true, false,
        );
        let (a1, a2, tokens) = ok.expect("QQ1-led direction fits");
        assert_eq!((a1, a2), ((10_000_001, 0), (3_235_503_279_094_231, -15)));
        // The tokens the sizing already derived are what gets MINTED — they used
        // to be computed, discarded, and replaced by a 1e7 placeholder.
        // The minted tokens carry the deposit's own fraction of the pool. (This
        // deposit is 136% of the XRP side, so the mint exceeding the outstanding
        // supply is correct, not a bug.)
        let f = |m: ox::Me| ox::me_to_value_string(m).parse::<f64>().unwrap();
        let share = f(tokens) / f(lpt_balance);
        let want = f(a2) / f(amount2_balance);
        // This is the QQ1-LED direction, so the fraction is set by the QQ1 side
        // and the XRP side is derived from it — rounded UP, which is why it does
        // not reproduce the fraction to the last digit.
        assert!(
            (share - want).abs() / want < 1e-9,
            "minted share {share} should track the deposit fraction {want}"
        );
        assert!(f(a1) / f(amount_balance) >= share);
    }

    #[test]
    fn an_amm_deposit_must_fund_its_xrp_side_after_reserve() {
        let depositor = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        let amm_acct = [0x05u8; 20];
        let mut cur = [0u8; 20];
        cur[12..15].copy_from_slice(b"PLX");

        // 5.646527 XRP at OwnerCount 21 ⇒ reserve(22) = 5.4 XRP ⇒ 0.246527 liquid.
        let mut state = make_state(&depositor, 5_646_527);
        {
            let k = keylet::account_root_key(&depositor);
            let mut a: serde_json::Value =
                serde_json::from_slice(&Sandbox::new(&state).read(&k).unwrap()).unwrap();
            a["OwnerCount"] = serde_json::json!(21);
            state.state_map.insert(k, serde_json::to_vec(&a).unwrap()).unwrap();
        }
        for id in [&issuer, &amm_acct] {
            let a = serde_json::json!({
                "LedgerEntryType": "AccountRoot", "Account": hex::encode(id),
                "Balance": "500000000", "Sequence": 1, "OwnerCount": 0,
            });
            state.state_map.insert(keylet::account_root_key(id), serde_json::to_vec(&a).unwrap()).unwrap();
        }
        let xrp_leg = crate::tx::offer::leg_of(&serde_json::json!("1")).unwrap();
        let plx = serde_json::json!({"currency": "PLX", "issuer": hex::encode(issuer), "value": "1"});
        let plx_leg = crate::tx::offer::leg_of(&plx).unwrap();
        let amm = serde_json::json!({
            "LedgerEntryType": "AMM", "Account": hex::encode(amm_acct), "TradingFee": 0,
        });
        let akey = keylet::amm_key(&plx_leg.cur, &plx_leg.issuer, &xrp_leg.cur, &xrp_leg.issuer);
        state.state_map.insert(akey, serde_json::to_vec(&amm).unwrap()).unwrap();

        let tx = TxFields {
            account: depositor,
            tx_type: "AMMDeposit".to_string(),
            fee: 12,
            sequence: 5,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Asset": {"currency": "PLX", "issuer": hex::encode(issuer)},
                "Asset2": {"currency": "XRP"},
                "Amount": {"currency": "PLX", "issuer": hex::encode(issuer), "value": "3651.65"},
                "Amount2": "446527",
                "Flags": 1_048_576u64,
            }),
        };
        // 446527 drops wanted, only 246527 liquid once the new LP line's
        // reserve is counted, and no LP line exists yet.
        assert_eq!(
            AMMDepositTransactor.preclaim(&tx, &Sandbox::new(&state)),
            TxResult::InsufReserveLine,
        );

        // Same shortfall, but with the LPToken line already present the
        // shortfall is funds rather than reserve.
        let lp_cur = amm_ctx(&tx, &Sandbox::new(&state)).expect("amm ctx").2.cur;
        let (lo, hi) = if depositor < amm_acct { (depositor, amm_acct) } else { (amm_acct, depositor) };
        let line = serde_json::json!({
            "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
            "Balance": {"currency": hex::encode_upper(lp_cur),
                        "issuer": "0000000000000000000000000000000000000000", "value": "0"},
            "LowLimit": {"currency": hex::encode_upper(lp_cur), "issuer": hex::encode(lo), "value": "1000000"},
            "HighLimit": {"currency": hex::encode_upper(lp_cur), "issuer": hex::encode(hi), "value": "1000000"},
        });
        state
            .state_map
            .insert(keylet::ripple_state_key(&depositor, &amm_acct, &lp_cur), serde_json::to_vec(&line).unwrap())
            .unwrap();
        // With the line present the reserve drops to 5.2 XRP, leaving exactly
        // 446527 liquid — and rippled's test is `>=`, so THIS deposit now just
        // fits. Ask for more than that to reach the other branch.
        assert_eq!(
            AMMDepositTransactor.preclaim(&tx, &Sandbox::new(&state)),
            TxResult::Success,
            "the freed reserve increment makes exactly this deposit fundable",
        );
        let mut bigger = tx.clone();
        bigger.fields["Amount2"] = serde_json::json!("600000");
        assert_eq!(
            AMMDepositTransactor.preclaim(&bigger, &Sandbox::new(&state)),
            TxResult::UnfundedAmm,
            "short with the line already there is funds, not reserve",
        );
    }

    #[test]
    fn amm_create_and_delete() {
        let alice = [0x01u8; 20];
        let state = make_state(&alice, 100_000_000);

        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: alice,
            tx_type: "AMMCreate".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Amount": "50000000",
                "Amount2": {"currency": "USD", "issuer": hex::encode([0x02u8; 20]), "value": "100"},
                "TradingFee": 500,
            }),
        };

        assert_eq!(AMMCreateTransactor.preflight(&tx), TxResult::Success);
        assert_eq!(AMMCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        // Check balance reduced
        let key = keylet::account_root_key(&alice);
        let data = sandbox.read(&key).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(v["Balance"].as_str().unwrap(), "50000000"); // 100M - 50M
        assert_eq!(v["OwnerCount"].as_u64().unwrap(), 1);
    }

    #[test]
    fn amm_vote() {
        let alice = [0x01u8; 20];
        let state = make_state(&alice, 100_000_000);

        // First create an AMM
        let mut sandbox = Sandbox::new(&state);
        let create_tx = TxFields {
            account: alice,
            tx_type: "AMMCreate".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Amount": "50000000",
                "Amount2": {"currency": "USD", "issuer": hex::encode([0x02u8; 20]), "value": "100"},
                "TradingFee": 500,
            }),
        };
        AMMCreateTransactor.do_apply(&create_tx, &mut sandbox);

        // Vote on it
        let vote_tx = TxFields {
            account: alice,
            tx_type: "AMMVote".to_string(),
            fee: 12,
            sequence: 2,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Asset": {"currency": "XRP"},
                "Asset2": {"currency": "USD", "issuer": hex::encode([0x02u8; 20])},
                "TradingFee": 300,
            }),
        };
        assert_eq!(AMMVoteTransactor.preflight(&vote_tx), TxResult::Success);
        assert_eq!(AMMVoteTransactor.do_apply(&vote_tx, &mut sandbox), TxResult::Success);
    }

    #[test]
    fn amm_deposit_withdraw() {
        let alice = [0x01u8; 20];
        let state = make_state(&alice, 100_000_000);

        let mut sandbox = Sandbox::new(&state);

        // Create AMM first
        let create_tx = TxFields {
            account: alice,
            tx_type: "AMMCreate".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Amount": "30000000",
                "Amount2": {"currency": "USD", "issuer": hex::encode([0x02u8; 20]), "value": "100"},
                "TradingFee": 500,
            }),
        };
        AMMCreateTransactor.do_apply(&create_tx, &mut sandbox);

        // Deposit more XRP
        let dep_tx = TxFields {
            account: alice,
            tx_type: "AMMDeposit".to_string(),
            fee: 12,
            sequence: 2,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Asset": {"currency": "XRP"},
                "Asset2": {"currency": "USD", "issuer": hex::encode([0x02u8; 20])},
                "Amount": "10000000",
            }),
        };
        assert_eq!(AMMDepositTransactor.do_apply(&dep_tx, &mut sandbox), TxResult::Success);

        // Balance: 100M - 30M - 10M = 60M
        let key = keylet::account_root_key(&alice);
        let data = sandbox.read(&key).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(v["Balance"].as_str().unwrap(), "60000000");

        // Withdraw
        let wd_tx = TxFields {
            account: alice,
            tx_type: "AMMWithdraw".to_string(),
            fee: 12,
            sequence: 3,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Asset": {"currency": "XRP"},
                "Asset2": {"currency": "USD", "issuer": hex::encode([0x02u8; 20])},
                "Amount": "5000000",
            }),
        };
        assert_eq!(AMMWithdrawTransactor.do_apply(&wd_tx, &mut sandbox), TxResult::Success);

        // Balance: 60M + 5M = 65M
        let data = sandbox.read(&key).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(v["Balance"].as_str().unwrap(), "65000000");
    }

    /// tfWithdrawAll redeems the LP's whole position and TEARS DOWN the LPToken
    /// trust line — deleted, not left Modified at zero. rippled AMMWithdraw
    /// apply-side under tfWithdrawAll; mainnet #105787513 / #105796380 emit the
    /// LP line as Deleted (:2), we used to Modify it (:1) and skip the payout.
    #[test]
    fn amm_withdraw_all_tears_down_lp_line() {
        let alice = [0x01u8; 20];
        let usd_issuer = [0x02u8; 20];
        let state = make_state(&alice, 100_000_000);
        let mut sandbox = Sandbox::new(&state);

        let create_tx = TxFields {
            account: alice, tx_type: "AMMCreate".to_string(), fee: 12, sequence: 1,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "Amount": "30000000",
                "Amount2": {"currency": "USD", "issuer": hex::encode(usd_issuer), "value": "100"},
                "TradingFee": 500,
            }),
        };
        assert_eq!(AMMCreateTransactor.do_apply(&create_tx, &mut sandbox), TxResult::Success);

        // A second deposit lifts her LPToken balance above the fixed default the
        // old partial path burns, so only a real full-balance burn empties the
        // line — otherwise the test would pass even without the branch.
        let dep = TxFields {
            account: alice, tx_type: "AMMDeposit".to_string(), fee: 12, sequence: 2,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "Asset": {"currency": "XRP"},
                "Asset2": {"currency": "USD", "issuer": hex::encode(usd_issuer)},
                "Amount": "10000000",
            }),
        };
        assert_eq!(AMMDepositTransactor.do_apply(&dep, &mut sandbox), TxResult::Success);

        let wd_all = TxFields {
            account: alice, tx_type: "AMMWithdraw".to_string(), fee: 12, sequence: 3,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "Asset": {"currency": "XRP"},
                "Asset2": {"currency": "USD", "issuer": hex::encode(usd_issuer)},
                "Flags": 0x0002_0000u64, // tfWithdrawAll
            }),
        };
        // Alice's LPToken trust line (she holds all the pool's LPTokens).
        let amm_key = amm_key_from_asset_fields(&wd_all).unwrap();
        let amm_obj: serde_json::Value = serde_json::from_slice(&sandbox.read(&amm_key).unwrap()).unwrap();
        let amm_acct = <[u8; 20]>::try_from(
            hex::decode(amm_obj["Account"].as_str().unwrap()).unwrap().as_slice(),
        ).unwrap();
        let lp_cur = keylet::amm_lpt_currency(
            &asset_currency20(&wd_all.fields["Asset"]),
            &asset_currency20(&wd_all.fields["Asset2"]),
        );
        let lp_line = keylet::ripple_state_key(&alice, &amm_acct, &lp_cur);
        assert!(sandbox.exists(&lp_line), "alice holds the LPToken line after create");

        assert_eq!(AMMWithdrawTransactor.do_apply(&wd_all, &mut sandbox), TxResult::Success);
        assert!(!sandbox.exists(&lp_line), "tfWithdrawAll deletes the LPToken trust line");
    }

    /// XRP drops on an account root, 0 when absent.
    fn xrp_drops(sb: &Sandbox, who: &[u8; 20]) -> u128 {
        sb.read(&keylet::account_root_key(who))
            .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
            .and_then(|a| a["Balance"].as_str().and_then(|v| v.parse::<u128>().ok()))
            .unwrap_or(0)
    }

    /// rippled's isWithdrawAll covers BOTH tfWithdrawAll (0x00020000) and
    /// tfOneAssetWithdrawAll (0x00040000) — AMMWithdraw.cpp:1133-1138. Under
    /// the latter the `Amount` is a MINIMUM, not the size of the withdrawal:
    /// the LP burns their ENTIRE token balance and takes it out in that ONE
    /// asset, sized by ammAssetOut.
    ///
    /// #105929166 39E145693A80 and #105922945 15FAEA4CC56D: mainnet DELETES the
    /// LP trust line (`:2`) and Modifies both owner directories; we emitted it
    /// Modified (`:1`) with a residual balance and left the directories alone.
    /// The same key appearing as missing-Deleted AND extra-Modified is the
    /// whole signature. Flags 262144 is easy to misread as tfSingleAsset —
    /// that is 0x00080000, a different transaction entirely.
    #[test]
    fn one_asset_withdraw_all_empties_the_position() {
        let alice = [0x01u8; 20];
        let usd_issuer = [0x02u8; 20];
        let state = make_state(&alice, 200_000_000);
        let mut sandbox = Sandbox::new(&state);

        let create_tx = TxFields {
            account: alice, tx_type: "AMMCreate".to_string(), fee: 12, sequence: 1,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "Amount": "40000000",
                "Amount2": {"currency": "USD", "issuer": hex::encode(usd_issuer), "value": "100"},
                "TradingFee": 236,
            }),
        };
        assert_eq!(AMMCreateTransactor.do_apply(&create_tx, &mut sandbox), TxResult::Success);

        let wd = TxFields {
            account: alice, tx_type: "AMMWithdraw".to_string(), fee: 12, sequence: 2,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "Asset": {"currency": "XRP"},
                "Asset2": {"currency": "USD", "issuer": hex::encode(usd_issuer)},
                // Amount names the side and acts as a FLOOR, not the size.
                "Amount": "1",
                "Flags": 0x0004_0000u64, // tfOneAssetWithdrawAll
            }),
        };
        let amm_key = amm_key_from_asset_fields(&wd).unwrap();
        let amm_obj: serde_json::Value = serde_json::from_slice(&sandbox.read(&amm_key).unwrap()).unwrap();
        let amm_acct = <[u8; 20]>::try_from(
            hex::decode(amm_obj["Account"].as_str().unwrap()).unwrap().as_slice(),
        ).unwrap();
        let lp_cur = keylet::amm_lpt_currency(
            &asset_currency20(&wd.fields["Asset"]),
            &asset_currency20(&wd.fields["Asset2"]),
        );
        let lp_line = keylet::ripple_state_key(&alice, &amm_acct, &lp_cur);
        assert!(sandbox.exists(&lp_line), "alice holds the LPToken line after create");

        let xrp_before = xrp_drops(&sandbox, &alice);
        assert_eq!(AMMWithdrawTransactor.do_apply(&wd, &mut sandbox), TxResult::Success);

        assert!(
            !sandbox.exists(&lp_line),
            "tfOneAssetWithdrawAll empties the position, so the LPToken line is torn down",
        );
        assert!(
            xrp_drops(&sandbox, &alice) > xrp_before,
            "and the named side is paid out",
        );
    }

    /// tfLPToken: the LP names only how many LPTokens to redeem and receives
    /// BOTH pool assets in proportion — the transaction carries no Amount or
    /// Amount2 at all. rippled's `equalWithdrawTokens` (AMMWithdraw.cpp:790-850)
    /// pays out amountBalance*frac and amount2Balance*frac where
    /// frac = LPTokenIn / lptAMMBalance.
    ///
    /// We only ever moved Amount/Amount2, found neither, and paid out nothing
    /// while still burning the tokens. #105880685 F2CCA2BD6FA4, #105840045
    /// 41110275D9B7 (same XRP/FARM pool) and #105877543 331E54E698CA — all
    /// our_muts=5 vs net_muts=8, the three missing nodes being exactly the two
    /// RippleStates and the AccountRoot a payout touches.
    #[test]
    fn amm_withdraw_by_lp_tokens_pays_out_both_assets() {
        let alice = [0x01u8; 20];
        let usd_issuer = [0x02u8; 20];
        let state = make_state(&alice, 200_000_000);
        let mut sandbox = Sandbox::new(&state);

        let create_tx = TxFields {
            account: alice, tx_type: "AMMCreate".to_string(), fee: 12, sequence: 1,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "Amount": "40000000",
                "Amount2": {"currency": "USD", "issuer": hex::encode(usd_issuer), "value": "100"},
                "TradingFee": 500,
            }),
        };
        assert_eq!(AMMCreateTransactor.do_apply(&create_tx, &mut sandbox), TxResult::Success);

        let wd = TxFields {
            account: alice, tx_type: "AMMWithdraw".to_string(), fee: 12, sequence: 2,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "Asset": {"currency": "XRP"},
                "Asset2": {"currency": "USD", "issuer": hex::encode(usd_issuer)},
                "Flags": 0x0001_0000u64, // tfLPToken — note: no Amount/Amount2
            }),
        };
        let amm_key = amm_key_from_asset_fields(&wd).unwrap();
        let amm_obj: serde_json::Value = serde_json::from_slice(&sandbox.read(&amm_key).unwrap()).unwrap();
        let amm_acct = <[u8; 20]>::try_from(
            hex::decode(amm_obj["Account"].as_str().unwrap()).unwrap().as_slice(),
        ).unwrap();
        let total_lp = amm_obj["LPTokenBalance"]["value"].as_str().unwrap().to_string();

        // Redeem a quarter of the outstanding LPTokens.
        let quarter = total_lp.parse::<f64>().unwrap() / 4.0;
        let mut wd = wd;
        wd.fields["LPTokenIn"] = serde_json::json!({
            "currency": hex::encode_upper(keylet::amm_lpt_currency(
                &asset_currency20(&wd.fields["Asset"]),
                &asset_currency20(&wd.fields["Asset2"]),
            )),
            "issuer": hex::encode(amm_acct),
            "value": format!("{quarter}"),
        });

        let xrp_before = xrp_drops(&sandbox, &alice);
        let pool_xrp_before = xrp_drops(&sandbox, &amm_acct);
        assert_eq!(AMMWithdrawTransactor.do_apply(&wd, &mut sandbox), TxResult::Success);
        let xrp_after = xrp_drops(&sandbox, &alice);
        let pool_xrp_after = xrp_drops(&sandbox, &amm_acct);

        assert!(
            xrp_after > xrp_before,
            "redeeming LPTokens must pay XRP out to the LP (before={xrp_before} after={xrp_after})",
        );
        assert!(
            pool_xrp_after < pool_xrp_before,
            "and the pool's XRP must fall by the same withdrawal",
        );
        assert_eq!(
            xrp_after - xrp_before,
            pool_xrp_before - pool_xrp_after,
            "XRP is conserved between pool and LP",
        );
    }

    #[test]
    fn amm_deposit_reserve_guard() {
        // rippled AMMDeposit apply-side reserve check (#105770848/#105783986):
        // a depositor holding ZERO LPTokens must keep XRP above
        // accountReserve(ownerCount + 1) or the deposit fails
        // tecINSUF_RESERVE_LINE — the LPToken line it would open costs reserve.
        let alice = [0x01u8; 20];
        let usd_issuer = [0x02u8; 20];
        let state = make_state(&alice, 100_000_000);
        let mut sandbox = Sandbox::new(&state);

        // Alice funds an XRP/USD pool; she ends up holding the LPTokens.
        let create_tx = TxFields {
            account: alice,
            tx_type: "AMMCreate".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Amount": "30000000",
                "Amount2": {"currency": "USD", "issuer": hex::encode(usd_issuer), "value": "100"},
                "TradingFee": 500,
            }),
        };
        assert_eq!(AMMCreateTransactor.do_apply(&create_tx, &mut sandbox), TxResult::Success);

        let deposit = |acct: [u8; 20]| TxFields {
            account: acct,
            tx_type: "AMMDeposit".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Asset": {"currency": "XRP"},
                "Asset2": {"currency": "USD", "issuer": hex::encode(usd_issuer)},
                "Amount": "500000",
            }),
        };
        let put_acct = |sb: &mut Sandbox, id: &[u8; 20], bal: &str| {
            sb.write(
                keylet::account_root_key(id),
                serde_json::to_vec(&serde_json::json!({
                    "LedgerEntryType": "AccountRoot",
                    "Account": hex::encode(id),
                    "Balance": bal,
                    "Sequence": 1,
                    "OwnerCount": 0,
                }))
                .unwrap(),
            );
        };

        // Broke depositor, no LPTokens: 1 XRP <= accountReserve(1) = 1.2 XRP → fail.
        let broke = [0x03u8; 20];
        put_acct(&mut sandbox, &broke, "1000000");
        assert_eq!(
            AMMDepositTransactor.do_apply(&deposit(broke), &mut sandbox),
            TxResult::InsufReserveLine
        );

        // Funded depositor, no LPTokens: well above reserve → deposit proceeds.
        let rich = [0x04u8; 20];
        put_acct(&mut sandbox, &rich, "100000000");
        assert_eq!(
            AMMDepositTransactor.do_apply(&deposit(rich), &mut sandbox),
            TxResult::Success
        );
    }
}
