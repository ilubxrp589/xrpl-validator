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
//! and a transaction that asks to be retried applies in a later pass, after every first-pass one (task 3.2).
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
            if runs == 1 {
                exact += 1;
                continue;
            }
            // The transactions after the first run: the ones a later pass applied.
            let first_break = ranks.windows(2).position(|w| w[1] < w[0]).unwrap() + 1;
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
        for l in retried.iter().chain(&unexplained).chain(&inner_misplaced).take(60) {
            eprintln!("  {l}");
        }
        assert!(unexplained.is_empty() && inner_misplaced.is_empty(), "{} unexplained, {} misplaced", unexplained.len(), inner_misplaced.len());
    }
}
