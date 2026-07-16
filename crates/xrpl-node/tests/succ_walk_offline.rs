//! Offline tests for the RPC successor-walk logic (`xrpl_node::succ_walk`).
//!
//! No network, no ffi feature: these exercise the pure parsing and
//! candidate-selection helpers that back `RpcProvider::succ` /
//! `LayeredProvider::succ` on the standalone-RPC apply path. The semantic
//! contract under test is rippled's `ReadView::succ`: the OPEN interval
//! (key, last) — strictly greater than `key`, strictly less than `last`.

use serde_json::json;
use xrpl_node::succ_walk::{
    layered_succ, merge_succ_candidates, overlay_succ_candidate, page_exhausts_bound,
    parse_ledger_data_page, select_succ_candidate, walk_pages_for_succ, LedgerDataPage,
};

/// Pad a byte to a 32-byte key (matches the ffi_engine test helper).
fn k(byte: u8) -> [u8; 32] {
    let mut arr = [0u8; 32];
    arr[0] = byte;
    arr
}

fn hexk(byte: u8) -> String {
    hex::encode_upper(k(byte))
}

fn page_body(key_bytes: &[u8], marker: Option<&str>) -> serde_json::Value {
    let state: Vec<serde_json::Value> = key_bytes
        .iter()
        .map(|b| json!({"data": "11006F", "index": hexk(*b)}))
        .collect();
    let mut result = json!({"ledger_index": 103515366, "state": state, "status": "success"});
    if let Some(m) = marker {
        result["marker"] = json!(m);
    }
    json!({"result": result})
}

// ---- parse_ledger_data_page ----

#[test]
fn parse_page_extracts_keys_and_marker() {
    let body = page_body(&[0x20, 0x30], Some(&hexk(0x30)));
    let page = parse_ledger_data_page(&body).expect("should parse");
    assert_eq!(page.keys, vec![k(0x20), k(0x30)]);
    assert_eq!(page.marker, Some(json!(hexk(0x30))));
}

#[test]
fn parse_page_without_marker_is_final() {
    let body = page_body(&[0x20], None);
    let page = parse_ledger_data_page(&body).expect("should parse");
    assert!(page.marker.is_none());
}

#[test]
fn parse_page_surfaces_body_error() {
    let body = json!({"result": {"error": "lgrNotFound", "status": "error"}});
    assert_eq!(parse_ledger_data_page(&body).unwrap_err(), "lgrNotFound");
}

#[test]
fn parse_page_rejects_missing_state() {
    let body = json!({"result": {"status": "success"}});
    assert!(parse_ledger_data_page(&body).is_err());
}

#[test]
fn parse_page_rejects_malformed_index() {
    // Non-hex index — dropping it silently could yield a WRONG successor.
    let body = json!({"result": {"state": [{"data": "11", "index": "ZZZZ"}]}});
    assert!(parse_ledger_data_page(&body).is_err());
    // Wrong-length hex.
    let body = json!({"result": {"state": [{"data": "11", "index": "DEADBEEF"}]}});
    assert!(parse_ledger_data_page(&body).is_err());
    // Missing index field entirely.
    let body = json!({"result": {"state": [{"data": "11"}]}});
    assert!(parse_ledger_data_page(&body).is_err());
}

// ---- select_succ_candidate ----

#[test]
fn select_candidate_is_strictly_greater() {
    // The seed key itself comes back from marker resumption (">= position")
    // and must be filtered out.
    let keys = [k(0x20), k(0x30), k(0x40)];
    assert_eq!(select_succ_candidate(&keys, &k(0x20), None), Some(k(0x30)));
}

#[test]
fn select_candidate_upper_bound_is_exclusive() {
    let keys = [k(0x20), k(0x30), k(0x40)];
    // 0x30 == last must NOT be returned (open interval).
    assert_eq!(select_succ_candidate(&keys, &k(0x20), Some(&k(0x30))), None);
    // Strictly inside the bound is fine.
    assert_eq!(
        select_succ_candidate(&keys, &k(0x20), Some(&k(0x31))),
        Some(k(0x30))
    );
}

#[test]
fn select_candidate_picks_minimum_even_if_unordered() {
    let keys = [k(0x40), k(0x30)];
    assert_eq!(select_succ_candidate(&keys, &k(0x10), None), Some(k(0x30)));
}

#[test]
fn select_candidate_none_when_nothing_qualifies() {
    let keys = [k(0x10), k(0x20)];
    assert_eq!(select_succ_candidate(&keys, &k(0x20), None), None);
    assert_eq!(select_succ_candidate(&[], &k(0x20), None), None);
}

// ---- page_exhausts_bound ----

#[test]
fn exhausts_bound_once_page_reaches_last() {
    assert!(page_exhausts_bound(&[k(0x10), k(0x30)], Some(&k(0x30))));
    assert!(page_exhausts_bound(&[k(0x40)], Some(&k(0x30))));
    assert!(!page_exhausts_bound(&[k(0x10), k(0x20)], Some(&k(0x30))));
    assert!(!page_exhausts_bound(&[k(0x40)], None));
}

// ---- walk_pages_for_succ ----

#[test]
fn walk_starts_from_caller_chosen_marker_and_finds_on_first_page() {
    let mut seen_markers: Vec<serde_json::Value> = Vec::new();
    let got = walk_pages_for_succ(
        |marker| {
            seen_markers.push(marker.clone());
            Some(parse_ledger_data_page(&page_body(&[0x20, 0x30], Some(&hexk(0x30)))).unwrap())
        },
        &k(0x20),
        None,
        8,
        json!(hexk(0x20)),
    );
    assert_eq!(got, Some(k(0x30)));
    assert_eq!(
        seen_markers,
        vec![json!(hexk(0x20))],
        "first fetch must use the caller's start marker verbatim"
    );
}

#[test]
fn walk_resumed_below_key_still_filters_strictly_greater() {
    // Clio-style resumption: the caller seeds an EXISTING key below `key`
    // (e.g. the largest prefetched book dir <= key). Keys <= key coming
    // back from that earlier resume point must be filtered out.
    let got = walk_pages_for_succ(
        |_| Some(parse_ledger_data_page(&page_body(&[0x10, 0x20, 0x30], None)).unwrap()),
        &k(0x20),
        None,
        8,
        json!(hexk(0x10)),
    );
    assert_eq!(got, Some(k(0x30)));
}

#[test]
fn walk_follows_marker_to_next_page() {
    let mut calls = 0usize;
    let mut seen_markers: Vec<serde_json::Value> = Vec::new();
    let got = walk_pages_for_succ(
        |marker| {
            seen_markers.push(marker.clone());
            calls += 1;
            match calls {
                // Page 1: only the seed key itself (>= resumption) — no candidate.
                1 => Some(parse_ledger_data_page(&page_body(&[0x20], Some("AB42"))).unwrap()),
                // Page 2: the successor.
                _ => Some(parse_ledger_data_page(&page_body(&[0x50], None)).unwrap()),
            }
        },
        &k(0x20),
        None,
        8,
        json!(hexk(0x20)),
    );
    assert_eq!(got, Some(k(0x50)));
    assert_eq!(calls, 2);
    // Page 2 must have been requested with page 1's marker, verbatim.
    assert_eq!(seen_markers[1], json!("AB42"));
}

#[test]
fn walk_stops_at_exclusive_bound_without_following_marker() {
    let mut calls = 0usize;
    let got = walk_pages_for_succ(
        |_| {
            calls += 1;
            // Page holds exactly the bound key — excluded, and nothing
            // later can qualify, so the walk must stop here.
            Some(parse_ledger_data_page(&page_body(&[0x30], Some(&hexk(0x30)))).unwrap())
        },
        &k(0x20),
        Some(&k(0x30)),
        8,
        json!(hexk(0x20)),
    );
    assert_eq!(got, None);
    assert_eq!(calls, 1, "must not follow the marker past the bound");
}

#[test]
fn walk_ends_when_no_marker_and_no_candidate() {
    let got = walk_pages_for_succ(
        |_| Some(parse_ledger_data_page(&page_body(&[0x20], None)).unwrap()),
        &k(0x20),
        None,
        8,
        json!(hexk(0x20)),
    );
    assert_eq!(got, None);
}

#[test]
fn walk_aborts_on_fetch_failure() {
    let got = walk_pages_for_succ(|_| None, &k(0x20), None, 8, json!(hexk(0x20)));
    assert_eq!(got, None);
}

#[test]
fn walk_respects_max_pages_cap() {
    let mut calls = 0usize;
    let got = walk_pages_for_succ(
        |_| {
            calls += 1;
            // Endless empty pages that always hand back a marker.
            Some(LedgerDataPage { keys: vec![], marker: Some(json!("AA")) })
        },
        &k(0x20),
        None,
        3,
        json!(hexk(0x20)),
    );
    assert_eq!(got, None);
    assert_eq!(calls, 3);
}

// ---- overlay_succ_candidate / merge ----

#[test]
fn overlay_candidate_respects_open_interval() {
    let live = [k(0x20), k(0x25), k(0x40)];
    assert_eq!(overlay_succ_candidate(live.iter(), &k(0x20), None), Some(k(0x25)));
    // key itself excluded; bound exclusive.
    assert_eq!(overlay_succ_candidate(live.iter(), &k(0x20), Some(&k(0x25))), None);
    assert_eq!(
        overlay_succ_candidate(live.iter(), &k(0x20), Some(&k(0x26))),
        Some(k(0x25))
    );
    let empty: [[u8; 32]; 0] = [];
    assert_eq!(overlay_succ_candidate(empty.iter(), &k(0x20), None), None);
}

#[test]
fn merge_takes_minimum() {
    assert_eq!(merge_succ_candidates(Some(k(0x30)), Some(k(0x25))), Some(k(0x25)));
    assert_eq!(merge_succ_candidates(Some(k(0x25)), Some(k(0x30))), Some(k(0x25)));
    assert_eq!(merge_succ_candidates(Some(k(0x25)), None), Some(k(0x25)));
    assert_eq!(merge_succ_candidates(None, Some(k(0x25))), Some(k(0x25)));
    assert_eq!(merge_succ_candidates(None, None), None);
}

// ---- layered_succ ----

/// RPC view over a fixed ascending key set, honoring the open interval —
/// stands in for RpcProvider::succ in offline tests.
fn fake_rpc(keys: Vec<[u8; 32]>) -> impl FnMut(&[u8; 32], Option<&[u8; 32]>) -> Option<[u8; 32]> {
    move |key, last| select_succ_candidate(&keys, key, last)
}

#[test]
fn layered_skips_tombstoned_rpc_keys() {
    // RPC chain: 0x20 → 0x30 → 0x40; overlay tombstoned 0x30.
    let got = layered_succ(
        fake_rpc(vec![k(0x20), k(0x30), k(0x40)]),
        |c| *c == k(0x30),
        None,
        &k(0x20),
        None,
    );
    assert_eq!(got, Some(k(0x40)), "tombstoned 0x30 must not be resurrected");
}

#[test]
fn layered_overlay_insert_wins_when_smaller() {
    // RPC says 0x30; overlay inserted 0x25 this ledger.
    let got = layered_succ(
        fake_rpc(vec![k(0x20), k(0x30)]),
        |_| false,
        Some(k(0x25)),
        &k(0x20),
        None,
    );
    assert_eq!(got, Some(k(0x25)));
}

#[test]
fn layered_falls_back_to_overlay_when_rpc_chain_all_tombstoned() {
    let got = layered_succ(
        fake_rpc(vec![k(0x30), k(0x40)]),
        |_| true, // everything deleted this ledger
        Some(k(0x50)),
        &k(0x20),
        None,
    );
    assert_eq!(got, Some(k(0x50)));
}

#[test]
fn layered_bound_applies_to_both_streams() {
    // RPC candidate 0x30 and overlay candidate 0x35 both >= last=0x30.
    let got = layered_succ(
        fake_rpc(vec![k(0x30)]),
        |_| false,
        overlay_succ_candidate([k(0x35)].iter(), &k(0x20), Some(&k(0x30))),
        &k(0x20),
        Some(&k(0x30)),
    );
    assert_eq!(got, None);
}

#[test]
fn layered_none_when_both_views_empty() {
    let got = layered_succ(fake_rpc(vec![]), |_| false, None, &k(0x20), None);
    assert_eq!(got, None);
}

/// Offline tests for the no-op-Modified filter on the mutation-divergence
/// comparison path (`ffi_engine::{capture_pre_state_for_modified,
/// build_ours_mutation_set}`) — the same standalone-RPC apply lane the succ
/// tests above cover, hence hosted in this (always-run, offline) binary.
///
/// rippled's meta-builder skips entries whose post bytes equal the pre bytes
/// (ApplyStateTable.cpp:155 — `*curNode == *origNode`) while libxrpl still
/// calls rawReplace, so our MutationCollector records a Modified whose bytes
/// equal the pre-state. The compared set must drop exactly those — and ONLY
/// those: a Modified whose pre-state can't be resolved is kept (a
/// false-positive divergence beats a silently wrong filter), and
/// Created/Deleted are never no-ops. Pre-state resolution mirrors the SAME
/// layered chain the apply used: overlay slot first (a tombstone never falls
/// through — the base still holds the stale pre-ledger version), then the
/// base lookup (RocksDB snapshot on the DB path, era-pinned RPC fallback on
/// the standalone path).
#[cfg(feature = "ffi")]
mod noop_modified_filter {
    use std::collections::HashMap;
    use xrpl_node::ffi_engine::{build_ours_mutation_set, capture_pre_state_for_modified};

    fn sle(kind: xrpl_ffi::MutationKind, key_byte: u8, data: &[u8]) -> xrpl_ffi::SleMutation {
        xrpl_ffi::SleMutation {
            key: [key_byte; 32],
            kind,
            data: data.to_vec(),
        }
    }

    /// F1 core: a Modified whose post bytes equal the captured pre bytes is
    /// a no-op — rippled's meta omits it, so the compared set must drop it.
    /// Changed Modifieds and Created/Deleted always stay.
    #[test]
    fn noop_modified_dropped_from_comparison_set() {
        let muts = vec![
            sle(xrpl_ffi::MutationKind::Modified, 0xAA, b"same-bytes"),
            sle(xrpl_ffi::MutationKind::Modified, 0xBB, b"post-bytes"),
            sle(xrpl_ffi::MutationKind::Created, 0xCC, b"same-bytes"),
            sle(xrpl_ffi::MutationKind::Deleted, 0xDD, b""),
        ];
        let mut pre = HashMap::new();
        pre.insert([0xAA; 32], b"same-bytes".to_vec()); // no-op
        pre.insert([0xBB; 32], b"pre-bytes".to_vec()); // real change
        let set = build_ours_mutation_set(&muts, &pre);
        assert!(
            !set.contains(&(hex::encode_upper([0xAA; 32]), 1u8)),
            "no-op Modified must be dropped"
        );
        assert!(
            set.contains(&(hex::encode_upper([0xBB; 32]), 1u8)),
            "changed Modified must stay"
        );
        assert!(
            set.contains(&(hex::encode_upper([0xCC; 32]), 0u8)),
            "Created is never a no-op"
        );
        assert!(
            set.contains(&(hex::encode_upper([0xDD; 32]), 2u8)),
            "Deleted is never a no-op"
        );
        assert_eq!(set.len(), 3);
    }

    /// Invariant guard: a Modified with NO captured pre-state must be kept
    /// (false-positive divergence beats a silently wrong filter).
    #[test]
    fn modified_without_prestate_kept_in_comparison_set() {
        let muts = vec![sle(xrpl_ffi::MutationKind::Modified, 0xEE, b"bytes")];
        let pre = HashMap::new();
        let set = build_ours_mutation_set(&muts, &pre);
        assert!(set.contains(&(hex::encode_upper([0xEE; 32]), 1u8)));
    }

    /// A Created whose bytes coincidentally equal some captured pre-state
    /// must never be filtered — only Modified can be a no-op.
    #[test]
    fn created_with_matching_prestate_still_kept() {
        let muts = vec![sle(xrpl_ffi::MutationKind::Created, 0xAF, b"same")];
        let mut pre = HashMap::new();
        pre.insert([0xAF; 32], b"same".to_vec());
        let set = build_ours_mutation_set(&muts, &pre);
        assert!(set.contains(&(hex::encode_upper([0xAF; 32]), 0u8)));
    }

    /// F1 regression (RPC path): a key first touched by THIS tx has no
    /// overlay slot — pre-state must come from the base lookup (era-pinned
    /// RPC on the standalone path). Before the fix the base was `None`
    /// there, the no-op survived filtering, and every affected Payment
    /// reported the "+1 ModifiedNode" phantom (ours=4 net=3).
    #[test]
    fn capture_prestate_consults_base_on_overlay_miss() {
        let muts = vec![sle(xrpl_ffi::MutationKind::Modified, 0xAA, b"same-bytes")];
        let overlay: HashMap<[u8; 32], Option<Vec<u8>>> = HashMap::new();
        let pre = capture_pre_state_for_modified(&muts, &overlay, |key| {
            assert_eq!(key, &[0xAA; 32]);
            Some(b"same-bytes".to_vec())
        });
        assert_eq!(
            pre.get(&[0xAA; 32]).map(Vec::as_slice),
            Some(&b"same-bytes"[..])
        );
        // End-to-end: with the captured pre-state the no-op drops out.
        let set = build_ours_mutation_set(&muts, &pre);
        assert!(
            set.is_empty(),
            "no-op Modified must vanish from the compared set"
        );
    }

    /// An overlay hit (an earlier tx in this ledger wrote the key) shadows
    /// the base — the base lookup must not run at all.
    #[test]
    fn capture_prestate_overlay_hit_shadows_base() {
        let muts = vec![sle(xrpl_ffi::MutationKind::Modified, 0xAB, b"post")];
        let mut overlay: HashMap<[u8; 32], Option<Vec<u8>>> = HashMap::new();
        overlay.insert([0xAB; 32], Some(b"overlay-pre".to_vec()));
        let pre = capture_pre_state_for_modified(&muts, &overlay, |_| {
            panic!("base must not be consulted on overlay hit")
        });
        assert_eq!(
            pre.get(&[0xAB; 32]).map(Vec::as_slice),
            Some(&b"overlay-pre"[..])
        );
    }

    /// A tombstone (key deleted earlier this ledger) must NOT fall through
    /// to the base — the base still holds the stale pre-ledger version. The
    /// key stays uncaptured, so the comparison keeps the node.
    #[test]
    fn capture_prestate_tombstone_never_falls_through() {
        let muts = vec![sle(xrpl_ffi::MutationKind::Modified, 0xAC, b"post")];
        let mut overlay: HashMap<[u8; 32], Option<Vec<u8>>> = HashMap::new();
        overlay.insert([0xAC; 32], None); // tombstone
        let pre = capture_pre_state_for_modified(&muts, &overlay, |_| {
            panic!("base must not be consulted past a tombstone")
        });
        assert!(pre.is_empty());
    }

    /// Created/Deleted mutations are never no-ops — capture must skip them
    /// without touching the base (no wasted era-pinned RPC reads).
    #[test]
    fn capture_prestate_skips_created_and_deleted() {
        let muts = vec![
            sle(xrpl_ffi::MutationKind::Created, 0xAD, b"new"),
            sle(xrpl_ffi::MutationKind::Deleted, 0xAE, b""),
        ];
        let overlay: HashMap<[u8; 32], Option<Vec<u8>>> = HashMap::new();
        let pre = capture_pre_state_for_modified(&muts, &overlay, |_| {
            panic!("base must not be consulted for Created/Deleted")
        });
        assert!(pre.is_empty());
    }

    /// The same key Modified twice in one tx's mutation list must resolve
    /// its pre-state exactly once (first occurrence wins) — one base read,
    /// not two.
    #[test]
    fn capture_prestate_dedupes_repeated_keys() {
        let muts = vec![
            sle(xrpl_ffi::MutationKind::Modified, 0xBA, b"post-1"),
            sle(xrpl_ffi::MutationKind::Modified, 0xBA, b"post-2"),
        ];
        let overlay: HashMap<[u8; 32], Option<Vec<u8>>> = HashMap::new();
        let calls = std::cell::Cell::new(0u32);
        let pre = capture_pre_state_for_modified(&muts, &overlay, |_| {
            calls.set(calls.get() + 1);
            Some(b"pre".to_vec())
        });
        assert_eq!(calls.get(), 1, "base consulted exactly once per key");
        assert_eq!(pre.len(), 1);
    }
}
