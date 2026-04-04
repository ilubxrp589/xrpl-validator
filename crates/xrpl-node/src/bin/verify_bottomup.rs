//! Verify the bottom-up hash computation against the proven insert_hash_only approach.
//! Downloads all state, computes hash BOTH ways, compares.

use xrpl_core::types::Hash256;
use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_LEAF_NODE, HASH_PREFIX_INNER_NODE};
use xrpl_ledger::shamap::node::{ZERO_HASH, nibble_at};
use xrpl_ledger::shamap::tree::{SHAMap, TreeType};

const RPC: &str = "http://localhost:5005";

fn compute_subtree_hash(entries: &[(Hash256, Hash256)], depth: usize) -> Hash256 {
    if entries.is_empty() { return ZERO_HASH; }
    if entries.len() == 1 { return entries[0].1; }

    let mut child_hashes = [ZERO_HASH; 16];
    let mut start = 0;
    for nibble in 0..16u8 {
        let end = entries[start..].partition_point(|&(key, _)| {
            nibble_at(&key, depth) <= nibble
        }) + start;
        let bucket_start = entries[start..end].partition_point(|&(key, _)| {
            nibble_at(&key, depth) < nibble
        }) + start;
        if bucket_start < end {
            child_hashes[nibble as usize] = compute_subtree_hash(&entries[bucket_start..end], depth + 1);
        }
        start = end;
    }

    let mut data = [0u8; 16 * 32];
    for (i, h) in child_hashes.iter().enumerate() {
        data[i * 32..(i + 1) * 32].copy_from_slice(&h.0);
    }
    sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, &data)
}

fn main() {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60)).build().unwrap();

    let resp: serde_json::Value = client.post(RPC)
        .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":"validated"}]}))
        .send().unwrap().json().unwrap();
    let seq: u32 = resp["result"]["ledger"]["ledger_index"].as_str()
        .and_then(|s| s.parse().ok()).unwrap_or(0);
    let expected = resp["result"]["ledger"]["account_hash"].as_str().unwrap_or("?").to_string();
    eprintln!("Ledger #{seq}  Expected: {}", &expected[..16]);

    // Download all state
    let mut all: Vec<(Hash256, Hash256)> = Vec::with_capacity(19_000_000);
    let mut marker: Option<String> = None;
    let mut count = 0u64;
    let start = std::time::Instant::now();
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
                        if k.len() == 32 {
                            let mut key = [0u8; 32]; key.copy_from_slice(&k);
                            let mut buf = Vec::with_capacity(v.len() + 32);
                            buf.extend_from_slice(&v);
                            buf.extend_from_slice(&key);
                            let lh = sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf);
                            all.push((Hash256(key), lh));
                            count += 1;
                        }
                    }
                }
            }
        }
        marker = r["result"]["marker"].as_str().map(String::from);
        if marker.is_none() { break; }
        if count % 2_000_000 < 2048 { eprintln!("  {count}..."); }
    }
    let dl_time = start.elapsed().as_secs_f64();
    eprintln!("Downloaded {count} in {dl_time:.0}s");

    // Method 1: insert_hash_only (proven correct)
    let t1 = std::time::Instant::now();
    let mut tree = SHAMap::new(TreeType::State);
    for (key, lh) in &all {
        let _ = tree.insert_hash_only(*key, *lh);
    }
    let h1 = hex::encode(tree.root_hash().0);
    let t1_elapsed = t1.elapsed().as_secs_f64();
    eprintln!("Method 1 (insert_hash_only): {} in {t1_elapsed:.1}s  match={}", &h1[..16],
        h1.to_uppercase() == expected.to_uppercase());

    // Method 2: sorted bottom-up (no tree)
    let t2 = std::time::Instant::now();
    all.sort_unstable_by(|a, b| a.0.0.cmp(&b.0.0));
    let h2 = hex::encode(compute_subtree_hash(&all, 0).0);
    let t2_elapsed = t2.elapsed().as_secs_f64();
    eprintln!("Method 2 (bottom-up sorted): {} in {t2_elapsed:.1}s  match={}", &h2[..16],
        h2.to_uppercase() == expected.to_uppercase());

    eprintln!("\nMethod1 == Method2: {}", h1 == h2);
    eprintln!("Method1 == Expected: {}", h1.to_uppercase() == expected.to_uppercase());
    eprintln!("Method2 == Expected: {}", h2.to_uppercase() == expected.to_uppercase());
}
