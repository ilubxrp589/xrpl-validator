//! Independence task 3.3: the writer's own transaction order and results.
//!
//! The native shadow applies each ledger in the network's order, each transaction's TransactionIndex. Task
//! 3.3 moves it onto the order rippled builds a ledger in, from the transaction set alone: canonical order
//! and up to three passes ([`xrpl_ledger::ledger::close`]), each attempt judged on the engine's own result.
//!
//! - `XRPL_NATIVE_ORDER=shadow`: the shadow first applies the ledger our way, keeps what that gave (the order,
//!   each result, the bytes left behind), undoes it, and applies in the network's order as before. The writer
//!   offers the network-order apply's bytes. This only counts and logs whether our order, our results and our
//!   bytes are the network's.
//! - `XRPL_NATIVE_ORDER=on`: our order is the apply. A ledger where it disagrees with the network's order is
//!   a writer refusal, so the network's bytes are written instead; a result that disagrees already is one.
//! - unset (or anything else): off, as before.
//!
//! The ledger's differences go to `XRPL_NATIVE_ORDER_LOG` (default /mnt/xrpl-data/native_order.jsonl): one line
//! a ledger, and nothing once the file passes 20 MB, so a systematic miss cannot wear the disk.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use serde_json::{json, Value};
use xrpl_core::types::Hash256;
use xrpl_ledger::ledger::close::{ApplyOutcome, SeqProxy, TransactionEntry, ALWAYS_CLAIMED, LEDGER_TOTAL_PASSES};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    Shadow,
    On,
}

impl Mode {
    pub fn parse(v: Option<&str>) -> Self {
        match v {
            Some("shadow") => Mode::Shadow,
            Some("on") => Mode::On,
            _ => Mode::Off,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Shadow => "shadow",
            Mode::On => "on",
        }
    }
}

/// `XRPL_NATIVE_ORDER`, read once.
pub fn mode() -> Mode {
    static M: OnceLock<Mode> = OnceLock::new();
    *M.get_or_init(|| Mode::parse(std::env::var("XRPL_NATIVE_ORDER").ok().as_deref()))
}

#[derive(Default)]
pub struct OrderStats {
    pub ledgers: AtomicU64,
    /// Order, every result and (in shadow) every byte the network's.
    pub exact: AtomicU64,
    pub order_differ: AtomicU64,
    pub result_differ: AtomicU64,
    /// Task 3.4 live: the parent's total XRP less what our walk burned, against the header's.
    pub coins_match: AtomicU64,
    pub coins_differ: AtomicU64,
    pub bytes_differ: AtomicU64,
    /// `on`: ledgers our order refused.
    pub refused: AtomicU64,
    pub txs: AtomicU64,
    /// Transactions applied after the first pass.
    pub retried: AtomicU64,
    pub attempts: AtomicU64,
    pub last_seq: AtomicU64,
    pub ms_last: AtomicU64,
    /// Ledgers salted with the set a quorum of trusted validations accepted, and those salted with the root of the
    /// ledger's own transactions (no quorum named a set yet).
    pub salt_agreed: AtomicU64,
    pub salt_own: AtomicU64,
}

pub fn stats() -> &'static OrderStats {
    static S: OnceLock<OrderStats> = OnceLock::new();
    S.get_or_init(OrderStats::default)
}

/// The last ledger judged, and whether it was exact (independence 5.5 reads it when that ledger lands).
static LAST: std::sync::Mutex<Option<(u32, bool)>> = std::sync::Mutex::new(None);

/// Whether ledger `seq`'s order, results and (in shadow) bytes were the network's. None: not judged (order off,
/// or another ledger judged since).
pub fn exact_for(seq: u32) -> Option<bool> {
    let last = *LAST.lock().unwrap_or_else(|e| e.into_inner());
    last.filter(|(s, _)| *s == seq).map(|(_, e)| e)
}

/// The consensus set a ledger was built from, in rippled's canonical order: every transaction but the Batch
/// inners (`skip`, which apply inside their outer). A pseudo-transaction's account is zero (EnableAmendment spells
/// it rrrrrrrrrrrrrrrrrrrrrhoLvTp, UNLModify as "").
///
/// rippled salts with the consensus set's root (`CanonicalTXSet{result.txns.map_->getHash()}`), and that set can
/// hold transactions the ledger never filed: a tef drops out, a ter still waiting after the last pass carries over.
/// So `agreed`, the set a quorum of trusted validations named in their `sfConsensusHash`, is the salt when we have
/// it; without it, the root of the ledger's own transactions, which is the same set whenever every one was filed.
/// Mainnet #107652744, #107652745, #107652752 (soak #48): our order differed at position 0 on exactly the three
/// ledgers whose own root was not the validators' set (13 neighbours agreed).
pub fn canonical_set(ordered: &[&Value], skip: &HashSet<String>, agreed: Option<[u8; 32]>) -> Vec<TransactionEntry> {
    let mut entries: Vec<TransactionEntry> = ordered
        .iter()
        .filter_map(|t| {
            let h = t["hash"].as_str()?.to_uppercase();
            if skip.contains(&h) {
                return None;
            }
            let hash = <[u8; 32]>::try_from(hex::decode(&h).ok()?.as_slice()).ok()?;
            let account = t["Account"].as_str().and_then(crate::native_apply::decode_address).unwrap_or([0u8; 20]);
            let seq_proxy = SeqProxy::of(
                t["Sequence"].as_u64().unwrap_or(0) as u32,
                t.get("TicketSequence").and_then(Value::as_u64).map(|v| v as u32),
            );
            Some(TransactionEntry { hash: Hash256(hash), account, seq_proxy, blob: Vec::new() })
        })
        .collect();
    let salt = match agreed {
        Some(set) => {
            stats().salt_agreed.fetch_add(1, Ordering::Relaxed);
            Hash256(set)
        }
        None => {
            stats().salt_own.fetch_add(1, Ordering::Relaxed);
            xrpl_ledger::ledger::close::consensus_salt(&entries.iter().map(|e| e.hash).collect::<Vec<_>>())
        }
    };
    xrpl_ledger::ledger::close::canonical_order(&mut entries, &salt);
    entries
}

/// How BuildLedger's applyTransactions takes one attempt's result: tes applies; ter waits; a tec waits while a
/// retry is certain, unless doApply returned one of the codes rippled always claims; tef, tem, tel and
/// anything else fail.
pub fn classify(code: &str, certain_retry: bool, from_do_apply: bool) -> ApplyOutcome {
    match &code[..3.min(code.len())] {
        "tes" => ApplyOutcome::Success,
        "ter" => ApplyOutcome::Retry,
        "tec" if certain_retry && !(from_do_apply && ALWAYS_CLAIMED.contains(&code)) => ApplyOutcome::Retry,
        "tec" => ApplyOutcome::Success,
        _ => ApplyOutcome::Fail,
    }
}

/// The network's order of the set: TransactionIndex order (`ordered` is already in it) without the inners.
pub fn network_order(ordered: &[&Value], skip: &HashSet<String>) -> Vec<String> {
    ordered
        .iter()
        .filter_map(|t| t["hash"].as_str().map(str::to_uppercase))
        .filter(|h| !skip.contains(h))
        .collect()
}

/// What applying a ledger our way did.
#[derive(Debug, Default, Clone)]
pub struct Walked {
    /// (pass, transaction id, result), in the order they applied.
    pub applied: Vec<(usize, String, String)>,
    pub failed: Vec<String>,
    pub left: Vec<String>,
    pub attempts: u64,
}

/// One ledger's comparison with the network's.
#[derive(Debug, Default)]
pub struct Verdict {
    /// Where our order first leaves the network's, said in words.
    pub order: Option<String>,
    /// Our result against the network's, for each transaction they differ on (and each one we left out).
    pub results: Vec<String>,
    pub per_pass: Vec<usize>,
    /// The header's total XRP against the parent's less what we burned, when they differ (the caller sets it).
    pub coins: Option<String>,
    /// Whether the total was checked (the parent's total is known only after a consecutive ledger).
    pub coins_checked: bool,
}

pub fn verdict(walked: &Walked, network: &[String], network_result: &dyn Fn(&str) -> Option<String>) -> Verdict {
    let ours: Vec<&str> = walked.applied.iter().map(|(_, h, _)| h.as_str()).collect();
    let order = if ours.iter().copied().eq(network.iter().map(String::as_str)) {
        None
    } else {
        let i = ours.iter().zip(network).position(|(a, b)| *a != b.as_str()).unwrap_or(ours.len().min(network.len()));
        let short = |h: Option<&str>| h.map(|h| h[..12.min(h.len())].to_string()).unwrap_or_else(|| "-".into());
        Some(format!(
            "differs at position {i}: ours {} vs network {} ({} ours, {} network; {} failed, {} left)",
            short(ours.get(i).copied()),
            short(network.get(i).map(String::as_str)),
            ours.len(),
            network.len(),
            walked.failed.len(),
            walked.left.len()
        ))
    };
    let mine: HashMap<&str, &str> = walked.applied.iter().map(|(_, h, c)| (h.as_str(), c.as_str())).collect();
    let mut results = Vec::new();
    for h in network {
        let theirs = network_result(h).unwrap_or_default();
        match mine.get(h.as_str()) {
            Some(c) if *c == theirs => {}
            Some(c) => results.push(format!("{} ours={c} network={theirs}", &h[..12.min(h.len())])),
            None => results.push(format!("{} not applied by us, network={theirs}", &h[..12.min(h.len())])),
        }
    }
    let per_pass = (0..LEDGER_TOTAL_PASSES).map(|p| walked.applied.iter().filter(|(q, _, _)| *q == p).count()).collect();
    Verdict { order, results, per_pass, coins: None, coins_checked: false }
}

/// Task 3.4 live: the parent's total XRP less the fees our walk burned, against the header's. None: it holds.
pub fn coins_verdict(parent_total: u64, burned: u64, header_total: u64) -> Option<String> {
    let ours = parent_total.checked_sub(burned);
    (ours != Some(header_total)).then(|| {
        format!("total XRP: ours {} (parent {parent_total} less {burned} burned) vs header {header_total}", ours.map_or("-".into(), |o| o.to_string()))
    })
}

/// A ledger's comparison, counted: the refusal `on` makes of it, and the receipt line to log when anything
/// differed ([`log_receipt`]).
#[derive(Debug, Default)]
pub struct Judged {
    pub refusal: Option<String>,
    pub receipt: Option<Value>,
}

/// Count ledger `seq`'s comparison. `bytes`: in shadow, the objects whose bytes our order left different from
/// the network order's (their number and the first).
pub fn judge(seq: u32, mode: Mode, v: &Verdict, walked: &Walked, bytes: Option<(usize, Option<Hash256>)>, ms: u64) -> Judged {
    let st = stats();
    st.ledgers.fetch_add(1, Ordering::Relaxed);
    st.last_seq.store(u64::from(seq), Ordering::Relaxed);
    st.ms_last.store(ms, Ordering::Relaxed);
    st.txs.fetch_add(walked.applied.len() as u64, Ordering::Relaxed);
    st.attempts.fetch_add(walked.attempts, Ordering::Relaxed);
    st.retried.fetch_add(walked.applied.iter().filter(|(p, _, _)| *p > 0).count() as u64, Ordering::Relaxed);
    let bytes_n = bytes.map_or(0, |b| b.0);
    if v.order.is_some() {
        st.order_differ.fetch_add(1, Ordering::Relaxed);
    }
    if !v.results.is_empty() {
        st.result_differ.fetch_add(1, Ordering::Relaxed);
    }
    if bytes_n > 0 {
        st.bytes_differ.fetch_add(1, Ordering::Relaxed);
    }
    if v.coins.is_some() {
        st.coins_differ.fetch_add(1, Ordering::Relaxed);
    } else if v.coins_checked {
        st.coins_match.fetch_add(1, Ordering::Relaxed);
    }
    let exact = v.order.is_none() && v.results.is_empty() && bytes_n == 0 && v.coins.is_none();
    *LAST.lock().unwrap_or_else(|e| e.into_inner()) = Some((seq, exact));
    let mut receipt = None;
    if exact {
        st.exact.fetch_add(1, Ordering::Relaxed);
    } else {
        let mut line = json!({
            "seq": seq, "mode": mode.name(), "order": v.order, "results": v.results.iter().take(5).collect::<Vec<_>>(),
            "results_n": v.results.len(), "passes": v.per_pass, "attempts": walked.attempts, "ms": ms,
        });
        if let Some(c) = &v.coins {
            line["coins"] = json!(c);
        }
        if let Some((n, first)) = bytes {
            line["bytes"] = json!({ "differ": n, "first": first.map(|k| hex::encode_upper(k.0)) });
        }
        receipt = Some(line);
    }
    let n = st.ledgers.load(Ordering::Relaxed);
    if n % 256 == 1 {
        eprintln!("[native-order] heartbeat #{seq} ({}): {}", mode.name(), summary());
    }
    let refusal = match (mode, &v.order) {
        (Mode::On, Some(o)) => Some(format!("order {o}")),
        _ => None,
    };
    if refusal.is_some() {
        st.refused.fetch_add(1, Ordering::Relaxed);
    }
    Judged { refusal, receipt }
}

/// `XRPL_NATIVE_ORDER_LOG`, else /mnt/xrpl-data/native_order.jsonl.
pub fn log_path() -> std::path::PathBuf {
    std::env::var_os("XRPL_NATIVE_ORDER_LOG").map(Into::into).unwrap_or_else(|| "/mnt/xrpl-data/native_order.jsonl".into())
}

/// Append one receipt line, unless the file has passed 20 MB.
pub fn log_receipt(path: &std::path::Path, line: &Value) {
    if std::fs::metadata(path).map(|m| m.len() > 20 << 20).unwrap_or(false) {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

pub fn summary() -> String {
    let st = stats();
    let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
    format!(
        "ledgers {} exact {} | order differ {} results differ {} bytes differ {} total XRP match {} differ {} refused {} | txs {} applied after pass 1 {} attempts {}",
        l(&st.ledgers), l(&st.exact), l(&st.order_differ), l(&st.result_differ), l(&st.bytes_differ), l(&st.coins_match), l(&st.coins_differ), l(&st.refused),
        l(&st.txs), l(&st.retried), l(&st.attempts)
    )
}

/// The `/api/engine` section.
pub fn json() -> Value {
    let st = stats();
    let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
    json!({
        "mode": mode().name(), "last_seq": l(&st.last_seq), "ledgers": l(&st.ledgers), "exact": l(&st.exact),
        "order_differ": l(&st.order_differ), "result_differ": l(&st.result_differ), "bytes_differ": l(&st.bytes_differ),
        "coins_match": l(&st.coins_match), "coins_differ": l(&st.coins_differ), "refused": l(&st.refused), "txs": l(&st.txs), "retried": l(&st.retried), "attempts": l(&st.attempts),
        "ms_last": l(&st.ms_last), "salt_agreed": l(&st.salt_agreed), "salt_own": l(&st.salt_own),
    })
}

/// Extra /metrics lines.
pub fn metrics_text() -> String {
    let st = stats();
    let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
    let mut s = String::new();
    let mode_n = match mode() {
        Mode::Off => 0,
        Mode::Shadow => 1,
        Mode::On => 2,
    };
    s.push_str(&format!(
        "# HELP xrpl_native_order_mode Our own transaction order: 0 off, 1 shadow, 2 on.\n# TYPE xrpl_native_order_mode gauge\nxrpl_native_order_mode {mode_n}\n"
    ));
    for (name, help, v) in [
        ("xrpl_native_order_ledgers_total", "Ledgers applied in our own order and passes.", l(&st.ledgers)),
        ("xrpl_native_order_exact_total", "Ledgers whose order, results and bytes were the network's.", l(&st.exact)),
        ("xrpl_native_order_order_differ_total", "Ledgers where our order differed from the network's.", l(&st.order_differ)),
        ("xrpl_native_order_result_differ_total", "Ledgers where a result of ours differed from the network's.", l(&st.result_differ)),
        ("xrpl_native_order_bytes_differ_total", "Ledgers where our order left bytes different from the network order's.", l(&st.bytes_differ)),
        ("xrpl_native_order_coins_match_total", "Ledgers whose total XRP, the parent's less our burn, matched the header's.", l(&st.coins_match)),
        ("xrpl_native_order_coins_differ_total", "Ledgers whose total XRP, the parent's less our burn, differed from the header's.", l(&st.coins_differ)),
        ("xrpl_native_order_refused_total", "Ledgers the writer refused because our order differed (mode on).", l(&st.refused)),
        ("xrpl_native_order_retried_total", "Transactions applied after the first pass.", l(&st.retried)),
    ] {
        s.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n{name} {v}\n"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tx(hash: char, account: &str, seq: u64, result: &str, index: u64) -> Value {
        json!({ "hash": hash.to_string().repeat(64), "Account": account, "Sequence": seq,
                "metaData": { "TransactionResult": result, "TransactionIndex": index } })
    }

    #[test]
    fn the_mode_is_off_unless_named() {
        assert_eq!(Mode::parse(None), Mode::Off);
        assert_eq!(Mode::parse(Some("1")), Mode::Off, "only the words switch it");
        assert_eq!(Mode::parse(Some("shadow")), Mode::Shadow);
        assert_eq!(Mode::parse(Some("on")), Mode::On);
    }

    #[test]
    fn results_are_judged_as_build_ledger_judges_them() {
        assert_eq!(classify("tesSUCCESS", true, true), ApplyOutcome::Success);
        assert_eq!(classify("terPRE_SEQ", false, false), ApplyOutcome::Retry);
        assert_eq!(classify("tecPATH_DRY", true, true), ApplyOutcome::Retry, "a tec waits while a retry is certain");
        assert_eq!(classify("tecPATH_DRY", false, true), ApplyOutcome::Success, "then it is claimed");
        assert_eq!(classify("tecKILLED", true, true), ApplyOutcome::Success, "doApply's tecKILLED is always claimed");
        assert_eq!(classify("tecEXPIRED", true, false), ApplyOutcome::Retry, "the same code from preclaim waits");
        assert_eq!(classify("tefPAST_SEQ", true, false), ApplyOutcome::Fail);
        assert_eq!(classify("temMALFORMED", false, false), ApplyOutcome::Fail);
        assert_eq!(classify("", false, false), ApplyOutcome::Fail);
    }

    /// rippled salts with the consensus set's root, which the validators name; the ledger's own root stands in only
    /// when no quorum named one (mainnet #107652744).
    #[test]
    fn the_salt_is_the_agreed_set_when_there_is_one() {
        use xrpl_ledger::ledger::close::{canonical_order, consensus_salt};
        let txs: Vec<Value> = [
            ('A', "rpZNAnHcvr6TbaY7QJa9yrVfu6coDz9pPH"), // accounts 0x11…, 0x22…, 0x33…, 0x44…
            ('B', "rhf7192NqpPvBUnAobBJAryNFQNbPKz11w"),
            ('C', "rnC5oDiiksa4mHdRUtGTupTMjaiPXzGs18"),
            ('D', "rfDxjJ97Cs8qMPwtcBMc1pA7do7B8J5trV"),
        ]
        .iter()
        .enumerate()
        .map(|(i, (h, a))| tx(*h, a, 1, "tesSUCCESS", i as u64))
        .collect();
        let ordered: Vec<&Value> = txs.iter().collect();
        let order = |set: &[TransactionEntry]| set.iter().map(|e| e.hash.0[0]).collect::<Vec<u8>>();
        let mut want_own = canonical_set(&ordered, &HashSet::new(), None);
        canonical_order(&mut want_own, &consensus_salt(&want_own.iter().map(|e| e.hash).collect::<Vec<_>>()));
        assert_eq!(order(&canonical_set(&ordered, &HashSet::new(), None)), order(&want_own));
        // Salts that order the four accounts differently (0x11 ^ 0x00 < 0x22 ^ 0x00, but 0x11 ^ 0x33 > 0x22 ^ 0x33).
        assert_ne!(order(&canonical_set(&ordered, &HashSet::new(), Some([0; 32]))), order(&canonical_set(&ordered, &HashSet::new(), Some([0x33; 32]))));
        for agreed in [[0u8; 32], [0xFF; 32], [0x33; 32]] {
            let mut want = canonical_set(&ordered, &HashSet::new(), None);
            canonical_order(&mut want, &Hash256(agreed));
            assert_eq!(order(&canonical_set(&ordered, &HashSet::new(), Some(agreed))), order(&want));
        }
    }

    #[test]
    fn the_set_leaves_out_the_inners_and_gives_a_pseudo_transaction_account_zero() {
        let a = tx('A', "rHb9CJAWyB4rj91VRWn96DkukG4bwdtyTh", 5, "tesSUCCESS", 0);
        let b = tx('B', "", 0, "tesSUCCESS", 1);
        let c = tx('C', "rHb9CJAWyB4rj91VRWn96DkukG4bwdtyTh", 6, "tesSUCCESS", 2);
        let ordered = vec![&a, &b, &c];
        let skip: HashSet<String> = ["C".repeat(64)].into_iter().collect();
        let set = canonical_set(&ordered, &skip, None);
        assert_eq!(set.len(), 2);
        assert!(set.iter().any(|e| e.account == [0u8; 20] && e.hash.0 == [0xBB; 32]));
        assert_eq!(network_order(&ordered, &skip), vec!["A".repeat(64), "B".repeat(64)]);
    }

    #[test]
    fn the_verdict_names_the_first_difference_and_each_result() {
        let (a, b, c) = ("A".repeat(64), "B".repeat(64), "C".repeat(64));
        let network = vec![a.clone(), b.clone(), c.clone()];
        let result = |h: &str| Some(if h.starts_with('C') { "tecPATH_DRY" } else { "tesSUCCESS" }.to_string());
        let same = Walked {
            applied: vec![(0, a.clone(), "tesSUCCESS".into()), (0, b.clone(), "tesSUCCESS".into()), (2, c.clone(), "tecPATH_DRY".into())],
            attempts: 5,
            ..Default::default()
        };
        let v = verdict(&same, &network, &result);
        assert!(v.order.is_none() && v.results.is_empty());
        assert_eq!(v.per_pass, vec![2, 0, 1]);

        let swapped = Walked {
            applied: vec![(0, b.clone(), "tesSUCCESS".into()), (0, a.clone(), "tesSUCCESS".into())],
            left: vec![c.clone()],
            attempts: 5,
            ..Default::default()
        };
        let v = verdict(&swapped, &network, &result);
        let o = v.order.unwrap();
        assert!(o.starts_with("differs at position 0: ours BBBBBBBBBBBB vs network AAAAAAAAAAAA"), "{o}");
        assert!(o.contains("2 ours, 3 network; 0 failed, 1 left"), "{o}");
        assert_eq!(v.results, vec!["CCCCCCCCCCCC not applied by us, network=tecPATH_DRY".to_string()]);
    }

    #[test]
    fn total_xrp_is_the_parents_less_the_burn() {
        assert_eq!(coins_verdict(1_000, 12, 988), None);
        let c = coins_verdict(1_000, 10, 988).unwrap();
        assert_eq!(c, "total XRP: ours 990 (parent 1000 less 10 burned) vs header 988");
        assert!(coins_verdict(5, 10, 0).unwrap().starts_with("total XRP: ours - "), "a burn above the total never matches");
    }

    #[test]
    fn every_metrics_line_starts_at_the_margin() {
        for line in metrics_text().lines() {
            assert!(!line.starts_with(' '), "{line:?}");
        }
    }
}
