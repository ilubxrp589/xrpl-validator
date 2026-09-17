//! native_apply — the native engine's per-tx apply core plus the
//! ledger-level helpers (skip list, canonical re-spelling), shared verbatim
//! by `state_replay` (the offline proof) and `native_shadow` (the in-process
//! Stage 4 leg). One implementation, two harnesses: what the replay proved
//! byte-for-byte is exactly what the shadow leg runs.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};
use xrpl_core::types::Hash256;
use xrpl_ledger::ledger::keylet;
use xrpl_ledger::ledger::sandbox::{Sandbox, SandboxEntry};
use xrpl_ledger::ledger::state::LedgerState;
use xrpl_ledger::ledger::transactor::{apply_common, TxFields, TxResult};
use xrpl_ledger::shamap::hash::{sha512_half, sha512_half_prefixed, HASH_PREFIX_TRANSACTION_ID};
use xrpl_ledger::tx::dispatch::get_transactor;

pub const ACCOUNT_FIELDS: &[&str] = &["Destination", "Owner", "Authorize", "Unauthorize", "RegularKey"];

pub fn decode_address(addr: &str) -> Option<[u8; 20]> {
    const ALPHABET: &[u8] = b"rpshnaf39wBUDNEGHJKLM4PQRST7VWXYZ2bcdeCg65jkm8oFqi1tuvAxyz";
    let mut n: Vec<u8> = vec![0];
    for ch in addr.bytes() {
        let carry = ALPHABET.iter().position(|&c| c == ch)?;
        let mut c = carry;
        for byte in n.iter_mut().rev() {
            c += (*byte as usize) * 58;
            *byte = (c & 0xFF) as u8;
            c >>= 8;
        }
        while c > 0 {
            n.insert(0, (c & 0xFF) as u8);
            c >>= 8;
        }
    }
    let leading = addr.bytes().take_while(|&b| b == b'r').count();
    let mut result = vec![0u8; leading];
    result.extend_from_slice(&n);
    if result.len() < 25 {
        return None;
    }
    let mut id = [0u8; 20];
    id.copy_from_slice(&result[1..21]);
    Some(id)
}

/// Recursively rewrite any base58 classic-address string (`r…`) to 20-byte
/// hex — the native engine's account-field convention. Defect B's whole
/// story: the offline worlds (snapshot loader, probe hydration) always ran
/// this pass, the live mirror didn't, and every hex-only field parser
/// (check.rs parse_account_id and kin) read r-addresses as ABSENT —
/// tecNO_PERMISSION storms on ledgers the replay proves byte-perfect.
pub fn hexify_addresses(v: &mut Value) {
    match v {
        Value::String(s) => {
            if s.starts_with('r') && s.len() >= 25 && s.len() <= 40 {
                if let Some(id) = decode_address(s) {
                    *v = json!(hex::encode(id));
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(hexify_addresses),
        Value::Object(m) => m.values_mut().for_each(hexify_addresses),
        _ => {}
    }
}

pub fn build_txfields(txjson: &Value) -> Option<TxFields> {
    TxFields::from_json(txjson)
}

/// Native per-tx apply — identical branching to differential_probe's copy
/// (which mirrors apply.rs::apply_transaction_set). Returns (ter, mods).
pub fn native_apply_one(state: &LedgerState, tx: &TxFields) -> (String, HashMap<Hash256, SandboxEntry>) {
    let transactor = match get_transactor(&tx.tx_type) {
        Some(t) => t,
        None => {
            let mut sb = Sandbox::new(state);
            let r = apply_common(tx, &mut sb);
            if r.is_success() {
                return (TxResult::Unsupported.code_str().to_string(), sb.into_modifications());
            }
            return (r.code_str().to_string(), HashMap::new());
        }
    };
    if xrpl_ledger::tx::dispatch::is_pseudo(&tx.tx_type) {
        let pf = transactor.preflight(tx);
        if !pf.is_success() {
            return (pf.code_str().to_string(), HashMap::new());
        }
        let mut sb = Sandbox::new(state);
        let applied = transactor.do_apply(tx, &mut sb);
        if applied.is_success() {
            return (TxResult::Success.code_str().to_string(), sb.into_modifications());
        }
        return (applied.code_str().to_string(), HashMap::new());
    }
    let preflight = transactor.preflight(tx);
    if !preflight.is_success() {
        if preflight.is_claimed() {
            let mut sb = Sandbox::new(state);
            let common = apply_common(tx, &mut sb);
            if common.is_success() {
                return (preflight.code_str().to_string(), sb.into_modifications());
            }
            return (common.code_str().to_string(), HashMap::new());
        }
        return (preflight.code_str().to_string(), HashMap::new());
    }
    let mut sb = Sandbox::new(state);
    let preclaim = transactor.preclaim(tx, &sb);
    if !preclaim.is_success() && !preclaim.is_claimed() {
        return (preclaim.code_str().to_string(), HashMap::new());
    }
    if !preclaim.is_success() {
        let common = apply_common(tx, &mut sb);
        if common.is_success() {
            return (preclaim.code_str().to_string(), sb.into_modifications());
        }
        return (common.code_str().to_string(), HashMap::new());
    }
    let common = apply_common(tx, &mut sb);
    if !common.is_success() {
        return (common.code_str().to_string(), HashMap::new());
    }
    let snap = sb.snapshot();
    // Finding 253: presence BEFORE do_apply is what rippled's stamp sees.
    let txn_id_armed = xrpl_ledger::ledger::transactor::account_txn_id_armed(tx, &sb);
    let applied = transactor.do_apply(tx, &mut sb);
    if applied.is_success() {
        // Success-only (Transactor.cpp:660; tec rolls the stamp back).
        xrpl_ledger::ledger::transactor::stamp_account_txn_id(tx, &mut sb, txn_id_armed);
        (TxResult::Success.code_str().to_string(), sb.into_modifications())
    } else if applied.is_claimed() {
        // tecEXPIRED keeps its NFTokenOffer / Credential erasures (finding 245,
        // rippled's processPersistentChanges) — same settlement as apply.rs.
        if applied == TxResult::Expired {
            xrpl_ledger::ledger::apply::settle_expired(&mut sb, snap);
        } else if applied != TxResult::Killed {
            sb.restore_snapshot(snap);
        }
        (applied.code_str().to_string(), sb.into_modifications())
    } else {
        (applied.code_str().to_string(), HashMap::new())
    }
}

/// Re-spell engine-internal JSON into the canonical forms the binary codec
/// demands (same table as differential_probe's byte census).
pub fn canon_for_encode(v: &mut Value) {
    const U64_HEX: &[&str] = &[
        "OwnerNode", "BookNode", "LowNode", "HighNode", "DestinationNode",
        "IndexNext", "IndexPrevious", "XChainClaimID", "XChainAccountCreateCount",
        "XChainAccountClaimCount", "ReferenceCount", "NFTokenOfferNode", "IssuerNode",
        "AssetPrice",
        // 2026-08-31 census vs definitions.json: the remaining u64 SFields a
        // ledger entry can carry (hex-string forms pass through unchanged, so
        // these are identity for decoded objects and normalization for any
        // future number-form engine write). Hook/Emit families (Xahau, never
        // in mainnet state) deliberately absent.
        "ExchangeRate", "SubjectNode", "LoanBrokerNode", "VaultNode",
        "BaseFee", "Cookie", "ServerVersion",
    ];
    const U64_DEC: &[&str] = &["MaximumAmount", "OutstandingAmount", "MPTAmount", "LockedAmount"];
    const ACCTS: &[&str] = &[
        "Account", "Owner", "Destination", "Issuer", "RegularKey", "Authorize",
        "Unauthorize", "NFTokenMinter", "Holder", "OtherChainSource",
        "AttestationSignerAccount", "AttestationRewardAccount", "LockingChainDoor",
        "IssuingChainDoor", "issuer",
        // 2026-08-31: every remaining AccountID-typed SField (definitions.json
        // census). "Subject" alone was 386 of 19.8M hydrate-audit failures —
        // every Credential in the mirror was unencodable, its 40-hex Subject
        // fed to the base58 address parser (InvalidBase58/InvalidAddress).
        "Subject", "Delegate", "Counterparty", "Borrower",
        "OtherChainDestination", "EmitCallback", "HookAccount",
    ];
    match v {
        Value::Array(a) => {
            for e in a {
                canon_for_encode(e);
            }
        }
        Value::Object(o) => {
            for (name, val) in o.iter_mut() {
                if U64_HEX.contains(&name.as_str()) {
                    let n = val.as_u64().or_else(|| {
                        val.as_str().and_then(|s| u64::from_str_radix(s, 16).ok())
                    });
                    if let Some(n) = n {
                        *val = Value::String(format!("{n:016X}"));
                    }
                } else if U64_DEC.contains(&name.as_str()) {
                    let n = val
                        .as_u64()
                        .or_else(|| val.as_str().and_then(|s| s.parse::<u64>().ok()));
                    if let Some(n) = n {
                        *val = Value::String(format!("{n:016X}"));
                    }
                } else if ACCTS.contains(&name.as_str()) {
                    if let Some(s) = val.as_str() {
                        if s.len() == 40 {
                            if let Ok(b) = hex::decode(s) {
                                if let Ok(arr) = <[u8; 20]>::try_from(b.as_slice()) {
                                    *val = Value::String(
                                        xrpl_core::AccountId::from_bytes(arr).to_address(),
                                    );
                                }
                            }
                        }
                    }
                } else {
                    canon_for_encode(val);
                }
            }
        }
        _ => {}
    }
}

/// Batch (BatchV1_1): rippled records every inner transaction as its own
/// ledger entry — own hash, own metadata carrying `ParentBatchID`, the
/// indices right after its outer — and applies it INSIDE the outer's
/// application (`applyBatchTransactions`). The native engine's
/// `BatchTransactor::do_apply` does the same, so inner entries are skipped
/// in the replay and their metadata is attributed to the outer.
pub struct BatchAttribution {
    pub skip: HashSet<String>,
    pub inners_of: HashMap<String, Vec<String>>,
}

pub fn batch_attribution(ordered: &[&Value]) -> BatchAttribution {
    let mut skip = HashSet::new();
    let mut inners_of: HashMap<String, Vec<String>> = HashMap::new();
    for tx in ordered {
        let flags = tx.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
        let parent = tx["metaData"].get("ParentBatchID").and_then(|p| p.as_str());
        if flags & xrpl_ledger::tx::batch::TF_INNER_BATCH_TXN == 0 {
            continue;
        }
        // An inner is identified by the flag AND the link: a stray
        // tfInnerBatchTxn on a top-level tx is not somebody's inner.
        let Some(parent) = parent else { continue };
        let hash = tx["hash"].as_str().unwrap_or("").to_uppercase();
        skip.insert(hash.clone());
        inners_of.entry(parent.to_uppercase()).or_default().push(hash);
    }
    BatchAttribution { skip, inners_of }
}

#[cfg(test)]
mod canon_tests {
    use super::*;

    /// decode → hexify → canon → encode must reproduce the canonical bytes —
    /// the exact chain the shadow mirror lives on (hydrate, reconcile,
    /// compare). Vectors are real mainnet entries captured 2026-08-31.
    fn roundtrip_exact(hex_str: &str) {
        let bytes = hex::decode(hex_str.trim()).expect("vector hex");
        let mut jv = xrpl_core::codec::decode::decode_transaction_binary(&bytes).expect("decodes");
        hexify_addresses(&mut jv);
        canon_for_encode(&mut jv);
        let out = xrpl_core::codec::encode::encode_transaction_json(&jv, false).expect("encodes");
        assert_eq!(hex::encode_upper(&out), hex::encode_upper(&bytes), "roundtrip differs");
    }

    /// The 386-of-19.8M hydrate-audit class: Credential entries were
    /// unencodable because "Subject" was missing from canon's ACCTS table
    /// (40-hex fed to the base58 address parser).
    #[test]
    fn credential_entries_roundtrip() {
        roundtrip_exact(include_str!("../tests/vectors/credential_plain.hex"));
        roundtrip_exact(include_str!("../tests/vectors/credential_uri_expiration.hex"));
    }

    /// A one-offer BookDirectory root (the 2026-08-31 RECONCILE-LEAK key
    /// class) — always round-tripped offline; pinned here so it stays true.
    #[test]
    fn book_directory_root_roundtrips() {
        roundtrip_exact(include_str!("../tests/vectors/book_directory_root.hex"));
    }
}

/// keylet::skip(seq): the every-65536-block LedgerHashes entry —
/// SHA512Half(0x0073 ‖ u32be(seq >> 16)) (rippled Indexes.cpp).
pub fn skip_every_key(seq: u32) -> Hash256 {
    let mut buf = Vec::with_capacity(6);
    buf.extend_from_slice(&[0x00, 0x73]);
    buf.extend_from_slice(&(seq >> 16).to_be_bytes());
    sha512_half(&buf)
}

/// Ledger::updateSkipList, on the JSON state: at close of ledger `target`,
/// push hash(target-1) into the rolling 256-entry list (trim front at 256),
/// and — when (target-1) & 0xff == 0 — append it to the every-256th entry
/// for the 65536-block too (no trim; it holds exactly 256 when full).
pub fn update_skip_list(
    state: &mut LedgerState,
    dirty: &mut HashSet<Hash256>,
    target: u32,
    parent_hash_hex: &str,
) {
    let prev = target - 1;
    let mut write = |key: Hash256, trim: bool| {
        // read_json: the replay keeps raw leaves (lazy decode) — a direct
        // state_map lookup would fail to parse and silently restart the
        // skip list from empty on every ledger.
        let mut obj = state
            .read_json(&key)
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .unwrap_or_else(|| {
                json!({
                    "LedgerEntryType": "LedgerHashes",
                    "Flags": 0,
                    "Hashes": [],
                    "index": hex::encode_upper(key.0),
                })
            });
        let hashes = obj["Hashes"].as_array().cloned().unwrap_or_default();
        let mut hashes = hashes;
        if trim && hashes.len() == 256 {
            hashes.remove(0);
        }
        hashes.push(json!(parent_hash_hex.to_uppercase()));
        obj["Hashes"] = json!(hashes);
        obj["LastLedgerSequence"] = json!(prev);
        let _ = state.state_map.insert(key, serde_json::to_vec(&obj).unwrap_or_default());
        dirty.insert(key);
    };
    if prev & 0xff == 0 {
        write(skip_every_key(prev), false);
    }
    write(keylet::skip_list_key(), true);
}

/// The transaction ids of a Batch outer's inner transactions, in
/// `RawTransactions` order — the native counterpart of leg A's
/// `xrpl_ffi::batch_inner_ids`. rippled files every inner in the ledger as a
/// transaction in its own right, under the ordinary transaction id
/// `SHA512Half("TXN\0" ‖ its serialization)`, so a leg holding only the
/// OUTER can still name its inner entries: `differential_probe`'s fixtures
/// carry `tx_json` with the metadata stripped, so no inner entry there has a
/// `ParentBatchID` for `batch_attribution` to read.
pub fn batch_inner_ids(outer: &Value) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for raw in outer.get("RawTransactions").and_then(|v| v.as_array()).into_iter().flatten() {
        let Some(inner) = raw.get("RawTransaction") else { continue };
        let mut v = inner.clone();
        // Idempotent on API-form JSON (addresses already base58): the
        // re-spelling only bites when the caller holds the mirror dialect.
        canon_for_encode(&mut v);
        let Ok(blob) = xrpl_core::codec::encode::encode_transaction_json(&v, false) else {
            continue;
        };
        ids.push(hex::encode_upper(sha512_half_prefixed(&HASH_PREFIX_TRANSACTION_ID, &blob).0));
    }
    ids
}

/// One key touched by several entries of the same Batch: the expected side
/// reports the NET effect. The rules are leg A's, verbatim
/// (`ffi_engine.rs` `merged_expected`): Created wins over Modified, and a key
/// Created and then Deleted inside the batch is no entry at all. `nodes` is
/// the `(key, kind)` pairs of the outer's metadata followed by its inners' in
/// TransactionIndex order, kinds as the fixtures spell them — 0 Created,
/// 1 Modified, 2 Deleted.
///
/// Inside ONE metadata a key appears exactly once, so for a non-Batch
/// transaction this is the identity over its nodes.
pub fn fold_batch_mutset(nodes: &[(String, u8)]) -> HashSet<(String, u8)> {
    let mut kinds: HashMap<&str, (bool, bool, bool)> = HashMap::new();
    for (key, kind) in nodes {
        let e = kinds.entry(key.as_str()).or_default();
        match kind {
            0 => e.0 = true,
            2 => e.2 = true,
            _ => e.1 = true,
        }
    }
    let mut out: HashSet<(String, u8)> = HashSet::with_capacity(kinds.len());
    for (key, (created, _modified, deleted)) in kinds {
        if created && deleted {
            continue; // created and destroyed inside the batch — no entry
        }
        let kind = if created {
            0
        } else if deleted {
            2
        } else {
            1
        };
        out.insert((key.to_string(), kind));
    }
    out
}

#[cfg(test)]
mod batch_fold_tests {
    use super::*;
    use serde_json::json;

    fn k(key: &str, kind: u8) -> (String, u8) {
        (key.to_string(), kind)
    }

    #[test]
    fn fold_batch_mutset_created_wins_over_modified() {
        assert_eq!(fold_batch_mutset(&[k("AA", 1), k("AA", 0)]), HashSet::from([k("AA", 0)]));
        assert_eq!(fold_batch_mutset(&[k("AA", 0), k("AA", 1)]), HashSet::from([k("AA", 0)]));
    }

    #[test]
    fn fold_batch_mutset_created_then_deleted_vanishes() {
        assert!(fold_batch_mutset(&[k("AA", 0), k("AA", 2)]).is_empty());
        assert!(fold_batch_mutset(&[k("AA", 2), k("AA", 0)]).is_empty());
    }

    #[test]
    fn fold_batch_mutset_modified_then_deleted_is_deleted() {
        assert_eq!(fold_batch_mutset(&[k("AA", 1), k("AA", 2)]), HashSet::from([k("AA", 2)]));
    }

    #[test]
    fn fold_batch_mutset_keys_touched_once_pass_through() {
        let nodes = [k("AA", 0), k("BB", 1), k("CC", 2)];
        assert_eq!(fold_batch_mutset(&nodes), nodes.iter().cloned().collect::<HashSet<_>>());
    }

    /// devnet #5309670, Batch 0DB84681FAAD…: rippled files each inner as a
    /// transaction in its own right, so its ledger entry hash is the ordinary
    /// transaction id of the RawTransaction's own serialization. Both hashes
    /// below are the ledger's, read off `l5309670_blobs.txt`.
    #[test]
    fn batch_inner_ids_reproduce_the_devnet_inner_entry_hashes() {
        let outer = json!({
            "TransactionType": "Batch",
            "Flags": 0x0004_0000u64,
            "RawTransactions": [
                {"RawTransaction": {
                    "Account": "rJB72TyLVYfTHS7iPC2MBMPRHN7PqJms7D",
                    "Amount": "1000000",
                    "Destination": "rsvXCjhcBetR4fdpJWXf6DhJdy3KEbRRmw",
                    "Fee": "0",
                    "Flags": 1073741824u64,
                    "Sequence": 5309666,
                    "SigningPubKey": "",
                    "TransactionType": "Payment"
                }},
                {"RawTransaction": {
                    "Account": "r4JxwgKZJjHYfiFWhujThxsVxxrXcQYM4C",
                    "Amount": "1000000",
                    "Destination": "rsvXCjhcBetR4fdpJWXf6DhJdy3KEbRRmw",
                    "Fee": "0",
                    "Flags": 1073741824u64,
                    "Sequence": 5309667,
                    "SigningPubKey": "",
                    "TransactionType": "Payment"
                }},
            ]
        });
        assert_eq!(
            batch_inner_ids(&outer),
            vec![
                "F835E19C2C403DD7B5BC54E69D995CC06D0CB9ED34B5CC419182BC1146EE4AB3".to_string(),
                "C7A3E4417CBCA87BC90D1237F23203E16E1251B97452F29F272DA11CE3F96FB1".to_string(),
            ]
        );
    }

    #[test]
    fn a_transaction_without_raw_transactions_has_no_inner_ids() {
        assert!(batch_inner_ids(&json!({"TransactionType": "Payment"})).is_empty());
    }
}

#[cfg(test)]
mod batch_attribution_tests {
    use super::*;
    use serde_json::json;

    fn entry(hash: &str, idx: u64, flags: u64, parent: Option<&str>) -> Value {
        let mut meta = json!({"TransactionIndex": idx, "TransactionResult": "tesSUCCESS", "AffectedNodes": []});
        if let Some(p) = parent { meta["ParentBatchID"] = json!(p); }
        json!({"hash": hash, "TransactionType": "Payment", "Flags": flags, "metaData": meta})
    }

    #[test]
    fn inner_entries_are_skipped_and_grouped_under_their_outer_in_index_order() {
        let o = json!({"hash": "AA", "TransactionType": "Batch", "Flags": 0x0004_0000u64,
                       "metaData": {"TransactionIndex": 1, "TransactionResult": "tesSUCCESS", "AffectedNodes": []}});
        let i2 = entry("CC", 3, 0x4000_0000, Some("AA"));
        let i1 = entry("BB", 2, 0x4000_0000, Some("AA"));
        let other = entry("DD", 4, 0, None);
        let ordered: Vec<&Value> = vec![&o, &i1, &i2, &other];
        let a = batch_attribution(&ordered);
        assert_eq!(a.skip.len(), 2);
        assert!(a.skip.contains("BB") && a.skip.contains("CC"));
        assert_eq!(a.inners_of.get("AA").cloned().unwrap_or_default(), vec!["BB".to_string(), "CC".to_string()]);
        assert!(!a.skip.contains("DD"));
    }

    #[test]
    fn an_inner_flag_without_a_parent_is_not_an_inner() {
        let lone = entry("EE", 1, 0x4000_0000, None);
        let ordered: Vec<&Value> = vec![&lone];
        let a = batch_attribution(&ordered);
        assert!(a.skip.is_empty());
    }
}
