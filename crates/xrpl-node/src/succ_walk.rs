//! Pure logic for RPC-backed successor (`succ`) lookups.
//!
//! The standalone-RPC apply path (no state.rocks snapshot) needs directory /
//! offer-book traversal: `SleProvider::succ(key, last)` must return the
//! smallest ledger key STRICTLY GREATER than `key` and, when `last` is set,
//! STRICTLY LESS than `last` — the open interval (key, last), matching
//! rippled's `ReadView::succ`. The bound being exclusive is load-bearing:
//! NFT page walks call `succ(first, bound)` where `bound` can equal a real
//! key, and an inclusive bound corrupts page selection.
//!
//! The network side lives in `ffi_engine::RpcProvider` (feature = "ffi");
//! everything here is bytes/json in → candidate out so it is unit-testable
//! offline. The walk uses rippled's `ledger_data` RPC: marker-based
//! resumption is ">= position", so the caller seeds the marker with `key`
//! itself and this module filters for strictly-greater client-side.

/// One parsed `ledger_data` page: the state keys it contained (ledger order,
/// i.e. ascending) and the resumption marker, if any.
#[derive(Debug)]
pub struct LedgerDataPage {
    pub keys: Vec<[u8; 32]>,
    pub marker: Option<serde_json::Value>,
}

/// Parse a `ledger_data` response body into keys + marker.
///
/// Returns `Err` on any body-level error or malformed entry — a silently
/// dropped key could make the walk return a WRONG successor, which is worse
/// than failing the fetch (the caller treats `Err` as retryable/abort).
pub fn parse_ledger_data_page(body: &serde_json::Value) -> Result<LedgerDataPage, String> {
    if let Some(err) = body["result"]["error"].as_str() {
        return Err(err.to_string());
    }
    let state = body["result"]["state"]
        .as_array()
        .ok_or_else(|| "no_state_array".to_string())?;
    let mut keys = Vec::with_capacity(state.len());
    for entry in state {
        let idx_hex = entry["index"]
            .as_str()
            .ok_or_else(|| "entry_missing_index".to_string())?;
        let bytes = hex::decode(idx_hex).map_err(|_| format!("bad_index_hex: {idx_hex}"))?;
        if bytes.len() != 32 {
            return Err(format!("index_len_{}: {idx_hex}", bytes.len()));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        keys.push(arr);
    }
    let marker = match &body["result"]["marker"] {
        serde_json::Value::Null => None,
        m => Some(m.clone()),
    };
    Ok(LedgerDataPage { keys, marker })
}

/// Smallest key in `keys` strictly greater than `key` and, when `last` is
/// set, strictly less than `last` (open interval — see module doc).
pub fn select_succ_candidate(
    keys: &[[u8; 32]],
    key: &[u8; 32],
    last: Option<&[u8; 32]>,
) -> Option<[u8; 32]> {
    keys.iter()
        .filter(|k| k.as_slice() > key.as_slice())
        .filter(|k| last.map_or(true, |lk| k.as_slice() < lk.as_slice()))
        .min()
        .copied()
}

/// Given a page's keys (ascending ledger order), decide whether any LATER
/// page could still hold a candidate below the exclusive bound. Once a page
/// reaches a key >= `last`, all subsequent keys are >= it too — stop.
pub fn page_exhausts_bound(keys: &[[u8; 32]], last: Option<&[u8; 32]>) -> bool {
    match last {
        None => false,
        Some(lk) => keys.iter().any(|k| k.as_slice() >= lk.as_slice()),
    }
}

/// Walk `ledger_data` pages until a successor is found or the walk is
/// provably done. `fetch_page` maps a marker value to a parsed page (`None`
/// = fetch failed after retries — abort with no answer, never guess).
///
/// The first request seeds the marker with `key` itself: rippled resumes at
/// the first state key >= marker, so the successor is almost always on the
/// very first page. `max_pages` is a safety cap against marker loops.
pub fn walk_pages_for_succ<F>(
    mut fetch_page: F,
    key: &[u8; 32],
    last: Option<&[u8; 32]>,
    max_pages: usize,
) -> Option<[u8; 32]>
where
    F: FnMut(&serde_json::Value) -> Option<LedgerDataPage>,
{
    let mut marker = serde_json::Value::String(hex::encode_upper(key));
    for _ in 0..max_pages {
        let page = fetch_page(&marker)?;
        if let Some(cand) = select_succ_candidate(&page.keys, key, last) {
            return Some(cand);
        }
        if page_exhausts_bound(&page.keys, last) {
            return None;
        }
        match page.marker {
            None => return None, // end of ledger state — no successor
            Some(m) => marker = m,
        }
    }
    None
}

/// Smallest LIVE overlay key strictly inside the open interval (key, last).
/// The caller pre-filters for liveness (`Some(_)` values — not tombstones).
pub fn overlay_succ_candidate<'a, I>(
    live_keys: I,
    key: &[u8; 32],
    last: Option<&[u8; 32]>,
) -> Option<[u8; 32]>
where
    I: Iterator<Item = &'a [u8; 32]>,
{
    live_keys
        .filter(|k| k.as_slice() > key.as_slice())
        .filter(|k| last.map_or(true, |lk| k.as_slice() < lk.as_slice()))
        .min()
        .copied()
}

/// Merge the RPC-side and overlay-side candidates: the effective successor
/// is the minimum of the two views (mirrors `OverlayedDbProvider::succ`).
pub fn merge_succ_candidates(
    a: Option<[u8; 32]>,
    b: Option<[u8; 32]>,
) -> Option<[u8; 32]> {
    match (a, b) {
        (None, None) => None,
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (Some(x), Some(y)) => Some(if y < x { y } else { x }),
    }
}

/// Layered successor: chain the RPC view's successor past overlay
/// tombstones (a deleted entry must NOT be resurrected by the RPC view),
/// then take the minimum against the overlay-inserted candidate.
///
/// `rpc_succ(probe, last)` returns the RPC view's successor after `probe`;
/// `is_tombstoned` answers "did the overlay delete this key this ledger?".
pub fn layered_succ<F, T>(
    mut rpc_succ: F,
    is_tombstoned: T,
    overlay_cand: Option<[u8; 32]>,
    key: &[u8; 32],
    last: Option<&[u8; 32]>,
) -> Option<[u8; 32]>
where
    F: FnMut(&[u8; 32], Option<&[u8; 32]>) -> Option<[u8; 32]>,
    T: Fn(&[u8; 32]) -> bool,
{
    let mut probe = *key;
    let rpc_cand = loop {
        match rpc_succ(&probe, last) {
            None => break None,
            Some(c) if is_tombstoned(&c) => probe = c, // skip deleted, keep walking
            Some(c) => break Some(c),
        }
    };
    merge_succ_candidates(rpc_cand, overlay_cand)
}

// Offline tests live in crates/xrpl-node/tests/succ_walk_offline.rs — an
// integration target, because the gate runner only executes test targets
// (in-module #[cfg(test)] tests never run there). Everything the tests
// need is part of this module's public API.
