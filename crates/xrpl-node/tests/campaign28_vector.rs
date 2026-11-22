//! Campaign 28 (devnet, 2026-11-22, ledgers 7155359–7156485) byte-exact vectors: the LendingProtocol transactors on
//! real ledgers. 571 transactions were submitted (cap 600), 508 reached a ledger, 406 of them lending or
//! lending-affected vault transactions; all 406 were replayed: 404 byte-exact, 2 fee-sponsored ones failing on the
//! unported featureSponsor (finding S1 of campaign 27; pinned `#[ignore]`d).
//!
//! What is pinned: the first transaction of every (type, result, asset kind) that landed, and every successful
//! LoanSet, LoanPay, LoanManage, LoanDelete and LoanBrokerDelete, every LoanBrokerCoverClawback, and the vault
//! transactions sent while a loan's loss was outstanding or after one was realized:
//!
//! - LoanBrokerSet: create with and without every optional field, updates, the reserve boundary, tec results;
//! - first-loss capital: deposit / withdraw / clawback for XRP, IOU and MPT, partial and full, with and without debt;
//! - LoanBrokerDelete: with cover left, an owner without a holding (fixCleanup3_5_0 makes one), an unauthorized
//!   owner, frozen / locked arms, the issuer as owner, loans outstanding;
//! - LoanSet: XRP / IOU / MPT, origination fee, counter-signed by a regular key and by a signer list, a
//!   multi-signed submitter, the failure classes;
//! - LoanPay: regular, several periods at once, overpayment, late (fee and interest), full early payoff, final;
//! - LoanManage: impair, unimpair, default with enough, too little and no first-loss capital; LoanDelete;
//! - the vault under a loss: VaultWithdraw / VaultClawback with LossUnrealized outstanding and after a default.
//!
//! Devnet runs rippled 3.5.0-rc5 (SingleAssetVault, LendingProtocol, LendingProtocolV1_1, fixCleanup3_5_0 on);
//! every bundle carries its parent ledger's Amendments object and the resolution of its ledger. Bundles were built
//! by the campaign's own builder (`sweep28.py`, one request per object version) and cross-checked against
//! `scripts/loan_bundle.py` / `loan_broker_bundle.py` on a sample. Same harness and pass criteria as
//! loan_vector.rs (recorded result, byte-identical objects, no write the metadata does not name).
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
    state.state_map.insert(key32(key_hex), serde_json::to_vec(&jv).unwrap()).unwrap();
}

fn encoded(bytes: &[u8]) -> String {
    let mut jv: Value = serde_json::from_slice(bytes).unwrap();
    canon_for_encode(&mut jv);
    hex::encode_upper(xrpl_core::codec::encode::encode_transaction_json(&jv, false).unwrap())
}

/// Hydrate the pre-state, apply the transaction, compare the result and every object.
fn run(bundle_json: &str) {
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
        // The resolution of the ledger being built: a LoanSet's sfStartDate is the parent's close time plus it.
        close_time_resolution: bundle["close_time_resolution"].as_u64().unwrap_or(10) as u8,
        close_flags: 0,
    };
    let mut state = LedgerState::new_unverified(header);
    for (k, v) in bundle["pre"].as_object().unwrap() {
        hydrate(&mut state, k, v.as_str().unwrap());
    }
    let txf = build_txfields(&bundle["tx"]).expect("txfields");
    let (ter, mut mods) = native_apply_one(&state, &txf);
    assert_eq!(ter, bundle["result"].as_str().unwrap(), "devnet recorded this transaction result");
    let pre = |k: &Hash256| state.state_map.lookup(k).map(|b| b.to_vec());
    xrpl_ledger::ledger::threading::stamp_threading(&mut mods, &pre, bundle["tx"]["hash"].as_str().unwrap(), seq);

    let expect = bundle["expect"].as_object().unwrap();
    let mut differences = Vec::new();
    for (k, want) in expect {
        let want = want.as_str().unwrap().trim().to_uppercase();
        match mods.get(&key32(k)) {
            None => {
                // Not written: right only for a pinned, untouched pre-image.
                let seated = bundle["pre"][k].as_str().unwrap_or_default().trim().to_uppercase();
                if want.is_empty() || want != seated {
                    differences.push(format!("{k}: devnet changed it, the engine did not write it"));
                }
            }
            Some(SandboxEntry::Deleted) => {
                if !want.is_empty() {
                    differences.push(format!("{k}: the engine deleted it, devnet kept it"));
                }
            }
            Some(SandboxEntry::Created(b)) | Some(SandboxEntry::Modified(b)) => {
                if want.is_empty() {
                    differences.push(format!("{k}: devnet deleted it, the engine kept it"));
                } else if encoded(b) != want {
                    let ours: Value = serde_json::from_slice(b).unwrap();
                    let mut theirs = xrpl_core::codec::decode::decode_transaction_binary(&hex::decode(&want).unwrap()).unwrap();
                    hexify_addresses(&mut theirs);
                    differences.push(format!("{k}: bytes differ\n  ours   {ours}\n  devnet {theirs}"));
                }
            }
        }
    }
    // Nothing beyond what the metadata names.
    for (k, entry) in &mods {
        let key = hex::encode_upper(k.0);
        if !expect.contains_key(&key) {
            let what = match entry {
                SandboxEntry::Deleted => "deleted".to_string(),
                SandboxEntry::Created(b) | SandboxEntry::Modified(b) => String::from_utf8_lossy(b).into_owned(),
            };
            differences.push(format!("{key}: the engine wrote an object devnet's metadata does not name: {what}"));
        }
    }
    assert!(differences.is_empty(), "{} difference(s):\n{}", differences.len(), differences.join("\n"));
}

// ---------------------------------------------------------------------------------------------------------------
// LoanBrokerSet
// ---------------------------------------------------------------------------------------------------------------

/// 5F54273A89DE… (xrp) lbset.BX. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_set_lbset_bx_devnet_7155359() {
    run(include_str!("vectors/c28_loan_broker_set_lbset_bx_devnet_7155359.json"));
}

/// 88BDEF52AF5E… (iou) lbset.BI. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_set_lbset_bi_devnet_7155365() {
    run(include_str!("vectors/c28_loan_broker_set_lbset_bi_devnet_7155365.json"));
}

/// 102F2B42DAED… (mpt) lbset.BM. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_set_lbset_bm_devnet_7155367() {
    run(include_str!("vectors/c28_loan_broker_set_lbset_bm_devnet_7155367.json"));
}

/// 67D462267578… (na) lbset.tec.no_entry.vault. Devnet: tecNO_ENTRY.
#[test]
fn c28_loan_broker_set_lbset_tec_no_entry_vault_devnet_7155379() {
    run(include_str!("vectors/c28_loan_broker_set_lbset_tec_no_entry_vault_devnet_7155379.json"));
}

/// 4F96598C2529… (xrp) lbset.tec.no_permission.not_vault_owner. Devnet: tecNO_PERMISSION.
#[test]
fn c28_loan_broker_set_lbset_tec_no_permission_not_vault_owner_devnet_7155381() {
    run(include_str!("vectors/c28_loan_broker_set_lbset_tec_no_permission_not_vault_owner_devnet_7155381.json"));
}

/// 3D11E3E7F088… (xrp) lbset.upd.tec.no_entry.broker. Devnet: tecNO_ENTRY.
#[test]
fn c28_loan_broker_set_lbset_upd_tec_no_entry_broker_devnet_7155385() {
    run(include_str!("vectors/c28_loan_broker_set_lbset_upd_tec_no_entry_broker_devnet_7155385.json"));
}

/// 605AB6C099A5… (iou) lbset.upd.tec.no_permission.other_vault. Devnet: tecNO_PERMISSION.
#[test]
fn c28_loan_broker_set_lbset_upd_tec_no_permission_other_vault_devnet_7155387() {
    run(include_str!("vectors/c28_loan_broker_set_lbset_upd_tec_no_permission_other_vault_devnet_7155387.json"));
}

/// 66DF30C0AA49… (xrp) lbset.tec.precision.xrp.fraction. Devnet: tecPRECISION_LOSS.
#[test]
fn c28_loan_broker_set_lbset_tec_precision_xrp_fraction_devnet_7155391() {
    run(include_str!("vectors/c28_loan_broker_set_lbset_tec_precision_xrp_fraction_devnet_7155391.json"));
}

/// F5BB362E4B78… (mpt) lbset.tec.precision.mpt.fraction. Devnet: tecPRECISION_LOSS.
#[test]
fn c28_loan_broker_set_lbset_tec_precision_mpt_fraction_devnet_7155393() {
    run(include_str!("vectors/c28_loan_broker_set_lbset_tec_precision_mpt_fraction_devnet_7155393.json"));
}

/// BDDC2D78C5D0… (iou) lbset.tec.frozen.vault_pseudo. Devnet: tecFROZEN.
#[test]
fn c28_loan_broker_set_lbset_tec_frozen_vault_pseudo_devnet_7155401() {
    run(include_str!("vectors/c28_loan_broker_set_lbset_tec_frozen_vault_pseudo_devnet_7155401.json"));
}

/// 1942391732DA… (mpt) lbset.tec.locked.vault_pseudo. Devnet: tecLOCKED.
#[test]
fn c28_loan_broker_set_lbset_tec_locked_vault_pseudo_devnet_7155409() {
    run(include_str!("vectors/c28_loan_broker_set_lbset_tec_locked_vault_pseudo_devnet_7155409.json"));
}

/// D562ACE0AC1E… (xrp) lbset.tec.reserve.minus1. Devnet: tecINSUFFICIENT_RESERVE.
#[test]
fn c28_loan_broker_set_lbset_tec_reserve_minus1_devnet_7155446() {
    run(include_str!("vectors/c28_loan_broker_set_lbset_tec_reserve_minus1_devnet_7155446.json"));
}

/// CB424B6CF65E… (xrp) lbset.upd.tec.limit.below_debt. Devnet: tecLIMIT_EXCEEDED.
#[test]
fn c28_loan_broker_set_lbset_upd_tec_limit_below_debt_devnet_7155763() {
    run(include_str!("vectors/c28_loan_broker_set_lbset_upd_tec_limit_below_debt_devnet_7155763.json"));
}


// ---------------------------------------------------------------------------------------------------------------
// LoanBrokerCoverDeposit
// ---------------------------------------------------------------------------------------------------------------

/// D3AC17A47BA8… (xrp) cdep.xrp. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_cover_deposit_cdep_xrp_devnet_7155451() {
    run(include_str!("vectors/c28_loan_broker_cover_deposit_cdep_xrp_devnet_7155451.json"));
}

/// 0B42F4181D1B… (iou) cdep.iou. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_cover_deposit_cdep_iou_devnet_7155453() {
    run(include_str!("vectors/c28_loan_broker_cover_deposit_cdep_iou_devnet_7155453.json"));
}

/// BDB47A0C0ABB… (mpt) cdep.mpt. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_cover_deposit_cdep_mpt_devnet_7155455() {
    run(include_str!("vectors/c28_loan_broker_cover_deposit_cdep_mpt_devnet_7155455.json"));
}

/// 20C5E35F66FF… (na) cdep.tec.no_entry. Devnet: tecNO_ENTRY.
#[test]
fn c28_loan_broker_cover_deposit_cdep_tec_no_entry_devnet_7155467() {
    run(include_str!("vectors/c28_loan_broker_cover_deposit_cdep_tec_no_entry_devnet_7155467.json"));
}

/// 911FDCAF31A6… (xrp) cdep.tec.no_permission. Devnet: tecNO_PERMISSION.
#[test]
fn c28_loan_broker_cover_deposit_cdep_tec_no_permission_devnet_7155469() {
    run(include_str!("vectors/c28_loan_broker_cover_deposit_cdep_tec_no_permission_devnet_7155469.json"));
}

/// 6AD389CCBB82… (xrp) cdep.tec.wrong_asset.iou_into_xrp. Devnet: tecWRONG_ASSET.
#[test]
fn c28_loan_broker_cover_deposit_cdep_tec_wrong_asset_iou_into_xrp_devnet_7155471() {
    run(include_str!("vectors/c28_loan_broker_cover_deposit_cdep_tec_wrong_asset_iou_into_xrp_devnet_7155471.json"));
}

/// 73FFACA50A9D… (iou) cdep.tec.wrong_asset.xrp_into_iou. Devnet: tecWRONG_ASSET.
#[test]
fn c28_loan_broker_cover_deposit_cdep_tec_wrong_asset_xrp_into_iou_devnet_7155473() {
    run(include_str!("vectors/c28_loan_broker_cover_deposit_cdep_tec_wrong_asset_xrp_into_iou_devnet_7155473.json"));
}

/// D02E02121910… (mpt) cdep.tec.wrong_asset.other_mpt. Devnet: tecWRONG_ASSET.
#[test]
fn c28_loan_broker_cover_deposit_cdep_tec_wrong_asset_other_mpt_devnet_7155475() {
    run(include_str!("vectors/c28_loan_broker_cover_deposit_cdep_tec_wrong_asset_other_mpt_devnet_7155475.json"));
}

/// EEBA2D1A8F54… (xrp) cdep.tec.insufficient.xrp. Devnet: tecINSUFFICIENT_FUNDS.
#[test]
fn c28_loan_broker_cover_deposit_cdep_tec_insufficient_xrp_devnet_7155477() {
    run(include_str!("vectors/c28_loan_broker_cover_deposit_cdep_tec_insufficient_xrp_devnet_7155477.json"));
}

/// 95A4009A8EE2… (iou) cdep.tec.insufficient.iou. Devnet: tecINSUFFICIENT_FUNDS.
#[test]
fn c28_loan_broker_cover_deposit_cdep_tec_insufficient_iou_devnet_7155479() {
    run(include_str!("vectors/c28_loan_broker_cover_deposit_cdep_tec_insufficient_iou_devnet_7155479.json"));
}

/// 93F626A13B2D… (mpt) cdep.tec.insufficient.mpt. Devnet: tecINSUFFICIENT_FUNDS.
#[test]
fn c28_loan_broker_cover_deposit_cdep_tec_insufficient_mpt_devnet_7155481() {
    run(include_str!("vectors/c28_loan_broker_cover_deposit_cdep_tec_insufficient_mpt_devnet_7155481.json"));
}

/// A91E91D77EEE… (iou) cdep.tec.precision.iou. Devnet: tecPRECISION_LOSS.
#[test]
fn c28_loan_broker_cover_deposit_cdep_tec_precision_iou_devnet_7155483() {
    run(include_str!("vectors/c28_loan_broker_cover_deposit_cdep_tec_precision_iou_devnet_7155483.json"));
}

/// E9AE21515174… (iou) cdep.tec.frozen.owner. Devnet: tecFROZEN.
#[test]
fn c28_loan_broker_cover_deposit_cdep_tec_frozen_owner_devnet_7155487() {
    run(include_str!("vectors/c28_loan_broker_cover_deposit_cdep_tec_frozen_owner_devnet_7155487.json"));
}

/// 096AECFAC503… (mpt) cdep.tec.locked.owner. Devnet: tecLOCKED.
#[test]
fn c28_loan_broker_cover_deposit_cdep_tec_locked_owner_devnet_7155499() {
    run(include_str!("vectors/c28_loan_broker_cover_deposit_cdep_tec_locked_owner_devnet_7155499.json"));
}

/// 392CD1B3F84D… (xrp) cdep.sponsored.fee. Devnet: tesSUCCESS.
#[test]
#[ignore = "finding S1 (campaign 27): the fee payer is the sponsor; featureSponsor is not ported"]
fn c28_loan_broker_cover_deposit_cdep_sponsored_fee_devnet_7156444() {
    run(include_str!("vectors/c28_loan_broker_cover_deposit_cdep_sponsored_fee_devnet_7156444.json"));
}


// ---------------------------------------------------------------------------------------------------------------
// LoanBrokerCoverWithdraw
// ---------------------------------------------------------------------------------------------------------------

/// 1ECE7C87AFBA… (iou) cwd.frozen.owner.to_self. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_cover_withdraw_cwd_frozen_owner_to_self_devnet_7155488() {
    run(include_str!("vectors/c28_loan_broker_cover_withdraw_cwd_frozen_owner_to_self_devnet_7155488.json"));
}

/// 2934075FA744… (iou) cwd.tec.frozen.broker_pseudo. Devnet: tecFROZEN.
#[test]
fn c28_loan_broker_cover_withdraw_cwd_tec_frozen_broker_pseudo_devnet_7155493() {
    run(include_str!("vectors/c28_loan_broker_cover_withdraw_cwd_tec_frozen_broker_pseudo_devnet_7155493.json"));
}

/// 788A10C34689… (mpt) cwd.locked.owner.to_self. Devnet: tecLOCKED.
#[test]
fn c28_loan_broker_cover_withdraw_cwd_locked_owner_to_self_devnet_7155501() {
    run(include_str!("vectors/c28_loan_broker_cover_withdraw_cwd_locked_owner_to_self_devnet_7155501.json"));
}

/// C8063CDA568B… (xrp) cwd.xrp.self. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_cover_withdraw_cwd_xrp_self_devnet_7155505() {
    run(include_str!("vectors/c28_loan_broker_cover_withdraw_cwd_xrp_self_devnet_7155505.json"));
}

/// 1E3F918D766B… (mpt) cwd.mpt.self. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_cover_withdraw_cwd_mpt_self_devnet_7155511() {
    run(include_str!("vectors/c28_loan_broker_cover_withdraw_cwd_mpt_self_devnet_7155511.json"));
}

/// D27D04435B71… (xrp) cwd.tec.insufficient.xrp. Devnet: tecINSUFFICIENT_FUNDS.
#[test]
fn c28_loan_broker_cover_withdraw_cwd_tec_insufficient_xrp_devnet_7155517() {
    run(include_str!("vectors/c28_loan_broker_cover_withdraw_cwd_tec_insufficient_xrp_devnet_7155517.json"));
}

/// ECDDCEB2F99D… (iou) cwd.tec.insufficient.iou. Devnet: tecINSUFFICIENT_FUNDS.
#[test]
fn c28_loan_broker_cover_withdraw_cwd_tec_insufficient_iou_devnet_7155519() {
    run(include_str!("vectors/c28_loan_broker_cover_withdraw_cwd_tec_insufficient_iou_devnet_7155519.json"));
}

/// ED913561C1A0… (xrp) cwd.tec.no_permission. Devnet: tecNO_PERMISSION.
#[test]
fn c28_loan_broker_cover_withdraw_cwd_tec_no_permission_devnet_7155521() {
    run(include_str!("vectors/c28_loan_broker_cover_withdraw_cwd_tec_no_permission_devnet_7155521.json"));
}

/// DD930E580797… (na) cwd.tec.no_entry. Devnet: tecNO_ENTRY.
#[test]
fn c28_loan_broker_cover_withdraw_cwd_tec_no_entry_devnet_7155523() {
    run(include_str!("vectors/c28_loan_broker_cover_withdraw_cwd_tec_no_entry_devnet_7155523.json"));
}

/// B20193FA2F25… (xrp) cwd.tec.wrong_asset. Devnet: tecWRONG_ASSET.
#[test]
fn c28_loan_broker_cover_withdraw_cwd_tec_wrong_asset_devnet_7155525() {
    run(include_str!("vectors/c28_loan_broker_cover_withdraw_cwd_tec_wrong_asset_devnet_7155525.json"));
}

/// DDFADCB485DD… (xrp) cwd.tec.pseudo.vault. Devnet: tecPSEUDO_ACCOUNT.
#[test]
fn c28_loan_broker_cover_withdraw_cwd_tec_pseudo_vault_devnet_7155527() {
    run(include_str!("vectors/c28_loan_broker_cover_withdraw_cwd_tec_pseudo_vault_devnet_7155527.json"));
}

/// ABBA411F903D… (xrp) cwd.tec.no_dst. Devnet: tecNO_DST.
#[test]
fn c28_loan_broker_cover_withdraw_cwd_tec_no_dst_devnet_7155531() {
    run(include_str!("vectors/c28_loan_broker_cover_withdraw_cwd_tec_no_dst_devnet_7155531.json"));
}

/// F96112ECCDE3… (iou) cwd.tec.no_line. Devnet: tecNO_LINE.
#[test]
fn c28_loan_broker_cover_withdraw_cwd_tec_no_line_devnet_7155533() {
    run(include_str!("vectors/c28_loan_broker_cover_withdraw_cwd_tec_no_line_devnet_7155533.json"));
}

/// 0F7295834A4D… (mpt) cwd.tec.no_auth.mpt.dest_no_token. Devnet: tecNO_AUTH.
#[test]
fn c28_loan_broker_cover_withdraw_cwd_tec_no_auth_mpt_dest_no_token_devnet_7155535() {
    run(include_str!("vectors/c28_loan_broker_cover_withdraw_cwd_tec_no_auth_mpt_dest_no_token_devnet_7155535.json"));
}

/// 14B0C77340FD… (iou) cwd.tec.precision.iou. Devnet: tecPRECISION_LOSS.
#[test]
fn c28_loan_broker_cover_withdraw_cwd_tec_precision_iou_devnet_7155537() {
    run(include_str!("vectors/c28_loan_broker_cover_withdraw_cwd_tec_precision_iou_devnet_7155537.json"));
}

/// F213F74206AF… (xrp) cwd.tec.dst_tag_needed. Devnet: tecDST_TAG_NEEDED.
#[test]
fn c28_loan_broker_cover_withdraw_cwd_tec_dst_tag_needed_devnet_7155541() {
    run(include_str!("vectors/c28_loan_broker_cover_withdraw_cwd_tec_dst_tag_needed_devnet_7155541.json"));
}


// ---------------------------------------------------------------------------------------------------------------
// LoanBrokerCoverClawback
// ---------------------------------------------------------------------------------------------------------------

/// 0027D4FEA577… (iou) cclaw.iou.partial.both_fields. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_cover_clawback_cclaw_iou_partial_both_fields_devnet_7155551() {
    run(include_str!("vectors/c28_loan_broker_cover_clawback_cclaw_iou_partial_both_fields_devnet_7155551.json"));
}

/// FC67366C3B42… (iou) cclaw.iou.partial.amount_names_pseudo. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_cover_clawback_cclaw_iou_partial_amount_names_pseudo_devnet_7155553() {
    run(include_str!("vectors/c28_loan_broker_cover_clawback_cclaw_iou_partial_amount_names_pseudo_devnet_7155553.json"));
}

/// D079EF8C73FD… (iou) cclaw.iou.partial.amount_issuer_pseudo.with_id. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_cover_clawback_cclaw_iou_partial_amount_issuer_pseudo_with_id_devnet_7155555() {
    run(include_str!("vectors/c28_loan_broker_cover_clawback_cclaw_iou_partial_amount_issuer_pseudo_with_id_devnet_7155555.json"));
}

/// D491DEF358B0… (mpt) cclaw.mpt.partial. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_cover_clawback_cclaw_mpt_partial_devnet_7155556() {
    run(include_str!("vectors/c28_loan_broker_cover_clawback_cclaw_mpt_partial_devnet_7155556.json"));
}

/// 1BBC3CAE6269… (iou) cclaw.iou.more_than_cover. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_cover_clawback_cclaw_iou_more_than_cover_devnet_7155557() {
    run(include_str!("vectors/c28_loan_broker_cover_clawback_cclaw_iou_more_than_cover_devnet_7155557.json"));
}

/// 4A6A41B2E159… (iou) cclaw.iou.all.zero_amount. Devnet: tecINSUFFICIENT_FUNDS.
#[test]
fn c28_loan_broker_cover_clawback_cclaw_iou_all_zero_amount_devnet_7155559() {
    run(include_str!("vectors/c28_loan_broker_cover_clawback_cclaw_iou_all_zero_amount_devnet_7155559.json"));
}

/// E3DEE7EE95BE… (mpt) cclaw.mpt.all.id_only. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_cover_clawback_cclaw_mpt_all_id_only_devnet_7155561() {
    run(include_str!("vectors/c28_loan_broker_cover_clawback_cclaw_mpt_all_id_only_devnet_7155561.json"));
}

/// F8A6EFFFEEC2… (mpt) cclaw.mpt.empty_cover. Devnet: tecINSUFFICIENT_FUNDS.
#[test]
fn c28_loan_broker_cover_clawback_cclaw_mpt_empty_cover_devnet_7155563() {
    run(include_str!("vectors/c28_loan_broker_cover_clawback_cclaw_mpt_empty_cover_devnet_7155563.json"));
}

/// 29DA0182B755… (iou) cclaw.iou.issuer_owner.all. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_cover_clawback_cclaw_iou_issuer_owner_all_devnet_7155564() {
    run(include_str!("vectors/c28_loan_broker_cover_clawback_cclaw_iou_issuer_owner_all_devnet_7155564.json"));
}

/// 402BEBA1C3E3… (xrp) cclaw.tec.no_permission.xrp. Devnet: tecNO_PERMISSION.
#[test]
fn c28_loan_broker_cover_clawback_cclaw_tec_no_permission_xrp_devnet_7155568() {
    run(include_str!("vectors/c28_loan_broker_cover_clawback_cclaw_tec_no_permission_xrp_devnet_7155568.json"));
}

/// A8847B1FFC3E… (iou) cclaw.tec.no_permission.not_issuer. Devnet: tecNO_PERMISSION.
#[test]
fn c28_loan_broker_cover_clawback_cclaw_tec_no_permission_not_issuer_devnet_7155570() {
    run(include_str!("vectors/c28_loan_broker_cover_clawback_cclaw_tec_no_permission_not_issuer_devnet_7155570.json"));
}

/// 8283A89ECC82… (iou) cclaw.tec.no_permission.other_issuer. Devnet: tecNO_PERMISSION.
#[test]
fn c28_loan_broker_cover_clawback_cclaw_tec_no_permission_other_issuer_devnet_7155572() {
    run(include_str!("vectors/c28_loan_broker_cover_clawback_cclaw_tec_no_permission_other_issuer_devnet_7155572.json"));
}

/// 1DF0A6A65154… (iou) cclaw.tec.wrong_asset. Devnet: tecWRONG_ASSET.
#[test]
fn c28_loan_broker_cover_clawback_cclaw_tec_wrong_asset_devnet_7155574() {
    run(include_str!("vectors/c28_loan_broker_cover_clawback_cclaw_tec_wrong_asset_devnet_7155574.json"));
}

/// 43AC2D7A429A… (mpt) cclaw.tec.wrong_asset.mpt. Devnet: tecWRONG_ASSET.
#[test]
fn c28_loan_broker_cover_clawback_cclaw_tec_wrong_asset_mpt_devnet_7155576() {
    run(include_str!("vectors/c28_loan_broker_cover_clawback_cclaw_tec_wrong_asset_mpt_devnet_7155576.json"));
}

/// 7A875CFD89F7… (na) cclaw.tec.no_entry. Devnet: tecNO_ENTRY.
#[test]
fn c28_loan_broker_cover_clawback_cclaw_tec_no_entry_devnet_7155578() {
    run(include_str!("vectors/c28_loan_broker_cover_clawback_cclaw_tec_no_entry_devnet_7155578.json"));
}

/// 1E13933E2B91… (na) cclaw.tec.amount_names_non_broker. Devnet: tecOBJECT_NOT_FOUND.
#[test]
fn c28_loan_broker_cover_clawback_cclaw_tec_amount_names_non_broker_devnet_7155580() {
    run(include_str!("vectors/c28_loan_broker_cover_clawback_cclaw_tec_amount_names_non_broker_devnet_7155580.json"));
}

/// 413A6D2A5193… (mpt) cclaw.mpt.all.with_debt. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_cover_clawback_cclaw_mpt_all_with_debt_devnet_7155914() {
    run(include_str!("vectors/c28_loan_broker_cover_clawback_cclaw_mpt_all_with_debt_devnet_7155914.json"));
}

/// 008258D08CE1… (mpt) cclaw.mpt.more_than_cover.with_debt. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_cover_clawback_cclaw_mpt_more_than_cover_with_debt_devnet_7155918() {
    run(include_str!("vectors/c28_loan_broker_cover_clawback_cclaw_mpt_more_than_cover_with_debt_devnet_7155918.json"));
}

/// CB79C77F39A8… (iou) cclaw.iou.all.with_debt. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_cover_clawback_cclaw_iou_all_with_debt_devnet_7155949() {
    run(include_str!("vectors/c28_loan_broker_cover_clawback_cclaw_iou_all_with_debt_devnet_7155949.json"));
}

/// 2A3BD3EC23BE… (iou) cclaw.iou.at_minimum. Devnet: tecINSUFFICIENT_FUNDS.
#[test]
fn c28_loan_broker_cover_clawback_cclaw_iou_at_minimum_devnet_7155951() {
    run(include_str!("vectors/c28_loan_broker_cover_clawback_cclaw_iou_at_minimum_devnet_7155951.json"));
}

/// 8174BEF019B4… (iou) cclaw.iou.all.before_default. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_cover_clawback_cclaw_iou_all_before_default_devnet_7156476() {
    run(include_str!("vectors/c28_loan_broker_cover_clawback_cclaw_iou_all_before_default_devnet_7156476.json"));
}


// ---------------------------------------------------------------------------------------------------------------
// LoanBrokerDelete
// ---------------------------------------------------------------------------------------------------------------

/// 96EB149F184C… (xrp) bd.tec.has_obligations.loans. Devnet: tecHAS_OBLIGATIONS.
#[test]
fn c28_loan_broker_delete_bd_tec_has_obligations_loans_devnet_7155773() {
    run(include_str!("vectors/c28_loan_broker_delete_bd_tec_has_obligations_loans_devnet_7155773.json"));
}

/// 9A3C0F5C2118… (xrp) bd.CX.xrp.cover_left.after_default. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_delete_bd_cx_xrp_cover_left_after_default_devnet_7156199() {
    run(include_str!("vectors/c28_loan_broker_delete_bd_cx_xrp_cover_left_after_default_devnet_7156199.json"));
}

/// 37EC85D76CBF… (xrp) bd.CX2.xrp.clean. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_delete_bd_cx2_xrp_clean_devnet_7156201() {
    run(include_str!("vectors/c28_loan_broker_delete_bd_cx2_xrp_clean_devnet_7156201.json"));
}

/// CCFA9B5972D6… (iou) bd.CI.iou.no_cover. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_delete_bd_ci_iou_no_cover_devnet_7156209() {
    run(include_str!("vectors/c28_loan_broker_delete_bd_ci_iou_no_cover_devnet_7156209.json"));
}

/// 1B8AF7028D77… (mpt) bd.CM.mpt.cover_left.after_default. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_delete_bd_cm_mpt_cover_left_after_default_devnet_7156211() {
    run(include_str!("vectors/c28_loan_broker_delete_bd_cm_mpt_cover_left_after_default_devnet_7156211.json"));
}

/// F2F431DEF76A… (xrp) bd.tec.no_permission. Devnet: tecNO_PERMISSION.
#[test]
fn c28_loan_broker_delete_bd_tec_no_permission_devnet_7156220() {
    run(include_str!("vectors/c28_loan_broker_delete_bd_tec_no_permission_devnet_7156220.json"));
}

/// 489818F55D31… (na) bd.tec.no_entry. Devnet: tecNO_ENTRY.
#[test]
fn c28_loan_broker_delete_bd_tec_no_entry_devnet_7156222() {
    run(include_str!("vectors/c28_loan_broker_delete_bd_tec_no_entry_devnet_7156222.json"));
}

/// 90E6F311E3A3… (xrp) bd.BX2.xrp.cover_left. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_delete_bd_bx2_xrp_cover_left_devnet_7156224() {
    run(include_str!("vectors/c28_loan_broker_delete_bd_bx2_xrp_cover_left_devnet_7156224.json"));
}

/// 757321114DDE… (xrp) bd.BL.xrp.clean.owner_at_reserve. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_delete_bd_bl_xrp_clean_owner_at_reserve_devnet_7156226() {
    run(include_str!("vectors/c28_loan_broker_delete_bd_bl_xrp_clean_owner_at_reserve_devnet_7156226.json"));
}

/// B6445DFDD467… (iou) bd.BI2.iou.cover_empty. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_delete_bd_bi2_iou_cover_empty_devnet_7156228() {
    run(include_str!("vectors/c28_loan_broker_delete_bd_bi2_iou_cover_empty_devnet_7156228.json"));
}

/// 86AB8ADBE811… (iou) bd.BI3.tec.frozen.broker_pseudo. Devnet: tecFROZEN.
#[test]
fn c28_loan_broker_delete_bd_bi3_tec_frozen_broker_pseudo_devnet_7156234() {
    run(include_str!("vectors/c28_loan_broker_delete_bd_bi3_tec_frozen_broker_pseudo_devnet_7156234.json"));
}

/// 3BA3ACBE3144… (iou) bd.BI3.iou.cover_left.owner_frozen_not_deep. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_delete_bd_bi3_iou_cover_left_owner_frozen_not_deep_devnet_7156245() {
    run(include_str!("vectors/c28_loan_broker_delete_bd_bi3_iou_cover_left_owner_frozen_not_deep_devnet_7156245.json"));
}

/// 88B06D2CE7DC… (mpt) bd.BM2.mpt.owner_unauthorized. Devnet: tecNO_AUTH.
#[test]
fn c28_loan_broker_delete_bd_bm2_mpt_owner_unauthorized_devnet_7156252() {
    run(include_str!("vectors/c28_loan_broker_delete_bd_bm2_mpt_owner_unauthorized_devnet_7156252.json"));
}

/// B052C2983F68… (mpt) bd.BM2.mpt.owner_locked. Devnet: tecLOCKED.
#[test]
fn c28_loan_broker_delete_bd_bm2_mpt_owner_locked_devnet_7156257() {
    run(include_str!("vectors/c28_loan_broker_delete_bd_bm2_mpt_owner_locked_devnet_7156257.json"));
}

/// C9F1E8396046… (mpt) bd.BM2.mpt.cover_left. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_delete_bd_bm2_mpt_cover_left_devnet_7156266() {
    run(include_str!("vectors/c28_loan_broker_delete_bd_bm2_mpt_cover_left_devnet_7156266.json"));
}

/// A01003EB89B0… (mpt) bd.DM1.mpt.owner_no_mptoken. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_delete_bd_dm1_mpt_owner_no_mptoken_devnet_7156286() {
    run(include_str!("vectors/c28_loan_broker_delete_bd_dm1_mpt_owner_no_mptoken_devnet_7156286.json"));
}

/// 62DEAD2C1B8B… (iou) bd.DI1.iou.owner_no_line. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_delete_bd_di1_iou_owner_no_line_devnet_7156298() {
    run(include_str!("vectors/c28_loan_broker_delete_bd_di1_iou_owner_no_line_devnet_7156298.json"));
}

/// 91F920313276… (mpt) bd.DM2.tec.reserve.holding.minus1. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_delete_bd_dm2_tec_reserve_holding_minus1_devnet_7156303() {
    run(include_str!("vectors/c28_loan_broker_delete_bd_dm2_tec_reserve_holding_minus1_devnet_7156303.json"));
}

/// 32F5396865F2… (iou) bd.DI2.iou.owner_no_line.low_reserve. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_delete_bd_di2_iou_owner_no_line_low_reserve_devnet_7156309() {
    run(include_str!("vectors/c28_loan_broker_delete_bd_di2_iou_owner_no_line_low_reserve_devnet_7156309.json"));
}

/// 33BE05807BDA… (iou) bd.BG.iou.issuer_owner.cover_left. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_delete_bd_bg_iou_issuer_owner_cover_left_devnet_7156311() {
    run(include_str!("vectors/c28_loan_broker_delete_bd_bg_iou_issuer_owner_cover_left_devnet_7156311.json"));
}

/// 1455A73E4883… (mpt) bd.DM3.tec.reserve.holding.minus1. Devnet: tecINSUFFICIENT_RESERVE.
#[test]
fn c28_loan_broker_delete_bd_dm3_tec_reserve_holding_minus1_devnet_7156330() {
    run(include_str!("vectors/c28_loan_broker_delete_bd_dm3_tec_reserve_holding_minus1_devnet_7156330.json"));
}

/// 46360F0ED015… (mpt) bd.DM3.mpt.owner_no_mptoken.reserve_exact. Devnet: tesSUCCESS.
#[test]
fn c28_loan_broker_delete_bd_dm3_mpt_owner_no_mptoken_reserve_exact_devnet_7156334() {
    run(include_str!("vectors/c28_loan_broker_delete_bd_dm3_mpt_owner_no_mptoken_reserve_exact_devnet_7156334.json"));
}


// ---------------------------------------------------------------------------------------------------------------
// LoanSet
// ---------------------------------------------------------------------------------------------------------------

/// AD40353031A2… (xrp) ls.tec.too_soon.subscription. Devnet: tecTOO_SOON.
#[test]
fn c28_loan_set_ls_tec_too_soon_subscription_devnet_7155361() {
    run(include_str!("vectors/c28_loan_set_ls_tec_too_soon_subscription_devnet_7155361.json"));
}

/// C248C07DED6E… (na) ls.tec.no_entry. Devnet: tecNO_ENTRY.
#[test]
fn c28_loan_set_ls_tec_no_entry_devnet_7155587() {
    run(include_str!("vectors/c28_loan_set_ls_tec_no_entry_devnet_7155587.json"));
}

/// E25E4720601C… (xrp) ls.tec.no_permission.neither_owner. Devnet: tecNO_PERMISSION.
#[test]
fn c28_loan_set_ls_tec_no_permission_neither_owner_devnet_7155589() {
    run(include_str!("vectors/c28_loan_set_ls_tec_no_permission_neither_owner_devnet_7155589.json"));
}

/// 6041E1C26A00… (xrp) ls.tec.killed.interval. Devnet: tecKILLED.
#[test]
fn c28_loan_set_ls_tec_killed_interval_devnet_7155591() {
    run(include_str!("vectors/c28_loan_set_ls_tec_killed_interval_devnet_7155591.json"));
}

/// 31585FF2FA6B… (xrp) ls.tec.limit.debt_maximum. Devnet: tecLIMIT_EXCEEDED.
#[test]
fn c28_loan_set_ls_tec_limit_debt_maximum_devnet_7155599() {
    run(include_str!("vectors/c28_loan_set_ls_tec_limit_debt_maximum_devnet_7155599.json"));
}

/// 86768D9A7307… (xrp) ls.tec.precision.xrp.fraction. Devnet: tecPRECISION_LOSS.
#[test]
fn c28_loan_set_ls_tec_precision_xrp_fraction_devnet_7155601() {
    run(include_str!("vectors/c28_loan_set_ls_tec_precision_xrp_fraction_devnet_7155601.json"));
}

/// 3D4F47704352… (xrp) ls.tec.precision.xrp.interest_rounds_to_zero. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_tec_precision_xrp_interest_rounds_to_zero_devnet_7155608() {
    run(include_str!("vectors/c28_loan_set_ls_tec_precision_xrp_interest_rounds_to_zero_devnet_7155608.json"));
}

/// C378928B20DE… (mpt) ls.tec.precision.mpt.fraction. Devnet: tecPRECISION_LOSS.
#[test]
fn c28_loan_set_ls_tec_precision_mpt_fraction_devnet_7155610() {
    run(include_str!("vectors/c28_loan_set_ls_tec_precision_mpt_fraction_devnet_7155610.json"));
}

/// B9380934D3F0… (iou) ls.tec.precision.iou.servicefee.too_fine. Devnet: tecPRECISION_LOSS.
#[test]
fn c28_loan_set_ls_tec_precision_iou_servicefee_too_fine_devnet_7155612() {
    run(include_str!("vectors/c28_loan_set_ls_tec_precision_iou_servicefee_too_fine_devnet_7155612.json"));
}

/// EE9FAF705F45… (xrp) ls.tec.insufficient.vault_available. Devnet: tecINSUFFICIENT_FUNDS.
#[test]
fn c28_loan_set_ls_tec_insufficient_vault_available_devnet_7155614() {
    run(include_str!("vectors/c28_loan_set_ls_tec_insufficient_vault_available_devnet_7155614.json"));
}

/// 49BFB0ABA49A… (xrp) ls.tec.reserve.borrower. Devnet: tecINSUFFICIENT_RESERVE.
#[test]
fn c28_loan_set_ls_tec_reserve_borrower_devnet_7155618() {
    run(include_str!("vectors/c28_loan_set_ls_tec_reserve_borrower_devnet_7155618.json"));
}

/// 8538DF13ACEB… (mpt) ls.tec.no_auth.mpt.borrower. Devnet: tecNO_AUTH.
#[test]
fn c28_loan_set_ls_tec_no_auth_mpt_borrower_devnet_7155622() {
    run(include_str!("vectors/c28_loan_set_ls_tec_no_auth_mpt_borrower_devnet_7155622.json"));
}

/// 0630A32AFA40… (iou) ls.tec.frozen.borrower. Devnet: tecFROZEN.
#[test]
fn c28_loan_set_ls_tec_frozen_borrower_devnet_7155626() {
    run(include_str!("vectors/c28_loan_set_ls_tec_frozen_borrower_devnet_7155626.json"));
}

/// DF64A8A4946B… (mpt) ls.tec.locked.borrower. Devnet: tecLOCKED.
#[test]
fn c28_loan_set_ls_tec_locked_borrower_devnet_7155644() {
    run(include_str!("vectors/c28_loan_set_ls_tec_locked_borrower_devnet_7155644.json"));
}

/// 7A67A17B2ED3… (xrp) ls.TC. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_tc_devnet_7155648() {
    run(include_str!("vectors/c28_loan_set_ls_tc_devnet_7155648.json"));
}

/// 6A2639330126… (xrp) ls.TE. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_te_devnet_7155650() {
    run(include_str!("vectors/c28_loan_set_ls_te_devnet_7155650.json"));
}

/// 2B10E4FDF417… (iou) ls.TG. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_tg_devnet_7155652() {
    run(include_str!("vectors/c28_loan_set_ls_tg_devnet_7155652.json"));
}

/// 45E1FA49F4D8… (mpt) ls.TH. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_th_devnet_7155654() {
    run(include_str!("vectors/c28_loan_set_ls_th_devnet_7155654.json"));
}

/// E2364682811A… (xrp) ls.TB. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_tb_devnet_7155656() {
    run(include_str!("vectors/c28_loan_set_ls_tb_devnet_7155656.json"));
}

/// 5AC8C5D8D295… (iou) ls.TF. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_tf_devnet_7155658() {
    run(include_str!("vectors/c28_loan_set_ls_tf_devnet_7155658.json"));
}

/// B39BE7BB0ECF… (mpt) ls.TJ. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_tj_devnet_7155660() {
    run(include_str!("vectors/c28_loan_set_ls_tj_devnet_7155660.json"));
}

/// 49E6454EFC67… (xrp) ls.TA. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_ta_devnet_7155662() {
    run(include_str!("vectors/c28_loan_set_ls_ta_devnet_7155662.json"));
}

/// 514FC6CF04D6… (xrp) ls.LX1. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_lx1_devnet_7155761() {
    run(include_str!("vectors/c28_loan_set_ls_lx1_devnet_7155761.json"));
}

/// D5BE9BE374A8… (xrp) ls.LX6. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_lx6_devnet_7155825() {
    run(include_str!("vectors/c28_loan_set_ls_lx6_devnet_7155825.json"));
}

/// 2F52C13ED6B1… (xrp) ls.LX7.multisigned_counterparty.no_signingpubkey. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_lx7_multisigned_counterparty_no_signingpubkey_devnet_7155866() {
    run(include_str!("vectors/c28_loan_set_ls_lx7_multisigned_counterparty_no_signingpubkey_devnet_7155866.json"));
}

/// 73C0071185DA… (xrp) ls.LX8. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_lx8_devnet_7155874() {
    run(include_str!("vectors/c28_loan_set_ls_lx8_devnet_7155874.json"));
}

/// 65A1034BDB6C… (xrp) ls.LX9.broker_owner_borrows. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_lx9_broker_owner_borrows_devnet_7155876() {
    run(include_str!("vectors/c28_loan_set_ls_lx9_broker_owner_borrows_devnet_7155876.json"));
}

/// 8D850089FD34… (xrp) ls.LX10.origination_fee.whole_principal. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_lx10_origination_fee_whole_principal_devnet_7155878() {
    run(include_str!("vectors/c28_loan_set_ls_lx10_origination_fee_whole_principal_devnet_7155878.json"));
}

/// 0C4D563942DC… (iou) ls.LI1. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_li1_devnet_7155888() {
    run(include_str!("vectors/c28_loan_set_ls_li1_devnet_7155888.json"));
}

/// C807B9D568B3… (mpt) ls.LM1. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_lm1_devnet_7155903() {
    run(include_str!("vectors/c28_loan_set_ls_lm1_devnet_7155903.json"));
}

/// 52A401B8737C… (mpt) ls.LM3.no_mptoken.fee_is_everything. Devnet: tecINSUFFICIENT_FUNDS.
#[test]
fn c28_loan_set_ls_lm3_no_mptoken_fee_is_everything_devnet_7155923() {
    run(include_str!("vectors/c28_loan_set_ls_lm3_no_mptoken_fee_is_everything_devnet_7155923.json"));
}

/// D0A9A4EE5A76… (iou) ls.LI2. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_li2_devnet_7155936() {
    run(include_str!("vectors/c28_loan_set_ls_li2_devnet_7155936.json"));
}

/// 99E5E610CC6D… (iou) ls.LI6. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_li6_devnet_7155963() {
    run(include_str!("vectors/c28_loan_set_ls_li6_devnet_7155963.json"));
}

/// 500DBB02261A… (iou) ls.LI7. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_li7_devnet_7155968() {
    run(include_str!("vectors/c28_loan_set_ls_li7_devnet_7155968.json"));
}

/// 857AA6F1EA16… (iou) ls.LI8. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_li8_devnet_7155973() {
    run(include_str!("vectors/c28_loan_set_ls_li8_devnet_7155973.json"));
}

/// 2651A33DD605… (mpt) ls.LM3.no_mptoken.fee_is_everything. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_lm3_no_mptoken_fee_is_everything_devnet_7155986() {
    run(include_str!("vectors/c28_loan_set_ls_lm3_no_mptoken_fee_is_everything_devnet_7155986.json"));
}

/// 93F397F58C4D… (mpt) ls.LM4. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_lm4_devnet_7155988() {
    run(include_str!("vectors/c28_loan_set_ls_lm4_devnet_7155988.json"));
}

/// 78BC9B5804C4… (xrp) ls.SLX. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_slx_devnet_7156047() {
    run(include_str!("vectors/c28_loan_set_ls_slx_devnet_7156047.json"));
}

/// 7543E9AAEE5E… (iou) ls.SLI. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_sli_devnet_7156049() {
    run(include_str!("vectors/c28_loan_set_ls_sli_devnet_7156049.json"));
}

/// 1A473523851D… (mpt) ls.SLM. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_slm_devnet_7156051() {
    run(include_str!("vectors/c28_loan_set_ls_slm_devnet_7156051.json"));
}

/// 28DF69A833D4… (iou) ls.SLI2. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_sli2_devnet_7156053() {
    run(include_str!("vectors/c28_loan_set_ls_sli2_devnet_7156053.json"));
}

/// F38FED661303… (iou) ls.tec.no_permission.redemption_buffer. Devnet: tecNO_PERMISSION.
#[test]
fn c28_loan_set_ls_tec_no_permission_redemption_buffer_devnet_7156134() {
    run(include_str!("vectors/c28_loan_set_ls_tec_no_permission_redemption_buffer_devnet_7156134.json"));
}

/// 545F6FF7087E… (iou) ls.tec.expired.redemption. Devnet: tecEXPIRED.
#[test]
fn c28_loan_set_ls_tec_expired_redemption_devnet_7156167() {
    run(include_str!("vectors/c28_loan_set_ls_tec_expired_redemption_devnet_7156167.json"));
}

/// 68A4326BA33C… (xrp) ls.MX1. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_mx1_devnet_7156342() {
    run(include_str!("vectors/c28_loan_set_ls_mx1_devnet_7156342.json"));
}

/// B9BB41E25975… (xrp) ls.MX3. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_mx3_devnet_7156350() {
    run(include_str!("vectors/c28_loan_set_ls_mx3_devnet_7156350.json"));
}

/// 0AAEA9DCD97B… (xrp) ls.MX4. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_mx4_devnet_7156355() {
    run(include_str!("vectors/c28_loan_set_ls_mx4_devnet_7156355.json"));
}

/// 6DE10CB80735… (iou) ls.MI1. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_mi1_devnet_7156363() {
    run(include_str!("vectors/c28_loan_set_ls_mi1_devnet_7156363.json"));
}

/// 47A8B9406007… (iou) ls.MI3. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_mi3_devnet_7156373() {
    run(include_str!("vectors/c28_loan_set_ls_mi3_devnet_7156373.json"));
}

/// D62E70FEB421… (iou) ls.MI4. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_mi4_devnet_7156377() {
    run(include_str!("vectors/c28_loan_set_ls_mi4_devnet_7156377.json"));
}

/// 36B0C363AF03… (iou) ls.MI5. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_mi5_devnet_7156381() {
    run(include_str!("vectors/c28_loan_set_ls_mi5_devnet_7156381.json"));
}

/// F3225EFF670A… (mpt) ls.MM1. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_mm1_devnet_7156389() {
    run(include_str!("vectors/c28_loan_set_ls_mm1_devnet_7156389.json"));
}

/// 5AA7A60D6A64… (mpt) ls.MM2. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_mm2_devnet_7156396() {
    run(include_str!("vectors/c28_loan_set_ls_mm2_devnet_7156396.json"));
}

/// 44242469C07A… (mpt) ls.MM3. Devnet: tecNO_PERMISSION.
#[test]
fn c28_loan_set_ls_mm3_devnet_7156401() {
    run(include_str!("vectors/c28_loan_set_ls_mm3_devnet_7156401.json"));
}

/// ABCA627B297C… (iou) ls.XC1. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_xc1_devnet_7156436() {
    run(include_str!("vectors/c28_loan_set_ls_xc1_devnet_7156436.json"));
}

/// 50009FA2DBFE… (mpt) ls.XM1.vault_at_assets_maximum. Devnet: tesSUCCESS.
#[test]
fn c28_loan_set_ls_xm1_vault_at_assets_maximum_devnet_7156440() {
    run(include_str!("vectors/c28_loan_set_ls_xm1_vault_at_assets_maximum_devnet_7156440.json"));
}

/// 32C9838533FF… (iou) ls.tec.insufficient.cover_below_minimum. Devnet: tecINSUFFICIENT_FUNDS.
#[test]
fn c28_loan_set_ls_tec_insufficient_cover_below_minimum_devnet_7156481() {
    run(include_str!("vectors/c28_loan_set_ls_tec_insufficient_cover_below_minimum_devnet_7156481.json"));
}


// ---------------------------------------------------------------------------------------------------------------
// LoanPay
// ---------------------------------------------------------------------------------------------------------------

/// 99088AD13C57… (xrp) lp.TA.p1.exact. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_ta_p1_exact_devnet_7155664() {
    run(include_str!("vectors/c28_loan_pay_lp_ta_p1_exact_devnet_7155664.json"));
}

/// 618F4C659E2F… (xrp) lp.TA.tec.no_permission.overpayment_not_allowed. Devnet: tecNO_PERMISSION.
#[test]
fn c28_loan_pay_lp_ta_tec_no_permission_overpayment_not_allowed_devnet_7155666() {
    run(include_str!("vectors/c28_loan_pay_lp_ta_tec_no_permission_overpayment_not_allowed_devnet_7155666.json"));
}

/// E320736332AC… (xrp) lp.TA.tec.too_soon.late_flag_on_time. Devnet: tecTOO_SOON.
#[test]
fn c28_loan_pay_lp_ta_tec_too_soon_late_flag_on_time_devnet_7155668() {
    run(include_str!("vectors/c28_loan_pay_lp_ta_tec_too_soon_late_flag_on_time_devnet_7155668.json"));
}

/// A6FF17040647… (iou) lp.TF.tec.insufficient.late_fee_unpaid. Devnet: tecINSUFFICIENT_PAYMENT.
#[test]
fn c28_loan_pay_lp_tf_tec_insufficient_late_fee_unpaid_devnet_7155701() {
    run(include_str!("vectors/c28_loan_pay_lp_tf_tec_insufficient_late_fee_unpaid_devnet_7155701.json"));
}

/// 886267385FAD… (iou) lp.TF.p1.late.fee_and_interest. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_tf_p1_late_fee_and_interest_devnet_7155703() {
    run(include_str!("vectors/c28_loan_pay_lp_tf_p1_late_fee_and_interest_devnet_7155703.json"));
}

/// D3D180D58794… (mpt) lp.TJ.p1.late.mpt. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_tj_p1_late_mpt_devnet_7155705() {
    run(include_str!("vectors/c28_loan_pay_lp_tj_p1_late_mpt_devnet_7155705.json"));
}

/// BA70AC2D9C63… (xrp) lp.TB.p1.late.impaired.final. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_tb_p1_late_impaired_final_devnet_7155707() {
    run(include_str!("vectors/c28_loan_pay_lp_tb_p1_late_impaired_final_devnet_7155707.json"));
}

/// F770B6ED6AA7… (iou) lp.TF.p2.final. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_tf_p2_final_devnet_7155709() {
    run(include_str!("vectors/c28_loan_pay_lp_tf_p2_final_devnet_7155709.json"));
}

/// 5F4C4F6B222B… (mpt) lp.TJ.p2.final. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_tj_p2_final_devnet_7155711() {
    run(include_str!("vectors/c28_loan_pay_lp_tj_p2_final_devnet_7155711.json"));
}

/// 4F0A74883E67… (xrp) lp.TA.tec.expired.late_without_flag. Devnet: tecEXPIRED.
#[test]
fn c28_loan_pay_lp_ta_tec_expired_late_without_flag_devnet_7155718() {
    run(include_str!("vectors/c28_loan_pay_lp_ta_tec_expired_late_without_flag_devnet_7155718.json"));
}

/// 663DB03BFC36… (xrp) lp.TA.tec.insufficient.late. Devnet: tecINSUFFICIENT_PAYMENT.
#[test]
fn c28_loan_pay_lp_ta_tec_insufficient_late_devnet_7155720() {
    run(include_str!("vectors/c28_loan_pay_lp_ta_tec_insufficient_late_devnet_7155720.json"));
}

/// E4784B080D35… (xrp) lp.TA.p2.late.no_fees. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_ta_p2_late_no_fees_devnet_7155722() {
    run(include_str!("vectors/c28_loan_pay_lp_ta_p2_late_no_fees_devnet_7155722.json"));
}

/// 44EBEDABD4B6… (xrp) lp.TA.p3.final. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_ta_p3_final_devnet_7155724() {
    run(include_str!("vectors/c28_loan_pay_lp_ta_p3_final_devnet_7155724.json"));
}

/// 9AACB8521C69… (xrp) lp.TC.tec.killed.defaulted. Devnet: tecKILLED.
#[test]
fn c28_loan_pay_lp_tc_tec_killed_defaulted_devnet_7155735() {
    run(include_str!("vectors/c28_loan_pay_lp_tc_tec_killed_defaulted_devnet_7155735.json"));
}

/// C249266F1127… (na) lp.tec.no_entry. Devnet: tecNO_ENTRY.
#[test]
fn c28_loan_pay_lp_tec_no_entry_devnet_7155783() {
    run(include_str!("vectors/c28_loan_pay_lp_tec_no_entry_devnet_7155783.json"));
}

/// EE032972945B… (xrp) lp.tec.wrong_asset. Devnet: tecWRONG_ASSET.
#[test]
fn c28_loan_pay_lp_tec_wrong_asset_devnet_7155787() {
    run(include_str!("vectors/c28_loan_pay_lp_tec_wrong_asset_devnet_7155787.json"));
}

/// 8C7F0CB509D1… (xrp) lp.tec.insufficient_funds. Devnet: tecINSUFFICIENT_FUNDS.
#[test]
fn c28_loan_pay_lp_tec_insufficient_funds_devnet_7155789() {
    run(include_str!("vectors/c28_loan_pay_lp_tec_insufficient_funds_devnet_7155789.json"));
}

/// BCED4343A172… (xrp) lp.LX1.p1.exact. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_lx1_p1_exact_devnet_7155803() {
    run(include_str!("vectors/c28_loan_pay_lp_lx1_p1_exact_devnet_7155803.json"));
}

/// 2CEFEFD95D2F… (xrp) lp.LX1.p2-4.three_at_once. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_lx1_p2_4_three_at_once_devnet_7155805() {
    run(include_str!("vectors/c28_loan_pay_lp_lx1_p2_4_three_at_once_devnet_7155805.json"));
}

/// 981804B03C46… (xrp) lp.LX1.overpayment. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_lx1_overpayment_devnet_7155807() {
    run(include_str!("vectors/c28_loan_pay_lp_lx1_overpayment_devnet_7155807.json"));
}

/// 0CA892A06D60… (xrp) lp.LX1.overpayment_flag.exact_due. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_lx1_overpayment_flag_exact_due_devnet_7155808() {
    run(include_str!("vectors/c28_loan_pay_lp_lx1_overpayment_flag_exact_due_devnet_7155808.json"));
}

/// 9075DC7009D3… (xrp) lp.LX1.regular.excess_not_taken. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_lx1_regular_excess_not_taken_devnet_7155810() {
    run(include_str!("vectors/c28_loan_pay_lp_lx1_regular_excess_not_taken_devnet_7155810.json"));
}

/// DB5B230E8CE0… (xrp) lp.LX1.full_early_payoff. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_lx1_full_early_payoff_devnet_7155814() {
    run(include_str!("vectors/c28_loan_pay_lp_lx1_full_early_payoff_devnet_7155814.json"));
}

/// 306ECCAA24FE… (xrp) lp.LX6.full.first_payment.enough. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_lx6_full_first_payment_enough_devnet_7155862() {
    run(include_str!("vectors/c28_loan_pay_lp_lx6_full_first_payment_enough_devnet_7155862.json"));
}

/// 85F98EC6453A… (xrp) lp.LX7.overpayment_flag.on_last_payment. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_lx7_overpayment_flag_on_last_payment_devnet_7155870() {
    run(include_str!("vectors/c28_loan_pay_lp_lx7_overpayment_flag_on_last_payment_devnet_7155870.json"));
}

/// 5BF8D5822A54… (xrp) lp.LX8.p1. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_lx8_p1_devnet_7155879() {
    run(include_str!("vectors/c28_loan_pay_lp_lx8_p1_devnet_7155879.json"));
}

/// C7A15596331E… (xrp) lp.LX9.both_payments. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_lx9_both_payments_devnet_7155881() {
    run(include_str!("vectors/c28_loan_pay_lp_lx9_both_payments_devnet_7155881.json"));
}

/// 7FEBA9426B17… (xrp) lp.LX10.final. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_lx10_final_devnet_7155884() {
    run(include_str!("vectors/c28_loan_pay_lp_lx10_final_devnet_7155884.json"));
}

/// 3AD7FF9CCCF2… (iou) lp.LI1.p1.exact. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_li1_p1_exact_devnet_7155890() {
    run(include_str!("vectors/c28_loan_pay_lp_li1_p1_exact_devnet_7155890.json"));
}

/// 8BFDA5A4BC76… (iou) lp.LI1.p2-3.two_at_once. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_li1_p2_3_two_at_once_devnet_7155894() {
    run(include_str!("vectors/c28_loan_pay_lp_li1_p2_3_two_at_once_devnet_7155894.json"));
}

/// 22E28D378715… (iou) lp.LI1.overpayment. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_li1_overpayment_devnet_7155896() {
    run(include_str!("vectors/c28_loan_pay_lp_li1_overpayment_devnet_7155896.json"));
}

/// 304531F0BFB3… (iou) lp.LI1.owner_deep_frozen.fee_to_cover. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_li1_owner_deep_frozen_fee_to_cover_devnet_7155899() {
    run(include_str!("vectors/c28_loan_pay_lp_li1_owner_deep_frozen_fee_to_cover_devnet_7155899.json"));
}

/// D3FA12C09A42… (mpt) lp.LM1.p1.exact. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_lm1_p1_exact_devnet_7155904() {
    run(include_str!("vectors/c28_loan_pay_lp_lm1_p1_exact_devnet_7155904.json"));
}

/// 4D24575D4C07… (mpt) lp.LM1.tec.insufficient_payment. Devnet: tecINSUFFICIENT_PAYMENT.
#[test]
fn c28_loan_pay_lp_lm1_tec_insufficient_payment_devnet_7155906() {
    run(include_str!("vectors/c28_loan_pay_lp_lm1_tec_insufficient_payment_devnet_7155906.json"));
}

/// 891688E1D04C… (mpt) lp.LM1.overpayment. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_lm1_overpayment_devnet_7155908() {
    run(include_str!("vectors/c28_loan_pay_lp_lm1_overpayment_devnet_7155908.json"));
}

/// 553F6A3C1680… (mpt) lp.LM1.tec.locked.borrower. Devnet: tecLOCKED.
#[test]
fn c28_loan_pay_lp_lm1_tec_locked_borrower_devnet_7155912() {
    run(include_str!("vectors/c28_loan_pay_lp_lm1_tec_locked_borrower_devnet_7155912.json"));
}

/// 2ACB389F35FC… (mpt) lp.LM1.cover_at_minimum. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_lm1_cover_at_minimum_devnet_7155916() {
    run(include_str!("vectors/c28_loan_pay_lp_lm1_cover_at_minimum_devnet_7155916.json"));
}

/// EA5A22277951… (mpt) lp.LM1.full_early_payoff. Devnet: tecKILLED.
#[test]
fn c28_loan_pay_lp_lm1_full_early_payoff_devnet_7155919() {
    run(include_str!("vectors/c28_loan_pay_lp_lm1_full_early_payoff_devnet_7155919.json"));
}

/// D309EB7AB6ED… (iou) lp.LI2.p1.exact. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_li2_p1_exact_devnet_7155937() {
    run(include_str!("vectors/c28_loan_pay_lp_li2_p1_exact_devnet_7155937.json"));
}

/// 80F9A8C7EA5D… (iou) lp.LI2.tec.frozen.borrower. Devnet: tecFROZEN.
#[test]
fn c28_loan_pay_lp_li2_tec_frozen_borrower_devnet_7155941() {
    run(include_str!("vectors/c28_loan_pay_lp_li2_tec_frozen_borrower_devnet_7155941.json"));
}

/// 899D151113E4… (iou) lp.LI2.cover_at_minimum. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_li2_cover_at_minimum_devnet_7155955() {
    run(include_str!("vectors/c28_loan_pay_lp_li2_cover_at_minimum_devnet_7155955.json"));
}

/// 905C374695CE… (iou) lp.LI2.full_early_payoff. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_li2_full_early_payoff_devnet_7155959() {
    run(include_str!("vectors/c28_loan_pay_lp_li2_full_early_payoff_devnet_7155959.json"));
}

/// 71FC32C08720… (iou) lp.LI6.issuer.p1. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_li6_issuer_p1_devnet_7155965() {
    run(include_str!("vectors/c28_loan_pay_lp_li6_issuer_p1_devnet_7155965.json"));
}

/// 5310B098C1B7… (iou) lp.LI6.issuer.final. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_li6_issuer_final_devnet_7155966() {
    run(include_str!("vectors/c28_loan_pay_lp_li6_issuer_final_devnet_7155966.json"));
}

/// 94B7A30C8FD2… (iou) lp.LI7.final.issuer_owned_broker. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_li7_final_issuer_owned_broker_devnet_7155969() {
    run(include_str!("vectors/c28_loan_pay_lp_li7_final_issuer_owned_broker_devnet_7155969.json"));
}

/// 1E9DB5FD649C… (iou) lp.LI8.p1. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_li8_p1_devnet_7155974() {
    run(include_str!("vectors/c28_loan_pay_lp_li8_p1_devnet_7155974.json"));
}

/// 2D21755E0363… (iou) lp.LI8.final. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_li8_final_devnet_7155976() {
    run(include_str!("vectors/c28_loan_pay_lp_li8_final_devnet_7155976.json"));
}

/// 07C811003CC0… (mpt) lp.LM1.final. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_lm1_final_devnet_7155982() {
    run(include_str!("vectors/c28_loan_pay_lp_lm1_final_devnet_7155982.json"));
}

/// DD834CAF83B9… (mpt) lp.LM4.final. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_lm4_final_devnet_7155990() {
    run(include_str!("vectors/c28_loan_pay_lp_lm4_final_devnet_7155990.json"));
}

/// E32E57ED95D7… (iou) lp.SLI2.final.on_time. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_sli2_final_on_time_devnet_7156058() {
    run(include_str!("vectors/c28_loan_pay_lp_sli2_final_on_time_devnet_7156058.json"));
}

/// 73EAB120B67B… (iou) lp.SLI.late.impaired.in_redemption. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_sli_late_impaired_in_redemption_devnet_7156186() {
    run(include_str!("vectors/c28_loan_pay_lp_sli_late_impaired_in_redemption_devnet_7156186.json"));
}

/// 28BFD48A5644… (xrp) lp.MX1.0.regular. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_mx1_0_regular_devnet_7156344() {
    run(include_str!("vectors/c28_loan_pay_lp_mx1_0_regular_devnet_7156344.json"));
}

/// 4F372684755B… (xrp) lp.MX1.1.overpayment. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_mx1_1_overpayment_devnet_7156346() {
    run(include_str!("vectors/c28_loan_pay_lp_mx1_1_overpayment_devnet_7156346.json"));
}

/// 2FA878CDD0F5… (xrp) lp.MX1.2.full. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_mx1_2_full_devnet_7156347() {
    run(include_str!("vectors/c28_loan_pay_lp_mx1_2_full_devnet_7156347.json"));
}

/// 4AFB673CE5F6… (xrp) lp.MX3.0.regular. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_mx3_0_regular_devnet_7156351() {
    run(include_str!("vectors/c28_loan_pay_lp_mx3_0_regular_devnet_7156351.json"));
}

/// CB3379E950AC… (xrp) lp.MX3.1.full. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_mx3_1_full_devnet_7156353() {
    run(include_str!("vectors/c28_loan_pay_lp_mx3_1_full_devnet_7156353.json"));
}

/// 289BD4ECDEE6… (xrp) lp.MX4.0.x10. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_mx4_0_x10_devnet_7156357() {
    run(include_str!("vectors/c28_loan_pay_lp_mx4_0_x10_devnet_7156357.json"));
}

/// FDB6FFA86D0F… (xrp) lp.MX4.1.overpayment. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_mx4_1_overpayment_devnet_7156359() {
    run(include_str!("vectors/c28_loan_pay_lp_mx4_1_overpayment_devnet_7156359.json"));
}

/// 4B614AF25967… (xrp) lp.MX4.2.full. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_mx4_2_full_devnet_7156361() {
    run(include_str!("vectors/c28_loan_pay_lp_mx4_2_full_devnet_7156361.json"));
}

/// 6704607723DE… (iou) lp.MI1.0.regular. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_mi1_0_regular_devnet_7156365() {
    run(include_str!("vectors/c28_loan_pay_lp_mi1_0_regular_devnet_7156365.json"));
}

/// 91BE1C4600C1… (iou) lp.MI1.1.x2. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_mi1_1_x2_devnet_7156367() {
    run(include_str!("vectors/c28_loan_pay_lp_mi1_1_x2_devnet_7156367.json"));
}

/// D7AE75310FA5… (iou) lp.MI1.2.overpayment. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_mi1_2_overpayment_devnet_7156369() {
    run(include_str!("vectors/c28_loan_pay_lp_mi1_2_overpayment_devnet_7156369.json"));
}

/// 1A6BF1D0CBEC… (iou) lp.MI1.3.full. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_mi1_3_full_devnet_7156371() {
    run(include_str!("vectors/c28_loan_pay_lp_mi1_3_full_devnet_7156371.json"));
}

/// 67426FBD561B… (iou) lp.MI3.0.regular. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_mi3_0_regular_devnet_7156375() {
    run(include_str!("vectors/c28_loan_pay_lp_mi3_0_regular_devnet_7156375.json"));
}

/// AAF900E9D019… (iou) lp.MI4.0.regular. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_mi4_0_regular_devnet_7156379() {
    run(include_str!("vectors/c28_loan_pay_lp_mi4_0_regular_devnet_7156379.json"));
}

/// 1B3007685125… (iou) lp.MI5.0.regular. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_mi5_0_regular_devnet_7156383() {
    run(include_str!("vectors/c28_loan_pay_lp_mi5_0_regular_devnet_7156383.json"));
}

/// 5BB49C949A3D… (iou) lp.MI5.1.x5. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_mi5_1_x5_devnet_7156385() {
    run(include_str!("vectors/c28_loan_pay_lp_mi5_1_x5_devnet_7156385.json"));
}

/// DF10B820FA64… (iou) lp.MI5.2.overpayment. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_mi5_2_overpayment_devnet_7156387() {
    run(include_str!("vectors/c28_loan_pay_lp_mi5_2_overpayment_devnet_7156387.json"));
}

/// 19C9BD4DFA92… (mpt) lp.MM1.0.regular. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_mm1_0_regular_devnet_7156390() {
    run(include_str!("vectors/c28_loan_pay_lp_mm1_0_regular_devnet_7156390.json"));
}

/// 1BD9D752E3DF… (mpt) lp.MM1.1.overpayment. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_mm1_1_overpayment_devnet_7156392() {
    run(include_str!("vectors/c28_loan_pay_lp_mm1_1_overpayment_devnet_7156392.json"));
}

/// 883E9ED1F4EE… (mpt) lp.MM2.0.regular. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_mm2_0_regular_devnet_7156397() {
    run(include_str!("vectors/c28_loan_pay_lp_mm2_0_regular_devnet_7156397.json"));
}

/// B6E18589FABF… (mpt) lp.MM2.1.x16. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_mm2_1_x16_devnet_7156399() {
    run(include_str!("vectors/c28_loan_pay_lp_mm2_1_x16_devnet_7156399.json"));
}

/// 81A6A89BA2A1… (xrp) lp.LX8.sponsored.fee. Devnet: tesSUCCESS.
#[test]
#[ignore = "finding S1 (campaign 27): the fee payer is the sponsor; featureSponsor is not ported"]
fn c28_loan_pay_lp_lx8_sponsored_fee_devnet_7156446() {
    run(include_str!("vectors/c28_loan_pay_lp_lx8_sponsored_fee_devnet_7156446.json"));
}

/// 304FB954B182… (iou) lp.MI5.cover_below_minimum.fee_to_cover. Devnet: tesSUCCESS.
#[test]
fn c28_loan_pay_lp_mi5_cover_below_minimum_fee_to_cover_devnet_7156480() {
    run(include_str!("vectors/c28_loan_pay_lp_mi5_cover_below_minimum_fee_to_cover_devnet_7156480.json"));
}


// ---------------------------------------------------------------------------------------------------------------
// LoanManage
// ---------------------------------------------------------------------------------------------------------------

/// 3D366535FBBB… (xrp) lm.TB.tec.too_soon.impair_not_late. Devnet: tecTOO_SOON.
#[test]
fn c28_loan_manage_lm_tb_tec_too_soon_impair_not_late_devnet_7155670() {
    run(include_str!("vectors/c28_loan_manage_lm_tb_tec_too_soon_impair_not_late_devnet_7155670.json"));
}

/// A9A80F906ECF… (xrp) lm.TC.tec.no_permission.unimpair_not_impaired. Devnet: tecNO_PERMISSION.
#[test]
fn c28_loan_manage_lm_tc_tec_no_permission_unimpair_not_impaired_devnet_7155674() {
    run(include_str!("vectors/c28_loan_manage_lm_tc_tec_no_permission_unimpair_not_impaired_devnet_7155674.json"));
}

/// 0E6A510A01EF… (xrp) lm.TB.impair. Devnet: tesSUCCESS.
#[test]
fn c28_loan_manage_lm_tb_impair_devnet_7155689() {
    run(include_str!("vectors/c28_loan_manage_lm_tb_impair_devnet_7155689.json"));
}

/// 31250B76EBCC… (xrp) lm.TB.unimpair. Devnet: tesSUCCESS.
#[test]
fn c28_loan_manage_lm_tb_unimpair_devnet_7155692() {
    run(include_str!("vectors/c28_loan_manage_lm_tb_unimpair_devnet_7155692.json"));
}

/// 6059B3851B8A… (xrp) lm.TB.impair.again. Devnet: tesSUCCESS.
#[test]
fn c28_loan_manage_lm_tb_impair_again_devnet_7155694() {
    run(include_str!("vectors/c28_loan_manage_lm_tb_impair_again_devnet_7155694.json"));
}

/// 7BA97E1330F0… (iou) lm.TG.impair.iou. Devnet: tesSUCCESS.
#[test]
fn c28_loan_manage_lm_tg_impair_iou_devnet_7155695() {
    run(include_str!("vectors/c28_loan_manage_lm_tg_impair_iou_devnet_7155695.json"));
}

/// BB524945C2BE… (mpt) lm.TH.impair.mpt. Devnet: tesSUCCESS.
#[test]
fn c28_loan_manage_lm_th_impair_mpt_devnet_7155697() {
    run(include_str!("vectors/c28_loan_manage_lm_th_impair_mpt_devnet_7155697.json"));
}

/// 39E1C93A7D32… (xrp) lm.TE.impair.no_cover_broker. Devnet: tesSUCCESS.
#[test]
fn c28_loan_manage_lm_te_impair_no_cover_broker_devnet_7155699() {
    run(include_str!("vectors/c28_loan_manage_lm_te_impair_no_cover_broker_devnet_7155699.json"));
}

/// E748D2288FA4… (xrp) lm.TC.default.cover_enough. Devnet: tesSUCCESS.
#[test]
fn c28_loan_manage_lm_tc_default_cover_enough_devnet_7155728() {
    run(include_str!("vectors/c28_loan_manage_lm_tc_default_cover_enough_devnet_7155728.json"));
}

/// 577ACDDB9146… (xrp) lm.TE.default.impaired.no_cover. Devnet: tesSUCCESS.
#[test]
fn c28_loan_manage_lm_te_default_impaired_no_cover_devnet_7155737() {
    run(include_str!("vectors/c28_loan_manage_lm_te_default_impaired_no_cover_devnet_7155737.json"));
}

/// 430098BEBED5… (iou) lm.TG.default.impaired.iou. Devnet: tesSUCCESS.
#[test]
fn c28_loan_manage_lm_tg_default_impaired_iou_devnet_7155739() {
    run(include_str!("vectors/c28_loan_manage_lm_tg_default_impaired_iou_devnet_7155739.json"));
}

/// 162933AC3041… (mpt) lm.TH.unimpair.mpt. Devnet: tesSUCCESS.
#[test]
fn c28_loan_manage_lm_th_unimpair_mpt_devnet_7155741() {
    run(include_str!("vectors/c28_loan_manage_lm_th_unimpair_mpt_devnet_7155741.json"));
}

/// 74486D9C2F71… (mpt) lm.TH.default.mpt. Devnet: tesSUCCESS.
#[test]
fn c28_loan_manage_lm_th_default_mpt_devnet_7155742() {
    run(include_str!("vectors/c28_loan_manage_lm_th_default_mpt_devnet_7155742.json"));
}

/// 6949690BB4DC… (na) lm.tec.no_entry. Devnet: tecNO_ENTRY.
#[test]
fn c28_loan_manage_lm_tec_no_entry_devnet_7155794() {
    run(include_str!("vectors/c28_loan_manage_lm_tec_no_entry_devnet_7155794.json"));
}

/// 410FF9DD6BF7… (xrp) lm.LX1.no_flag. Devnet: tesSUCCESS.
#[test]
fn c28_loan_manage_lm_lx1_no_flag_devnet_7155796() {
    run(include_str!("vectors/c28_loan_manage_lm_lx1_no_flag_devnet_7155796.json"));
}

/// 058A2D69DDB2… (xrp) lm.SLX.impair. Devnet: tesSUCCESS.
#[test]
fn c28_loan_manage_lm_slx_impair_devnet_7156074() {
    run(include_str!("vectors/c28_loan_manage_lm_slx_impair_devnet_7156074.json"));
}

/// 78CC72B0CDC1… (iou) lm.SLI.impair. Devnet: tesSUCCESS.
#[test]
fn c28_loan_manage_lm_sli_impair_devnet_7156076() {
    run(include_str!("vectors/c28_loan_manage_lm_sli_impair_devnet_7156076.json"));
}

/// E6B7CA8DC6F3… (mpt) lm.SLM.impair. Devnet: tesSUCCESS.
#[test]
fn c28_loan_manage_lm_slm_impair_devnet_7156077() {
    run(include_str!("vectors/c28_loan_manage_lm_slm_impair_devnet_7156077.json"));
}

/// 970B5DD9CECA… (xrp) lm.SLX.default.cover_short. Devnet: tesSUCCESS.
#[test]
fn c28_loan_manage_lm_slx_default_cover_short_devnet_7156087() {
    run(include_str!("vectors/c28_loan_manage_lm_slx_default_cover_short_devnet_7156087.json"));
}

/// 827FD9B51510… (mpt) lm.SLM.default.impaired.cover_full. Devnet: tesSUCCESS.
#[test]
fn c28_loan_manage_lm_slm_default_impaired_cover_full_devnet_7156188() {
    run(include_str!("vectors/c28_loan_manage_lm_slm_default_impaired_cover_full_devnet_7156188.json"));
}

/// BBF45AB19114… (iou) lm.XC1.default.cover_at_minimum. Devnet: tesSUCCESS.
#[test]
fn c28_loan_manage_lm_xc1_default_cover_at_minimum_devnet_7156478() {
    run(include_str!("vectors/c28_loan_manage_lm_xc1_default_cover_at_minimum_devnet_7156478.json"));
}


// ---------------------------------------------------------------------------------------------------------------
// LoanDelete
// ---------------------------------------------------------------------------------------------------------------

/// E0CD387C1344… (xrp) ld.tec.no_permission.stranger. Devnet: tecNO_PERMISSION.
#[test]
fn c28_loan_delete_ld_tec_no_permission_stranger_devnet_7155744() {
    run(include_str!("vectors/c28_loan_delete_ld_tec_no_permission_stranger_devnet_7155744.json"));
}

/// 23867C51643A… (xrp) ld.TC.defaulted.by_borrower. Devnet: tesSUCCESS.
#[test]
fn c28_loan_delete_ld_tc_defaulted_by_borrower_devnet_7155746() {
    run(include_str!("vectors/c28_loan_delete_ld_tc_defaulted_by_borrower_devnet_7155746.json"));
}

/// 8A0262899023… (xrp) ld.TE.defaulted.by_broker_owner. Devnet: tesSUCCESS.
#[test]
fn c28_loan_delete_ld_te_defaulted_by_broker_owner_devnet_7155748() {
    run(include_str!("vectors/c28_loan_delete_ld_te_defaulted_by_broker_owner_devnet_7155748.json"));
}

/// C6E27A084ABB… (iou) ld.TG.defaulted.iou. Devnet: tesSUCCESS.
#[test]
fn c28_loan_delete_ld_tg_defaulted_iou_devnet_7155750() {
    run(include_str!("vectors/c28_loan_delete_ld_tg_defaulted_iou_devnet_7155750.json"));
}

/// 91976A6099A8… (mpt) ld.TH.defaulted.mpt. Devnet: tesSUCCESS.
#[test]
fn c28_loan_delete_ld_th_defaulted_mpt_devnet_7155752() {
    run(include_str!("vectors/c28_loan_delete_ld_th_defaulted_mpt_devnet_7155752.json"));
}

/// 7078C23B7FD7… (xrp) ld.TA.paid.by_borrower. Devnet: tesSUCCESS.
#[test]
fn c28_loan_delete_ld_ta_paid_by_borrower_devnet_7155753() {
    run(include_str!("vectors/c28_loan_delete_ld_ta_paid_by_borrower_devnet_7155753.json"));
}

/// F8CBA97C78B5… (xrp) ld.TB.paid.by_broker_owner. Devnet: tesSUCCESS.
#[test]
fn c28_loan_delete_ld_tb_paid_by_broker_owner_devnet_7155755() {
    run(include_str!("vectors/c28_loan_delete_ld_tb_paid_by_broker_owner_devnet_7155755.json"));
}

/// DEEB52D516B4… (na) ld.tec.no_entry.deleted. Devnet: tecNO_ENTRY.
#[test]
fn c28_loan_delete_ld_tec_no_entry_deleted_devnet_7155757() {
    run(include_str!("vectors/c28_loan_delete_ld_tec_no_entry_deleted_devnet_7155757.json"));
}

/// 3837EB96FCCD… (xrp) ld.LX1.tec.has_obligations. Devnet: tecHAS_OBLIGATIONS.
#[test]
fn c28_loan_delete_ld_lx1_tec_has_obligations_devnet_7155801() {
    run(include_str!("vectors/c28_loan_delete_ld_lx1_tec_has_obligations_devnet_7155801.json"));
}

/// FA74B936C958… (xrp) ld.LX1.paid.by_borrower. Devnet: tesSUCCESS.
#[test]
fn c28_loan_delete_ld_lx1_paid_by_borrower_devnet_7155822() {
    run(include_str!("vectors/c28_loan_delete_ld_lx1_paid_by_borrower_devnet_7155822.json"));
}

/// E5F68BE91616… (xrp) ld.LX6.paid.by_broker_owner.2. Devnet: tesSUCCESS.
#[test]
fn c28_loan_delete_ld_lx6_paid_by_broker_owner_2_devnet_7155864() {
    run(include_str!("vectors/c28_loan_delete_ld_lx6_paid_by_broker_owner_2_devnet_7155864.json"));
}

/// 8E70EA9672B9… (xrp) ld.LX9. Devnet: tesSUCCESS.
#[test]
fn c28_loan_delete_ld_lx9_devnet_7155882() {
    run(include_str!("vectors/c28_loan_delete_ld_lx9_devnet_7155882.json"));
}

/// 37A191DF66BA… (mpt) ld.LM1.paid. Devnet: tecHAS_OBLIGATIONS.
#[test]
fn c28_loan_delete_ld_lm1_paid_devnet_7155921() {
    run(include_str!("vectors/c28_loan_delete_ld_lm1_paid_devnet_7155921.json"));
}

/// 82A3AD832C4F… (iou) ld.LI1.paid. Devnet: tesSUCCESS.
#[test]
fn c28_loan_delete_ld_li1_paid_devnet_7155934() {
    run(include_str!("vectors/c28_loan_delete_ld_li1_paid_devnet_7155934.json"));
}

/// 1AD5A1846B3A… (iou) ld.LI2.paid. Devnet: tesSUCCESS.
#[test]
fn c28_loan_delete_ld_li2_paid_devnet_7155961() {
    run(include_str!("vectors/c28_loan_delete_ld_li2_paid_devnet_7155961.json"));
}

/// D72124572AE0… (iou) ld.LI6.issuer. Devnet: tesSUCCESS.
#[test]
fn c28_loan_delete_ld_li6_issuer_devnet_7155967() {
    run(include_str!("vectors/c28_loan_delete_ld_li6_issuer_devnet_7155967.json"));
}

/// 56597EF625C8… (iou) ld.LI7.by_issuer_owner. Devnet: tesSUCCESS.
#[test]
fn c28_loan_delete_ld_li7_by_issuer_owner_devnet_7155971() {
    run(include_str!("vectors/c28_loan_delete_ld_li7_by_issuer_owner_devnet_7155971.json"));
}

/// B2B943DAADA8… (iou) ld.LI8. Devnet: tesSUCCESS.
#[test]
fn c28_loan_delete_ld_li8_devnet_7155978() {
    run(include_str!("vectors/c28_loan_delete_ld_li8_devnet_7155978.json"));
}

/// B2332F42776B… (mpt) ld.LM1.paid. Devnet: tesSUCCESS.
#[test]
fn c28_loan_delete_ld_lm1_paid_devnet_7155984() {
    run(include_str!("vectors/c28_loan_delete_ld_lm1_paid_devnet_7155984.json"));
}

/// 62E4607FD55A… (mpt) ld.LM4. Devnet: tesSUCCESS.
#[test]
fn c28_loan_delete_ld_lm4_devnet_7155992() {
    run(include_str!("vectors/c28_loan_delete_ld_lm4_devnet_7155992.json"));
}

/// BD3F028CBE7A… (iou) ld.SLI2. Devnet: tesSUCCESS.
#[test]
fn c28_loan_delete_ld_sli2_devnet_7156060() {
    run(include_str!("vectors/c28_loan_delete_ld_sli2_devnet_7156060.json"));
}

/// 4B806CDBA12F… (xrp) ld.SLX. Devnet: tesSUCCESS.
#[test]
fn c28_loan_delete_ld_slx_devnet_7156190() {
    run(include_str!("vectors/c28_loan_delete_ld_slx_devnet_7156190.json"));
}

/// 6F3D447C5E10… (iou) ld.SLI. Devnet: tesSUCCESS.
#[test]
fn c28_loan_delete_ld_sli_devnet_7156191() {
    run(include_str!("vectors/c28_loan_delete_ld_sli_devnet_7156191.json"));
}

/// 1440C0F781AB… (mpt) ld.SLM. Devnet: tesSUCCESS.
#[test]
fn c28_loan_delete_ld_slm_devnet_7156193() {
    run(include_str!("vectors/c28_loan_delete_ld_slm_devnet_7156193.json"));
}

/// E1674EBD422F… (iou) ld.XC1.defaulted. Devnet: tesSUCCESS.
#[test]
fn c28_loan_delete_ld_xc1_defaulted_devnet_7156485() {
    run(include_str!("vectors/c28_loan_delete_ld_xc1_defaulted_devnet_7156485.json"));
}


// ---------------------------------------------------------------------------------------------------------------
// VaultSet
// ---------------------------------------------------------------------------------------------------------------

/// A1FA27F17B62… (xrp) vset.tec.limit.below_total. Devnet: tecLIMIT_EXCEEDED.
#[test]
fn c28_vault_set_vset_tec_limit_below_total_devnet_7155777() {
    run(include_str!("vectors/c28_vault_set_vset_tec_limit_below_total_devnet_7155777.json"));
}

/// B9FB72D118FD… (iou) vset.SI.loss_unrealized. Devnet: tesSUCCESS.
#[test]
fn c28_vault_set_vset_si_loss_unrealized_devnet_7156085() {
    run(include_str!("vectors/c28_vault_set_vset_si_loss_unrealized_devnet_7156085.json"));
}

/// 481054DF9105… (mpt) vset.VM.max_equals_total. Devnet: tesSUCCESS.
#[test]
fn c28_vault_set_vset_vm_max_equals_total_devnet_7156438() {
    run(include_str!("vectors/c28_vault_set_vset_vm_max_equals_total_devnet_7156438.json"));
}

/// C6E347DB0A7B… (mpt) vset.VM.max_zero. Devnet: tesSUCCESS.
#[test]
fn c28_vault_set_vset_vm_max_zero_devnet_7156442() {
    run(include_str!("vectors/c28_vault_set_vset_vm_max_zero_devnet_7156442.json"));
}


// ---------------------------------------------------------------------------------------------------------------
// VaultDeposit
// ---------------------------------------------------------------------------------------------------------------

/// 2CAB2C087FED… (xrp) vdep.tec.expired.investment. Devnet: tecEXPIRED.
#[test]
fn c28_vault_deposit_vdep_tec_expired_investment_devnet_7155781() {
    run(include_str!("vectors/c28_vault_deposit_vdep_tec_expired_investment_devnet_7155781.json"));
}

/// BFC03C2A17DB… (xrp) vdep.SX.tec.expired.investment. Devnet: tecEXPIRED.
#[test]
fn c28_vault_deposit_vdep_sx_tec_expired_investment_devnet_7156056() {
    run(include_str!("vectors/c28_vault_deposit_vdep_sx_tec_expired_investment_devnet_7156056.json"));
}

/// 7E37089D027E… (iou) vdep.SI.tec.expired.redemption. Devnet: tecEXPIRED.
#[test]
fn c28_vault_deposit_vdep_si_tec_expired_redemption_devnet_7156168() {
    run(include_str!("vectors/c28_vault_deposit_vdep_si_tec_expired_redemption_devnet_7156168.json"));
}


// ---------------------------------------------------------------------------------------------------------------
// VaultWithdraw
// ---------------------------------------------------------------------------------------------------------------

/// 1439A94751B5… (xrp) vwd.tec.too_soon.investment. Devnet: tecTOO_SOON.
#[test]
fn c28_vault_withdraw_vwd_tec_too_soon_investment_devnet_7155779() {
    run(include_str!("vectors/c28_vault_withdraw_vwd_tec_too_soon_investment_devnet_7155779.json"));
}

/// 0CADEF29C600… (xrp) vwd.SX.tec.too_soon.investment. Devnet: tecTOO_SOON.
#[test]
fn c28_vault_withdraw_vwd_sx_tec_too_soon_investment_devnet_7156055() {
    run(include_str!("vectors/c28_vault_withdraw_vwd_sx_tec_too_soon_investment_devnet_7156055.json"));
}

/// 48238332120D… (xrp) vwd.SX.after_realized_loss.assets. Devnet: tesSUCCESS.
#[test]
fn c28_vault_withdraw_vwd_sx_after_realized_loss_assets_devnet_7156170() {
    run(include_str!("vectors/c28_vault_withdraw_vwd_sx_after_realized_loss_assets_devnet_7156170.json"));
}

/// E26CB95E4D91… (xrp) vwd.SX.after_realized_loss.shares. Devnet: tesSUCCESS.
#[test]
fn c28_vault_withdraw_vwd_sx_after_realized_loss_shares_devnet_7156172() {
    run(include_str!("vectors/c28_vault_withdraw_vwd_sx_after_realized_loss_shares_devnet_7156172.json"));
}

/// B1440B8504D8… (iou) vwd.SI.loss_unrealized.assets. Devnet: tesSUCCESS.
#[test]
fn c28_vault_withdraw_vwd_si_loss_unrealized_assets_devnet_7156173() {
    run(include_str!("vectors/c28_vault_withdraw_vwd_si_loss_unrealized_assets_devnet_7156173.json"));
}

/// F6D4E067B3A5… (iou) vwd.SI.loss_unrealized.shares. Devnet: tesSUCCESS.
#[test]
fn c28_vault_withdraw_vwd_si_loss_unrealized_shares_devnet_7156175() {
    run(include_str!("vectors/c28_vault_withdraw_vwd_si_loss_unrealized_shares_devnet_7156175.json"));
}

/// AD30B1D9625D… (iou) vwd.SI.tec.insufficient.more_than_available. Devnet: tecINSUFFICIENT_FUNDS.
#[test]
fn c28_vault_withdraw_vwd_si_tec_insufficient_more_than_available_devnet_7156177() {
    run(include_str!("vectors/c28_vault_withdraw_vwd_si_tec_insufficient_more_than_available_devnet_7156177.json"));
}

/// 3FEA301AFA5A… (mpt) vwd.SM.loss_unrealized.assets. Devnet: tesSUCCESS.
#[test]
fn c28_vault_withdraw_vwd_sm_loss_unrealized_assets_devnet_7156179() {
    run(include_str!("vectors/c28_vault_withdraw_vwd_sm_loss_unrealized_assets_devnet_7156179.json"));
}

/// 294C17181000… (mpt) vwd.SM.all_available.loss_unrealized. Devnet: tesSUCCESS.
#[test]
fn c28_vault_withdraw_vwd_sm_all_available_loss_unrealized_devnet_7156180() {
    run(include_str!("vectors/c28_vault_withdraw_vwd_sm_all_available_loss_unrealized_devnet_7156180.json"));
}

/// C9F3F737FC40… (mpt) vwd.SM.tec.insufficient.nothing_available. Devnet: tesSUCCESS.
#[test]
fn c28_vault_withdraw_vwd_sm_tec_insufficient_nothing_available_devnet_7156182() {
    run(include_str!("vectors/c28_vault_withdraw_vwd_sm_tec_insufficient_nothing_available_devnet_7156182.json"));
}

/// 625863F5D90F… (xrp) vwd.SX.last.after_loss. Devnet: tesSUCCESS.
#[test]
fn c28_vault_withdraw_vwd_sx_last_after_loss_devnet_7156195() {
    run(include_str!("vectors/c28_vault_withdraw_vwd_sx_last_after_loss_devnet_7156195.json"));
}


// ---------------------------------------------------------------------------------------------------------------
// VaultClawback
// ---------------------------------------------------------------------------------------------------------------

/// 2BF1ADC1E07D… (iou) vclaw.SI.partial.loss_unrealized. Devnet: tesSUCCESS.
#[test]
fn c28_vault_clawback_vclaw_si_partial_loss_unrealized_devnet_7156079() {
    run(include_str!("vectors/c28_vault_clawback_vclaw_si_partial_loss_unrealized_devnet_7156079.json"));
}

/// D175F2FF3F45… (mpt) vclaw.SM.partial.loss_unrealized. Devnet: tesSUCCESS.
#[test]
fn c28_vault_clawback_vclaw_sm_partial_loss_unrealized_devnet_7156081() {
    run(include_str!("vectors/c28_vault_clawback_vclaw_sm_partial_loss_unrealized_devnet_7156081.json"));
}

/// 66B4A4DB79A2… (iou) vclaw.SI.more_than_available.loss_unrealized. Devnet: tecINSUFFICIENT_FUNDS.
#[test]
fn c28_vault_clawback_vclaw_si_more_than_available_loss_unrealized_devnet_7156083() {
    run(include_str!("vectors/c28_vault_clawback_vclaw_si_more_than_available_loss_unrealized_devnet_7156083.json"));
}

/// 816B3C13BD18… (iou) vclaw.SI.all.loss_unrealized. Devnet: tesSUCCESS.
#[test]
fn c28_vault_clawback_vclaw_si_all_loss_unrealized_devnet_7156184() {
    run(include_str!("vectors/c28_vault_clawback_vclaw_si_all_loss_unrealized_devnet_7156184.json"));
}


// ---------------------------------------------------------------------------------------------------------------
// VaultDelete
// ---------------------------------------------------------------------------------------------------------------

/// 7AB07825D236… (xrp) vdel.tec.has_obligations.brokers. Devnet: tecHAS_OBLIGATIONS.
#[test]
fn c28_vault_delete_vdel_tec_has_obligations_brokers_devnet_7155775() {
    run(include_str!("vectors/c28_vault_delete_vdel_tec_has_obligations_brokers_devnet_7155775.json"));
}

/// E4EF35B683D9… (xrp) vdel.SX.tec.has_obligations.brokers. Devnet: tecHAS_OBLIGATIONS.
#[test]
fn c28_vault_delete_vdel_sx_tec_has_obligations_brokers_devnet_7156197() {
    run(include_str!("vectors/c28_vault_delete_vdel_sx_tec_has_obligations_brokers_devnet_7156197.json"));
}

/// 066573A7E073… (xrp) vdel.SX.after_lending. Devnet: tesSUCCESS.
#[test]
fn c28_vault_delete_vdel_sx_after_lending_devnet_7156203() {
    run(include_str!("vectors/c28_vault_delete_vdel_sx_after_lending_devnet_7156203.json"));
}

/// AF57A2180D9B… (iou) vdel.SI.after_lending. Devnet: tesSUCCESS.
#[test]
fn c28_vault_delete_vdel_si_after_lending_devnet_7156213() {
    run(include_str!("vectors/c28_vault_delete_vdel_si_after_lending_devnet_7156213.json"));
}

/// 10F10272CA6B… (mpt) vdel.SM.after_lending. Devnet: tesSUCCESS.
#[test]
fn c28_vault_delete_vdel_sm_after_lending_devnet_7156215() {
    run(include_str!("vectors/c28_vault_delete_vdel_sm_after_lending_devnet_7156215.json"));
}
