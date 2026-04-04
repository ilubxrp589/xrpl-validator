//! Debug harness: compare SHAMap tree vs flat array hash after real ledger updates.
//! Reads from the live RocksDB (read-only) and fetches real ledger data from rippled.
//! Finds the exact key that causes divergence.

use std::collections::HashSet;
use xrpl_core::types::Hash256;
use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_LEAF_NODE, HASH_PREFIX_INNER_NODE};
use xrpl_ledger::shamap::node::{ZERO_HASH, nibble_at};
use xrpl_ledger::shamap::tree::{SHAMap, TreeType};

const RPC: &str = "http://localhost:5005";

fn compute_subtree(entries: &[(Hash256, Hash256)], depth: usize) -> Hash256 {
    if entries.is_empty() { return ZERO_HASH; }
    if entries.len() == 1 { return entries[0].1; }
    let mut child_hashes = [ZERO_HASH; 16];
    let mut pos = 0;
    for nibble in 0..16u8 {
        let end = entries[pos..].partition_point(|&(key, _)| nibble_at(&key, depth) <= nibble) + pos;
        let bucket_start = entries[pos..end].partition_point(|&(key, _)| nibble_at(&key, depth) < nibble) + pos;
        if bucket_start < end {
            child_hashes[nibble as usize] = compute_subtree(&entries[bucket_start..end], depth + 1);
        }
        pos = end;
    }
    let mut data = [0u8; 16 * 32];
    for (i, h) in child_hashes.iter().enumerate() {
        data[i * 32..(i + 1) * 32].copy_from_slice(&h.0);
    }
    sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, &data)
}

fn leaf_hash(data: &[u8], key: &[u8; 32]) -> Hash256 {
    let mut buf = Vec::with_capacity(data.len() + 32);
    buf.extend_from_slice(data);
    buf.extend_from_slice(key);
    sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf)
}

#[tokio::main]
async fn main() {
    let db_path = std::env::args().nth(1)
        .unwrap_or_else(|| "/mnt/xrpl-data/sync/state.rocks".to_string());

    eprintln!("Opening RocksDB at {db_path} (read-only)...");
    let db = rocksdb::DB::open_for_read_only(&rocksdb::Options::default(), &db_path, false)
        .expect("Failed to open RocksDB");

    // Step 1: Build both structures from RocksDB
    eprintln!("Building flat array + SHAMap from RocksDB...");
    let mut flat: Vec<(Hash256, Hash256)> = Vec::with_capacity(19_000_000);
    let mut tree = SHAMap::new(TreeType::State);
    let mut count = 0u64;

    let snapshot = db.snapshot();
    for item in snapshot.iterator(rocksdb::IteratorMode::Start) {
        if let Ok((key, value)) = item {
            if key.len() == 32 {
                let mut k = [0u8; 32];
                k.copy_from_slice(&key);
                let lh = leaf_hash(&value, &k);
                flat.push((Hash256(k), lh));
                let _ = tree.insert_hash_only(Hash256(k), lh);
                count += 1;
                if count % 2_000_000 == 0 {
                    eprintln!("  {count}...");
                }
            }
        }
    }
    drop(snapshot);
    eprintln!("Built {count} entries");

    // Step 2: Compare initial hashes
    let flat_hash = compute_subtree(&flat, 0);
    let tree_hash = tree.root_hash();
    eprintln!("Initial flat:  {}", hex::encode(&flat_hash.0[..16]));
    eprintln!("Initial tree:  {}", hex::encode(&tree_hash.0[..16]));
    if flat_hash != tree_hash {
        eprintln!("INITIAL MISMATCH — tree is structurally wrong at this scale!");
        // Find which root branch differs
        for nibble in 0..16u8 {
            let bucket: Vec<_> = flat.iter()
                .filter(|(k, _)| nibble_at(k, 0) == nibble)
                .cloned().collect();
            let flat_branch = compute_subtree(&bucket, 1);
            // Get tree branch hash
            if let xrpl_ledger::shamap::node::SHAMapNode::Inner(ref inner) = tree.root {
                let tree_branch = inner.child_hash(nibble);
                if flat_branch != tree_branch {
                    eprintln!("  Branch {nibble:X}: flat={} tree={} ({} entries)",
                        hex::encode(&flat_branch.0[..8]),
                        hex::encode(&tree_branch.0[..8]),
                        bucket.len());
                }
            }
        }
        return;
    }
    eprintln!("Initial hashes MATCH!");

    // Step 3: Fetch a real ledger's updates from rippled
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build().unwrap();

    // Get current validated ledger
    let resp = client.post(RPC)
        .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":"validated"}]}))
        .send().await.unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let val_seq: u32 = body["result"]["ledger"]["ledger_index"].as_str()
        .and_then(|s| s.parse().ok()).unwrap_or(0);
    eprintln!("\nFetching ledger #{val_seq} metadata...");

    // Fetch transaction metadata
    let resp = client.post(RPC)
        .json(&serde_json::json!({"method":"ledger","params":[{
            "ledger_index": val_seq, "transactions": true, "expand": true, "binary": false
        }]}))
        .send().await.unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let txs = body["result"]["ledger"]["transactions"].as_array().unwrap();

    // Extract affected nodes (sorted by TransactionIndex)
    let mut sorted_txs: Vec<&serde_json::Value> = txs.iter().collect();
    sorted_txs.sort_by_key(|tx| {
        let meta = if tx["meta"].is_object() { &tx["meta"] }
            else if tx["metaData"].is_object() { &tx["metaData"] }
            else { return 0u64; };
        meta["TransactionIndex"].as_u64().unwrap_or(0)
    });

    let mut modified: HashSet<String> = HashSet::new();
    let mut deleted: HashSet<String> = HashSet::new();
    for tx in &sorted_txs {
        let meta = if tx["meta"].is_object() { &tx["meta"] }
            else if tx["metaData"].is_object() { &tx["metaData"] }
            else { continue; };
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
                        if !deleted.contains(i) { modified.insert(i.to_string()); }
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

    // Add singletons
    modified.insert("B4979A36CDC7F3D3D5C31A4EAE2AC7D7209DDA877588B9AFC66799692AB0D66B".to_string());
    modified.insert("2E8A59AA9D3B5B186B0B9E0F62E6C02587CA74A4D778938E957B6357D364B244".to_string());
    modified.insert("7DB0788C020F02780A673DC74757F23823FA3014C1866E72CC4CD8B226CD6EF4".to_string());
    modified.insert("4BC50C9B0D8515D3EAAE1E74B29A95804346C491EE1A95BF25E4AAB854A6A651".to_string());
    // LedgerHashes sub-page
    {
        use sha2::{Sha512, Digest};
        let group = val_seq / 65536;
        let mut data = vec![0x00u8, 0x73];
        data.extend_from_slice(&group.to_be_bytes());
        let hash = Sha512::digest(&data);
        modified.insert(hex::encode(&hash[..32]).to_uppercase());
    }
    // Remove deleted from modified
    for d in deleted.iter().cloned().collect::<Vec<_>>() {
        modified.remove(&d);
    }

    eprintln!("Ledger #{val_seq}: {} txs, {} modified, {} deleted", txs.len(), modified.len(), deleted.len());

    // Step 4: Fetch each modified object and apply to both structures
    let mut updates: Vec<(Hash256, Option<Hash256>)> = Vec::new();

    for index_hex in &modified {
        let resp = client.post(RPC)
            .json(&serde_json::json!({"method":"ledger_entry","params":[{
                "index": index_hex, "binary": true, "ledger_index": val_seq
            }]}))
            .send().await;
        if let Ok(r) = resp {
            if let Ok(body) = r.json::<serde_json::Value>().await {
                if let Some(data_hex) = body["result"]["node_binary"].as_str() {
                    if let (Ok(data), Ok(kb)) = (hex::decode(data_hex), hex::decode(index_hex)) {
                        if kb.len() == 32 {
                            let mut k = [0u8; 32];
                            k.copy_from_slice(&kb);
                            let lh = leaf_hash(&data, &k);
                            updates.push((Hash256(k), Some(lh)));
                        }
                    }
                } else if body["result"]["error"].as_str() == Some("entryNotFound") {
                    if let Ok(kb) = hex::decode(index_hex) {
                        if kb.len() == 32 {
                            let mut k = [0u8; 32];
                            k.copy_from_slice(&kb);
                            updates.push((Hash256(k), None));
                        }
                    }
                }
            }
        }
    }
    for index_hex in &deleted {
        if let Ok(kb) = hex::decode(index_hex) {
            if kb.len() == 32 {
                let mut k = [0u8; 32];
                k.copy_from_slice(&kb);
                updates.push((Hash256(k), None));
            }
        }
    }

    eprintln!("Fetched {} updates. Applying...", updates.len());

    // Apply to SHAMap tree
    for (key, hash) in &updates {
        match hash {
            Some(h) => { let _ = tree.insert_hash_only(*key, *h); }
            None => { let _ = tree.delete(key); }
        }
    }

    // Apply to flat array (single-pass merge)
    updates.sort_by(|a, b| a.0 .0.cmp(&b.0 .0));
    let mut new_flat = Vec::with_capacity(flat.len() + updates.len());
    let (mut ci, mut ui) = (0usize, 0usize);
    while ci < flat.len() || ui < updates.len() {
        if ui >= updates.len() {
            new_flat.extend_from_slice(&flat[ci..]);
            break;
        } else if ci >= flat.len() {
            for u in &updates[ui..] { if let Some(h) = u.1 { new_flat.push((u.0, h)); } }
            break;
        } else if flat[ci].0 .0 < updates[ui].0 .0 {
            new_flat.push(flat[ci]); ci += 1;
        } else if flat[ci].0 .0 > updates[ui].0 .0 {
            if let Some(h) = updates[ui].1 { new_flat.push((updates[ui].0, h)); } ui += 1;
        } else {
            if let Some(h) = updates[ui].1 { new_flat.push((updates[ui].0, h)); }
            ci += 1; ui += 1;
        }
    }
    flat = new_flat;

    // Step 5: Compare hashes
    let flat_hash2 = compute_subtree(&flat, 0);
    tree.invalidate_caches();
    let tree_hash2 = tree.root_hash();

    // Count: verify both have same number of entries
    eprintln!("  Flat entries: {}  Tree entries: {}", flat.len(), tree.len());
    eprintln!("\nAfter updates:");
    eprintln!("  Flat:  {} ({} entries)", hex::encode(&flat_hash2.0[..16]), flat.len());
    eprintln!("  Tree:  {} ({} entries)", hex::encode(&tree_hash2.0[..16]), tree.len());

    if flat_hash2 == tree_hash2 {
        eprintln!("MATCH on ledger #{val_seq}!");

        // Continue testing sequential ledgers
        for next in 1..=20u32 {
            let next_seq = val_seq + next;
            let mut next_modified: HashSet<String> = HashSet::new();
            let mut next_deleted: HashSet<String> = HashSet::new();
            let mut next_tx_count = 0u32;

            // Fetch metadata
            let resp = client.post(RPC)
                .json(&serde_json::json!({"method":"ledger","params":[{
                    "ledger_index": next_seq, "transactions": true, "expand": true, "binary": false
                }]}))
                .send().await;
            if resp.is_err() { eprintln!("  Ledger #{next_seq}: fetch failed, stopping"); break; }
            let body: serde_json::Value = match resp.unwrap().json().await {
                Ok(b) => b, Err(_) => { eprintln!("  Ledger #{next_seq}: parse failed"); break; }
            };
            let txs = match body["result"]["ledger"]["transactions"].as_array() {
                Some(t) => t.clone(), None => { eprintln!("  Ledger #{next_seq}: no txs"); break; }
            };
            let mut stxs: Vec<&serde_json::Value> = txs.iter().collect();
            stxs.sort_by_key(|tx| {
                let meta = if tx["meta"].is_object() { &tx["meta"] }
                    else if tx["metaData"].is_object() { &tx["metaData"] }
                    else { return 0u64; };
                meta["TransactionIndex"].as_u64().unwrap_or(0)
            });
            for tx in &stxs {
                let meta = if tx["meta"].is_object() { &tx["meta"] }
                    else if tx["metaData"].is_object() { &tx["metaData"] }
                    else { continue; };
                if let Some(nodes) = meta["AffectedNodes"].as_array() {
                    for node in nodes {
                        if let Some(c) = node.get("CreatedNode") {
                            if let Some(i) = c["LedgerIndex"].as_str() { next_modified.insert(i.to_string()); next_deleted.remove(i); }
                        }
                        if let Some(m) = node.get("ModifiedNode") {
                            if let Some(i) = m["LedgerIndex"].as_str() { if !next_deleted.contains(i) { next_modified.insert(i.to_string()); } }
                        }
                        if let Some(d) = node.get("DeletedNode") {
                            if let Some(i) = d["LedgerIndex"].as_str() { next_deleted.insert(i.to_string()); next_modified.remove(i); }
                        }
                    }
                }
            }
            // Singletons
            next_modified.insert("B4979A36CDC7F3D3D5C31A4EAE2AC7D7209DDA877588B9AFC66799692AB0D66B".to_string());
            next_modified.insert("2E8A59AA9D3B5B186B0B9E0F62E6C02587CA74A4D778938E957B6357D364B244".to_string());
            next_modified.insert("7DB0788C020F02780A673DC74757F23823FA3014C1866E72CC4CD8B226CD6EF4".to_string());
            next_modified.insert("4BC50C9B0D8515D3EAAE1E74B29A95804346C491EE1A95BF25E4AAB854A6A651".to_string());
            { use sha2::{Sha512, Digest}; let g = next_seq / 65536; let mut d = vec![0x00u8, 0x73]; d.extend_from_slice(&g.to_be_bytes()); let h = Sha512::digest(&d); next_modified.insert(hex::encode(&h[..32]).to_uppercase()); }
            for d in next_deleted.iter().cloned().collect::<Vec<_>>() { next_modified.remove(&d); }

            // Fetch objects and apply
            let mut next_updates: Vec<(Hash256, Option<Hash256>)> = Vec::new();
            for idx in next_modified.iter().chain(next_deleted.iter()) {
                let resp = client.post(RPC)
                    .json(&serde_json::json!({"method":"ledger_entry","params":[{"index": idx, "binary": true, "ledger_index": next_seq}]}))
                    .send().await;
                if let Ok(r) = resp {
                    if let Ok(b) = r.json::<serde_json::Value>().await {
                        if let Some(dh) = b["result"]["node_binary"].as_str() {
                            if let (Ok(data), Ok(kb)) = (hex::decode(dh), hex::decode(idx)) {
                                if kb.len() == 32 {
                                    let mut k = [0u8; 32]; k.copy_from_slice(&kb);
                                    next_updates.push((Hash256(k), Some(leaf_hash(&data, &k))));
                                }
                            }
                        } else if b["result"]["error"].as_str() == Some("entryNotFound") {
                            if let Ok(kb) = hex::decode(idx) {
                                if kb.len() == 32 { let mut k = [0u8; 32]; k.copy_from_slice(&kb); next_updates.push((Hash256(k), None)); }
                            }
                        }
                    }
                }
            }

            // Apply to tree
            for (key, hash) in &next_updates {
                match hash { Some(h) => { let _ = tree.insert_hash_only(*key, *h); }, None => { let _ = tree.delete(key); } }
            }
            // Apply to flat
            next_updates.sort_by(|a, b| a.0.0.cmp(&b.0.0));
            let mut nf = Vec::with_capacity(flat.len() + next_updates.len());
            let (mut ci, mut ui) = (0, 0);
            while ci < flat.len() || ui < next_updates.len() {
                if ui >= next_updates.len() { nf.extend_from_slice(&flat[ci..]); break; }
                else if ci >= flat.len() { for u in &next_updates[ui..] { if let Some(h) = u.1 { nf.push((u.0, h)); } } break; }
                else if flat[ci].0.0 < next_updates[ui].0.0 { nf.push(flat[ci]); ci += 1; }
                else if flat[ci].0.0 > next_updates[ui].0.0 { if let Some(h) = next_updates[ui].1 { nf.push((next_updates[ui].0, h)); } ui += 1; }
                else { if let Some(h) = next_updates[ui].1 { nf.push((next_updates[ui].0, h)); } ci += 1; ui += 1; }
            }
            flat = nf;

            let fh = compute_subtree(&flat, 0);
            tree.invalidate_caches();
            let th = tree.root_hash();
            if fh == th {
                eprintln!("  Ledger #{next_seq}: MATCH ({} txs, {} updates, flat={} tree={})",
                    txs.len(), next_updates.len(), flat.len(), tree.len());
            } else {
                eprintln!("  Ledger #{next_seq}: DIVERGENCE! flat={} tree={}", hex::encode(&fh.0[..8]), hex::encode(&th.0[..8]));
                eprintln!("  flat_entries={} tree_entries={}", flat.len(), tree.len());
                // Check: which update keys are in the divergent area?
                for (key, hash) in &next_updates {
                    // Look up key in flat array
                    if let Ok(idx) = flat.binary_search_by(|e| e.0.0.cmp(&key.0)) {
                        // Key exists in flat — check if tree has same leaf hash
                        // by doing a fresh insert and seeing if hash changes
                    } else if hash.is_some() {
                        eprintln!("    KEY NOT IN FLAT: {} (should exist!)", hex::encode(&key.0[..8]));
                    }
                }
                // Rebuild a FRESH tree from the flat array and compare
                let mut fresh = SHAMap::new(TreeType::State);
                for (k, h) in &flat { let _ = fresh.insert_hash_only(*k, *h); }
                let fresh_hash = fresh.root_hash();
                eprintln!("  Fresh tree from flat: {} (entries={})", hex::encode(&fresh_hash.0[..8]), fresh.len());
                eprintln!("  Incremental tree:    {} (entries={})", hex::encode(&th.0[..8]), tree.len());
                if fresh_hash == fh {
                    eprintln!("  Fresh tree matches flat → incremental tree has structural damage");
                    // The updates from this ledger corrupted the tree
                    // Which specific update caused it?
                    for (i, (key, hash)) in next_updates.iter().enumerate() {
                        // Apply this ONE update to a fresh copy
                        let mut test = SHAMap::new(TreeType::State);
                        // Re-apply all previous updates + this one
                        // Too expensive — just log which updates were deletes
                        if hash.is_none() {
                            eprintln!("    Update #{i}: DELETE {}", hex::encode(&key.0[..16]));
                        }
                    }
                }
                break;
            }
        }
    } else {
        eprintln!("DIVERGENCE FOUND!");
        // Find which root branch differs
        for nibble in 0..16u8 {
            let bucket: Vec<_> = flat.iter()
                .filter(|(k, _)| nibble_at(k, 0) == nibble)
                .cloned().collect();
            let flat_branch = compute_subtree(&bucket, 1);
            if let xrpl_ledger::shamap::node::SHAMapNode::Inner(ref inner) = tree.root {
                let tree_branch = inner.child_hash(nibble);
                if flat_branch != tree_branch {
                    eprintln!("  Branch {nibble:X} differs: flat={} tree={} ({} entries)",
                        hex::encode(&flat_branch.0[..8]),
                        hex::encode(&tree_branch.0[..8]),
                        bucket.len());

                    // Drill into this branch to find the exact key
                    for depth2_nibble in 0..16u8 {
                        let sub: Vec<_> = bucket.iter()
                            .filter(|(k, _)| nibble_at(k, 1) == depth2_nibble)
                            .cloned().collect();
                        let flat_sub = compute_subtree(&sub, 2);
                        if let Some(child) = inner.get_child_node(nibble) {
                            if let xrpl_ledger::shamap::node::SHAMapNode::Inner(ref inner2) = child {
                                let tree_sub = inner2.child_hash(depth2_nibble);
                                if flat_sub != tree_sub {
                                    eprintln!("    Sub {nibble:X}{depth2_nibble:X} differs: {} entries, flat={} tree={}",
                                        sub.len(), hex::encode(&flat_sub.0[..8]), hex::encode(&tree_sub.0[..8]));
                                                    // Drill to depth 3
                                    for d3 in 0..16u8 {
                                        let sub3: Vec<_> = sub.iter()
                                            .filter(|(k, _)| nibble_at(k, 2) == d3)
                                            .cloned().collect();
                                        let flat_sub3 = compute_subtree(&sub3, 3);
                                        if let Some(child2) = inner2.get_child_node(depth2_nibble) {
                                            if let xrpl_ledger::shamap::node::SHAMapNode::Inner(ref inner3) = child2 {
                                                let tree_sub3 = inner3.child_hash(d3);
                                                if flat_sub3 != tree_sub3 {
                                                    eprintln!("      Depth3 {nibble:X}{depth2_nibble:X}{d3:X}: {} entries flat={} tree={}",
                                                        sub3.len(), hex::encode(&flat_sub3.0[..8]), hex::encode(&tree_sub3.0[..8]));
                                                    // Check if this is a key from our updates
                                                    for (k, _) in sub3.iter().take(3) {
                                                        let was_updated = updates.binary_search_by(|u| u.0 .0.cmp(&k.0)).is_ok();
                                                        eprintln!("        key={} updated={was_updated}", hex::encode(&k.0[..8]));
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
