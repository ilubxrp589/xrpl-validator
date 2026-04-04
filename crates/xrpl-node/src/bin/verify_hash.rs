//! Quick hash verification — downloads a small sample of state objects from
//! rippled and verifies our SHAMap hash computation matches.
//!
//! Run: cargo run --release -p xrpl-node --bin verify_hash

use serde_json::{json, Value};
use std::time::{Duration, Instant};
use xrpl_core::types::Hash256;
use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_LEAF_NODE};
use xrpl_ledger::shamap::tree::{SHAMap, TreeType};

const RPC: &str = "http://localhost:5005";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;

    // Get validated ledger
    let resp: Value = client
        .post(RPC)
        .json(&json!({"method":"ledger","params":[{"ledger_index":"validated"}]}))
        .send()
        .await?
        .json()
        .await?;

    let ledger = &resp["result"]["ledger"];
    let seq: u32 = ledger["ledger_index"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let expected_hash = ledger["account_hash"].as_str().unwrap_or("???");

    eprintln!("Ledger #{seq}");
    eprintln!("Expected account_hash: {expected_hash}");
    eprintln!();

    // Download ALL state objects via ledger_data pagination
    eprintln!("Downloading all state objects...");
    let start = Instant::now();
    let mut all_objects: Vec<(Hash256, Vec<u8>)> = Vec::new();
    let mut marker: Option<String> = None;
    let mut pages = 0u32;

    loop {
        pages += 1;
        let params = if let Some(ref m) = marker {
            json!({"ledger_index": seq, "limit": 2048, "binary": true, "marker": m})
        } else {
            json!({"ledger_index": seq, "limit": 2048, "binary": true})
        };

        let resp: Value = client
            .post(RPC)
            .json(&json!({"method": "ledger_data", "params": [params]}))
            .send()
            .await?
            .json()
            .await?;

        if let Some(state) = resp["result"]["state"].as_array() {
            for obj in state {
                let index = obj["index"].as_str().unwrap_or("");
                let data_hex = obj["data"].as_str().unwrap_or("");
                if let (Ok(key_bytes), Ok(data)) = (hex::decode(index), hex::decode(data_hex)) {
                    if key_bytes.len() == 32 {
                        let mut key = [0u8; 32];
                        key.copy_from_slice(&key_bytes);
                        all_objects.push((Hash256(key), data));
                    }
                }
            }
        }

        marker = resp["result"]["marker"].as_str().map(String::from);
        if marker.is_none() {
            break;
        }

        if pages % 100 == 0 {
            eprintln!("  page {pages}: {} objects so far...", all_objects.len());
        }
    }

    let dl_time = start.elapsed();
    eprintln!(
        "Downloaded {} objects in {:.1}s ({pages} pages)",
        all_objects.len(),
        dl_time.as_secs_f64(),
    );

    // Method 1: Build SHAMap using standard insert_hash_only
    eprintln!("\nMethod 1: Standard insert_hash_only...");
    let build_start = Instant::now();
    let mut map1 = SHAMap::new(TreeType::State);
    for (key, data) in &all_objects {
        let mut buf = Vec::with_capacity(32 + data.len());
        buf.extend_from_slice(data);
        buf.extend_from_slice(&key.0);
        let leaf_hash = sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf);
        let _ = map1.insert_hash_only(*key, leaf_hash);
    }
    let hash1 = hex::encode(map1.root_hash().0);
    eprintln!("  Hash: {hash1}");
    eprintln!("  Time: {:.1}s", build_start.elapsed().as_secs_f64());
    eprintln!("  Match: {}", hash1.to_uppercase() == expected_hash.to_uppercase());

    // Method 2: Build SHAMap using standard insert (with data)
    eprintln!("\nMethod 2: Standard insert with data...");
    let build_start = Instant::now();
    let mut map2 = SHAMap::new(TreeType::State);
    for (key, data) in &all_objects {
        let _ = map2.insert(*key, data.clone());
    }
    let hash2 = hex::encode(map2.root_hash().0);
    eprintln!("  Hash: {hash2}");
    eprintln!("  Time: {:.1}s", build_start.elapsed().as_secs_f64());
    eprintln!("  Match: {}", hash2.to_uppercase() == expected_hash.to_uppercase());

    // Method 3: Sort + bulk_build (same as state_hash.rs uses)
    eprintln!("\nMethod 3: Sorted bulk_build...");
    let build_start = Instant::now();
    let mut leaf_hashes: Vec<(Hash256, Hash256)> = all_objects
        .iter()
        .map(|(key, data)| {
            let mut buf = Vec::with_capacity(32 + data.len());
            buf.extend_from_slice(&key.0);
            buf.extend_from_slice(data);
            let leaf_hash = sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf);
            (*key, leaf_hash)
        })
        .collect();
    // Sort by key (same order as RocksDB would iterate)
    leaf_hashes.sort_by(|a, b| a.0 .0.cmp(&b.0 .0));

    use xrpl_ledger::shamap::hash::HASH_PREFIX_INNER_NODE;
    use xrpl_ledger::shamap::node::{InnerNode, LeafNode, SHAMapNode, ZERO_HASH};

    fn bulk_build(
        leaf_hashes: &[(Hash256, Hash256)],
        depth: usize,
    ) -> (SHAMapNode, Hash256) {
        if leaf_hashes.is_empty() {
            return (SHAMapNode::Inner(Box::<InnerNode>::default()), ZERO_HASH);
        }
        if leaf_hashes.len() == 1 {
            let node = SHAMapNode::Leaf(LeafNode::new_hash_only(
                leaf_hashes[0].0,
                leaf_hashes[0].1,
            ));
            return (node, leaf_hashes[0].1);
        }

        let mut inner = Box::<InnerNode>::default();
        let mut child_hashes_arr: [Hash256; 16] = [ZERO_HASH; 16];
        let mut child_start = 0;

        for nibble in 0..16u8 {
            let end = leaf_hashes[child_start..].partition_point(|&(key, _)| {
                let byte = key.0[depth / 2];
                let n = if depth % 2 == 0 {
                    (byte >> 4) & 0x0F
                } else {
                    byte & 0x0F
                };
                n <= nibble
            }) + child_start;

            let start_for_nibble =
                leaf_hashes[child_start..end].partition_point(|&(key, _)| {
                    let byte = key.0[depth / 2];
                    let n = if depth % 2 == 0 {
                        (byte >> 4) & 0x0F
                    } else {
                        byte & 0x0F
                    };
                    n < nibble
                }) + child_start;

            if start_for_nibble < end {
                let (child_node, child_hash) =
                    bulk_build(&leaf_hashes[start_for_nibble..end], depth + 1);
                inner.set_child_node(nibble, child_node);
                child_hashes_arr[nibble as usize] = child_hash;
            }
            child_start = end;
        }

        let mut data = [0u8; 16 * 32];
        for (i, h) in child_hashes_arr.iter().enumerate() {
            data[i * 32..(i + 1) * 32].copy_from_slice(&h.0);
        }
        let hash = sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, &data);
        inner.set_cached_hash(hash);
        (SHAMapNode::Inner(inner), hash)
    }

    let (root_node, root_hash) = bulk_build(&leaf_hashes, 0);
    let hash3 = hex::encode(root_hash.0);
    eprintln!("  Hash: {hash3}");
    eprintln!("  Time: {:.1}s", build_start.elapsed().as_secs_f64());
    eprintln!("  Match: {}", hash3.to_uppercase() == expected_hash.to_uppercase());

    // Summary
    eprintln!("\n=== Summary ===");
    eprintln!("Expected: {expected_hash}");
    eprintln!("Method 1: {hash1} (insert_hash_only)");
    eprintln!("Method 2: {hash2} (insert with data)");
    eprintln!("Method 3: {hash3} (bulk_build)");
    eprintln!("1==2: {}", hash1 == hash2);
    eprintln!("1==3: {}", hash1 == hash3);

    Ok(())
}
