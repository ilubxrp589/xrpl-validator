//! Byte-exact vector drills for payment channels (2026-09-02).
//!
//! Each test replays one mainnet transaction against its same-ledger
//! pre-images and compares every touched object byte-for-byte with the
//! ledger, honouring the recorded TransactionResult.
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
    assert_eq!(ter, want_ter, "mainnet recorded this transaction result");

    xrpl_ledger::ledger::threading::stamp_threading(
        &mut mods,
        &|k| state.state_map.lookup(k).map(|b| b.to_vec()),
        tx_hash,
        seq,
    );

    for (k, want_hex) in bundle["expect"].as_object().unwrap() {
        let want_deleted = want_hex.as_str().unwrap().trim().is_empty();
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

/// Finding 85 — #106703535 F44C919F: a PaymentChannelCreate of 1000 drops
/// by an account under its next reserve; rippled's preclaim reserves for
/// OwnerCount + 1 and refuses with tecINSUFFICIENT_RESERVE.
#[test]
fn payment_channel_create_reserves_for_the_new_object() {
    run_bundle(include_str!("vectors/paychan_create_reserve_plus_one_106703535.json"));
}

/// Campaign 14 (testnet PayChan / Check / DepositAuth depth, 2026-09-21) 1-3: t2 funds t1's channel.
/// Testnet's verdict, byte-exact.
#[test]
fn paychan_c14_fund_by_a_non_owner_is_tec_no_permission_14c21a0999ab() {
    run_bundle(include_str!("vectors/paychan_c14_fund_by_a_non_owner_is_tec_no_permission_14C21A0999AB.json"));
}

/// Campaign 14 (testnet PayChan / Check / DepositAuth depth, 2026-09-21) 1-4: t2 claims 2 XRP against t1's signed claim.
/// Testnet's verdict, byte-exact.
#[test]
fn paychan_c14_claim_with_the_sources_signature_8b6122849541() {
    run_bundle(include_str!("vectors/paychan_c14_claim_with_the_sources_signature_8B6122849541.json"));
}

/// Campaign 14 (testnet PayChan / Check / DepositAuth depth, 2026-09-21) 1-6: t2 re-claims the same 2 XRP.
/// Testnet's verdict, byte-exact.
#[test]
fn paychan_c14_claim_not_above_the_current_balance_is_tec_unfunded_payment_a0fc4598a5bb() {
    run_bundle(include_str!("vectors/paychan_c14_claim_not_above_the_current_balance_is_tec_unfunded_payment_A0FC4598A5BB.json"));
}

/// Campaign 14 (testnet PayChan / Check / DepositAuth depth, 2026-09-21) 1-7: t3 claims with a valid signature.
/// Testnet's verdict, byte-exact.
#[test]
fn paychan_c14_claim_by_a_stranger_is_tec_no_permission_a80b87fd144c() {
    run_bundle(include_str!("vectors/paychan_c14_claim_by_a_stranger_is_tec_no_permission_A80B87FD144C.json"));
}

/// Campaign 14 (testnet PayChan / Check / DepositAuth depth, 2026-09-21) 1-8: t1 (source) raises Balance to 3 XRP.
/// Testnet's verdict, byte-exact.
#[test]
fn paychan_c14_source_sets_balance_without_a_signature_75b5c53a9133() {
    run_bundle(include_str!("vectors/paychan_c14_source_sets_balance_without_a_signature_75B5C53A9133.json"));
}

/// Campaign 14 (testnet PayChan / Check / DepositAuth depth, 2026-09-21) 1-9: t2 claims 9 XRP of a 6 XRP channel.
/// Testnet's verdict, byte-exact.
#[test]
fn paychan_c14_claim_over_the_channel_amount_is_tec_unfunded_payment_0b5b1e2ef840() {
    run_bundle(include_str!("vectors/paychan_c14_claim_over_the_channel_amount_is_tec_unfunded_payment_0B5B1E2EF840.json"));
}

/// Campaign 14 (testnet PayChan / Check / DepositAuth depth, 2026-09-21) 1-10: t3 sends tfClose.
/// Testnet's verdict, byte-exact.
#[test]
fn paychan_c14_close_by_a_stranger_is_tec_no_permission_1f93ff7d660d() {
    run_bundle(include_str!("vectors/paychan_c14_close_by_a_stranger_is_tec_no_permission_1F93FF7D660D.json"));
}

/// Campaign 14 (testnet PayChan / Check / DepositAuth depth, 2026-09-21) 1-11: t2 (dest) tfClose deletes the channel.
/// Testnet's verdict, byte-exact.
#[test]
fn paychan_c14_destination_close_returns_the_remainder_0be8db6ba5d6() {
    run_bundle(include_str!("vectors/paychan_c14_destination_close_returns_the_remainder_0BE8DB6BA5D6.json"));
}

/// Campaign 14 (testnet PayChan / Check / DepositAuth depth, 2026-09-21) 1-12: t2 claims after the close.
/// Testnet's verdict, byte-exact.
#[test]
fn paychan_c14_claim_on_a_deleted_channel_is_tec_no_target_58352e1c16d3() {
    run_bundle(include_str!("vectors/paychan_c14_claim_on_a_deleted_channel_is_tec_no_target_58352E1C16D3.json"));
}

/// Campaign 14 (testnet PayChan / Check / DepositAuth depth, 2026-09-21) 1-14: t1 tfClose = Expiration now + SettleDelay.
/// Testnet's verdict, byte-exact.
#[test]
fn paychan_c14_source_close_sets_expiration_9b555f269a77() {
    run_bundle(include_str!("vectors/paychan_c14_source_close_sets_expiration_9B555F269A77.json"));
}

/// Campaign 14 (testnet PayChan / Check / DepositAuth depth, 2026-09-21) 1-16: t1 tfRenew.
/// Testnet's verdict, byte-exact.
#[test]
fn paychan_c14_source_renew_clears_expiration_2c6db700b8b2() {
    run_bundle(include_str!("vectors/paychan_c14_source_renew_clears_expiration_2C6DB700B8B2.json"));
}

/// Campaign 14 (testnet PayChan / Check / DepositAuth depth, 2026-09-21) 1-18: t3 tfClose on an expired channel.
/// Testnet's verdict, byte-exact.
#[test]
fn paychan_c14_stranger_removes_an_expired_channel_e0b6f0dda163() {
    run_bundle(include_str!("vectors/paychan_c14_stranger_removes_an_expired_channel_E0B6F0DDA163.json"));
}

/// Campaign 14 (testnet PayChan / Check / DepositAuth depth, 2026-09-21) 1-21: t1 pushes 0.4 XRP into t3 under DepositAuth.
/// Testnet's verdict, byte-exact.
#[test]
fn paychan_c14_claim_into_a_deposit_auth_destination_needs_preauth_37014f883c6c() {
    run_bundle(include_str!("vectors/paychan_c14_claim_into_a_deposit_auth_destination_needs_preauth_37014F883C6C.json"));
}

/// Campaign 14 (testnet PayChan / Check / DepositAuth depth, 2026-09-21) 1-22: t3 claims the same signed amount.
/// Testnet's verdict, byte-exact.
#[test]
fn paychan_c14_destination_claims_itself_under_deposit_auth_1c6126424ffd() {
    run_bundle(include_str!("vectors/paychan_c14_destination_claims_itself_under_deposit_auth_1C6126424FFD.json"));
}

/// Campaign 14 (testnet PayChan / Check / DepositAuth depth, 2026-09-21) 1-26: t1 opens a channel to t2 without a tag.
/// Testnet's verdict, byte-exact.
#[test]
fn paychan_c14_create_to_a_require_dest_account_is_tec_dst_tag_needed_3117c75b84b6() {
    run_bundle(include_str!("vectors/paychan_c14_create_to_a_require_dest_account_is_tec_dst_tag_needed_3117C75B84B6.json"));
}

/// Campaign 14 (testnet PayChan / Check / DepositAuth depth, 2026-09-21) 3-10b: t2 opens a channel to t3 under DepositAuth (allowed).
/// Testnet's verdict, byte-exact.
#[test]
fn paychan_c14_create_to_a_deposit_auth_destination_9638598b44ac() {
    run_bundle(include_str!("vectors/paychan_c14_create_to_a_deposit_auth_destination_9638598B44AC.json"));
}
