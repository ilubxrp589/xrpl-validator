//! Campaign 27 (devnet, 2026-11-21, ledgers 7133688–7135650) byte-exact vectors: SingleAssetVault edge states.
//! 806 real vault transactions were replayed (801 byte-exact, 5 fee-sponsored ones failing on the unported
//! featureSponsor); these are the bundles the campaign report recommends keeping, plus one for each
//! (transaction type, result) pair the recommendation did not already cover:
//!
//! - VaultDelete in every reachable shape (empty XRP / IOU / MPT / private, MemoData and Memos, after withdrawals
//!   and clawbacks, frozen / locked pseudo-account holdings, reserve edges, Tickets, two-page directories, closed-
//!   ended), and its tec results;
//! - tec results of all six types, with the reserve boundaries at −1 / exact / +1 drop;
//! - the fixCleanup3_5_0 arms as devnet (amendment ON) took them: VaultWithdraw to another holder through an IOU
//!   TransferRate and an MPT TransferFee, AssetsMaximum out of range on VaultSet (tecPRECISION_LOSS);
//! - VaultClawback with an explicit Amount (partial, sub-scale, zero, above the holder's shares);
//! - rounding at Scale 0 / 6 / 18 and at the 2^63 share ceiling.
//!
//! Devnet runs rippled 3.5.0-rc5; every bundle carries its parent ledger's Amendments object. The three
//! co-signed fee-sponsor transactions (finding S1) are pinned `#[ignore]`d: they pass once the fee payer is
//! chosen as `STTx::getFeePayerID` does. Same harness and pass criteria as vault_vector.rs (recorded result,
//! byte-identical objects, no write the metadata does not name).
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
        close_time_resolution: 10,
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
// VaultCreate
// ---------------------------------------------------------------------------------------------------------------

/// 6106BD1A0CBA… (tecINSUFFICIENT_RESERVE): tecINSUFFICIENT_RESERVE one drop under reserve(OwnerCount+3) (the owner's share MPToken is the third object).
#[test]
fn c27_vault_create_create_reserve3_xrp_minus1_devnet_7134180() {
    run(include_str!("vectors/c27_vault_create_create_reserve3_xrp_minus1_devnet_7134180.json"));
}

/// B0F88311FD4D… (tesSUCCESS): exactly reserve(OwnerCount+3): success.
#[test]
fn c27_vault_create_create_reserve3_xrp_exact_devnet_7134186() {
    run(include_str!("vectors/c27_vault_create_create_reserve3_xrp_exact_devnet_7134186.json"));
}

/// 788A95032019… (tecINSUFFICIENT_RESERVE): tecINSUFFICIENT_RESERVE at reserve(OwnerCount+2)+1: passes the vault's check, fails the MPToken's.
#[test]
fn c27_vault_create_create_reserve_xrp_plus1_devnet_7134067() {
    run(include_str!("vectors/c27_vault_create_create_reserve_xrp_plus1_devnet_7134067.json"));
}

/// 91A8DEE5B33A… (tecINSUFFICIENT_RESERVE): owner already owning 2 objects, one drop short.
#[test]
fn c27_vault_create_create_reserve3_oc2_minus1_devnet_7134243() {
    run(include_str!("vectors/c27_vault_create_create_reserve3_oc2_minus1_devnet_7134243.json"));
}

/// 2A68BDD17A38… (tecNO_AUTH): tecNO_AUTH: MPT without CanTransfer.
#[test]
fn c27_vault_create_create_tec_mpt_nontransferable_devnet_7133956() {
    run(include_str!("vectors/c27_vault_create_create_tec_mpt_nontransferable_devnet_7133956.json"));
}

/// CAEE0B9CBFE6… (tecOBJECT_NOT_FOUND): tecOBJECT_NOT_FOUND: no such MPTokenIssuance.
#[test]
fn c27_vault_create_create_tec_mpt_missing_devnet_7133958() {
    run(include_str!("vectors/c27_vault_create_create_tec_mpt_missing_devnet_7133958.json"));
}

/// 30F471D687DF… (tecWRONG_ASSET): tecWRONG_ASSET: the asset is another vault's share.
#[test]
fn c27_vault_create_create_tec_asset_is_vault_share_devnet_7133960() {
    run(include_str!("vectors/c27_vault_create_create_tec_asset_is_vault_share_devnet_7133960.json"));
}

/// 16CF1B787CB3… (tecWRONG_ASSET): tecWRONG_ASSET: IOU issued by a pseudo-account.
#[test]
fn c27_vault_create_create_tec_iou_issuer_pseudo_devnet_7133962() {
    run(include_str!("vectors/c27_vault_create_create_tec_iou_issuer_pseudo_devnet_7133962.json"));
}

/// 171ECB8411FD… (tecOBJECT_NOT_FOUND): tecOBJECT_NOT_FOUND: DomainID names no domain.
#[test]
fn c27_vault_create_create_tec_domain_missing_devnet_7134010() {
    run(include_str!("vectors/c27_vault_create_create_tec_domain_missing_devnet_7134010.json"));
}

/// B2B809EAE934… (tecEXPIRED): tecEXPIRED: SubscriptionDate equal to the parent close time.
#[test]
fn c27_vault_create_create_tec_subscription_now_devnet_7134014() {
    run(include_str!("vectors/c27_vault_create_create_tec_subscription_now_devnet_7134014.json"));
}

/// B5C6BA69675E… (tecFROZEN): tecFROZEN: owner's trust line frozen.
#[test]
fn c27_vault_create_create_tec_owner_line_frozen_devnet_7134101() {
    run(include_str!("vectors/c27_vault_create_create_tec_owner_line_frozen_devnet_7134101.json"));
}

/// 5D6AAE916D9F… (tecFROZEN): tecFROZEN: issuer's global freeze.
#[test]
fn c27_vault_create_create_tec_global_freeze_devnet_7134121() {
    run(include_str!("vectors/c27_vault_create_create_tec_global_freeze_devnet_7134121.json"));
}

/// 2367E07E153D… (tecLOCKED): tecLOCKED: owner's MPToken locked.
#[test]
fn c27_vault_create_create_tec_owner_mptoken_locked_devnet_7134111() {
    run(include_str!("vectors/c27_vault_create_create_tec_owner_mptoken_locked_devnet_7134111.json"));
}

/// CF5D91A83CE6… (tesSUCCESS): XRP vault created with AssetsMaximum 1.5: the vault stores 2.
#[test]
fn c27_vault_create_create_tem_assetsmax_xrp_fraction_devnet_7134134() {
    run(include_str!("vectors/c27_vault_create_create_tem_assetsmax_xrp_fraction_devnet_7134134.json"));
}

/// 8E18124776B8… (tesSUCCESS): VaultCreate with a Ticket: the vault keylet uses the ticket number.
#[test]
fn c27_vault_create_x_create_with_ticket_31_devnet_7134927() {
    run(include_str!("vectors/c27_vault_create_x_create_with_ticket_31_devnet_7134927.json"));
}

/// E7B5DCF6AA68… (tesSUCCESS): owner with 31 objects: vault and share MPToken straddle the directory page boundary.
#[test]
fn c27_vault_create_x_create_page_31_tickets_devnet_7134921() {
    run(include_str!("vectors/c27_vault_create_x_create_page_31_tickets_devnet_7134921.json"));
}

// ---------------------------------------------------------------------------------------------------------------
// VaultSet
// ---------------------------------------------------------------------------------------------------------------

/// 82F4117C8FE6… (tecPRECISION_LOSS): fixCleanup3_5_0: tecPRECISION_LOSS, AssetsMaximum 1e17+1 drops.
#[test]
fn c27_vault_set_set_350_xrp_1e17_1_devnet_7134533() {
    run(include_str!("vectors/c27_vault_set_set_350_xrp_1e17_1_devnet_7134533.json"));
}

/// 2E88F178C672… (tesSUCCESS): fixCleanup3_5_0: AssetsMaximum exactly 1e17 drops accepted.
#[test]
fn c27_vault_set_set_350_xrp_1e17_devnet_7134531() {
    run(include_str!("vectors/c27_vault_set_set_350_xrp_1e17_devnet_7134531.json"));
}

/// 6B928F066C0D… (tecPRECISION_LOSS): fixCleanup3_5_0: tecPRECISION_LOSS, AssetsMaximum 2^63 on MPT.
#[test]
fn c27_vault_set_set_350_mpt_2_63_devnet_7134545() {
    run(include_str!("vectors/c27_vault_set_set_350_mpt_2_63_devnet_7134545.json"));
}

/// 91650E44B944… (tesSUCCESS): fixCleanup3_5_0: AssetsMaximum 2^63-1 on MPT accepted.
#[test]
fn c27_vault_set_set_350_mpt_2_63_1_devnet_7134543() {
    run(include_str!("vectors/c27_vault_set_set_350_mpt_2_63_1_devnet_7134543.json"));
}

/// DC4AB9A2FC62… (tecPRECISION_LOSS): fixCleanup3_5_0: tecPRECISION_LOSS, AssetsMaximum 1e97 on IOU.
#[test]
fn c27_vault_set_set_350_iou_1e97_devnet_7134557() {
    run(include_str!("vectors/c27_vault_set_set_350_iou_1e97_devnet_7134557.json"));
}

/// 9C30BCB0A950… (tesSUCCESS): IOU AssetsMaximum with 19 significant digits.
#[test]
fn c27_vault_set_set_350_iou_19digits_devnet_7134559() {
    run(include_str!("vectors/c27_vault_set_set_350_iou_19digits_devnet_7134559.json"));
}

/// 7706613F6433… (tecLIMIT_EXCEEDED): tecLIMIT_EXCEEDED: fractional AssetsMaximum on an XRP vault below the total.
#[test]
fn c27_vault_set_set_350_xrp_fraction_devnet_7134537() {
    run(include_str!("vectors/c27_vault_set_set_350_xrp_fraction_devnet_7134537.json"));
}

/// FF20E0F71894… (tecLIMIT_EXCEEDED): tecLIMIT_EXCEEDED: AssetsMaximum = AssetsTotal-1.
#[test]
fn c27_vault_set_set_max_total_1_devnet_7134517() {
    run(include_str!("vectors/c27_vault_set_set_max_total_1_devnet_7134517.json"));
}

/// 97D7640C58A4… (tesSUCCESS): AssetsMaximum = AssetsTotal exactly.
#[test]
fn c27_vault_set_set_max_total_devnet_7134519() {
    run(include_str!("vectors/c27_vault_set_set_max_total_devnet_7134519.json"));
}

/// F53B2602BC19… (tecNO_PERMISSION): tecNO_PERMISSION: DomainID on a public vault.
#[test]
fn c27_vault_set_set_tec_domain_on_public_devnet_7134509() {
    run(include_str!("vectors/c27_vault_set_set_tec_domain_on_public_devnet_7134509.json"));
}

/// A90160E6E2C7… (tecOBJECT_NOT_FOUND): tecOBJECT_NOT_FOUND.
#[test]
fn c27_vault_set_set_tec_domain_missing_devnet_7134513() {
    run(include_str!("vectors/c27_vault_set_set_tec_domain_missing_devnet_7134513.json"));
}

/// 7D1A91C581A8… (tesSUCCESS): DomainID zero clears the share issuance's DomainID.
#[test]
fn c27_vault_set_dom_set_domain_zero_devnet_7134360() {
    run(include_str!("vectors/c27_vault_set_dom_set_domain_zero_devnet_7134360.json"));
}

/// 7F77BC543C66… (tesSUCCESS): VaultSet with a Ticket.
#[test]
fn c27_vault_set_x_set_ticket_32_devnet_7134943() {
    run(include_str!("vectors/c27_vault_set_x_set_ticket_32_devnet_7134943.json"));
}

/// 9AF4E630FC38… (tecNO_ENTRY): tecNO_ENTRY (set.tec.novault).
#[test]
fn c27_vault_set_set_tec_novault_devnet_7134505() {
    run(include_str!("vectors/c27_vault_set_set_tec_novault_devnet_7134505.json"));
}

// ---------------------------------------------------------------------------------------------------------------
// VaultDeposit
// ---------------------------------------------------------------------------------------------------------------

/// 6E1409C2E97B… (tecEXPIRED): tecEXPIRED in doApply: the expired credential is deleted although the transaction fails.
#[test]
fn c27_vault_deposit_dom_dep_expiring_before_devnet_7134278() {
    run(include_str!("vectors/c27_vault_deposit_dom_dep_expiring_before_devnet_7134278.json"));
}

/// 4D77EBC706B7… (tecEXPIRED): tecEXPIRED, depositor without a share MPToken; credential deleted.
#[test]
fn c27_vault_deposit_dom2_dep_tec_expired_no_mptoken_devnet_7134538() {
    run(include_str!("vectors/c27_vault_deposit_dom2_dep_tec_expired_no_mptoken_devnet_7134538.json"));
}

/// 11B3FA95933F… (tecNO_AUTH): tecNO_AUTH: private vault, not in the domain.
#[test]
fn c27_vault_deposit_dom_dep_tec_nocred_devnet_7134288() {
    run(include_str!("vectors/c27_vault_deposit_dom_dep_tec_nocred_devnet_7134288.json"));
}

/// 76E3F84D7589… (tecOBJECT_NOT_FOUND): tecOBJECT_NOT_FOUND: the vault's domain was deleted.
#[test]
fn c27_vault_deposit_dom_dep_domain_deleted_devnet_7134374() {
    run(include_str!("vectors/c27_vault_deposit_dom_dep_domain_deleted_devnet_7134374.json"));
}

/// 8D1EC512256D… (tecNO_AUTH): tecNO_AUTH: private vault without a domain.
#[test]
fn c27_vault_deposit_dep_tec_private_nodomain_stranger_devnet_7134173() {
    run(include_str!("vectors/c27_vault_deposit_dep_tec_private_nodomain_stranger_devnet_7134173.json"));
}

/// AE49E57E0726… (tesSUCCESS): deposit reaching AssetsMaximum exactly.
#[test]
fn c27_vault_deposit_dep_limit_xrp_exact_devnet_7134183() {
    run(include_str!("vectors/c27_vault_deposit_dep_limit_xrp_exact_devnet_7134183.json"));
}

/// 21EA84A9565E… (tecLIMIT_EXCEEDED): tecLIMIT_EXCEEDED one drop over.
#[test]
fn c27_vault_deposit_dep_limit_xrp_over_by_1_devnet_7134181() {
    run(include_str!("vectors/c27_vault_deposit_dep_limit_xrp_over_by_1_devnet_7134181.json"));
}

/// 43E1788F8437… (tesSUCCESS): IOU deposit of 0.000001 reaching AssetsMaximum 5000 exactly.
#[test]
fn c27_vault_deposit_dep_limit_iou_exact_devnet_7134264() {
    run(include_str!("vectors/c27_vault_deposit_dep_limit_iou_exact_devnet_7134264.json"));
}

/// 02D44128D29D… (tesSUCCESS): XRP vault created with AssetsMaximum 1.5 (stored 2) accepts 2 drops.
#[test]
fn c27_vault_deposit_x_dep_xrp_max1_5_two_devnet_7134979() {
    run(include_str!("vectors/c27_vault_deposit_x_dep_xrp_max1_5_two_devnet_7134979.json"));
}

/// 41D89D886A94… (tecLIMIT_EXCEEDED): tecLIMIT_EXCEEDED: MPT vault created with AssetsMaximum 10.5 (stored 10) refuses 11.
#[test]
fn c27_vault_deposit_x_dep_mpt_max10_5_eleven_devnet_7134985() {
    run(include_str!("vectors/c27_vault_deposit_x_dep_mpt_max10_5_eleven_devnet_7134985.json"));
}

/// 60319F05A33E… (tecPATH_DRY): tecPATH_DRY: Scale 18, shares would exceed 2^63-1.
#[test]
fn c27_vault_deposit_dep_s18_10_overflow_devnet_7134286() {
    run(include_str!("vectors/c27_vault_deposit_dep_s18_10_overflow_devnet_7134286.json"));
}

/// 8FB52D7C115F… (tecPRECISION_LOSS): tecPRECISION_LOSS: Scale 0, 0.9999999999999999.
#[test]
fn c27_vault_deposit_dep_s0_just_under_1_devnet_7134274() {
    run(include_str!("vectors/c27_vault_deposit_dep_s0_just_under_1_devnet_7134274.json"));
}

/// 9B2AD01D690E… (tesSUCCESS): issuer deposits 1e15 of its own IOU.
#[test]
fn c27_vault_deposit_dep_s0_issuer_1e15_devnet_7134280() {
    run(include_str!("vectors/c27_vault_deposit_dep_s0_issuer_1e15_devnet_7134280.json"));
}

/// E21C30781F94… (tesSUCCESS): deposit taking the share total to the last whole share under 2^63.
#[test]
fn c27_vault_deposit_rnd2_h1_fill_exact_devnet_7135453() {
    run(include_str!("vectors/c27_vault_deposit_rnd2_h1_fill_exact_devnet_7135453.json"));
}

/// 5B16D261D8E8… (tecPRECISION_LOSS): tecPRECISION_LOSS: one more 0.000001 against a 9.2e12 total.
#[test]
fn c27_vault_deposit_rnd2_h2_over_ceiling_devnet_7135455() {
    run(include_str!("vectors/c27_vault_deposit_rnd2_h2_over_ceiling_devnet_7135455.json"));
}

/// F53010E2195E… (tecINSUFFICIENT_RESERVE): tecINSUFFICIENT_RESERVE for the share MPToken, one drop short.
#[test]
fn c27_vault_deposit_dep_reserve_mptoken_minus1_devnet_7134331() {
    run(include_str!("vectors/c27_vault_deposit_dep_reserve_mptoken_minus1_devnet_7134331.json"));
}

/// 13C351C0BB0F… (tesSUCCESS): share MPToken created exactly at the reserve.
#[test]
fn c27_vault_deposit_dep_reserve_mptoken_exact_devnet_7134344() {
    run(include_str!("vectors/c27_vault_deposit_dep_reserve_mptoken_exact_devnet_7134344.json"));
}

/// C0DBBA180B5F… (tecINSUFFICIENT_FUNDS): tecINSUFFICIENT_FUNDS: one drop more than balance minus reserve.
#[test]
fn c27_vault_deposit_dep_funds_xrp_liq_1_devnet_7134189() {
    run(include_str!("vectors/c27_vault_deposit_dep_funds_xrp_liq_1_devnet_7134189.json"));
}

/// B6BE36048FD7… (tesSUCCESS): XRP deposit of exactly balance minus reserve.
#[test]
fn c27_vault_deposit_dep_funds_xrp_liq_devnet_7134193() {
    run(include_str!("vectors/c27_vault_deposit_dep_funds_xrp_liq_devnet_7134193.json"));
}

/// 04D7FC888535… (tecFROZEN): tecFROZEN: deep-frozen depositor.
#[test]
fn c27_vault_deposit_dep_tec_iou_holder_deepfrozen_devnet_7134448() {
    run(include_str!("vectors/c27_vault_deposit_dep_tec_iou_holder_deepfrozen_devnet_7134448.json"));
}

/// 35A226F3EBAD… (tecFROZEN): tecFROZEN: the vault's own trust line frozen.
#[test]
fn c27_vault_deposit_dep_tec_iou_pseudo_frozen_devnet_7134456() {
    run(include_str!("vectors/c27_vault_deposit_dep_tec_iou_pseudo_frozen_devnet_7134456.json"));
}

/// 09BE4622B382… (tecLOCKED): tecLOCKED: issuance locked.
#[test]
fn c27_vault_deposit_dep_tec_mpt_global_locked_devnet_7134434() {
    run(include_str!("vectors/c27_vault_deposit_dep_tec_mpt_global_locked_devnet_7134434.json"));
}

/// 121915D0446C… (tecNO_AUTH): tecNO_AUTH: RequireAuth IOU, line not authorized.
#[test]
fn c27_vault_deposit_dep_aut_unauthorized_line_devnet_7134486() {
    run(include_str!("vectors/c27_vault_deposit_dep_aut_unauthorized_line_devnet_7134486.json"));
}

/// 63F274C002DB… (tecNO_LINE): tecNO_LINE.
#[test]
fn c27_vault_deposit_dep_aut_noline_devnet_7134488() {
    run(include_str!("vectors/c27_vault_deposit_dep_aut_noline_devnet_7134488.json"));
}

/// EB1D1F5F76F3… (tesSUCCESS): RequireAuth MPT: deposit into a vault whose owner has no authorized MPToken of the asset.
#[test]
fn c27_vault_deposit_dep_mauth_vault_owner_unauthorized_devnet_7134412() {
    run(include_str!("vectors/c27_vault_deposit_dep_mauth_vault_owner_unauthorized_devnet_7134412.json"));
}

/// 9C92D574B2BA… (tecNO_AUTH): tecNO_AUTH after the issuer turned RequireAuth on under a live vault (DynamicMPT).
#[test]
fn c27_vault_deposit_dyn_dep_after_requireauth_devnet_7135323() {
    run(include_str!("vectors/c27_vault_deposit_dyn_dep_after_requireauth_devnet_7135323.json"));
}

/// 25CBDBA822A7… (tecEXPIRED): tecEXPIRED 2 s after SubscriptionDate.
#[test]
fn c27_vault_deposit_ce_dep_xrp_boundary_1_devnet_7134651() {
    run(include_str!("vectors/c27_vault_deposit_ce_dep_xrp_boundary_1_devnet_7134651.json"));
}

/// 0F10AE64C806… (tesSUCCESS): depositor with a full directory page: the share MPToken opens page 2.
#[test]
fn c27_vault_deposit_x_dep_depositor_page_32_devnet_7134973() {
    run(include_str!("vectors/c27_vault_deposit_x_dep_depositor_page_32_devnet_7134973.json"));
}

/// F6FCBA1ACF4B… (tesSUCCESS): deposit of a whole trust-line balance on a line with limit 0.
#[test]
fn c27_vault_deposit_x_dep_line_limit0_all_devnet_7135047() {
    run(include_str!("vectors/c27_vault_deposit_x_dep_line_limit0_all_devnet_7135047.json"));
}

/// 5232488A6340… (tecNO_ENTRY): tecNO_ENTRY (dep.tec.novault).
#[test]
fn c27_vault_deposit_dep_tec_novault_devnet_7134161() {
    run(include_str!("vectors/c27_vault_deposit_dep_tec_novault_devnet_7134161.json"));
}

/// 430F8AD20C2E… (tecWRONG_ASSET): tecWRONG_ASSET (dep.tec.wrongasset.iou.into.xrp).
#[test]
fn c27_vault_deposit_dep_tec_wrongasset_iou_into_xrp_devnet_7134163() {
    run(include_str!("vectors/c27_vault_deposit_dep_tec_wrongasset_iou_into_xrp_devnet_7134163.json"));
}

// ---------------------------------------------------------------------------------------------------------------
// VaultWithdraw
// ---------------------------------------------------------------------------------------------------------------

/// 613F5259F2FA… (tesSUCCESS): fixCleanup3_5_0: IOU TransferRate 1.2 charged, Amount in assets, to another holder.
#[test]
fn c27_vault_withdraw_wd_350_rate_assets_to_holder_devnet_7134760() {
    run(include_str!("vectors/c27_vault_withdraw_wd_350_rate_assets_to_holder_devnet_7134760.json"));
}

/// C35C8A269CFD… (tesSUCCESS): fixCleanup3_5_0: same by shares.
#[test]
fn c27_vault_withdraw_wd_350_rate_shares_to_holder_devnet_7134762() {
    run(include_str!("vectors/c27_vault_withdraw_wd_350_rate_shares_to_holder_devnet_7134762.json"));
}

/// 4C5B210440AC… (tesSUCCESS): fixCleanup3_5_0: one share through a 1.2 rate.
#[test]
fn c27_vault_withdraw_wd_350_rate_1share_devnet_7134772() {
    run(include_str!("vectors/c27_vault_withdraw_wd_350_rate_1share_devnet_7134772.json"));
}

/// 6F24082B4E92… (tesSUCCESS): no fee to self.
#[test]
fn c27_vault_withdraw_wd_350_rate_self_devnet_7134764() {
    run(include_str!("vectors/c27_vault_withdraw_wd_350_rate_self_devnet_7134764.json"));
}

/// 02EA6F670F70… (tesSUCCESS): no fee to the issuer.
#[test]
fn c27_vault_withdraw_wd_350_rate_to_issuer_devnet_7134766() {
    run(include_str!("vectors/c27_vault_withdraw_wd_350_rate_to_issuer_devnet_7134766.json"));
}

/// 82A260135C93… (tesSUCCESS): no fee when the withdrawer is the issuer.
#[test]
fn c27_vault_withdraw_wd_350_rate_by_issuer_to_holder_devnet_7134768() {
    run(include_str!("vectors/c27_vault_withdraw_wd_350_rate_by_issuer_to_holder_devnet_7134768.json"));
}

/// 392018122F0B… (tecINSUFFICIENT_FUNDS): tecINSUFFICIENT_FUNDS: the gross amount exceeds the shares held.
#[test]
fn c27_vault_withdraw_wd_350_rate_h2_assets_gross_exceeds_held_devnet_7134818() {
    run(include_str!("vectors/c27_vault_withdraw_wd_350_rate_h2_assets_gross_exceeds_held_devnet_7134818.json"));
}

/// 4385862A4980… (tesSUCCESS): fixCleanup3_5_0: MPT TransferFee 1% charged, by assets.
#[test]
fn c27_vault_withdraw_wd_350_fee_assets_to_holder_devnet_7134822() {
    run(include_str!("vectors/c27_vault_withdraw_wd_350_fee_assets_to_holder_devnet_7134822.json"));
}

/// B17F0E893C62… (tecPRECISION_LOSS): tecPRECISION_LOSS: 1 share, nothing left after the fee.
#[test]
fn c27_vault_withdraw_wd_350_fee_shares_1_devnet_7134838() {
    run(include_str!("vectors/c27_vault_withdraw_wd_350_fee_shares_1_devnet_7134838.json"));
}

/// A4A49882C9F9… (tesSUCCESS): MPT fee rounding at 101.
#[test]
fn c27_vault_withdraw_wd_350_fee_shares_101_devnet_7134846() {
    run(include_str!("vectors/c27_vault_withdraw_wd_350_fee_shares_101_devnet_7134846.json"));
}

/// 471CE7B110F6… (tesSUCCESS): final exit through the fee; share MPToken removed.
#[test]
fn c27_vault_withdraw_wd_350_fee_holder_exits_to_holder_devnet_7134886() {
    run(include_str!("vectors/c27_vault_withdraw_wd_350_fee_holder_exits_to_holder_devnet_7134886.json"));
}

/// DD368E65A287… (tesSUCCESS): TransferFee set under a live vault (DynamicMPT), 19 units.
#[test]
fn c27_vault_withdraw_dyn_wd_fee_19_devnet_7135315() {
    run(include_str!("vectors/c27_vault_withdraw_dyn_wd_fee_19_devnet_7135315.json"));
}

/// C95F5F58F581… (tecNO_LINE_INSUF_RESERVE): tecNO_LINE_INSUF_RESERVE: self-withdrawal must re-create the trust line, one drop short.
#[test]
fn c27_vault_withdraw_last_wd_iou_line_recreate_minus1_devnet_7135529() {
    run(include_str!("vectors/c27_vault_withdraw_last_wd_iou_line_recreate_minus1_devnet_7135529.json"));
}

/// EFE0762C5192… (tesSUCCESS): trust line re-created exactly at the reserve.
#[test]
fn c27_vault_withdraw_last_wd_iou_line_recreate_exact_devnet_7135550() {
    run(include_str!("vectors/c27_vault_withdraw_last_wd_iou_line_recreate_exact_devnet_7135550.json"));
}

/// DA976085222E… (tecINSUFFICIENT_RESERVE): tecINSUFFICIENT_RESERVE: self-withdrawal must re-create the asset MPToken.
#[test]
fn c27_vault_withdraw_last_wd_mpt_mptoken_recreate_minus1_devnet_7135609() {
    run(include_str!("vectors/c27_vault_withdraw_last_wd_mpt_mptoken_recreate_minus1_devnet_7135609.json"));
}

/// B8DA55DD987D… (tesSUCCESS): asset MPToken re-created exactly at the reserve.
#[test]
fn c27_vault_withdraw_last_wd_mpt_mptoken_recreate_exact_devnet_7135628() {
    run(include_str!("vectors/c27_vault_withdraw_last_wd_mpt_mptoken_recreate_exact_devnet_7135628.json"));
}

/// 62F00D07249C… (tesSUCCESS): closed-ended: parent close time == SubscriptionDate, still the subscription phase.
#[test]
fn c27_vault_withdraw_ce_wd_boundary_0_devnet_7134649() {
    run(include_str!("vectors/c27_vault_withdraw_ce_wd_boundary_0_devnet_7134649.json"));
}

/// D3C8264B7B82… (tecTOO_SOON): tecTOO_SOON.
#[test]
fn c27_vault_withdraw_ce_wd_tec_xrp_investment_devnet_7134667() {
    run(include_str!("vectors/c27_vault_withdraw_ce_wd_tec_xrp_investment_devnet_7134667.json"));
}

/// 32B9E518105D… (tesSUCCESS): closed-ended: first second of redemption.
#[test]
fn c27_vault_withdraw_ce_wd_xrp_boundary_red_1_devnet_7134712() {
    run(include_str!("vectors/c27_vault_withdraw_ce_wd_xrp_boundary_red_1_devnet_7134712.json"));
}

/// 7B2DE49DE5AF… (tecEXPIRED): tecEXPIRED: destination's domain credential expired (nothing deleted in preclaim).
#[test]
fn c27_vault_withdraw_dom2_wd_tec_to_expired_devnet_7134522() {
    run(include_str!("vectors/c27_vault_withdraw_dom2_wd_tec_to_expired_devnet_7134522.json"));
}

/// 353AB11BF0F3… (tecEXPIRED): tecEXPIRED: own CredentialIDs expired; the credential is deleted.
#[test]
fn c27_vault_withdraw_dom2_wd_tec_creds_expired_devnet_7134528() {
    run(include_str!("vectors/c27_vault_withdraw_dom2_wd_tec_creds_expired_devnet_7134528.json"));
}

/// 5C39D9EB4111… (tesSUCCESS): expired CredentialIDs on a self-withdrawal: success.
#[test]
fn c27_vault_withdraw_dom2_wd_tec_c9_creds_expired_self_with_creds_devnet_7134542() {
    run(include_str!("vectors/c27_vault_withdraw_dom2_wd_tec_c9_creds_expired_self_with_creds_devnet_7134542.json"));
}

/// 5966505AC9B5… (tesSUCCESS): DepositAuth destination, credential preauthorization.
#[test]
fn c27_vault_withdraw_dom_wd_depositauth_creds_devnet_7134318() {
    run(include_str!("vectors/c27_vault_withdraw_dom_wd_depositauth_creds_devnet_7134318.json"));
}

/// 344A0B96739E… (tecBAD_CREDENTIALS): tecBAD_CREDENTIALS.
#[test]
fn c27_vault_withdraw_dom_wd_tec_creds_not_own_devnet_7134322() {
    run(include_str!("vectors/c27_vault_withdraw_dom_wd_tec_creds_not_own_devnet_7134322.json"));
}

/// 96D85548742D… (tecNO_AUTH): tecNO_AUTH: private vault, destination outside the domain.
#[test]
fn c27_vault_withdraw_dom_wd_tec_to_nonmember_devnet_7134300() {
    run(include_str!("vectors/c27_vault_withdraw_dom_wd_tec_to_nonmember_devnet_7134300.json"));
}

/// BDC3AF97E624… (tecNO_AUTH): tecNO_AUTH: the owner itself is not a domain member.
#[test]
fn c27_vault_withdraw_dom_wd_owner_to_member_devnet_7134306() {
    run(include_str!("vectors/c27_vault_withdraw_dom_wd_owner_to_member_devnet_7134306.json"));
}

/// D54A2A5A8426… (tecNO_PERMISSION): tecNO_PERMISSION: DepositAuth without preauth.
#[test]
fn c27_vault_withdraw_wd_tec_dest_depositauth_devnet_7134620() {
    run(include_str!("vectors/c27_vault_withdraw_wd_tec_dest_depositauth_devnet_7134620.json"));
}

/// 7A36165CCFB0… (tesSUCCESS): DepositAuth with DepositPreauth.
#[test]
fn c27_vault_withdraw_wd_dest_depositauth_preauth_devnet_7134624() {
    run(include_str!("vectors/c27_vault_withdraw_wd_dest_depositauth_preauth_devnet_7134624.json"));
}

/// 90C25262512F… (tecDST_TAG_NEEDED): tecDST_TAG_NEEDED.
#[test]
fn c27_vault_withdraw_wd_tec_dest_tag_needed_devnet_7134616() {
    run(include_str!("vectors/c27_vault_withdraw_wd_tec_dest_tag_needed_devnet_7134616.json"));
}

/// 2C3BF8C25888… (tecNO_DST): tecNO_DST.
#[test]
fn c27_vault_withdraw_wd_tec_dest_missing_devnet_7134579() {
    run(include_str!("vectors/c27_vault_withdraw_wd_tec_dest_missing_devnet_7134579.json"));
}

/// 6375894854F1… (tecPSEUDO_ACCOUNT): tecPSEUDO_ACCOUNT.
#[test]
fn c27_vault_withdraw_wd_tec_dest_pseudo_devnet_7134581() {
    run(include_str!("vectors/c27_vault_withdraw_wd_tec_dest_pseudo_devnet_7134581.json"));
}

/// 2154B95DC48D… (tecNO_LINE): tecNO_LINE: destination's limit would be exceeded.
#[test]
fn c27_vault_withdraw_wd_iou_dest_limit_plus_devnet_7134662() {
    run(include_str!("vectors/c27_vault_withdraw_wd_iou_dest_limit_plus_devnet_7134662.json"));
}

/// E738CF005170… (tesSUCCESS): frozen holder may withdraw to itself.
#[test]
fn c27_vault_withdraw_wd_iou_self_frozen_devnet_7134670() {
    run(include_str!("vectors/c27_vault_withdraw_wd_iou_self_frozen_devnet_7134670.json"));
}

/// 2A40D9B3D063… (tecFROZEN): tecFROZEN: deep-frozen holder.
#[test]
fn c27_vault_withdraw_wd_tec_iou_self_deepfrozen_devnet_7134680() {
    run(include_str!("vectors/c27_vault_withdraw_wd_tec_iou_self_deepfrozen_devnet_7134680.json"));
}

/// 96B502831975… (tesSUCCESS): global freeze: withdrawal to the issuer still allowed.
#[test]
fn c27_vault_withdraw_wd_global_freeze_to_issuer_devnet_7134700() {
    run(include_str!("vectors/c27_vault_withdraw_wd_global_freeze_to_issuer_devnet_7134700.json"));
}

/// 0093229FD6AE… (tecLOCKED): tecLOCKED: locked holder, self-withdrawal.
#[test]
fn c27_vault_withdraw_wd_mpt_self_locked_devnet_7134862() {
    run(include_str!("vectors/c27_vault_withdraw_wd_mpt_self_locked_devnet_7134862.json"));
}

/// AE28717F21A6… (tecNO_AUTH): tecNO_AUTH: holder un-authorized by the issuer.
#[test]
fn c27_vault_withdraw_wd_mauth_self_after_unauthorize_devnet_7134915() {
    run(include_str!("vectors/c27_vault_withdraw_wd_mauth_self_after_unauthorize_devnet_7134915.json"));
}

/// D44AE2CB6A0B… (tecPRECISION_LOSS): tecPRECISION_LOSS: one share at Scale 18.
#[test]
fn c27_vault_withdraw_wd_s18_1share_devnet_7134925() {
    run(include_str!("vectors/c27_vault_withdraw_wd_s18_1share_devnet_7134925.json"));
}

/// EE3C46D03FB8… (tecPRECISION_LOSS): tecPRECISION_LOSS: one share against a 9.2e12 total.
#[test]
fn c27_vault_withdraw_rnd2_wd_h1_1share_devnet_7135459() {
    run(include_str!("vectors/c27_vault_withdraw_rnd2_wd_h1_1share_devnet_7135459.json"));
}

/// 812F37170A57… (tesSUCCESS): last withdrawal of a vault at the share ceiling.
#[test]
fn c27_vault_withdraw_rnd2_exit_gw_devnet_7135475() {
    run(include_str!("vectors/c27_vault_withdraw_rnd2_exit_gw_devnet_7135475.json"));
}

/// B9AFB2FB5AC4… (tesSUCCESS): withdrawal by a holder that got its shares by Payment.
#[test]
fn c27_vault_withdraw_x_wd_received_shares_assets_devnet_7135000() {
    run(include_str!("vectors/c27_vault_withdraw_x_wd_received_shares_assets_devnet_7135000.json"));
}

/// 6849502DB6C5… (tesSUCCESS): exit removes the share MPToken and its directory page.
#[test]
fn c27_vault_withdraw_x_wd_depositor_page_exit_32_devnet_7134977() {
    run(include_str!("vectors/c27_vault_withdraw_x_wd_depositor_page_exit_32_devnet_7134977.json"));
}

/// 84E238486204… (tecOBJECT_NOT_FOUND): tecOBJECT_NOT_FOUND (dom.wd.to.other.domain.deleted).
#[test]
fn c27_vault_withdraw_dom_wd_to_other_domain_deleted_devnet_7134378() {
    run(include_str!("vectors/c27_vault_withdraw_dom_wd_to_other_domain_deleted_devnet_7134378.json"));
}

/// 56011DC9D03A… (tecNO_ENTRY): tecNO_ENTRY (wd.tec.novault).
#[test]
fn c27_vault_withdraw_wd_tec_novault_devnet_7134585() {
    run(include_str!("vectors/c27_vault_withdraw_wd_tec_novault_devnet_7134585.json"));
}

/// 9D6CE9D103C0… (tecWRONG_ASSET): tecWRONG_ASSET (wd.tec.wrongasset.iou).
#[test]
fn c27_vault_withdraw_wd_tec_wrongasset_iou_devnet_7134587() {
    run(include_str!("vectors/c27_vault_withdraw_wd_tec_wrongasset_iou_devnet_7134587.json"));
}

/// 35816D24C00E… (tecPATH_DRY): tecPATH_DRY (rnd.wd.issuer.most).
#[test]
fn c27_vault_withdraw_rnd_wd_issuer_most_devnet_7135227() {
    run(include_str!("vectors/c27_vault_withdraw_rnd_wd_issuer_most_devnet_7135227.json"));
}

// ---------------------------------------------------------------------------------------------------------------
// VaultClawback
// ---------------------------------------------------------------------------------------------------------------

/// FD549B3C2C9F… (tesSUCCESS): issuer, explicit Amount (10 of ~4900).
#[test]
fn c27_vault_clawback_claw_iou_partial_devnet_7134941() {
    run(include_str!("vectors/c27_vault_clawback_claw_iou_partial_devnet_7134941.json"));
}

/// DD147F9B9E26… (tesSUCCESS): explicit Amount with digits below the vault scale.
#[test]
fn c27_vault_clawback_claw_iou_partial_subscale_devnet_7134943() {
    run(include_str!("vectors/c27_vault_clawback_claw_iou_partial_subscale_devnet_7134943.json"));
}

/// 3EB1F65B1143… (tesSUCCESS): Amount 0 = everything; holder's share MPToken removed.
#[test]
fn c27_vault_clawback_claw_iou_amount_zero_all_devnet_7134989() {
    run(include_str!("vectors/c27_vault_clawback_claw_iou_amount_zero_all_devnet_7134989.json"));
}

/// 376276D2C69E… (tesSUCCESS): no Amount, the last holder: the vault ends empty.
#[test]
fn c27_vault_clawback_claw_iou_all_h1_devnet_7134993() {
    run(include_str!("vectors/c27_vault_clawback_claw_iou_all_h1_devnet_7134993.json"));
}

/// C6D293D977BA… (tesSUCCESS): holder is the vault owner: its share MPToken stays.
#[test]
fn c27_vault_clawback_claw_s0_owner_as_holder_devnet_7135004() {
    run(include_str!("vectors/c27_vault_clawback_claw_s0_owner_as_holder_devnet_7135004.json"));
}

/// 70CDFC671546… (tecINSUFFICIENT_FUNDS): tecINSUFFICIENT_FUNDS: Amount above the holder's shares.
#[test]
fn c27_vault_clawback_claw_iou_more_than_held_devnet_7134987() {
    run(include_str!("vectors/c27_vault_clawback_claw_iou_more_than_held_devnet_7134987.json"));
}

/// D3BF5BA5DED9… (tecNO_AUTH): tecNO_AUTH: explicit Amount, holder without a share MPToken.
#[test]
fn c27_vault_clawback_claw_tec_holder_no_shares_devnet_7134965() {
    run(include_str!("vectors/c27_vault_clawback_claw_tec_holder_no_shares_devnet_7134965.json"));
}

/// B178DBDBBA50… (tecPRECISION_LOSS): tecPRECISION_LOSS: no Amount, holder without shares.
#[test]
fn c27_vault_clawback_claw_tec_holder_no_shares_noamount_devnet_7134967() {
    run(include_str!("vectors/c27_vault_clawback_claw_tec_holder_no_shares_noamount_devnet_7134967.json"));
}

/// E234C561ABB3… (tecPRECISION_LOSS): tecPRECISION_LOSS: Amount below one share.
#[test]
fn c27_vault_clawback_claw_tec_iou_below_scale_devnet_7134945() {
    run(include_str!("vectors/c27_vault_clawback_claw_tec_iou_below_scale_devnet_7134945.json"));
}

/// 684A392E478B… (tecNO_PERMISSION): tecNO_PERMISSION: owner's share clawback while the vault has assets.
#[test]
fn c27_vault_clawback_claw_tec_owner_no_amount_devnet_7134951() {
    run(include_str!("vectors/c27_vault_clawback_claw_tec_owner_no_amount_devnet_7134951.json"));
}

/// 7EF6C884330D… (tecNO_PERMISSION): tecNO_PERMISSION: issuer naming shares.
#[test]
fn c27_vault_clawback_claw_tec_issuer_share_amount_devnet_7134947() {
    run(include_str!("vectors/c27_vault_clawback_claw_tec_issuer_share_amount_devnet_7134947.json"));
}

/// 60B738C94FD0… (tecNO_PERMISSION): tecNO_PERMISSION.
#[test]
fn c27_vault_clawback_claw_tec_holder_is_issuer_devnet_7134961() {
    run(include_str!("vectors/c27_vault_clawback_claw_tec_holder_is_issuer_devnet_7134961.json"));
}

/// EBCA203C9F4D… (tecPSEUDO_ACCOUNT): tecPSEUDO_ACCOUNT.
#[test]
fn c27_vault_clawback_claw_tec_holder_pseudo_devnet_7134963() {
    run(include_str!("vectors/c27_vault_clawback_claw_tec_holder_pseudo_devnet_7134963.json"));
}

/// 738DE5D47509… (tecWRONG_ASSET): tecWRONG_ASSET.
#[test]
fn c27_vault_clawback_claw_tec_wrongasset_otheriou_devnet_7134969() {
    run(include_str!("vectors/c27_vault_clawback_claw_tec_wrongasset_otheriou_devnet_7134969.json"));
}

/// AF0E9EAE863C… (tecWRONG_ASSET): tecWRONG_ASSET: issuer is the owner, no Amount.
#[test]
fn c27_vault_clawback_claw_tec_issuer_is_owner_noamount_devnet_7135014() {
    run(include_str!("vectors/c27_vault_clawback_claw_tec_issuer_is_owner_noamount_devnet_7135014.json"));
}

/// 939A8956232B… (tesSUCCESS): issuer is the owner, explicit asset Amount.
#[test]
fn c27_vault_clawback_claw_issuer_is_owner_asset_partial_devnet_7135018() {
    run(include_str!("vectors/c27_vault_clawback_claw_issuer_is_owner_asset_partial_devnet_7135018.json"));
}

/// 73FF829CFA60… (tecNO_PERMISSION): tecNO_PERMISSION: XRP vault.
#[test]
fn c27_vault_clawback_claw_tec_xrp_owner_devnet_7134998() {
    run(include_str!("vectors/c27_vault_clawback_claw_tec_xrp_owner_devnet_7134998.json"));
}

/// DE7DB629BE15… (tecNO_PERMISSION): tecNO_PERMISSION: issuer without AllowTrustLineClawback.
#[test]
fn c27_vault_clawback_claw_tec_issuer_no_clawback_flag_devnet_7135493() {
    run(include_str!("vectors/c27_vault_clawback_claw_tec_issuer_no_clawback_flag_devnet_7135493.json"));
}

/// AF7EFFE16357… (tesSUCCESS): clawback while the vault's trust line is frozen.
#[test]
fn c27_vault_clawback_claw_iou_frozen_pseudo_devnet_7134983() {
    run(include_str!("vectors/c27_vault_clawback_claw_iou_frozen_pseudo_devnet_7134983.json"));
}

/// F597C9E6FFA1… (tesSUCCESS): IOU with a TransferRate: no fee on clawback.
#[test]
fn c27_vault_clawback_claw_ust_all_devnet_7135026() {
    run(include_str!("vectors/c27_vault_clawback_claw_ust_all_devnet_7135026.json"));
}

/// F48C69D35403… (tesSUCCESS): MPT, explicit Amount.
#[test]
fn c27_vault_clawback_claw_mpt_partial_devnet_7135030() {
    run(include_str!("vectors/c27_vault_clawback_claw_mpt_partial_devnet_7135030.json"));
}

/// B4A83ED100C5… (tesSUCCESS): MPT, Amount 1.
#[test]
fn c27_vault_clawback_claw_mpt_1_devnet_7135032() {
    run(include_str!("vectors/c27_vault_clawback_claw_mpt_1_devnet_7135032.json"));
}

/// C5912453DE7B… (tesSUCCESS): MPT clawback while the issuance is locked.
#[test]
fn c27_vault_clawback_claw_mpt_global_locked_devnet_7135050() {
    run(include_str!("vectors/c27_vault_clawback_claw_mpt_global_locked_devnet_7135050.json"));
}

/// 86E2DDA484E3… (tecNO_PERMISSION): tecNO_PERMISSION: MPT without CanClawback.
#[test]
fn c27_vault_clawback_claw_tec_mpt_no_canclawback_devnet_7135064() {
    run(include_str!("vectors/c27_vault_clawback_claw_tec_mpt_no_canclawback_devnet_7135064.json"));
}

/// 84F15D588329… (tesSUCCESS): CanClawback turned on under a live vault (DynamicMPT).
#[test]
fn c27_vault_clawback_dyn_claw_after_set_devnet_7135307() {
    run(include_str!("vectors/c27_vault_clawback_dyn_claw_after_set_devnet_7135307.json"));
}

/// 98D61F393877… (tesSUCCESS): clawback during a closed-ended vault's investment phase.
#[test]
fn c27_vault_clawback_ce_claw_iou_investment_devnet_7134679() {
    run(include_str!("vectors/c27_vault_clawback_ce_claw_iou_investment_devnet_7134679.json"));
}

/// F269E7549407… (tesSUCCESS): partial clawback against a 9.2e12 total.
#[test]
fn c27_vault_clawback_rnd2_claw_h4_partial_devnet_7135463() {
    run(include_str!("vectors/c27_vault_clawback_rnd2_claw_h4_partial_devnet_7135463.json"));
}

/// 26D8C660BB96… (tecNO_ENTRY): tecNO_ENTRY (claw.tec.novault).
#[test]
fn c27_vault_clawback_claw_tec_novault_devnet_7134973() {
    run(include_str!("vectors/c27_vault_clawback_claw_tec_novault_devnet_7134973.json"));
}

// ---------------------------------------------------------------------------------------------------------------
// VaultDelete
// ---------------------------------------------------------------------------------------------------------------

/// 7623495D2E22… (tesSUCCESS): empty XRP vault, never used.
#[test]
fn c27_vault_delete_del_xrp_fresh_devnet_7135096() {
    run(include_str!("vectors/c27_vault_delete_del_xrp_fresh_devnet_7135096.json"));
}

/// B8A9121078EF… (tesSUCCESS): empty IOU vault (Scale 3, Data): the pseudo-account's trust line goes too.
#[test]
fn c27_vault_delete_del_iou_fresh_devnet_7135102() {
    run(include_str!("vectors/c27_vault_delete_del_iou_fresh_devnet_7135102.json"));
}

/// AA30B13406F0… (tesSUCCESS): empty MPT vault (AssetsMaximum 77): the pseudo-account's MPToken goes too.
#[test]
fn c27_vault_delete_del_mpt_fresh_devnet_7135104() {
    run(include_str!("vectors/c27_vault_delete_del_mpt_fresh_devnet_7135104.json"));
}

/// D6579BCB4812… (tesSUCCESS): empty private vault with non-transferable shares.
#[test]
fn c27_vault_delete_del_private_fresh_devnet_7135106() {
    run(include_str!("vectors/c27_vault_delete_del_private_fresh_devnet_7135106.json"));
}

/// 285DA65CFB33… (tesSUCCESS): MemoData at the 256-byte maximum.
#[test]
fn c27_vault_delete_del_memodata_256_devnet_7135108() {
    run(include_str!("vectors/c27_vault_delete_del_memodata_256_devnet_7135108.json"));
}

/// F7A1737B0CD7… (tesSUCCESS): MemoData plus a Memos array, IOU with a TransferRate.
#[test]
fn c27_vault_delete_del_ust_fresh_memos_devnet_7135110() {
    run(include_str!("vectors/c27_vault_delete_del_ust_fresh_memos_devnet_7135110.json"));
}

/// DC986946B115… (tesSUCCESS): after owner and last holder withdrew everything; owner's empty share MPToken removed.
#[test]
fn c27_vault_delete_del_l_xrp_emptied_devnet_7135151() {
    run(include_str!("vectors/c27_vault_delete_del_l_xrp_emptied_devnet_7135151.json"));
}

/// FDE8B15D4357… (tesSUCCESS): after the issuer clawed every holder back.
#[test]
fn c27_vault_delete_del_after_clawback_usd_def_devnet_7135129() {
    run(include_str!("vectors/c27_vault_delete_del_after_clawback_usd_def_devnet_7135129.json"));
}

/// 267329B6C95F… (tesSUCCESS): IOU vault whose pseudo-account trust line is frozen by the issuer.
#[test]
fn c27_vault_delete_del_l_iou_pseudo_frozen_devnet_7135165() {
    run(include_str!("vectors/c27_vault_delete_del_l_iou_pseudo_frozen_devnet_7135165.json"));
}

/// F4EBE06F5476… (tesSUCCESS): MPT vault whose pseudo-account MPToken is locked.
#[test]
fn c27_vault_delete_del_l_mpt_pseudo_locked_devnet_7135175() {
    run(include_str!("vectors/c27_vault_delete_del_l_mpt_pseudo_locked_devnet_7135175.json"));
}

/// 3351847B985F… (tesSUCCESS): owner sitting at the base reserve: delete releases 2 owner counts.
#[test]
fn c27_vault_delete_del_owner_below_reserve_devnet_7135184() {
    run(include_str!("vectors/c27_vault_delete_del_owner_below_reserve_devnet_7135184.json"));
}

/// 09725A120002… (tesSUCCESS): owner exactly at reserve(OwnerCount) after create.
#[test]
fn c27_vault_delete_del_owner_at_reserve_xrp_devnet_7134188() {
    run(include_str!("vectors/c27_vault_delete_del_owner_at_reserve_xrp_devnet_7134188.json"));
}

/// C170A3D7E541… (tesSUCCESS): owner's own trust line of the asset deep-frozen.
#[test]
fn c27_vault_delete_del_owner_deepfrozen_devnet_7135190() {
    run(include_str!("vectors/c27_vault_delete_del_owner_deepfrozen_devnet_7135190.json"));
}

/// 3846257D5BCB… (tesSUCCESS): owner directory with two pages: the vault's entry leaves page 2.
#[test]
fn c27_vault_delete_x_del_page_32_devnet_7134951() {
    run(include_str!("vectors/c27_vault_delete_x_del_page_32_devnet_7134951.json"));
}

/// 18A9DBB7BA53… (tesSUCCESS): VaultDelete spending a Ticket.
#[test]
fn c27_vault_delete_x_del_with_ticket_30_devnet_7134907() {
    run(include_str!("vectors/c27_vault_delete_x_del_with_ticket_30_devnet_7134907.json"));
}

/// 51536843AA97… (tesSUCCESS): owner gave up its empty share MPToken before deleting.
#[test]
fn c27_vault_delete_x_del_owner_without_share_mptoken_devnet_7135036() {
    run(include_str!("vectors/c27_vault_delete_x_del_owner_without_share_mptoken_devnet_7135036.json"));
}

/// 203CB9A32C3B… (tesSUCCESS): private vault with a DomainID, emptied.
#[test]
fn c27_vault_delete_dom_del_private_domain_emptied_devnet_7134421() {
    run(include_str!("vectors/c27_vault_delete_dom_del_private_domain_emptied_devnet_7134421.json"));
}

/// 98648CE61B90… (tesSUCCESS): closed-ended vault, empty, deleted during the investment phase.
#[test]
fn c27_vault_delete_ce_del_empty_investment_devnet_7134689() {
    run(include_str!("vectors/c27_vault_delete_ce_del_empty_investment_devnet_7134689.json"));
}

/// 7F3E1EAA0480… (tesSUCCESS): closed-ended MPT vault after redemption, MemoData.
#[test]
fn c27_vault_delete_ce_del_mpt_redemption_devnet_7134742() {
    run(include_str!("vectors/c27_vault_delete_ce_del_mpt_redemption_devnet_7134742.json"));
}

/// FBE54FAAE01A… (tecHAS_OBLIGATIONS): tecHAS_OBLIGATIONS: assets left.
#[test]
fn c27_vault_delete_del_tec_xrp_assets_devnet_7135112() {
    run(include_str!("vectors/c27_vault_delete_del_tec_xrp_assets_devnet_7135112.json"));
}

/// 974ECAC1725F… (tecHAS_OBLIGATIONS): tecHAS_OBLIGATIONS: owner out, another holder still in.
#[test]
fn c27_vault_delete_del_tec_l_xrp_other_holder_devnet_7135147() {
    run(include_str!("vectors/c27_vault_delete_del_tec_l_xrp_other_holder_devnet_7135147.json"));
}

/// 32D97EB789FC… (tecNO_PERMISSION): tecNO_PERMISSION: not the owner.
#[test]
fn c27_vault_delete_del_tec_notowner_devnet_7135090() {
    run(include_str!("vectors/c27_vault_delete_del_tec_notowner_devnet_7135090.json"));
}

/// 04A050B3A7FE… (tecNO_ENTRY): tecNO_ENTRY.
#[test]
fn c27_vault_delete_del_tec_novault_devnet_7135092() {
    run(include_str!("vectors/c27_vault_delete_del_tec_novault_devnet_7135092.json"));
}

// ---------------------------------------------------------------------------------------------------------------
// Fee sponsorship (finding S1): parked until featureSponsor is ported
// ---------------------------------------------------------------------------------------------------------------

/// 8F47F269C3FB… (tesSUCCESS): fee sponsored, co-signed: the 20-drop fee is the sponsor's.
#[test]
#[ignore = "featureSponsor not ported: fee payer is the sponsor (STTx::getFeePayerID)"]
fn c27_vault_deposit_sp_dep_fee_sponsored_cosigned_devnet_7135193() {
    run(include_str!("vectors/c27_vault_deposit_sp_dep_fee_sponsored_cosigned_devnet_7135193.json"));
}

/// BB98C4816795… (tesSUCCESS): fee sponsored, co-signed.
#[test]
#[ignore = "featureSponsor not ported: fee payer is the sponsor (STTx::getFeePayerID)"]
fn c27_vault_withdraw_sp_wd_fee_sponsored_cosigned_devnet_7135195() {
    run(include_str!("vectors/c27_vault_withdraw_sp_wd_fee_sponsored_cosigned_devnet_7135195.json"));
}

/// A6E74358DE68… (tesSUCCESS): fee sponsored, co-signed.
#[test]
#[ignore = "featureSponsor not ported: fee payer is the sponsor (STTx::getFeePayerID)"]
fn c27_vault_delete_sp_del_fee_sponsored_cosigned_devnet_7135204() {
    run(include_str!("vectors/c27_vault_delete_sp_del_fee_sponsored_cosigned_devnet_7135204.json"));
}
