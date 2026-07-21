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
        if prev_bound >= t && read_page(sandbox, &prev).is_some() {
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
            if let Some(arr) = arr {
                let pos = arr
                    .iter()
                    .position(|e| entry_id(e) > id_hex)
                    .unwrap_or(arr.len());
                arr.insert(pos, entry);
                sandbox.write(pk, serde_json::to_vec(&page).unwrap_or_default());
                return false;
            }
        }
    }
    // No pages yet — create the owner's max page.
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
pub struct PageRemoval {
    /// The removed entry, URI and all — reinsert it to transfer the token.
    pub entry: serde_json::Value,
    /// True if the page emptied and was deleted (caller owes OwnerCount -1).
    pub page_deleted: bool,
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
            return Some(PageRemoval { entry: removed, page_deleted: false });
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
        return Some(PageRemoval { entry: removed, page_deleted: true });
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
    let mut cur = max_page_key(owner);
    for _ in 0..100_000 {
        let page = read_page(sandbox, &cur)?;
        let found = page["NFTokens"]
            .as_array()
            .map(|a| a.iter().any(|e| entry_id(e) == id_hex))
            .unwrap_or(false);
        if found {
            return Some((cur, page));
        }
        cur = parse_page_ref(page.get("PreviousPageMin"))?;
    }
    None
}
