//! A transaction's own record of what it changed (independence plan, Step 2).
//!
//! rippled builds each transaction's metadata in `ApplyStateTable::apply` (src/libxrpl/ledger/
//! ApplyStateTable.cpp, 3.4.1) from two versions of every object the transaction changed: the one it started
//! from and the one it left. Task 2.2 captures both here, from the engine's sandbox: [`node_changes`] turns a
//! transaction's modifications into one [`NodeChange`] per object, in ledger-key order (the order
//! `TxMeta` sorts AffectedNodes into). The AffectedNodes builder (task 2.3) reads them with the field
//! flags in [`super::field_meta`].

use std::collections::HashMap;

use serde_json::Value;
use xrpl_core::types::Hash256;

use super::sandbox::{SandboxChanges, SandboxEntry};
use super::state::LedgerState;

/// What happened to one object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Created,
    Modified,
    Deleted,
}

/// One object a transaction changed, in engine JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeChange {
    pub key: Hash256,
    pub kind: NodeKind,
    /// The object as the transaction found it: the state it applied on. None when it created the object.
    pub before: Option<Value>,
    /// The object as the transaction left it, threading stamped. For a deleted object, its content at the
    /// deletion (rippled's DeletedNode FinalFields come from this version, not from `before`).
    pub after: Value,
    /// The transactor wrote this key, a write-back finding 335 drops as unchanged included. A key it did not
    /// write is one threadOwners threaded: an owner root of an object created or deleted.
    pub written: bool,
    /// What rippled's `sle->thread()` sees as the previous transaction (PreviousTxnID, PreviousTxnLgrSeq):
    /// the transactor's own copy of the object just before the stamp when it wrote one (None for a fresh
    /// copy, e.g. a SignerList replaced in place), else `before`'s. A ModifiedNode carries it at node level.
    pub thread_from: Option<(String, u32)>,
}

fn threading_of(v: &Value) -> Option<(String, u32)> {
    let id = v.get("PreviousTxnID")?.as_str()?.to_uppercase();
    let seq = v.get("PreviousTxnLgrSeq")?.as_u64()?;
    Some((id, u32::try_from(seq).ok()?))
}

/// The changes of one transaction: `stamped` is its modifications with this transaction's threading applied
/// (`threading::stamp_threading`), `unstamped` the sandbox's own record (`Sandbox::into_changes`), `base` the
/// state it applied on. Sorted by key. An entry whose bytes do not parse as JSON is left out (an engine
/// sandbox only ever holds JSON).
pub fn node_changes(base: &LedgerState, stamped: &HashMap<Hash256, SandboxEntry>, unstamped: &SandboxChanges) -> Vec<NodeChange> {
    let parse = |b: &[u8]| serde_json::from_slice::<Value>(b).ok();
    let mut keys: Vec<&Hash256> = stamped.keys().collect();
    keys.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        let before = base.read_json(key).as_deref().and_then(parse);
        let (kind, after) = match &stamped[key] {
            SandboxEntry::Created(b) => (NodeKind::Created, parse(b)),
            SandboxEntry::Modified(b) => (NodeKind::Modified, parse(b)),
            SandboxEntry::Deleted => (NodeKind::Deleted, unstamped.deleted_final.get(key).and_then(|b| parse(b)).or_else(|| before.clone())),
        };
        let Some(after) = after else { continue };
        // A write over an object the base does not hold is a creation, whatever the sandbox called it.
        let kind = if before.is_none() && kind == NodeKind::Modified { NodeKind::Created } else { kind };
        let own = unstamped.mods.get(key);
        let written = own.is_some() || unstamped.unchanged.contains(key);
        let thread_from = match own {
            Some(SandboxEntry::Created(b)) | Some(SandboxEntry::Modified(b)) => parse(b).as_ref().and_then(threading_of),
            Some(SandboxEntry::Deleted) => None,
            None => before.as_ref().and_then(threading_of),
        };
        out.push(NodeChange { key: *key, kind, before, after, written, thread_from });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::sandbox::Sandbox;
    use serde_json::json;

    fn state() -> LedgerState {
        LedgerState::new_unverified(LedgerHeader {
            sequence: 1,
            total_coins: 0,
            parent_hash: Hash256([0; 32]),
            transaction_hash: Hash256([0; 32]),
            account_hash: Hash256([0; 32]),
            parent_close_time: 0,
            close_time: 0,
            close_time_resolution: 10,
            close_flags: 0,
        })
    }

    fn key(n: u8) -> Hash256 {
        Hash256([n; 32])
    }

    #[test]
    fn each_change_carries_its_before_after_and_threading_in_key_order() {
        let mut st = state();
        let th = |id: &str, v: i64| json!({"LedgerEntryType": "Offer", "v": v, "PreviousTxnID": id, "PreviousTxnLgrSeq": 5});
        for (n, v) in [(1u8, 10), (2, 20), (3, 30), (4, 40)] {
            st.state_map.insert(key(n), th("AA", v).to_string().into_bytes()).unwrap();
        }
        let mut sb = Sandbox::new(&st);
        sb.write(key(9), json!({"LedgerEntryType": "Offer", "v": 90}).to_string().into_bytes());
        sb.write(key(2), th("AA", 21).to_string().into_bytes());
        sb.write(key(3), th("AA", 31).to_string().into_bytes());
        sb.delete(key(3)); // consumed, then deleted: its final content is the consumed one
        sb.write(key(1), th("AA", 10).to_string().into_bytes()); // unchanged
        let own = sb.into_changes();
        // The stamp: every changed write threaded with this transaction, and key 4 as a threadOwners root.
        let mut stamped = own.mods.clone();
        for k in [key(2), key(9)] {
            if let Some(SandboxEntry::Created(b) | SandboxEntry::Modified(b)) = stamped.get_mut(&k) {
                let mut v: Value = serde_json::from_slice(b).unwrap();
                v["PreviousTxnID"] = json!("BB");
                v["PreviousTxnLgrSeq"] = json!(6);
                *b = v.to_string().into_bytes();
            }
        }
        stamped.insert(key(4), SandboxEntry::Modified(th("BB", 40).to_string().into_bytes()));
        let ch = node_changes(&st, &stamped, &own);
        let got: Vec<(u8, NodeKind, Option<i64>, i64, bool, Option<(String, u32)>)> = ch
            .iter()
            .map(|c| (c.key.0[0], c.kind, c.before.as_ref().map(|b| b["v"].as_i64().unwrap()), c.after["v"].as_i64().unwrap(), c.written, c.thread_from.clone()))
            .collect();
        let aa = Some(("AA".to_string(), 5));
        assert_eq!(
            got,
            vec![
                (2, NodeKind::Modified, Some(20), 21, true, aa.clone()),
                (3, NodeKind::Deleted, Some(30), 31, true, None),
                (4, NodeKind::Modified, Some(40), 40, false, aa),
                (9, NodeKind::Created, None, 90, true, None),
            ],
            "an unchanged write-back is no change (finding 335); the deleted offer keeps its consumed content; a \
             threadOwners root is unwritten and threads from its before"
        );
        assert!(own.unchanged.contains(&key(1)));
    }
}
