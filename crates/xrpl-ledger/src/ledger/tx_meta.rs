//! A transaction's own record of what it changed (independence plan, Step 2).
//!
//! rippled builds each transaction's metadata in `ApplyStateTable::apply` (src/libxrpl/ledger/
//! ApplyStateTable.cpp, 3.4.1) from two versions of every object the transaction changed: the one it started
//! from and the one it left. Task 2.2 captures both here, from the engine's sandbox: [`node_changes`] turns a
//! transaction's modifications into one [`NodeChange`] per object, in ledger-key order (the order
//! `TxMeta` sorts AffectedNodes into). The AffectedNodes builder (task 2.3) reads them with the field
//! flags in [`super::field_meta`].

use std::collections::HashMap;

use serde_json::{Map, Value};
use xrpl_core::types::Hash256;

use super::field_meta::{self, Section};
use super::sandbox::{SandboxChanges, SandboxEntry};
use super::state::LedgerState;
use super::threading::{canon_object, equal_but_threading, owner_root_keys, same_field};

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

/// Keys an engine or decoder object can carry that are not fields of the object: the client-side `index`, and
/// `LedgerIndex`, which rippled puts on the AffectedNodes entry itself and never inside a section.
const NOT_OBJECT_FIELDS: [&str; 2] = ["index", "LedgerIndex"];

/// rippled's AffectedNodes for one transaction's changes (independence task 2.3), in rippled's JSON shape and
/// in key order (`TxMeta::addRaw` sorts by LedgerIndex). The rules are `ApplyStateTable::apply`'s
/// (src/libxrpl/ledger/ApplyStateTable.cpp:131-245 in 3.4.1) with the flags in [`super::field_meta`]:
/// - CreatedNode: NewFields hold every `kSmdCreate | kSmdAlways` field whose value is not the default.
/// - ModifiedNode: PreviousFields hold the old value of every `kSmdChangeOrig` field of `before` with no
///   equal entry in `after`, a removed field included. A `kSmdChangeOrig` field the transaction added lists
///   nothing, but still makes the section, empty: rippled walks the old object's template, absent fields
///   included. FinalFields hold every `kSmdAlways | kSmdChangeNew` field of `after`, defaults kept. The node
///   carries the PreviousTxnID/PreviousTxnLgrSeq the object had just before this transaction threaded it
///   (`threadItem`); a fresh SLE has none.
/// - An owner only re-threaded (`threadOwners`) is a ModifiedNode with no sections. The exception is an owner
///   the transactor wrote back unchanged when a created or deleted node sorting before it threads it: rippled
///   threads it before its loop reaches it, the loop then sees a change, and the node gets its FinalFields
///   (mainnet #106758384 F578BE463712).
/// - DeletedNode: PreviousFields as for a ModifiedNode; FinalFields hold every `kSmdAlways | kSmdDeleteFinal`
///   field of the object as the transaction deleted it. No node-level threading.
/// - An empty section is left out, apart from the added-field PreviousFields above.
///
/// Values keep the spelling of `changes` (engine JSON), so compare the output with mainnet's canonically.
pub fn affected_nodes(changes: &[NodeChange]) -> Vec<Value> {
    let mut sorted: Vec<&NodeChange> = changes.iter().collect();
    sorted.sort_by(|a, b| a.key.0.cmp(&b.key.0));
    sorted.iter().filter_map(|c| affected_node(c, &sorted)).collect()
}

/// AffectedNodes re-spelled so that equal values compare equal, whoever wrote them: engine JSON (hex accounts,
/// pointers as numbers) and mainnet's (r-addresses, hex pointers) meet in the spelling finding 335's filter
/// compares in. For diffing [`affected_nodes`] against mainnet's records (dp, task 2.6); the byte-exact
/// comparison is the encoding's (2.5).
pub fn canonical_affected_nodes(nodes: &[Value]) -> Vec<Value> {
    nodes
        .iter()
        .map(|n| {
            let mut n = n.clone();
            if let Some(inner) = n.as_object_mut().and_then(|o| o.values_mut().next()) {
                if let Some(fields) = inner.as_object_mut() {
                    for s in ["PreviousFields", "FinalFields", "NewFields"] {
                        if let Some(v) = fields.get_mut(s) {
                            canon_object(v);
                        }
                    }
                }
                canon_object(inner);
            }
            n
        })
        .collect()
}

fn affected_node(c: &NodeChange, all: &[&NodeChange]) -> Option<Value> {
    let entry_type = c.after.get("LedgerEntryType").or_else(|| c.before.as_ref()?.get("LedgerEntryType"));
    let mut node = Map::new();
    node.insert("LedgerEntryType".into(), entry_type.cloned().unwrap_or(Value::Null));
    node.insert("LedgerIndex".into(), Value::String(hex::encode_upper(c.key.0)));
    let kind = match (c.kind, &c.before) {
        (NodeKind::Deleted, before) => {
            if let Some(before) = before {
                previous_fields(&mut node, before, &c.after);
            }
            section(&mut node, "FinalFields", &c.after, Section::DeletedFinalFields);
            "DeletedNode"
        }
        (NodeKind::Modified, Some(before)) => {
            let threaded = thread(&mut node, c);
            if equal_but_threading(before, &c.after) && !rewritten(c, before) {
                if c.written && threaded_before_reached(c, all) {
                    section(&mut node, "FinalFields", &c.after, Section::ModifiedFinalFields);
                } else if !threaded {
                    return None; // rippled emits an untouched object only through threadItem's node threading
                }
            } else {
                previous_fields(&mut node, before, &c.after);
                section(&mut node, "FinalFields", &c.after, Section::ModifiedFinalFields);
            }
            "ModifiedNode"
        }
        // A write over an object the base did not hold is a creation (node_changes already says so).
        (NodeKind::Created, _) | (NodeKind::Modified, None) => {
            section(&mut node, "NewFields", &c.after, Section::NewFields);
            "CreatedNode"
        }
    };
    let mut out = Map::new();
    out.insert(kind.into(), Value::Object(node));
    Some(Value::Object(out))
}

/// `threadItem` on a ModifiedNode: the threading the object carried just before this transaction's stamp, when
/// it had any. Whether the node got it.
fn thread(node: &mut Map<String, Value>, c: &NodeChange) -> bool {
    match &c.thread_from {
        Some((id, seq)) if !all_zeros(id) => {
            node.insert("PreviousTxnID".into(), Value::String(id.clone()));
            node.insert("PreviousTxnLgrSeq".into(), Value::from(*seq));
            true
        }
        _ => false,
    }
}

/// Whether the transactor's copy of a threading-only change was not the object as it found it: a fresh SLE
/// (deleted and written again) carries no threading of its own, so rippled's loop sees it as changed.
fn rewritten(c: &NodeChange, before: &Value) -> bool {
    c.written && threading_of(before) != c.thread_from
}

/// Whether a created or deleted node sorting before `c` names it as an owner (`threadOwners` threads the
/// created node's new image and the deleted node's old one).
fn threaded_before_reached(c: &NodeChange, all: &[&NodeChange]) -> bool {
    all.iter().take_while(|t| t.key.0 < c.key.0).any(|t| {
        let image = match t.kind {
            NodeKind::Created => Some(&t.after),
            NodeKind::Deleted => t.before.as_ref(),
            NodeKind::Modified => None,
        };
        image.is_some_and(|i| owner_root_keys(i).contains(&c.key))
    })
}

fn previous_fields(node: &mut Map<String, Value>, before: &Value, after: &Value) {
    let (Some(old), Some(new)) = (before.as_object(), after.as_object()) else { return };
    let mut prevs = Map::new();
    for (name, value) in old {
        if changes_orig(name) && !new.get(name).is_some_and(|v| same_field(name, value, v)) {
            prevs.insert(name.clone(), value.clone());
        }
    }
    let added = new.keys().any(|name| !old.contains_key(name) && changes_orig(name));
    if !prevs.is_empty() || added {
        node.insert("PreviousFields".into(), Value::Object(prevs));
    }
}

fn changes_orig(name: &str) -> bool {
    !NOT_OBJECT_FIELDS.contains(&name) && field_meta::by_name(name).is_some_and(|f| f.allowed_in(Section::PreviousFields))
}

fn section(node: &mut Map<String, Value>, name: &str, object: &Value, which: Section) {
    let Some(fields) = object.as_object() else { return };
    let mut out = Map::new();
    for (field, value) in fields {
        if NOT_OBJECT_FIELDS.contains(&field.as_str()) {
            continue;
        }
        let Some(f) = field_meta::by_name(field) else { continue };
        if f.allowed_in(which) && !(which == Section::NewFields && is_default(f.type_code, value)) {
            out.insert(field.clone(), value.clone());
        }
    }
    if !out.is_empty() {
        node.insert(name.into(), Value::Object(out));
    }
}

/// rippled's `isDefault` for a value of a field of this serialized type (`STI_*`); NewFields leaves defaults
/// out. A zero integer, hash or Number; XRP zero (an IOU or MPT amount never is, zero or not:
/// `STAmount::isDefault`); an empty blob, array, object, Vector256 or path set; the XRP issue or currency. An
/// account that is present is set, so never the default (`STAccount::default_`), and neither is a bridge,
/// whose doors are accounts.
fn is_default(type_code: u16, v: &Value) -> bool {
    match type_code {
        1..=5 | 10 | 11 | 16 | 17 | 20..=23 => is_zero(v),
        6 => !v.is_object() && is_zero(v),
        7 | 8 => v.as_str() == Some(""),
        9 => match v {
            Value::Number(n) => n.as_f64() == Some(0.0),
            Value::String(s) => {
                let mantissa = s.trim_start_matches('-').split(['e', 'E']).next().unwrap_or("");
                mantissa.bytes().any(|b| b == b'0') && mantissa.bytes().all(|b| b == b'0' || b == b'.')
            }
            _ => false,
        },
        14 => v.as_object().is_some_and(Map::is_empty),
        15 | 18 | 19 => v.as_array().is_some_and(Vec::is_empty),
        24 => v.as_object().is_some_and(|o| {
            !o.contains_key("issuer") && !o.contains_key("mpt_issuance_id") && o.get("currency").is_some_and(is_xrp_currency)
        }),
        26 => is_xrp_currency(v),
        _ => false,
    }
}

fn is_zero(v: &Value) -> bool {
    match v {
        Value::Number(n) => n.as_u64() == Some(0) || n.as_i64() == Some(0),
        Value::String(s) => all_zeros(s),
        _ => false,
    }
}

/// A decimal or hex spelling of zero.
fn all_zeros(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b == b'0')
}

fn is_xrp_currency(v: &Value) -> bool {
    v.as_str().is_some_and(|s| s == "XRP" || (s.len() == 40 && all_zeros(s)))
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

#[cfg(test)]
mod affected_nodes_tests {
    use super::*;
    use serde_json::json;

    const A: &str = "rUgxCk4Z1S9DZEtS7aWoWtAc4KQSgHL9JL";
    const B: &str = "rRKNF5Svv8k13vZ4R9o4xrkUmf5b4i52B";
    /// This transaction.
    const T: u8 = 0xAA;

    fn key(n: u8) -> Hash256 {
        Hash256([n; 32])
    }

    /// A 64-hex transaction id.
    fn txn(n: u8) -> String {
        format!("{n:02X}").repeat(32)
    }

    /// The object as the stamp leaves it: threaded with this transaction.
    fn stamped(mut v: Value) -> Value {
        v["PreviousTxnID"] = json!(txn(T));
        v["PreviousTxnLgrSeq"] = json!(200);
        v
    }

    fn root(account: &str, balance: &str, sequence: u32, prev: u8, prev_seq: u32) -> Value {
        json!({"LedgerEntryType": "AccountRoot", "Account": account, "Balance": balance, "Flags": 0, "OwnerCount": 0,
               "Sequence": sequence, "PreviousTxnID": txn(prev), "PreviousTxnLgrSeq": prev_seq})
    }

    fn offer(gets: &str, pays: &str, prev: u8) -> Value {
        json!({"LedgerEntryType": "Offer", "Account": A, "BookDirectory": "DD".repeat(32), "BookNode": "0", "Flags": 0,
               "OwnerNode": "0", "Sequence": 7, "TakerGets": gets,
               "TakerPays": {"currency": "USD", "issuer": B, "value": pays},
               "PreviousTxnID": txn(prev), "PreviousTxnLgrSeq": 120})
    }

    /// A change the transactor wrote, threading from the object as it found it.
    fn modified(key: Hash256, before: Value, after: Value) -> NodeChange {
        let thread_from = threading_of(&before);
        NodeChange { key, kind: NodeKind::Modified, before: Some(before), after, written: true, thread_from }
    }

    fn change(key: Hash256, kind: NodeKind, before: Option<Value>, after: Value) -> NodeChange {
        NodeChange { key, kind, before, after, written: true, thread_from: None }
    }

    /// The expected AffectedNodes entry: `{kind: {LedgerEntryType, LedgerIndex, ..rest}}`.
    fn node(kind: &str, key: &Hash256, entry_type: &str, rest: Value) -> Value {
        let mut inner = Map::new();
        inner.insert("LedgerEntryType".into(), json!(entry_type));
        inner.insert("LedgerIndex".into(), json!(hex::encode_upper(key.0)));
        inner.extend(rest.as_object().unwrap().clone());
        let mut outer = Map::new();
        outer.insert(kind.into(), Value::Object(inner));
        Value::Object(outer)
    }

    #[test]
    fn a_payment_modifies_two_account_roots_in_key_order() {
        let a0 = root(A, "1000000", 5, 0x11, 100);
        let mut a1 = a0.clone();
        a1["Balance"] = json!("899988");
        a1["Sequence"] = json!(6);
        let b0 = root(B, "50000000", 9, 0x22, 150);
        let mut b1 = b0.clone();
        b1["Balance"] = json!("50100000");
        let got = affected_nodes(&[modified(key(9), a0, stamped(a1)), modified(key(3), b0, stamped(b1))]);
        assert_eq!(
            got,
            vec![
                node("ModifiedNode", &key(3), "AccountRoot", json!({
                    "FinalFields": {"Account": B, "Balance": "50100000", "Flags": 0, "OwnerCount": 0, "Sequence": 9},
                    "PreviousFields": {"Balance": "50000000"},
                    "PreviousTxnID": txn(0x22), "PreviousTxnLgrSeq": 150})),
                node("ModifiedNode", &key(9), "AccountRoot", json!({
                    "FinalFields": {"Account": A, "Balance": "899988", "Flags": 0, "OwnerCount": 0, "Sequence": 6},
                    "PreviousFields": {"Balance": "1000000", "Sequence": 5},
                    "PreviousTxnID": txn(0x11), "PreviousTxnLgrSeq": 100})),
            ]
        );
    }

    #[test]
    fn an_offer_partly_consumed_lists_its_old_amounts_and_keeps_its_zero_pointers() {
        let got = affected_nodes(&[modified(key(5), offer("1000000", "10", 0x33), stamped(offer("600000", "6", 0x33)))]);
        assert_eq!(
            got,
            vec![node("ModifiedNode", &key(5), "Offer", json!({
                "FinalFields": {"Account": A, "BookDirectory": "DD".repeat(32), "BookNode": "0", "Flags": 0,
                                "OwnerNode": "0", "Sequence": 7, "TakerGets": "600000",
                                "TakerPays": {"currency": "USD", "issuer": B, "value": "6"}},
                "PreviousFields": {"TakerGets": "1000000", "TakerPays": {"currency": "USD", "issuer": B, "value": "10"}},
                "PreviousTxnID": txn(0x33), "PreviousTxnLgrSeq": 120}))]
        );
    }

    #[test]
    fn an_offer_consumed_then_deleted_shows_what_it_held_at_the_deletion() {
        let mut c = change(key(5), NodeKind::Deleted, Some(offer("1000000", "10", 0x33)), offer("0", "0", 0x33));
        c.thread_from = threading_of(c.before.as_ref().unwrap()); // never used for a deleted node
        assert_eq!(
            affected_nodes(&[c]),
            vec![node("DeletedNode", &key(5), "Offer", json!({
                "PreviousFields": {"TakerGets": "1000000", "TakerPays": {"currency": "USD", "issuer": B, "value": "10"}},
                "FinalFields": {"Account": A, "BookDirectory": "DD".repeat(32), "BookNode": "0", "Flags": 0,
                                "OwnerNode": "0", "Sequence": 7, "TakerGets": "0",
                                "TakerPays": {"currency": "USD", "issuer": B, "value": "0"},
                                "PreviousTxnID": txn(0x33), "PreviousTxnLgrSeq": 120}}))]
        );
    }

    #[test]
    fn a_trust_line_created_leaves_its_defaults_out() {
        let line = stamped(json!({"LedgerEntryType": "RippleState",
            "Balance": {"currency": "USD", "issuer": "rrrrrrrrrrrrrrrrrrrrBZbvji", "value": "0"},
            "Flags": 131072, "HighLimit": {"currency": "USD", "issuer": B, "value": "0"}, "HighNode": "0",
            "LowLimit": {"currency": "USD", "issuer": A, "value": "100"}, "LowNode": "0", "LowQualityIn": 0}));
        assert_eq!(
            affected_nodes(&[change(key(7), NodeKind::Created, None, line)]),
            vec![node("CreatedNode", &key(7), "RippleState", json!({"NewFields": {
                "Balance": {"currency": "USD", "issuer": "rrrrrrrrrrrrrrrrrrrrBZbvji", "value": "0"},
                "Flags": 131072, "HighLimit": {"currency": "USD", "issuer": B, "value": "0"},
                "LowLimit": {"currency": "USD", "issuer": A, "value": "100"}}}))],
            "an IOU zero is not a default (STAmount::isDefault); zero pointers, a zero quality and the stamp are left out"
        );
    }

    #[test]
    fn an_owner_only_rethreaded_has_no_sections() {
        let before = root(B, "50000000", 9, 0x44, 130);
        let mut c = modified(key(4), before.clone(), stamped(before));
        c.written = false;
        assert_eq!(
            affected_nodes(&[c]),
            vec![node("ModifiedNode", &key(4), "AccountRoot", json!({"PreviousTxnID": txn(0x44), "PreviousTxnLgrSeq": 130}))]
        );
    }

    /// Mainnet #106758384 F578BE463712: the owner root a deleted escrow threads, written back unchanged by the
    /// transactor, has its FinalFields only when the escrow sorts before it.
    #[test]
    fn an_owner_written_back_unchanged_gets_final_fields_only_when_threaded_first() {
        let owner_key = crate::ledger::keylet::account_root_key(&crate::tx::offer::decode20(B).unwrap());
        let escrow = json!({"LedgerEntryType": "Escrow", "Account": A, "Destination": B, "Amount": "1000", "Flags": 0,
                            "OwnerNode": "0", "DestinationNode": "0", "PreviousTxnID": txn(0x55), "PreviousTxnLgrSeq": 140});
        let owner = root(B, "50000000", 9, 0x66, 141);
        let owner_node = |escrow_key: Hash256, written: bool| {
            let mut o = modified(owner_key, owner.clone(), stamped(owner.clone()));
            o.written = written;
            let gone = change(escrow_key, NodeKind::Deleted, Some(escrow.clone()), escrow.clone());
            affected_nodes(&[gone, o]).into_iter().find(|n| n["ModifiedNode"]["LedgerIndex"] == hex::encode_upper(owner_key.0))
        };
        let threading = json!({"PreviousTxnID": txn(0x66), "PreviousTxnLgrSeq": 141});
        let mut with_finals = threading.clone();
        with_finals["FinalFields"] = json!({"Account": B, "Balance": "50000000", "Flags": 0, "OwnerCount": 0, "Sequence": 9});
        assert_eq!(owner_node(Hash256([0x00; 32]), true), Some(node("ModifiedNode", &owner_key, "AccountRoot", with_finals)));
        assert_eq!(owner_node(Hash256([0xFF; 32]), true), Some(node("ModifiedNode", &owner_key, "AccountRoot", threading.clone())));
        assert_eq!(owner_node(Hash256([0x00; 32]), false), Some(node("ModifiedNode", &owner_key, "AccountRoot", threading)));
    }

    #[test]
    fn a_removed_field_is_listed_with_its_old_value() {
        let mut before = root(A, "1000000", 5, 0x11, 100);
        before["Domain"] = json!("6578616D706C652E636F6D");
        let mut after = root(A, "999988", 6, 0x11, 100);
        after.as_object_mut().unwrap().remove("Domain");
        let got = affected_nodes(&[modified(key(2), before, stamped(after))]);
        assert_eq!(
            got[0]["ModifiedNode"]["PreviousFields"],
            json!({"Balance": "1000000", "Domain": "6578616D706C652E636F6D", "Sequence": 5})
        );
        assert!(got[0]["ModifiedNode"]["FinalFields"].get("Domain").is_none());
    }

    /// 154 nodes in the s2 answer key, e.g. TicketCreate #106679743 6FD6E6555936: a directory page that gained
    /// IndexNext lists no previous value, but carries an empty PreviousFields.
    #[test]
    fn an_added_field_alone_still_makes_an_empty_previous_fields() {
        let page = json!({"LedgerEntryType": "DirectoryNode", "Flags": 0, "Owner": A, "RootIndex": "EE".repeat(32),
                          "Indexes": ["01".repeat(32)], "PreviousTxnID": txn(0x77), "PreviousTxnLgrSeq": 150});
        let mut grown = page.clone();
        grown["IndexNext"] = json!("1");
        grown["Indexes"] = json!(["01".repeat(32), "02".repeat(32)]);
        assert_eq!(
            affected_nodes(&[modified(key(6), page, stamped(grown))]),
            vec![node("ModifiedNode", &key(6), "DirectoryNode", json!({
                "FinalFields": {"Flags": 0, "IndexNext": "1", "Owner": A, "RootIndex": "EE".repeat(32)},
                "PreviousFields": {},
                "PreviousTxnID": txn(0x77), "PreviousTxnLgrSeq": 150}))]
        );
    }

    #[test]
    fn a_page_whose_only_change_is_its_indexes_shows_final_fields_only() {
        let page = json!({"LedgerEntryType": "DirectoryNode", "Flags": 0, "Owner": A, "RootIndex": "EE".repeat(32),
                          "Indexes": ["01".repeat(32)], "PreviousTxnID": txn(0x88), "PreviousTxnLgrSeq": 151});
        let mut grown = page.clone();
        grown["Indexes"] = json!(["01".repeat(32), "02".repeat(32)]);
        grown["index"] = json!("06".repeat(32)); // a client-side key, not a field
        grown["LedgerIndex"] = json!("06".repeat(32));
        assert_eq!(
            affected_nodes(&[modified(key(6), page, stamped(grown))]),
            vec![node("ModifiedNode", &key(6), "DirectoryNode", json!({
                "FinalFields": {"Flags": 0, "Owner": A, "RootIndex": "EE".repeat(32)},
                "PreviousTxnID": txn(0x88), "PreviousTxnLgrSeq": 151}))],
            "Indexes is kSmdNever, and index/LedgerIndex are not fields of the object"
        );
    }

    /// Mainnet #106739411 780DFE9BF728: a SignerListSet replacing a list writes a fresh SignerList, and the
    /// owner directory root is re-created too. Fresh copies carry no threading, so neither node is threaded.
    #[test]
    fn a_fresh_copy_carries_no_node_threading() {
        let list = |entries: Value, prev: Option<u8>| {
            let mut v = json!({"LedgerEntryType": "SignerList", "Flags": 65536, "Owner": A, "OwnerNode": "0",
                               "SignerEntries": entries, "SignerListID": 0, "SignerQuorum": 2});
            if let Some(p) = prev {
                v["PreviousTxnID"] = json!(txn(p));
                v["PreviousTxnLgrSeq"] = json!(152);
            }
            v
        };
        let old = json!([{"SignerEntry": {"Account": A, "SignerWeight": 1}}]);
        let new = json!([{"SignerEntry": {"Account": B, "SignerWeight": 1}}]);
        let mut replaced = modified(key(8), list(old.clone(), Some(0x99)), stamped(list(new.clone(), None)));
        replaced.thread_from = None; // the transactor's copy is fresh
        let page = json!({"LedgerEntryType": "DirectoryNode", "Flags": 0, "Owner": A, "RootIndex": "0B".repeat(32),
                          "Indexes": ["08".repeat(32)], "PreviousTxnID": txn(0x9A), "PreviousTxnLgrSeq": 153});
        let mut root_page = modified(key(0x0B), page.clone(), stamped(page));
        root_page.thread_from = None;
        assert_eq!(
            affected_nodes(&[replaced, root_page]),
            vec![
                node("ModifiedNode", &key(8), "SignerList", json!({
                    "FinalFields": {"Flags": 65536, "Owner": A, "OwnerNode": "0", "SignerEntries": new,
                                    "SignerListID": 0, "SignerQuorum": 2},
                    "PreviousFields": {"SignerEntries": old}})),
                node("ModifiedNode", &key(0x0B), "DirectoryNode", json!({
                    "FinalFields": {"Flags": 0, "Owner": A, "RootIndex": "0B".repeat(32)}})),
            ]
        );
    }

    #[test]
    fn the_same_value_in_another_spelling_is_no_change() {
        let hex_a = hex::encode_upper(crate::tx::offer::decode20(A).unwrap());
        let mut before = root(A, "1000000", 5, 0x11, 100);
        before["OwnerNode"] = json!("0000000000000000");
        let mut after = root(&hex_a, "999988", 5, 0x11, 100);
        after["OwnerNode"] = json!("0");
        let got = affected_nodes(&[modified(key(2), before, stamped(after))]);
        assert_eq!(got[0]["ModifiedNode"]["PreviousFields"], json!({"Balance": "1000000"}));
    }

    #[test]
    fn engine_and_mainnet_spellings_meet_in_the_canonical_form() {
        let hex_a = hex::encode_upper(crate::tx::offer::decode20(A).unwrap());
        let ours = node("ModifiedNode", &key(0xAB), "AccountRoot", json!({
            "FinalFields": {"Account": hex_a, "Balance": "999988", "OwnerNode": "0000000000000000"},
            "PreviousFields": {"Balance": "1000000"},
            "PreviousTxnID": txn(0xCD).to_lowercase(), "PreviousTxnLgrSeq": 100}));
        let theirs = |balance: &str| node("ModifiedNode", &key(0xAB), "AccountRoot", json!({
            "FinalFields": {"Account": A, "Balance": balance, "OwnerNode": "0"},
            "PreviousFields": {"Balance": "1000000"},
            "PreviousTxnID": txn(0xCD), "PreviousTxnLgrSeq": 100}));
        assert_ne!(ours, theirs("999988"));
        assert_eq!(canonical_affected_nodes(std::slice::from_ref(&ours)), canonical_affected_nodes(&[theirs("999988")]));
        assert_ne!(canonical_affected_nodes(&[ours]), canonical_affected_nodes(&[theirs("999989")]));
    }

    #[test]
    fn defaults_follow_rippled_is_default() {
        assert!(is_default(2, &json!(0)) && !is_default(2, &json!(5)));
        assert!(is_default(3, &json!("0000000000000000")) && is_default(3, &json!("0")) && !is_default(3, &json!("10")));
        assert!(is_default(5, &json!("0".repeat(64))) && !is_default(5, &json!("0".repeat(63) + "1")));
        assert!(is_default(6, &json!("0")), "XRP zero");
        assert!(!is_default(6, &json!({"currency": "USD", "issuer": A, "value": "0"})), "an IOU zero is not");
        assert!(!is_default(6, &json!({"mpt_issuance_id": "00".repeat(24), "value": "0"})), "nor an MPT zero");
        assert!(is_default(7, &json!("")) && !is_default(7, &json!("00")));
        assert!(!is_default(8, &json!(A)), "a present account is set");
        assert!(is_default(9, &json!("0")) && is_default(9, &json!("0e-5")) && !is_default(9, &json!("1e-3")));
        assert!(is_default(14, &json!({})) && is_default(15, &json!([])) && is_default(19, &json!([])));
        assert!(is_default(24, &json!({"currency": "XRP"})) && !is_default(24, &json!({"currency": "USD", "issuer": A})));
        assert!(is_default(26, &json!("XRP")) && !is_default(26, &json!("USD")));
    }

    /// A mainnet AffectedNodes entry rebuilt as the change that made it. `after` is the listed fields
    /// (FinalFields or NewFields) with the entry type and this transaction's stamp; `before` is `after` with
    /// PreviousFields' old values put back and the node's threading. Two changes the record can't show are
    /// rebuilt on purpose: a kSmdChangeOrig field the transaction added (an empty PreviousFields) is taken out
    /// of `before`, and a directory page with FinalFields and no PreviousFields changed only its Indexes
    /// (kSmdNever). A threaded ModifiedNode with FinalFields was written by the transactor; one without is a
    /// threadOwners root nobody wrote.
    fn rebuild(n: &Value, tx: &str, seq: u32) -> NodeChange {
        let (kind, x) = n.as_object().unwrap().iter().next().unwrap();
        let key = Hash256(hex::decode(x["LedgerIndex"].as_str().unwrap()).unwrap().try_into().unwrap());
        let entry_type = x["LedgerEntryType"].clone();
        let listed = |s: &str| x.get(s).and_then(Value::as_object).cloned();
        let stamp = |o: &mut Map<String, Value>| {
            o.insert("PreviousTxnID".into(), json!(tx));
            o.insert("PreviousTxnLgrSeq".into(), json!(seq));
        };
        let node_thread = x.get("PreviousTxnID").zip(x.get("PreviousTxnLgrSeq")).map(|(id, s)| {
            (id.as_str().unwrap().to_uppercase(), u32::try_from(s.as_u64().unwrap()).unwrap())
        });
        let typed = |mut o: Map<String, Value>| {
            o.insert("LedgerEntryType".into(), entry_type.clone());
            o
        };
        match kind.as_str() {
            "CreatedNode" => {
                let mut after = typed(listed("NewFields").unwrap_or_default());
                stamp(&mut after);
                change(key, NodeKind::Created, None, Value::Object(after))
            }
            "DeletedNode" => {
                let after = typed(listed("FinalFields").unwrap_or_default());
                let mut before = after.clone();
                before.extend(listed("PreviousFields").unwrap_or_default());
                change(key, NodeKind::Deleted, Some(Value::Object(before)), Value::Object(after))
            }
            _ => {
                let finals = listed("FinalFields");
                let mut after = typed(finals.clone().unwrap_or_default());
                let mut before = after.clone();
                match listed("PreviousFields") {
                    Some(p) if p.is_empty() => {
                        let added = after.keys().find(|k| changes_orig(k)).cloned().expect("an added kSmdChangeOrig field");
                        before.remove(&added);
                    }
                    Some(p) => before.extend(p),
                    None if finals.is_some() && entry_type == "DirectoryNode" => {
                        before.insert("Indexes".into(), json!(["01".repeat(32)]));
                        after.insert("Indexes".into(), json!([]));
                    }
                    None => {}
                }
                if let Some((id, s)) = &node_thread {
                    before.insert("PreviousTxnID".into(), json!(id));
                    before.insert("PreviousTxnLgrSeq".into(), json!(s));
                    stamp(&mut after);
                }
                NodeChange {
                    key,
                    kind: NodeKind::Modified,
                    before: Some(Value::Object(before)),
                    after: Value::Object(after),
                    written: finals.is_some(),
                    thread_from: node_thread,
                }
            }
        }
    }

    fn shape(n: &Value) -> String {
        let (kind, x) = n.as_object().unwrap().iter().next().unwrap();
        let has = |k: &str, c: char| if x.get(k).is_some() { c } else { '-' };
        format!("{kind} {} {}{}{}{}", x["LedgerEntryType"].as_str().unwrap_or("?"), has("NewFields", 'N'),
                has("FinalFields", 'F'), has("PreviousFields", 'P'), has("PreviousTxnID", 'T'))
    }

    fn first_difference(got: &[Value], want: &[Value]) -> String {
        let short = |v: Option<&Value>| v.map(|v| v.to_string().chars().take(600).collect()).unwrap_or_else(|| "-".to_string());
        (0..got.len().max(want.len()))
            .find(|&i| got.get(i) != want.get(i))
            .map(|i| format!("node {i}: ours {} / mainnet {}", short(got.get(i)), short(want.get(i))))
            .unwrap_or_default()
    }

    /// Independence task 2.3's round trip over mainnet's own records: every node of every transaction's
    /// metadata is rebuilt as a change, and `affected_nodes` must give the metadata back exactly. That tests
    /// which fields go where, the empty sections, the node threading and the order. It can't test
    /// completeness, since a field mainnet leaves out can't be rebuilt (dp, task 2.6, does that with the
    /// engine's full objects).
    ///
    ///   XRPL_META_CORPUS=<scripts/fetch_meta_ledgers.py json/ folder> \
    ///     cargo test -p xrpl-ledger --lib round_trip -- --ignored --nocapture
    #[test]
    #[ignore]
    fn round_trip_over_mainnet_metadata() {
        let dir = std::env::var_os("XRPL_META_CORPUS").expect("XRPL_META_CORPUS: a folder of `ledger` answers");
        let mut paths: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        paths.sort();
        assert!(!paths.is_empty(), "no ledgers in {dir:?}");
        let (mut txs, mut nodes, mut misses) = (0usize, 0usize, Vec::new());
        let mut shapes = std::collections::BTreeMap::<String, usize>::new();
        for path in &paths {
            let doc: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            let doc = doc.get("result").unwrap_or(&doc);
            let ledger = &doc["ledger"];
            let seq = doc["ledger_index"].as_u64().or_else(|| ledger["ledger_index"].as_str()?.parse().ok()).unwrap();
            for tx in ledger["transactions"].as_array().unwrap() {
                let meta = tx.get("metaData").or_else(|| tx.get("meta")).unwrap();
                let hash = tx["hash"].as_str().unwrap();
                let want = meta["AffectedNodes"].as_array().unwrap();
                let changes: Vec<NodeChange> = want.iter().map(|n| rebuild(n, hash, seq as u32)).collect();
                let got = affected_nodes(&changes);
                txs += 1;
                nodes += want.len();
                for n in want {
                    *shapes.entry(shape(n)).or_default() += 1;
                }
                if got != *want {
                    misses.push(format!("#{seq} {}: {}", &hash[..12], first_difference(&got, want)));
                }
            }
        }
        eprintln!("round trip: {} ledgers, {txs} transactions, {nodes} nodes; {} transactions differ", paths.len(), misses.len());
        for (shape, n) in &shapes {
            eprintln!("  {shape}: {n}");
        }
        for m in misses.iter().take(20) {
            eprintln!("  {m}");
        }
        assert!(misses.is_empty(), "{} transactions' AffectedNodes differ from mainnet's", misses.len());
    }
}
