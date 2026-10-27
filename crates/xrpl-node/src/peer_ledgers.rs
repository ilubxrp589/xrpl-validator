//! Independence Step 4: the ledgers the engine applies, taken from peers (`XRPL_LEDGER_SOURCE=peers`,
//! [`crate::ledger_source`]).
//!
//! - **Which ledger, by which hash.** Our own tally of the trusted validators' validations ([`crate::own_validated`])
//!   names a hash for each ledger that reaches quorum here. A ledger that falls short here (27 of quorum 28 seen,
//!   mainnet #107783807-814 on 10-20) takes its hash from the next ledger that did reach it: that ledger's header
//!   names its parent, and so on back ([`Chain`]). Every header is proven to hash to the hash it was asked by, so a
//!   hash from the chain is as good as the quorum at its top.
//! - **Its inputs.** [`crate::peer_fetch::fetch_ledger`] brings the header and the transaction tree, proven to that
//!   hash. [`crate::peer_source::assemble`] makes them into exactly what ws-sync took from .39 (Stage A held the two
//!   equal on every ledger since cycle 186), and [`PeerLedger`] adds the object keys the records name.
//! - **When.** [`trigger`] yields the newest quorum ledger each time it rises. It drives ws-sync's loop as .39's
//!   `ledgerClosed` stream did.

use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::sync::atomic::Ordering;
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;
use xrpl_ledger::ledger::header::LedgerHeader;

/// Hashes kept from walks: a warm restart's whole gap and then some.
pub const CHAIN_KEEP: usize = crate::resume::MAX_WARM_GAP as usize + 256;

/// One ledger's inputs, from peers: what ws-sync took from .39 for it.
#[derive(Debug, Clone)]
pub struct PeerLedger {
    pub header: crate::ws_sync::LedgerHeader,
    /// The state root, upper-case hex as rippled's JSON gives it.
    pub account_hash: String,
    /// rippled's JSON for each transaction, with `hash` and `metaData`, in TransactionIndex order.
    pub txs: Vec<Value>,
    /// The transaction and record bytes, in the same order.
    pub pairs: Vec<(Vec<u8>, Vec<u8>)>,
    /// The objects the records create or modify, and those they delete (ws-sync's sets, upper-case hex).
    pub modified: HashSet<String>,
    pub deleted: HashSet<String>,
    pub tx_count: u32,
    /// AccountRoots created less those deleted.
    pub wallet_delta: i64,
}

impl From<crate::peer_source::Assembled> for PeerLedger {
    fn from(a: crate::peer_source::Assembled) -> Self {
        let (mut modified, mut deleted) = (HashSet::new(), HashSet::new());
        let mut wallet_delta = 0;
        for tx in &a.txs {
            crate::ws_sync::extract_affected_nodes(tx, &mut modified, &mut deleted);
            wallet_delta += crate::ws_sync::count_wallet_delta(tx);
        }
        PeerLedger {
            header: a.header,
            account_hash: hex::encode_upper(a.account_hash),
            tx_count: a.txs.len() as u32,
            txs: a.txs,
            pairs: a.pairs,
            modified,
            deleted,
            wallet_delta,
        }
    }
}

/// Where proven headers come from.
pub trait Headers {
    fn header(&self, hash: [u8; 32]) -> impl Future<Output = Result<LedgerHeader, String>> + Send;
}

/// From peers.
pub struct PeerHeaders;

impl Headers for PeerHeaders {
    fn header(&self, hash: [u8; 32]) -> impl Future<Output = Result<LedgerHeader, String>> + Send {
        crate::peer_fetch::fetch_header(hash)
    }
}

/// Validated hashes learned by walking parent hashes back from a quorum ledger.
#[derive(Debug, Default)]
pub struct Chain {
    known: BTreeMap<u32, [u8; 32]>,
}

impl Chain {
    /// The validated hash of ledger `seq`: the quorum's when it reached one here, else the parent chain's from the
    /// nearest newer ledger known (a quorum ledger, or one an earlier walk passed). `first_quorum(from)` names the
    /// lowest ledger at or after `from` that reached quorum, and its hash. Err when no newer ledger has.
    pub async fn hash_for<H: Headers>(
        &mut self,
        headers: &H,
        seq: u32,
        first_quorum: impl Fn(u32) -> Option<(u32, [u8; 32])>,
    ) -> Result<[u8; 32], String> {
        let quorum = first_quorum(seq);
        if let Some((s, h)) = quorum {
            if s == seq {
                return Ok(h);
            }
        }
        if let Some(h) = self.known.get(&seq) {
            return Ok(*h);
        }
        let cached = self.known.range(seq..).next().map(|(s, h)| (*s, *h));
        let (mut s, mut h) = match (quorum, cached) {
            (Some(q), Some(c)) => {
                if c.0 < q.0 {
                    c
                } else {
                    q
                }
            }
            (Some(q), None) => q,
            (None, Some(c)) => c,
            (None, None) => return Err(format!("no ledger at or after #{seq} has reached quorum here yet")),
        };
        while s > seq {
            let header = headers.header(h).await.map_err(|e| format!("walking back to #{seq}, the header of #{s}: {e}"))?;
            if header.hash().0 != h || header.sequence != s {
                return Err(format!("walking back to #{seq}: the header asked for #{s} is #{}, hash {}", header.sequence, &hex::encode_upper(header.hash().0)[..16]));
            }
            s -= 1;
            h = header.parent_hash.0;
            self.known.insert(s, h);
            crate::ledger_source::stats().chain_walks.fetch_add(1, Ordering::Relaxed);
        }
        while self.known.len() > CHAIN_KEEP {
            self.known.pop_first();
        }
        Ok(h)
    }
}

fn chain() -> &'static tokio::sync::Mutex<Chain> {
    static C: OnceLock<tokio::sync::Mutex<Chain>> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// The validated hash of ledger `seq`, from our own tally or its parent chain ([`Chain::hash_for`]).
pub async fn hash_for(seq: u32) -> Result<[u8; 32], String> {
    chain().lock().await.hash_for(&PeerHeaders, seq, crate::own_validated::first_quorum_from).await
}

/// Ledger `seq`'s inputs from peers. Err when its hash is not known yet, or no peer delivered it in time: the
/// caller holds the ledger and asks again.
pub async fn ledger(seq: u32) -> Result<PeerLedger, String> {
    let hash = hash_for(seq).await?;
    let (header, pairs) = crate::peer_fetch::fetch_ledger(seq, hash).await?;
    let a = tokio::task::spawn_blocking(move || crate::peer_source::assemble(&header, pairs))
        .await
        .map_err(|e| format!("assembling #{seq}: {e}"))?
        .map_err(|e| format!("#{seq}: {e}"))?;
    Ok(PeerLedger::from(a))
}

/// The newest ledger that reached quorum here, each time it rises. A one-second look also runs, in case a wake was
/// missed.
pub fn trigger() -> impl futures_util::Stream<Item = u32> {
    futures_util::stream::unfold(0u32, |last| async move {
        loop {
            if let Some((seq, _)) = crate::own_validated::newest_quorum() {
                if seq > last {
                    return Some((seq, seq));
                }
            }
            let _ = tokio::time::timeout(Duration::from_secs(1), crate::own_validated::quorum_moved().notified()).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;
    use xrpl_core::types::Hash256;

    /// Headers #first..=#last, each naming the one before as its parent.
    fn chain_of(first: u32, last: u32) -> Vec<LedgerHeader> {
        let mut out: Vec<LedgerHeader> = Vec::new();
        for seq in first..=last {
            let parent = out.last().map_or(Hash256([0xAB; 32]), |h| h.hash());
            out.push(LedgerHeader {
                sequence: seq,
                total_coins: 99_985_000_000_000_000,
                parent_hash: parent,
                transaction_hash: Hash256([seq as u8; 32]),
                account_hash: Hash256([2; 32]),
                parent_close_time: 800_000_000 + seq,
                close_time: 800_000_003 + seq,
                close_time_resolution: 10,
                close_flags: 0,
            });
        }
        out
    }

    struct Fake {
        by_hash: HashMap<[u8; 32], LedgerHeader>,
        calls: AtomicUsize,
    }

    impl Fake {
        fn new(headers: &[LedgerHeader]) -> Self {
            Fake { by_hash: headers.iter().map(|h| (h.hash().0, h.clone())).collect(), calls: AtomicUsize::new(0) }
        }
    }

    impl Headers for Fake {
        fn header(&self, hash: [u8; 32]) -> impl Future<Output = Result<LedgerHeader, String>> + Send {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let r = self.by_hash.get(&hash).cloned().ok_or_else(|| "no peer has it".to_string());
            async move { r }
        }
    }

    #[tokio::test]
    async fn a_seq_without_quorum_takes_its_hash_from_the_next_quorum_ledgers_parent() {
        let hs = chain_of(100, 105);
        let hash = |seq: u32| hs[(seq - 100) as usize].hash().0;
        let fake = Fake::new(&hs);
        let top = hash(105);
        let quorum = move |from: u32| (from <= 105).then_some((105, top));
        let mut c = Chain::default();
        assert_eq!(c.hash_for(&fake, 105, quorum).await, Ok(hash(105)), "the quorum's own");
        assert_eq!(fake.calls.load(Ordering::Relaxed), 0);
        assert_eq!(c.hash_for(&fake, 102, quorum).await, Ok(hash(102)));
        assert_eq!(fake.calls.load(Ordering::Relaxed), 3, "headers #105, #104, #103");
        assert_eq!(c.hash_for(&fake, 103, quorum).await, Ok(hash(103)));
        assert_eq!(fake.calls.load(Ordering::Relaxed), 3, "#103 was learned on the way");
        assert_eq!(c.hash_for(&fake, 101, quorum).await, Ok(hash(101)));
        assert_eq!(fake.calls.load(Ordering::Relaxed), 4, "one more step, from #102");
    }

    #[tokio::test]
    async fn a_header_that_breaks_the_chain_is_an_error() {
        let mut hs = chain_of(200, 203);
        let top = hs[3].hash().0;
        // A peer's "header for #203" that is really another ledger's: refused, though it parses.
        let mut fake = Fake::new(&hs);
        hs[3].sequence = 999;
        fake.by_hash.insert(top, hs[3].clone());
        let e = Chain::default().hash_for(&fake, 201, move |_| Some((203, top))).await.unwrap_err();
        assert!(e.contains("the header asked for #203 is #999"), "{e}");
    }

    #[tokio::test]
    async fn without_a_newer_quorum_ledger_there_is_no_hash_yet() {
        let fake = Fake::new(&[]);
        let e = Chain::default().hash_for(&fake, 300, |_| None).await.unwrap_err();
        assert!(e.contains("no ledger at or after #300"), "{e}");
    }

    #[test]
    fn peer_ledger_key_sets_match_ws_syncs() {
        use crate::peer_source::fixtures::{header_for, payment, theirs};
        let record = serde_json::json!({
            "TransactionIndex": 0, "TransactionResult": 0,
            "AffectedNodes": [
                {"CreatedNode": {"LedgerEntryType": "AccountRoot", "LedgerIndex": "AA".repeat(32),
                    "NewFields": {"Account": "rPT1Sjq2YGrBMTttX4GZHjKu9dyfzbpAYe", "Balance": "1000000", "Sequence": 1}}},
                {"ModifiedNode": {"LedgerEntryType": "AccountRoot", "LedgerIndex": "BB".repeat(32),
                    "FinalFields": {"Account": "rHb9CJAWyB4rj91VRWn96DkukG4bwdtyTh", "Balance": "5000000", "Flags": 0, "OwnerCount": 0, "Sequence": 6},
                    "PreviousFields": {"Balance": "6000012", "Sequence": 5}}},
                {"DeletedNode": {"LedgerEntryType": "Ticket", "LedgerIndex": "CC".repeat(32),
                    "FinalFields": {"Account": "rHb9CJAWyB4rj91VRWn96DkukG4bwdtyTh", "Flags": 0, "TicketSequence": 4}}}
            ]
        });
        let pairs = vec![(crate::meta_check::encode(&payment(5)).unwrap(), crate::meta_check::encode(&record).unwrap())];
        let a = crate::peer_source::assemble(&header_for(&pairs), pairs).unwrap();
        let (_, their_root, their_txs) = theirs(&a);
        let (mut modified, mut deleted, mut wallets) = (HashSet::new(), HashSet::new(), 0);
        for tx in &their_txs {
            crate::ws_sync::extract_affected_nodes(tx, &mut modified, &mut deleted);
            wallets += crate::ws_sync::count_wallet_delta(tx);
        }
        let p = PeerLedger::from(a);
        assert_eq!(p.modified.len(), 2, "{:?}", p.modified);
        assert_eq!(p.deleted.len(), 1, "{:?}", p.deleted);
        assert_eq!(p.wallet_delta, 1);
        assert_eq!((p.modified, p.deleted, p.wallet_delta), (modified, deleted, wallets));
        assert_eq!(p.account_hash, their_root.to_uppercase());
        assert_eq!(p.tx_count as usize, their_txs.len());
    }
}
