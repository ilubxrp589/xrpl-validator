//! Verify a harvested full-state snapshot against the ledger's account_hash.
//!
//! Usage: state_verify <snapshot_dir> <ledger_seq> [--rpc URL]
//!
//! Reads every `*_batch_*.json` produced by snapshot.py (arrays of
//! {"index": hex32, "data": hexblob}), builds the State SHAMap
//! (leaf = SHA512Half(MLN\0 || data || key), the same formula verify_rocks
//! and the live validator use), and compares the computed root against the
//! pinned ledger's account_hash from RPC. This is phase 4's foundation
//! check: snapshot + SHAMap + hashing, end-to-end, byte-for-byte.

use serde_json::{json, Value};
use std::time::Instant;
use xrpl_core::types::Hash256;
use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_LEAF_NODE};
use xrpl_ledger::shamap::tree::{SHAMap, TreeType};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let dir = args.get(1).map(|s| s.as_str()).unwrap_or("xrpl_clean_snapshot");
    let seq: u64 = args.get(2).and_then(|s| s.parse().ok()).expect("ledger seq required");
    let rpc = args
        .iter()
        .position(|a| a == "--rpc")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
        .unwrap_or("http://localhost:5005")
        .to_string();

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let resp: Value = client
        .post(&rpc)
        .json(&json!({"method":"ledger","params":[{"ledger_index": seq}]}))
        .send()?
        .json()?;
    let expected = resp["result"]["ledger"]["account_hash"].as_str().unwrap_or("").to_uppercase();
    if expected.is_empty() {
        anyhow::bail!("no account_hash for ledger {seq} from {rpc}");
    }
    eprintln!("Ledger #{seq} expected account_hash {expected}");

    let mut files: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "json").unwrap_or(false))
        .collect();
    files.sort();
    eprintln!("{} batch files in {dir}", files.len());

    let start = Instant::now();
    let mut map = SHAMap::new(TreeType::State);
    let mut n: u64 = 0;
    for (i, path) in files.iter().enumerate() {
        let data = std::fs::read_to_string(path)?;
        let batch: Vec<Value> = serde_json::from_str(&data)?;
        for obj in &batch {
            let (Some(idx), Some(dh)) = (obj["index"].as_str(), obj["data"].as_str()) else {
                continue;
            };
            let (Ok(kb), Ok(blob)) = (hex::decode(idx), hex::decode(dh)) else { continue };
            let Ok(karr) = <[u8; 32]>::try_from(kb.as_slice()) else { continue };
            let key = Hash256(karr);
            let mut buf = Vec::with_capacity(blob.len() + 32);
            buf.extend_from_slice(&blob);
            buf.extend_from_slice(&key.0);
            let leaf = sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf);
            let _ = map.insert_hash_only(key, leaf);
            n += 1;
        }
        if (i + 1) % 500 == 0 {
            eprintln!("  {}/{} files, {n} objects, {:.0}s", i + 1, files.len(), start.elapsed().as_secs_f64());
        }
    }
    eprintln!("Inserted {n} objects in {:.0}s; computing root…", start.elapsed().as_secs_f64());
    let root = hex::encode_upper(map.root_hash().0);
    println!("computed {root}");
    println!("expected {expected}");
    println!("STATE-VERIFY {}", if root == expected { "MATCH" } else { "MISMATCH" });
    Ok(())
}
