//! Owner-directory bookkeeping — shared across object-creating transactors.
//!
//! Every account has an *owner directory*: a linked list of `DirectoryNode`
//! pages listing the ledger objects it owns (trust lines, offers, checks,
//! escrows, …). When an object is created it must be inserted into its
//! owner(s)' directory; when deleted, removed. rippled tracks this so the
//! reserve (via `OwnerCount`) and object enumeration stay correct.
//!
//! # DEAD CODE WARNING
//! Reference implementation only — the live validator applies via the FFI
//! (rippled) path. This exists so the native engine can reach mainnet parity.
//!
//! ## Scope (rewritten 2026-07-27 — the old note was stale)
//! Multi-page directories ARE handled: [`dir_insert`] follows the root's
//! `IndexPrevious` back-pointer to the tail page, appends there while it has
//! room, and on overflow writes a fresh `dir_page_key(root, n)` page with the
//! `IndexNext`/`IndexPrevious` relinking rippled does. [`dir_remove`] takes
//! `keep_root` to match rippled's `dirRemove` (ApplyView.cpp:279-331): the
//! page is erased-from and `update()`d — hence Modified — before an emptied
//! root is considered for deletion at all.
//!
//! The live constraint is **hydration, not paging**. These functions only see
//! what the caller loaded, so appending into a >32-object directory whose tail
//! page was never fetched invents a fresh root instead of Modifying the real
//! page (#105798519 CheckCash: the SAM issuer's 58-page dir, root D2215EC9,
//! tail BA1033D3). `differential_probe::load_owner_dir_tail` fetches that tail
//! for the already-loaded roots — currently wired for CheckCash only; extend
//! it for any transactor that appends into another account's directory.
//!
//! The differential gate compares mutation (key, kind) SETS, not DirectoryNode
//! byte content, so the `Indexes` array need only be *present*, not
//! byte-identical — what matters is that the node is touched at the right key
//! with the right kind (Created if new, Modified if it already existed).

use xrpl_core::types::Hash256;

use super::keylet;
use super::sandbox::Sandbox;

/// Max entries per DirectoryNode page (rippled `dirNodeMax`).
const DIR_MAX: usize = 32;

/// Parse a directory page number, which rippled encodes as a UInt64 (JSON:
/// number, or hex/decimal string depending on source).
fn page_num(v: &serde_json::Value) -> u64 {
    if let Some(n) = v.as_u64() {
        return n;
    }
    if let Some(s) = v.as_str() {
        return u64::from_str_radix(s, 16).ok().or_else(|| s.parse().ok()).unwrap_or(0);
    }
    0
}

/// Key of directory page `num` under `root` (page 0 is the root itself).
fn page_key(root: &Hash256, num: u64) -> Hash256 {
    if num == 0 {
        *root
    } else {
        keylet::dir_page_key(root, num)
    }
}

fn read_dir(sandbox: &Sandbox, key: &Hash256) -> Option<serde_json::Value> {
    sandbox
        .read(key)
        .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
}

fn new_page(owner: Option<&[u8; 20]>, root_key: &Hash256, entry: &str, prev: u64) -> serde_json::Value {
    let mut page = serde_json::json!({
        "LedgerEntryType": "DirectoryNode",
        "Flags": 0,
        "RootIndex": hex::encode(root_key.0),
        "Indexes": [entry],
        "IndexPrevious": prev,
        "IndexNext": 0,
    });
    if let Some(o) = owner {
        page["Owner"] = serde_json::Value::String(hex::encode(o));
    }
    page
}

/// Insert `object_key` into the directory rooted at `root_key` (owner dir OR an
/// NFT buy/sell offer dir — same page mechanics), walking the page chain
/// (rippled appends to the last page, spilling to a new page when full). Touches
/// exactly the pages mainnet does: append → the last page (Modified); overflow →
/// the last page (Modified) + root (Modified) + a new page (Created).
///
/// Requires the directory ROOT page to be present in the pre-state so the walk
/// can start (the differential harness loads it via `native_read_keys`). If the
/// root is absent, a fresh single-page directory is created. `owner` only
/// shapes new-page content (token dirs carry no Owner field).
pub fn dir_insert(
    sandbox: &mut Sandbox,
    root_key: &Hash256,
    owner: Option<&[u8; 20]>,
    object_key: &Hash256,
) {
    let root_key = *root_key;
    let entry = hex::encode_upper(object_key.0);

    let Some(mut root) = read_dir(sandbox, &root_key) else {
        // No directory yet — create the root page.
        sandbox.write(
            root_key,
            serde_json::to_vec(&new_page(owner, &root_key, &entry, 0)).unwrap_or_default(),
        );
        return;
    };

    // Walk to the last page via the root's back-pointer.
    let last_num = root.get("IndexPrevious").map(page_num).unwrap_or(0);
    let last_key = page_key(&root_key, last_num);
    let mut last = if last_num == 0 {
        root.clone()
    } else {
        read_dir(sandbox, &last_key).unwrap_or_else(|| root.clone())
    };

    let full = last
        .get("Indexes")
        .and_then(|v| v.as_array())
        .map(|a| a.len() >= DIR_MAX)
        .unwrap_or(false);

    if !full {
        // Append to the last page (Modified). If last IS root, this writes root.
        if let Some(arr) = last.get_mut("Indexes").and_then(|v| v.as_array_mut()) {
            if !arr.iter().any(|x| x.as_str().is_some_and(|s| s.eq_ignore_ascii_case(&entry))) {
                arr.push(serde_json::Value::String(entry));
            }
        }
        sandbox.write(last_key, serde_json::to_vec(&last).unwrap_or_default());
        return;
    }

    // Last page full — create a new page and relink.
    let new_num = last_num + 1;
    let new_key = keylet::dir_page_key(&root_key, new_num);
    sandbox.write(
        new_key,
        serde_json::to_vec(&new_page(owner, &root_key, &entry, last_num)).unwrap_or_default(),
    );
    if last_num == 0 {
        // Root was the last page: set both links on the single root object.
        root["IndexNext"] = serde_json::json!(new_num);
        root["IndexPrevious"] = serde_json::json!(new_num);
        sandbox.write(root_key, serde_json::to_vec(&root).unwrap_or_default());
    } else {
        last["IndexNext"] = serde_json::json!(new_num);
        sandbox.write(last_key, serde_json::to_vec(&last).unwrap_or_default());
        root["IndexPrevious"] = serde_json::json!(new_num);
        sandbox.write(root_key, serde_json::to_vec(&root).unwrap_or_default());
    }
}

/// Insert `object_key` into `owner`'s owner directory. See [`dir_insert`].
pub fn owner_dir_insert(sandbox: &mut Sandbox, owner: &[u8; 20], object_key: &Hash256) {
    dir_insert(sandbox, &keylet::owner_dir_key(owner), Some(owner), object_key)
}

/// Try to remove `entry` from page `num` of the directory rooted at `root_key`.
/// Returns true iff the entry was found (and removed) there. Handles page
/// emptying: root-only directory → delete root; empty non-root page → delete +
/// relink neighbours + fix root back-pointer.
fn try_remove_at(
    sandbox: &mut Sandbox,
    root_key: &Hash256,
    num: u64,
    entry: &str,
    keep_root: bool,
) -> bool {
    let cur_key = page_key(root_key, num);
    let Some(mut page) = read_dir(sandbox, &cur_key) else { return false };
    let has = page
        .get("Indexes")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().any(|x| x.as_str().is_some_and(|s| s.eq_ignore_ascii_case(entry))))
        .unwrap_or(false);
    if !has {
        return false;
    }
    if let Some(arr) = page.get_mut("Indexes").and_then(|v| v.as_array_mut()) {
        arr.retain(|x| !x.as_str().is_some_and(|s| s.eq_ignore_ascii_case(entry)));
    }
    let empty = page
        .get("Indexes")
        .and_then(|v| v.as_array())
        .map(|a| a.is_empty())
        .unwrap_or(true);
    if !empty || num == 0 {
        // An emptied ROOT page is only deleted when the caller did not ask to
        // keep it: "the root page will not be deleted even if it is empty,
        // unless keepRoot is not set and the directory is empty"
        // (ApplyView.h dirRemove doc; ApplyView.cpp:324 `if (keepRoot) return`).
        // With keep_root the page stays as a Modified empty directory.
        if empty && num == 0 && !keep_root && page.get("IndexNext").map(page_num).unwrap_or(0) == 0
        {
            sandbox.delete(*root_key); // only page, now empty → delete directory
        } else {
            sandbox.write(cur_key, serde_json::to_vec(&page).unwrap_or_default());
        }
        return true;
    }
    // Empty non-root page → delete it and relink the chain.
    let prev = page.get("IndexPrevious").map(page_num).unwrap_or(0);
    let next = page.get("IndexNext").map(page_num).unwrap_or(0);
    sandbox.delete(cur_key);
    if prev == 0 && next == 0 {
        // That was the only non-root page. If the root holds no entries of its
        // own the whole directory dies with it (rippled cascades the delete);
        // otherwise the root just drops its links.
        if let Some(mut root) = read_dir(sandbox, root_key) {
            let root_empty = root
                .get("Indexes")
                .and_then(|v| v.as_array())
                .map(|a| a.is_empty())
                .unwrap_or(true);
            if root_empty && !keep_root {
                sandbox.delete(*root_key);
            } else {
                root["IndexNext"] = serde_json::json!(0);
                root["IndexPrevious"] = serde_json::json!(0);
                sandbox.write(*root_key, serde_json::to_vec(&root).unwrap_or_default());
            }
        }
        return true;
    }
    let prev_key = page_key(root_key, prev);
    if let Some(mut pp) = read_dir(sandbox, &prev_key) {
        pp["IndexNext"] = serde_json::json!(next);
        sandbox.write(prev_key, serde_json::to_vec(&pp).unwrap_or_default());
    }
    if next != 0 {
        let nk = page_key(root_key, next);
        if let Some(mut np) = read_dir(sandbox, &nk) {
            np["IndexPrevious"] = serde_json::json!(prev);
            sandbox.write(nk, serde_json::to_vec(&np).unwrap_or_default());
        }
    }
    // If this was the last page, fix root's back-pointer.
    if let Some(mut root) = read_dir(sandbox, root_key) {
        if root.get("IndexPrevious").map(page_num).unwrap_or(0) == num {
            root["IndexPrevious"] = serde_json::json!(prev);
            sandbox.write(*root_key, serde_json::to_vec(&root).unwrap_or_default());
        }
    }
    true
}

/// Remove `object_key` from the directory rooted at `root_key` (owner dir OR
/// order-book quality dir — same page mechanics). rippled never walks the
/// chain for removal: every object stores its page number (OwnerNode/BookNode,
/// or LowNode/HighNode on a RippleState) and `dirRemove` jumps straight to
/// that page — pass it as `hint`. The hint path does not require the root page
/// to be present (for book dirs the hinted page often IS the root). Falls back
/// to last-page-then-chain-walk from the root when the hint is absent or
/// stale. No-op if the entry cannot be found.
pub fn dir_remove(
    sandbox: &mut Sandbox,
    root_key: &Hash256,
    object_key: &Hash256,
    hint: Option<u64>,
    keep_root: bool,
) {
    let entry = hex::encode_upper(object_key.0);
    if let Some(h) = hint {
        if try_remove_at(sandbox, root_key, h, &entry, keep_root) {
            return;
        }
    }
    let Some(root) = read_dir(sandbox, root_key) else { return };
    // No (valid) hint: try the LAST page first — root's back-pointer. Inserts
    // always append there, so entries added earlier in the same ledger (the
    // create→delete farm pattern) are found without traversing the chain.
    let last = root.get("IndexPrevious").map(page_num).unwrap_or(0);
    if last != 0 && hint != Some(last) && try_remove_at(sandbox, root_key, last, &entry, keep_root) {
        return;
    }
    let mut cur = 0u64;
    for _ in 0..100_000 {
        if try_remove_at(sandbox, root_key, cur, &entry, keep_root) {
            return;
        }
        let Some(page) = read_dir(sandbox, &page_key(root_key, cur)) else { return };
        let next = page.get("IndexNext").map(page_num).unwrap_or(0);
        if next == 0 {
            return;
        }
        cur = next;
    }
}

/// Remove `object_key` from `owner`'s owner directory. See [`dir_remove`].
pub fn owner_dir_remove(
    sandbox: &mut Sandbox,
    owner: &[u8; 20],
    object_key: &Hash256,
    hint: Option<u64>,
    keep_root: bool,
) {
    dir_remove(sandbox, &keylet::owner_dir_key(owner), object_key, hint, keep_root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::state::LedgerState;

    fn empty_state() -> LedgerState {
        LedgerState::new_unverified(LedgerHeader {
            sequence: 1,
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

    #[test]
    fn insert_creates_then_modifies_root_page() {
        let state = empty_state();
        let owner = [0x11u8; 20];
        let obj_a = Hash256([0xAAu8; 32]);
        let obj_b = Hash256([0xBBu8; 32]);
        let dir_key = keylet::owner_dir_key(&owner);

        let mods = {
            let mut sb = Sandbox::new(&state);
            owner_dir_insert(&mut sb, &owner, &obj_a); // creates the page
            owner_dir_insert(&mut sb, &owner, &obj_b); // appends
            sb.read(&dir_key).unwrap()
        };
        let dir: serde_json::Value = serde_json::from_slice(&mods).unwrap();
        assert_eq!(dir["LedgerEntryType"], "DirectoryNode");
        let idx = dir["Indexes"].as_array().unwrap();
        assert_eq!(idx.len(), 2);
        assert_eq!(idx[0].as_str().unwrap(), hex::encode_upper(obj_a.0));
        assert_eq!(idx[1].as_str().unwrap(), hex::encode_upper(obj_b.0));
    }

    #[test]
    fn remove_deletes_page_when_empty() {
        let state = empty_state();
        let owner = [0x22u8; 20];
        let obj = Hash256([0xCCu8; 32]);
        let dir_key = keylet::owner_dir_key(&owner);

        let mut sb = Sandbox::new(&state);
        owner_dir_insert(&mut sb, &owner, &obj);
        assert!(sb.read(&dir_key).is_some());
        owner_dir_remove(&mut sb, &owner, &obj, None, false);
        assert!(sb.read(&dir_key).is_none()); // empty → deleted
    }

    /// The mirror of the above: with `keep_root` the emptied root page survives
    /// as a Modified empty directory rather than being deleted — "the root page
    /// will not be deleted even if it is empty, unless keepRoot is not set"
    /// (ApplyView.h dirRemove doc). CheckCancel, CheckCash, OracleDelete and
    /// Ticket consumption all pass true; offers, NFT offers and trust lines
    /// pass false. Mainnet #105797946 emits the emptied dir root as Modified
    /// (`:1`); deleting it (`:2`) diverged on six transactions.
    #[test]
    fn remove_keeps_empty_root_when_asked() {
        let state = empty_state();
        let owner = [0x23u8; 20];
        let obj = Hash256([0xDDu8; 32]);
        let dir_key = keylet::owner_dir_key(&owner);

        let mut sb = Sandbox::new(&state);
        owner_dir_insert(&mut sb, &owner, &obj);
        owner_dir_remove(&mut sb, &owner, &obj, None, true);

        let page = sb.read(&dir_key).expect("root page must survive keep_root");
        let page: serde_json::Value = serde_json::from_slice(&page).unwrap();
        assert_eq!(
            page.get("Indexes").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(0),
            "root should remain, emptied"
        );
    }
}
