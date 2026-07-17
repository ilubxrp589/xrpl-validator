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
//! ## Scope (2026-07-17, first slice)
//! Handles the **single root page** case (accounts with ≤ 32 owned objects,
//! the large majority). Multi-page directories — where objects spill onto
//! `dir_page_key(root, n)` pages with `IndexNext`/`IndexPrevious` links, and
//! page splitting on overflow — are NOT yet handled; an object for a >32-object
//! account will be placed on the root page here, diverging from mainnet's
//! actual page. That is the remaining owner-directory work.
//!
//! The differential gate compares mutation (key, kind) SETS, not DirectoryNode
//! byte content, so the `Indexes` array need only be *present*, not
//! byte-identical — what matters is that the node is touched at the right key
//! with the right kind (Created if new, Modified if it already existed).

use xrpl_core::types::Hash256;

use super::keylet;
use super::sandbox::Sandbox;

/// Insert `object_key` into `owner`'s owner directory (single root page).
/// Creates the `DirectoryNode` if absent (→ Created mutation), else appends
/// (→ Modified mutation).
pub fn owner_dir_insert(sandbox: &mut Sandbox, owner: &[u8; 20], object_key: &Hash256) {
    let dir_key = keylet::owner_dir_key(owner);
    let entry = hex::encode(object_key.0);
    match sandbox
        .read(&dir_key)
        .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
    {
        Some(mut dir) => {
            if let Some(arr) = dir.get_mut("Indexes").and_then(|v| v.as_array_mut()) {
                if !arr.iter().any(|x| x.as_str() == Some(entry.as_str())) {
                    arr.push(serde_json::Value::String(entry));
                }
            }
            sandbox.write(dir_key, serde_json::to_vec(&dir).unwrap_or_default());
        }
        None => {
            let dir = serde_json::json!({
                "LedgerEntryType": "DirectoryNode",
                "Flags": 0,
                "Owner": hex::encode(owner),
                "RootIndex": hex::encode(dir_key.0),
                "Indexes": [entry],
            });
            sandbox.write(dir_key, serde_json::to_vec(&dir).unwrap_or_default());
        }
    }
}

/// Remove `object_key` from `owner`'s owner directory (single root page). If the
/// page becomes empty it is deleted (→ Deleted mutation); otherwise the entry is
/// dropped (→ Modified). No-op if the directory or entry is absent.
pub fn owner_dir_remove(sandbox: &mut Sandbox, owner: &[u8; 20], object_key: &Hash256) {
    let dir_key = keylet::owner_dir_key(owner);
    let entry = hex::encode(object_key.0);
    let Some(mut dir) = sandbox
        .read(&dir_key)
        .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
    else {
        return;
    };
    let mut now_empty = false;
    if let Some(arr) = dir.get_mut("Indexes").and_then(|v| v.as_array_mut()) {
        arr.retain(|x| x.as_str() != Some(entry.as_str()));
        now_empty = arr.is_empty();
    }
    if now_empty {
        sandbox.delete(dir_key);
    } else {
        sandbox.write(dir_key, serde_json::to_vec(&dir).unwrap_or_default());
    }
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
        assert_eq!(idx[0].as_str().unwrap(), hex::encode(obj_a.0));
        assert_eq!(idx[1].as_str().unwrap(), hex::encode(obj_b.0));
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
        owner_dir_remove(&mut sb, &owner, &obj);
        assert!(sb.read(&dir_key).is_none()); // empty → deleted
    }
}
