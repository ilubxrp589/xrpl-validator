//! PreviousTxnID / PreviousTxnLgrSeq stamping — the threading every ledger
//! object carries and the differential harness historically ignored (its
//! compares strip the fields; the fixture metas never carry them inside
//! FinalFields — rippled puts them as SIBLINGS of FinalFields).
//!
//! rippled's rule (ApplyStateTable): a touched item whose final content
//! equals its original is DROPPED from the metadata and never threaded — so
//! only MATERIALLY CHANGED items get stamped with the touching transaction's
//! hash and the current ledger sequence. Created items are stamped too (a
//! fresh object's PreviousTxn* name its creating transaction). 28 of the 30
//! ledger entry types carry the fields (fixPreviousTxnID added them to
//! DirectoryNode and friends — verified live: mainnet dir pages thread);
//! LedgerHashes does not.
//!
//! Verified 2026-08-21 against post-state: offer A6FED001 after #106433073
//! carries PreviousTxnID = 462DE605… (the crossing tx), PreviousTxnLgrSeq =
//! 106433073 — exactly what `stamp_threading` produces.

use std::collections::HashMap;

use super::sandbox::SandboxEntry;
use xrpl_core::types::Hash256;

/// Canonicalise the spelling variance that is NOT a material change:
/// directory/node pointer fields (hex-string vs number) and the threading
/// fields themselves. Mirror of the probe's `canon_ptrs` + PreviousTxn strip.
fn canon_material(v: &mut serde_json::Value) {
    const PTRS: [&str; 7] = [
        "IndexNext",
        "IndexPrevious",
        "OwnerNode",
        "BookNode",
        "HighNode",
        "LowNode",
        "DestinationNode",
    ];
    let Some(obj) = v.as_object_mut() else { return };
    for k in PTRS {
        if let Some(f) = obj.get_mut(k) {
            let n = f
                .as_u64()
                .or_else(|| f.as_str().and_then(|s| u64::from_str_radix(s, 16).ok()));
            if let Some(n) = n {
                *f = serde_json::Value::from(n);
            }
        }
    }
    obj.remove("PreviousTxnID");
    obj.remove("PreviousTxnLgrSeq");
    obj.remove("index");
}

/// Content-equal modulo pointer spelling and existing threading — the writes
/// rippled's ApplyStateTable drops from the meta and does NOT thread.
pub fn semantically_equal(pre: &[u8], post: &[u8]) -> bool {
    let (Ok(mut a), Ok(mut b)) = (
        serde_json::from_slice::<serde_json::Value>(pre),
        serde_json::from_slice::<serde_json::Value>(post),
    ) else {
        return false;
    };
    canon_material(&mut a);
    canon_material(&mut b);
    a == b
}

/// Stamp `PreviousTxnID`/`PreviousTxnLgrSeq` onto this transaction's
/// materially-changed writes. `pre` looks up the pre-transaction bytes of a
/// key (None for created objects).
pub fn stamp_threading(
    mods: &mut HashMap<Hash256, SandboxEntry>,
    pre: &dyn Fn(&Hash256) -> Option<Vec<u8>>,
    tx_hash_hex: &str,
    ledger_seq: u32,
) {
    let hash_upper = tx_hash_hex.to_uppercase();
    for (k, ent) in mods.iter_mut() {
        let modified = matches!(ent, SandboxEntry::Modified(_));
        let bytes = match ent {
            SandboxEntry::Created(b) | SandboxEntry::Modified(b) => b,
            SandboxEntry::Deleted => continue,
        };
        let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(bytes.as_slice()) else {
            continue;
        };
        if v.get("LedgerEntryType").and_then(|t| t.as_str()) == Some("LedgerHashes") {
            continue;
        }
        if modified {
            if let Some(pb) = pre(k) {
                if semantically_equal(&pb, bytes) {
                    continue; // write-back, not a change — rippled never threads it
                }
            }
        }
        v["PreviousTxnID"] = serde_json::Value::String(hash_upper.clone());
        v["PreviousTxnLgrSeq"] = serde_json::Value::Number(ledger_seq.into());
        *bytes = serde_json::to_vec(&v).unwrap_or_default();
    }

    // `threadOwners` (ApplyStateTable.cpp:640-668): every CREATED or DELETED
    // node also threads the transaction to its owner accounts' roots — both
    // limit issuers for a RippleState, else sfAccount and sfDestination when
    // present, nothing for an AccountRoot. Those roots become the meta's
    // pure-threading ModifiedNodes (the FinalFields==pre refreshes the
    // expected-side filter drops). #106124864 E969E24F: EscrowCreate
    // EB5DF108 threads the escrow DESTINATION's root — an account the
    // transaction itself never writes.
    let mut owners: Vec<[u8; 20]> = Vec::new();
    let mut collect = |v: &serde_json::Value| {
        let ty = v.get("LedgerEntryType").and_then(|t| t.as_str()).unwrap_or("");
        match ty {
            "AccountRoot" => {}
            "RippleState" => {
                for side in ["LowLimit", "HighLimit"] {
                    if let Some(a) = v
                        .get(side)
                        .and_then(|l| l.get("issuer"))
                        .and_then(|i| i.as_str())
                        .and_then(crate::tx::offer::decode20)
                    {
                        owners.push(a);
                    }
                }
            }
            _ => {
                for f in ["Account", "Destination"] {
                    if let Some(a) =
                        v.get(f).and_then(|x| x.as_str()).and_then(crate::tx::offer::decode20)
                    {
                        owners.push(a);
                    }
                }
            }
        }
    };
    for (k, ent) in mods.iter() {
        match ent {
            SandboxEntry::Created(b) => {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(b) {
                    collect(&v);
                }
            }
            SandboxEntry::Deleted => {
                if let Some(pb) = pre(k) {
                    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&pb) {
                        collect(&v);
                    }
                }
            }
            SandboxEntry::Modified(_) => {}
        }
    }
    owners.sort_unstable();
    owners.dedup();
    for acct in owners {
        let rk = super::keylet::account_root_key(&acct);
        let stamped = |b: &[u8]| -> Option<Vec<u8>> {
            let mut v = serde_json::from_slice::<serde_json::Value>(b).ok()?;
            v["PreviousTxnID"] = serde_json::Value::String(hash_upper.clone());
            v["PreviousTxnLgrSeq"] = serde_json::Value::Number(ledger_seq.into());
            serde_json::to_vec(&v).ok()
        };
        match mods.get_mut(&rk) {
            Some(SandboxEntry::Deleted) => {} // just deleted — rippled warns and skips
            Some(SandboxEntry::Created(b)) | Some(SandboxEntry::Modified(b)) => {
                // Already written this tx — thread unconditionally (a
                // threadOwners hit is threaded even when the write itself
                // was a content-equal write-back).
                if let Some(nb) = stamped(b) {
                    *b = nb;
                }
            }
            None => {
                if let Some(pb) = pre(&rk) {
                    if let Some(nb) = stamped(&pb) {
                        mods.insert(rk, SandboxEntry::Modified(nb));
                    }
                }
            }
        }
    }
}
