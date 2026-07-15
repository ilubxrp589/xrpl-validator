//! Regression test for the 2026-07-15 #105612802 false-divergence burst.
//!
//! ## The incident
//!
//! During a .39 RPC flap, `process_ledger` committed #105612802's write batch
//! but could not verify it (`fetch_account_hash` had failed → empty), fell
//! through, and returned `false` — so ws-sync held position and re-drove the
//! ledger against a DB that ALREADY contained its effects. The FFI verify
//! lane's "pre-ledger" snapshot was therefore post-ledger: all 53 txs
//! replayed onto their own effects and produced 43 tefPAST_SEQ + 10
//! tefBAD_LEDGER — 53 false entries in divergences.jsonl and a false
//! `diverged=53` on the dashboard. The canonical lane self-healed (idempotent
//! re-write → same root → MATCH), so ONLY the oracle lane lied.
//!
//! ## What this test checks
//!
//! The era sentinel in `apply_ledger_in_order`: replaying a ledger whose
//! pre-state is era-wrong (here: RPC provider pinned at L instead of L-1,
//! by passing `ledger_seq = L+1`) must SKIP the whole-ledger verify —
//! `verify_skipped_era == 1`, zero txs attempted, zero divergences recorded —
//! instead of logging a false tef* storm.
//!
//! Uses the same bundled fixture as `ticket_cluster_reapply` (all 67 tx
//! blobs of mainnet ledger 103515367) against s2.ripple.com full history.
//!
//! Run explicitly with:
//! `cargo test --features ffi --test era_sentinel_reapply -- --ignored --nocapture`

#![cfg(feature = "ffi")]

use xrpl_node::ffi_engine::{
    apply_ledger_in_order, fetch_mainnet_amendments, new_stats, scan_sequence_in_tx,
};

/// All 67 tx blobs from ledger 103515367, sorted by TransactionIndex.
const LEDGER_BLOBS_TSV: &str = include_str!("data/l103515367_blobs.txt");

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

/// Pure unit test — no network. The sentinel's tx-Sequence scanner walks the
/// canonical UInt-field prefix and bails (None) on anything unexpected.
#[test]
fn scan_sequence_in_tx_walks_uint_prefix() {
    // TransactionType(0x12) then Flags(0x22) then Sequence(0x24) = 0x056A5F59
    let blob = [
        0x12, 0x00, 0x00, // TransactionType = Payment
        0x22, 0x80, 0x00, 0x00, 0x00, // Flags
        0x24, 0x05, 0x6A, 0x5F, 0x59, // Sequence
    ];
    assert_eq!(scan_sequence_in_tx(&blob), Some(0x056A_5F59));

    // NetworkID + SourceTag variants also precede Sequence in canonical order.
    let blob2 = [
        0x12, 0x00, 0x07, // TransactionType = OfferCreate
        0x21, 0x00, 0x00, 0x00, 0x01, // NetworkID
        0x23, 0x00, 0x00, 0x00, 0x2A, // SourceTag
        0x24, 0x00, 0x00, 0x00, 0x07, // Sequence = 7
    ];
    assert_eq!(scan_sequence_in_tx(&blob2), Some(7));

    // Ticket-based tx: Sequence field present but zero.
    let ticket = [0x12, 0x00, 0x00, 0x24, 0x00, 0x00, 0x00, 0x00];
    assert_eq!(scan_sequence_in_tx(&ticket), Some(0));

    // Unexpected leading field → None (sentinel treats as inconclusive).
    assert_eq!(scan_sequence_in_tx(&[0x73, 0x21, 0x00]), None);

    // Truncated Sequence payload → None.
    assert_eq!(scan_sequence_in_tx(&[0x12, 0x00, 0x00, 0x24, 0x00]), None);

    // Empty blob → None.
    assert_eq!(scan_sequence_in_tx(&[]), None);
}

/// The incident in a test tube: pre-state pinned at L (post-state for L's
/// txs) — the sentinel must skip the verify entirely and record nothing in
/// the divergence counters.
#[test]
#[ignore = "network-dependent: fetches SLE state from s2.ripple.com"]
fn replay_on_post_state_skips_verify_instead_of_logging_false_divergences() {
    let txs: Vec<Vec<u8>> = parse_ledger_blobs(LEDGER_BLOBS_TSV);
    assert_eq!(txs.len(), 67, "expected 67 txs in ledger 103515367");

    println!("Fetching active mainnet amendments from {RPC_URL}...");
    let amendments = fetch_mainnet_amendments(RPC_URL);
    println!("Loaded {} amendments", amendments.len());

    let stats = new_stats();
    let rpc_urls = vec![RPC_URL.to_string()];
    let parent_hash = hex_to_array32(PARENT_HASH);

    // ledger_seq = L+1 pins the internal RpcProvider at L — the txs' own
    // post-state — exactly the wrong-era pre-state a re-driven ledger saw
    // on 2026-07-15.
    println!("Replaying {} txs of #{LEDGER_SEQ} against POST-state (pin @L)...", txs.len());
    let overlay = apply_ledger_in_order(
        &stats,
        &txs,
        LEDGER_SEQ + 1,
        &rpc_urls,
        &amendments,
        parent_hash,
        PARENT_CLOSE_TIME,
        TOTAL_DROPS,
        None, // divergence_log
        None, // db_snapshot → LayeredProvider path (pure RPC)
        None, // silent_divergence_log
        None, // expected_outcomes
        None, // mutation_divergence_log
        None, // expected_mutations
    );

    let s = stats.lock();
    println!();
    println!("=== era-mismatch replay result ===");
    println!("  verify_skipped_era:      {}", s.verify_skipped_era);
    println!("  verify_skipped_era_last: {}", s.verify_skipped_era_last);
    println!("  attempted:               {}", s.live_apply_attempted);
    println!("  diverged:                {}", s.live_apply_diverged);
    if !s.diverged_tx_samples.is_empty() {
        println!("  false divergences that would have been logged:");
        for sample in &s.diverged_tx_samples {
            println!("    {sample}");
        }
    }

    assert_eq!(
        s.verify_skipped_era, 1,
        "era sentinel must fire exactly once for the whole ledger"
    );
    assert_eq!(
        s.live_apply_diverged, 0,
        "an era-mismatched replay must record ZERO divergences (2026-07-15 bug: 53 false tef* entries)"
    );
    assert_eq!(
        s.live_apply_attempted, 0,
        "no tx may be applied once the era sentinel trips"
    );
    assert!(overlay.is_empty(), "skipped verify must return an empty overlay");
}
