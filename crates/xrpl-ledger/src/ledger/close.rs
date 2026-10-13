//! Canonical transaction order: the order rippled applies a consensus transaction set in (independence task
//! 3.1).
//!
//! rippled puts the agreed set into a `CanonicalTXSet` (include/xrpl/ledger/CanonicalTXSet.h and
//! src/libxrpl/ledger/CanonicalTXSet.cpp in 3.4.1), salted with the set's own hash
//! (src/xrpld/app/consensus/RCLConsensus.cpp:502), and applies it in that key order:
//! 1. the account, copied into the first 20 bytes of a 256-bit value and XORed with the salt, compared as bytes;
//! 2. the [`SeqProxy`]: a sequence before a ticket, then by value;
//! 3. the transaction id.
//!
//! The salt is the root of the transaction set without metadata ([`consensus_salt`]). Every transaction in the
//! set goes in, pseudo-transactions included (their account is zero). A Batch's inner transactions are not
//! in the set: they apply inside their outer, and take the indices right after it.
//!
//! The order a ledger records (each transaction's TransactionIndex) is this one only when every transaction
//! applies on the first pass. `BuildLedger.cpp`'s applyTransactions makes up to three passes over the set,
//! and a transaction that asks to be retried applies in a later pass, after every first-pass one
//! ([`apply_in_passes`], task 3.2).
//!
//! Not yet called by the live validator: Step 3 (task 3.3) moves the writer onto it.

use xrpl_core::types::Hash256;

/// rippled's `SeqProxy` (include/xrpl/protocol/SeqProxy.h): a transaction's sequence or ticket. A sequence
/// sorts before a ticket, then each by value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SeqProxy {
    Sequence(u32),
    Ticket(u32),
}

impl SeqProxy {
    /// `STTx::getSeqProxy`: a non-zero Sequence; else the TicketSequence when there is one; else Sequence 0.
    pub fn of(sequence: u32, ticket_sequence: Option<u32>) -> Self {
        match (sequence, ticket_sequence) {
            (0, Some(ticket)) => SeqProxy::Ticket(ticket),
            (sequence, _) => SeqProxy::Sequence(sequence),
        }
    }
}

/// A transaction with the fields its canonical position depends on.
#[derive(Debug, Clone)]
pub struct TransactionEntry {
    /// Transaction id (SHA512Half of TXN\0 || blob).
    pub hash: Hash256,
    /// The account that sent it (20-byte account id; zero for a pseudo-transaction).
    pub account: [u8; 20],
    /// Its sequence or ticket.
    pub seq_proxy: SeqProxy,
    /// Raw transaction blob.
    pub blob: Vec<u8>,
}

/// The salt rippled orders a consensus set with: the set's hash, the root of its SHAMap of transactions
/// without metadata (`compute_tx_set_root`).
pub fn consensus_salt(set: &[Hash256]) -> Hash256 {
    crate::shamap::tx_set::compute_tx_set_root(set)
}

/// `CanonicalTXSet::accountKey`: the account in the first 20 bytes of 32, XORed with the salt.
pub fn account_key(account: &[u8; 20], salt: &Hash256) -> [u8; 32] {
    let mut key = [0u8; 32];
    key[..20].copy_from_slice(account);
    for (k, s) in key.iter_mut().zip(salt.0) {
        *k ^= s;
    }
    key
}

/// Sort a consensus set into rippled's canonical order, salted with `salt` (normally [`consensus_salt`] of
/// the same set).
pub fn canonical_order(txs: &mut [TransactionEntry], salt: &Hash256) {
    txs.sort_by_cached_key(|t| (account_key(&t.account, salt), t.seq_proxy, t.hash.0));
}

/// What applying one transaction did: rippled's `ApplyTransactionResult` (src/libxrpl/tx/apply.cpp).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// Applied: tesSUCCESS, or a tec whose fee was claimed.
    Success,
    /// Can never apply (tef, tem, tel, or an exception): dropped from the set.
    Fail,
    /// Not now (a ter, or a tec while a retry is still certain): kept for the next pass.
    Retry,
}

/// `LEDGER_TOTAL_PASSES` (src/xrpld/app/ledger/OpenLedger.h): passes over the set at most.
pub const LEDGER_TOTAL_PASSES: usize = 3;
/// `LEDGER_RETRY_PASSES`: the last pass that may still be a certain-retry one.
pub const LEDGER_RETRY_PASSES: usize = 1;

/// A canonical set applied in passes ([`apply_in_passes`]).
#[derive(Debug)]
pub struct Passes<T> {
    /// What applied, in the order it applied (the ledger's TransactionIndex order), with its pass.
    pub applied: Vec<(usize, T)>,
    /// What failed for good.
    pub failed: Vec<T>,
    /// What still wanted a retry after the last pass: left out of the ledger.
    pub left: Vec<T>,
}

/// rippled's `applyTransactions` (src/xrpld/app/ledger/detail/BuildLedger.cpp, 3.4.1), over a set already
/// in canonical order. `apply(tx, certain_retry)` applies one transaction; `certain_retry` is rippled's
/// `retryAssured`, which sets `tapRETRY`. Under it a tec is not claimed but retried (`isTecClaimHardFail`),
/// except tecOVERSIZE, tecKILLED, tecINCOMPLETE and tecEXPIRED, which are always claimed (Transactor.cpp).
/// Retries stay certain through pass LEDGER_RETRY_PASSES while each pass changes something; after a pass that
/// changes nothing, the next one claims, and a claiming pass that changes nothing ends it. (rippled also skips
/// a transaction the ledger being rebuilt already holds; a fresh ledger holds none.)
pub fn apply_in_passes<T>(set: Vec<T>, mut apply: impl FnMut(&T, bool) -> ApplyOutcome) -> Passes<T> {
    let (mut set, mut applied, mut failed) = (set, Vec::new(), Vec::new());
    let mut certain_retry = true;
    for pass in 0..LEDGER_TOTAL_PASSES {
        let mut changes = 0;
        let mut retry = Vec::new();
        for tx in set {
            match apply(&tx, certain_retry) {
                ApplyOutcome::Success => {
                    applied.push((pass, tx));
                    changes += 1;
                }
                ApplyOutcome::Fail => failed.push(tx),
                ApplyOutcome::Retry => retry.push(tx),
            }
        }
        set = retry;
        if changes == 0 && !certain_retry {
            break;
        }
        if changes == 0 || pass >= LEDGER_RETRY_PASSES {
            certain_retry = false;
        }
    }
    Passes { applied, failed, left: set }
}

/// The tec results rippled claims even while a retry is certain, when a transactor's doApply returns them
/// (Transactor.cpp: tecOVERSIZE, tecKILLED, tecINCOMPLETE and tecEXPIRED go to processPersistentChanges
/// whatever the flags). The same code from preclaim waits like any tec: preclaim's `likelyToClaimFee` is
/// false under tapRETRY (applySteps.h), so the transaction is not applied and is retried.
pub const ALWAYS_CLAIMED: [&str; 4] = ["tecOVERSIZE", "tecKILLED", "tecINCOMPLETE", "tecEXPIRED"];

#[cfg(test)]
mod tests {
    use super::*;

    fn tx(account_byte: u8, seq_proxy: SeqProxy, hash_byte: u8) -> TransactionEntry {
        TransactionEntry { hash: Hash256([hash_byte; 32]), account: [account_byte; 20], seq_proxy, blob: vec![] }
    }

    fn order(txs: &[TransactionEntry]) -> Vec<u8> {
        txs.iter().map(|t| t.hash.0[0]).collect()
    }

    #[test]
    fn the_salt_reorders_accounts() {
        let mut txs = vec![tx(0x10, SeqProxy::Sequence(1), 1), tx(0x20, SeqProxy::Sequence(1), 2), tx(0x30, SeqProxy::Sequence(1), 3)];
        canonical_order(&mut txs, &Hash256([0; 32]));
        assert_eq!(order(&txs), [1, 2, 3], "a zero salt keeps the plain account order");
        // 0x10^0x30 = 0x20, 0x20^0x30 = 0x10, 0x30^0x30 = 0x00
        canonical_order(&mut txs, &Hash256([0x30; 32]));
        assert_eq!(order(&txs), [3, 2, 1]);
    }

    #[test]
    fn the_salt_covers_all_32_bytes_but_only_the_first_20_hold_the_account() {
        let mut salt = [0u8; 32];
        salt[31] = 0xFF;
        let key = account_key(&[0xAB; 20], &Hash256(salt));
        assert_eq!(&key[..20], &[0xAB; 20]);
        assert_eq!(&key[20..31], &[0u8; 11]);
        assert_eq!(key[31], 0xFF);
    }

    #[test]
    fn one_account_goes_sequences_first_then_tickets_then_by_id() {
        let mut txs = vec![
            tx(0x10, SeqProxy::Ticket(3), 1),
            tx(0x10, SeqProxy::Sequence(9), 2),
            tx(0x10, SeqProxy::Ticket(2), 3),
            tx(0x10, SeqProxy::Sequence(8), 4),
            tx(0x10, SeqProxy::Ticket(2), 0),
        ];
        canonical_order(&mut txs, &Hash256([0x5A; 32]));
        assert_eq!(order(&txs), [4, 2, 0, 3, 1]);
    }

    #[test]
    fn seq_proxy_follows_sttx_get_seq_proxy() {
        assert_eq!(SeqProxy::of(5, None), SeqProxy::Sequence(5));
        assert_eq!(SeqProxy::of(5, Some(9)), SeqProxy::Sequence(5), "a non-zero Sequence wins");
        assert_eq!(SeqProxy::of(0, Some(9)), SeqProxy::Ticket(9));
        assert_eq!(SeqProxy::of(0, None), SeqProxy::Sequence(0), "a pseudo-transaction");
        assert!(SeqProxy::Sequence(u32::MAX) < SeqProxy::Ticket(0));
    }

    /// A pass-test closure: `tes` applies, `tec` is claimed only once retries stop, `wait:X` waits for X,
    /// `fail` fails, `never` always retries.
    fn passes(set: &[&'static str]) -> Passes<&'static str> {
        let mut done = std::collections::HashSet::new();
        apply_in_passes(set.to_vec(), |tx: &&str, certain| {
            let (name, how) = tx.split_once(' ').unwrap();
            let outcome = match how {
                "tes" => ApplyOutcome::Success,
                "tec" if certain => ApplyOutcome::Retry,
                "tec" => ApplyOutcome::Success,
                "fail" => ApplyOutcome::Fail,
                "never" => ApplyOutcome::Retry,
                wait => if done.contains(wait.trim_start_matches("wait:")) { ApplyOutcome::Success } else { ApplyOutcome::Retry },
            };
            if outcome == ApplyOutcome::Success {
                done.insert(name.to_string());
            }
            outcome
        })
    }

    #[test]
    fn every_success_applies_in_the_first_pass() {
        assert_eq!(passes(&["a tes", "b tes", "c tes"]).applied, vec![(0, "a tes"), (0, "b tes"), (0, "c tes")]);
    }

    #[test]
    fn a_tec_is_claimed_only_once_retries_stop() {
        // Pass 0 applies a and c; pass 1 still retries with certainty and changes nothing; pass 2 claims b.
        assert_eq!(passes(&["a tes", "b tec", "c tes"]).applied, vec![(0, "a tes"), (0, "c tes"), (2, "b tec")]);
    }

    #[test]
    fn a_pass_that_changes_nothing_ends_the_certain_retries() {
        assert_eq!(passes(&["b tec"]).applied, vec![(1, "b tec")]);
    }

    #[test]
    fn a_transaction_waiting_on_a_later_one_applies_in_the_next_pass() {
        assert_eq!(passes(&["x wait:y", "y tes"]).applied, vec![(0, "y tes"), (1, "x wait:y")]);
    }

    #[test]
    fn what_never_gets_ready_is_left_out_and_a_failure_is_dropped() {
        let p = passes(&["n never", "f fail", "s tes"]);
        assert_eq!(p.applied, vec![(0, "s tes")]);
        assert_eq!(p.failed, vec!["f fail"]);
        assert_eq!(p.left, vec!["n never"]);
    }

    #[test]
    fn empty_and_single_sets_are_fine() {
        let mut none: Vec<TransactionEntry> = vec![];
        canonical_order(&mut none, &Hash256([1; 32]));
        let mut one = vec![tx(0x01, SeqProxy::Sequence(1), 0xFF)];
        canonical_order(&mut one, &Hash256([1; 32]));
        assert_eq!(order(&one), [0xFF]);
    }

    /// Independence task 3.1's check against mainnet: for every ledger of a `ledger` answer store
    /// (scripts/fetch_meta_ledgers.py json/), the set is the ledger's transactions without Batch inners, the
    /// salt is its root, and our canonical order is compared with mainnet's TransactionIndex order. A ledger
    /// that differs must be explained by rippled's retry passes: mainnet's order splits into at most three runs,
    /// each in our order (the first pass, then the retries). Batch inners must sit right after their outer.
    ///
    ///   XRPL_META_CORPUS=<json folder> cargo test -p xrpl-ledger --lib canonical_order_against -- --ignored --nocapture
    #[test]
    #[ignore]
    fn canonical_order_against_mainnet() {
        use serde_json::Value;
        fn index_of(t: &Value) -> u64 {
            t["metaData"]["TransactionIndex"].as_u64().unwrap()
        }
        fn id(t: &Value) -> Hash256 {
            Hash256(hex::decode(t["hash"].as_str().unwrap()).unwrap().try_into().unwrap())
        }
        fn is_inner(t: &Value) -> bool {
            t["metaData"].get("ParentBatchID").is_some()
                || t["Flags"].as_u64().unwrap_or(0) & crate::tx::batch::TF_INNER_BATCH_TXN != 0
        }
        let dir = std::env::var_os("XRPL_META_CORPUS").expect("XRPL_META_CORPUS: a folder of `ledger` answers");
        let mut paths: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        paths.sort();
        assert!(!paths.is_empty(), "no ledgers in {dir:?}");
        let (mut txs_seen, mut exact, mut retried, mut pseudo_ledgers) = (0usize, 0usize, Vec::new(), 0usize);
        let (mut unexplained, mut inner_misplaced, mut inners) = (Vec::new(), Vec::new(), 0usize);
        // For task 3.2: how many runs, which results apply after the first run, and how often mainnet's order is
        // simply every non-tec transaction in our order followed by every tec one (rippled defers a tec claim
        // while it can still retry, tapRETRY).
        let mut runs_seen = std::collections::BTreeMap::<usize, usize>::new();
        let mut later_results = std::collections::BTreeMap::<String, usize>::new();
        let mut tec_last = 0usize;
        for path in &paths {
            let doc: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            let doc = doc.get("result").unwrap_or(&doc);
            let seq = doc["ledger_index"].as_u64().unwrap_or_default();
            let all = doc["ledger"]["transactions"].as_array().unwrap();
            txs_seen += all.len();
            // Batch inners: right after their outer, in order.
            let by_id: std::collections::HashMap<String, &Value> =
                all.iter().map(|t| (t["hash"].as_str().unwrap().to_uppercase(), t)).collect();
            let mut followers: std::collections::BTreeMap<u64, Vec<u64>> = Default::default();
            for t in all.iter().filter(|t| is_inner(t)) {
                inners += 1;
                let parent = t["metaData"]["ParentBatchID"].as_str().unwrap_or("").to_uppercase();
                match by_id.get(&parent) {
                    Some(outer) => followers.entry(index_of(outer)).or_default().push(index_of(t)),
                    None => inner_misplaced.push(format!("#{seq} inner {} names an outer not in the ledger", &t["hash"].as_str().unwrap()[..12])),
                }
            }
            for (outer, mut after) in followers {
                after.sort_unstable();
                if after != (outer + 1..=outer + after.len() as u64).collect::<Vec<_>>() {
                    inner_misplaced.push(format!("#{seq} outer at {outer}: inners at {after:?}"));
                }
            }
            // The set, salted with its own root, against mainnet's order.
            let set: Vec<&Value> = all.iter().filter(|t| !is_inner(t)).collect();
            let mut entries: Vec<TransactionEntry> = set
                .iter()
                .map(|t| TransactionEntry {
                    hash: id(t),
                    // A pseudo-transaction's account is zero: EnableAmendment spells it rrrrrrrrrrrrrrrrrrrrrhoLvTp,
                    // UNLModify as "" (an STAccount never set, serialized empty).
                    account: match t["Account"].as_str() {
                        Some("") => [0u8; 20],
                        Some(a) => crate::tx::offer::decode20(a).unwrap(),
                        None => panic!("#{seq} {} has no Account", t["hash"]),
                    },
                    seq_proxy: SeqProxy::of(
                        t["Sequence"].as_u64().unwrap_or(0) as u32,
                        t.get("TicketSequence").and_then(Value::as_u64).map(|v| v as u32),
                    ),
                    blob: vec![],
                })
                .collect();
            if entries.iter().any(|e| e.account == [0u8; 20]) {
                pseudo_ledgers += 1;
            }
            let salt = consensus_salt(&entries.iter().map(|e| e.hash).collect::<Vec<_>>());
            canonical_order(&mut entries, &salt);
            let rank: std::collections::HashMap<[u8; 32], usize> =
                entries.iter().enumerate().map(|(i, e)| (e.hash.0, i)).collect();
            let mut theirs: Vec<&Value> = set.clone();
            theirs.sort_by_key(|t| index_of(t));
            let ranks: Vec<usize> = theirs.iter().map(|t| rank[&id(t).0]).collect();
            let runs = 1 + ranks.windows(2).filter(|w| w[1] < w[0]).count();
            *runs_seen.entry(runs).or_default() += 1;
            let result_of = |h: &Hash256| {
                let t = by_id[&hex::encode_upper(h.0)];
                t["metaData"]["TransactionResult"].as_str().unwrap_or("?").to_string()
            };
            let (claims, rest): (Vec<Hash256>, Vec<Hash256>) =
                entries.iter().map(|e| e.hash).partition(|h| result_of(h).starts_with("tec"));
            let tec_last_order: Vec<Hash256> = rest.into_iter().chain(claims).collect();
            if theirs.iter().map(|t| id(t)).eq(tec_last_order) {
                tec_last += 1;
            }
            if runs == 1 {
                exact += 1;
                continue;
            }
            // The transactions after the first run: the ones a later pass applied.
            let first_break = ranks.windows(2).position(|w| w[1] < w[0]).unwrap() + 1;
            for t in &theirs[first_break..] {
                let result = t["metaData"]["TransactionResult"].as_str().unwrap_or("?");
                *later_results.entry(result[..3.min(result.len())].to_string()).or_default() += 1;
            }
            let later: Vec<String> = theirs[first_break..]
                .iter()
                .map(|t| format!("{} {} {}", t["TransactionType"].as_str().unwrap_or("?"),
                                 t["metaData"]["TransactionResult"].as_str().unwrap_or("?"), &t["hash"].as_str().unwrap()[..12]))
                .collect();
            let line = format!("#{seq}: {} txs, {runs} runs; after the first run: {}", set.len(), later.join(", "));
            if runs <= 3 {
                retried.push(line);
            } else {
                unexplained.push(line);
            }
        }
        eprintln!(
            "canonical order: {} ledgers, {txs_seen} transactions ({inners} Batch inners); {exact} exact, {} by retry passes, \
             {} unexplained; {pseudo_ledgers} ledgers hold pseudo-transactions; {} inner placements wrong",
            paths.len(), retried.len(), unexplained.len(), inner_misplaced.len()
        );
        eprintln!(
            "  runs per ledger {runs_seen:?}; results applied after the first run {later_results:?}; \
             {tec_last} ledgers are exactly every non-tec transaction in our order, then every tec one"
        );
        for l in retried.iter().chain(&unexplained).chain(&inner_misplaced).take(60) {
            eprintln!("  {l}");
        }
        assert!(unexplained.is_empty() && inner_misplaced.is_empty(), "{} unexplained, {} misplaced", unexplained.len(), inner_misplaced.len());
    }

    /// Independence task 3.2(a)'s check against mainnet: rippled's passes ([`apply_in_passes`]) over each
    /// ledger's canonical set, driven by each transaction's FINAL mainnet result and its account's sequence and
    /// tickets, must give mainnet's TransactionIndex order. A transaction waits (ter) while its sequence or
    /// ticket is not reached (terPRE_SEQ, terPRE_TICKET) or its account does not exist yet (terNO_ACCOUNT);
    /// a tec waits while retries are certain, unless it is one rippled always claims. Each account's sequence
    /// at the start of the ledger comes from the first record of its root in mainnet's order. A Batch outer
    /// applies its inners with it. Ledgers it can't reproduce are reported by class: those needing a
    /// transaction's result in an earlier pass, not its final one, are task 3.2(b)'s.
    ///
    ///   XRPL_META_CORPUS=<json folder> cargo test -p xrpl-ledger --lib passes_against -- --ignored --nocapture
    #[test]
    #[ignore]
    fn passes_against_mainnet() {
        use serde_json::Value;
        use std::collections::{BTreeMap, HashMap, HashSet};

        struct Tx<'a> {
            id: Hash256,
            account: [u8; 20],
            seq_proxy: SeqProxy,
            kind: &'a str,
            tickets: u32,
            result: &'a str,
            index: u64,
            meta: &'a Value,
        }

        /// Where rippled 3.4.1 returns one of the ALWAYS_CLAIMED codes from preclaim, which waits under tapRETRY
        /// like any tec (found by searching the transactors for those returns; mainnet #106793245's
        /// NFTokenCreateOffer tecEXPIRED is one).
        const PRECLAIM_TEC: [(&str, &str); 10] = [
            ("OfferCreate", "tecEXPIRED"),
            ("NFTokenMint", "tecEXPIRED"),
            ("NFTokenCreateOffer", "tecEXPIRED"),
            ("CheckCreate", "tecEXPIRED"),
            ("CheckCash", "tecEXPIRED"),
            ("VaultCreate", "tecEXPIRED"),
            ("VaultDeposit", "tecEXPIRED"),
            ("LoanSet", "tecEXPIRED"),
            ("LoanSet", "tecKILLED"),
            ("LoanPay", "tecKILLED"),
        ];

        /// The account's sequence after `t` applied (a TicketCreate also skips the tickets it made).
        fn advance(t: &Tx, seq: &mut HashMap<[u8; 20], u32>) {
            let made = if t.kind == "TicketCreate" && t.result == "tesSUCCESS" { t.tickets } else { 0 };
            match t.seq_proxy {
                SeqProxy::Sequence(s) if s != 0 => {
                    seq.insert(t.account, s + 1 + made);
                }
                SeqProxy::Ticket(_) if made > 0 => *seq.entry(t.account).or_insert(0) += made,
                _ => {}
            }
        }

        let class_of = |result: &str| if result.starts_with("tes") || result.starts_with("tec") { result[..3].to_string() } else { result.to_string() };
        let dir = std::env::var_os("XRPL_META_CORPUS").expect("XRPL_META_CORPUS: a folder of `ledger` answers");
        let mut paths: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        paths.sort();
        assert!(!paths.is_empty(), "no ledgers in {dir:?}");
        let (mut exact, mut model_errors) = (0usize, Vec::new());
        let mut classes = BTreeMap::<String, usize>::new();
        let mut examples = Vec::new();
        let mut pass_counts = BTreeMap::<usize, usize>::new();
        // Ledgers where a new account's transaction comes up before the payment that creates it:
        // the terNO_ACCOUNT wait (xrpl-node tests/new_account_passes.rs).
        let mut waited_for_account = std::collections::BTreeSet::<String>::new();
        for path in &paths {
            let doc: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            let doc = doc.get("result").unwrap_or(&doc);
            let seq_no = doc["ledger_index"].as_u64().unwrap_or_default();
            let raw = doc["ledger"]["transactions"].as_array().unwrap();
            let txs: Vec<Tx> = raw
                .iter()
                .map(|t| Tx {
                    id: Hash256(hex::decode(t["hash"].as_str().unwrap()).unwrap().try_into().unwrap()),
                    account: match t["Account"].as_str() {
                        Some("") | None => [0u8; 20],
                        Some(a) => crate::tx::offer::decode20(a).unwrap(),
                    },
                    seq_proxy: SeqProxy::of(
                        t["Sequence"].as_u64().unwrap_or(0) as u32,
                        t.get("TicketSequence").and_then(Value::as_u64).map(|v| v as u32),
                    ),
                    kind: t["TransactionType"].as_str().unwrap_or("?"),
                    tickets: t["TicketCount"].as_u64().unwrap_or(0) as u32,
                    result: t["metaData"]["TransactionResult"].as_str().unwrap_or("?"),
                    index: t["metaData"]["TransactionIndex"].as_u64().unwrap(),
                    meta: &t["metaData"],
                })
                .collect();
            let is_inner = |i: usize| {
                txs[i].meta.get("ParentBatchID").is_some()
                    || raw[i]["Flags"].as_u64().unwrap_or(0) & crate::tx::batch::TF_INNER_BATCH_TXN != 0
            };
            let pos: HashMap<[u8; 32], usize> = txs.iter().enumerate().map(|(i, t)| (t.id.0, i)).collect();
            let mut inners_of: HashMap<usize, Vec<usize>> = HashMap::new();
            for i in (0..txs.len()).filter(|&i| is_inner(i)) {
                let parent = txs[i].meta["ParentBatchID"].as_str().map(|p| hex::decode(p).unwrap());
                if let Some(&outer) = parent.and_then(|p| <[u8; 32]>::try_from(p).ok()).as_ref().and_then(|p| pos.get(p)) {
                    inners_of.entry(outer).or_default().push(i);
                }
            }
            for list in inners_of.values_mut() {
                list.sort_by_key(|&i| txs[i].index);
            }
            // Each account root's sequence at the start: its first record in mainnet's order.
            let mut by_index: Vec<usize> = (0..txs.len()).collect();
            by_index.sort_by_key(|&i| txs[i].index);
            let mut first: HashMap<String, (u32, Option<usize>)> = HashMap::new();
            for &i in &by_index {
                for node in txs[i].meta["AffectedNodes"].as_array().unwrap() {
                    let (kind, x) = node.as_object().unwrap().iter().next().unwrap();
                    if x["LedgerEntryType"] != "AccountRoot" {
                        continue;
                    }
                    let key = x["LedgerIndex"].as_str().unwrap().to_uppercase();
                    if first.contains_key(&key) {
                        continue;
                    }
                    let created = kind == "CreatedNode";
                    let found = if created {
                        x["NewFields"]["Sequence"].as_u64()
                    } else {
                        x["PreviousFields"]["Sequence"].as_u64().or_else(|| x["FinalFields"]["Sequence"].as_u64())
                    };
                    if let Some(s) = found {
                        first.insert(key, (s as u32, created.then_some(i)));
                    }
                }
            }
            let mut seq: HashMap<[u8; 20], u32> = HashMap::new();
            let mut missing: HashSet<[u8; 20]> = HashSet::new();
            let mut created_by: HashMap<usize, Vec<([u8; 20], u32)>> = HashMap::new();
            for t in txs.iter().filter(|t| t.account != [0u8; 20]) {
                let root = hex::encode_upper(crate::ledger::keylet::account_root_key(&t.account).0);
                match first.get(&root) {
                    Some(&(s, None)) => {
                        seq.insert(t.account, s);
                    }
                    Some(&(s, Some(creator))) => {
                        missing.insert(t.account);
                        created_by.entry(creator).or_default().push((t.account, s));
                    }
                    None => {}
                }
            }
            // The canonical set, salted with its own root.
            let set: Vec<usize> = (0..txs.len()).filter(|&i| !is_inner(i)).collect();
            let mut entries: Vec<TransactionEntry> = set
                .iter()
                .map(|&i| TransactionEntry { hash: txs[i].id, account: txs[i].account, seq_proxy: txs[i].seq_proxy, blob: vec![] })
                .collect();
            let salt = consensus_salt(&entries.iter().map(|e| e.hash).collect::<Vec<_>>());
            canonical_order(&mut entries, &salt);
            let ordered: Vec<usize> = entries.iter().map(|e| pos[&e.hash.0]).collect();
            let passes = apply_in_passes(ordered, |&i: &usize, certain| {
                let t = &txs[i];
                if t.account != [0u8; 20] {
                    if missing.contains(&t.account) {
                        waited_for_account.insert(format!(
                            "#{seq_no}: {} {} waited for the transaction creating its account",
                            t.kind,
                            &hex::encode_upper(t.id.0)[..12]
                        ));
                        return ApplyOutcome::Retry; // terNO_ACCOUNT
                    }
                    if let Some(&a) = seq.get(&t.account) {
                        match t.seq_proxy {
                            SeqProxy::Sequence(s) if s > a => return ApplyOutcome::Retry, // terPRE_SEQ
                            SeqProxy::Sequence(s) if s < a => return ApplyOutcome::Fail,  // tefPAST_SEQ
                            SeqProxy::Ticket(k) if a <= k => return ApplyOutcome::Retry,  // terPRE_TICKET
                            _ => {}
                        }
                    }
                    let from_apply = ALWAYS_CLAIMED.contains(&t.result) && !PRECLAIM_TEC.contains(&(t.kind, t.result));
                    let claims = t.result.starts_with("tes") || from_apply || !certain;
                    if !claims {
                        return ApplyOutcome::Retry;
                    }
                }
                for j in std::iter::once(i).chain(inners_of.get(&i).into_iter().flatten().copied()) {
                    advance(&txs[j], &mut seq);
                    for (x, s) in created_by.get(&j).into_iter().flatten() {
                        missing.remove(x);
                        seq.insert(*x, *s);
                    }
                }
                ApplyOutcome::Success
            });
            for (pass, _) in &passes.applied {
                *pass_counts.entry(*pass).or_default() += 1;
            }
            let model: Vec<usize> = passes.applied.iter().map(|(_, i)| *i).collect();
            let mut theirs = set.clone();
            theirs.sort_by_key(|&i| txs[i].index);
            if !passes.failed.is_empty() {
                model_errors.push(format!("#{seq_no}: the model failed {} (tefPAST_SEQ)", passes.failed.len()));
            }
            if model == theirs {
                exact += 1;
                continue;
            }
            let k = (0..).find(|&k| model.get(k) != theirs.get(k)).unwrap();
            let (class, who) = match (model.get(k), theirs.get(k)) {
                (Some(&p), Some(_)) if theirs.iter().position(|&x| x == p).is_some_and(|q| q > k) => {
                    (format!("mainnet deferred a {} the model applied", class_of(txs[p].result)), p)
                }
                (_, Some(&m)) => (format!("the model deferred or left a {} mainnet applied", class_of(txs[m].result)), m),
                (Some(&p), None) => ("the model applied more than mainnet holds".to_string(), p),
                (None, None) => unreachable!(),
            };
            *classes.entry(class.clone()).or_default() += 1;
            let pass_of = passes.applied.iter().find(|(_, i)| *i == who).map(|(p, _)| p.to_string()).unwrap_or("-".into());
            examples.push(format!(
                "#{seq_no} at {k}/{}: {class}: {} {} {} (model pass {pass_of})",
                theirs.len(), txs[who].kind, txs[who].result, &hex::encode_upper(txs[who].id.0)[..12]
            ));
        }
        eprintln!(
            "passes: {} ledgers, {exact} exactly as mainnet; model passes applied {pass_counts:?}; leftovers by class {classes:?}; {} model errors",
            paths.len(), model_errors.len()
        );
        for line in model_errors.iter().chain(&examples).take(80) {
            eprintln!("  {line}");
        }
        eprintln!("terNO_ACCOUNT waits (a new account's transaction ahead of its creator): {}", waited_for_account.len());
        for line in waited_for_account.iter().take(40) {
            eprintln!("  {line}");
        }
        assert!(model_errors.is_empty(), "the model's sequence state went wrong");
    }
}
