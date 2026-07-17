//! parity_probe — replay a fixture ledger at runtime and report mainnet parity.
//!
//! The scout loop's workhorse: unlike the compiled-in `backlog_reapply` gates,
//! this binary loads fixture files by PATH, so freshly-fetched ledgers can be
//! probed without a rebuild.
//!
//! Usage:
//!   parity_probe <lNNN_blobs.txt> <lNNN_expected.json> [--rpc URL]
//!
//! Prints the standard parity block (attempted / diverged / silent diverged /
//! mutation diverged + samples). Exit codes: 0 full parity, 1 divergence,
//! 2 usage/fixture error. SLE pre-state is fetched from the RPC pinned at
//! seq-1 (LayeredProvider path), amendments from the same endpoint.

#![cfg(feature = "ffi")]

use std::collections::HashMap;

use xrpl_node::ffi_engine::{apply_ledger_in_order, fetch_mainnet_amendments, new_stats};

const DEFAULT_RPC: &str = "https://s2.ripple.com:51234";

fn hex_to_array32(s: &str) -> Option<[u8; 32]> {
    let v = hex::decode(s).ok()?;
    if v.len() != 32 {
        return None;
    }
    let mut a = [0u8; 32];
    a.copy_from_slice(&v);
    Some(a)
}

fn parse_blobs(tsv: &str) -> Vec<Vec<u8>> {
    let mut rows: Vec<(u32, Vec<u8>)> = tsv
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|l| {
            let mut p = l.splitn(2, '\t');
            let idx: u32 = p.next()?.parse().ok()?;
            Some((idx, hex::decode(p.next()?).ok()?))
        })
        .collect();
    rows.sort_by_key(|(i, _)| *i);
    rows.into_iter().map(|(_, b)| b).collect()
}

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: parity_probe <blobs.txt> <expected.json> [--rpc URL]");
        return 2;
    }
    let rpc_url = args
        .iter()
        .position(|a| a == "--rpc")
        .and_then(|i| args.get(i + 1).cloned())
        .unwrap_or_else(|| DEFAULT_RPC.to_string());

    let blobs_tsv = match std::fs::read_to_string(&args[1]) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("blobs file {}: {e}", args[1]);
            return 2;
        }
    };
    let expected_json = match std::fs::read_to_string(&args[2]) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("expected file {}: {e}", args[2]);
            return 2;
        }
    };
    let exp: serde_json::Value = match serde_json::from_str(&expected_json) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("expected json parse: {e}");
            return 2;
        }
    };

    let hdr = &exp["header"];
    let (Some(seq), Some(pct), Some(drops)) = (
        hdr["ledger_seq"].as_u64(),
        hdr["parent_close_time"].as_u64(),
        hdr["total_drops"].as_u64(),
    ) else {
        eprintln!("expected json missing header fields");
        return 2;
    };
    let seq = seq as u32;
    let Some(parent_hash) = hdr["parent_hash"].as_str().and_then(hex_to_array32) else {
        eprintln!("expected json bad parent_hash");
        return 2;
    };

    let mut expected_outcomes: HashMap<String, String> = HashMap::new();
    let mut expected_mutations: HashMap<String, Vec<(String, u8)>> = HashMap::new();
    let Some(txmap) = exp["txs"].as_object() else {
        eprintln!("expected json missing txs");
        return 2;
    };
    for (hash, v) in txmap {
        if let Some(ter) = v["ter"].as_str() {
            expected_outcomes.insert(hash.clone(), ter.to_string());
        }
        let nodes: Vec<(String, u8)> = v["nodes"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|n| Some((n[0].as_str()?.to_string(), n[1].as_u64()? as u8)))
                    .collect()
            })
            .unwrap_or_default();
        if !nodes.is_empty() {
            expected_mutations.insert(hash.clone(), nodes);
        }
    }

    let txs = parse_blobs(&blobs_tsv);
    if txs.is_empty() {
        eprintln!("fixture has no txs");
        return 2;
    }
    println!("Fetching mainnet amendments from {rpc_url}…");
    let amendments = fetch_mainnet_amendments(&rpc_url);
    println!("Probing {} txs of #{seq} for mainnet parity…", txs.len());

    let stats = new_stats();
    let rpc_urls = vec![rpc_url];
    let _overlay = apply_ledger_in_order(
        &stats,
        &txs,
        seq,
        &rpc_urls,
        &amendments,
        parent_hash,
        pct as u32,
        drops,
        None,
        None,
        None,
        Some(&expected_outcomes),
        None,
        Some(&expected_mutations),
    );

    let s = stats.lock();
    println!();
    println!("=== #{seq} parity result ===");
    println!("  attempted:          {}", s.live_apply_attempted);
    println!("  ok/claimed:         {}/{}", s.live_apply_ok, s.live_apply_claimed);
    println!("  diverged:           {}", s.live_apply_diverged);
    println!("  silent diverged:    {}", s.live_apply_silent_diverged);
    println!("  mutation diverged:  {}", s.live_apply_mutation_diverged);
    for x in &s.diverged_tx_samples {
        println!("    [ter ] {x}");
    }
    for x in &s.silent_diverged_samples {
        println!("    [tersilent] {x}");
    }
    for x in &s.mutation_diverged_samples {
        println!("    [muta] {x}");
    }

    let attempted_all = s.live_apply_attempted as usize == txs.len();
    let clean = s.live_apply_diverged == 0
        && s.live_apply_silent_diverged == 0
        && s.live_apply_mutation_diverged == 0;
    if !attempted_all {
        println!("PROBE: INCOMPLETE (attempted {} of {})", s.live_apply_attempted, txs.len());
        return 2;
    }
    if clean {
        println!("PROBE: CLEAN");
        0
    } else {
        println!("PROBE: DIVERGENT");
        1
    }
}
