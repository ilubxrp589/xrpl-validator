//! Test multiple rounds of incremental sync to find where it breaks.
use std::collections::HashSet;
use xrpl_core::types::Hash256;
use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_LEAF_NODE};
use xrpl_ledger::shamap::tree::{SHAMap, TreeType};
const RPC: &str = "http://10.0.0.39:5005";
const LEDGER_HASHES_KEY: &str = "B4979A36CDC7F3D3D5C31A4EAE2AC7D7209DDA877588B9AFC66799692AB0D66B";

fn build_hash(db: &rocksdb::DB) -> (String, u64) {
    let mut map = SHAMap::new(TreeType::State);
    let mut count = 0u64;
    for item in db.iterator(rocksdb::IteratorMode::Start) {
        if let Ok((key, value)) = item {
            if key.len() == 32 {
                let mut k = [0u8; 32];
                k.copy_from_slice(&key);
                let mut buf = Vec::with_capacity(value.len() + 32);
                buf.extend_from_slice(&value);
                buf.extend_from_slice(&k);
                let lh = sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf);
                let _ = map.insert_hash_only(Hash256(k), lh);
                count += 1;
            }
        }
    }
    (hex::encode(map.root_hash().0), count)
}

fn apply_ledger(client: &reqwest::blocking::Client, seq: u32, db: &rocksdb::DB) -> (u32, u32, u32) {
    let resp: serde_json::Value = client.post(RPC)
        .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":seq,"transactions":true,"expand":true,"binary":false}]}))
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
    let mut fetched = 0u32;
    let mut failed = 0u32;
    for idx in &modified {
        let r: serde_json::Value = client.post(RPC)
            .json(&serde_json::json!({"method":"ledger_entry","params":[{"index":idx,"binary":true,"ledger_index":seq}]}))
            .send().unwrap().json().unwrap();
        if let Some(d) = r["result"]["node_binary"].as_str() {
            if let (Ok(k), Ok(v)) = (hex::decode(idx), hex::decode(d)) {
                if k.len() == 32 { db.put(&k, &v).unwrap(); fetched += 1; }
            }
        } else { failed += 1; }
    }
    let mut del_count = 0u32;
    for idx in &deleted {
        if let Ok(k) = hex::decode(idx) { if k.len() == 32 { db.delete(&k).unwrap(); del_count += 1; } }
    }
    (fetched, del_count, failed)
}

fn parallel_download(db: &rocksdb::DB, seq: u32) -> u64 {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    let synced = Arc::new(AtomicU64::new(0));
    let start = std::time::Instant::now();
    let mut handles = Vec::new();
    for i in 0..4u8 {
        let db_path = "/tmp/test_multi_db";
        let synced = synced.clone();
        let start_byte = i * 64;
        let end_byte = if i == 3 { 255 } else { (i + 1) * 64 - 1 };
        let is_last = i == 3;
        handles.push(std::thread::spawn(move || {
            let client = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(60)).build().unwrap();
            let local_db = rocksdb::DB::open_default(db_path).unwrap_or_else(|_| panic!("open"));
            let mut marker: Option<String> = if i == 0 { None } else {
                let mut k = [0u8; 32]; k[0] = start_byte; Some(hex::encode(k))
            };
            let mut local = 0u64;
            let mut batch = rocksdb::WriteBatch::default();
            let mut bs = 0u32;
            loop {
                let mut p = serde_json::json!({"ledger_index":seq,"binary":true,"limit":2048});
                if let Some(ref m) = marker { p["marker"] = serde_json::Value::String(m.clone()); }
                let r: serde_json::Value = match client.post(RPC)
                    .json(&serde_json::json!({"method":"ledger_data","params":[p]}))
                    .send() { Ok(r) => r.json().unwrap(), Err(_) => continue };
                let mut past = false;
                if let Some(st) = r["result"]["state"].as_array() {
                    for o in st {
                        if let (Some(idx), Some(d)) = (o["index"].as_str(), o["data"].as_str()) {
                            if !is_last && idx.len() >= 2 {
                                let fb = u8::from_str_radix(&idx[0..2], 16).unwrap_or(0);
                                if fb > end_byte { past = true; break; }
                            }
                            if idx.len() == 64 {
                                if let (Ok(k), Ok(v)) = (hex::decode(idx), hex::decode(d)) {
                                    if k.len() == 32 { batch.put(&k, &v); bs += 1; local += 1; }
                                }
                            }
                        }
                    }
                }
                if bs >= 10_000 { let _ = local_db.write(batch); batch = rocksdb::WriteBatch::default(); bs = 0; }
                synced.fetch_add(local, Ordering::Relaxed);
                if past { break; }
                marker = r["result"]["marker"].as_str().map(String::from);
                if marker.is_none() { break; }
            }
            if bs > 0 { let _ = local_db.write(batch); }
            local
        }));
    }
    // Can't share DB across threads with default open. Use sequential download instead but
    // with the main thread. Actually the threads all try to open the same path which fails.
    // Let me just keep sequential but it's fine for a test.
    drop(handles); // This approach won't work — RocksDB can't be opened by multiple threads.
    0
}

fn main() {
    let client = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(60)).build().unwrap();
    let resp: serde_json::Value = client.post(RPC)
        .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":"validated"}]}))
        .send().unwrap().json().unwrap();
    let seq: u32 = resp["result"]["ledger"]["ledger_index"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
    eprintln!("Downloading ALL state at #{seq} (4 workers)...");
    let tmp = "/tmp/test_multi_db";
    let _ = std::fs::remove_dir_all(tmp);
    let db = std::sync::Arc::new(rocksdb::DB::open_default(tmp).unwrap());
    let synced = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let start = std::time::Instant::now();
    let mut handles = Vec::new();
    for i in 0..4u8 {
        let db = db.clone();
        let synced = synced.clone();
        let start_byte = i * 64;
        let end_byte = if i == 3 { 255u8 } else { (i + 1) * 64 - 1 };
        let is_last = i == 3;
        handles.push(std::thread::spawn(move || {
            let client = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(60)).build().unwrap();
            let mut marker: Option<String> = if i == 0 { None } else {
                let mut k = [0u8; 32]; k[0] = start_byte; Some(hex::encode(k))
            };
            let mut local = 0u64;
            let mut batch = rocksdb::WriteBatch::default();
            let mut bs = 0u32;
            loop {
                let mut p = serde_json::json!({"ledger_index":seq,"binary":true,"limit":2048});
                if let Some(ref m) = marker { p["marker"] = serde_json::Value::String(m.clone()); }
                let r: serde_json::Value = match client.post(RPC)
                    .json(&serde_json::json!({"method":"ledger_data","params":[p]}))
                    .send() { Ok(r) => r.json().unwrap(), Err(_) => continue };
                let mut past = false;
                if let Some(st) = r["result"]["state"].as_array() {
                    for o in st {
                        if let (Some(idx), Some(d)) = (o["index"].as_str(), o["data"].as_str()) {
                            if !is_last && idx.len() >= 2 {
                                let fb = u8::from_str_radix(&idx[0..2], 16).unwrap_or(0);
                                if fb > end_byte { past = true; break; }
                            }
                            if idx.len() == 64 {
                                if let (Ok(k), Ok(v)) = (hex::decode(idx), hex::decode(d)) {
                                    if k.len() == 32 { batch.put(&k, &v); bs += 1; local += 1; }
                                }
                            }
                        }
                    }
                }
                if bs >= 10_000 { let _ = db.write(batch); batch = rocksdb::WriteBatch::default(); bs = 0; }
                synced.fetch_add(local, std::sync::atomic::Ordering::Relaxed);
                if past { break; }
                marker = r["result"]["marker"].as_str().map(String::from);
                if marker.is_none() { break; }
            }
            if bs > 0 { let _ = db.write(batch); }
            local
        }));
    }
    for h in handles { let _ = h.join(); }
    let count = synced.load(std::sync::atomic::Ordering::Relaxed);
    let elapsed = start.elapsed().as_secs_f64();
    eprintln!("Downloaded {count} in {elapsed:.0}s ({:.0}/s)", count as f64 / elapsed);
    // Need to drop Arc to get owned DB for the rest
    let db = std::sync::Arc::try_unwrap(db).unwrap_or_else(|a| {
        // fallback: reopen
        drop(a);
        rocksdb::DB::open_default(tmp).unwrap()
    });
    let (h, _n) = build_hash(&db);
    let exp = resp["result"]["ledger"]["account_hash"].as_str().unwrap_or("?");
    eprintln!("Downloaded {count}. Hash: {} ({})", if h.to_uppercase() == exp.to_uppercase() {"MATCH"} else {"MISMATCH"}, &h[..16]);

    // Build persistent SHAMap for incremental updates (same as live system)
    eprintln!("Building live SHAMap...");
    let mut live_map = SHAMap::new(TreeType::State);
    for item in db.iterator(rocksdb::IteratorMode::Start) {
        if let Ok((key, value)) = item {
            if key.len() == 32 {
                let mut k = [0u8; 32]; k.copy_from_slice(&key);
                let mut buf = Vec::with_capacity(value.len() + 32);
                buf.extend_from_slice(&value); buf.extend_from_slice(&k);
                let lh = sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf);
                let _ = live_map.insert_hash_only(Hash256(k), lh);
            }
        }
    }
    let _initial = live_map.root_hash(); // force cache computation
    eprintln!("Live SHAMap ready.");

    // Apply 60 rounds
    for round in 1..=60 {
        let next = seq + round;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let r: serde_json::Value = client.post(RPC)
                .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":next}]}))
                .send().unwrap().json().unwrap();
            if r["result"]["ledger"]["account_hash"].is_string() { break; }
        }
        let r: serde_json::Value = client.post(RPC)
            .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":next}]}))
            .send().unwrap().json().unwrap();
        let exp = r["result"]["ledger"]["account_hash"].as_str().unwrap_or("?");
        let (fetched, deleted, failed) = apply_ledger(&client, next, &db);

        // Method A: Full rebuild (proven to work)
        let (h_rebuild, n) = build_hash(&db);
        let m_rebuild = if h_rebuild.to_uppercase() == exp.to_uppercase() {"MATCH"} else {"MISMATCH"};

        // Method B: Incremental update on persistent SHAMap (same as live system)
        // Re-read modified keys from RocksDB and update the live map
        let r2: serde_json::Value = client.post(RPC)
            .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":next,"transactions":true,"expand":true,"binary":false}]}))
            .send().unwrap().json().unwrap();
        let txs = r2["result"]["ledger"]["transactions"].as_array().unwrap();
        let mut all_keys: HashSet<String> = HashSet::new();
        let mut del_keys: HashSet<String> = HashSet::new();
        for tx in txs {
            let meta = if tx["metaData"].is_object() { &tx["metaData"] } else if tx["meta"].is_object() { &tx["meta"] } else { continue; };
            if let Some(nodes) = meta["AffectedNodes"].as_array() {
                for node in nodes {
                    if let Some(c) = node.get("CreatedNode") { if let Some(i) = c["LedgerIndex"].as_str() { all_keys.insert(i.to_string()); } }
                    if let Some(m) = node.get("ModifiedNode") { if let Some(i) = m["LedgerIndex"].as_str() { all_keys.insert(i.to_string()); } }
                    if let Some(d) = node.get("DeletedNode") { if let Some(i) = d["LedgerIndex"].as_str() { del_keys.insert(i.to_string()); all_keys.remove(i); } }
                }
            }
        }
        all_keys.insert(LEDGER_HASHES_KEY.to_string());
        for idx in &all_keys {
            if let Ok(key_bytes) = hex::decode(idx) {
                if key_bytes.len() == 32 {
                    if let Ok(Some(data)) = db.get(&key_bytes) {
                        let mut k = [0u8; 32]; k.copy_from_slice(&key_bytes);
                        let mut buf = Vec::with_capacity(data.len() + 32);
                        buf.extend_from_slice(&data); buf.extend_from_slice(&k);
                        let lh = sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf);
                        let _ = live_map.insert_hash_only(Hash256(k), lh);
                    }
                }
            }
        }
        for idx in &del_keys {
            if let Ok(key_bytes) = hex::decode(idx) {
                if key_bytes.len() == 32 {
                    let mut k = [0u8; 32]; k.copy_from_slice(&key_bytes);
                    let _ = live_map.delete(&Hash256(k));
                }
            }
        }
        let h_inc = hex::encode(live_map.root_hash().0);
        let m_inc = if h_inc.to_uppercase() == exp.to_uppercase() {"MATCH"} else {"MISMATCH"};

        eprintln!("Round {round} (#{next}): rebuild={m_rebuild} incremental={m_inc} — {fetched}f {deleted}d {failed}e {n}n");
        if m_inc == "MISMATCH" {
            eprintln!("  Rebuild:     {}", &h_rebuild[..32]);
            eprintln!("  Incremental: {}", &h_inc[..32]);
            eprintln!("  Expected:    {}", &exp[..32]);
            // Don't break — keep going to see if it stays mismatched
        }
    }
    drop(db);
    let _ = std::fs::remove_dir_all(tmp);
}
