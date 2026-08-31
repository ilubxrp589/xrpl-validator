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
use xrpl_ledger::shamap::hash::sha512_half;
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
    // Pseudo-transactions carry Account: "" and Fee: "0" — the zero account.
    let account = match txjson["Account"].as_str()? {
        "" => [0u8; 20],
        a => decode_address(a)?,
    };
    let tx_type = txjson["TransactionType"].as_str()?.to_string();
    let fee = txjson["Fee"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
    let sequence = txjson["Sequence"].as_u64().unwrap_or(0) as u32;
    let ticket_seq = txjson.get("TicketSequence").and_then(|v| v.as_u64()).map(|v| v as u32);
    let last_ledger_seq = txjson.get("LastLedgerSequence").and_then(|v| v.as_u64()).map(|v| v as u32);
    let mut fields = txjson.clone();
    for k in ACCOUNT_FIELDS {
        if let Some(a) = fields.get(*k).and_then(|v| v.as_str()) {
            if a.starts_with('r') {
                if let Some(id) = decode_address(a) {
                    fields[*k] = json!(hex::encode(id));
                }
            }
        }
    }
    Some(TxFields { account, tx_type, fee, sequence, ticket_seq, last_ledger_seq, fields })
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
    let applied = transactor.do_apply(tx, &mut sb);
    if applied.is_success() {
        // Success-only (Transactor.cpp:660; tec rolls the stamp back).
        xrpl_ledger::ledger::transactor::stamp_account_txn_id(tx, &mut sb);
        (TxResult::Success.code_str().to_string(), sb.into_modifications())
    } else if applied.is_claimed() {
        if applied != TxResult::Killed {
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
        let mut obj = state
            .state_map
            .lookup(&key)
            .and_then(|b| serde_json::from_slice::<Value>(b).ok())
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

