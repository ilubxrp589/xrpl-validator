//! NFTokenPage model — real rippled page keys and token placement.
//!
//! rippled reference: `NFTokenUtils.cpp` / `Indexes.cpp`. NFTs do NOT get one
//! ledger object each: they live packed in `NFTokenPage` objects. A page key
//! is structural, not hashed: `owner_account (20 bytes) ++ bound (12 bytes)`,
//! where `bound` is an upper bound on the low 96 bits of the NFTokenIDs the
//! page may hold. Every owner with tokens has a "max" page at bound
//! `FFFF…FF`; earlier pages (from splits) chain via `PreviousPageMin` /
//! `NextPageMin` (full 64-hex page keys). A token with low-96 `t` belongs on
//! the first page (ascending bound order) whose bound ≥ `t`.
//!
//! Page splitting on overflow (> 32 tokens) is not implemented yet — none of
//! the differential corpus exercises a split; `page_insert` appends past 32
//! rather than splitting, which will surface as a mutation divergence the day
//! a split case appears.

use xrpl_core::types::Hash256;

use super::sandbox::Sandbox;

/// Max NFTokens per page (rippled `dirMaxTokensPerPage`).
pub const PAGE_MAX: usize = 32;

/// Low 96 bits (12 bytes) of an NFTokenID.
pub fn low96(id: &Hash256) -> [u8; 12] {
    let mut b = [0u8; 12];
    b.copy_from_slice(&id.0[20..32]);
    b
}

/// Issuer account embedded in an NFTokenID (bytes 4..24).
pub fn issuer_of(id: &Hash256) -> [u8; 20] {
    let mut b = [0u8; 20];
    b.copy_from_slice(&id.0[4..24]);
    b
}

/// Structural page key: `owner ++ bound`.
pub fn page_key(owner: &[u8; 20], bound: &[u8; 12]) -> Hash256 {
    let mut k = [0u8; 32];
    k[..20].copy_from_slice(owner);
    k[20..].copy_from_slice(bound);
    Hash256(k)
}

/// The owner's last page — bound `FF…FF`. Exists whenever the owner holds any
/// tokens.
pub fn max_page_key(owner: &[u8; 20]) -> Hash256 {
    page_key(owner, &[0xFF; 12])
}

fn read_page(sandbox: &Sandbox, key: &Hash256) -> Option<serde_json::Value> {
    sandbox
        .read(key)
        .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
}

fn parse_page_ref(v: Option<&serde_json::Value>) -> Option<Hash256> {
    let s = v?.as_str()?;
    let b = hex::decode(s).ok()?;
    (b.len() == 32).then(|| {
        let mut k = [0u8; 32];
        k.copy_from_slice(&b);
        Hash256(k)
    })
}

fn entry_id(entry: &serde_json::Value) -> String {
    entry["NFToken"]["NFTokenID"]
        .as_str()
        .unwrap_or("")
        .to_uppercase()
}

/// Find the page a token with `id` belongs on: walk backward from the max
/// page while the previous page's bound still covers the token's low-96.
pub fn find_page(sandbox: &Sandbox, owner: &[u8; 20], id: &Hash256) -> Option<Hash256> {
    let t = low96(id);
    let mut candidate = max_page_key(owner);
    read_page(sandbox, &candidate)?;
    for _ in 0..100_000 {
        let page = read_page(sandbox, &candidate)?;
        let Some(prev) = parse_page_ref(page.get("PreviousPageMin")) else {
            break;
        };
        let mut prev_bound = [0u8; 12];
        prev_bound.copy_from_slice(&prev.0[20..32]);
        // STRICTLY greater: "The low 96-bits of NFT ID must be strictly
        // less than the low 96-bits of the enclosing page's index"
        // (NFTokenHelpers.cpp:180-184), and getPageForToken looks for the
        // first page with a key strictly greater than nftpage(base, id).
        // A page whose bound EQUALS the token's low96 cannot hold it — the
        // token belongs one page later. The inclusive walk sent #106008702
        // 0740822A's token (low96 == the BAE35151… boundary, a returning
        // token whose id once DEFINED that split) into the 21-slot page,
        // skipping the FULL E6061704… page mainnet splits (7v9).
        if prev_bound > t && read_page(sandbox, &prev).is_some() {
            candidate = prev;
        } else {
            break;
        }
    }
    Some(candidate)
}

/// Insert a token entry (`{"NFToken": {"NFTokenID": …, "URI"?: …}}`) into the
/// owner's pages, keeping NFTokens sorted by id. Returns true if a fresh page
/// was created (the caller owes an OwnerCount bump).
/// rippled `compareTokens`: order by the LOW 96 BITS first, and only fall back
/// to the whole 256-bit ID when those tie. Pages are keyed on the low 96 bits,
/// so this is what keeps "equivalent" tokens adjacent — plain whole-ID ordering
/// would interleave them and break the split rule below.
fn compare_tokens(a: &Hash256, b: &Hash256) -> std::cmp::Ordering {
    low96(a).cmp(&low96(b)).then_with(|| a.0.cmp(&b.0))
}

fn entry_hash(entry: &serde_json::Value) -> Option<Hash256> {
    let b = hex::decode(entry_id(entry)).ok()?;
    <[u8; 32]>::try_from(b.as_slice()).ok().map(Hash256)
}

/// 256-bit increment — rippled's `uint256::next()`.
fn next_id(id: &Hash256) -> Hash256 {
    let mut out = id.0;
    for byte in out.iter_mut().rev() {
        let (v, carry) = byte.overflowing_add(1);
        *byte = v;
        if !carry {
            break;
        }
    }
    Hash256(out)
}

/// Split a FULL page and place `entry`, mirroring rippled `getPageForToken`.
///
/// The first part of the page moves to a NEW page keyed below it; the remainder
/// stays. The split point is NOT the midpoint: it rounds forward past any run
/// of tokens sharing the middle token's low-96 bits, because "all equivalent
/// NFTs must be kept on the same page", so the split may be lopsided.
///
/// Returns true when a page was created (the caller owes OwnerCount +1).
fn split_and_insert(
    sandbox: &mut Sandbox,
    owner: &[u8; 20],
    cp_key: &Hash256,
    cp: &mut serde_json::Value,
    entry: serde_json::Value,
    id: &Hash256,
) -> bool {
    let mut narr: Vec<serde_json::Value> =
        cp["NFTokens"].as_array().cloned().unwrap_or_default();
    if narr.len() != PAGE_MAX {
        return false;
    }
    let half = PAGE_MAX / 2;
    let Some(mid) = entry_hash(&narr[half - 1]) else { return false };
    let cmp = low96(&mid);

    // First entry at or past the midpoint whose low-96 differs from cmp.
    let mut split = narr[half..]
        .iter()
        .position(|o| entry_hash(o).map(|h| low96(&h) != cmp).unwrap_or(true))
        .map(|i| i + half);
    // All equivalent from the middle onwards — look from the front instead.
    if split.is_none() {
        split = narr
            .iter()
            .position(|o| entry_hash(o).map(|h| low96(&h) == cmp).unwrap_or(false));
    }
    let Some(mut split) = split else { return false };

    if split == 0 {
        // The whole page is one equivalence class.
        match low96(id).cmp(&cmp) {
            std::cmp::Ordering::Equal => return false, // cannot be stored
            std::cmp::Ordering::Greater => split = narr.len(), // keep all, new token goes alone
            std::cmp::Ordering::Less => {}             // move all, new token goes alone
        }
    }

    let carr: Vec<serde_json::Value> = narr.split_off(split);

    // "The low 96-bits of an NFT ID must be strictly less than the low 96-bits
    // of the enclosing page's index", so a full lower half is keyed one above
    // its largest token.
    let token_for_new_page = if narr.len() == PAGE_MAX {
        let Some(last) = entry_hash(&narr[PAGE_MAX - 1]) else { return false };
        next_id(&last)
    } else {
        let Some(first) = carr.first().and_then(entry_hash) else { return false };
        first
    };
    let np_key = page_key(owner, &low96(&token_for_new_page));

    let mut np = serde_json::json!({
        "LedgerEntryType": "NFTokenPage",
        "Flags": 0,
        "NFTokens": narr,
        "NextPageMin": hex::encode_upper(cp_key.0),
    });
    if let Some(ppm) = parse_page_ref(cp.get("PreviousPageMin")) {
        np["PreviousPageMin"] = serde_json::json!(hex::encode_upper(ppm.0));
        if let Some(mut p3) = read_page(sandbox, &ppm) {
            p3["NextPageMin"] = serde_json::json!(hex::encode_upper(np_key.0));
            sandbox.write(ppm, serde_json::to_vec(&p3).unwrap_or_default());
        }
    }
    cp["NFTokens"] = serde_json::Value::Array(carr);
    cp["PreviousPageMin"] = serde_json::json!(hex::encode_upper(np_key.0));

    // The new token lands in whichever page now covers its key.
    let target_is_new = page_key(owner, &low96(id)).0 < np_key.0;
    let sink: &mut serde_json::Value = if target_is_new { &mut np } else { &mut *cp };
    if let Some(arr) = sink.get_mut("NFTokens").and_then(|v| v.as_array_mut()) {
        arr.push(entry);
        arr.sort_by(|x, y| match (entry_hash(x), entry_hash(y)) {
            (Some(a), Some(b)) => compare_tokens(&a, &b),
            _ => std::cmp::Ordering::Equal,
        });
    }

    sandbox.write(np_key, serde_json::to_vec(&np).unwrap_or_default());
    sandbox.write(*cp_key, serde_json::to_vec(cp).unwrap_or_default());
    true
}

pub fn page_insert(sandbox: &mut Sandbox, owner: &[u8; 20], entry: serde_json::Value) -> bool {
    let id_hex = entry_id(&entry);
    let Ok(idb) = hex::decode(&id_hex) else { return false };
    if idb.len() != 32 {
        return false;
    }
    let mut id = [0u8; 32];
    id.copy_from_slice(&idb);
    let id = Hash256(id);

    if let Some(pk) = find_page(sandbox, owner, &id) {
        if let Some(mut page) = read_page(sandbox, &pk) {
            let arr = page
                .get_mut("NFTokens")
                .and_then(|v| v.as_array_mut());
            let full = arr.as_ref().map(|a| a.len() >= PAGE_MAX).unwrap_or(false);
            if std::env::var("DX_NFT").is_ok() {
                eprintln!(
                    "DX_NFT insert owner={} id={} page={} len={:?} full={full}",
                    hex::encode(owner), id_hex,
                    hex::encode_upper(&pk.0[20..]),
                    arr.as_ref().map(|a| a.len()),
                );
            }
            if full {
                // rippled getPageForToken: a full page is SPLIT before the
                // insert. #105818293 012140AAC3A0 mints into rKqqb5QZ's full
                // page, so mainnet Creates a new page and Modifies the
                // predecessor's link — 10 nodes to our 8, and we quietly grew
                // the page to 33 tokens.
                return split_and_insert(sandbox, owner, &pk, &mut page, entry, &id);
            }
            if let Some(arr) = arr {
                let pos = arr
                    .iter()
                    .position(|e| match entry_hash(e) {
                        Some(h) => compare_tokens(&h, &id) == std::cmp::Ordering::Greater,
                        None => false,
                    })
                    .unwrap_or(arr.len());
                arr.insert(pos, entry);
                sandbox.write(pk, serde_json::to_vec(&page).unwrap_or_default());
                return false;
            }
        }
    }
    // No pages yet — create the owner's max page.
    //
    // DX_NFT: this branch is also where a page-placement divergence lands, so
    // say WHY it was reached — a missing max page (hydration) and a failed
    // backward walk (engine) look identical from the outside.
    if std::env::var("DX_NFT").is_ok() {
        eprintln!(
            "DX_NFT page_insert CREATES max owner={} id={id_hex} max_present={} find_page={:?}",
            hex::encode(owner),
            read_page(sandbox, &max_page_key(owner)).is_some(),
            find_page(sandbox, owner, &id).map(|k| hex::encode_upper(k.0)),
        );
    }
    let page = serde_json::json!({
        "LedgerEntryType": "NFTokenPage",
        "Flags": 0,
        "NFTokens": [entry],
    });
    sandbox.write(
        max_page_key(owner),
        serde_json::to_vec(&page).unwrap_or_default(),
    );
    true
}

/// Result of removing a token from an owner's pages.
/// Merge NFToken page `p1` into `p2` when their tokens fit in one page.
///
/// rippled `mergePages` (NFTokenHelpers.cpp): requires `p1 < p2`, `p1.next ==
/// p2` and `p2.prev == p1`. Returns false — no merge — when
/// `p1.len + p2.len > kDirMaxTokensPerPage`, "since it only makes sense to do
/// this if it would mean that one of them can be deleted as a result". The
/// SURVIVOR is p2: the merged token list is written there, p1's predecessor is
/// relinked to p2, and p1 is erased.
fn merge_pages(sandbox: &mut Sandbox, p1_key: &Hash256, p2_key: &Hash256) -> bool {
    if p1_key.0 >= p2_key.0 {
        return false;
    }
    let (Some(p1), Some(mut p2)) = (read_page(sandbox, p1_key), read_page(sandbox, p2_key)) else {
        return false;
    };
    // Links must agree, exactly as rippled asserts before merging.
    if parse_page_ref(p1.get("NextPageMin")) != Some(*p2_key)
        || parse_page_ref(p2.get("PreviousPageMin")) != Some(*p1_key)
    {
        return false;
    }
    let empty: Vec<serde_json::Value> = Vec::new();
    let a1 = p1["NFTokens"].as_array().cloned().unwrap_or_else(|| empty.clone());
    let a2 = p2["NFTokens"].as_array().cloned().unwrap_or(empty);
    if a1.len() + a2.len() > PAGE_MAX {
        return false;
    }
    let mut merged: Vec<serde_json::Value> = a1.into_iter().chain(a2).collect();
    // Two adjacent pages are each a `compare_tokens`-ordered run with p1's
    // low-96 bound below p2's, so the concatenation is ALREADY ordered —
    // rippled's merge moves the arrays together and never re-sorts. Sorting
    // by the whole-ID hex here (as this once did) REORDERS across the seam
    // whenever whole-ID order disagrees with low-96 order: #106455111
    // D9C28F93's consolidation pulled 00081388… (small ID, HIGH low 96)
    // ahead of seven 00082710… tokens whose low-96s sort first, and the
    // page landed a whole token off mainnet — REPLAY-only, invisible to
    // every per-tx leg. Sort defensively, but by the module's one rule.
    merged.sort_by(|x, y| match (entry_hash(x), entry_hash(y)) {
        (Some(a), Some(b)) => compare_tokens(&a, &b),
        _ => std::cmp::Ordering::Equal,
    });
    p2["NFTokens"] = serde_json::Value::Array(merged);

    // Relink: p2 inherits p1's previous, and that page points at p2.
    match parse_page_ref(p1.get("PreviousPageMin")) {
        Some(p0k) => {
            p2["PreviousPageMin"] = serde_json::json!(hex::encode_upper(p0k.0));
            if let Some(mut p0) = read_page(sandbox, &p0k) {
                p0["NextPageMin"] = serde_json::json!(hex::encode_upper(p2_key.0));
                sandbox.write(p0k, serde_json::to_vec(&p0).unwrap_or_default());
            }
        }
        None => {
            if let Some(o) = p2.as_object_mut() {
                o.remove("PreviousPageMin");
            }
        }
    }
    sandbox.write(*p2_key, serde_json::to_vec(&p2).unwrap_or_default());
    sandbox.delete(*p1_key);
    true
}

pub struct PageRemoval {
    /// The removed entry, URI and all — reinsert it to transfer the token.
    pub entry: serde_json::Value,
    /// True if the page emptied and was deleted (caller owes OwnerCount -1).
    pub page_deleted: bool,
    /// Pages consolidated away after the removal (caller owes OwnerCount -N).
    pub pages_merged: u32,
}

/// Remove the token `id` from the owner's pages, searching the chain by
/// membership (robust against bound drift). Deleting an emptied page relinks
/// its neighbours' NextPageMin/PreviousPageMin.
pub fn page_remove(sandbox: &mut Sandbox, owner: &[u8; 20], id: &Hash256) -> Option<PageRemoval> {
    let id_hex = hex::encode_upper(id.0);
    let mut cur = max_page_key(owner);
    for _ in 0..100_000 {
        let Some(mut page) = read_page(sandbox, &cur) else { return None };
        let found = page["NFTokens"]
            .as_array()
            .map(|a| a.iter().any(|e| entry_id(e) == id_hex))
            .unwrap_or(false);
        if !found {
            let Some(prev) = parse_page_ref(page.get("PreviousPageMin")) else {
                return None;
            };
            cur = prev;
            continue;
        }
        let mut removed = serde_json::Value::Null;
        if let Some(arr) = page.get_mut("NFTokens").and_then(|v| v.as_array_mut()) {
            if let Some(pos) = arr.iter().position(|e| entry_id(e) == id_hex) {
                removed = arr.remove(pos);
            }
        }
        let empty = page["NFTokens"].as_array().map(|a| a.is_empty()).unwrap_or(true);
        if !empty {
            sandbox.write(cur, serde_json::to_vec(&page).unwrap_or_default());
            // A page that merely SHRANK still has to be consolidated with its
            // neighbours — rippled removeToken: "The current page isn't empty.
            // Update it and then try to consolidate pages. Note that this
            // consolidation attempt may actually merge three pages into one!"
            //
            // #105952436 41292DD9E9B4: the seller's chain is 228A/233D/243B and
            // the sold token lived in 243B. Losing it let 233D merge into 243B,
            // so mainnet Deletes 233D and Modifies 228A (relinked) — 9 nodes to
            // our 7. We removed the token and stopped.
            let prev = parse_page_ref(page.get("PreviousPageMin"));
            let next = parse_page_ref(page.get("NextPageMin"));
            let mut pages_merged = 0u32;
            if let Some(pk) = prev {
                if merge_pages(sandbox, &pk, &cur) {
                    pages_merged += 1;
                }
            }
            if let Some(nk) = next {
                if merge_pages(sandbox, &cur, &nk) {
                    pages_merged += 1;
                }
            }
            return Some(PageRemoval { entry: removed, page_deleted: false, pages_merged });
        }
        // Page emptied — delete it and relink neighbours.
        let prev = parse_page_ref(page.get("PreviousPageMin"));
        let next = parse_page_ref(page.get("NextPageMin"));
        sandbox.delete(cur);
        if let Some(pk) = prev {
            if let Some(mut pp) = read_page(sandbox, &pk) {
                match next {
                    Some(n) => pp["NextPageMin"] = serde_json::json!(hex::encode_upper(n.0)),
                    None => {
                        pp.as_object_mut().map(|o| o.remove("NextPageMin"));
                    }
                }
                sandbox.write(pk, serde_json::to_vec(&pp).unwrap_or_default());
            }
        }
        if let Some(nk) = next {
            if let Some(mut np) = read_page(sandbox, &nk) {
                match prev {
                    Some(p) => np["PreviousPageMin"] = serde_json::json!(hex::encode_upper(p.0)),
                    None => {
                        np.as_object_mut().map(|o| o.remove("PreviousPageMin"));
                    }
                }
                sandbox.write(nk, serde_json::to_vec(&np).unwrap_or_default());
            }
        }
        return Some(PageRemoval { entry: removed, page_deleted: true, pages_merged: 0 });
    }
    None
}

/// Find (without modifying) the page holding `id` and return its key + parsed
/// body — the read-side of `page_remove`, used by NFTokenModify.
pub fn locate_token(
    sandbox: &Sandbox,
    owner: &[u8; 20],
    id: &Hash256,
) -> Option<(Hash256, serde_json::Value)> {
    let id_hex = hex::encode_upper(id.0);
    let dx = std::env::var("DX_NFT").is_ok();
    let mut cur = max_page_key(owner);
    for hop in 0..100_000 {
        let Some(page) = read_page(sandbox, &cur) else {
            if dx {
                eprintln!("DX_NFT locate hop={hop} PAGE ABSENT {}", hex::encode_upper(cur.0));
            }
            return None;
        };
        let found = page["NFTokens"]
            .as_array()
            .map(|a| a.iter().any(|e| entry_id(e) == id_hex))
            .unwrap_or(false);
        if found {
            return Some((cur, page));
        }
        let Some(next) = parse_page_ref(page.get("PreviousPageMin")) else {
            if dx {
                eprintln!(
                    "DX_NFT locate hop={hop} CHAIN END at {} (prev={:?})",
                    hex::encode_upper(cur.0),
                    page.get("PreviousPageMin")
                );
            }
            return None;
        };
        cur = next;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::state::LedgerState;

    fn empty_state() -> LedgerState {
        LedgerState::new_unverified(LedgerHeader {
            sequence: 100,
            total_coins: 100_000_000_000_000_000,
            parent_hash: Hash256([0; 32]),
            transaction_hash: Hash256([0; 32]),
            account_hash: Hash256([0; 32]),
            parent_close_time: 0,
            close_time: 10,
            close_time_resolution: 10,
            close_flags: 0,
        })
    }

    /// Token whose low-96 bits are `lo` repeated — keeps ordering obvious.
    fn tok(lo: u8, n: u8) -> serde_json::Value {
        let mut id = [0u8; 32];
        id[20] = lo;
        id[31] = n;
        serde_json::json!({ "NFToken": { "NFTokenID": hex::encode_upper(id) } })
    }

    /// A page that merely SHRANK is still consolidated with its neighbours.
    /// rippled removeToken: "The current page isn't empty. Update it and then
    /// try to consolidate pages." mergePages refuses when the two together
    /// exceed kDirMaxTokensPerPage, "since it only makes sense to do this if it
    /// would mean that one of them can be deleted as a result".
    ///
    /// #105952436 41292DD9E9B4: the seller's chain is 228A/233D/243B and the
    /// sold token lived in 243B. Losing it let 233D merge into 243B, so mainnet
    /// Deletes 233D and Modifies 228A — 9 nodes to our 7.
    #[test]
    fn a_shrunken_page_consolidates_with_its_neighbour() {
        let owner = [0x01u8; 20];
        let p1_key = page_key(&owner, &[0x50u8; 12]);
        let p2_key = max_page_key(&owner);

        // 5 + 28 = 33 > 32, so no merge is possible yet.
        let p1 = serde_json::json!({
            "LedgerEntryType": "NFTokenPage",
            "NFTokens": (0..5).map(|n| tok(0x10, n)).collect::<Vec<_>>(),
            "NextPageMin": hex::encode_upper(p2_key.0),
        });
        let p2 = serde_json::json!({
            "LedgerEntryType": "NFTokenPage",
            "NFTokens": (0..28).map(|n| tok(0x90, n)).collect::<Vec<_>>(),
            "PreviousPageMin": hex::encode_upper(p1_key.0),
        });
        let mut state = empty_state();
        state.state_map.insert(p1_key, serde_json::to_vec(&p1).unwrap()).unwrap();
        state.state_map.insert(p2_key, serde_json::to_vec(&p2).unwrap()).unwrap();

        // Remove one token from p2: 5 + 27 = 32, exactly the limit, so p1 folds
        // into p2 and is erased.
        let mut sb = Sandbox::new(&state);
        let victim = tok(0x90, 3);
        let vid = victim["NFToken"]["NFTokenID"].as_str().unwrap();
        let id = Hash256(<[u8; 32]>::try_from(hex::decode(vid).unwrap().as_slice()).unwrap());

        let removal = page_remove(&mut sb, &owner, &id).expect("token removed");
        assert!(!removal.page_deleted, "p2 still holds tokens");
        assert_eq!(removal.pages_merged, 1, "p1 must be consolidated away");
        assert!(!sb.exists(&p1_key), "the merged-away page is erased");

        let survivor: serde_json::Value =
            serde_json::from_slice(&sb.read(&p2_key).expect("p2 survives")).unwrap();
        assert_eq!(
            survivor["NFTokens"].as_array().map(|a| a.len()),
            Some(32),
            "p2 carries both pages' tokens",
        );
        assert!(
            survivor.get("PreviousPageMin").is_none(),
            "p1 had no previous, so p2's link is dropped",
        );
    }

    /// A FULL page is SPLIT before an insert — rippled `getPageForToken`. The
    /// first part moves to a NEW page keyed below it; the rest stays put, and
    /// the predecessor's NextPageMin is relinked.
    ///
    /// #105818293 012140AAC3A0 mints into rKqqb5QZ's full page: mainnet Creates
    /// a page and Modifies the neighbour (10 nodes to our 8) while we silently
    /// grew the page to 33 tokens — over kDirMaxTokensPerPage.
    #[test]
    fn a_full_page_splits_on_insert() {
        let owner = [0x03u8; 20];
        let max_key = max_page_key(&owner);

        // 32 tokens with DISTINCT low-96 bits, so the split lands mid-page.
        let full: Vec<serde_json::Value> = (0..PAGE_MAX as u8).map(|n| tok(n + 1, 0)).collect();
        let page = serde_json::json!({
            "LedgerEntryType": "NFTokenPage",
            "NFTokens": full,
        });
        let mut state = empty_state();
        state.state_map.insert(max_key, serde_json::to_vec(&page).unwrap()).unwrap();

        let mut sb = Sandbox::new(&state);
        // Low-96 above every existing token, so it belongs in the surviving page.
        let created = page_insert(&mut sb, &owner, tok(0xF0, 0));
        assert!(created, "a split creates a page, so the caller owes OwnerCount +1");

        let survivor: serde_json::Value =
            serde_json::from_slice(&sb.read(&max_key).expect("max page survives")).unwrap();
        let np_key = parse_page_ref(survivor.get("PreviousPageMin"))
            .expect("the survivor points back at the new page");
        let np: serde_json::Value =
            serde_json::from_slice(&sb.read(&np_key).expect("new page exists")).unwrap();

        let n_new = np["NFTokens"].as_array().unwrap().len();
        let n_old = survivor["NFTokens"].as_array().unwrap().len();
        assert_eq!(n_new + n_old, PAGE_MAX + 1, "all 33 tokens are still held");
        assert!(n_new <= PAGE_MAX && n_old <= PAGE_MAX, "neither page exceeds the limit");
        assert_eq!(
            parse_page_ref(np.get("NextPageMin")),
            Some(max_key),
            "the new page links forward to the page it split from",
        );
        assert!(np_key.0 < max_key.0, "the new page is keyed BELOW the one it split from");
    }

    /// The mirror: when the pages would not fit together, nothing merges.
    #[test]
    fn pages_that_do_not_fit_are_left_alone() {
        let owner = [0x02u8; 20];
        let p1_key = page_key(&owner, &[0x50u8; 12]);
        let p2_key = max_page_key(&owner);

        // 6 + 28 = 34; removing one leaves 33, still over the 32 limit.
        let p1 = serde_json::json!({
            "LedgerEntryType": "NFTokenPage",
            "NFTokens": (0..6).map(|n| tok(0x10, n)).collect::<Vec<_>>(),
            "NextPageMin": hex::encode_upper(p2_key.0),
        });
        let p2 = serde_json::json!({
            "LedgerEntryType": "NFTokenPage",
            "NFTokens": (0..28).map(|n| tok(0x90, n)).collect::<Vec<_>>(),
            "PreviousPageMin": hex::encode_upper(p1_key.0),
        });
        let mut state = empty_state();
        state.state_map.insert(p1_key, serde_json::to_vec(&p1).unwrap()).unwrap();
        state.state_map.insert(p2_key, serde_json::to_vec(&p2).unwrap()).unwrap();

        let mut sb = Sandbox::new(&state);
        let victim = tok(0x90, 3);
        let vid = victim["NFToken"]["NFTokenID"].as_str().unwrap();
        let id = Hash256(<[u8; 32]>::try_from(hex::decode(vid).unwrap().as_slice()).unwrap());

        let removal = page_remove(&mut sb, &owner, &id).expect("token removed");
        assert_eq!(removal.pages_merged, 0, "33 tokens do not fit in one page");
        assert!(sb.exists(&p1_key), "both pages survive");
    }
}
