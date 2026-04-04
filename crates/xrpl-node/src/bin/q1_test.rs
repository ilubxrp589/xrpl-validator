//! Q1 Killer Test: Is the live SHAMap's structure correct (cache bug)
//! or wrong (structural bug)?
//!
//! Downloads fresh, builds SHAMap, applies ONE ledger incrementally,
//! then compares: update_round hash vs rebuild hash vs cache-cleared hash.

use std::collections::HashSet;
use xrpl_core::types::Hash256;
use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_LEAF_NODE};
use xrpl_ledger::shamap::tree::{SHAMap, TreeType};
use xrpl_ledger::shamap::node::SHAMapNode;

const RPC: &str = "http://localhost:5005";
const LEDGER_HASHES_KEY: &str = "B4979A36CDC7F3D3D5C31A4EAE2AC7D7209DDA877588B9AFC66799692AB0D66B";
const NEGATIVE_UNL_KEY: &str = "2E8A59AA9D3B5B186B0B9E0F62E6C02587CA74A4D778938E957B6357D364B244";
const AMENDMENTS_KEY: &str = "7DB0788C020F02780A673DC74757F23823FA3014C1866E72CC4CD8B226CD6EF4";

fn force_clear_all_caches(node: &mut SHAMapNode) {
    if let SHAMapNode::Inner(inner) = node {
        // get_child_node_mut clears this node's cached_hash via set(None)
        // Calling it for any child is enough to clear THIS node's cache
        // Then recurse into all children
        for slot in 0..16u8 {
            if inner.has_child(slot) {
                let child = inner.get_child_node_mut(slot).unwrap();
                force_clear_all_caches(child);
            }
        }
        // Also clear cache for nodes with 0 children (empty inner nodes shouldn't exist but just in case)
        // The above loop already called get_child_node_mut which cleared the cache
        // But if no children exist, we need to clear manually
        if inner.child_count() == 0 {
            inner.set_cached_hash(Hash256([0u8; 32])); // will be wrong but forces recompute...
            // Actually we need cached_hash = None. Let's access the cell directly.
        }
    }
}

fn leaf_hash(key: &[u8; 32], data: &[u8]) -> Hash256 {
    let mut buf = Vec::with_capacity(data.len() + 32);
    buf.extend_from_slice(data);
    buf.extend_from_slice(key);
    sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf)
}

fn main() {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build().unwrap();

    // Get validated ledger
    let resp: serde_json::Value = client.post(RPC)
        .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":"validated"}]}))
        .send().unwrap().json().unwrap();
    let seq: u32 = resp["result"]["ledger"]["ledger_index"].as_str()
        .and_then(|s| s.parse().ok()).unwrap_or(0);
    let expected0 = resp["result"]["ledger"]["account_hash"].as_str().unwrap_or("?").to_string();
    eprintln!("=== Download at #{seq}, expected hash: {} ===", &expected0[..16]);

    // Download ALL state to temp RocksDB
    let tmp = "/tmp/q1_test_db";
    let _ = std::fs::remove_dir_all(tmp);
    let db = rocksdb::DB::open_default(tmp).unwrap();
    let mut marker: Option<String> = None;
    let mut count = 0u64;
    loop {
        let mut p = serde_json::json!({"ledger_index":seq,"binary":true,"limit":2048});
        if let Some(ref m) = marker { p["marker"] = serde_json::Value::String(m.clone()); }
        let r: serde_json::Value = client.post(RPC)
            .json(&serde_json::json!({"method":"ledger_data","params":[p]}))
            .send().unwrap().json().unwrap();
        if let Some(st) = r["result"]["state"].as_array() {
            for o in st {
                if let (Some(i), Some(d)) = (o["index"].as_str(), o["data"].as_str()) {
                    if let (Ok(k), Ok(v)) = (hex::decode(i), hex::decode(d)) {
                        if k.len() == 32 { db.put(&k, &v).unwrap(); count += 1; }
                    }
                }
            }
        }
        marker = r["result"]["marker"].as_str().map(String::from);
        if marker.is_none() { break; }
        if count % 2_000_000 < 2048 { eprintln!("  {count}..."); }
    }
    eprintln!("Downloaded {count} objects");

    // Build SHAMap (same as live system's start_computation)
    eprintln!("Building SHAMap...");
    let mut live_map = SHAMap::new(TreeType::State);
    for item in db.iterator(rocksdb::IteratorMode::Start) {
        if let Ok((key, value)) = item {
            if key.len() == 32 {
                let mut k = [0u8; 32]; k.copy_from_slice(&key);
                let lh = leaf_hash(&k, &value);
                let _ = live_map.insert_hash_only(Hash256(k), lh);
            }
        }
    }
    let initial_hash = hex::encode(live_map.root_hash().0);
    let m0 = if initial_hash.to_uppercase() == expected0.to_uppercase() { "MATCH" } else { "MISMATCH" };
    eprintln!("Initial build: {m0}");

    // Wait for next ledger
    let next = seq + 1;
    eprintln!("Waiting for #{next}...");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        let r: serde_json::Value = client.post(RPC)
            .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":next}]}))
            .send().unwrap().json().unwrap();
        if r["result"]["ledger"]["account_hash"].is_string() { break; }
    }
    let resp: serde_json::Value = client.post(RPC)
        .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":next}]}))
        .send().unwrap().json().unwrap();
    let expected1 = resp["result"]["ledger"]["account_hash"].as_str().unwrap_or("?").to_string();

    // Apply ONE ledger via update_round path (same as live system)
    let resp: serde_json::Value = client.post(RPC)
        .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":next,"transactions":true,"expand":true,"binary":false}]}))
        .send().unwrap().json().unwrap();
    let txs = resp["result"]["ledger"]["transactions"].as_array().unwrap();
    let mut modified: HashSet<String> = HashSet::new();
    let mut deleted: HashSet<String> = HashSet::new();
    for tx in txs {
        let meta = if tx["metaData"].is_object() { &tx["metaData"] } else if tx["meta"].is_object() { &tx["meta"] } else { continue; };
        if let Some(nodes) = meta["AffectedNodes"].as_array() {
            for node in nodes {
                if let Some(c) = node.get("CreatedNode") { if let Some(i) = c["LedgerIndex"].as_str() { modified.insert(i.to_string()); } }
                if let Some(m) = node.get("ModifiedNode") { if let Some(i) = m["LedgerIndex"].as_str() { modified.insert(i.to_string()); } }
                if let Some(d) = node.get("DeletedNode") { if let Some(i) = d["LedgerIndex"].as_str() { deleted.insert(i.to_string()); modified.remove(i); } }
            }
        }
    }
    modified.insert(LEDGER_HASHES_KEY.to_string());
    modified.insert(NEGATIVE_UNL_KEY.to_string());
    modified.insert(AMENDMENTS_KEY.to_string());

    // Fetch + write to RocksDB + update SHAMap (EXACTLY like update_round)
    let mut fetched = 0u32;
    for idx in &modified {
        let r: serde_json::Value = client.post(RPC)
            .json(&serde_json::json!({"method":"ledger_entry","params":[{"index":idx,"binary":true,"ledger_index":next}]}))
            .send().unwrap().json().unwrap();
        if let Some(d) = r["result"]["node_binary"].as_str() {
            if let (Ok(k), Ok(v)) = (hex::decode(idx), hex::decode(d)) {
                if k.len() == 32 {
                    db.put(&k, &v).unwrap();
                    let mut ka = [0u8; 32]; ka.copy_from_slice(&k);
                    let lh = leaf_hash(&ka, &v);
                    let _ = live_map.insert_hash_only(Hash256(ka), lh);
                    fetched += 1;
                }
            }
        }
    }
    for idx in &deleted {
        if let Ok(k) = hex::decode(idx) {
            if k.len() == 32 {
                db.delete(&k).unwrap();
                let mut ka = [0u8; 32]; ka.copy_from_slice(&k);
                let _ = live_map.delete(&Hash256(ka));
            }
        }
    }
    eprintln!("Applied #{next}: {fetched} fetched, {} deleted", deleted.len());

    // TEST A: Incremental hash (what the live system does)
    let hash_incremental = hex::encode(live_map.root_hash().0);
    let m_inc = if hash_incremental.to_uppercase() == expected1.to_uppercase() { "MATCH" } else { "MISMATCH" };

    // TEST B: Force-clear all caches, then rehash (Q1 killer test)
    force_clear_all_caches(&mut live_map.root);
    let hash_cleared = hex::encode(live_map.root_hash().0);
    let m_clr = if hash_cleared.to_uppercase() == expected1.to_uppercase() { "MATCH" } else { "MISMATCH" };

    // TEST C: Full rebuild from RocksDB (known good)
    let mut rebuild_map = SHAMap::new(TreeType::State);
    for item in db.iterator(rocksdb::IteratorMode::Start) {
        if let Ok((key, value)) = item {
            if key.len() == 32 {
                let mut k = [0u8; 32]; k.copy_from_slice(&key);
                let lh = leaf_hash(&k, &value);
                let _ = rebuild_map.insert_hash_only(Hash256(k), lh);
            }
        }
    }
    let hash_rebuild = hex::encode(rebuild_map.root_hash().0);
    let m_reb = if hash_rebuild.to_uppercase() == expected1.to_uppercase() { "MATCH" } else { "MISMATCH" };

    eprintln!("\n=== Q1 RESULTS ===");
    eprintln!("Expected:     {}", &expected1[..32]);
    eprintln!("Incremental:  {} — {m_inc}", &hash_incremental[..32]);
    eprintln!("Cache-clear:  {} — {m_clr}", &hash_cleared[..32]);
    eprintln!("Rebuild:      {} — {m_reb}", &hash_rebuild[..32]);
    eprintln!("Inc==Rebuild: {}", hash_incremental == hash_rebuild);
    eprintln!("Inc==Clear:   {}", hash_incremental == hash_cleared);

    if m_reb == "MATCH" && m_inc == "MISMATCH" && m_clr == "MATCH" {
        eprintln!("\n>>> CACHE BUG: tree structure correct, caches stale <<<");
    } else if m_reb == "MATCH" && m_inc == "MISMATCH" && m_clr == "MISMATCH" {
        eprintln!("\n>>> STRUCTURAL BUG: leaves in wrong positions <<<");
    } else if m_reb == "MATCH" && m_inc == "MATCH" {
        eprintln!("\n>>> ALL CORRECT — inc-sync works from clean state <<<");
    }

    drop(db);
    let _ = std::fs::remove_dir_all(tmp);
}
