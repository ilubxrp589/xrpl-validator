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
/// An `Asset`/`Asset2` field ({"currency"[, "issuer"]}) as a walk leg: XRP
/// when it names XRP, else the 160-bit currency with its issuer.
fn asset_leg(v: &serde_json::Value) -> Option<crate::tx::offer::Leg> {
    let cur = v.get("currency").and_then(|c| c.as_str())?;
    if cur == "XRP" && v.get("issuer").is_none() {
        return Some(crate::tx::offer::Leg { xrp: true, cur: [0u8; 20], issuer: [0u8; 20] });
    }
    let issuer_s = v.get("issuer").and_then(|i| i.as_str())?;
    let issuer = crate::tx::offer::decode20(issuer_s)?;
    Some(crate::tx::offer::Leg { xrp: false, cur: asset_currency20(v), issuer })
}

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
        use crate::tx::offer as ox;
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) { return TxResult::NoAccount; }
        // AMMCreate::preclaim, in rippled's order (AMMCreate.cpp:100-186).
        let (Some(a1), Some(a2)) = (tx.fields.get("Amount"), tx.fields.get("Amount2")) else {
            return TxResult::Malformed;
        };
        let (Some(l1), Some(l2)) = (ox::leg_of(a1), ox::leg_of(a2)) else {
            return TxResult::Malformed;
        };
        // 1. A pool for this pair already exists — tecDUPLICATE (:101-106).
        //    The probe hydrates the existing pool for AMMCreate exactly so
        //    this is decidable; absence (fresh pair) is the normal case.
        //    #106118993 D98E76D5 recreates XRP/USD rvYAfWj5 — mainnet says
        //    tecDUPLICATE where we answered tecNO_PERMISSION.
        let pool = keylet::amm_key(&l1.cur, &l1.issuer, &l2.cur, &l2.issuer);
        if sandbox.exists(&pool) {
            return TxResult::Duplicate;
        }
        let Some(acct) = ox::json_at(sandbox, &acct_key) else {
            return TxResult::NoAccount;
        };
        let bal: u128 =
            acct["Balance"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
        let oc = acct["OwnerCount"].as_u64().unwrap_or(0);
        // 2. The creator must clear the reserve WITH the LPToken line it is
        //    about to open: xrpLiquid(account, +1) ≤ 0 is
        //    tecINSUF_RESERVE_LINE (:150-157). Preclaim runs pre-fee, which
        //    IS rippled's view of it.
        let liquid = bal
            .saturating_sub(crate::ledger::fees::account_reserve(sandbox, oc + 1) as u128);
        if liquid == 0 {
            return TxResult::InsufReserveLine;
        }
        // 3. Funds (:159-175): the XRP side against that same liquid; an IOU
        //    side via accountFunds under ZeroIfFrozen — offer::available's
        //    exact semantics (ZeroIfUnauthorized is not modeled; require-auth
        //    issuers do not appear on the corpora). Falling short is
        //    tecUNFUNDED_AMM. #106071927 3CCED155: a DRGN/BURN create whose
        //    creator cannot fund a side — we built the whole pool, 10
        //    mutations against mainnet's fee.
        for (v, leg) in [(a1, &l1), (a2, &l2)] {
            let Some(amt) = keylet::amount_mant_exp(v) else { continue };
            if amt.0 == 0 {
                continue;
            }
            let short = if leg.xrp {
                liquid < ox::me_rescale(amt, 0, false)
            } else {
                ox::me_cmp(ox::available(sandbox, &tx.account, leg), amt).is_lt()
            };
            if short {
                return TxResult::UnfundedAmm;
            }
        }
        // 4. Neither side may itself be an LP token: an issuer whose
        //    AccountRoot carries AMMID refuses with tecAMM_INVALID_TOKENS
        //    (:177-186). Only a READABLE issuer condemns (collect_issuers
        //    hydrates every named issuer).
        for leg in [&l1, &l2] {
            if leg.xrp {
                continue;
            }
            if ox::json_at(sandbox, &keylet::account_root_key(&leg.issuer))
                .map(|r| r.get("AMMID").is_some())
                .unwrap_or(false)
            {
                return TxResult::AmmInvalidTokens;
            }
        }
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

        // rippled orders the pool pair (std::minmax on Issue — currency,
        // then issuer): the OBJECT's Asset/Asset2 carry the sorted pair just
        // like the keylet input does, regardless of the tx's Amount/Amount2
        // order. #106674486: ours were swapped with everything else already
        // byte-equal.
        let (asset1, asset2) =
            if (&c1, &i1) <= (&c2, &i2) { (asset1, asset2) } else { (asset2, asset1) };

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
            // view.seq() is the ledger BEING BUILT — parent + 1.
            "Sequence": sandbox.base().header.sequence + 1,
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
                    // rippled AMMCreate sendAndTrustSet: every non-XRP asset
                    // line of the pool is stamped lsfAMMNode (AMMCreate.cpp:
                    // 288-300). The LP-token line is NOT stamped — the live
                    // shadow's #106674486 gate showed both asset lines a bit
                    // short (0x00010000 vs 0x01010000) and the LP line clean.
                    if !leg.xrp {
                        let lk = keylet::ripple_state_key(&amm_acct, &leg.issuer, &leg.cur);
                        if let Some(mut line) = ox::json_at(sandbox, &lk) {
                            let f = line["Flags"].as_u64().unwrap_or(0) | 0x0100_0000;
                            line["Flags"] = serde_json::json!(f);
                            ox::put_json(sandbox, lk, &line);
                        }
                    }
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

        // Create AMM ledger entry. rippled dirLinks the AMM object into the
        // AMM account's own owner directory (AMMCreate.cpp:263, no reserve /
        // OwnerCount) and the object carries the resulting OwnerNode plus an
        // explicit zero Flags — #106674486's gate counted exactly those 14
        // missing bytes and the absent directory entry.
        let amm_owner_node = crate::ledger::directory::owner_dir_insert(sandbox, &amm_acct, &amm_hash);
        let amm_obj = serde_json::json!({
            "LedgerEntryType": "AMM",
            "Flags": 0,
            "Account": hex::encode(amm_acct),
            "OwnerNode": format!("{amm_owner_node:x}"),
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

        // rippled AMMCreate.cpp:260 initializes the fee, auction slot and
        // vote slots on the freshly created object; the placeholders above
        // were all a created AMM ever got until 2026-08-31 (live shadow
        // #106674486: ours 205B vs canonical 333B — the missing 128 bytes
        // ARE the AuctionSlot + VoteEntry). The helper already carried the
        // faithful port for the deposit-revival path.
        let tfee = tx.fields.get("TradingFee").and_then(|f| f.as_u64()).unwrap_or(0) as u16;
        initialize_fee_auction_vote(sandbox, &amm_hash, &tx.account, tfee, &lp_leg);

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

/// Reduce a mantissa to 16 significant digits rounding HALF-EVEN on the full
/// shed remainder — Number's plain addition semantics (finding 36: the LP
/// sum's shed .6 rounds up where truncation and the old exact-string path
/// both landed one ULP low).
fn round16_nearest(m: crate::tx::offer::Me) -> crate::tx::offer::Me {
    let mut t = m.0;
    let mut k = 0u32;
    while t >= 10_000_000_000_000_000 {
        t /= 10;
        k += 1;
    }
    if k == 0 {
        return m;
    }
    let d = 10u128.pow(k);
    let (q, r) = (m.0 / d, m.0 % d);
    let mut q = match (r * 2).cmp(&d) {
        std::cmp::Ordering::Greater => q + 1,
        std::cmp::Ordering::Less => q,
        std::cmp::Ordering::Equal => q + (q & 1),
    };
    let mut e = m.1 + k as i32;
    if q >= 10_000_000_000_000_000 {
        q /= 10;
        e += 1;
    }
    (q, e)
}

/// fixAMMv1_1 `verifyAndAdjustLPTokenBalance` (AMMUtils.cpp), run by
/// AMMWithdraw before anything is sized: the last LP's trust line and the
/// AMM's LPTokenBalance drift apart by rounding dust over the pool's life,
/// so when the withdrawer is the ONLY liquidity provider — no other
/// account holds an LPToken line in the AMM account's owner directory
/// (`isOnlyLiquidityProvider`) — the object's LPTokenBalance is snapped to
/// the LP's line balance, provided the two are within 1e-3 relative
/// distance (`withinRelativeDistance`, else tecAMM_INVALID_TOKENS).
///
/// #106696868 DFC7A0F4 (finding 63, tfTwoAsset by the sole LP): the line
/// held 507670.4691518975 against an object balance of 507670.469151897.
/// Sized from the object, every step — tokens UP, adjustLPTokens, SCRATCH
/// DOWN — lands one ulp off (…392 for …394 tokens, …620 for …619
/// SCRATCH) and the object and line end 5 ulps apart (…578 / …583);
/// snapped first, both end at mainnet's …581.
fn verify_and_adjust_lp_token_balance(
    sandbox: &mut Sandbox,
    amm_key: &xrpl_core::types::Hash256,
    amm_acct: &[u8; 20],
    lp: &[u8; 20],
    lp_leg: &crate::tx::offer::Leg,
    lp_tokens: crate::tx::offer::Me,
) -> Result<(), TxResult> {
    use crate::tx::offer as ox;
    // isOnlyLiquidityProvider: any LPToken line in the AMM's owner
    // directory whose other party is not this LP is another LP.
    let dir_root = keylet::owner_dir_key(amm_acct);
    let mut page_key = dir_root;
    let lp_cur = hex::encode_upper(lp_leg.cur);
    for _ in 0..1000 {
        let Some(page) = ox::json_at(sandbox, &page_key) else { break };
        for idx in page.get("Indexes").and_then(|v| v.as_array()).into_iter().flatten() {
            let Some(k) = idx
                .as_str()
                .and_then(|s| hex::decode(s).ok())
                .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
            else {
                continue;
            };
            let Some(obj) = ox::json_at(sandbox, &xrpl_core::types::Hash256(k)) else { continue };
            if obj["LedgerEntryType"].as_str() != Some("RippleState") {
                continue;
            }
            if !obj["LowLimit"]["currency"].as_str().unwrap_or("").eq_ignore_ascii_case(&lp_cur) {
                continue;
            }
            let low = obj["LowLimit"]["issuer"].as_str().and_then(ox::decode20);
            let high = obj["HighLimit"]["issuer"].as_str().and_then(ox::decode20);
            if low != Some(*lp) && high != Some(*lp) {
                return Ok(()); // another LP holds LPTokens: no snap
            }
        }
        let next = page
            .get("IndexNext")
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| u64::from_str_radix(s, 16).ok())))
            .unwrap_or(0);
        if next == 0 {
            break;
        }
        page_key = keylet::dir_page_key(&dir_root, next);
    }
    let Some(mut obj) = ox::json_at(sandbox, amm_key) else { return Ok(()) };
    let Some(cur) = obj["LPTokenBalance"]["value"]
        .as_str()
        .and_then(|v| keylet::amount_mant_exp(&serde_json::Value::String(v.to_string())))
    else {
        return Ok(());
    };
    if ox::me_cmp(cur, lp_tokens).is_eq() {
        return Ok(());
    }
    // withinRelativeDistance: (max - min) / max < 1e-3
    let (lo, hi) = if ox::me_cmp(cur, lp_tokens).is_lt() { (cur, lp_tokens) } else { (lp_tokens, cur) };
    let ratio = ox::st_divide(ox::me_sub(hi, lo), hi, false);
    if !ox::me_cmp(ratio, (1_000_000_000_000_000, -18)).is_lt() {
        return Err(TxResult::AmmInvalidTokens);
    }
    if std::env::var("DX_AMMWD").is_ok() {
        eprintln!("DX_AMMWD only-LP snap: LPTokenBalance {cur:?} -> line {lp_tokens:?}");
    }
    obj["LPTokenBalance"]["value"] = serde_json::Value::String(ox::me_to_value_string(lp_tokens));
    ox::put_json(sandbox, *amm_key, &obj);
    Ok(())
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
    // Finding 36 (#106644320 749D3E45, tfTwoAsset deposit): the minted LP
    // amount was already byte-exact (receipts: minted 251769204.3826585 ==
    // truth to all 16 digits) — the divergence is THE ADD: the exact sum
    // 279228264.81815046 must 16-digit-round NEAREST (truth keeps …1505;
    // both the old exact-string path and a downward round give …1504).
    // Number's plain addition semantics — half-even on the shed remainder.
    // F84 — the burn is the same Number subtraction: nearest on BOTH sides.
    // #106702692 6F7C52C0 (tfWithdrawAll): 29320.99190061032 − 993.427226328693
    // = 28327.564674281627 → mainnet 28327.56467428163, ours truncated …62.
    let mag = round16_nearest(mag);
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
/// Is the amendment with this 64-hex feature id active in THIS state? Read
/// straight off the Amendments singleton, so every world — probe hydration,
/// full-state replay, the live mirror — answers for its own era. The census
/// taught the need: #105880685 (pre-fixAMMv1_3) demands the legacy withdraw
/// fraction while #106629215 (post) demands the ceil; rippled's own code
/// splits on rules().enabled(fixAMMv1_3).
#[allow(dead_code)] // first amendment-reading helper; gates will want it
fn amendment_enabled(sandbox: &Sandbox, id_hex: &str) -> bool {
    ox::json_at(sandbox, &keylet::amendments_key())
        .and_then(|o| o.get("Amendments").cloned())
        .and_then(|a| a.as_array().cloned())
        .map(|a| a.iter().any(|v| v.as_str().is_some_and(|s| s.eq_ignore_ascii_case(id_hex))))
        .unwrap_or(false)
}

#[allow(dead_code)]
const FIX_AMM_V1_3: &str = "7CA70A7674A26FA517412858659EBC7EDEEF7D2D608824464E6FDEFD06854E14";


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
        let out = mul_directed(out_balance, frac, false, out_xrp);
        if std::env::var("DX_AMMWD").is_ok() {
            eprintln!("DX_AMMWD led num={num:?} den={den:?} tokens={tokens:?} frac={frac:?} out={out:?} (out_balance={out_balance:?})");
        }
        Some((out, tokens))
    };
    if let Some((a2, t)) = led(amount, amount_balance, amount2_balance, amount2_xrp) {
        if std::env::var("DX_AMMWD").is_ok() {
            eprintln!("DX_AMMWD way1 a2={a2:?} <= amount2={amount2:?} ? {}", ox::me_cmp(a2, amount2).is_le());
        }
        if ox::me_cmp(a2, amount2).is_le() {
            return Some((amount, a2, t));
        }
    }
    if let Some((a1, t)) = led(amount2, amount2_balance, amount_balance, amount_xrp) {
        if std::env::var("DX_AMMWD").is_ok() {
            eprintln!("DX_AMMWD way2 a1={a1:?} <= amount={amount:?} ? {}", ox::me_cmp(a1, amount).is_le());
        }
        if ox::me_cmp(a1, amount).is_le() {
            return Some((a1, amount2, t));
        }
    }
    None
}

// ─── AMMDeposit ───

/// One leg of rippled's AMMDeposit funds test — the `balance` lambda in
/// preclaim (AMMDeposit.cpp:230-252) and `checkBalance` in deposit()
/// (AMMDeposit.cpp:514-538) share this shape. XRP is judged as `xrpLiquid`
/// with the owner count bumped by one when the depositor has NO LPToken line
/// yet (the deposit is about to create it); an IOU as `accountFunds` under
/// IgnoreFreeze — the raw signed line balance, the leg's issuer never short,
/// a missing line holding nothing. Freeze is deliberately NOT consulted:
/// rippled refuses frozen assets elsewhere (the AMMClawback preclaim check),
/// not in this lambda — zeroing frozen holdings here would swap rippled's
/// freeze verdict for a wrong tecUNFUNDED_AMM.
fn deposit_leg_funded(
    sandbox: &Sandbox,
    who: &[u8; 20],
    leg: &crate::tx::offer::Leg,
    amt: crate::tx::offer::Me,
    amm_acct: &[u8; 20],
    lp_cur: &[u8; 20],
) -> bool {
    use crate::tx::offer as ox;
    let dbg = std::env::var("DX_AMMFUND").is_ok();
    if leg.xrp {
        let Some(acct) = ox::json_at(sandbox, &keylet::account_root_key(who)) else {
            return true; // unreadable depositor: never condemn on absence
        };
        let oc = acct["OwnerCount"].as_u64().unwrap_or(0);
        let bal = acct["Balance"].as_str().and_then(|s| s.parse::<u128>().ok()).unwrap_or(0);
        let lp_line = keylet::ripple_state_key(who, amm_acct, lp_cur);
        let adj = u64::from(!sandbox.exists(&lp_line));
        let liquid =
            bal.saturating_sub(crate::ledger::fees::account_reserve(sandbox, oc + adj) as u128);
        let want = ox::me_rescale(amt, 0, false);
        if dbg {
            eprintln!("DX_AMMFUND xrp bal={bal} oc={oc} adj={adj} liquid={liquid} want={want}");
        }
        liquid >= want
    } else if who == &leg.issuer {
        true // the issuer funds its own IOU without limit
    } else {
        let Some(line) =
            ox::json_at(sandbox, &keylet::ripple_state_key(who, &leg.issuer, &leg.cur))
        else {
            if dbg {
                eprintln!("DX_AMMFUND iou NO-LINE want={amt:?}");
            }
            return false; // no line, nothing held
        };
        let (neg, bal) = ox::signed_value(&line["Balance"]);
        // Balance is stored from the LOW account's perspective.
        let party_holds = if who < &leg.issuer { !neg } else { neg };
        let ok = party_holds && bal.0 > 0 && ox::me_cmp(bal, amt) != std::cmp::Ordering::Less;
        if dbg {
            eprintln!("DX_AMMFUND iou holds={party_holds} bal={bal:?} want={amt:?} ok={ok}");
        }
        ok
    }
}

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
        // featureAMMClawback (AMMDeposit.cpp:255-283): BOTH pool assets pass
        // requireAuth(WeakAuth) then checkFrozen for the depositor, before
        // any funding arithmetic — a depositor with no line to a
        // RequireAuth issuer is tecNO_LINE, an unauthorized line tecNO_AUTH,
        // a frozen one tecFROZEN. Then each deposited amount's asset passes
        // the STRONG check (:297) — no line at all is tecNO_LINE regardless
        // of the issuer's flags. Finding 104 (#106721484 3F14213E0C76).
        for f in ["Asset", "Asset2"] {
            let Some(leg) = tx.fields.get(f).and_then(asset_leg) else { continue };
            if let Some(t) = ox::require_auth_ter(sandbox, &leg, &tx.account, false) {
                if t != TxResult::Success { return t; }
            }
            if let Some(t) = ox::frozen_ter(sandbox, &leg, &tx.account) {
                if t != TxResult::Success { return t; }
            }
        }
        for f in ["Amount", "Amount2"] {
            let Some(leg) = tx.fields.get(f).and_then(ox::leg_of) else { continue };
            if let Some(t) = ox::require_auth_ter(sandbox, &leg, &tx.account, true) {
                if t != TxResult::Success { return t; }
            }
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
                if amt.0 == 0 {
                    continue;
                }
                if !leg.xrp {
                    // The IOU arm of the same `balance` lambda
                    // (AMMDeposit.cpp:245-252): accountFunds under
                    // IgnoreFreeze must cover the STATED amount, else
                    // tecUNFUNDED_AMM — no INSUF_RESERVE_LINE split on this
                    // side, that verdict is the XRP branch's alone.
                    if !deposit_leg_funded(sandbox, &tx.account, &leg, amt, &amm_acct, &lp_leg.cur)
                    {
                        return TxResult::UnfundedAmm;
                    }
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
        // F82 — tfLimitLPToken (Amount + EPrice) is the single-asset deposit
        // with a price ceiling: `singleDepositEPrice` (AMMDeposit.cpp) first
        // tries the full Amount at Equation 3 and keeps it when the effective
        // price deposited/tokens is within EPrice; otherwise it solves the
        // quadratic for the amount that lands exactly on EPrice. This mode
        // fell through to the 1e7 placeholder. #106702459 F648B73A: 11.172545
        // XRP into Gta6/XRP at EPrice 4 drops — mainnet mints 3320034.6897731
        // (3.365 drops each), we minted 10000000.
        const TF_LIMIT_LP_TOKEN: u64 = 0x0040_0000;
        let flags_dep = tx.fields.get("Flags").and_then(|v| v.as_u64()).unwrap_or(0);
        let eprice = if flags_dep & TF_LIMIT_LP_TOKEN != 0 {
            tx.fields.get("EPrice").and_then(keylet::amount_mant_exp).filter(|m| m.0 > 0)
        } else {
            None
        };
        // F87 — tfOneAssetLPToken (Amount + LPTokenOut): `singleDepositTokens`
        // (AMMDeposit.cpp) adjusts the requested tokens, derives the asset
        // with ammAssetIn, and refuses tecAMM_FAILED when that exceeds Amount
        // (the stated Amount is a MAXIMUM). This mode also fell through to the
        // placeholder mover. #106704718 DC1B6BD3.
        const TF_ONE_ASSET_LP_TOKEN: u64 = 0x0020_0000;
        let lp_token_out = if flags_dep & TF_ONE_ASSET_LP_TOKEN != 0 {
            tx.fields.get("LPTokenOut").and_then(keylet::amount_mant_exp).filter(|m| m.0 > 0)
        } else {
            None
        };
        let single_pre = if flags_dep & TF_SINGLE_ASSET != 0 || eprice.is_some() || lp_token_out.is_some() {
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
        //
        // Finding 39 (#106668165 3691EF47, 1-drop XRP into XRP/SPY): tokens
        // that ADJUST TO ZERO are a refusal, not an absence. rippled's
        // singleDeposit fails tecAMM_INVALID_TOKENS the moment
        // adjustLPTokensOut rounds the dust away (AMMDeposit.cpp — fixAMMv1_3
        // arm; adjustAssetInByTokens zero is the same verdict, and deposit()'s
        // epilogue re-checks `lpTokensDepositActual <= 0`). Our old shape
        // returned None here, and the mover's fallback then deposited the RAW
        // stated amount with tesSUCCESS — the exact live shadow catch (3 extra
        // keys, PRE-OK byte diff, ter flip). Err carries the refusal past the
        // "not a single-asset deposit" meaning of None.
        let mut one_asset_lp_token_over = false;
        let single_adj: Option<Result<(ox::Me, ox::Me), ()>> =
            single_pre.and_then(|(pool_pre, amt)| {
                let obj = ox::json_at(sandbox, &amm_key)?;
                let lpt = keylet::amount_mant_exp(&serde_json::Value::String(
                    obj["LPTokenBalance"]["value"].as_str()?.to_string(),
                ))?;
                // F66: the slot holder deposits at the DISCOUNTED fee (getTradingFee).
                let tfee = crate::tx::amm_swap::effective_trading_fee(sandbox, &obj, &tx.account);
                let xrp = tx.fields.get("Amount").and_then(ox::leg_of).map(|l| l.xrp)?;
                if let Some(want_tokens) = lp_token_out {
                    let tokens_adj = crate::tx::amm_swap::adjust_lp_tokens_out(lpt, want_tokens);
                    if tokens_adj.0 == 0 {
                        return Some(Err(()));
                    }
                    let Some(amount_dep) = crate::tx::amm_swap::amm_asset_in(pool_pre, lpt, tokens_adj, tfee, xrp) else {
                        return Some(Err(()));
                    };
                    if crate::tx::amm_swap::n_cmp(amount_dep, amt) == std::cmp::Ordering::Greater {
                        one_asset_lp_token_over = true;
                        return Some(Err(()));
                    }
                    return Some(Ok((amount_dep, tokens_adj)));
                }
                let t0 = crate::tx::amm_swap::adjust_lp_tokens(
                    lpt,
                    crate::tx::amm_swap::lp_tokens_out(pool_pre, amt, lpt, tfee),
                    true,
                );
                if t0.0 == 0 {
                    return Some(Err(()));
                }
                let (tokens, deposited) = crate::tx::amm_swap::adjust_asset_in_by_tokens(
                    pool_pre, amt, lpt, t0, tfee, xrp,
                );
                if tokens.0 == 0 || deposited.0 == 0 {
                    return Some(Err(()));
                }
                let Some(ep_max) = eprice else {
                    return Some(Ok((deposited, tokens)));
                };
                use crate::tx::amm_swap::{n_add, n_div, n_mul, n_sqrt, n_sub, Rnd};
                let ep = n_div(deposited, tokens, Rnd::Near);
                if crate::tx::amm_swap::n_cmp(ep, ep_max) != std::cmp::Ordering::Greater {
                    return Some(Ok((deposited, tokens)));
                }
                // Past the ceiling: the amount whose price is exactly EPrice.
                // R = (-b1 + sqrt(b1^2 - 4*a1*c1)) / (2*a1) with
                //   f1 = 1 - fee, f2 = (1 - fee/2) / f1, c = f1*B / (E*T),
                //   d = f1 + c*f2 - c, a1 = c^2, b1 = (c*f2)^2 + 2c - d^2,
                //   c1 = 2c*f2^2 + 1 - 2*d*f2   (AMMDeposit.cpp, singleDepositEPrice)
                let one: ox::Me = (1_000_000_000_000_000, -15);
                let fee = (tfee as u128, -5);
                let f1 = n_sub(one, fee, Rnd::Near);
                let f2 = n_div(n_sub(one, (tfee as u128 * 5, -6), Rnd::Near), f1, Rnd::Near);
                let c = n_div(n_mul(f1, pool_pre, Rnd::Near), n_mul(ep_max, lpt, Rnd::Near), Rnd::Near);
                let d = n_sub(n_add(f1, n_mul(c, f2, Rnd::Near), Rnd::Near), c, Rnd::Near);
                let a1 = n_mul(c, c, Rnd::Near);
                let cf2 = n_mul(c, f2, Rnd::Near);
                let b1 = n_sub(n_add(n_mul(cf2, cf2, Rnd::Near), n_mul((2, 0), c, Rnd::Near), Rnd::Near), n_mul(d, d, Rnd::Near), Rnd::Near);
                let c1 = n_sub(
                    n_add(n_mul((2, 0), n_mul(c, n_mul(f2, f2, Rnd::Near), Rnd::Near), Rnd::Near), one, Rnd::Near),
                    n_mul((2, 0), n_mul(d, f2, Rnd::Near), Rnd::Near),
                    Rnd::Near,
                );
                let disc = n_sub(n_mul(b1, b1, Rnd::Near), n_mul((4, 0), n_mul(a1, c1, Rnd::Near), Rnd::Near), Rnd::Near);
                if disc.0 == 0 && b1.0 != 0 {
                    return Some(Err(()));
                }
                let r = n_div(n_sub(n_sqrt(disc), b1, Rnd::Near), n_mul((2, 0), a1, Rnd::Near), Rnd::Near);
                // getRoundedAsset(deposit): multiply(balance, f1*R, upward)
                let amount_dep = crate::tx::amm_swap::to_amount(n_mul(pool_pre, n_mul(f1, r, Rnd::Near), Rnd::Up), xrp, Rnd::Up);
                if amount_dep.0 == 0 {
                    return Some(Err(()));
                }
                // getRoundedLPTokens(deposit): tokens = amount / EPrice rounded DOWN, then adjustLPTokens
                let tok = crate::tx::amm_swap::adjust_lp_tokens(lpt, n_div(amount_dep, ep_max, Rnd::Down), true);
                if tok.0 == 0 {
                    return Some(Err(()));
                }
                let (tokens2, deposited2) = crate::tx::amm_swap::adjust_asset_in_by_tokens(
                    pool_pre, amount_dep, lpt, tok, tfee, xrp,
                );
                Some(if tokens2.0 > 0 && deposited2.0 > 0 { Ok((deposited2, tokens2)) } else { Err(()) })
            });
        if matches!(single_adj, Some(Err(()))) {
            return if one_asset_lp_token_over { TxResult::AmmFailed } else { TxResult::AmmInvalidTokens };
        }
        let single_adj: Option<(ox::Me, ox::Me)> = single_adj.and_then(|r| r.ok());

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
                        // rippled re-checks each ACTUAL amount right before
                        // sending it (`checkBalance`, AMMDeposit.cpp:514-538
                        // — check, send, check, send; both branches verdict
                        // tecUNFUNDED_AMM). Preclaim passed the STATED
                        // amounts on the PRE-fee balance; by now the fee is
                        // gone and the amounts are the derived actuals.
                        //
                        // #106120152 8574794F calibrates it: stated XRP
                        // 14432011 equals the pre-fee liquid EXACTLY
                        // (15832011 − reserve(2) 1400000), so preclaim
                        // passes by zero margin; post-fee liquid 14431999
                        // is 12 drops short of the actual and mainnet
                        // refuses. Five specimens, all this shape.
                        if !deposit_leg_funded(
                            sandbox,
                            &tx.account,
                            &leg,
                            amt,
                            &amm_acct,
                            &lp_leg.cur,
                        ) {
                            return TxResult::UnfundedAmm;
                        }
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
        // Finding 36 (#106644320 749D3E45): a deposit into an EMPTY pool
        // (every LP previously withdrew) revives it — rippled runs
        // initializeFeeAuctionVote so the reviving depositor takes the
        // auction slot and the vote, with the fee from the deposit's own
        // TradingFee field (AMMDeposit.cpp:475-478).
        let was_empty = ox::json_at(sandbox, &amm_key)
            .and_then(|o| {
                o["LPTokenBalance"]["value"]
                    .as_str()
                    .map(|s| keylet::amount_mant_exp(&serde_json::Value::String(s.to_string())))
            })
            .flatten()
            .map(|m| m.0 == 0)
            .unwrap_or(false);
        if std::env::var("DX_AMMDEP").is_ok() {
            let cur = ox::json_at(sandbox, &amm_key)
                .and_then(|o| o["LPTokenBalance"]["value"].as_str().map(str::to_string));
            eprintln!("DX_AMMDEP minted={minted:?} lp_pre={cur:?} sized={sized:?} single_adj={single_adj:?}");
        }
        ox::move_leg(sandbox, &amm_acct, &tx.account, &lp_leg, minted);
        bump_lp_balance(sandbox, &amm_key, minted, true);
        if std::env::var("DX_AMMDEP").is_ok() {
            let post = ox::json_at(sandbox, &amm_key)
                .and_then(|o| o["LPTokenBalance"]["value"].as_str().map(str::to_string));
            eprintln!("DX_AMMDEP lp_post={post:?}");
        }
        if was_empty {
            let tfee = tx
                .fields
                .get("TradingFee")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u16;
            initialize_fee_auction_vote(sandbox, &amm_key, &tx.account, tfee, &lp_leg);
        }
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
    // …and ONLY when the LP's side is in default state. A plain withdraw
    // burns via redeemIOU — the ORDINARY line machinery — and trustDelete's
    // conditions apply: the LP-side limit must be zero and no quality
    // settings present, else the zeroed line SURVIVES as Modified.
    // (`deleteAMMTrustLine`, which deletes unconditionally, runs only when
    // the AMM itself is deleted.) #106024169 430F1F12: rKHB6QGL set a
    // 10000000 limit on its LPToken line; mainnet zeroes and keeps it —
    // we deleted it plus the whole directory cascade, 8v5.
    let (my_limit, my_q_in, my_q_out) = if who < amm_acct {
        ("LowLimit", "LowQualityIn", "LowQualityOut")
    } else {
        ("HighLimit", "HighQualityIn", "HighQualityOut")
    };
    let limit_zero = lp_line[my_limit]["value"].as_str().map(|v| v == "0").unwrap_or(true);
    let quality_zero = lp_line.get(my_q_in).is_none() && lp_line.get(my_q_out).is_none();
    if !(limit_zero && quality_zero) {
        // Zero the balance in place; the line stays.
        let mut line = lp_line.clone();
        if let Some(b) = line.get_mut("Balance") {
            b["value"] = serde_json::Value::String("0".to_string());
        }
        sandbox.write(lp_key, serde_json::to_vec(&line).unwrap_or_default());
        return;
    }
    let node = |field: &str| {
        lp_line.get(field).and_then(|v| v.as_str()).and_then(|s| u64::from_str_radix(s, 16).ok())
    };
    let (w_node, a_node) = if who < amm_acct { ("LowNode", "HighNode") } else { ("HighNode", "LowNode") };
    sandbox.delete(lp_key);
    crate::ledger::directory::owner_dir_remove(sandbox, who, &lp_key, node(w_node), false);
    crate::ledger::directory::owner_dir_remove(sandbox, amm_acct, &lp_key, node(a_node), false);
    // ONLY the holder's count falls. The pool is the LP token's ISSUER and
    // never pays reserve on these lines — rippled's deleteAMMTrustLine
    // adjusts the NON-AMM side alone (AMMUtils), the trustCreate rule
    // ("charge the CREATOR only") seen from the teardown end. #106455156
    // 713DEF80 (full LP redemption): mainnet threads the pool root
    // untouched at OwnerCount 1 and takes the holder 80 → 79; decrementing
    // the pool here wrote its root to 0 — the replay's ledger-end diff was
    // the only instrument that saw it (OwnerCount has no valcheck field).
    crate::tx::offer::owner_count_add(sandbox, who, -1);
}

/// `deleteAMMAccount` — the last LP's withdrawal leaves LPTokenBalance at
/// zero and the whole AMM is dismantled: every pool asset trust line is
/// deleted UNCONDITIONALLY (`deleteAMMTrustLine` — reserve-side OwnerCounts
/// adjusted per the line's lsfLow/HighReserve flags, rippled trustDelete's
/// rule), the AMM object leaves the pool's owner directory, and the pool
/// AccountRoot itself is erased. An XRP pool side has no line to remove; the
/// pool's XRP disposition on deletion is unmodeled (no specimen — 419A5D2C
/// is IOU/IOU).
fn delete_amm(
    sandbox: &mut Sandbox,
    amm_key: &xrpl_core::types::Hash256,
    amm_acct: &[u8; 20],
    tx: &TxFields,
) {
    use crate::tx::offer as ox;
    let leg_of_asset = |v: Option<&serde_json::Value>| -> Option<ox::Leg> {
        let v = v?;
        if v.get("currency").and_then(|c| c.as_str()) == Some("XRP") {
            return Some(ox::Leg { xrp: true, cur: [0u8; 20], issuer: [0u8; 20] });
        }
        let mut amt = v.clone();
        amt["value"] = serde_json::json!("0");
        ox::leg_of(&amt)
    };
    for f in ["Asset", "Asset2"] {
        let Some(leg) = leg_of_asset(tx.fields.get(f)) else { continue };
        if leg.xrp {
            continue;
        }
        let lkey = keylet::ripple_state_key(amm_acct, &leg.issuer, &leg.cur);
        let Some(line) = ox::json_at(sandbox, &lkey) else { continue };
        let flags = line["Flags"].as_u64().unwrap_or(0);
        let node = |field: &str| {
            line.get(field).and_then(|v| v.as_str()).and_then(|s| u64::from_str_radix(s, 16).ok())
        };
        let (low, high) = if amm_acct < &leg.issuer {
            (amm_acct, &leg.issuer)
        } else {
            (&leg.issuer, amm_acct)
        };
        sandbox.delete(lkey);
        crate::ledger::directory::owner_dir_remove(sandbox, low, &lkey, node("LowNode"), false);
        crate::ledger::directory::owner_dir_remove(sandbox, high, &lkey, node("HighNode"), false);
        if flags & 0x0001_0000 != 0 {
            crate::tx::offer::owner_count_add(sandbox, low, -1); // lsfLowReserve
        }
        if flags & 0x0002_0000 != 0 {
            crate::tx::offer::owner_count_add(sandbox, high, -1); // lsfHighReserve
        }
    }
    let amm_hint = ox::json_at(sandbox, amm_key)
        .and_then(|o| o.get("OwnerNode").and_then(|v| v.as_str()).map(str::to_string))
        .and_then(|s| u64::from_str_radix(&s, 16).ok());
    sandbox.delete(*amm_key);
    crate::ledger::directory::owner_dir_remove(sandbox, amm_acct, amm_key, amm_hint, false);
    sandbox.delete(keylet::account_root_key(amm_acct));
}

/// rippled `initializeFeeAuctionVote` (AMMHelpers.cpp:791-845), run when a
/// deposit REVIVES an empty pool (AMMDeposit.cpp:475-478) and on AMMCreate:
/// the depositor takes the auction slot for free — Expiration = parent close
/// + 24h, Price = zero LP tokens, stale AuthAccounts dropped
/// (fixCleanup3_2_0) — the vote slots reset to the depositor alone at full
/// weight, and the pool TradingFee becomes the caller-supplied fee (omitted
/// when zero, like every SoeDefault).
fn initialize_fee_auction_vote(
    sandbox: &mut Sandbox,
    amm_key: &xrpl_core::types::Hash256,
    account: &[u8; 20],
    tfee: u16,
    lp_leg: &crate::tx::offer::Leg,
) {
    use crate::tx::offer as ox;
    let Some(mut amm) = ox::json_at(sandbox, amm_key) else { return };
    let acct_hex = hex::encode(account);
    let mut ve = serde_json::json!({
        "Account": acct_hex,
        "VoteWeight": 100_000u64,
    });
    if tfee != 0 {
        ve["TradingFee"] = serde_json::json!(tfee);
    }
    amm["VoteSlots"] = serde_json::json!([{ "VoteEntry": ve }]);
    if tfee == 0 {
        if let Some(o) = amm.as_object_mut() {
            o.remove("TradingFee");
        }
    } else {
        amm["TradingFee"] = serde_json::json!(tfee);
    }
    let exp = sandbox.base().header.close_time as u64 + 86_400;
    let mut slot = serde_json::json!({
        "Account": hex::encode(account),
        "Expiration": exp,
        "Price": {
            "currency": hex::encode_upper(lp_leg.cur),
            "issuer": hex::encode(lp_leg.issuer),
            "value": "0",
        },
    });
    let dfee = tfee / 10;
    if dfee != 0 {
        slot["DiscountedFee"] = serde_json::json!(dfee);
    }
    // Whole-object replacement drops any stale AuthAccounts, matching the
    // fixCleanup3_2_0 makeFieldAbsent.
    amm["AuctionSlot"] = slot;
    ox::put_json(sandbox, *amm_key, &amm);
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
    withdraw_all: bool,
) -> bool {
    payout_proportional_to(sandbox, tx, amm_acct, &tx.account, tokens, total_lp, withdraw_all).is_some()
}

/// Same proportional two-asset payout, but to an arbitrary beneficiary
/// (AMMClawback withdraws FOR THE HOLDER), returning the paid shares so the
/// clawback can move them on to the issuer.
fn payout_proportional_to(
    sandbox: &mut Sandbox,
    tx: &TxFields,
    amm_acct: &[u8; 20],
    who: &[u8; 20],
    tokens: (u128, i32),
    total_lp: (u128, i32),
    withdraw_all: bool,
) -> Option<Vec<(crate::tx::offer::Leg, (u128, i32))>> {
    use crate::tx::offer as ox;
    let asset_leg = |v: &serde_json::Value| -> Option<ox::Leg> {
        if v.get("currency").and_then(|c| c.as_str()) == Some("XRP") {
            return Some(ox::Leg { xrp: true, cur: [0u8; 20], issuer: [0u8; 20] });
        }
        let mut amt = v.clone();
        amt["value"] = serde_json::json!("0");
        ox::leg_of(&amt)
    };
    // BOTH SIDES ARE COMPUTED BEFORE EITHER MOVES. rippled refuses an equal
    // withdrawal outright when a side rounds away:
    //     // ... the requested amount of LP tokens is likely too small and
    //     // results in one-sided pool withdrawal due to round off. Fail so
    //     // the user withdraws more tokens.
    //     if (amountWithdraw == beast::kZero || amount2Withdraw == beast::kZero)
    //         return {tecAMM_FAILED, ...};                (AMMWithdraw.cpp:840-845)
    // Skipping the zero side and paying the other is exactly the one-sided
    // withdrawal that check exists to prevent, so the sides must be known
    // before anything is written.
    //
    // #106014913 5788637E (tfWithdrawAll): 0.000001 LP against a supply of
    // 1471789206.03 is a fraction of 6.794e-16, so the XRP side of a
    // 34347264193-drop pool comes to 0.000023 drops — ZERO — while the XRG side
    // is 4.781e-08. Mainnet claims the fee with tecAMM_FAILED; we paid out the
    // XRG side alone in 8 mutations.
    let mut shares: Vec<(ox::Leg, (u128, i32))> = Vec::new();
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
        // Findings 31+32 (#106629211 7ED17A0F, ONE tfWithdrawAll paying an
        // ARMY/DROP two-IOU pool): under fixAMMv1_3 the FRACTION rounds UP
        // and both asset products round DOWN — the unique pair satisfying
        // BOTH sides of the same withdrawal (DROP wanted …2140 where
        // frac-nearest×down lands …2139; ARMY wanted …065 where up lands
        // …066). The dust calibrator #106014913 5788637E stays correct: a
        // ceiled dust fraction times the pool still FLOORS to zero and the
        // one-sided check fails with tecAMM_FAILED. PRE-amendment the legacy
        // nearest fraction stands — the census's #105880685 (one ULP HIGH
        // under an unconditional ceil) is the pre-era calibrator. rippled
        // splits on rules().enabled(fixAMMv1_3) (AMMHelpers getRoundedAsset).
        // F54 (#106692584 B27127D6, tfWithdrawAll Jocker/XRP): the mode
        // fork was a PHANTOM. #106629211's "ceil uniquely required" was
        // under-determined — with the withdrawer-line add and the pool-line
        // subtract both quantizing at their own exponents, frac-…2139 and
        // frac-…2140 land BYTE-IDENTICAL lines on BOTH sides, so that
        // specimen never distinguished ceil from nearest. The fresh
        // specimen does: ceil hands the withdrawer …615 where mainnet's
        // …613 demands the nearest fraction, and the nearest fraction also
        // satisfies #105880685 (partial), #106629211 (indifferent) and the
        // dust calibrator #106014913 (a ceiled OR nearest dust fraction
        // still floors the XRP side to zero → tecAMM_FAILED). One rule,
        // all modes: frac = divide(tokensAdj, lptAMMBalance) under Number
        // nearest, products DOWNWARD (getRoundedAsset, IsDeposit::No).
        // F61: this `frac` is rippled's STAmount `divide(tokensAdj,
        // lptAMMBalance, noIssue())` (AMMWithdraw.cpp:797), which carries the
        // legacy +5 into Number's nearest canonicalisation — not the `Number`
        // division of the two-asset paths. See `st_divide_legacy`.
        let _ = withdraw_all;
        let frac = ox::st_divide_legacy(tokens, total_lp);
        let share = mul_directed(pool, frac, false, leg.xrp);
        if std::env::var("DX_AMMWD").is_ok() {
            eprintln!(
                "DX_AMMWD {f} pool={pool:?} tokens={tokens:?} total_lp={total_lp:?} frac={frac:?} share={share:?}"
            );
        }
        shares.push((leg, share));
    }
    // A side that rounded to zero fails the whole withdrawal — nothing written.
    if shares.iter().any(|(_, sh)| sh.0 == 0) {
        return None;
    }
    for (leg, share) in &shares {
        ox::move_leg(sandbox, amm_acct, who, leg, *share);
    }
    Some(shares)
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
        // Taken up front so a refusal discovered mid-payout leaves nothing
        // behind: tecAMM_FAILED on a rounded-away side is decided after the
        // one-asset branch may already have moved value.
        let snap = sandbox.snapshot();
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
        // Then the LP position, in rippled's order and with rippled's split
        // (AMMWithdraw.cpp:272-288):
        //     if (lpTokens <= beast::kZero)              return tecAMM_BALANCE;
        //     if (*lpTokensWithdraw > lpTokens)          return tecAMM_INVALID_TOKENS;
        // Holding NONE is a balance failure; holding SOME BUT TOO FEW is an
        // invalid-tokens one, and the two are different result codes.
        //
        // #106308202 4D96E855: tfOneAssetLPToken burning 100000 LP against a
        // position of 40000, so mainnet answers tecAMM_INVALID_TOKENS. We had
        // no LP check at all and answered tecAMM_BALANCE — from the POOL check
        // above, because the pool's HONEY line was unhydrated and read as zero,
        // so `Amount 1 > 0` tripped first. Right code by luck, wrong reason.
        let lp_held = crate::tx::amm_swap::holds(sandbox, &tx.account, &lp_leg);
        if ox::me_is_zero(lp_held) {
            return TxResult::AmmBalance;
        }
        // fixAMMv1_1: the sole LP's line is the LPTokenBalance of record.
        if let Err(e) =
            verify_and_adjust_lp_token_balance(sandbox, &amm_key, &amm_acct, &tx.account, &lp_leg, lp_held)
        {
            sandbox.restore_snapshot(snap);
            return e;
        }
        if let Some(want) = tx.fields.get("LPTokenIn").and_then(keylet::amount_mant_exp) {
            if ox::me_cmp(want, lp_held).is_gt() {
                return TxResult::AmmInvalidTokens;
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
                        // F66: the slot holder withdraws at the DISCOUNTED fee (getTradingFee).
                        let tfee = ox::json_at(sandbox, &amm_key)
                            .map(|o| crate::tx::amm_swap::effective_trading_fee(sandbox, &o, &tx.account))
                            .unwrap_or(0);
                        if let Some(out) =
                            crate::tx::amm_swap::amm_asset_out(bal, total_lp, lp_bal, tfee, leg.xrp)
                        {
                            // singleWithdrawTokens (AMMWithdraw.cpp:1004-1016): the
                            // LP's whole position must buy at least `Amount`
                            // (`amount == 0 || amountWithdraw >= amount`), else
                            // tecAMM_FAILED. #106699133 6B6670FD (finding 68):
                            // 40.5M of 140.5M LP tokens as XRP alone against a
                            // 345.906940 XRP floor — mainnet failed it five ledgers
                            // running; we paid out.
                            let floor = keylet::amount_mant_exp(v).unwrap_or((0, 0));
                            if floor.0 > 0 && ox::me_cmp(out, floor).is_lt() {
                                sandbox.restore_snapshot(snap);
                                return TxResult::AmmFailed;
                            }
                            if out.0 > 0 {
                                ox::move_leg(sandbox, &amm_acct, &tx.account, &leg, out);
                            }
                        }
                    }
                }
            } else {
                // Both assets out, proportional to the redeemed LPToken share.
                if !payout_proportional(sandbox, tx, &amm_acct, lp_bal, total_lp, true) {
                    sandbox.restore_snapshot(snap);
                    return TxResult::AmmFailed;
                }
            }
            tear_down_lp_line(sandbox, &tx.account, &amm_acct, lp_key, &lp_line);
            bump_lp_balance(sandbox, &amm_key, lp_bal, false);
            // LAST LP OUT — the AMM dies with the withdrawal. rippled runs
            // deleteAMMAccountIfEmpty once LPTokenBalance hits zero:
            // `deleteAMMTrustLine` removes every pool line UNCONDITIONALLY
            // (the survive-at-zero rule above is for ORDINARY withdrawals),
            // then the AMM object and the pool ACCOUNT itself go. #106430239
            // 419A5D2C (tfWithdrawAll by the only LP): mainnet deletes the two
            // pool asset lines and the AMM object where we left them
            // zeroed-Modified — 12v12 with three ops flipped 1→2.
            let lpt_zero = ox::json_at(sandbox, &amm_key)
                .and_then(|o| o["LPTokenBalance"]["value"].as_str().map(|v| v == "0"))
                .unwrap_or(false);
            if lpt_zero {
                delete_amm(sandbox, &amm_key, &amm_acct, tx);
            }
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
                    if !payout_proportional(sandbox, tx, &amm_acct, tokens, total_lp, false) {
                        sandbox.restore_snapshot(snap);
                        return TxResult::AmmFailed;
                    }
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
                // F66: the slot holder withdraws at the DISCOUNTED fee (getTradingFee).
                let tfee = ox::json_at(sandbox, &amm_key)
                    .map(|o| crate::tx::amm_swap::effective_trading_fee(sandbox, &o, &tx.account))
                    .unwrap_or(0);
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
        // Finding 102 (#106720743 E0D5A4947D22): when NEITHER ordering fits
        // both maxima, rippled's equalWithdrawLimit returns tecAMM_FAILED
        // (AMMWithdraw.cpp:949, fixAMMv1_3) — the withdrawer asked for
        // exactly 98 % of both sides and each derived side overshot its cap
        // by a rounding hair. We fell through to the verbatim move and paid
        // the full amounts.
        if wd_flags & TF_TWO_ASSET_W != 0 && wd_sized.is_none() {
            return TxResult::AmmFailed;
        }
        // tfOneAssetLPToken: LPTokenIn is what is burned and the AMOUNT is
        // DERIVED from it via `ammAssetOut` (`Amount` is only a minimum) —
        // rippled `singleWithdrawTokens` (AMMWithdraw.cpp:1020).
        // F68: `Amount` is a FLOOR in this mode too (singleWithdrawTokens).
        let mut one_asset_floor_unmet = false;
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
                // F66: the slot holder withdraws at the DISCOUNTED fee (getTradingFee).
                let tfee = ox::json_at(sandbox, &amm_key)
                    .map(|o| crate::tx::amm_swap::effective_trading_fee(sandbox, &o, &tx.account))
                    .unwrap_or(0);
                let tokens = crate::tx::amm_swap::adjust_lp_tokens(lpt, lp_in, false);
                let out = crate::tx::amm_swap::amm_asset_out(
                    crate::tx::amm_swap::holds(sandbox, &amm_acct, &leg),
                    lpt,
                    tokens,
                    tfee,
                    leg.xrp,
                )?;
                let floor = keylet::amount_mant_exp(v).unwrap_or((0, 0));
                if floor.0 > 0 && ox::me_cmp(out, floor).is_lt() {
                    one_asset_floor_unmet = true;
                    return None;
                }
                Some((out, tokens))
            })()
        } else {
            None
        };
        if one_asset_floor_unmet {
            sandbox.restore_snapshot(snap);
            return TxResult::AmmFailed;
        }

        // rippled's withdraw() core judges the ACTUAL burn against the LP's
        // position for EVERY mode (AMMWithdraw.cpp:515-520): tokens that
        // exceed the holding — or adjust away to nothing — are
        // tecAMM_INVALID_TOKENS. The explicit-LPTokenIn form was already
        // judged above; the amount-driven modes DERIVE their burn and must
        // face the same judge. #106363879 55335BB9: tfSingleAsset taking
        // 538.5 XRP derives a burn of 396924.53 LP against 369936.01 held —
        // rippled's trace prints exactly that triple ("failed to withdraw,
        // invalid LP tokens"). #106290427 CCD6E831 is the tfTwoAsset twin.
        let derived_burn =
            wd_sized.map(|(_, _, t)| t).or(single_asset_burn.map(|(_, t)| t));
        if let Some(t) = derived_burn {
            if t.0 == 0 || ox::me_cmp(t, lp_held).is_gt() {
                sandbox.restore_snapshot(snap);
                return TxResult::AmmInvalidTokens;
            }
        }
        // tfOneAssetLPToken: the Amount is a MINIMUM — a burn whose worth
        // comes up short of it is tecAMM_FAILED (singleWithdrawTokens,
        // AMMWithdraw.cpp:1020ff, judged BEFORE the withdraw() core).
        // #106049416 3E802DB7: 3M LP buys less XDP than the 2452869.94
        // floor, so mainnet takes the fee and stops.
        if let Some((out, _)) = one_asset {
            if let Some(minimum) = tx.fields.get("Amount").and_then(keylet::amount_mant_exp) {
                if ox::me_cmp(out, minimum).is_lt() {
                    sandbox.restore_snapshot(snap);
                    return TxResult::AmmFailed;
                }
            }
        }

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
        // AMMVote::preclaim (AMMVote.cpp:59-77), in rippled's order: an
        // EMPTY pool (LPTokenBalance zero) refuses with tecAMM_EMPTY, then a
        // voter holding NO LP tokens with tecAMM_INVALID_TOKENS — "AMM Vote:
        // account is not LP." Judged by `ammLPHolds`, where a missing line
        // holds nothing; all four specimens are `ammLPHolds: no SLE`
        // (#106072851 2B47FAF6 and siblings — bots voting on pools they
        // never joined). A frozen LP line also reads as zero there, but an
        // AMM account never sets freeze flags, so only the missing/zero
        // cases are modeled.
        //
        // The AMM OBJECT gates the whole check: an unhydrated pool skips it,
        // never condemns — rippled's missing-AMM verdict is terNO_AMM, a
        // retry code no validated ledger carries. The probe hydrates the
        // pool and the voter's LP line for AMMVote (load_amm_prestate), the
        // AMMDeposit lesson: the check and its hydration land together.
        if let Some((amm_key, _amm_acct, lp_leg)) = amm_ctx(tx, sandbox) {
            if let Some(obj) = crate::tx::offer::json_at(sandbox, &amm_key) {
                if matches!(obj["LPTokenBalance"]["value"].as_str(), Some("0") | Some("-0")) {
                    return TxResult::AmmEmpty;
                }
            }
            let lkey = keylet::ripple_state_key(&tx.account, &lp_leg.issuer, &lp_leg.cur);
            let holds = crate::tx::offer::json_at(sandbox, &lkey)
                .map(|l| {
                    let (neg, bal) = crate::tx::offer::signed_value(&l["Balance"]);
                    // Balance from the LOW account's perspective.
                    let party_holds = if tx.account < lp_leg.issuer { !neg } else { neg };
                    party_holds && bal.0 > 0
                })
                .unwrap_or(false);
            if !holds {
                return TxResult::AmmInvalidTokens;
            }
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        // rippled AMMVote::applyVote. Entries are WRAPPED ({"VoteEntry": {…}})
        // and carry VoteWeight = lpTokens × 100000 / lptAMMBalance; EVERY
        // retained entry's weight is recomputed from its holder's CURRENT LP
        // balance (zero-weight entries drop); with 8 slots the lowest-weight
        // entry is evicted only when the new vote outweighs it (else
        // tecAMM_FAILED); the pool's TradingFee becomes the weight-averaged
        // fee of the surviving entries, Number-nearest. The old stub wrote
        // FLAT weightless entries — the codec rightly refused to encode them
        // and fresh-window replays cascaded from the dropped write. First
        // specimen: #106589344 D7B6AED8 (XRP/NCR).
        let Some((amm_key, amm_acct, lp_leg)) = amm_ctx(tx, sandbox) else {
            return TxResult::Success;
        };
        let Some(mut amm) = crate::tx::offer::json_at(sandbox, &amm_key) else {
            return TxResult::Success;
        };
        let Some(lpt_amm) = amm
            .get("LPTokenBalance")
            .and_then(keylet::amount_mant_exp)
        else {
            return TxResult::Success;
        };
        if lpt_amm.0 == 0 {
            return TxResult::AmmEmpty;
        }
        // Holder-side LP balance as (mant, exp).
        let lp_of = |sandbox: &Sandbox, acct: &[u8; 20]| -> crate::tx::offer::Me {
            let lkey = keylet::ripple_state_key(acct, &lp_leg.issuer, &lp_leg.cur);
            match crate::tx::offer::json_at(sandbox, &lkey) {
                Some(l) => {
                    let (neg, bal) = crate::tx::offer::signed_value(&l["Balance"]);
                    let holds = if *acct < lp_leg.issuer { !neg } else { neg };
                    if holds { bal } else { (0, 0) }
                }
                None => (0, 0),
            }
        };
        // VoteWeight = lp × 100000 / lptAMM, exact i128, Number half-even.
        let weight_of = |lp: crate::tx::offer::Me| -> u64 {
            if lp.0 == 0 {
                return 0;
            }
            let (mut num, mut den) = (lp.0 as i128 * 100_000, lpt_amm.0 as i128);
            let mut d = lp.1 - lpt_amm.1;
            while d > 0 {
                num *= 10;
                d -= 1;
            }
            while d < 0 {
                den *= 10;
                d += 1;
            }
            let q = num / den;
            let r = num % den;
            let q = if 2 * r > den || (2 * r == den && q % 2 == 1) { q + 1 } else { q };
            q as u64
        };
        // Finding 49 (#106678645, XPM/XRP AMMVote): the loop below is
        // applyVote ported line-for-line. The old form dropped every entry
        // whose ROUNDED weight was zero (rippled drops only holders whose LP
        // BALANCE is zero — a dust holder is retained at VoteWeight 0),
        // replaced-and-appended the voter (rippled updates IN PLACE, keeping
        // slot order), averaged the fee over rounded weights (rippled sums
        // fee×lpTokens over Σ lpTokens in Number arithmetic), evicted by
        // weight (rippled compares TOKENS, ties by lower fee then lower
        // AccountID), and refused with tecAMM_FAILED when a full slate could
        // not be outweighed (rippled falls through and simply refreshes the
        // slots). Live: ours collapsed an 8-slot slate to the voter alone
        // and wrote TradingFee 1000 where mainnet holds 761.
        use crate::tx::amm_swap::{n_add, n_div, n_sub, Rnd};
        type Me = crate::tx::offer::Me;
        let voter_hex = hex::encode(tx.account);
        let new_fee = tx.fields.get("TradingFee").and_then(|f| f.as_u64()).unwrap_or(0);
        let voter_lp = lp_of(sandbox, &tx.account);

        let existing: Vec<serde_json::Value> = amm
            .get("VoteSlots")
            .and_then(|s| s.as_array())
            .cloned()
            .unwrap_or_default();
        let mut updated: Vec<(String, u64, u64)> = Vec::new(); // (acct_hex, fee, weight)
        let mut num: Me = (0, 0);
        let mut den: Me = (0, 0);
        let mut found = false;
        // Least entry: (position, lpTokens, fee, account bytes) — rippled's
        // comparator is lp <, then fee <, then AccountID <.
        let mut min: Option<(usize, Me, u64, [u8; 20])> = None;
        for e in &existing {
            let Some(ve) = e.get("VoteEntry") else { continue };
            let Some(acct) = ve.get("Account").and_then(|a| a.as_str()) else { continue };
            let Some(aid) = crate::tx::offer::decode20(acct) else { continue };
            let is_voter = acct.eq_ignore_ascii_case(&voter_hex);
            let lp = if is_voter { voter_lp } else { lp_of(sandbox, &aid) };
            if lp.0 == 0 {
                continue; // "account is not LP" — the only drop
            }
            let fee = if is_voter {
                found = true;
                new_fee
            } else {
                ve.get("TradingFee").and_then(|f| f.as_u64()).unwrap_or(0)
            };
            num = n_add(num, (lp.0 * fee as u128, lp.1), Rnd::Near);
            den = n_add(den, lp, Rnd::Near);
            let better = match &min {
                None => true,
                Some((_, mlp, mfee, maid)) => match crate::tx::offer::me_cmp(lp, *mlp) {
                    std::cmp::Ordering::Less => true,
                    std::cmp::Ordering::Equal => fee < *mfee || (fee == *mfee && aid < *maid),
                    std::cmp::Ordering::Greater => false,
                },
            };
            if better {
                min = Some((updated.len(), lp, fee, aid));
            }
            updated.push((acct.to_string(), fee, weight_of(lp)));
        }
        if !found {
            let new_weight = weight_of(voter_lp);
            if updated.len() < 8 {
                num = n_add(num, (voter_lp.0 * new_fee as u128, voter_lp.1), Rnd::Near);
                den = n_add(den, voter_lp, Rnd::Near);
                updated.push((voter_hex, new_fee, new_weight));
            } else if let Some((mi, mlp, mfee, _)) = min {
                let outweighs = match crate::tx::offer::me_cmp(voter_lp, mlp) {
                    std::cmp::Ordering::Greater => true,
                    std::cmp::Ordering::Equal => new_fee > mfee,
                    std::cmp::Ordering::Less => false,
                };
                if outweighs {
                    num = n_sub(num, (mlp.0 * mfee as u128, mlp.1), Rnd::Near);
                    den = n_sub(den, mlp, Rnd::Near);
                    num = n_add(num, (voter_lp.0 * new_fee as u128, voter_lp.1), Rnd::Near);
                    den = n_add(den, voter_lp, Rnd::Near);
                    updated[mi] = (voter_hex, new_fee, new_weight);
                }
                // else: full slate, not outweighed — the vote still succeeds
                // and merely refreshes the surviving slots ("Update anyway").
            }
        }
        // TradingFee = Σ(fee·lp) / Σlp, Number arithmetic, nearest to integer.
        let fee_avg: u64 = if den.0 == 0 {
            0
        } else {
            // F71 — `Number::operator rep()` (Number.cpp:845-875) rounds on the
            // EXACT discarded fraction: above half rounds up, exactly half rounds
            // to even. We floored q×10 and read its last digit as the tie test, so
            // every fraction in [0.5, 0.6) looked like an exact tie and rounded to
            // even. #106698734 C1FE51B5 tallies 740.58… — mainnet 741, ours 740.
            let (m, e) = n_div(num, den, Rnd::Near);
            if e >= 0 {
                m.saturating_mul(10u128.saturating_pow(e as u32)) as u64
            } else if -e > 38 {
                0
            } else {
                let div = 10u128.pow((-e) as u32);
                let (ip, rem) = (m / div, m % div);
                let half = div / 2;
                (if rem > half || (rem == half && ip % 2 == 1) { ip + 1 } else { ip }) as u64
            }
        };

        amm["VoteSlots"] = serde_json::Value::Array(
            updated
                .iter()
                .map(|(a, f, w)| {
                    // Finding 33 (#106629215 AC9BA657): sfTradingFee is
                    // SoeDefault in VoteEntry — a ZERO fee is OMITTED from the
                    // serialization (r4eqGMgL votes fee 0; truth's object is
                    // 3 bytes shorter than ours per omitted entry). Same rule
                    // for the AMM root's own TradingFee below.
                    if *f == 0 {
                        serde_json::json!({"VoteEntry": {"Account": a, "VoteWeight": w}})
                    } else {
                        serde_json::json!({"VoteEntry": {"Account": a, "TradingFee": f, "VoteWeight": w}})
                    }
                })
                .collect(),
        );
        if fee_avg == 0 {
            if let Some(o) = amm.as_object_mut() {
                o.remove("TradingFee");
            }
        } else {
            amm["TradingFee"] = serde_json::json!(fee_avg);
        }
        // Finding 33b (#106629215 AC9BA657): the AuctionSlot's DiscountedFee
        // follows the vote — fee/10, set when nonzero, REMOVED when zero
        // (AMMVote.cpp:208-218; truth 185/10=18 where we left the stale 10).
        if amm.get("AuctionSlot").is_some() {
            let df = fee_avg / 10;
            if df == 0 {
                if let Some(slot) = amm["AuctionSlot"].as_object_mut() {
                    slot.remove("DiscountedFee");
                }
            } else {
                amm["AuctionSlot"]["DiscountedFee"] = serde_json::json!(df);
            }
        }
        let _ = amm_acct;
        crate::tx::offer::put_json(sandbox, amm_key, &amm);
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

    // rippled AMMBid::preclaim, tec paths only (tem/ter shapes never reach a
    // validated ledger): empty pool → tecAMM_EMPTY; bidder holding no LP →
    // tecAMM_INVALID_TOKENS; BidMin/BidMax above the bidder's LP or at/past
    // the pool's whole balance, or min > max → tecAMM_INVALID_TOKENS.
    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        use crate::tx::offer as ox;
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) { return TxResult::NoAccount; }
        let Some((amm_key, _amm_acct, lp_leg)) = amm_ctx(tx, sandbox) else {
            return TxResult::Success; // unhydrated pool: never condemn (F49 rule)
        };
        let Some(amm) = ox::json_at(sandbox, &amm_key) else {
            return TxResult::Success;
        };
        let Some(lpt_amm) = amm.get("LPTokenBalance").and_then(keylet::amount_mant_exp) else {
            return TxResult::Success;
        };
        if lpt_amm.0 == 0 {
            return TxResult::AmmEmpty;
        }
        let lkey = keylet::ripple_state_key(&tx.account, &lp_leg.issuer, &lp_leg.cur);
        let lp_tokens = match ox::json_at(sandbox, &lkey) {
            Some(l) => {
                let (neg, bal) = ox::signed_value(&l["Balance"]);
                let holds = if tx.account < lp_leg.issuer { !neg } else { neg };
                if holds { bal } else { (0, 0) }
            }
            None => (0, 0),
        };
        if lp_tokens.0 == 0 {
            return TxResult::AmmInvalidTokens;
        }
        let bid_of = |f: &str| -> Option<crate::tx::offer::Me> {
            tx.fields.get(f).map(|v| ox::signed_value(v).1)
        };
        let (bid_min, bid_max) = (bid_of("BidMin"), bid_of("BidMax"));
        for b in [bid_min, bid_max].into_iter().flatten() {
            if ox::me_cmp(b, lp_tokens).is_gt() || !ox::me_cmp(b, lpt_amm).is_lt() {
                return TxResult::AmmInvalidTokens;
            }
        }
        if let (Some(mn), Some(mx)) = (bid_min, bid_max) {
            if ox::me_cmp(mn, mx).is_gt() {
                return TxResult::AmmInvalidTokens;
            }
        }
        TxResult::Success
    }

    // rippled AMMBid.cpp applyBid, line for line. The old stub stamped a
    // fake slot ({Account, Expiration: 0, Price: BidMin}) and always
    // succeeded — first live conviction #106692271 DD579576F908 (XDC/XRP,
    // XPmarket bid): computedPrice above BidMax must be tecAMM_FAILED.
    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        use crate::tx::amm_swap::{n_add, n_div, n_mul, n_pow, n_sub, Rnd};
        use crate::tx::offer as ox;
        let Some((amm_key, _amm_acct, lp_leg)) = amm_ctx(tx, sandbox) else {
            return TxResult::Success;
        };
        let Some(mut amm) = ox::json_at(sandbox, &amm_key) else {
            return TxResult::Success;
        };
        let Some(lpt_amm) = amm.get("LPTokenBalance").and_then(keylet::amount_mant_exp) else {
            return TxResult::Success;
        };
        // Bidder's LP holding, read ONCE before any movement (the payPrice
        // ceiling judges against it).
        let lp_tokens = {
            let lkey = keylet::ripple_state_key(&tx.account, &lp_leg.issuer, &lp_leg.cur);
            match ox::json_at(sandbox, &lkey) {
                Some(l) => {
                    let (neg, bal) = ox::signed_value(&l["Balance"]);
                    let holds = if tx.account < lp_leg.issuer { !neg } else { neg };
                    if holds { bal } else { (0, 0) }
                }
                None => (0, 0),
            }
        };
        // `current` = the building ledger's parentCloseTime (the F46
        // Expiration convention: base header close_time).
        let current = sandbox.base().header.close_time as u64;
        let tfee = amm.get("TradingFee").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
        let discounted_fee = tfee / 10;
        // tradingFee = Number{tfee}/100000; minSlotPrice = lptAMM × fee / 25.
        let trading_fee = n_div((tfee as u128, 0), (100_000, 0), Rnd::Near);
        let min_slot_price = n_div(n_mul(lpt_amm, trading_fee, Rnd::Near), (25, 0), Rnd::Near);
        let slot = amm.get("AuctionSlot").cloned().unwrap_or(serde_json::json!({}));
        // ammAuctionTimeSlot: 0-19 while inside the 24h window, else None.
        let time_slot: Option<u64> = slot
            .get("Expiration")
            .and_then(|v| v.as_u64())
            .filter(|e| *e >= 86_400)
            .and_then(|e| {
                let start = e - 86_400;
                current.checked_sub(start).filter(|d| *d < 86_400).map(|d| d / 4_320)
            });
        let owner: Option<[u8; 20]> = slot
            .get("Account")
            .and_then(|v| v.as_str())
            .and_then(|h| hex::decode(h).ok())
            .and_then(|b| <[u8; 20]>::try_from(b).ok());
        // Valid range is 0-19 but the tailing slot (19) pays MinSlotPrice
        // and doesn't refund, so the check is < 19.
        let valid_owner = owner.is_some_and(|o| {
            time_slot.is_some_and(|t| t < 19)
                && sandbox.exists(&keylet::account_root_key(&o))
        });
        let bid_of = |f: &str| -> Option<crate::tx::offer::Me> {
            tx.fields.get(f).map(|v| ox::signed_value(v).1)
        };
        let (bid_min, bid_max) = (bid_of("BidMin"), bid_of("BidMax"));
        // getPayPrice: range-check against BidMin/BidMax, then the bidder
        // must actually hold the price.
        let get_pay_price = |computed: crate::tx::offer::Me| -> Result<crate::tx::offer::Me, TxResult> {
            let pay = match (bid_min, bid_max) {
                (Some(mn), Some(mx)) => {
                    if !ox::me_cmp(computed, mx).is_gt() {
                        if ox::me_cmp(computed, mn).is_lt() { mn } else { computed }
                    } else {
                        return Err(TxResult::AmmFailed);
                    }
                }
                (Some(mn), None) => {
                    if ox::me_cmp(computed, mn).is_lt() { mn } else { computed }
                }
                (None, Some(mx)) => {
                    if !ox::me_cmp(computed, mx).is_gt() { computed } else {
                        return Err(TxResult::AmmFailed);
                    }
                }
                (None, None) => computed,
            };
            if ox::me_cmp(pay, lp_tokens).is_gt() {
                return Err(TxResult::AmmInvalidTokens);
            }
            Ok(pay)
        };
        let (pay_price, burn) = if !valid_owner {
            // No one owns the slot, or it expired: pay off minSlotPrice and
            // burn the whole price.
            match get_pay_price(min_slot_price) {
                Ok(p) => (p, p),
                Err(e) => return e,
            }
        } else {
            // Occupied: 1.05× the purchase price, decayed by the used
            // fraction of the 20-interval day, plus the floor.
            let t = time_slot.unwrap_or(0);
            let price_purchased = ox::signed_value(&slot["Price"]).1;
            let fraction_used = n_div(n_add((t as u128, 0), (1, 0), Rnd::Near), (20, 0), Rnd::Near);
            let fraction_remaining = n_sub((1, 0), fraction_used, Rnd::Near);
            let p105 = (105u128, -2i32);
            let computed = if t == 0 {
                n_add(n_mul(price_purchased, p105, Rnd::Near), min_slot_price, Rnd::Near)
            } else {
                let decay = n_sub((1, 0), n_pow(fraction_used, 60), Rnd::Near);
                n_add(
                    n_mul(n_mul(price_purchased, p105, Rnd::Near), decay, Rnd::Near),
                    min_slot_price,
                    Rnd::Near,
                )
            };
            let pay = match get_pay_price(computed) {
                Ok(p) => p,
                Err(e) => return e,
            };
            // Refund the previous owner the unused fraction of what they
            // paid, in LP tokens, then burn the rest.
            let refund = n_mul(fraction_remaining, price_purchased, Rnd::Near);
            if ox::me_cmp(refund, pay).is_gt() {
                return TxResult::AmmFailed; // "should never occur" (tecINTERNAL)
            }
            if refund.0 > 0 {
                if let Some(o) = owner {
                    ox::move_leg(sandbox, &tx.account, &o, &lp_leg, refund);
                }
            }
            (pay, n_sub(pay, refund, Rnd::Near))
        };
        // updateSlot: rewrite the slot whole (stale AuthAccounts drop), burn
        // the bid from the bidder's line and the pool's LPTokenBalance.
        let mut new_slot = serde_json::json!({
            "Account": hex::encode(tx.account),
            "Expiration": current + 86_400,
            "Price": {
                "currency": hex::encode_upper(lp_leg.cur),
                "issuer": hex::encode(lp_leg.issuer),
                "value": ox::me_to_value_string(pay_price),
            },
        });
        if discounted_fee != 0 {
            new_slot["DiscountedFee"] = serde_json::json!(discounted_fee);
        }
        if let Some(aa) = tx.fields.get("AuthAccounts") {
            // Dialect: the mirror stores hex account ids.
            let mut arr = Vec::new();
            if let Some(list) = aa.as_array() {
                for e in list {
                    if let Some(a) = e
                        .get("AuthAccount")
                        .and_then(|o| o.get("Account"))
                        .and_then(|v| v.as_str())
                        .and_then(ox::decode20)
                    {
                        arr.push(serde_json::json!({
                            "AuthAccount": { "Account": hex::encode(a) }
                        }));
                    }
                }
            }
            new_slot["AuthAccounts"] = serde_json::Value::Array(arr);
        }
        amm["AuctionSlot"] = new_slot;
        ox::put_json(sandbox, amm_key, &amm);
        let sa_burn = crate::tx::amm_swap::adjust_lp_tokens(lpt_amm, burn, false);
        if !ox::me_cmp(sa_burn, lpt_amm).is_lt() {
            return TxResult::AmmFailed; // burn >= pool balance: tecINTERNAL shape
        }
        if sa_burn.0 > 0 {
            ox::line_adjust(sandbox, &tx.account, &lp_leg, sa_burn, false);
            bump_lp_balance(sandbox, &amm_key, sa_burn, false);
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

        // The depositor must actually HOLD the PLX side: the preclaim now
        // judges the IOU leg too (the balance lambda's other arm), and with
        // no line at all rippled answers tecUNFUNDED_AMM on Amount before
        // ever reaching the XRP branch this test is about. The mainnet
        // specimen's depositor held its PLX; model that. Depositor [0x01] is
        // the LOW side against issuer [0x02], so a positive Balance is held
        // by the depositor.
        {
            let plx_line = serde_json::json!({
                "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
                "Balance": {"currency": hex::encode_upper(plx_leg.cur),
                            "issuer": "0000000000000000000000000000000000000000", "value": "5000"},
                "LowLimit": {"currency": hex::encode_upper(plx_leg.cur), "issuer": hex::encode(depositor), "value": "1000000"},
                "HighLimit": {"currency": hex::encode_upper(plx_leg.cur), "issuer": hex::encode(issuer), "value": "0"},
            });
            state
                .state_map
                .insert(
                    keylet::ripple_state_key(&depositor, &issuer, &plx_leg.cur),
                    serde_json::to_vec(&plx_line).unwrap(),
                )
                .unwrap();
        }

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

    /// #106072851 2B47FAF6 and three siblings: a bot votes on a pool it never
    /// joined. rippled's preclaim (AMMVote.cpp:59-77) answers tecAMM_EMPTY
    /// for an emptied pool and tecAMM_INVALID_TOKENS for a voter with no LP
    /// tokens — "AMM Vote: account is not LP."
    #[test]
    fn amm_vote_requires_lp_tokens() {
        let voter = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        let amm_acct = [0x05u8; 20];
        let mut state = make_state(&voter, 100_000_000);
        let tx = TxFields {
            account: voter,
            tx_type: "AMMVote".to_string(),
            fee: 12,
            sequence: 2,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Asset": {"currency": "XRP"},
                "Asset2": {"currency": "USD", "issuer": hex::encode(issuer)},
                "TradingFee": 300,
            }),
        };
        let akey = amm_key_from_asset_fields(&tx).unwrap();
        let amm = |lpt: &str| {
            serde_json::json!({
                "LedgerEntryType": "AMM", "Account": hex::encode(amm_acct),
                "LPTokenBalance": {"value": lpt}, "TradingFee": 0,
            })
        };
        state.state_map.insert(akey, serde_json::to_vec(&amm("1000")).unwrap()).unwrap();

        // No LP line at all: not an LP.
        assert_eq!(
            AMMVoteTransactor.preclaim(&tx, &Sandbox::new(&state)),
            TxResult::AmmInvalidTokens,
        );

        // Holding LP tokens: allowed. Voter [0x01] is LOW against the AMM
        // account [0x05], so a positive Balance is held by the voter.
        let (_, aacct, lp_leg) = amm_ctx(&tx, &Sandbox::new(&state)).expect("amm ctx");
        let line = serde_json::json!({
            "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
            "Balance": {"currency": hex::encode_upper(lp_leg.cur),
                        "issuer": "0000000000000000000000000000000000000000", "value": "5"},
        });
        state
            .state_map
            .insert(
                keylet::ripple_state_key(&voter, &aacct, &lp_leg.cur),
                serde_json::to_vec(&line).unwrap(),
            )
            .unwrap();
        assert_eq!(AMMVoteTransactor.preclaim(&tx, &Sandbox::new(&state)), TxResult::Success);

        // An emptied pool refuses BEFORE the holds check, even for an LP.
        state.state_map.insert(akey, serde_json::to_vec(&amm("0")).unwrap()).unwrap();
        assert_eq!(AMMVoteTransactor.preclaim(&tx, &Sandbox::new(&state)), TxResult::AmmEmpty);
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

        // Balance: 60M + the single-asset payout. alice created the pool, so
        // she holds its auction slot and (F66, getTradingFee) deposits and
        // withdraws at the DISCOUNTED fee 50, not 500 — the payout the burned
        // tokens buy back rounds to 4,999,999 drops at that fee.
        let data = sandbox.read(&key).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(v["Balance"].as_str().unwrap(), "64999999");
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

// ---------------------------------------------------------------------------
// AMMClawback — the issuer of a pool asset claws back a holder's share by
// force-withdrawing their LP position (AMMClawback.cpp; ⚠ no mainnet
// specimen in any window — blind source port, verified by the scout when
// one lands).
// ---------------------------------------------------------------------------

pub struct AMMClawbackTransactor;

const TF_CLAW_TWO_ASSETS: u64 = 0x0000_0001;

impl Transactor for AMMClawbackTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "AMMClawback" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        if tx.fields.get("Holder").is_none()
            || tx.fields.get("Asset").is_none()
            || tx.fields.get("Asset2").is_none()
        {
            return TxResult::Malformed;
        }
        // Asset must be issued by the clawing account; with tfClawTwoAssets
        // both must be (preflight in rippled; Amount, when present, must
        // match Asset — ported as the same issuer test).
        let issued_by_us = |f: &str| {
            tx.fields
                .get(f)
                .and_then(|v| v.get("issuer"))
                .and_then(|i| i.as_str())
                .and_then(crate::tx::offer::decode20)
                .map(|i| i == tx.account)
                .unwrap_or(false)
        };
        if !issued_by_us("Asset") {
            return TxResult::Malformed;
        }
        let flags = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
        if flags & TF_CLAW_TWO_ASSETS != 0 && !issued_by_us("Asset2") {
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
        use crate::tx::offer as ox;
        let Some(holder) = tx
            .fields
            .get("Holder")
            .and_then(|h| h.as_str())
            .and_then(ox::decode20)
        else {
            return TxResult::Malformed;
        };
        if !sandbox.exists(&keylet::account_root_key(&holder)) {
            return TxResult::NoAccount;
        }
        let snap = sandbox.snapshot();
        let Some((amm_key, amm_acct, _lp_leg)) = amm_ctx(tx, sandbox) else {
            // terNO_AMM is a ter retry code our enum lacks; NoEntry is the
            // closest honest stand-in until a specimen decides it.
            return TxResult::NoEntry;
        };
        // lsfAllowTrustLineClawback (0x80000000) required, lsfNoFreeze
        // (0x00200000) forbidden, on the clawing issuer.
        let iflags = ox::json_at(sandbox, &keylet::account_root_key(&tx.account))
            .and_then(|a| a["Flags"].as_u64())
            .unwrap_or(0);
        if iflags & 0x8000_0000 == 0 || iflags & 0x0020_0000 != 0 {
            return TxResult::NoPermission;
        }

        // The holder's LP token line against this pool.
        let lp_cur = ox::json_at(sandbox, &amm_key)
            .and_then(|o| {
                o["LPTokenBalance"]["currency"].as_str().and_then(|c| {
                    hex::decode(c).ok().and_then(|b| <[u8; 20]>::try_from(b.as_slice()).ok())
                })
            })
            .unwrap_or([0u8; 20]);
        let lp_leg = ox::Leg { xrp: false, cur: lp_cur, issuer: amm_acct };
        let lp_key = keylet::ripple_state_key(&holder, &amm_acct, &lp_leg.cur);
        let Some(lp_line) = ox::json_at(sandbox, &lp_key) else {
            return TxResult::AmmBalance;
        };
        let (_neg, lp_bal) = ox::signed_value(&lp_line["Balance"]);
        if lp_bal.0 == 0 {
            return TxResult::AmmBalance;
        }
        let Some(total_lp) = ox::json_at(sandbox, &amm_key)
            .and_then(|o| o["LPTokenBalance"]["value"].as_str().map(str::to_string))
            .and_then(|s| keylet::amount_mant_exp(&serde_json::Value::String(s)))
        else {
            return TxResult::AmmBalance;
        };

        // Withdraw size: full position without Amount; with Amount, the
        // fraction that makes the ASSET side equal Amount — falling back to
        // the full position when that would burn more LP than held
        // (equalWithdrawMatchingOneAmount).
        let asset_leg = tx.fields.get("Asset").and_then(|v| {
            if v.get("currency").and_then(|c| c.as_str()) == Some("XRP") {
                return Some(ox::Leg { xrp: true, cur: [0u8; 20], issuer: [0u8; 20] });
            }
            let mut amt = v.clone();
            amt["value"] = serde_json::json!("0");
            ox::leg_of(&amt)
        });
        let tokens = match tx.fields.get("Amount").and_then(keylet::amount_mant_exp) {
            None => lp_bal,
            Some(amount) => {
                let Some(leg) = asset_leg.as_ref() else { return TxResult::Malformed };
                let pool = crate::tx::amm_swap::holds(sandbox, &amm_acct, leg);
                if pool.0 == 0 {
                    return TxResult::AmmBalance;
                }
                // frac = amount/pool, 16-digit steps as everywhere in the
                // AMM lane; LP burn = total_lp x frac (round DOWN).
                let frac = ox::st_divide(amount, pool, false);
                let burn = mul_directed(total_lp, frac, false, false);
                if ox::me_cmp(burn, lp_bal) == std::cmp::Ordering::Greater {
                    lp_bal
                } else {
                    burn
                }
            }
        };

        let Some(shares) =
            // Clawback: partial semantics until a specimen calibrates the
            // claw-all arm (rippled threads WithdrawAll there too).
            payout_proportional_to(sandbox, tx, &amm_acct, &holder, tokens, total_lp, false)
        else {
            sandbox.restore_snapshot(snap);
            return TxResult::AmmFailed;
        };

        // Burn the LP tokens: full burn tears the line down, partial burns
        // adjust it — the ordinary-withdraw machinery.
        if ox::me_cmp(tokens, lp_bal) == std::cmp::Ordering::Equal {
            tear_down_lp_line(sandbox, &holder, &amm_acct, lp_key, &lp_line);
        } else {
            let mut line = lp_line.clone();
            let rest = ox::me_sub(lp_bal, tokens);
            let neg = holder >= amm_acct;
            line["Balance"]["value"] = serde_json::Value::String(if neg {
                format!("-{}", ox::me_to_value_string(rest))
            } else {
                ox::me_to_value_string(rest)
            });
            sandbox.write(lp_key, serde_json::to_vec(&line).unwrap_or_default());
        }
        bump_lp_balance(sandbox, &amm_key, tokens, false);
        let lpt_zero = ox::json_at(sandbox, &amm_key)
            .and_then(|o| o["LPTokenBalance"]["value"].as_str().map(|v| v == "0"))
            .unwrap_or(false);
        if lpt_zero {
            delete_amm(sandbox, &amm_key, &amm_acct, tx);
        }

        // The claw: the ASSET share moves holder -> issuer (redemption);
        // Asset2's share moves too only under tfClawTwoAssets — otherwise
        // the holder keeps it.
        let flags = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
        for (leg, share) in &shares {
            let ours = leg.issuer == tx.account && !leg.xrp;
            let is_asset = asset_leg.as_ref().map(|a| a.cur == leg.cur && a.issuer == leg.issuer).unwrap_or(false);
            if ours && (is_asset || flags & TF_CLAW_TWO_ASSETS != 0) {
                ox::move_leg(sandbox, &holder, &tx.account, leg, *share);
            }
        }
        TxResult::Success
    }
}
