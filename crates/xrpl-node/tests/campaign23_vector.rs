//! Campaign 23 (devnet, 2026-09-23) byte-exact vectors: Batch (BatchV1_1,
//! activation due on mainnet ~2026-10-09) combined with delegation, MPT, credentials,
//! tickets, AccountDelete and LedgerStateFix inners — findings 389-393.
//!
//! A Batch bundle carries every inner's id (`raw_inner_hashes`, RawTransactions
//! order) and the verdicts the ledger FILED (`filed_results`). Each inner the
//! engine attempted is threaded with its own id and judged exactly the way the
//! live shadow judges it (`pair_inner_verdicts`), so refused and never-attempted
//! inners pin too, not only the filed ones.
use serde_json::Value;
use std::collections::HashMap;
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

/// Hydrate the pre-state, apply the bundle's transaction, and pin its TER.
/// Returns the state as of BEFORE the apply (the threading stamps need it to
/// tell a real change from a write-back) together with the raw, unstamped
/// mutation map.
fn apply_bundle(bundle: &Value) -> (LedgerState, String, HashMap<Hash256, SandboxEntry>) {
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

    let txf = build_txfields(&bundle["tx"]).expect("txfields");
    let (ter, mods) = native_apply_one(&state, &txf);
    let want_ter = bundle["result"].as_str().unwrap_or("tesSUCCESS");
    assert_eq!(ter, want_ter, "mainnet recorded this transaction result");
    (state, ter, mods)
}

/// Every `expect` entry byte-for-byte against the stamped mutation map.
fn assert_expect(bundle: &Value, mods: &HashMap<Hash256, SandboxEntry>) {
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

fn run_c23_bundle(bundle_json: &str) {
    let bundle: Value = serde_json::from_str(bundle_json).unwrap();
    let (state, _ter, mut mods) = apply_bundle(&bundle);
    let pre = |k: &Hash256| state.state_map.lookup(k).map(|b| b.to_vec());
    let tx_hash = bundle["tx"]["hash"].as_str().unwrap();
    let seq = bundle["seq"].as_u64().unwrap() as u32;
    if bundle["tx"]["TransactionType"].as_str() != Some("Batch") {
        xrpl_ledger::ledger::threading::stamp_threading(&mut mods, &pre, tx_hash, seq);
        assert_expect(&bundle, &mods);
        return;
    }
    let ours = xrpl_ledger::tx::batch::take_inner_results();
    let touched = xrpl_ledger::tx::batch::take_inner_touched();
    let raw: Vec<String> = bundle["raw_inner_hashes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_uppercase())
        .collect();
    assert!(ours.len() <= raw.len() && touched.len() <= raw.len(), "more attempted inners than RawTransactions");
    xrpl_ledger::ledger::threading::stamp_batch_threading(&mut mods, &pre, tx_hash, seq, &raw[..touched.len()], &touched);
    let filed: HashMap<String, String> = bundle["filed_results"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.to_uppercase(), v.as_str().unwrap().to_string()))
        .collect();
    let aon = xrpl_node::native_apply::batch_all_or_nothing(&bundle["tx"]);
    for (i, id, want, mismatch) in xrpl_node::native_apply::pair_inner_verdicts(&raw, &ours, &filed, aon) {
        let got = ours.get(i).map(String::as_str).unwrap_or(xrpl_node::native_apply::INNER_NOT_ATTEMPTED);
        assert!(!mismatch, "inner {i} {id}: ours {got} vs ledger {want}");
    }
    assert_expect(&bundle, &mods);
}

/// Campaign 23 — 0-1 Cc sets regular key RK (F424E2F75AC1, SetRegularKey tesSUCCESS).
#[test]
fn c23_0_1_cc_sets_regular_key_rk_devnet_5533557() {
    run_c23_bundle(include_str!("vectors/c23_0_1_cc_sets_regular_key_rk_devnet_5533557.json"));
}

/// Campaign 23 — 0-2 M SignerList [B, Cc] quorum 2 (3861A7A4D253, SignerListSet tesSUCCESS).
#[test]
fn c23_0_2_m_signerlist_b_cc_quorum_2_devnet_5533559() {
    run_c23_bundle(include_str!("vectors/c23_0_2_m_signerlist_b_cc_quorum_2_devnet_5533559.json"));
}

/// Campaign 23 — 0-3 T arms AccountTxnID (asf 5) (F3AE1FAA582F, AccountSet tesSUCCESS).
#[test]
fn c23_0_3_t_arms_accounttxnid_asf_5_devnet_5533561() {
    run_c23_bundle(include_str!("vectors/c23_0_3_t_arms_accounttxnid_asf_5_devnet_5533561.json"));
}

/// Campaign 23 — 0-4 D sets DepositAuth (asf 9) (E599BAEFA832, AccountSet tesSUCCESS).
#[test]
fn c23_0_4_d_sets_depositauth_asf_9_devnet_5533563() {
    run_c23_bundle(include_str!("vectors/c23_0_4_d_sets_depositauth_asf_9_devnet_5533563.json"));
}

/// Campaign 23 — 0-5 I issues KYC credential to A (A92567257293, CredentialCreate tesSUCCESS).
#[test]
fn c23_0_5_i_issues_kyc_credential_to_a_devnet_5533565() {
    run_c23_bundle(include_str!("vectors/c23_0_5_i_issues_kyc_credential_to_a_devnet_5533565.json"));
}

/// Campaign 23 — 0-6 A accepts KYC (B841FC0E0349, CredentialAccept tesSUCCESS).
#[test]
fn c23_0_6_a_accepts_kyc_devnet_5533567() {
    run_c23_bundle(include_str!("vectors/c23_0_6_a_accepts_kyc_devnet_5533567.json"));
}

/// Campaign 23 — 0-7 D preauthorizes credential set [I, KYC] (A70EF5E7275C, DepositPreauth tesSUCCESS).
#[test]
fn c23_0_7_d_preauthorizes_credential_set_i_kyc_devnet_5533569() {
    run_c23_bundle(include_str!("vectors/c23_0_7_d_preauthorizes_credential_set_i_kyc_devnet_5533569.json"));
}

/// Campaign 23 — 0-8 A delegates 6 permissions to B (08AA426BAACF, DelegateSet tesSUCCESS).
#[test]
fn c23_0_8_a_delegates_6_permissions_to_b_devnet_5533571() {
    run_c23_bundle(include_str!("vectors/c23_0_8_a_delegates_6_permissions_to_b_devnet_5533571.json"));
}

/// Campaign 23 — 0-9 A creates 6 tickets (ACB1C43B9593, TicketCreate tesSUCCESS).
#[test]
fn c23_0_9_a_creates_6_tickets_devnet_5533573() {
    run_c23_bundle(include_str!("vectors/c23_0_9_a_creates_6_tickets_devnet_5533573.json"));
}

/// Campaign 23 — 1-allornothing control: A->Cc, A->H (456FA33BBAF4, Batch tesSUCCESS).
#[test]
fn c23_1_allornothing_control_a_cc_a_h_devnet_5533577() {
    run_c23_bundle(include_str!("vectors/c23_1_allornothing_control_a_cc_a_h_devnet_5533577.json"));
}

/// Campaign 23 — 1-onlyone control: A->Cc, A->H (DF3CE8B0BEFE, Batch tesSUCCESS).
#[test]
fn c23_1_onlyone_control_a_cc_a_h_devnet_5533579() {
    run_c23_bundle(include_str!("vectors/c23_1_onlyone_control_a_cc_a_h_devnet_5533579.json"));
}

/// Campaign 23 — 1-untilfailure control: A->Cc, A->H (83DF675EAE8A, Batch tesSUCCESS).
#[test]
fn c23_1_untilfailure_control_a_cc_a_h_devnet_5533581() {
    run_c23_bundle(include_str!("vectors/c23_1_untilfailure_control_a_cc_a_h_devnet_5533581.json"));
}

/// Campaign 23 — 1-independent control: A->Cc, A->H (BC30B68796A6, Batch tesSUCCESS).
#[test]
fn c23_1_independent_control_a_cc_a_h_devnet_5533583() {
    run_c23_bundle(include_str!("vectors/c23_1_independent_control_a_cc_a_h_devnet_5533583.json"));
}

/// Campaign 23 — 2-1 allornothing own + delegated(B) inner, B signs (2990DE269F57, Batch tesSUCCESS).
#[test]
fn c23_2_1_allornothing_own_delegated_b_inner_b_signs_devnet_5533592() {
    run_c23_bundle(include_str!("vectors/c23_2_1_allornothing_own_delegated_b_inner_b_signs_devnet_5533592.json"));
}

/// Campaign 23 — 2-2 independent outer = delegate B, no BatchSigners (21E049BB2084, Batch tesSUCCESS).
#[test]
fn c23_2_2_independent_outer_delegate_b_no_batchsigners_devnet_5533594() {
    run_c23_bundle(include_str!("vectors/c23_2_2_independent_outer_delegate_b_no_batchsigners_devnet_5533594.json"));
}

/// Campaign 23 — 2-3a allornothing delegated tec -> discard (D0A2B63837D7, Batch tesSUCCESS).
#[test]
fn c23_2_3a_allornothing_delegated_tec_discard_devnet_5533597() {
    run_c23_bundle(include_str!("vectors/c23_2_3a_allornothing_delegated_tec_discard_devnet_5533597.json"));
}

/// Campaign 23 — 2-3b onlyone delegated tec then delegated tes (EA8ACD3A8F02, Batch tesSUCCESS).
#[test]
fn c23_2_3b_onlyone_delegated_tec_then_delegated_tes_devnet_5533600() {
    run_c23_bundle(include_str!("vectors/c23_2_3b_onlyone_delegated_tec_then_delegated_tes_devnet_5533600.json"));
}

/// Campaign 23 — 2-3c untilfailure delegated tes, delegated tec, stop (BA9F4FA6036B, Batch tesSUCCESS).
#[test]
fn c23_2_3c_untilfailure_delegated_tes_delegated_tec_stop_devnet_5533602() {
    run_c23_bundle(include_str!("vectors/c23_2_3c_untilfailure_delegated_tes_delegated_tec_stop_devnet_5533602.json"));
}

/// Campaign 23 — 2-3d independent outer B: delegated tec + delegated tes (C7EF4DCB6662, Batch tesSUCCESS).
#[test]
fn c23_2_3d_independent_outer_b_delegated_tec_delegated_tes_devnet_5533604() {
    run_c23_bundle(include_str!("vectors/c23_2_3d_independent_outer_b_delegated_tec_delegated_tes_devnet_5533604.json"));
}

/// Campaign 23 — 2-4 independent outer B: delegated TrustSet + TicketCreate (CB9C35698A9A, Batch tesSUCCESS).
#[test]
fn c23_2_4_independent_outer_b_delegated_trustset_ticketcreate_devnet_5533606() {
    run_c23_bundle(include_str!("vectors/c23_2_4_independent_outer_b_delegated_trustset_ticketcreate_devnet_5533606.json"));
}

/// Finding 393 — 2-5 independent outer B: granular AccountDomainSet ok / EmailHash refused (484A78BF9771, Batch tesSUCCESS).
#[test]
fn c23_2_5_independent_outer_b_granular_accountdomainset_ok_emailha_devnet_5533608() {
    run_c23_bundle(include_str!("vectors/c23_2_5_independent_outer_b_granular_accountdomainset_ok_emailha_devnet_5533608.json"));
}

/// Finding 393 — 2-6a independent: H inner delegated to B with NO delegation (B01A4C8FB093, Batch tesSUCCESS).
#[test]
fn c23_2_6a_independent_h_inner_delegated_to_b_with_no_delegation_devnet_5533610() {
    run_c23_bundle(include_str!("vectors/c23_2_6a_independent_h_inner_delegated_to_b_with_no_delegation_devnet_5533610.json"));
}

/// Finding 393 — 2-6b allornothing: own + no-permission delegated (EA69BE1A0C39, Batch tesSUCCESS).
#[test]
fn c23_2_6b_allornothing_own_no_permission_delegated_devnet_5533613() {
    run_c23_bundle(include_str!("vectors/c23_2_6b_allornothing_own_no_permission_delegated_devnet_5533613.json"));
}

/// Finding 393 — 2-6c untilfailure: no-permission delegated first (6B03D4D45BC4, Batch tesSUCCESS).
#[test]
fn c23_2_6c_untilfailure_no_permission_delegated_first_devnet_5533616() {
    run_c23_bundle(include_str!("vectors/c23_2_6c_untilfailure_no_permission_delegated_first_devnet_5533616.json"));
}

/// Finding 393 — 2-6d onlyone: no-permission delegated, then own (09E9AAA23188, Batch tesSUCCESS).
#[test]
fn c23_2_6d_onlyone_no_permission_delegated_then_own_devnet_5533618() {
    run_c23_bundle(include_str!("vectors/c23_2_6d_onlyone_no_permission_delegated_then_own_devnet_5533618.json"));
}

/// Campaign 23 — 3-1 allornothing: DelegateSet Cc->B then Cc payment delegated to B (F767BCA0A122, Batch tesSUCCESS).
#[test]
fn c23_3_1_allornothing_delegateset_cc_b_then_cc_payment_delegated_devnet_5533621() {
    run_c23_bundle(include_str!("vectors/c23_3_1_allornothing_delegateset_cc_b_then_cc_payment_delegated_devnet_5533621.json"));
}

/// Campaign 23 — 3-2 independent outer B: uses Cc->B delegation from 3-1 (65AD48CB8DCD, Batch tesSUCCESS).
#[test]
fn c23_3_2_independent_outer_b_uses_cc_b_delegation_from_3_1_devnet_5533623() {
    run_c23_bundle(include_str!("vectors/c23_3_2_independent_outer_b_uses_cc_b_delegation_from_3_1_devnet_5533623.json"));
}

/// Campaign 23 — 3-3 independent: DelegateSet A->H mixed with a payment (4D6D8DAA104F, Batch tesSUCCESS).
#[test]
fn c23_3_3_independent_delegateset_a_h_mixed_with_a_payment_devnet_5533625() {
    run_c23_bundle(include_str!("vectors/c23_3_3_independent_delegateset_a_h_mixed_with_a_payment_devnet_5533625.json"));
}

/// Finding 393 — 3-4 independent: revoke Cc->B, then delegated payment, then own (same seq) (47CE523C740B, Batch tesSUCCESS).
#[test]
fn c23_3_4_independent_revoke_cc_b_then_delegated_payment_then_own_devnet_5533627() {
    run_c23_bundle(include_str!("vectors/c23_3_4_independent_revoke_cc_b_then_delegated_payment_then_own_devnet_5533627.json"));
}

/// Campaign 23 — 4-1 allornothing: MPTokenIssuanceCreate + holder Authorize + MPT payment (43BA268560B9, Batch tesSUCCESS).
#[test]
fn c23_4_1_allornothing_mptokenissuancecreate_holder_authorize_mpt_devnet_5533630() {
    run_c23_bundle(include_str!("vectors/c23_4_1_allornothing_mptokenissuancecreate_holder_authorize_mpt_devnet_5533630.json"));
}

/// Campaign 23 — 4-2 independent outer B: delegated MPTokenIssuanceCreate (A via B) + H authorizes (396155BF809A, Batch tesSUCCESS).
#[test]
fn c23_4_2_independent_outer_b_delegated_mptokenissuancecreate_a_vi_devnet_5533632() {
    run_c23_bundle(include_str!("vectors/c23_4_2_independent_outer_b_delegated_mptokenissuancecreate_a_vi_devnet_5533632.json"));
}

/// Campaign 23 — 4-3 independent: ticketed MPTokenIssuanceCreate + payment (013F89FBB6D5, Batch tesSUCCESS).
#[test]
fn c23_4_3_independent_ticketed_mptokenissuancecreate_payment_devnet_5533634() {
    run_c23_bundle(include_str!("vectors/c23_4_3_independent_ticketed_mptokenissuancecreate_payment_devnet_5533634.json"));
}

/// Campaign 23 — 4-4 untilfailure: MPT payment to a non-holder, then to H (1818C736A45F, Batch tesSUCCESS).
#[test]
fn c23_4_4_untilfailure_mpt_payment_to_a_non_holder_then_to_h_devnet_5533637() {
    run_c23_bundle(include_str!("vectors/c23_4_4_untilfailure_mpt_payment_to_a_non_holder_then_to_h_devnet_5533637.json"));
}

/// Campaign 23 — 5-0a independent: I issues 4 expiring credentials to A (4D05A69F0167, Batch tesSUCCESS).
#[test]
fn c23_5_0a_independent_i_issues_4_expiring_credentials_to_a_devnet_5533639() {
    run_c23_bundle(include_str!("vectors/c23_5_0a_independent_i_issues_4_expiring_credentials_to_a_devnet_5533639.json"));
}

/// Campaign 23 — 5-0b allornothing: A accepts all 4 (6927E438AEF3, Batch tesSUCCESS).
#[test]
fn c23_5_0b_allornothing_a_accepts_all_4_devnet_5533641() {
    run_c23_bundle(include_str!("vectors/c23_5_0b_allornothing_a_accepts_all_4_devnet_5533641.json"));
}

/// Campaign 23 — 5-1 independent: KYC pay, EXP1 pay (tecEXPIRED), plain pay (0A0D8E76C476, Batch tesSUCCESS).
#[test]
fn c23_5_1_independent_kyc_pay_exp1_pay_tecexpired_plain_pay_devnet_5533667() {
    run_c23_bundle(include_str!("vectors/c23_5_1_independent_kyc_pay_exp1_pay_tecexpired_plain_pay_devnet_5533667.json"));
}

/// Campaign 23 — 5-2 allornothing: KYC pay, EXP2 pay -> discard (EXP2 survives) (C5C1D482401B, Batch tesSUCCESS).
#[test]
fn c23_5_2_allornothing_kyc_pay_exp2_pay_discard_exp2_survives_devnet_5533669() {
    run_c23_bundle(include_str!("vectors/c23_5_2_allornothing_kyc_pay_exp2_pay_discard_exp2_survives_devnet_5533669.json"));
}

/// Campaign 23 — 5-3 untilfailure: EXP3 pay first (7ECB6FFFF4C8, Batch tesSUCCESS).
#[test]
fn c23_5_3_untilfailure_exp3_pay_first_devnet_5533671() {
    run_c23_bundle(include_str!("vectors/c23_5_3_untilfailure_exp3_pay_first_devnet_5533671.json"));
}

/// Campaign 23 — 5-4 onlyone: EXP4 pay, KYC pay, plain (F500AA3EB1C4, Batch tesSUCCESS).
#[test]
fn c23_5_4_onlyone_exp4_pay_kyc_pay_plain_devnet_5533673() {
    run_c23_bundle(include_str!("vectors/c23_5_4_onlyone_exp4_pay_kyc_pay_plain_devnet_5533673.json"));
}

/// Campaign 23 — 5-5 standalone EXP2 pay (survived 5-2) (7E7F5D06640B, Payment tecEXPIRED).
#[test]
fn c23_5_5_standalone_exp2_pay_survived_5_2_devnet_5533675() {
    run_c23_bundle(include_str!("vectors/c23_5_5_standalone_exp2_pay_survived_5_2_devnet_5533675.json"));
}

/// Campaign 23 — 5-6 independent: no-credential pay to DepositAuth D, then KYC pay (5226FCE4985D, Batch tesSUCCESS).
#[test]
fn c23_5_6_independent_no_credential_pay_to_depositauth_d_then_kyc_devnet_5533677() {
    run_c23_bundle(include_str!("vectors/c23_5_6_independent_no_credential_pay_to_depositauth_d_then_kyc_devnet_5533677.json"));
}

/// Campaign 23 — 6-1 allornothing: TicketCreate 2 then pay on its first ticket (BFE6E8761744, Batch tesSUCCESS).
#[test]
fn c23_6_1_allornothing_ticketcreate_2_then_pay_on_its_first_ticket_devnet_5533679() {
    run_c23_bundle(include_str!("vectors/c23_6_1_allornothing_ticketcreate_2_then_pay_on_its_first_ticket_devnet_5533679.json"));
}

/// Finding 389, 392 — 6-2 independent: TicketCreate 1, AccountSet on the jumped seq (tefPAST_SEQ), pay on the ticket (94A1543BE598, Batch tesSUCCESS).
#[test]
fn c23_6_2_independent_ticketcreate_1_accountset_on_the_jumped_seq_devnet_5533681() {
    run_c23_bundle(include_str!("vectors/c23_6_2_independent_ticketcreate_1_accountset_on_the_jumped_seq_devnet_5533681.json"));
}

/// Campaign 23 — 6-3 untilfailure: two existing tickets (BE066F7CB00C, Batch tesSUCCESS).
#[test]
fn c23_6_3_untilfailure_two_existing_tickets_devnet_5533683() {
    run_c23_bundle(include_str!("vectors/c23_6_3_untilfailure_two_existing_tickets_devnet_5533683.json"));
}

/// Campaign 23 — 6-4 independent: outer on a Ticket, inners on the sequence (345CFA58F176, Batch tesSUCCESS).
#[test]
fn c23_6_4_independent_outer_on_a_ticket_inners_on_the_sequence_devnet_5533685() {
    run_c23_bundle(include_str!("vectors/c23_6_4_independent_outer_on_a_ticket_inners_on_the_sequence_devnet_5533685.json"));
}

/// Finding 389 — 6-5 independent: AccountSet with a FUTURE seq (terPRE_SEQ), then pay (B302B7F73FA9, Batch tesSUCCESS).
#[test]
fn c23_6_5_independent_accountset_with_a_future_seq_terpre_seq_then_devnet_5533687() {
    run_c23_bundle(include_str!("vectors/c23_6_5_independent_accountset_with_a_future_seq_terpre_seq_then_devnet_5533687.json"));
}

/// Finding 389 — 6-6 independent: AccountSet on a missing Ticket (terPRE_TICKET), then pay (CF922992F12E, Batch tesSUCCESS).
#[test]
fn c23_6_6_independent_accountset_on_a_missing_ticket_terpre_ticket_devnet_5533690() {
    run_c23_bundle(include_str!("vectors/c23_6_6_independent_accountset_on_a_missing_ticket_terpre_ticket_devnet_5533690.json"));
}

/// Finding 389 — 6-7 independent: AccountSet with LastLedgerSequence in the past (tefMAX_LEDGER), then pay (DED9CFF3FBF7, Batch tesSUCCESS).
#[test]
fn c23_6_7_independent_accountset_with_lastledgersequence_in_the_pa_devnet_5533692() {
    run_c23_bundle(include_str!("vectors/c23_6_7_independent_accountset_with_lastledgersequence_in_the_pa_devnet_5533692.json"));
}

/// Finding 390 — 6-8 allornothing: AccountTxnID-armed T, two inners (stamp = last inner id) (32095E9BE770, Batch tesSUCCESS).
#[test]
fn c23_6_8_allornothing_accounttxnid_armed_t_two_inners_stamp_last_devnet_5533694() {
    run_c23_bundle(include_str!("vectors/c23_6_8_allornothing_accounttxnid_armed_t_two_inners_stamp_last_devnet_5533694.json"));
}

/// Finding 389, 390 — 6-9 independent: inner with a wrong AccountTxnID prior (tefWRONG_PRIOR), then pay (B2757646E328, Batch tesSUCCESS).
#[test]
fn c23_6_9_independent_inner_with_a_wrong_accounttxnid_prior_tefwro_devnet_5533696() {
    run_c23_bundle(include_str!("vectors/c23_6_9_independent_inner_with_a_wrong_accounttxnid_prior_tefwro_devnet_5533696.json"));
}

/// Campaign 23 — 7-1 independent: LedgerStateFix NFTokenPageLink (nothing to fix) + pay (381D28A34B22, Batch tesSUCCESS).
#[test]
fn c23_7_1_independent_ledgerstatefix_nftokenpagelink_nothing_to_fi_devnet_5533699() {
    run_c23_bundle(include_str!("vectors/c23_7_1_independent_ledgerstatefix_nftokenpagelink_nothing_to_fi_devnet_5533699.json"));
}

/// Campaign 23 — 7-2 allornothing: pay + LedgerStateFix tec -> discard (10370C051771, Batch tesSUCCESS).
#[test]
fn c23_7_2_allornothing_pay_ledgerstatefix_tec_discard_devnet_5533701() {
    run_c23_bundle(include_str!("vectors/c23_7_2_allornothing_pay_ledgerstatefix_tec_discard_devnet_5533701.json"));
}

/// Campaign 23 — 7-3 untilfailure outer B: delegated LedgerStateFix tec, stop (C84A8C28A0C8, Batch tesSUCCESS).
#[test]
fn c23_7_3_untilfailure_outer_b_delegated_ledgerstatefix_tec_stop_devnet_5533703() {
    run_c23_bundle(include_str!("vectors/c23_7_3_untilfailure_outer_b_delegated_ledgerstatefix_tec_stop_devnet_5533703.json"));
}

/// Campaign 23 — 7-4 onlyone: LedgerStateFix BookExchangeRate on a missing dir, pay, pay (A63E0D8C4249, Batch tesSUCCESS).
#[test]
fn c23_7_4_onlyone_ledgerstatefix_bookexchangerate_on_a_missing_dir_devnet_5533705() {
    run_c23_bundle(include_str!("vectors/c23_7_4_onlyone_ledgerstatefix_bookexchangerate_on_a_missing_dir_devnet_5533705.json"));
}

/// Campaign 23 — 8-1 allornothing: Cc inner, Cc signs with its REGULAR key (F9027ACC827E, Batch tesSUCCESS).
#[test]
fn c23_8_1_allornothing_cc_inner_cc_signs_with_its_regular_key_devnet_5533707() {
    run_c23_bundle(include_str!("vectors/c23_8_1_allornothing_cc_inner_cc_signs_with_its_regular_key_devnet_5533707.json"));
}

/// Campaign 23 — 8-2 independent: M inner, M multi-signed by B and Cc (48FEE1B25BCB, Batch tesSUCCESS).
#[test]
fn c23_8_2_independent_m_inner_m_multi_signed_by_b_and_cc_devnet_5533709() {
    run_c23_bundle(include_str!("vectors/c23_8_2_independent_m_inner_m_multi_signed_by_b_and_cc_devnet_5533709.json"));
}

/// Finding 391 — 9-1 independent: X AccountDelete, X pay after (terNO_ACCOUNT), A pay (178D9FEB415E, Batch tesSUCCESS).
#[test]
fn c23_9_1_independent_x_accountdelete_x_pay_after_terno_account_a_devnet_5533809() {
    run_c23_bundle(include_str!("vectors/c23_9_1_independent_x_accountdelete_x_pay_after_terno_account_a_devnet_5533809.json"));
}

/// Finding 391 — 9-2 independent: outer Y deletes ITSELF in inner 1, A pays in inner 2 (92A328919BA7, Batch tesSUCCESS).
#[test]
fn c23_9_2_independent_outer_y_deletes_itself_in_inner_1_a_pays_in_devnet_5533811() {
    run_c23_bundle(include_str!("vectors/c23_9_2_independent_outer_y_deletes_itself_in_inner_1_a_pays_in_devnet_5533811.json"));
}

/// Finding 391 — 9-3 allornothing: W AccountDelete + A pay (5F651E0870B7, Batch tesSUCCESS).
#[test]
fn c23_9_3_allornothing_w_accountdelete_a_pay_devnet_5533813() {
    run_c23_bundle(include_str!("vectors/c23_9_3_allornothing_w_accountdelete_a_pay_devnet_5533813.json"));
}

/// Campaign 23 — 8-3b independent: A funds new E, E acts at Sequence=5533818 (attempt 0) (9C3CB4955342, Batch tesSUCCESS).
#[test]
fn c23_8_3b_independent_a_funds_new_e_e_acts_at_sequence_5533818_att_devnet_5533818() {
    run_c23_bundle(include_str!("vectors/c23_8_3b_independent_a_funds_new_e_e_acts_at_sequence_5533818_att_devnet_5533818.json"));
}
