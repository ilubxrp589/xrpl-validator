//! Integration test — reproduce the TicketCreate cascade from cluster #103515367
//! by running the EXACT production code path (`apply_ledger_in_order` with
//! `LayeredProvider` backed by a real rippled RPC endpoint).
//!
//! ## Setup
//!
//! The 4 txs that form the divergence cluster in ledger 103515367 (in canonical
//! TransactionIndex order from mainnet metadata):
//!
//! | Idx | Hash                 | Type         | Seq      | Mainnet | FFI on m3060   |
//! |-----|----------------------|--------------|----------|---------|----------------|
//! | 45  | C873F269F2355C34     | TicketCreate | 102165188| tesSUCCESS | tesSUCCESS |
//! | 46  | 039CC1F535370BD3     | TicketCreate | 102165190| tesSUCCESS | **terPRE_SEQ** |
//! | 47  | 4CF597F5419D8D08     | TicketCreate | 102165192| tesSUCCESS | **tefPAST_SEQ** |
//! | 48  | 812EB831CFD6761F     | OfferCreate  | 102165194| tesSUCCESS | **tefPAST_SEQ** |
//!
//! In production we see 1 `terPRE_SEQ` + 2 `tefPAST_SEQ` = 3 divergences here.
//!
//! ## What this test checks
//!
//! `apply_ledger_in_order` uses `LayeredProvider(overlay, rpc_fallback)` when
//! no `OwnedSnapshot` is supplied. `rpc_fallback` points at s2.ripple.com — a
//! full-history public mainnet node — so the provider fetches pre-ledger SLE
//! state on demand. This eliminates any dependency on our local state.rocks
//! and exercises the SAME logic that runs on m3060:
//!
//! - apply_ledger_in_order's threading loop
//! - LayeredProvider's overlay-first / RPC-fallback reads
//! - LayeredProvider's arena that returns `&[u8]` slices to arena-held Vecs
//! - libxrpl's apply path via the xrpl_ffi trampoline
//! - MutationCollector → xrpl_ffi mutation extraction → overlay write-back
//!
//! ## Expected result
//!
//! `stats.live_apply_diverged == 0` — all 4 txs must match rippled's outcomes.
//!
//! **If the test fails with 3 divergences:** we've reproduced the exact
//! production bug. Iterate on the fix against this test.
//!
//! **If the test passes (0 divergences):** the bug is NOT in LayeredProvider's
//! path. Candidates then are:
//! - OverlayedDbProvider specifically (used when db_snapshot is Some)
//! - Cross-tx interference from the 45 txs that run BEFORE C873F269 in the
//!   real ledger (our test only applies the 4-tx subset from rBtVeRQ8)
//! - An incremental rippled state transition between when our harness reads
//!   and when m3060 reads
//!
//! Marked `#[ignore]` — network-dependent (~20 RPC calls to s2.ripple.com).
//! Run explicitly with: `cargo test --features ffi --test ticket_cluster_reapply -- --ignored --nocapture`

#![cfg(feature = "ffi")]

use xrpl_node::ffi_engine::{apply_ledger_in_order, fetch_mainnet_amendments, new_stats};

/// All 67 tx blobs from ledger 103515367, sorted by TransactionIndex.
/// One line per tx in format: `<index>\t<hex_blob>`. Generated via
/// `scripts/fetch_cluster_txs.sh`, captured from s2.ripple.com at the
/// `ledger` RPC with `binary=true, expand=true`.
const LEDGER_BLOBS_TSV: &str = include_str!("data/l103515367_blobs.txt");

/// Ledger 103515367 header, fetched from mainnet via `ledger` command.
const PARENT_HASH: &str = "2AD40E1B3F716EE3ED71A9AC8B8518F789FFDB5237E6C824872718AB73579B95";
const PARENT_CLOSE_TIME: u32 = 829332722;
const TOTAL_DROPS: u64 = 99_985_676_152_971_157;
const LEDGER_SEQ: u32 = 103_515_367;

const RPC_URL: &str = "https://s2.ripple.com:51234";

fn hex_to_bytes(s: &str) -> Vec<u8> {
    hex::decode(s).expect("hex decode")
}

fn hex_to_array32(s: &str) -> [u8; 32] {
    let v = hex_to_bytes(s);
    assert_eq!(v.len(), 32);
    let mut a = [0u8; 32];
    a.copy_from_slice(&v);
    a
}

fn parse_ledger_blobs(tsv: &str) -> Vec<Vec<u8>> {
    let mut rows: Vec<(u32, Vec<u8>)> = tsv
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|l| {
            let mut parts = l.splitn(2, '\t');
            let idx: u32 = parts.next()?.parse().ok()?;
            let blob = hex_to_bytes(parts.next()?);
            Some((idx, blob))
        })
        .collect();
    rows.sort_by_key(|(i, _)| *i);
    rows.into_iter().map(|(_, b)| b).collect()
}

#[test]
#[ignore = "network-dependent: fetches pre-ledger SLE state from s2.ripple.com (~100 RPC calls)"]
fn cluster_103515367_reapplies_with_zero_divergences() {
    let txs: Vec<Vec<u8>> = parse_ledger_blobs(LEDGER_BLOBS_TSV);
    assert_eq!(txs.len(), 67, "expected 67 txs in ledger 103515367");

    println!("Fetching active mainnet amendments from {RPC_URL}...");
    let amendments = fetch_mainnet_amendments(RPC_URL);
    println!("Loaded {} amendments", amendments.len());

    let stats = new_stats();
    let rpc_urls = vec![RPC_URL.to_string()];
    let parent_hash = hex_to_array32(PARENT_HASH);

    println!("Applying {} txs in canonical order...", txs.len());
    let overlay = apply_ledger_in_order(
        &stats,
        &txs,
        LEDGER_SEQ,
        &rpc_urls,
        &amendments,
        parent_hash,
        PARENT_CLOSE_TIME,
        TOTAL_DROPS,
        None, // divergence_log
        None, // db_snapshot → forces LayeredProvider path
    );

    let s = stats.lock();
    println!();
    println!("=== apply_ledger_in_order result ===");
    println!("  attempted:  {}", s.live_apply_attempted);
    println!("  ok:         {}", s.live_apply_ok);
    println!("  claimed:    {}", s.live_apply_claimed);
    println!("  diverged:   {}", s.live_apply_diverged);
    println!("  overlay size: {}", overlay.len());
    if !s.diverged_tx_samples.is_empty() {
        println!("  samples:");
        for sample in &s.diverged_tx_samples {
            println!("    {sample}");
        }
    }
    if !s.live_diverged_by_type.is_empty() {
        println!("  by type:");
        for (k, v) in &s.live_diverged_by_type {
            println!("    {k}: {v}");
        }
    }

    assert_eq!(
        s.live_apply_attempted, 67,
        "all 67 txs should have been attempted"
    );
    assert_eq!(
        s.live_apply_diverged, 0,
        "Production (m3060) sees 3 divergences from rBtVeRQ8's TicketCreate cluster. \
         Reproducing them here means we have a local, deterministic handle on the bug. \
         Zero divergences means the test matches mainnet exactly."
    );
}
