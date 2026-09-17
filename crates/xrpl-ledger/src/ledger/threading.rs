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

use std::collections::{HashMap, HashSet};

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
    // Finding 258: an amount's VALUE is a number, not a spelling. The
    // decoder and the engine write the same IOU differently once the
    // exponent is large (9999999999999999e80 as a 96-digit string on one
    // side, its scientific form on the other), and a textual compare then
    // called an untouched bridge offer changed and threaded it. Compare
    // every amount by its canonical (mantissa, exponent).
    for (_, f) in obj.iter_mut() {
        canon_amount(f);
    }
}

/// Rewrite an amount — a drops string or an {currency, issuer, value}
/// object — so that equal values spell the same.
fn canon_amount(f: &mut serde_json::Value) {
    // Textual canonicalisation — no integer parse, so a 96-digit spelling
    // and its scientific twin meet as the same (digits, exponent).
    let canon = |s: &str| -> Option<String> {
        let neg = s.starts_with('-');
        let s = s.trim_start_matches('-');
        let (mant, mut exp): (&str, i64) = match s.find(['e', 'E']) {
            Some(i) => (&s[..i], s[i + 1..].parse().ok()?),
            None => (s, 0),
        };
        let mut digits = String::with_capacity(mant.len());
        for (i, ch) in mant.chars().enumerate() {
            match ch {
                '0'..='9' => digits.push(ch),
                '.' => exp -= (mant.len() - i - 1) as i64,
                _ => return None,
            }
        }
        let trimmed = digits.trim_start_matches('0');
        let tail_zeros = trimmed.len() - trimmed.trim_end_matches('0').len();
        let core = &trimmed[..trimmed.len() - tail_zeros];
        if core.is_empty() {
            return Some("0".into());
        }
        Some(format!("{}{}e{}", if neg { "-" } else { "" }, core, exp + tail_zeros as i64))
    };
    match f {
        serde_json::Value::Object(o) => {
            if let Some(serde_json::Value::String(v)) = o.get("value") {
                if let Some(c) = canon(v) {
                    o.insert("value".into(), serde_json::Value::String(c));
                }
            }
        }
        serde_json::Value::String(v) if v.bytes().all(|b| b.is_ascii_digit()) && !v.is_empty() => {
            if let Some(c) = canon(v) {
                *f = serde_json::Value::String(c);
            }
        }
        _ => {}
    }
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
    stamp_threading_keys(mods, pre, tx_hash_hex, ledger_seq, None)
}

/// `stamp_threading` restricted to a set of keys: with `only` given, every
/// `mods` entry outside it is left exactly as it was — including for the
/// `threadOwners` pass, which then considers only the created/deleted nodes
/// among those keys (the owner roots it threads are still written wherever
/// they live, as rippled does). `None` visits everything, which is
/// `stamp_threading`.
///
/// A `Batch` needs this: rippled applies each inner as its own transaction
/// (`applyBatchTransactions` -> `apply(..., inner, tapBATCH)`), so an object
/// an inner touched is threaded with the INNER's id, not the outer's — see
/// `stamp_batch_threading`.
pub fn stamp_threading_keys(
    mods: &mut HashMap<Hash256, SandboxEntry>,
    pre: &dyn Fn(&Hash256) -> Option<Vec<u8>>,
    tx_hash_hex: &str,
    ledger_seq: u32,
    only: Option<&HashSet<Hash256>>,
) {
    let hash_upper = tx_hash_hex.to_uppercase();
    let visit = |k: &Hash256| only.map(|s| s.contains(k)).unwrap_or(true);
    for (k, ent) in mods.iter_mut() {
        if !visit(k) {
            continue;
        }
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
        if !visit(k) {
            continue;
        }
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

/// Thread a `Batch` outer and its inners the way rippled's
/// `ApplyStateTable` ends up doing it: the outer's own application is
/// stamped with the outer's id first (its fee and sequence on its own
/// account root), then each inner re-stamps, in order, exactly the keys it
/// touched with the inner's own transaction id. The LAST toucher wins, which
/// is what a mainnet post-state records.
///
/// `inner_hashes` are the ledger's inner transaction ids in RawTransactions
/// order and `inner_touched` the engine's per-inner touched-key sets
/// (`tx::batch::take_inner_touched`), in the same order. The engine does not
/// hash transactions, so the two must be supplied together; if their lengths
/// disagree the pairing is unknown and this stamps the outer alone rather
/// than attributing a key to the wrong inner.
pub fn stamp_batch_threading(
    mods: &mut HashMap<Hash256, SandboxEntry>,
    pre: &dyn Fn(&Hash256) -> Option<Vec<u8>>,
    outer_hash_hex: &str,
    ledger_seq: u32,
    inner_hashes: &[String],
    inner_touched: &[Vec<Hash256>],
) {
    stamp_threading_keys(mods, pre, outer_hash_hex, ledger_seq, None);
    if inner_hashes.len() != inner_touched.len() {
        return;
    }
    for (hash, touched) in inner_hashes.iter().zip(inner_touched.iter()) {
        if touched.is_empty() {
            continue; // not applied, or applied and rolled back — touched nothing
        }
        let only: HashSet<Hash256> = touched.iter().copied().collect();
        stamp_threading_keys(mods, pre, hash, ledger_seq, Some(&only));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn k(n: u8) -> Hash256 {
        let mut b = [0u8; 32];
        b[31] = n;
        Hash256(b)
    }

    /// An AccountRoot image — the entry type `threadOwners` ignores, so these
    /// tests see the per-key stamping alone.
    fn root(n: u8, balance: u64) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "LedgerEntryType": "AccountRoot",
            "Account": hex::encode([n; 20]),
            "Balance": balance.to_string(),
        }))
        .expect("json")
    }

    /// The pre-transaction image of every key: a different Balance, so no
    /// entry below is a content-equal write-back.
    fn pre(h: &Hash256) -> Option<Vec<u8>> {
        Some(root(h.0[31], 1))
    }

    fn threading_of(mods: &HashMap<Hash256, SandboxEntry>, key: Hash256) -> (String, u64) {
        let bytes = match mods.get(&key) {
            Some(SandboxEntry::Created(b)) | Some(SandboxEntry::Modified(b)) => b,
            _ => panic!("no entry for the key"),
        };
        let v: serde_json::Value = serde_json::from_slice(bytes).expect("json");
        (
            v["PreviousTxnID"].as_str().unwrap_or_default().to_string(),
            v["PreviousTxnLgrSeq"].as_u64().unwrap_or_default(),
        )
    }

    fn three_roots() -> HashMap<Hash256, SandboxEntry> {
        let mut mods = HashMap::new();
        mods.insert(k(1), SandboxEntry::Modified(root(1, 10)));
        mods.insert(k(2), SandboxEntry::Modified(root(2, 20)));
        mods.insert(k(3), SandboxEntry::Modified(root(3, 30)));
        mods
    }

    #[test]
    fn stamp_batch_threading_gives_every_key_to_its_last_toucher() {
        let mut mods = three_roots();
        let inner_hashes = ["INNER1".to_string(), "INNER2".to_string()];
        // inner 1 touched B; inner 2 touched C and B again.
        let touched = [vec![k(2)], vec![k(3), k(2)]];
        stamp_batch_threading(&mut mods, &pre, "OUTER", 7, &inner_hashes, &touched);
        assert_eq!(threading_of(&mods, k(1)), ("OUTER".into(), 7), "no inner touched A");
        assert_eq!(threading_of(&mods, k(2)), ("INNER2".into(), 7), "the last inner to touch B wins");
        assert_eq!(threading_of(&mods, k(3)), ("INNER2".into(), 7));
    }

    #[test]
    fn stamp_batch_threading_falls_back_to_the_outer_on_a_length_mismatch() {
        let mut mods = three_roots();
        let inner_hashes = ["INNER1".to_string(), "INNER2".to_string()];
        let touched = [vec![k(2)]];
        stamp_batch_threading(&mut mods, &pre, "OUTER", 7, &inner_hashes, &touched);
        for n in 1..=3 {
            assert_eq!(threading_of(&mods, k(n)), ("OUTER".into(), 7), "key {n}");
        }
    }

    #[test]
    fn stamp_threading_keys_leaves_every_key_outside_the_restriction_alone() {
        let mut mods = three_roots();
        let only: std::collections::HashSet<Hash256> = [k(2)].into_iter().collect();
        stamp_threading_keys(&mut mods, &pre, "INNER1", 7, Some(&only));
        assert_eq!(threading_of(&mods, k(2)), ("INNER1".into(), 7));
        for n in [1u8, 3] {
            let bytes = match mods.get(&k(n)) {
                Some(SandboxEntry::Modified(b)) => b.clone(),
                _ => panic!("no entry"),
            };
            let v: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
            assert!(v.get("PreviousTxnID").is_none(), "key {n} was not in the restriction");
        }
    }
}
