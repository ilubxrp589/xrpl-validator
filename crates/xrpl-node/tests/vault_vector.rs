//! SingleAssetVault byte-exact vectors: real devnet transactions of the six vault types (VaultCreate, VaultSet,
//! VaultDelete, VaultDeposit, VaultWithdraw, VaultClawback), each with the pre-state of everything it reads, its
//! recorded result and the post-state of everything its metadata names.
//!
//! - Devnet runs rippled 3.5.0-rc5 with SingleAssetVault, LendingProtocol, LendingProtocolV1_1 and (in the later
//!   ledgers) fixCleanup3_5_0 enabled. Every bundle carries the Amendments object of its parent ledger, so the
//!   engine applies each transaction under the rules devnet applied it under.
//! - A vector passes when the engine returns the recorded TransactionResult, every object the metadata names
//!   comes out byte-identical (threading included), and the engine wrote no object the metadata does not name.
//! - Bundles: `scripts/vault_bundle.py <hash> <ledger> <out>` with `XRPL_RPC` set to a devnet server (it drives
//!   `scripts/fetch_tx_bundle.py`, the format of the campaign vectors).
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

/// EED356998F8C…: an open-ended public XRP vault (Data only).
#[test]
fn vault_create_xrp_public_devnet_6792041() {
    run(include_str!("vectors/vault_create_xrp_public_devnet_6792041.json"));
}

/// 3628630C8C…: a private XRP vault with a DomainID.
#[test]
fn vault_create_xrp_private_domain_devnet_6791924() {
    run(include_str!("vectors/vault_create_xrp_private_domain_devnet_6791924.json"));
}

/// 6710EA6FAC…: a closed-ended XRP vault (VaultKind 1, both dates, AssetsMaximum 0).
#[test]
fn vault_create_xrp_closed_ended_devnet_6418478() {
    run(include_str!("vectors/vault_create_xrp_closed_ended_devnet_6418478.json"));
}

/// 38823CF190…: a private USD vault with a DomainID and Scale 6; the pseudo-account's trust line and the issuer's
/// root threaded.
#[test]
fn vault_create_iou_private_domain_devnet_6966500() {
    run(include_str!("vectors/vault_create_iou_private_domain_devnet_6966500.json"));
}

/// 7E781F3270…: a closed-ended USD vault.
#[test]
fn vault_create_iou_closed_ended_devnet_6471757() {
    run(include_str!("vectors/vault_create_iou_closed_ended_devnet_6471757.json"));
}

/// 392C580CE2…: a private vault of an MPT with a DomainID.
#[test]
fn vault_create_mpt_private_domain_devnet_6966508() {
    run(include_str!("vectors/vault_create_mpt_private_domain_devnet_6966508.json"));
}

/// 3D81ECDC6C…: a public vault of an MPT.
#[test]
fn vault_create_mpt_public_devnet_6630795() {
    run(include_str!("vectors/vault_create_mpt_public_devnet_6630795.json"));
}

// ---------------------------------------------------------------------------------------------------------------
// VaultDeposit
// ---------------------------------------------------------------------------------------------------------------

/// CA2356A0A9…: VaultDeposit of 100 EUR, tesSUCCESS.
#[test]
fn vault_deposit_eur_100_devnet_6630191() {
    run(include_str!("vectors/vault_deposit_eur_100_devnet_6630191.json"));
}

/// AE8397151E…: VaultDeposit of 20 EUR, tesSUCCESS.
#[test]
fn vault_deposit_eur_20_second_devnet_6630193() {
    run(include_str!("vectors/vault_deposit_eur_20_second_devnet_6630193.json"));
}

/// 5F6FF6FFA8…: VaultDeposit of 1 EUR, tesSUCCESS.
#[test]
fn vault_deposit_eur_after_unfreeze_devnet_6630244() {
    run(include_str!("vectors/vault_deposit_eur_after_unfreeze_devnet_6630244.json"));
}

/// 4A34004588…: VaultDeposit of 1 EUR, tesSUCCESS.
#[test]
fn vault_deposit_eur_last_devnet_6630271() {
    run(include_str!("vectors/vault_deposit_eur_last_devnet_6630271.json"));
}

/// E638E87D2C…: VaultDeposit of 1 EUR, tecFROZEN.
#[test]
fn vault_deposit_eur_tec_frozen_again_devnet_6630254() {
    run(include_str!("vectors/vault_deposit_eur_tec_frozen_again_devnet_6630254.json"));
}

/// 50A0AE50F9…: VaultDeposit of 1 EUR, tecFROZEN.
#[test]
fn vault_deposit_eur_tec_frozen_devnet_6630227() {
    run(include_str!("vectors/vault_deposit_eur_tec_frozen_devnet_6630227.json"));
}

/// 90BBDCF4AF…: VaultDeposit of 10000 USD, tesSUCCESS.
#[test]
fn vault_deposit_iou_first_devnet_6471759() {
    run(include_str!("vectors/vault_deposit_iou_first_devnet_6471759.json"));
}

/// 3390694B19…: VaultDeposit of 10000 MPT, tesSUCCESS.
#[test]
fn vault_deposit_mpt_first_devnet_6418435() {
    run(include_str!("vectors/vault_deposit_mpt_first_devnet_6418435.json"));
}

/// B411C976DF…: VaultDeposit of 10000 MPT, tesSUCCESS.
#[test]
fn vault_deposit_mpt_private_domain_devnet_6966509() {
    run(include_str!("vectors/vault_deposit_mpt_private_domain_devnet_6966509.json"));
}

/// D8F5FBBCB5…: VaultDeposit of 100.5 USD, tesSUCCESS.
#[test]
fn vault_deposit_usd_100_5_devnet_6791424() {
    run(include_str!("vectors/vault_deposit_usd_100_5_devnet_6791424.json"));
}

/// 63029D6FC5…: VaultDeposit of 1000000000000000e2 USD, tesSUCCESS.
#[test]
fn vault_deposit_usd_1e17_scale_0_devnet_6858220() {
    run(include_str!("vectors/vault_deposit_usd_1e17_scale_0_devnet_6858220.json"));
}

/// D3C20C23B7…: VaultDeposit of 10000 USD, tesSUCCESS.
#[test]
fn vault_deposit_usd_private_domain_devnet_6966503() {
    run(include_str!("vectors/vault_deposit_usd_private_domain_devnet_6966503.json"));
}

/// A789702CE8…: VaultDeposit of 10000 drops, tesSUCCESS.
#[test]
fn vault_deposit_xrp_first_devnet_7112017() {
    run(include_str!("vectors/vault_deposit_xrp_first_devnet_7112017.json"));
}

/// EE6DB857D9…: VaultDeposit of 10000 drops, tesSUCCESS.
#[test]
fn vault_deposit_xrp_private_closed_ended_devnet_6485842() {
    run(include_str!("vectors/vault_deposit_xrp_private_closed_ended_devnet_6485842.json"));
}

/// B47D5F17EC…: VaultDeposit of 10000 drops, tesSUCCESS.
#[test]
fn vault_deposit_xrp_private_devnet_6966497() {
    run(include_str!("vectors/vault_deposit_xrp_private_devnet_6966497.json"));
}

/// 49C709E03F…: VaultDeposit of 10000 drops, tesSUCCESS.
#[test]
fn vault_deposit_xrp_second_devnet_7112018() {
    run(include_str!("vectors/vault_deposit_xrp_second_devnet_7112018.json"));
}
