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

/// Finding 79b — #106644297 2B6E3D5A: a tfSell XRP→BTC offer partially
/// crossed. Under fixReducedOffersV1 the resting out is
/// divRoundStrict(in, rate, roundUp = false): 0.007216356069340599 BTC on
/// mainnet; the first cut of finding 79 rounded it up to …600.
#[test]
fn offer_create_sell_remainder_rounds_down_under_fix_reduced_offers() {
    run_bundle(include_str!("vectors/offer_sell_remainder_106644297.json"));
}

/// #106703173 AE6C73F9 (finding 90): FLR→BTC OfferCreate against the FLR/BTC
/// pool (fee 969) where FLR carries a 1.003 transfer rate. The single-path
/// fill is sized by StrandFlow::limitOut from the strand's QualityFunction;
/// BookStep::getQualityFunc folds the in-side rate into that function (m and
/// b scaled by 1/trIn at Nearest) and the limit is the crossing's inflated
/// limitQuality (Quality{out, sendMax}). Solving the net form instead lands
/// out at 5.509588100530126e-8 for rippled's 5.509588100528479e-8 and both
/// pool lines two ulps off.
#[test]
fn offer_amm_limit_solve_folds_the_transfer_rate_into_the_quality_function() {
    run_bundle(include_str!("vectors/offer_amm_limit_qf_transfer_rate_106703173.json"));
}

/// #106702066 344657FD (finding 91): a tfSell|tfImmediateOrCancel XSPECTAR→XRP
/// crossing that runs nine flow iterations. rippled re-derives the remaining
/// input each iteration as sendMax minus the ascending 16-digit fold of every
/// saved iteration in (StrandFlow.h:724), never a running subtraction; the
/// fold budgets the ninth iteration at 7963.16183528205 where the running
/// remainder reads …206. Its last fill therefore pays …205, the maker's
/// TakerPays residual sits one ulp higher, and the taker keeps 1e-11 on its
/// line where we drained it to zero.
#[test]
fn offer_sell_remaining_input_is_the_fold_of_saved_iteration_ins() {
    run_bundle(include_str!("vectors/offer_sell_in_fold_106702066.json"));
}

/// #106708835 74C9F462 (finding 93): a tfSell|tfFillOrKill EUR→ETH crossing
/// that autobridges through XRP with a BOOK leg A and a pool leg B, and whose
/// single slice exhausts the taker's 100 EUR. rippled's forward pass is
/// in-limited — `limitStepIn` → `ceilInStrict(…, roundUp = false)` — so the
/// XRP mid-leg FLOORS for any offer type: 86946030 drops, not the ceil's
/// 86946031, and the pool then pays 0.048037377799289 ETH instead of
/// …378344132. Byte-exact on the maker's XRP offer, both trust lines and
/// both XRP balances.
#[test]
fn offer_bridged_in_limited_book_leg_floors_the_mid_leg() {
    run_bundle(include_str!("vectors/offer_bridged_in_limited_floor_106708835.json"));
}

/// #106710940 3F9DDD254714 (finding 95): a tfPassive|tfSell offer of 16.71404
/// SOL for 1250 XRP under the SOL issuer's TickSize 6. rippled rounds the
/// rate UP to the tick (74787500 drops/SOL) and re-derives TakerPays with
/// `multiply` under Number semantics: 1250001266.5 rounds half-EVEN to
/// 1250001266 drops, and the filed rate is then `divide`'s `+5` folded
/// half-even to `…22CEF8678`. Our XRP-side multiply still carried the legacy
/// `+7` form and filed 1250001267 one book level higher. Byte-exact on the
/// created Offer and its directory page.
#[test]
fn offer_tick_size_sell_rounds_taker_pays_half_even() {
    run_bundle(include_str!("vectors/offer_ticksize_halfeven_106710940.json"));
}

/// #106712068 070969D26A53 (finding 97): a tfPassive|tfSell USDC→RLUSD
/// crossing whose first fill is the DIRECT head inside a bridge attempt,
/// bound by the maker's funds (807.6986554420118 of 807.6986563115758
/// RLUSD). rippled's funds clamp is `limitOut(…, roundUp = false)`
/// (BookStep.cpp:790) — a floor at 16 digits — so the taker pays
/// 807.6978469356594 USDC; the site's unconditional ceil charged …595, and
/// the one-ulp drift reached the next maker's fill and line. Byte-exact on
/// the second maker's USDC line and its partially consumed offer.
#[test]
fn offer_bridge_direct_head_funds_bound_fill_floors_the_input() {
    run_bundle(include_str!("vectors/offer_bridge_head_funds_floor_106712068.json"));
}

/// #106714102 A1B242D893E7 (finding 99): 2.376631 USD.Bitstamp for 1759848
/// drops against the XRP/USD pool, with the order book's tip sitting between
/// the net limit and the transfer-rate-inflated one. rippled anchors the AMM
/// offer on the LOB quality, it comes out strictly better than the tip, so
/// the tip IS the AMM offer: `qualityUpperBound` waives the fee, the strand
/// is admitted, and the pool fills the whole want (in 2.372382758599013).
/// Our tail gate rested the offer whenever the tip sat in that band. Byte-
/// exact on both USD lines and both AccountRoots (taker and pool).
#[test]
fn offer_amm_admitted_when_pool_spot_beats_a_tip_inside_the_fee_band() {
    run_bundle(include_str!("vectors/offer_amm_tip_in_fee_band_106714102.json"));
}

/// #106715477 01019A7D3E59 (finding 99, revised): the same account and book
/// as #106714102 five hundred ledgers later — pool spot better than the
/// order book tip, tip inside the fee band — and mainnet RESTS the offer.
/// The anchored pool offer rippled generates (11.153 USD / 8272983 XRP,
/// 1.34815e-6) misses the taker's net limit (1.34781e-6), so the strand is
/// dropped at admission ("All strands dry"); the first specimen's anchored
/// offer cleared its limit. Admission judges the anchored offer, not the
/// spot. Byte-exact on the created Offer, both directory pages and the
/// taker's AccountRoot; the pool untouched.
#[test]
fn offer_amm_rests_when_the_anchored_pool_offer_misses_the_net_limit() {
    run_bundle(include_str!("vectors/offer_amm_anchored_misses_net_limit_106715477.json"));
}

/// #106716594 DE0753F0C77B (finding 101): the crossed book's tip is the
/// taker's OWN resting offer, one level beyond the net limit, and the
/// XRP/USD pool anchored at that level covers the whole want. rippled runs
/// tryAMM before it visits the level's offers (BookStep.cpp:837-846); the
/// pool fills everything, execOffer returns false, and the self offer is
/// never reached — it stays, OwnerCount 4. Our sweep self-reaped it before
/// the tail took the identical pool fill (OwnerCount 3, offer and page
/// gone). Byte-exact on the pool root, both USD lines and the taker's root.
#[test]
fn offer_pool_covering_the_want_leaves_the_takers_own_tip_offer_alone() {
    run_bundle(include_str!("vectors/offer_amm_fills_before_self_offer_reap_106716594.json"));
}

/// Finding 111 (#106723136 DDE9287F69AF): an OfferCreate selling 172.013 RLUSD
/// for 172.013 USDC with no flags — single-path, the direct book beyond the
/// limit, so the XRP bridge alone. Leg A (RLUSD→XRP) has a book tip inside
/// the limit AND a pool whose spot beats it: rippled's `tryAMM(tip)` runs
/// first and `getOffer(tip)` anchors a pool offer at the tip's quality, so
/// BookStep consumes the pool (301.29 RLUSD / 222.98 XRP, all of it needed)
/// and the tip's offer 8FD686B8 never moves. We priced the leg by the
/// multi-path fib slice, lost to the tip by 1.4e-4, took the tip's offer and
/// wrote the maker's nodes instead of the pool's. Three targets byte-pinned;
/// the taker's in is rippled's 171.7982923412997 RLUSD.
#[test]
fn offer_bridge_leg_takes_the_pool_anchored_at_its_book_tip() {
    run_bundle(include_str!("vectors/offer_bridge_leg_pool_anchored_at_the_tip_106723136.json"));
}

/// Finding 103 (#106720945 527C1FA56527): a tfSell|tfFillOrKill OfferCreate
/// selling 30,920,313.975492 XDC for 2497.84 XRPH. No direct book or pool;
/// the XRP bridge has an XDC/XRP pool behind a book tip and an XRP/XRPH pool
/// with no book. rippled's `tip()` judges leg A by the anchored pool offer's
/// OWN quality — Quality{amounts} of the 550,390,143.78 XDC / 466,550,412
/// drop slice that walks the spot up to the tip, 1.1797 XDC per drop against
/// a tip of 1.408 — and leg B by maxOffer's Quality{balances}, the feeless
/// spot; composed, 11,714 XDC per XRPH sits inside the 12,378 limit and one
/// single-path pass fills the whole offer (→ 3084.200478487 XRPH). We judged
/// leg A at the tip, read 13,986, and killed it. Five targets byte-pinned.
#[test]
fn offer_bridge_anchored_leg_is_judged_by_its_offers_own_quality() {
    run_bundle(include_str!("vectors/offer_bridge_anchored_leg_judged_by_its_offer_106720945.json"));
}

/// Finding 112 (#106727096 DD7DCA711B8A): a tfSell|tfImmediateOrCancel sale
/// of ONE drop for 0.000001 USD. The book's tip 5B469CA9 is a one-drop
/// remainder (TakerPays 1 drop, TakerGets 0.0000013709342 USD) whose
/// quality, recomputed from what is left, rounds WORSE than the directory it
/// sits in: rippled's OfferStream::step removes it unexecuted
/// (shouldRmSmallIncreasedQOffer — fixRmSmallIncreasedQOffers) and crosses
/// the next level, BD4970D4, for the drop (2.380987 → 2.380985629809433 USD).
/// We crossed the remainder itself and left BD4970D4 alone. Seven targets
/// byte-pinned, BD4970D4 among them (created earlier in the same ledger).
#[test]
fn offer_dust_remainder_whose_quality_rounds_worse_is_removed_not_crossed() {
    run_bundle(include_str!("vectors/offer_dust_xrp_offer_increased_quality_removed_106727096.json"));
}


/// Finding 114 (#106730661 3241437EB11D): a tfImmediateOrCancel buy of
/// 20.575959 XRP for 28.215814 RLUSD by a taker holding 0.0000206 RLUSD.
/// rippled's REV pass sizes the book against the WANT before the DirectStep
/// limits the sender to their funds: it consumes the 2.456-XRP tip 2B30E6F4
/// whole, and the `do … while (offers.step())` loop then steps the stream —
/// removing the expired 729F17CE and 392EFC07 on the next level — until the
/// live offer behind them ends the pass (a worse level). The fwd pass, limited
/// to the funds, is then "rejected by limitQuality": nothing crosses,
/// tecKILLED, and the two expired offers are gone with their page — "rm bad
/// offers even if the strand fails", applied through sbCancel. Our walk sized
/// every fill by min(want, funds) from the first offer and never left the tip.
/// Three targets byte-pinned: the reaped makers' owner directory and root and
/// the taker's root; the page …4F04DF824B0BC6DD was created and removed
/// earlier in the same ledger, so its pre-image is seated by hand.
#[test]
fn offer_killed_crossing_still_reaps_what_the_rev_pass_stepped_over() {
    run_bundle(include_str!("vectors/offer_killed_crossing_reaps_the_rev_pass_step_106730661.json"));
}
