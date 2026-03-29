//! Real-time state sync via WebSocket subscription to rippled.
//!
//! Architecture:
//! 1. Subscribe to "ledger" + "transactions" streams
//! 2. Accumulate transaction metadata as txs stream in
//! 3. On ledgerClosed: process the accumulated data, fetch objects, update state
//! 4. For the FIRST ledger after subscribe: use RPC to get full tx metadata
//!    (some txs may have streamed before we subscribed)
//!
//! No gap fill. No historical fetches. Objects hot in rippled's cache.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use xrpl_core::types::Hash256;
use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_LEAF_NODE};

const WS_URL: &str = "ws://10.0.0.97:6006";
const RPC_URL: &str = "http://10.0.0.97:5005";

pub async fn start_ws_sync(
    db: Arc<rocksdb::DB>,
    hash_comp: Arc<crate::state_hash::StateHashComputer>,
    last_synced: Arc<AtomicU32>,
) {
    let rpc_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap();

    loop {
        eprintln!("[ws-sync] Connecting to {WS_URL}...");
        let ws = match connect_async(WS_URL).await {
            Ok((ws, _)) => ws,
            Err(e) => {
                eprintln!("[ws-sync] Connect failed: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };
        let (mut sink, mut stream) = ws.split();

        // Subscribe to ledger stream only — we fetch tx metadata via RPC
        // (streaming txs arrive BEFORE ledgerClosed, causing timing issues)
        let sub = serde_json::json!({"command":"subscribe","streams":["ledger"]});
        if let Err(e) = sink.send(tokio_tungstenite::tungstenite::Message::Text(sub.to_string())).await {
            eprintln!("[ws-sync] Subscribe failed: {e}");
            continue;
        }
        eprintln!("[ws-sync] Connected + subscribed");

        // Accumulate tx metadata per ledger
        let mut acc_seq: u32 = 0;
        let mut acc_modified: HashSet<String> = HashSet::new();
        let mut acc_deleted: HashSet<String> = HashSet::new();
        let mut acc_tx_count: u32 = 0;
        let mut first_ledger = true;
        let mut processing = false;
        let mut last_processed: u32 = 0;

        while let Some(msg) = stream.next().await {
            let text = match msg {
                Ok(tokio_tungstenite::tungstenite::Message::Text(t)) => t,
                Ok(tokio_tungstenite::tungstenite::Message::Ping(data)) => {
                    let _ = sink.send(tokio_tungstenite::tungstenite::Message::Pong(data)).await;
                    continue;
                }
                Ok(tokio_tungstenite::tungstenite::Message::Close(_)) => {
                    eprintln!("[ws-sync] Server closed connection");
                    break;
                }
                Err(e) => {
                    eprintln!("[ws-sync] Read error: {e}");
                    break;
                }
                _ => continue,
            };

            let body: serde_json::Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let msg_type = body.get("type").and_then(|t| t.as_str()).unwrap_or("");

            // Ledger closed — process ALL ledgers from last_synced+1 to this one
            if msg_type == "ledgerClosed" {
                let closed_seq = body["ledger_index"].as_u64().unwrap_or(0) as u32;
                if closed_seq == 0 { continue; }

                // Fill gap from last_processed+1 to closed_seq
                let start_seq = if last_processed > 0 { last_processed + 1 } else { last_synced.load(Ordering::Relaxed) + 1 };

                for process_seq in start_seq..=closed_seq {
                    let account_hash = if process_seq == closed_seq {
                        fetch_account_hash(&rpc_client, process_seq).await.unwrap_or_default()
                    } else {
                        String::new()
                    };

                    acc_modified.clear();
                    acc_deleted.clear();
                    acc_tx_count = 0;

                    let mut meta_ok = false;
                    for attempt in 0..5u32 {
                        match fetch_ledger_metadata(&rpc_client, process_seq, &mut acc_modified, &mut acc_deleted, &mut acc_tx_count).await {
                            Ok(()) => { meta_ok = true; break; }
                            Err(e) => {
                                if attempt < 4 {
                                    let delay = 500 * (1 << attempt); // 500ms, 1s, 2s, 4s
                                    eprintln!("[ws-sync] Metadata #{process_seq} attempt {}: {e} — retry in {delay}ms", attempt + 1);
                                    tokio::time::sleep(std::time::Duration::from_millis(delay as u64)).await;
                                    acc_modified.clear();
                                    acc_deleted.clear();
                                    acc_tx_count = 0;
                                } else {
                                    eprintln!("[ws-sync] Metadata #{process_seq} FAILED after 5 attempts: {e} — skipping");
                                }
                            }
                        }
                    }
                    if !meta_ok { continue; }

                    // Always include protocol-level singletons
                    acc_modified.insert("B4979A36CDC7F3D3D5C31A4EAE2AC7D7209DDA877588B9AFC66799692AB0D66B".to_string()); // LedgerHashes
                    acc_modified.insert("2E8A59AA9D3B5B186B0B9E0F62E6C02587CA74A4D778938E957B6357D364B244".to_string()); // NegativeUNL
                    acc_modified.insert("7DB0788C020F02780A673DC74757F23823FA3014C1866E72CC4CD8B226CD6EF4".to_string()); // Amendments
                    acc_modified.insert("4BC50C9B0D8515D3EAAE1E74B29A95804346C491EE1A95BF25E4AAB854A6A651".to_string()); // FeeSettings

                    // Final cleanup
                    for d in acc_deleted.iter().cloned().collect::<Vec<_>>() {
                        acc_modified.remove(&d);
                    }

                    // Compute hash on every ledger close (the last one in each batch)
                    let compute_hash = process_seq == closed_seq;
                    let result = process_ledger(
                        &rpc_client, &db, &hash_comp,
                        process_seq, &acc_modified, &acc_deleted,
                        &account_hash, acc_tx_count, compute_hash,
                    ).await;

                    last_processed = process_seq;
                    if result {
                        last_synced.store(process_seq, Ordering::Relaxed);
                    }
                } // end for process_seq

                first_ledger = false;
                acc_seq = closed_seq + 1;
            }
        }

        eprintln!("[ws-sync] Disconnected. Reconnecting in 3s...");
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}

fn extract_affected_nodes(body: &serde_json::Value, modified: &mut HashSet<String>, deleted: &mut HashSet<String>) {
    let meta = if body["meta"].is_object() { &body["meta"] }
        else if body["metaData"].is_object() { &body["metaData"] }
        else { return; };

    if let Some(nodes) = meta["AffectedNodes"].as_array() {
        for node in nodes {
            if let Some(c) = node.get("CreatedNode") {
                if let Some(i) = c["LedgerIndex"].as_str() {
                    modified.insert(i.to_string());
                    deleted.remove(i);
                }
            }
            if let Some(m) = node.get("ModifiedNode") {
                if let Some(i) = m["LedgerIndex"].as_str() {
                    // Guard: don't re-add if already marked deleted
                    if !deleted.contains(i) {
                        modified.insert(i.to_string());
                    }
                }
            }
            if let Some(d) = node.get("DeletedNode") {
                if let Some(i) = d["LedgerIndex"].as_str() {
                    deleted.insert(i.to_string());
                    modified.remove(i);
                }
            }
        }
    }
}

async fn fetch_ledger_metadata(
    client: &reqwest::Client, seq: u32,
    modified: &mut HashSet<String>, deleted: &mut HashSet<String>,
    tx_count: &mut u32,
) -> Result<(), String> {
    let resp = client.post(RPC_URL)
        .json(&serde_json::json!({
            "method": "ledger",
            "params": [{"ledger_index": seq, "transactions": true, "expand": true, "binary": false}]
        }))
        .send().await.map_err(|e| format!("{e}"))?;

    let body: serde_json::Value = resp.json().await.map_err(|e| format!("{e}"))?;
    let txs = body["result"]["ledger"]["transactions"].as_array()
        .ok_or("no transactions")?;

    // Sort by TransactionIndex (execution order) — array order can differ!
    let mut sorted_txs: Vec<&serde_json::Value> = txs.iter().collect();
    sorted_txs.sort_by_key(|tx| {
        let meta = if tx["meta"].is_object() { &tx["meta"] }
            else if tx["metaData"].is_object() { &tx["metaData"] }
            else { return 0u64; };
        meta["TransactionIndex"].as_u64().unwrap_or(0)
    });

    for tx in sorted_txs {
        extract_affected_nodes(tx, modified, deleted);
        *tx_count += 1;
    }
    Ok(())
}

async fn process_ledger(
    client: &reqwest::Client,
    db: &Arc<rocksdb::DB>,
    hash_comp: &Arc<crate::state_hash::StateHashComputer>,
    seq: u32,
    modified: &HashSet<String>,
    deleted: &HashSet<String>,
    account_hash: &str,
    tx_count: u32,
    compute_hash: bool,
) -> bool {
    // Fetch all modified objects sequentially (hot in cache)
    let mut fetched_data: Vec<([u8; 32], Vec<u8>)> = Vec::new();
    let mut failed = 0u32;

    for index_hex in modified {
        let mut ok = false;
        for _ in 0..3u32 {
            let resp = client.post(RPC_URL)
                .json(&serde_json::json!({
                    "method": "ledger_entry",
                    "params": [{"index": index_hex, "binary": true, "ledger_index": seq}]
                }))
                .send().await;

            if let Ok(r) = resp {
                if let Ok(body) = r.json::<serde_json::Value>().await {
                    // Verify response is for the correct ledger
                    let resp_seq = body["result"]["ledger_index"].as_u64().unwrap_or(0) as u32;
                    if resp_seq != 0 && resp_seq != seq {
                        eprintln!("[ws-sync] WRONG LEDGER for {}: requested={seq} got={resp_seq}", &index_hex[..8]);
                        continue; // retry
                    }
                    if let Some(data_hex) = body["result"]["node_binary"].as_str() {
                        if let Ok(data) = hex::decode(data_hex) {
                            if let Ok(kb) = hex::decode(index_hex) {
                                if kb.len() == 32 {
                                    let mut key = [0u8; 32];
                                    key.copy_from_slice(&kb);
                                    fetched_data.push((key, data));
                                    ok = true;
                                    break;
                                }
                            }
                        }
                    }
                    if body["result"]["error"].as_str() == Some("entryNotFound") {
                        // Try xrplcluster.com as fallback (full history)
                        let fb = client.post("https://xrplcluster.com")
                            .json(&serde_json::json!({
                                "method": "ledger_entry",
                                "params": [{"index": index_hex, "binary": true, "ledger_index": seq}]
                            }))
                            .send().await;
                        if let Ok(r2) = fb {
                            if let Ok(body2) = r2.json::<serde_json::Value>().await {
                                if let Some(data_hex) = body2["result"]["node_binary"].as_str() {
                                    if let Ok(data) = hex::decode(data_hex) {
                                        if let Ok(kb) = hex::decode(index_hex) {
                                            if kb.len() == 32 {
                                                let mut key = [0u8; 32];
                                                key.copy_from_slice(&kb);
                                                fetched_data.push((key, data));
                                                ok = true;
                                                break;
                                            }
                                        }
                                    }
                                }
                                // Both local + public say entryNotFound — object was deleted
                                // Treat as deletion: add to fetched with empty marker so we delete from DB
                                if body2["result"]["error"].as_str() == Some("entryNotFound") {
                                    if let Ok(kb) = hex::decode(index_hex) {
                                        if kb.len() == 32 {
                                            let mut key = [0u8; 32];
                                            key.copy_from_slice(&kb);
                                            // Push with empty data — we'll detect and delete below
                                            fetched_data.push((key, Vec::new()));
                                        }
                                    }
                                    ok = true;
                                    break;
                                }
                            }
                        }
                        break; // fallback failed — ok stays false
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        if !ok { failed += 1; }
    }

    if failed > 0 {
        eprintln!("[ws-sync] Ledger #{seq}: {failed} failures — skipping");
        return false;
    }

    // Atomic write to RocksDB
    let mut batch = rocksdb::WriteBatch::default();
    let mut keys: Vec<Hash256> = Vec::new();

    for (key, data) in &fetched_data {
        if data.is_empty() {
            // entryNotFound on both local + public — delete stale entry
            batch.delete(key);
        } else {
            batch.put(key, data);
        }
        keys.push(Hash256(*key));
    }
    for index_hex in deleted {
        if let Ok(kb) = hex::decode(index_hex) {
            if kb.len() == 32 {
                batch.delete(&kb);
                let mut k = Hash256([0u8; 32]);
                k.0.copy_from_slice(&kb);
                keys.push(k);
            }
        }
    }

    if let Err(e) = db.write(batch) {
        eprintln!("[ws-sync] Ledger #{seq}: DB write failed: {e}");
        return false;
    }

    if compute_hash {
        // Full hash computation (~2.5s) — update cache + compute root
        if let Some(root) = hash_comp.update_and_hash(db, &keys) {
            let ours = hex::encode(root.0);
            if !account_hash.is_empty() {
                let matched = ours.to_uppercase() == account_hash.to_uppercase();
                hash_comp.set_network_hash(account_hash, seq);
                if matched {
                    eprintln!("[ws-sync] #{seq}: MATCH ({tx_count} txs, {} objs)", fetched_data.len());
                } else {
                    eprintln!("[ws-sync] #{seq}: MISMATCH ({tx_count} txs, {} modified, {} deleted) ours={} net={}",
                        modified.len(), deleted.len(),
                        &ours[..16], &account_hash[..16.min(account_hash.len())]);
                    // DIAGNOSTIC: on first mismatch, scan ALL ledger_data pages to find differing keys
                    let mut diag_marker: Option<String> = None;
                    let mut differ = 0u32;
                    let mut missing = 0u32;
                    let mut extra_in_db = 0u32;
                    let mut net_count = 0u64;
                    let mut net_keys: std::collections::HashSet<[u8;32]> = std::collections::HashSet::new();
                    loop {
                        let mut p = serde_json::json!({"ledger_index":seq,"binary":true,"limit":2048});
                        if let Some(ref m) = diag_marker { p["marker"] = serde_json::Value::String(m.clone()); }
                        let diag_resp = match client.post(RPC_URL)
                            .json(&serde_json::json!({"method":"ledger_data","params":[p]}))
                            .send().await {
                            Ok(r) => r,
                            Err(_) => break,
                        };
                        let diag_body: serde_json::Value = match diag_resp.json().await {
                            Ok(b) => b,
                            Err(_) => break,
                        };
                        if let Some(objs) = diag_body["result"]["state"].as_array() {
                            for obj in objs {
                                if let (Some(idx), Some(net_data)) = (obj["index"].as_str(), obj["data"].as_str()) {
                                    if let Ok(kb) = hex::decode(idx) {
                                        if kb.len() == 32 {
                                            let mut k32 = [0u8;32];
                                            k32.copy_from_slice(&kb);
                                            net_keys.insert(k32);
                                            net_count += 1;
                                            match db.get(&kb) {
                                                Ok(Some(our_data)) => {
                                                    if let Ok(nd) = hex::decode(net_data) {
                                                        if our_data.as_ref() as &[u8] != nd.as_slice() {
                                                            differ += 1;
                                                            if differ <= 5 {
                                                                eprintln!("[ws-sync] DIFF key={} ours={}b net={}b", &idx[..16], our_data.len(), nd.len());
                                                            }
                                                        }
                                                    }
                                                }
                                                Ok(None) => {
                                                    missing += 1;
                                                    if missing <= 5 {
                                                        eprintln!("[ws-sync] MISSING key={}", &idx[..16]);
                                                    }
                                                }
                                                Err(_) => {}
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        diag_marker = diag_body["result"]["marker"].as_str().map(String::from);
                        if diag_marker.is_none() { break; }
                        if net_count % 2_000_000 < 2048 {
                            eprintln!("[ws-sync] DIAG scanning: {net_count}... ({differ} diff, {missing} missing)");
                        }
                    }
                    // Check for extra keys in our DB
                    let snap = db.snapshot();
                    let iter = snap.iterator(rocksdb::IteratorMode::Start);
                    let mut our_count = 0u64;
                    for item in iter {
                        if let Ok((key, _)) = item {
                            if key.len() == 32 {
                                our_count += 1;
                                let mut k32 = [0u8;32];
                                k32.copy_from_slice(&key);
                                if !net_keys.contains(&k32) {
                                    extra_in_db += 1;
                                    if extra_in_db <= 5 {
                                        eprintln!("[ws-sync] EXTRA in DB: {}", hex::encode(&key[..16]));
                                    }
                                }
                            }
                        }
                    }
                    eprintln!("[ws-sync] DIAG COMPLETE: net={net_count} ours={our_count} | {differ} differ, {missing} missing, {extra_in_db} extra in DB");
                }
                return matched;
            }
        }
    } else {
        // Fast path: just update the leaf cache, skip root hash computation
        // This takes ~0.5s instead of ~3s
        use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_LEAF_NODE};
        let mut cache = hash_comp.leaf_cache.lock();
        if !cache.is_empty() {
            for key in &keys {
                match db.get(&key.0) {
                    Ok(Some(data)) => {
                        let mut buf = Vec::with_capacity(data.len() + 32);
                        buf.extend_from_slice(&data);
                        buf.extend_from_slice(&key.0);
                        let lh = sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf);
                        match cache.binary_search_by(|e| e.0 .0.cmp(&key.0)) {
                            Ok(idx) => cache[idx].1 = lh,
                            Err(idx) => cache.insert(idx, (*key, lh)),
                        }
                    }
                    Ok(None) => {
                        if let Ok(idx) = cache.binary_search_by(|e| e.0 .0.cmp(&key.0)) {
                            cache.remove(idx);
                        }
                    }
                    Err(_) => {}
                }
            }
        }
        eprintln!("[ws-sync] #{seq}: synced ({tx_count} txs, {} objs)", fetched_data.len());
    }
    false
}

async fn fetch_account_hash(client: &reqwest::Client, seq: u32) -> Option<String> {
    let resp = client.post(RPC_URL)
        .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":seq}]}))
        .send().await.ok()?;
    let body: serde_json::Value = resp.json().await.ok()?;
    body["result"]["ledger"]["account_hash"].as_str().map(String::from)
}
