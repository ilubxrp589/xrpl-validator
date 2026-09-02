//! Byte-exact vector drills for the class-B line-ULP family (2026-09-01).
//!
//! One bot (rJfVTbJs…, tfSell|tfIoC, always selling Bitstamp IOUs, issuer
//! rate 1.0015) generated the six-fix era's dominant byte-diff family. The
//! shadow flags a one-ULP Balance divergence on the maker/taker lines the
//! fill credits — ours-len == ffi-len, PRE-OK, tx-tagged. Root cause (F50):
//! rippled's flowCross grosses sendMax ONCE (multiplyRound roundUp) and the
//! in-limited final fill consumes the remaining GROSS verbatim, deriving
//! its net by division (limitStepIn) — our walk tracked the NET budget and
//! re-grossed per leg, one conversion too many.
//!
//! - 106679738 DA3C22D8: USD→XRP, real offer + AMM; maker line 0339209F…
//!   must land …759 (net chain said …760). The founding specimen.
//! - 106679743 AA7BF8C5: same key five ledgers later — the recurrence pin.
//! - 106688646 5524F0F2: ETH→RLUSD (IOU/IOU) — the 4-hot-key cluster
//!   specimen (A6203B55/77AC9AC2/A46F22FC/27577E3A), every touched node
//!   byte-compared.
use serde_json::Value;
use xrpl_core::types::Hash256;
use xrpl_ledger::ledger::header::LedgerHeader;
use xrpl_ledger::ledger::sandbox::SandboxEntry;
use xrpl_ledger::ledger::state::LedgerState;
use xrpl_node::native_apply::{build_txfields, canon_for_encode, hexify_addresses, native_apply_one};

fn key32(hex_key: &str) -> Hash256 {
    Hash256(<[u8; 32]>::try_from(hex::decode(hex_key).unwrap().as_slice()).unwrap())
}

fn hydrate(state: &mut LedgerState, key_hex: &str, entry_hex: &str) {
    let bytes = hex::decode(entry_hex.trim()).unwrap();
    let mut jv = xrpl_core::codec::decode::decode_transaction_binary(&bytes).unwrap();
    hexify_addresses(&mut jv);
    state
        .state_map
        .insert(key32(key_hex), serde_json::to_vec(&jv).unwrap())
        .unwrap();
}

fn run_bundle(bundle_json: &str) {
    let bundle: Value = serde_json::from_str(bundle_json).unwrap();
    let seq = bundle["seq"].as_u64().unwrap() as u32;
    let pct = bundle["parent_close_time"].as_u64().unwrap() as u32;
    let header = LedgerHeader {
        sequence: seq - 1,
        total_coins: bundle["total_coins"].as_u64().unwrap(),
        parent_hash: key32(bundle["parent_hash"].as_str().unwrap()),
        transaction_hash: Hash256([0; 32]),
        account_hash: Hash256([0; 32]),
        parent_close_time: pct,
        close_time: pct,
        close_time_resolution: 10,
        close_flags: 0,
    };
    let mut state = LedgerState::new_unverified(header);
    for (k, v) in bundle["pre"].as_object().unwrap() {
        hydrate(&mut state, k, v.as_str().unwrap());
    }

    let tx = &bundle["tx"];
    let tx_hash = tx["hash"].as_str().unwrap();
    let txf = build_txfields(tx).expect("txfields");
    let (ter, mut mods) = native_apply_one(&state, &txf);
    // The bundle carries the ledger's verdict ("result"); a tec pin compares
    // the fee-only root exactly like a success pin compares its targets.
    let want_ter = bundle["result"].as_str().unwrap_or("tesSUCCESS");
    assert_eq!(ter, want_ter, "mainnet's result for this OfferCreate");

    xrpl_ledger::ledger::threading::stamp_threading(
        &mut mods,
        &|k| state.state_map.lookup(k).map(|b| b.to_vec()),
        tx_hash,
        seq,
    );

    for (k, want_hex) in bundle["expect"].as_object().unwrap() {
        let ent = mods
            .get(&key32(k))
            .unwrap_or_else(|| panic!("target {k} must be written by the apply"));
        let bytes = match ent {
            SandboxEntry::Created(b) | SandboxEntry::Modified(b) => b.clone(),
            SandboxEntry::Deleted => panic!("target {k} deleted?"),
        };
        let mut jv: Value = serde_json::from_slice(&bytes).unwrap();
        canon_for_encode(&mut jv);
        let enc = xrpl_core::codec::encode::encode_transaction_json(&jv, false).unwrap();
        let want = hex::decode(want_hex.as_str().unwrap().trim()).unwrap();
        assert_eq!(
            hex::encode_upper(&enc),
            hex::encode_upper(&want),
            "target {k} must byte-match the mainnet post-state"
        );
    }
}

#[test]
fn offer_fill_line_is_byte_exact() {
    run_bundle(include_str!("vectors/offer_ulp_106679738.json"));
}

#[test]
fn offer_fill_line_recurrence_is_byte_exact() {
    run_bundle(include_str!("vectors/offer_ulp_106679743.json"));
}

#[test]
fn offer_fill_cluster_is_byte_exact() {
    run_bundle(include_str!("vectors/offer_ulp_106688646.json"));
}

/// F56's regression guard (#106674447 2049BE47, XAH→RLUSD tfIoC through the
/// XRP bridge, buying EXACTLY 1.0 RLUSD). The second slice's raw pool offer
/// exceeds the remaining budget, so the in-clamp marks it IN-EXHAUSTED — and
/// then the out-cap clamp binds tighter and re-derives it. With the gross
/// cap armed for every fully-funded IOU-in taker (F56), the stale flag made
/// an OUT-limited slice floor its pool mid (5253 for 5254), skip the
/// whole-drop reprice and debit the taker's WHOLE remaining 929.66 XAH as
/// the leg-A gross: six nodes off, forty ledgers of soak-98 cascade.
/// rippled sizes an out-limited iteration by `ceil_out` and its in is the
/// derived amount — the budget is not exhausted, whatever the raw offer
/// said before the limit applied.
#[test]
fn offer_fill_out_limited_bridge_under_armed_cap_is_byte_exact() {
    run_bundle(include_str!("vectors/offer_outlimit_106674447.json"));
}

/// F62's regression guard (#106696774 12AF1556 — the 183B@108 family: the
/// XAH→RLUSD tfIoC bot through the XRP bridge, hot key 61C1E7D1, still
/// bleeding after the F45-F60 deploy). Slice 1 crosses book leg A
/// (A1A868D4) with a book leg B (8A50B0EF) whose maker holds only 0.32275
/// of its 0.47025 RLUSD. rippled's BookStep sizes an owner-FUNDS-limited
/// offer with `limitOut(…, roundUp=false)` (fixReducedOffersV1, "prevent
/// order book blocking by strictly rounding down"), so the mid-leg XRP is
/// floor(0.32275 × 348573 / 0.47025) = 239238 drops — the FFI trace's
/// `accountSendIOU … 239238/XRP`. Our ceil paid 239239, and one drop's
/// worth of XAH more on leg A: five nodes off, roots ±1 drop, both lines
/// and the rested offer.
#[test]
fn offer_fill_funds_limited_bridge_leg_floors_the_mid_leg_is_byte_exact() {
    run_bundle(include_str!("vectors/offer_bridge_funds_106696774.json"));
}

/// F67's regression guard (#106698333 314D2290): an OfferCreate buying GTA6
/// from a RequireAuth issuer (rhgbv9S3…) with a trust line the issuer has
/// not authorized. CreateOffer::preclaim's checkAcceptAsset on the TakerPays
/// side refuses with tecNO_AUTH (fee only); we crossed it. The pinned
/// target is the sender's fee-only root.
#[test]
fn offer_create_unauthorized_taker_pays_line_is_tec_no_auth() {
    run_bundle(include_str!("vectors/offer_noauth_106698333.json"));
}

/// Finding 69 — #106698812 5D50FA86: tfSell|tfIoC selling 9.08905 QUWAGI for
/// 193960 drops. The taker's QUWAGI line carries NoRipple on the ISSUER's
/// side (Flags 0x330000), so rippled's `BookStep::check` refuses the strand
/// (`toStep failed: -90` = terNO_RIPPLE), nothing crosses although the book's
/// tip is a funds-limited offer at a better rate, and the IoC is tecKILLED
/// with one mutation. We crossed the tip and reported tesSUCCESS.
#[test]
fn offer_create_issuer_side_noripple_refuses_the_book_step() {
    run_bundle(include_str!("vectors/offer_ioc_noripple_106698812.json"));
}

/// Finding 75 — #106700231 4CEDDA54: a funds-limited tfSell|tfIoC selling
/// XRP for RLUSD, filled by an AMM slice, two CLOB offers and a tail AMM
/// slice. The XRP CLOB fills were never added to the gross-spent
/// accumulator, so the tail slice's remaining cap was stale (11.69M drops
/// for a 5.18M slice); the oversized debit was dropped and the taker kept
/// 5,181,445 drops the pool had already received.
#[test]
fn offer_create_xrp_fills_feed_the_gross_cap_of_the_tail_slice() {
    run_bundle(include_str!("vectors/offer_xrp_tail_gross_106700231.json"));
}

/// Findings 76 + 78 — #106701467 DAE80780: an FLR/USD offer crossed through
/// the direct FLR/USD pool and the XRP bridge (both legs pools) with a
/// 2-drop dust tip on the XRP→USD book. rippled ranks the pool slice against
/// the tip AS FILED (76) and grosses the direct-pool slice by the in-issuer's
/// rate (78); red with either missing.
#[test]
fn offer_create_multipath_pools_rank_on_the_raw_tip_and_gross_the_direct_slice() {
    run_bundle(include_str!("vectors/offer_multipath_rawtip_fee_106701467.json"));
}

/// Findings 77 + 79 — #106701372 8BFFFACE: a passive RLUSD/BTC offer whose
/// bridge leg B is the XRP/BTC pool with no book (77), resting a remainder
/// that rippled re-derives from the offer's own rate with round-up (79):
/// 1728.598227693783 RLUSD, one ULP above the walk's net remainder.
#[test]
fn offer_create_pool_only_leg_fills_and_the_remainder_follows_the_rate() {
    run_bundle(include_str!("vectors/offer_pool_leg_remainder_106701372.json"));
}
