//! Campaign 22 (devnet, 2026-09-23) byte-exact vectors — Finding 360,
//! PermissionDelegationV1_1 (majority on mainnet 2026-09-21, active ~10-05):
//! DelegateSet create / replace / delete / refusals; delegated Payment (XRP,
//! IOU, tec, Ticket), TrustSet, TicketCreate, DepositPreauth — the DELEGATE
//! pays the fee, the ACCOUNT's sequence moves; the pre-fee reserve boundary;
//! AccountDelete of a delegate and of a delegator. The live engine (7c9fde0)
//! failed 22 of these 29. Same harness as did_vector.rs.
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

/// Finding 360 — 0-1 I sets DefaultRipple (0DFCE52F57E9, AccountSet tesSUCCESS).
#[test]
fn c22_i_sets_defaultripple_devnet_5531448() {
    run_bundle(include_str!("vectors/c22_i_sets_defaultripple_devnet_5531448.json"));
}

/// Finding 360 — 1-1 A delegates 6 tx types to B (5F65FF0E3CCC, DelegateSet tesSUCCESS).
#[test]
fn c22_a_delegates_6_tx_types_to_b_devnet_5531450() {
    run_bundle(include_str!("vectors/c22_a_delegates_6_tx_types_to_b_devnet_5531450.json"));
}

/// Finding 360 — 1-2 A replaces the set with 4 types (70DDBE8EFD41, DelegateSet tesSUCCESS).
#[test]
fn c22_a_replaces_the_set_with_4_types_devnet_5531452() {
    run_bundle(include_str!("vectors/c22_a_replaces_the_set_with_4_types_devnet_5531452.json"));
}

/// Finding 360 — 1-3 B pays 1 XRP from A to Cd (delegated) (5AD846E52F11, Payment tesSUCCESS, delegated).
#[test]
fn c22_b_pays_1_xrp_from_a_to_cd_delegated_devnet_5531454() {
    run_bundle(include_str!("vectors/c22_b_pays_1_xrp_from_a_to_cd_delegated_devnet_5531454.json"));
}

/// Finding 360 — 1-4 B pays 1e6 XRP from A (delegated, unfunded) (4D3A80BD0FA4, Payment tecUNFUNDED_PAYMENT, delegated).
#[test]
fn c22_b_pays_1e6_xrp_from_a_delegated_unfunded_devnet_5531456() {
    run_bundle(include_str!("vectors/c22_b_pays_1e6_xrp_from_a_delegated_unfunded_devnet_5531456.json"));
}

/// Finding 360 — 1-5 B makes A trust I for USD (delegated) (DC1E8AB1B75D, TrustSet tesSUCCESS, delegated).
#[test]
fn c22_b_makes_a_trust_i_for_usd_delegated_devnet_5531458() {
    run_bundle(include_str!("vectors/c22_b_makes_a_trust_i_for_usd_delegated_devnet_5531458.json"));
}

/// Finding 360 — 1-6 Cd trusts I for USD (508CC89A558C, TrustSet tesSUCCESS).
#[test]
fn c22_cd_trusts_i_for_usd_devnet_5531460() {
    run_bundle(include_str!("vectors/c22_cd_trusts_i_for_usd_devnet_5531460.json"));
}

/// Finding 360 — 1-7 I issues 50 USD to A (2A83C425CCF6, Payment tesSUCCESS).
#[test]
fn c22_i_issues_50_usd_to_a_devnet_5531462() {
    run_bundle(include_str!("vectors/c22_i_issues_50_usd_to_a_devnet_5531462.json"));
}

/// Finding 360 — 1-8 B pays 5 USD from A to Cd (delegated IOU) (0E66E90DCE94, Payment tesSUCCESS, delegated).
#[test]
fn c22_b_pays_5_usd_from_a_to_cd_delegated_iou_devnet_5531464() {
    run_bundle(include_str!("vectors/c22_b_pays_5_usd_from_a_to_cd_delegated_iou_devnet_5531464.json"));
}

/// Finding 360 — 1-9 B creates 2 tickets for A (delegated) (36245A405A35, TicketCreate tesSUCCESS, delegated).
#[test]
fn c22_b_creates_2_tickets_for_a_delegated_devnet_5531466() {
    run_bundle(include_str!("vectors/c22_b_creates_2_tickets_for_a_delegated_devnet_5531466.json"));
}

/// Finding 360 — 1-10 B pays 1 XRP from A on a Ticket (delegated) (783CCB44D42B, Payment tesSUCCESS, delegated).
#[test]
fn c22_b_pays_1_xrp_from_a_on_a_ticket_delegated_devnet_5531468() {
    run_bundle(include_str!("vectors/c22_b_pays_1_xrp_from_a_on_a_ticket_delegated_devnet_5531468.json"));
}

/// Finding 360 — 1-11 B preauthorizes Cd for A (delegated DepositPreauth) (82D6A9952BB1, DepositPreauth tesSUCCESS, delegated).
#[test]
fn c22_b_preauthorizes_cd_for_a_delegated_depositpreauth_devnet_5531470() {
    run_bundle(include_str!("vectors/c22_b_preauthorizes_cd_for_a_delegated_depositpreauth_devnet_5531470.json"));
}

/// Finding 360 — 1-12 A delegates to an account that does not exist (5A48FF34A556, DelegateSet tecNO_TARGET).
#[test]
fn c22_a_delegates_to_an_account_that_does_not_exist_devnet_5531472() {
    run_bundle(include_str!("vectors/c22_a_delegates_to_an_account_that_does_not_exist_devnet_5531472.json"));
}

/// Finding 360 — 1-13 A withdraws a delegation that does not exist (700418E072F3, DelegateSet tecNO_ENTRY).
#[test]
fn c22_a_withdraws_a_delegation_that_does_not_exist_devnet_5531474() {
    run_bundle(include_str!("vectors/c22_a_withdraws_a_delegation_that_does_not_exist_devnet_5531474.json"));
}

/// Finding 360 — 1-14 I creates an XRP/USD AMM (83F4F49E4AC8, AMMCreate tesSUCCESS).
#[test]
fn c22_i_creates_an_xrp_usd_amm_devnet_5531476() {
    run_bundle(include_str!("vectors/c22_i_creates_an_xrp_usd_amm_devnet_5531476.json"));
}

/// Finding 360 — 1-15 A delegates to the AMM pseudo-account (163A5564F4A8, DelegateSet tecPSEUDO_ACCOUNT).
#[test]
fn c22_a_delegates_to_the_amm_pseudo_account_devnet_5531478() {
    run_bundle(include_str!("vectors/c22_a_delegates_to_the_amm_pseudo_account_devnet_5531478.json"));
}

/// Finding 360 — 2-1 E delegates DepositPreauth to B (5092783DE632, DelegateSet tesSUCCESS).
#[test]
fn c22_e_delegates_depositpreauth_to_b_devnet_5531480() {
    run_bundle(include_str!("vectors/c22_e_delegates_depositpreauth_to_b_devnet_5531480.json"));
}

/// Finding 360 — 2-2 E sends away all but 1399999 drops (18C14D328638, Payment tesSUCCESS).
#[test]
fn c22_e_sends_away_all_but_1399999_drops_devnet_5531482() {
    run_bundle(include_str!("vectors/c22_e_sends_away_all_but_1399999_drops_devnet_5531482.json"));
}

/// Finding 360 — 2-3 B preauthorizes Cd for E one drop under reserve (delegated) (423E594DB90F, DepositPreauth tecINSUFFICIENT_RESERVE, delegated).
#[test]
fn c22_b_preauthorizes_cd_for_e_one_drop_under_reserve_delegated_devnet_5531484() {
    run_bundle(include_str!("vectors/c22_b_preauthorizes_cd_for_e_one_drop_under_reserve_delegated_devnet_5531484.json"));
}

/// Finding 360 — 2-4 Cd tops E up by 1 drop (B435A5C2C4B5, Payment tesSUCCESS).
#[test]
fn c22_cd_tops_e_up_by_1_drop_devnet_5531486() {
    run_bundle(include_str!("vectors/c22_cd_tops_e_up_by_1_drop_devnet_5531486.json"));
}

/// Finding 360 — 2-5 B preauthorizes Cd for E, E exactly at reserve (delegated) (5D7565A2C299, DepositPreauth tesSUCCESS, delegated).
#[test]
fn c22_b_preauthorizes_cd_for_e_e_exactly_at_reserve_delegated_devnet_5531488() {
    run_bundle(include_str!("vectors/c22_b_preauthorizes_cd_for_e_e_exactly_at_reserve_delegated_devnet_5531488.json"));
}

/// Finding 360 — 2-6 E new DelegateSet below the next reserve (967EDB008C61, DelegateSet tecINSUFFICIENT_RESERVE).
#[test]
fn c22_e_new_delegateset_below_the_next_reserve_devnet_5531490() {
    run_bundle(include_str!("vectors/c22_e_new_delegateset_below_the_next_reserve_devnet_5531490.json"));
}

/// Finding 360 — 3-1 A withdraws its delegation to B (14535F841D2E, DelegateSet tesSUCCESS).
#[test]
fn c22_a_withdraws_its_delegation_to_b_devnet_5531492() {
    run_bundle(include_str!("vectors/c22_a_withdraws_its_delegation_to_b_devnet_5531492.json"));
}

/// Finding 360 — 3-2 A withdraws its delegation to B again (3C943AA176FC, DelegateSet tecNO_ENTRY).
#[test]
fn c22_a_withdraws_its_delegation_to_b_again_devnet_5531494() {
    run_bundle(include_str!("vectors/c22_a_withdraws_its_delegation_to_b_again_devnet_5531494.json"));
}

/// Finding 360 — 3-3 F delegates Payment to G (DFF8FAFC44A4, DelegateSet tesSUCCESS).
#[test]
fn c22_f_delegates_payment_to_g_devnet_5531496() {
    run_bundle(include_str!("vectors/c22_f_delegates_payment_to_g_devnet_5531496.json"));
}

/// Finding 360 — 3-4 A delegates Payment to G (5EE1FEF35498, DelegateSet tesSUCCESS).
#[test]
fn c22_a_delegates_payment_to_g_devnet_5531498() {
    run_bundle(include_str!("vectors/c22_a_delegates_payment_to_g_devnet_5531498.json"));
}

/// Finding 360 — 3-5 F delegates Payment to B (BB14A19C1848, DelegateSet tesSUCCESS).
#[test]
fn c22_f_delegates_payment_to_b_devnet_5531500() {
    run_bundle(include_str!("vectors/c22_f_delegates_payment_to_b_devnet_5531500.json"));
}

/// Finding 360 — 4-1 G (delegate of A and F) deletes itself (F8575162A2AA, AccountDelete tesSUCCESS).
#[test]
fn c22_g_delegate_of_a_and_f_deletes_itself_devnet_5531712() {
    run_bundle(include_str!("vectors/c22_g_delegate_of_a_and_f_deletes_itself_devnet_5531712.json"));
}

/// Finding 360 — 4-2 F (delegator to B) deletes itself (3DD7525B1B4B, AccountDelete tesSUCCESS).
#[test]
fn c22_f_delegator_to_b_deletes_itself_devnet_5531714() {
    run_bundle(include_str!("vectors/c22_f_delegator_to_b_deletes_itself_devnet_5531714.json"));
}
