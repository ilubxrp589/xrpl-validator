//! The direct MPT Payment between two holders with a transfer fee
//! (Payment.cpp's legacy MPT arm), as rippled 3.5.0-rc1's MPToken_test pins
//! it with and without fixCleanup3_5_0:
//!
//! - without the fix (3.4.1 Payment.cpp:716-735, also 3.5.0-rc1
//!   Payment.cpp:750-766): the cost is `multiply(Amount, rate)` and a partial
//!   delivery `divide(SendMax, rate)`, both to nearest; either throws
//!   (tefEXCEPTION) where it does not fit, and the divide's `+ 5` can wrap
//!   to a zero delivery;
//! - with it (3.5.0-rc1 Payment.cpp:722-749): the cost is
//!   `tryMulRatio(Amount, rate, 1e9, up)` — past int64 it exceeds any SendMax
//!   — a partial delivery `mulRatio(SendMax, 1e9, rate, down)`, nothing
//!   delivered is tecPATH_PARTIAL, and the sender is debited
//!   `addTransferFee(deliver, rate)`, rounded up (TokenHelpers.cpp:1363-1373).
use super::*;
use crate::ledger::amendments as am;
use crate::ledger::header::LedgerHeader;
use crate::ledger::state::LedgerState;
use serde_json::json;

const BOB: [u8; 20] = [0x11; 20];
const CAROL: [u8; 20] = [0x12; 20];
/// The issuer.
const ALICE: [u8; 20] = [0x21; 20];
const TF_PARTIAL_PAYMENT: u64 = 0x0002_0000;

fn mptid() -> [u8; 24] {
    let mut id = [0u8; 24];
    id[..4].copy_from_slice(&1u32.to_be_bytes());
    id[4..].copy_from_slice(&ALICE);
    id
}

fn put(state: &mut LedgerState, key: Hash256, v: &serde_json::Value) {
    state.state_map.insert(key, serde_json::to_vec(v).unwrap()).unwrap();
}

/// Mainnet's rules today, plus fixCleanup3_5_0 when `fix`.
fn rules(fix: bool) -> Vec<&'static str> {
    let mut ids = vec![am::FIX_CLEANUP_3_2_0, am::FIX_CLEANUP_3_3_0, am::FIX_CLEANUP_3_4_0, am::MPTOKENS_V1, FIX_MPT_DELIVERED_AMOUNT];
    if fix {
        ids.push(am::FIX_CLEANUP_3_5_0);
    }
    ids
}

/// ALICE's transferable issuance with `transfer_fee` (1/10 bps) and the
/// largest MaximumAmount; BOB holds `bob`, CAROL holds nothing.
fn state(transfer_fee: u64, bob: u64, fix: bool) -> LedgerState {
    let mut state = LedgerState::new_unverified(LedgerHeader {
        sequence: 100, total_coins: 100_000_000_000_000_000,
        parent_hash: Hash256([0; 32]), transaction_hash: Hash256([0; 32]), account_hash: Hash256([0; 32]),
        parent_close_time: 0, close_time: 10, close_time_resolution: 10, close_flags: 0,
    });
    put(&mut state, keylet::amendments_key(), &json!({"LedgerEntryType": "Amendments", "Flags": 0, "Amendments": rules(fix)}));
    for id in [ALICE, BOB, CAROL] {
        put(&mut state, keylet::account_root_key(&id), &json!({
            "LedgerEntryType": "AccountRoot", "Account": hex::encode(id), "Balance": "50000000",
            "Sequence": 1, "OwnerCount": 1, "Flags": 0,
        }));
    }
    let id = mptid();
    let issuance_key = keylet::mpt_issuance_key(&id);
    put(&mut state, issuance_key, &json!({
        "LedgerEntryType": "MPTokenIssuance", "Issuer": hex::encode(ALICE), "Flags": LSF_MPT_CAN_TRANSFER,
        "Sequence": 1, "OutstandingAmount": bob.to_string(), "OwnerNode": "0",
        "MaximumAmount": MAX_MPT_AMOUNT.to_string(), "TransferFee": transfer_fee,
    }));
    for (holder, amount) in [(BOB, bob), (CAROL, 0)] {
        put(&mut state, keylet::mptoken_key(&issuance_key, &holder), &json!({
            "LedgerEntryType": "MPToken", "Account": hex::encode(holder), "MPTokenIssuanceID": hex::encode_upper(id),
            "MPTAmount": amount.to_string(), "Flags": 0, "OwnerNode": "0",
        }));
    }
    state
}

#[derive(Debug, PartialEq, Eq)]
struct Paid {
    result: TxResult,
    bob: u64,
    carol: u64,
    outstanding: u64,
    /// The metadata's DeliveredAmount value, when the payment recorded one.
    delivered: Option<String>,
}

/// BOB pays CAROL `amount`, with `send_max` when given.
fn pay(state: &LedgerState, amount: u64, send_max: Option<u64>, partial: bool) -> Paid {
    let id = hex::encode_upper(mptid());
    let mut fields = json!({
        "Destination": hex::encode(CAROL),
        "Amount": {"mpt_issuance_id": id, "value": amount.to_string()},
        "Flags": if partial { TF_PARTIAL_PAYMENT } else { 0 },
    });
    if let Some(m) = send_max {
        fields["SendMax"] = json!({"mpt_issuance_id": id, "value": m.to_string()});
    }
    let tx = TxFields {
        account: BOB, tx_type: "Payment".to_string(), fee: 10, sequence: 1, ticket_seq: None, last_ledger_seq: None,
        fields, inner_batch: false,
    };
    let _ = crate::tx::take_delivered();
    let mut sb = Sandbox::new(state);
    let result = apply_mpt_payment(&tx, &mut sb, &CAROL, mptid(), amount, partial);
    let issuance_key = keylet::mpt_issuance_key(&mptid());
    let held = |holder: &[u8; 20]| json_at(&sb, &keylet::mptoken_key(&issuance_key, holder)).map(|t| dec_field(&t, "MPTAmount")).unwrap_or(0);
    let delivered = crate::tx::take_delivered().and_then(|d| d["value"].as_str().map(str::to_string));
    Paid {
        result,
        bob: held(&BOB),
        carol: held(&CAROL),
        outstanding: json_at(&sb, &issuance_key).map(|i| dec_field(&i, "OutstandingAmount")).unwrap_or(0),
        delivered,
    }
}

/// MPToken_test.cpp:3924-3945: a 0.1% fee. 3.4.1 charges 50 for 50 (50.05
/// to nearest); under the fix the cost is 51, past the implicit SendMax, and
/// the partial payment delivers 49 for 50.
#[test]
fn a_fee_on_a_holder_payment_rounds_up_under_fixcleanup350() {
    let old = state(100, 1_000, false);
    let p = pay(&old, 50, None, false);
    assert_eq!((p.result, p.bob, p.carol), (TxResult::Success, 950, 50));

    let fixed = state(100, 1_000, true);
    assert_eq!(pay(&fixed, 50, None, false).result, TxResult::PathPartial);
    let p = pay(&fixed, 50, None, true);
    assert_eq!((p.result, p.bob, p.carol), (TxResult::Success, 950, 49));
    assert_eq!(p.delivered.as_deref(), Some("49"));
}

/// MPToken_test.cpp:7847-7890 (testPartialPaymentRounding): a 10% fee, 100
/// asked for at most 90. 3.4.1: 90 / 1.1 to nearest is 82, which costs 90;
/// the fix: 81 (rounded down), which costs 89.1 rounded up — 90.
#[test]
fn a_partial_payment_delivers_the_floor_under_fixcleanup350() {
    let p = pay(&state(10_000, 780, false), 100, Some(90), true);
    assert_eq!((p.result, p.bob, p.carol), (TxResult::Success, 690, 82));
    let p = pay(&state(10_000, 780, true), 100, Some(90), true);
    assert_eq!((p.result, p.bob, p.carol), (TxResult::Success, 690, 81));
    assert_eq!(p.outstanding, 690 + 81, "the 9 of fee is redeemed to the issuer");
}

/// MPToken_test.cpp:7918-7966: SendMax 184472974760317629 at a 0.003% fee.
/// 3.4.1's divide reaches `muldiv(…) == UINT64_MAX − 2`, the `+ 5` wraps to
/// 2, and the delivery canonicalises to zero: tesSUCCESS, nothing moved, a
/// DeliveredAmount of 0. The fix delivers floor(SendMax / 1.00003) for all
/// of SendMax.
#[test]
fn a_partial_payment_whose_divide_wraps_delivers_nothing_before_fixcleanup350() {
    const SEND_MAX: u64 = 184_472_974_760_317_629;
    let p = pay(&state(3, SEND_MAX, false), SEND_MAX, Some(SEND_MAX), true);
    assert_eq!(
        p,
        Paid { result: TxResult::Success, bob: SEND_MAX, carol: 0, outstanding: SEND_MAX, delivered: Some("0".to_string()) }
    );
    const DELIVERED: u64 = 184_467_440_737_095_516;
    let p = pay(&state(3, SEND_MAX, true), SEND_MAX, Some(SEND_MAX), true);
    assert_eq!(
        p,
        Paid { result: TxResult::Success, bob: 0, carol: DELIVERED, outstanding: DELIVERED, delivered: Some(DELIVERED.to_string()) }
    );
}

/// MPToken_test.cpp:7968-8007: SendMax 2e17 — 3.4.1's `muldiv` itself passes
/// 64 bits and throws: tefEXCEPTION. The fix delivers 199994000179994600.
#[test]
fn a_partial_payment_whose_divide_overflows_throws_before_fixcleanup350() {
    const SEND_MAX: u64 = 200_000_000_000_000_000;
    assert_eq!(pay(&state(3, SEND_MAX, false), SEND_MAX, Some(SEND_MAX), true).result, TxResult::TefException);
    let p = pay(&state(3, SEND_MAX, true), SEND_MAX, Some(SEND_MAX), true);
    assert_eq!((p.result, p.carol), (TxResult::Success, 199_994_000_179_994_600));
    assert!(p.bob <= 1, "at most a rounding unit left: {}", p.bob);
    assert_eq!(p.outstanding, p.bob + p.carol);
}

/// MPToken_test.cpp:8009-8040: delivering the largest MPT amount at a fee
/// costs more than int64 holds. 3.4.1's `multiply` throws (tefEXCEPTION),
/// partial or not; under the fix the cost exceeds SendMax: tecPATH_PARTIAL,
/// and the partial payment delivers floor(1000 / 1.00003) = 999 for 1000.
#[test]
fn a_cost_past_int64_exceeds_sendmax_under_fixcleanup350() {
    let old = state(3, 1_000, false);
    assert_eq!(pay(&old, MAX_MPT_AMOUNT, Some(1_000), false).result, TxResult::TefException);
    assert_eq!(pay(&old, MAX_MPT_AMOUNT, Some(1_000), true).result, TxResult::TefException);

    let fixed = state(3, 1_000, true);
    let p = pay(&fixed, MAX_MPT_AMOUNT, Some(1_000), false);
    assert_eq!((p.result, p.bob, p.carol), (TxResult::PathPartial, 1_000, 0));
    let p = pay(&fixed, MAX_MPT_AMOUNT, Some(1_000), true);
    assert_eq!((p.result, p.bob, p.carol), (TxResult::Success, 0, 999));
}

/// MPToken_test.cpp:8042-8063: under the fix a partial payment that would
/// deliver nothing (SendMax 1 at a fee) is tecPATH_PARTIAL, not a success
/// that moves nothing.
#[test]
fn a_partial_payment_delivering_nothing_fails_under_fixcleanup350() {
    let p = pay(&state(3, 10, true), 10, Some(1), true);
    assert_eq!((p.result, p.bob, p.carol), (TxResult::PathPartial, 10, 0));
}

/// MPToken_test.cpp:8085-8160 (testPartialPaymentSendMax, Number's Small
/// scale): SendMax 122354741735175090 at 0.003%. 3.4.1 delivers ...999 (to
/// nearest) and its 16-digit cost overcharges SendMax by 10 (finding 370);
/// the fix charges exactly SendMax for floor(SendMax / rate).
#[test]
fn a_partial_payment_never_charges_past_sendmax_under_fixcleanup350() {
    const SEND_MAX: u64 = 122_354_741_735_175_090;
    let p = pay(&state(3, SEND_MAX + 1_000, false), SEND_MAX, Some(SEND_MAX), true);
    assert_eq!((p.result, p.bob, p.carol), (TxResult::Success, 1_000 - 10, 122_351_071_203_038_999));
    let p = pay(&state(3, SEND_MAX + 1_000, true), SEND_MAX, Some(SEND_MAX), true);
    assert_eq!((p.result, p.bob, p.carol), (TxResult::Success, 1_000, 122_351_071_203_038_998));
}
