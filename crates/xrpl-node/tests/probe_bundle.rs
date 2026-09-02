//! Throwaway bundle prober: PROBE_BUNDLE=<path> runs any generator bundle
//! through the vector pipeline. Not committed — the drill's scratch tool.
use serde_json::Value;
use xrpl_core::types::Hash256;
use xrpl_ledger::ledger::header::LedgerHeader;
use xrpl_ledger::ledger::sandbox::SandboxEntry;
use xrpl_ledger::ledger::state::LedgerState;
use xrpl_node::native_apply::{build_txfields, canon_for_encode, hexify_addresses, native_apply_one};

fn key32(hex_key: &str) -> Hash256 {
    Hash256(<[u8; 32]>::try_from(hex::decode(hex_key).unwrap().as_slice()).unwrap())
}

#[test]
fn probe_bundle() {
    let Ok(path) = std::env::var("PROBE_BUNDLE") else { return };
    let bundle: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
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
        let bytes = hex::decode(v.as_str().unwrap().trim()).unwrap();
        let mut jv = xrpl_core::codec::decode::decode_transaction_binary(&bytes).unwrap();
        hexify_addresses(&mut jv);
        state
            .state_map
            .insert(key32(k), serde_json::to_vec(&jv).unwrap())
            .unwrap();
    }

    let tx = &bundle["tx"];
    let tx_hash = tx["hash"].as_str().unwrap();
    let txf = build_txfields(tx).expect("txfields");
    let (ter, mut mods) = native_apply_one(&state, &txf);
    // The bundle carries the ledger's verdict (fetch_tx_bundle.py "result");
    // older bundles without it were all tesSUCCESS specimens.
    let want_ter = bundle["result"].as_str().unwrap_or("tesSUCCESS");
    assert_eq!(ter, want_ter, "mainnet's result for this tx");

    xrpl_ledger::ledger::threading::stamp_threading(
        &mut mods,
        &|k| state.state_map.lookup(k).map(|b| b.to_vec()),
        tx_hash,
        seq,
    );

    // PROBE_LIST=1: every key the apply wrote, with its kind and entry type —
    // for receipts whose target set cannot name the object we create or drop.
    if std::env::var("PROBE_LIST").is_ok() {
        for (k, ent) in mods.iter() {
            let (kind, ty) = match ent {
                SandboxEntry::Created(b) | SandboxEntry::Modified(b) => {
                    let ty = serde_json::from_slice::<Value>(b)
                        .ok()
                        .and_then(|j| j["LedgerEntryType"].as_str().map(str::to_string))
                        .unwrap_or_default();
                    (if matches!(ent, SandboxEntry::Created(_)) { "CREATED" } else { "MODIFIED" }, ty)
                }
                SandboxEntry::Deleted => ("DELETED", String::new()),
            };
            let detail = match ent {
                SandboxEntry::Created(b) | SandboxEntry::Modified(b) => serde_json::from_slice::<Value>(b)
                    .ok()
                    .map(|j| {
                        let mut j = j;
                        if let Some(ix) = j.get_mut("Indexes").and_then(|v| v.as_array_mut()) {
                            let n = ix.len();
                            ix.truncate(2);
                            ix.push(Value::String(format!("…{n} entries")));
                        }
                        j.to_string()
                    })
                    .unwrap_or_default(),
                SandboxEntry::Deleted => String::new(),
            };
            println!("LIST {kind} {ty} {} {}", hex::encode_upper(k.0), &detail[..detail.len().min(420)]);
        }
    }
    let mut bad = 0;
    for (k, want_hex) in bundle["expect"].as_object().unwrap() {
        let Some(ent) = mods.get(&key32(k)) else {
            println!("PROBE {k}: NOT WRITTEN");
            bad += 1;
            continue;
        };
        let bytes = match ent {
            SandboxEntry::Created(b) | SandboxEntry::Modified(b) => b.clone(),
            SandboxEntry::Deleted => {
                println!("PROBE {k}: DELETED");
                bad += 1;
                continue;
            }
        };
        let mut jv: Value = serde_json::from_slice(&bytes).unwrap();
        canon_for_encode(&mut jv);
        let enc = xrpl_core::codec::encode::encode_transaction_json(&jv, false).unwrap();
        let want = hex::decode(want_hex.as_str().unwrap().trim()).unwrap();
        if enc != want {
            println!("PROBE {k}: BYTE DIFF ours={} want={}", hex::encode_upper(&enc), hex::encode_upper(&want));
            bad += 1;
        }
    }
    assert_eq!(bad, 0, "all targets byte-exact");
}
