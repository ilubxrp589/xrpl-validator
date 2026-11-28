//! Quality's `ceil_out` on an XRP product past STAmount's range, through
//! the ported flow engine — 3.5.0-rc1 Offer_test.cpp:449-536
//! (testRmSmallIncreasedQOffersClampNative), both rule states.
//!
//! carol offers 9.999999999999995 USD for 1e17 drops (kMaxNativeN) but holds
//! 9.999999999999994 USD. Reducing her offer to her funds multiplies the
//! funds by the offer's rate, 1.000000000000001e16 drops per USD: about
//! 1.00000000000000040e17 drops, out of range.
//!
//! - 3.4.1 (Quality.cpp:83-96): `mulRoundStrict` throws "Native currency
//!   amount out of range"; the flow leaves and the Payment, or the
//!   OfferCreate's crossing, is tecINTERNAL with nothing applied
//!   (RippleCalc.cpp:95-103, OfferCreate.cpp:577-581).
//! - fixCleanup3_5_0 (3.5.0-rc1 Quality.cpp:88-127): `tryMulRound` catches
//!   that range error for an XRP in side, the reduced in is carol's whole
//!   in (1e17), and the flow goes on.
use crate::flow::amounts::{EitherAmount, IouAmount};
use crate::flow::steps::Asset;
use crate::ledger::amendments as am;
use crate::ledger::header::LedgerHeader;
use crate::ledger::keylet;
use crate::ledger::sandbox::{apply_modifications, Sandbox};
use crate::ledger::state::LedgerState;
use crate::ledger::transactor::{Transactor, TxFields, TxResult};
use serde_json::{json, Value};
use xrpl_core::types::Hash256;

const ALICE: [u8; 20] = [0x11; 20];
const BOB: [u8; 20] = [0x12; 20];
const CAROL: [u8; 20] = [0x13; 20];
const DAN: [u8; 20] = [0x14; 20];
const GW: [u8; 20] = [0xA0; 20];
/// 10,000 XRP.
const FUNDS: u64 = 10_000_000_000;
const CAROL_PAYS: &str = "100000000000000000";
const CAROL_GETS: &str = "9.999999999999995";
const CAROL_FUNDS: &str = "9.999999999999994";

fn usd() -> [u8; 20] {
    let mut c = [0u8; 20];
    c[12..15].copy_from_slice(b"USD");
    c
}

fn usd_amount(value: &str) -> Value {
    json!({"currency": "USD", "issuer": hex::encode(GW), "value": value})
}

fn put(state: &mut LedgerState, key: Hash256, v: &Value) {
    state.state_map.insert(key, serde_json::to_vec(v).unwrap()).unwrap();
}

fn account(state: &mut LedgerState, id: &[u8; 20]) {
    put(state, keylet::account_root_key(id), &json!({
        "LedgerEntryType": "AccountRoot", "Account": hex::encode(id), "Balance": FUNDS.to_string(),
        "Sequence": 1, "OwnerCount": 1, "Flags": 0,
    }));
}

/// `holder`'s USD line to GW (every holder sorts below GW), holding `held`.
fn line(state: &mut LedgerState, holder: &[u8; 20], held: &str) {
    let cur = hex::encode_upper(usd());
    put(state, keylet::ripple_state_key(holder, &GW, &usd()), &json!({
        "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
        "Balance": {"currency": cur, "issuer": "0000000000000000000000000000000000000000", "value": held},
        "LowLimit": {"currency": cur, "issuer": hex::encode(holder), "value": "1000"},
        "HighLimit": {"currency": cur, "issuer": hex::encode(GW), "value": "0"},
        "LowNode": "0", "HighNode": "0",
    }));
}

fn offer_create(account: [u8; 20], taker_pays: Value, taker_gets: Value) -> TxFields {
    TxFields {
        account, tx_type: "OfferCreate".to_string(), fee: 10, sequence: 1, ticket_seq: None, last_ledger_seq: None,
        fields: json!({"TakerPays": taker_pays, "TakerGets": taker_gets}),
        inner_batch: false,
    }
}

/// Offer_test.cpp:468-482's setup: carol's offer, and bob's 10 USD for 10
/// XRP ahead of it when `bob_offer`. `extra` amendments join mainnet's.
fn setup(bob_offer: bool, extra: &[&str]) -> LedgerState {
    let mut state = LedgerState::new_unverified(LedgerHeader {
        sequence: 100, total_coins: 100_000_000_000_000_000, parent_hash: Hash256([0; 32]),
        transaction_hash: Hash256([0; 32]), account_hash: Hash256([0; 32]),
        parent_close_time: 0, close_time: 10, close_time_resolution: 10, close_flags: 0,
    });
    let mut rules = vec![am::FIX_CLEANUP_3_2_0, am::FIX_CLEANUP_3_3_0, am::FIX_CLEANUP_3_4_0];
    rules.extend(extra);
    put(&mut state, keylet::amendments_key(), &json!({"LedgerEntryType": "Amendments", "Flags": 0, "Amendments": rules}));
    for id in [ALICE, BOB, CAROL, DAN, GW] {
        account(&mut state, &id);
    }
    line(&mut state, &ALICE, "0");
    line(&mut state, &BOB, "10");
    line(&mut state, &CAROL, CAROL_FUNDS);
    line(&mut state, &DAN, "0");
    let mut sb = Sandbox::new(&state);
    let offer = crate::tx::offer::OfferCreateTransactor;
    if bob_offer {
        assert_eq!(offer.do_apply(&offer_create(BOB, json!("10000000"), usd_amount("10")), &mut sb), TxResult::Success, "bob's offer");
    }
    assert_eq!(offer.do_apply(&offer_create(CAROL, json!(CAROL_PAYS), usd_amount(CAROL_GETS)), &mut sb), TxResult::Success, "carol's offer");
    let mods = sb.into_modifications();
    apply_modifications(&mut state, mods).unwrap();
    assert_eq!(offers_of(&Sandbox::new(&state), &CAROL), 1);
    state
}

fn read(sb: &Sandbox, key: &Hash256) -> Option<Value> {
    sb.read(key).map(|b| serde_json::from_slice(&b).unwrap())
}

/// How many offers `owner` has left (offer sequence 1 is each owner's only one).
fn offers_of(sb: &Sandbox, owner: &[u8; 20]) -> usize {
    usize::from(sb.exists(&keylet::offer_key(owner, 1)))
}

fn usd_held(sb: &Sandbox, holder: &[u8; 20]) -> IouAmount {
    let l = read(sb, &keylet::ripple_state_key(holder, &GW, &usd())).unwrap();
    let (neg, me) = crate::tx::offer::signed_value(&l["Balance"]);
    IouAmount::from_me(neg && me.0 > 0, me)
}

fn iou(value: &str) -> IouAmount {
    let (neg, me) = crate::tx::offer::signed_value(&json!({"value": value}));
    IouAmount::from_me(neg, me)
}

/// Offer_test.cpp:493-512: alice pays dan 20 USD through the XRP → USD book
/// for at most 5,000 XRP, partially.
fn payment() -> TxFields {
    TxFields {
        account: ALICE, tx_type: "Payment".to_string(), fee: 10, sequence: 1, ticket_seq: None, last_ledger_seq: None,
        fields: json!({
            "Destination": hex::encode(DAN),
            "Amount": usd_amount("20"),
            "SendMax": "5000000000",
            "Paths": [[{"currency": "USD", "issuer": hex::encode(GW)}]],
            "Flags": 0x0002_0000u64,
        }),
        inner_batch: false,
    }
}

/// Offer_test.cpp:515-534: alice offers 5,000 XRP for 1e-7 USD.
fn alice_offer() -> TxFields {
    offer_create(ALICE, usd_amount("0.0000001"), json!("5000000000"))
}

fn usd_asset() -> Asset {
    Asset { currency: usd(), issuer: Some(GW) }
}

#[test]
fn a_payment_reaching_the_out_of_range_reduction_is_tecinternal_before_fixcleanup350() {
    let state = setup(true, &[]);
    let mut sb = Sandbox::new(&state);
    assert_eq!(crate::flow::payment_flow::apply_ripple_payment(&payment(), &mut sb), TxResult::TecInternal);
    assert_eq!((offers_of(&sb, &BOB), offers_of(&sb, &CAROL)), (1, 1), "bob's offer is not taken");
    assert_eq!(usd_held(&sb, &DAN), IouAmount::ZERO);
    assert_eq!(usd_held(&sb, &CAROL), iou(CAROL_FUNDS));
}

#[test]
fn a_payment_takes_the_clamped_offer_under_fixcleanup350() {
    let state = setup(true, &[am::FIX_CLEANUP_3_5_0]);
    let mut sb = Sandbox::new(&state);
    assert_eq!(crate::flow::payment_flow::apply_ripple_payment(&payment(), &mut sb), TxResult::Success);
    assert_eq!((offers_of(&sb, &BOB), offers_of(&sb, &CAROL)), (0, 1), "bob's offer consumed, carol's partly taken");
    assert!(usd_held(&sb, &DAN) > iou("10"), "dan {:?}", usd_held(&sb, &DAN));
    assert!(usd_held(&sb, &CAROL) < iou(CAROL_FUNDS));
}

#[test]
fn a_crossing_reaching_the_out_of_range_reduction_is_tecinternal_before_fixcleanup350() {
    let state = setup(false, &[]);
    let mut sb = Sandbox::new(&state);
    let r = crate::flow::offer_cross::flow_cross(&mut sb, &ALICE, Asset::XRP, EitherAmount::Xrp(5_000_000_000), usd_asset(), EitherAmount::Iou(iou("0.0000001")), 0, None);
    assert_eq!(r.ter, TxResult::TecInternal);
    assert!(r.removed.is_empty());
    assert_eq!((r.after_in, r.after_out), (EitherAmount::Xrp(5_000_000_000), EitherAmount::Iou(iou("0.0000001"))), "takerAmount untouched");
    assert_eq!(offers_of(&sb, &CAROL), 1);
    assert_eq!(usd_held(&sb, &ALICE), IouAmount::ZERO);
}

#[test]
fn a_crossing_takes_the_clamped_offer_under_fixcleanup350() {
    let state = setup(false, &[am::FIX_CLEANUP_3_5_0]);
    let mut sb = Sandbox::new(&state);
    let r = crate::flow::offer_cross::flow_cross(&mut sb, &ALICE, Asset::XRP, EitherAmount::Xrp(5_000_000_000), usd_asset(), EitherAmount::Iou(iou("0.0000001")), 0, None);
    assert_eq!(r.ter, TxResult::Success);
    assert_eq!(r.actual_out, EitherAmount::Iou(iou("0.0000001")));
    assert_eq!(usd_held(&sb, &ALICE), iou("0.0000001"));
    assert_eq!(offers_of(&sb, &CAROL), 1);
}

/// The two transactions through the whole pipeline, as rippled's test runs
/// them: every amendment, SingleAssetVault included — which also selects
/// the ported engine whatever XRPL_FLOW_ENGINE says (`port_enabled`).
#[test]
fn both_transactions_through_the_pipeline_in_both_rule_states() {
    let apply = |state: &LedgerState, tx: &TxFields| {
        let mut sb = Sandbox::new(state);
        let r = crate::tx::dispatch::apply_on_sandbox(tx, &mut sb);
        let mods = sb.into_modifications();
        let mut after = state.clone();
        apply_modifications(&mut after, mods).unwrap();
        (r, after)
    };
    for fix in [false, true] {
        let mut extra = vec![am::SINGLE_ASSET_VAULT];
        if fix {
            extra.push(am::FIX_CLEANUP_3_5_0);
        }
        let ((r, applied), after) = apply(&setup(true, &extra), &payment());
        let sb = Sandbox::new(&after);
        if fix {
            assert_eq!((r, applied), (TxResult::Success, true));
            assert_eq!(offers_of(&sb, &BOB), 0);
            assert!(usd_held(&sb, &DAN) > iou("10"));
        } else {
            assert_eq!((r, applied), (TxResult::TecInternal, true), "a tec: the fee is claimed");
            assert_eq!((offers_of(&sb, &BOB), offers_of(&sb, &CAROL)), (1, 1));
            assert_eq!(usd_held(&sb, &DAN), IouAmount::ZERO);
            let alice = read(&sb, &keylet::account_root_key(&ALICE)).unwrap();
            assert_eq!(alice["Balance"], json!((FUNDS - 10).to_string()), "only the fee");
        }

        let ((r, applied), after) = apply(&setup(false, &extra), &alice_offer());
        let sb = Sandbox::new(&after);
        assert_eq!(offers_of(&sb, &CAROL), 1);
        assert_eq!(offers_of(&sb, &ALICE), 0, "no offer placed");
        if fix {
            assert_eq!((r, applied), (TxResult::Success, true));
            assert_eq!(usd_held(&sb, &ALICE), iou("0.0000001"));
        } else {
            assert_eq!((r, applied), (TxResult::TecInternal, true));
            assert_eq!(usd_held(&sb, &ALICE), IouAmount::ZERO);
            assert_eq!(usd_held(&sb, &CAROL), iou(CAROL_FUNDS));
        }
    }
}
