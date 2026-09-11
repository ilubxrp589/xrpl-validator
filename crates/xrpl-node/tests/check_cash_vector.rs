//! Check byte-exact vectors (bundle-driven) — CheckCash.
//!
//! Same harness as deposit_preauth_vector.rs: hydrate the bundle's pre-state,
//! apply the one transaction natively, compare the result code and every
//! target object byte for byte against mainnet's post-state.
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
    let want_ter = bundle["result"].as_str().unwrap_or("tesSUCCESS");
    assert_eq!(ter, want_ter, "mainnet result for this transaction");

    xrpl_ledger::ledger::threading::stamp_threading(
        &mut mods,
        &|k| state.state_map.lookup(k).map(|b| b.to_vec()),
        tx_hash,
        seq,
    );

    for (k, want_hex) in bundle["expect"].as_object().unwrap() {
        // An EMPTY expectation is a deletion pin (finding 158): mainnet's
        // meta deleted the object in this transaction, so must the apply.
        let want_deleted = want_hex.as_str().unwrap().trim().is_empty();
        // An expectation the apply did not write is legitimate only when it
        // pins an object the transaction must leave ALONE: its post-image is
        // then its pre-image, and the vector says so by expecting exactly
        // the seated bytes (finding 143 — the taker's own bid beyond the
        // ask's limit, which mainnet never names).
        let Some(ent) = mods.get(&key32(k)) else {
            assert!(!want_deleted, "target {k} must be deleted by the apply, which never wrote it");
            let pre_hex = bundle["pre"][k].as_str().unwrap_or_default().trim().to_uppercase();
            assert_eq!(
                want_hex.as_str().unwrap().trim().to_uppercase(),
                pre_hex,
                "target {k} was not written by the apply and does not pin the untouched pre-image"
            );
            continue;
        };
        let bytes = match ent {
            SandboxEntry::Created(b) | SandboxEntry::Modified(b) => {
                assert!(!want_deleted, "target {k} must be deleted by the apply, which wrote it instead");
                b.clone()
            }
            SandboxEntry::Deleted => {
                assert!(want_deleted, "target {k} deleted?");
                continue;
            }
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

/// F59's regression guard (#106693003 E9919AA2, the path-TER flagship): a
/// tfPartialPayment self-payment buying BCHAMP with 1485.4 XRP of SendMax,
/// DeliverMin 737956.89. Mainnet fills it through 33 book offers interleaved
/// with 13 AMM slices — 46 driver iterations — spending the whole SendMax
/// and delivering 763166.81. The driver here was capped at 32 rounds (the
/// old multi-strand interleave cap, kept for lone strands in 4566c4e): round
/// 32 left 497 XRP unspent and 510958.66 delivered, under DeliverMin —
/// tecPATH_PARTIAL against mainnet's tesSUCCESS, the live shadow's
/// ter-mismatch signature. rippled's loop runs on its remainders alone
/// (maxTries = 1000 is a failure bound, not a fill count).
/// #106708057 CEDC31F6 (finding 92): a CheckCreate whose XAU SendMax is
/// frozen — CheckCreate::preclaim refuses a globally frozen currency, a
/// source line frozen by the issuer, or a destination line the destination
/// froze, with tecFROZEN. We had no freeze block at all and created the
/// Check plus its two directory entries where mainnet claims the fee alone.
/// Finding 132 (#106737708 2EDFEDC7): rhWt2bhR authorizes r4H6GW2J. rippled's
/// `DepositPreauth::doApply` inserts the new entry into the owner directory
/// and records the page in `OwnerNode`; the entry carries `Flags` zero like
/// every ledger entry. We wrote it bare and never touched the directory —
/// mainnet's page 7D05A31B read unwritten and the object serialized two
/// fields short. Three targets byte-pinned.
// Finding 152 — #106744882 0BFD2FD7C45E (rJPSUEG8 cashes 8B269A63 for 25
// FLL): the check's Expiration is behind the parent close time, so mainnet
// returns tecEXPIRED and changes nothing but the fee. We cashed it.
#[test]
fn check_cash_of_an_expired_check_is_tec_expired() {
    run_bundle(include_str!("vectors/check_cash_expired_is_tec_expired_106744882.json"));
}

// Finding 234 — #106849342 0B8830B868D4 (rhxXuUBjYo cashes rHKatUKdi's 1 EVR
// check, issuer ra9g3LAJ with TransferRate 1.002, DeliverMin 0.998): rippled
// cashes through the payment engine — the writer pays the whole SendMax
// through the issuer, fee included, and DeliverMin is only a floor checked
// afterwards. Mainnet debits 1 EVR and credits the casher's new line
// 0.9980039920159681; we moved 0.998 fee-free. Both lines byte-pinned.
#[test]
fn check_cash_with_deliver_min_takes_everything_send_max_buys_106849342() {
    run_bundle(include_str!("vectors/check_cash_with_deliver_min_takes_everything_send_max_buys_106849342.json"));
}

/// Finding 241 (#106868992 8ADC7115B92C): rEfFcvUUe3 cashes a 50 XRP check
/// whose WRITER cannot cover it. rippled refuses in preclaim —
/// availableFunds = accountFunds(check.Account, value) plus one reserve
/// increment back for an unsponsored XRP check, and `value > availableFunds`
/// is tecPATH_PARTIAL (CheckCash.cpp:169-192) — so the apply path's
/// tecUNFUNDED_PAYMENT (:353) is never reached. F234 rewrote the apply path
/// faithfully but dropped that gate, and every writer shortfall answered with
/// the wrong tec: 292 of them rode through cycle 108's windows unnoticed
/// because a wrong result code writes identical state.
#[test]
fn check_cash_writer_shortfall_is_path_partial_in_preclaim_106868992() {
    run_bundle(include_str!("vectors/check_cash_writer_shortfall_is_path_partial_in_preclaim_106868992.json"));
}

/// Finding 246 (#106900377 A252917DB8E8): rpcjmdWFCQ — neither writer nor
/// destination — cancels rDoxEVqFy3's check 3FF6163A to rJdFnb1nif after
/// its Expiration 842398366 (parent close 842398501). An expired check may
/// be cancelled by anyone (CancelCheck.cpp:56-66): the check, both directory
/// links and the writer's reserve unit go; we refused with tecNO_PERMISSION.
#[test]
fn check_cancel_of_an_expired_check_by_a_stranger_106900377() {
    run_bundle(include_str!("vectors/check_cancel_of_an_expired_check_by_a_stranger_106900377.json"));
}
