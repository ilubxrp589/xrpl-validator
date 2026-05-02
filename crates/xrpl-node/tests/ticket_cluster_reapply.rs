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
        None, // silent_divergence_log
        None, // expected_outcomes
        None, // mutation_divergence_log
        None, // expected_mutations
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

/// Mainnet AffectedNodes for BF6C928F — fetched from s2.ripple.com `tx` method
/// and recorded here for regression comparison. libxrpl's apply of this tx
/// should emit a mutation for each of these 10 keys when given the correct
/// pre-apply state.
///
/// Source-of-truth listing (mainnet metadata from ledger 103515367):
/// - ModifiedNode AccountRoot  2B7EE4494372DAAE... ← rBtVeRQ8 (the cascade trigger)
/// - ModifiedNode AccountRoot  A90EA9A8D130DFF4...
/// - ModifiedNode AccountRoot  E476CB526D88DEC7...
/// - ModifiedNode RippleState  BAEB6B6C84253C7B...
/// - ModifiedNode RippleState  E40A1558DC7638FF...
/// - ModifiedNode RippleState  F5297CFDE8F496C0...
/// - ModifiedNode DirectoryNode AC9AA39051CD0F01...
/// - DeletedNode  DirectoryNode CFEC8953D22B1B50...
/// - DeletedNode  Offer         982C0B255D33231A...
/// - ModifiedNode Offer         F5D5D29A6A3CFA7A...
const BF6C928F_MAINNET_MUTATION_KEYS: &[&str] = &[
    "2B7EE4494372DAAEE9226010B426EFDF3B563DFA7257046F73AE3F31D142D1A9", // ModifiedNode AccountRoot (rBtVeRQ8)
    "982C0B255D33231AB1FE9B83C5CE42CEDDF037F59E1414348E8520795AD56332", // DeletedNode  Offer (the one BF6C928F crosses)
    "A90EA9A8D130DFF4AC38B12F56C46E93A48B166928D8D279568CCC6D91FF6F35", // ModifiedNode AccountRoot (rwnJpjMn submitter)
    "AC9AA39051CD0F01B3C72DFCEB90E2D659E42C1E96B538B9D9B20191D16F8A63", // ModifiedNode DirectoryNode
    "BAEB6B6C84253C7BB682E879329B11EC47480724DD4A26246848D82408071314", // ModifiedNode RippleState
    "CFEC8953D22B1B50F20F46ECBCD51990A26BDB71951ED8265A1AA92902C9A600", // DeletedNode  DirectoryNode
    "E40A1558DC7638FF795B2D8D10C59F8624C52988A254D51EA4A92700F14E4F68", // ModifiedNode RippleState
    "E476CB526D88DEC7742BBB3937D20F910A7A74D4095EA8FCA34EA315C360E7BD", // ModifiedNode AccountRoot
    "F5297CFDE8F496C0EA6EFC463DF508674DA4EC39617AC97CE83AB7805B1CEB65", // ModifiedNode RippleState
    "F5D5D29A6A3CFA7A5E9EDFDEF22620961F680DBF2BF30D1F515253754C9A0DD9", // ModifiedNode Offer
];

/// Isolation test: run BF6C928F (the cross-account OfferCreate at tx_index=19)
/// standalone via `apply_ledger_in_order` with a 1-tx vec. This tells us whether
/// the 4-vs-10 mutation mismatch comes from:
///
/// - **Pre-ledger RPC state** — LayeredProvider → RpcProvider returns offer/book
///   state at ledger_seq-1 that makes libxrpl cross DIFFERENT offers than
///   mainnet did. If standalone also produces 4 mutations → this is the cause,
///   and the fix is in how we query rippled for state.
///
/// - **Earlier tx interference** — one or more of mainnet's txs 0-18 mutates
///   the offer book in a way that affects what BF6C928F sees. If standalone
///   produces 10 mutations → this is the cause, and the fix is in how we
///   thread those earlier mutations.
#[test]
#[ignore = "network-dependent: fetches pre-ledger SLE state from s2.ripple.com"]
fn bf6c928f_isolated_mutations_match_mainnet() {
    let all_blobs = parse_ledger_blobs(LEDGER_BLOBS_TSV);
    assert_eq!(all_blobs.len(), 67, "expected 67 tx blobs in the data file");
    // Pick index 19 — BF6C928F OfferCreate from rwnJpjMn
    let bf6c928f = all_blobs.into_iter().nth(19).expect("idx 19 must exist");
    let txs = vec![bf6c928f];

    println!("Fetching mainnet amendments...");
    let amendments = fetch_mainnet_amendments(RPC_URL);
    println!("Loaded {} amendments", amendments.len());

    let stats = new_stats();
    let rpc_urls = vec![RPC_URL.to_string()];
    let parent_hash = hex_to_array32(PARENT_HASH);

    println!("Applying BF6C928F standalone (no prior tx state threaded)...");
    let overlay = apply_ledger_in_order(
        &stats,
        &txs,
        LEDGER_SEQ,
        &rpc_urls,
        &amendments,
        parent_hash,
        PARENT_CLOSE_TIME,
        TOTAL_DROPS,
        None,
        None,
        None,
        None,
        None,
        None,
    );

    let s = stats.lock();
    println!();
    println!("=== BF6C928F standalone result ===");
    println!("  attempted:  {}", s.live_apply_attempted);
    println!("  ok:         {}", s.live_apply_ok);
    println!("  claimed:    {}", s.live_apply_claimed);
    println!("  diverged:   {}", s.live_apply_diverged);
    println!("  overlay size: {}", overlay.len());
    println!();
    println!("=== Overlay contents (all mutations) ===");
    let mut overlay_keys: Vec<String> = overlay
        .iter()
        .map(|(k, v)| format!("{} ({:?})", hex::encode_upper(k), v.as_ref().map(|b| b.len())))
        .collect();
    overlay_keys.sort();
    for k in &overlay_keys {
        let hex_key = &k[..64];
        let mainnet_has = BF6C928F_MAINNET_MUTATION_KEYS.iter().any(|m| *m == hex_key);
        let marker = if mainnet_has { "✓" } else { "✗ NOT IN MAINNET" };
        println!("  [{marker}] {k}");
    }
    println!();
    println!("=== Mainnet mutation keys NOT in our overlay ===");
    for mainnet_key in BF6C928F_MAINNET_MUTATION_KEYS {
        let found = overlay.keys().any(|k| hex::encode_upper(k) == *mainnet_key);
        if !found {
            println!("  ✗ MISSING {mainnet_key}");
        }
    }
    println!();

    assert_eq!(s.live_apply_diverged, 0, "BF6C928F should succeed in isolation");
    assert_eq!(s.live_apply_ok, 1, "BF6C928F should return tesSUCCESS");
    assert_eq!(
        overlay.len(),
        10,
        "BF6C928F should emit 10 mutations to match mainnet AffectedNodes. \
         If this assertion fails with 4, we've pinned the bug to the pre-ledger \
         state (RpcProvider). If it passes (10 mutations), the bug is in how \
         earlier txs in the full ledger replay are mutating state that \
         BF6C928F subsequently reads."
    );
}
