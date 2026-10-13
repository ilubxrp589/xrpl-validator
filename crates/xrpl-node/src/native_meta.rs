//! Independence task 2.7: our own transaction records on the live validator, in shadow.
//!
//! Writer mode only (`XRPL_NATIVE_WRITER=1`, the live configuration): the check runs at the writer's commit,
//! after the gate's pull. With `XRPL_NATIVE_META=1`, the native shadow builds every transaction's metadata from the engine's own
//! changes as it applies the ledger ([`record`]), and ws-sync's transaction-tree gate, which already pulls
//! each ledger's binary transactions and metadata, hands them over at the end ([`check_ledger`]): each of our
//! records against the network's, byte for byte, and the transaction tree built from OUR records against the
//! header's root. Nothing it computes is used: it only counts, and logs the first differences of a ledger to
//! `XRPL_NATIVE_META_LOG` (default /mnt/xrpl-data/native_meta.jsonl; at most 3 lines a ledger, and nothing
//! once the file passes 20 MB, so a systematic miss cannot wear the disk).

use std::collections::HashMap;
use std::io::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

/// `XRPL_NATIVE_META=1`, read once.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("XRPL_NATIVE_META").map(|v| v == "1").unwrap_or(false))
}

#[derive(Default)]
pub struct MetaStats {
    pub ledgers: AtomicU64,
    pub records: AtomicU64,
    pub exact: AtomicU64,
    pub differ: AtomicU64,
    pub missing: AtomicU64,
    pub tree_match: AtomicU64,
    pub tree_differ: AtomicU64,
    pub tree_incomplete: AtomicU64,
    pub last_seq: AtomicU64,
}

pub fn stats() -> &'static MetaStats {
    static S: OnceLock<MetaStats> = OnceLock::new();
    S.get_or_init(MetaStats::default)
}

/// This ledger's records so far, by transaction id (upper hex): our encoded metadata, or why there is none.
static RECORDS: Mutex<Option<(u32, HashMap<String, Result<Vec<u8>, String>>)>> = Mutex::new(None);

/// Start ledger `seq`'s records (the shadow calls this before applying it).
pub fn begin(seq: u32) {
    *RECORDS.lock().unwrap_or_else(|e| e.into_inner()) = Some((seq, HashMap::new()));
}

/// Record one applied transaction of ledger `seq`: its own metadata and, for a Batch outer, each inner the
/// ledger filed under it. `index_of` gives a transaction's TransactionIndex and ParentBatchID as the ledger
/// records them (Step 2 keeps the network's positions; Step 3 makes them ours).
pub fn record(
    seq: u32,
    tx_hash: &str,
    tc: &crate::native_apply::TxChanges,
    index_of: &dyn Fn(&str) -> Option<(u64, Option<String>)>,
) {
    let mut g = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
    let Some((s, map)) = g.as_mut() else { return };
    if *s != seq {
        return;
    }
    let h = tx_hash.to_uppercase();
    if let Some((idx, _)) = index_of(&h) {
        map.insert(h.clone(), crate::meta_check::encode(&crate::meta_check::our_meta(tc, idx)));
    }
    for inner in &tc.inners {
        let id = inner.id.to_uppercase();
        if let Some((idx, Some(parent))) = index_of(&id) {
            if parent.eq_ignore_ascii_case(&h) {
                map.insert(id, crate::meta_check::encode(&crate::meta_check::inner_meta(inner, idx, &h)));
            }
        }
    }
}

/// Ledger `seq` is applied: judge our records against the network's `(tx_blob, meta)` pairs and our tree
/// against the header's root.
pub fn check_ledger(seq: u32, network: &[(Vec<u8>, Vec<u8>)], header_root: &[u8; 32]) {
    let Some((s, ours)) = RECORDS.lock().unwrap_or_else(|e| e.into_inner()).take() else { return };
    if s != seq || network.is_empty() {
        return;
    }
    let st = stats();
    st.ledgers.fetch_add(1, Ordering::Relaxed);
    st.last_seq.store(u64::from(seq), Ordering::Relaxed);
    let mut pairs = Vec::with_capacity(network.len());
    let mut receipts: Vec<Value> = Vec::new();
    for (blob, theirs) in network {
        let id = hex::encode_upper(xrpl_ledger::shamap::tx_tree::tx_id(blob).0);
        st.records.fetch_add(1, Ordering::Relaxed);
        match ours.get(&id) {
            Some(Ok(b)) => {
                match crate::meta_check::compare_bytes(b, theirs) {
                    Ok(()) => st.exact.fetch_add(1, Ordering::Relaxed),
                    Err(d) => {
                        receipts.push(json!({"seq": seq, "tx": id, "diff": d}));
                        st.differ.fetch_add(1, Ordering::Relaxed)
                    }
                };
                pairs.push((blob.clone(), b.clone()));
            }
            Some(Err(e)) => {
                receipts.push(json!({"seq": seq, "tx": id, "diff": e}));
                st.differ.fetch_add(1, Ordering::Relaxed);
            }
            None => {
                st.missing.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    if pairs.len() == network.len() {
        if xrpl_ledger::shamap::tx_tree::compute_tx_tree_root(&pairs).0 == *header_root {
            st.tree_match.fetch_add(1, Ordering::Relaxed);
        } else {
            st.tree_differ.fetch_add(1, Ordering::Relaxed);
            receipts.push(json!({"seq": seq, "tree": "our transaction tree root differs from the header's"}));
        }
    } else {
        st.tree_incomplete.fetch_add(1, Ordering::Relaxed);
    }
    if !receipts.is_empty() {
        log_receipts(&receipts);
    }
    let n = st.ledgers.load(Ordering::Relaxed);
    if n % 256 == 1 {
        eprintln!("[native-meta] heartbeat #{seq}: {}", summary());
    }
}

fn log_receipts(receipts: &[Value]) {
    let path = std::env::var("XRPL_NATIVE_META_LOG").unwrap_or_else(|_| "/mnt/xrpl-data/native_meta.jsonl".into());
    if std::fs::metadata(&path).map(|m| m.len() > 20 << 20).unwrap_or(false) {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        for r in receipts.iter().take(3) {
            let _ = writeln!(f, "{r}");
        }
    }
}

pub fn summary() -> String {
    let st = stats();
    let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
    format!(
        "ledgers {} | records {} exact {} differ {} missing {} | trees match {} differ {} incomplete {}",
        l(&st.ledgers), l(&st.records), l(&st.exact), l(&st.differ), l(&st.missing), l(&st.tree_match), l(&st.tree_differ), l(&st.tree_incomplete)
    )
}

/// The `/api/engine` section.
pub fn json() -> Value {
    let st = stats();
    let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
    json!({
        "enabled": enabled(), "last_seq": l(&st.last_seq), "ledgers": l(&st.ledgers),
        "records": l(&st.records), "exact": l(&st.exact), "differ": l(&st.differ), "missing": l(&st.missing),
        "tree_match": l(&st.tree_match), "tree_differ": l(&st.tree_differ), "tree_incomplete": l(&st.tree_incomplete),
    })
}

/// Extra /metrics lines.
pub fn metrics_text() -> String {
    let st = stats();
    let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
    let mut s = String::new();
    for (name, help, v) in [
        ("xrpl_native_meta_records_exact_total", "Our transaction records byte-identical to the network's.", l(&st.exact)),
        ("xrpl_native_meta_records_differ_total", "Our transaction records that differ from the network's.", l(&st.differ)),
        ("xrpl_native_meta_records_missing_total", "Network records we built none for.", l(&st.missing)),
        ("xrpl_native_meta_tree_match_total", "Ledgers whose transaction tree, from our records, matched the header.", l(&st.tree_match)),
        ("xrpl_native_meta_tree_differ_total", "Ledgers whose transaction tree, from our records, differed.", l(&st.tree_differ)),
        ("xrpl_native_meta_tree_incomplete_total", "Ledgers with a record missing or unencodable.", l(&st.tree_incomplete)),
    ] {
        s.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n{name} {v}\n"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ledger_without_records_or_network_pairs_counts_nothing() {
        begin(7);
        check_ledger(7, &[], &[0; 32]);
        assert_eq!(stats().ledgers.load(Ordering::Relaxed), 0, "no gate pairs (lag, gate off): not judged");
        check_ledger(8, &[(vec![1], vec![2])], &[0; 32]); // no records begun for #8
        assert_eq!(stats().ledgers.load(Ordering::Relaxed), 0);
    }
}
