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

/// Offline tests for the NFTokenPage prefetch that backs `RpcProvider::succ`
/// on the standalone-RPC path (F2). rippled locates a token's page with
/// `succ(nftpage(min(owner), tokenID), nftpage_max(owner).next())`: the start
/// key is derived from the token ID (not a real object, so Clio rejects it as
/// a ledger_data marker) and the bound is EXCLUSIVE and can equal the next
/// account's first page.
mod nft_page_prefetch {
    use serde_json::json;
    use std::collections::BTreeSet;
    use xrpl_node::nft_pages::{
        collect_owner_pages, interval_within_owner, nftpage_max, nftpage_max_next, nftpage_min,
        owner_of_page_key, parse_account_objects_pages, parse_nft_page_owners, succ_from_pages,
        NftSuccAnswer, OwnerNftPages,
    };

    /// An owner AccountID whose 20 bytes are all `byte`.
    fn owner(byte: u8) -> [u8; 20] {
        [byte; 20]
    }

    /// A page key in `owner`'s range with page-tag low byte `tag`.
    fn page(o: &[u8; 20], tag: u8) -> [u8; 32] {
        let mut k = nftpage_min(o);
        k[31] = tag;
        k
    }

    fn pages_of(o: &[u8; 20], tags: &[u8], complete: bool) -> OwnerNftPages {
        OwnerNftPages {
            owner: *o,
            pages: tags.iter().map(|t| page(o, *t)).collect::<BTreeSet<_>>(),
            complete,
        }
    }

    #[test]
    fn nftpage_keylets_are_owner_prefixed() {
        let o = owner(0xAB);
        assert_eq!(&nftpage_min(&o)[..20], &o[..], "min keeps the owner prefix");
        assert_eq!(&nftpage_min(&o)[20..], &[0u8; 12][..], "min zeroes the page tag");
        assert_eq!(&nftpage_max(&o)[..20], &o[..], "max keeps the owner prefix");
        assert_eq!(&nftpage_max(&o)[20..], &[0xFFu8; 12][..], "max fills the page tag");
        assert_eq!(owner_of_page_key(&page(&o, 7)), o, "owner recovered from a page key");
    }

    /// The exclusive bound rippled passes is nftpage_max(owner).next(), which
    /// is the NEXT account's nftpage_min — a real key whenever that account
    /// owns pages. This is the 6893bbe lesson in keylet form.
    #[test]
    fn max_next_is_the_next_owners_min() {
        let o = owner(0x10);
        let mut next_owner = o;
        next_owner[19] = 0x11;
        assert_eq!(
            nftpage_max_next(&o),
            Some(nftpage_min(&next_owner)),
            "max.next() rolls into the next account's first page key"
        );
        assert!(
            nftpage_max_next(&owner(0xFF)).is_none(),
            "all-ones owner sits at the top of the keyspace — no next bound"
        );
    }

    /// The bound is EXCLUSIVE: the next owner's first page must never be
    /// surfaced as this owner's page. An inclusive bound here is exactly the
    /// "NFT found in incorrect page" invariant failure.
    #[test]
    fn succ_never_returns_the_next_owners_page() {
        let o = owner(0x10);
        let bound = nftpage_max_next(&o).expect("bound exists");
        // This owner has NO pages; the next owner's first page IS `bound`.
        let set = pages_of(&o, &[], true);
        assert_eq!(
            succ_from_pages(&set, &nftpage_min(&o), Some(&bound)),
            NftSuccAnswer::NoneAuthoritative,
            "an empty owner must answer None, not the next owner's page at the bound"
        );
        // And with a page present, the in-range page wins and the bound is
        // still excluded.
        let set = pages_of(&o, &[0x40], true);
        assert_eq!(
            succ_from_pages(&set, &nftpage_min(&o), Some(&bound)),
            NftSuccAnswer::Found(page(&o, 0x40))
        );
    }

    #[test]
    fn succ_is_the_open_interval_within_the_owner() {
        let o = owner(0x22);
        let set = pages_of(&o, &[0x10, 0x20, 0x30], true);
        let bound = nftpage_max_next(&o).expect("bound exists");
        // Strictly greater than key.
        assert_eq!(
            succ_from_pages(&set, &page(&o, 0x10), Some(&bound)),
            NftSuccAnswer::Found(page(&o, 0x20)),
            "key itself is excluded — succ is strictly greater"
        );
        // Strictly less than last.
        assert_eq!(
            succ_from_pages(&set, &page(&o, 0x10), Some(&page(&o, 0x20))),
            NftSuccAnswer::NoneAuthoritative,
            "last is excluded — 0x20 must not be returned when it IS the bound"
        );
        assert_eq!(
            succ_from_pages(&set, &page(&o, 0x10), Some(&page(&o, 0x21))),
            NftSuccAnswer::Found(page(&o, 0x20)),
            "0x20 is returned once the bound moves strictly above it"
        );
        // The lowest page wins from below — succ is the SMALLEST candidate
        // in the interval, not merely any of them.
        assert_eq!(
            succ_from_pages(&set, &page(&o, 0x00), Some(&bound)),
            NftSuccAnswer::Found(page(&o, 0x10)),
            "succ returns the smallest qualifying page"
        );
    }

    /// An owner with no pages at all is a legitimate COMPLETE view: answering
    /// authoritative None is what lets a first-ever mint place its page.
    #[test]
    fn empty_page_set_is_authoritative_when_complete() {
        let o = owner(0x33);
        let bound = nftpage_max_next(&o).expect("bound exists");
        assert_eq!(
            succ_from_pages(&pages_of(&o, &[], true), &nftpage_min(&o), Some(&bound)),
            NftSuccAnswer::NoneAuthoritative
        );
        assert_eq!(
            succ_from_pages(&pages_of(&o, &[], false), &nftpage_min(&o), Some(&bound)),
            NftSuccAnswer::Unknown,
            "a truncated prefetch must never claim 'no page' — fall back to the walk"
        );
    }

    /// Probes that are not provably confined to the owner's page range must
    /// fall through to the walk rather than be answered from a set that only
    /// covers that owner.
    #[test]
    fn probes_outside_the_owner_range_are_unknown() {
        let o = owner(0x44);
        let set = pages_of(&o, &[0x10], true);
        assert_eq!(
            succ_from_pages(&set, &page(&o, 0x20), None),
            NftSuccAnswer::Unknown,
            "unbounded probe can run past the owner's range — not answerable here"
        );
        let mut far = nftpage_min(&owner(0x99));
        far[31] = 1;
        assert_eq!(
            succ_from_pages(&set, &page(&o, 0x20), Some(&far)),
            NftSuccAnswer::Unknown,
            "bound above the owner's range — not answerable here"
        );
        assert!(!interval_within_owner(&o, &nftpage_min(&owner(0x45)), Some(&far)));
        assert!(interval_within_owner(
            &o,
            &nftpage_min(&o),
            Some(&nftpage_max_next(&o).unwrap())
        ));
        // The all-ones owner has no bound above it, so any probe is confined.
        assert!(interval_within_owner(&owner(0xFF), &nftpage_min(&owner(0xFF)), None));
    }

    #[test]
    fn parses_account_objects_pages_and_marker() {
        let o = owner(0x55);
        let body = json!({"result": {"account_objects": [
            {"LedgerEntryType": "NFTokenPage", "index": hex::encode_upper(page(&o, 0x01))},
            {"LedgerEntryType": "RippleState", "index": hex::encode_upper([0xEEu8; 32])},
            {"LedgerEntryType": "NFTokenPage", "index": hex::encode_upper(page(&o, 0x02))},
        ], "marker": "abc"}});
        let (keys, marker) = parse_account_objects_pages(&body).expect("parses");
        assert_eq!(keys, vec![page(&o, 0x01), page(&o, 0x02)], "non-page entries ignored");
        assert_eq!(marker, Some(json!("abc")));

        let no_marker = json!({"result": {"account_objects": []}});
        assert_eq!(parse_account_objects_pages(&no_marker).unwrap(), (vec![], None));
    }

    /// A dropped page could make succ return a WRONG key — malformed input
    /// must fail the fetch, never silently shrink the set.
    #[test]
    fn malformed_account_objects_are_rejected() {
        assert!(parse_account_objects_pages(&json!({"result": {"error": "actNotFound"}})).is_err());
        assert!(parse_account_objects_pages(&json!({"result": {}})).is_err());
        assert!(parse_account_objects_pages(
            &json!({"result": {"account_objects": [{"LedgerEntryType": "NFTokenPage"}]}})
        )
        .is_err());
        assert!(parse_account_objects_pages(&json!({"result": {"account_objects": [
            {"LedgerEntryType": "NFTokenPage", "index": "AABB"}
        ]}}))
        .is_err());
    }

    #[test]
    fn collect_owner_pages_follows_markers_then_completes() {
        let o = owner(0x66);
        let mut calls = 0;
        let set = collect_owner_pages(
            |marker| {
                calls += 1;
                match calls {
                    1 => {
                        assert!(marker.is_none(), "first fetch is unmarked");
                        Some((vec![page(&o, 0x01)], Some(json!("m1"))))
                    }
                    _ => {
                        assert_eq!(marker, Some(&json!("m1")), "marker threaded through");
                        Some((vec![page(&o, 0x02)], None))
                    }
                }
            },
            &o,
            8,
        )
        .expect("collects");
        assert_eq!(calls, 2);
        assert!(set.complete, "marker-less final page means a complete view");
        assert_eq!(
            set.pages.iter().copied().collect::<Vec<_>>(),
            vec![page(&o, 0x01), page(&o, 0x02)]
        );
    }

    #[test]
    fn collect_owner_pages_fails_closed() {
        let o = owner(0x77);
        // A failed fetch must yield no set at all: a partial set marked
        // complete would be a silently wrong authoritative answer.
        assert!(collect_owner_pages(|_| None, &o, 8).is_none(), "fetch failure means no set");
        // Exhausting the page budget with a marker pending leaves a prefix.
        let set = collect_owner_pages(|_| Some((vec![page(&o, 0x01)], Some(json!("m")))), &o, 2)
            .expect("returns a prefix");
        assert!(!set.complete, "marker still pending at the cap — not authoritative");
    }

    // ---- tx owner extraction -------------------------------------------

    /// Serialize a minimal tx: TransactionType `tt` plus the given
    /// (field_code, account) AccountID (type 8) fields, in canonical order.
    fn nft_tx(tt: u16, accounts: &[(u8, [u8; 20])]) -> Vec<u8> {
        let mut out = vec![0x12]; // UInt16 / TransactionType (1,2)
        out.extend_from_slice(&tt.to_be_bytes());
        for (field, id) in accounts {
            out.push(0x80 | field); // AccountID (type 8) / field
            out.push(20); // VL length
            out.extend_from_slice(id);
        }
        out
    }

    #[test]
    fn extracts_owners_from_nft_txs() {
        // NFTokenMint (25) with Account + Issuer.
        let account = owner(0xA1);
        let issuer = owner(0xB2);
        assert_eq!(
            parse_nft_page_owners(&nft_tx(25, &[(1, account), (4, issuer)])),
            vec![account, issuer],
            "mint names the minter and the issuer"
        );
        // NFTokenCreateOffer (27) buy offer: Owner holds the token.
        let holder = owner(0xC3);
        assert_eq!(
            parse_nft_page_owners(&nft_tx(27, &[(1, account), (2, holder)])),
            vec![account, holder],
            "create-offer names the token's owner"
        );
        for tt in [26u16, 28, 29] {
            assert_eq!(
                parse_nft_page_owners(&nft_tx(tt, &[(1, account)])),
                vec![account],
                "tt {tt} is an NFT page-walking type"
            );
        }
    }

    #[test]
    fn ignores_non_nft_txs_and_junk() {
        let account = owner(0xA1);
        for tt in [0u16, 7, 20, 24, 30] {
            assert!(
                parse_nft_page_owners(&nft_tx(tt, &[(1, account)])).is_empty(),
                "tt {tt} does not walk NFT pages — no prefetch"
            );
        }
        assert!(parse_nft_page_owners(&[]).is_empty(), "empty blob");
        assert!(parse_nft_page_owners(&[0x12, 0x00]).is_empty(), "truncated tt");
        // Zero account is a placeholder, never a page owner.
        assert!(
            parse_nft_page_owners(&nft_tx(25, &[(1, [0u8; 20])])).is_empty(),
            "zero account dropped"
        );
    }

    /// The scan must cross VL fields (SigningPubKey/TxnSignature), STArrays
    /// (Memos) and every fixed-width type to reach the AccountID region —
    /// dropping out early would silently skip the prefetch.
    #[test]
    fn crosses_full_canonical_field_order() {
        let account = owner(0xA1);
        let mut tx = vec![0x12, 0x00, 0x19]; // TransactionType = 25 (NFTokenMint)
        tx.extend_from_slice(&[0x22, 0x00, 0x00, 0x00, 0x08]); // UInt32 Flags
        tx.extend_from_slice(&[0x20, 0x2A, 0x00, 0x00, 0x00, 0x05]); // UInt32 (2,42)
        tx.extend_from_slice(&[0x50, 0x14]); // Hash256 (5,20)
        tx.extend_from_slice(&[0u8; 32]);
        tx.push(0x61); // Amount (6,1) XRP
        tx.extend_from_slice(&[0x40, 0, 0, 0, 0, 0, 0, 10]);
        tx.extend_from_slice(&[0x73, 0x02, 0xAA, 0xBB]); // VL SigningPubKey (7,3)
        tx.extend_from_slice(&[0x80 | 1, 20]); // AccountID Account (8,1)
        tx.extend_from_slice(&account);
        tx.extend_from_slice(&[0xF9, 0xEA, 0xE1, 0xF1]); // Memos: [ {} ]
        assert_eq!(
            parse_nft_page_owners(&tx),
            vec![account],
            "Account found after fixed-width, VL and STArray fields"
        );
    }
}
