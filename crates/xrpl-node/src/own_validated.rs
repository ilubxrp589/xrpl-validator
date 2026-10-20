//! Independence task 4.5: "validated" without .39.
//!
//! Today the validator learns which ledger is validated from the stock xrpld on .39: ws-sync takes a ledger only once
//! that node reports it validated. rippled decides for itself from the trusted validators' validations
//! (`LedgerMaster::checkAccept`, 3.4.1):
//! - the full validations (`vfFullValidation`) for the ledger's hash and sequence;
//! - from validators on the UNL, a signing key mapped to its master by the list's manifests
//!   (`Validations::getTrustedForLedger`);
//! - less those whose master key is on the ledger's negative UNL (`ValidatorList::negativeUNLFilter`);
//! - validated when they reach the quorum (`ValidatorList::quorum`, [`crate::consensus::quorum`]).
//!
//! This module counts the same thing from the validations the peer loop has already signature-checked, and holds
//! its verdict against .39's for every ledger ws-sync takes:
//! - **agree**: we reached quorum on the same hash, and how long before (or after) .39 said so;
//! - **differ**: we reached quorum on another hash for that sequence (a healthy network never does);
//! - **short**: we never reached quorum for that hash, judged once the ledger is [`GIVE_UP`] ledgers old.
//!
//! The validator signs on it: on the quorum's hash (cycle 183, `XRPL_SIGN_ON_QUORUM`), or, when the quorum falls
//! short here, on the hash .39 called validated and our own engine computed too (cycle 184, `XRPL_SIGN_ON_WITNESS`,
//! [`Tally::next_to_sign`]). With independence 5a (cycle 185, `XRPL_SIGN_OWN=1`) only on our own hash of the
//! ledger, once the quorum or .39 names it too ([`Tally::next_own_to_sign`]). `XRPL_OWN_VALIDATED=1` switches it
//! on (off by default); differences go to
//! `XRPL_OWN_VALIDATED_LOG` (default /mnt/xrpl-data/own_validated.jsonl, capped at 20 MB).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

/// A ledger .39 called validated, and we have not reached quorum on, is judged short once a ledger this many
/// sequences newer is taken.
pub const GIVE_UP: u32 = 8;
/// Sequences of votes kept.
const KEEP: u32 = 256;

/// `XRPL_OWN_VALIDATED=1`, read once.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("XRPL_OWN_VALIDATED").map(|v| v == "1").unwrap_or(false))
}

#[derive(Default)]
struct SeqVotes {
    /// Each trusted validator's latest full validation for this sequence: the hash, and when it arrived.
    votes: HashMap<Vec<u8>, ([u8; 32], u64)>,
    /// When each hash first reached quorum here.
    reached: HashMap<[u8; 32], u64>,
    /// Each trusted validator's `sfConsensusHash` for this sequence: the root of the transaction set it accepted.
    sets: HashMap<Vec<u8>, [u8; 32]>,
}

/// The tally behind the verdicts.
#[derive(Default)]
pub struct Tally {
    /// Signing key (ephemeral, or a master that signs itself) → master key.
    signer: HashMap<Vec<u8>, Vec<u8>>,
    masters: Vec<Vec<u8>>,
    negative: HashSet<Vec<u8>>,
    seqs: BTreeMap<u32, SeqVotes>,
    /// What .39 called validated and is not judged yet: sequence → (hash, when).
    pending: BTreeMap<u32, ([u8; 32], u64)>,
    /// How many ms before .39 we reached quorum, for the last judged ledgers (negative: after).
    leads: VecDeque<i64>,
    /// The first sequence a trusted validation named: ledgers before it closed before the tally listened.
    first_seq: Option<u32>,
    /// Ledgers .39 called validated whose hash our own engine computed too (independence 5.5, [`Self::witness`]).
    witnessed: BTreeMap<u32, [u8; 32]>,
    /// Our own hash of each ledger ws-sync landed, made from every part we compute; None when a part was borrowed
    /// (independence 5a, [`Self::own_ledger`]).
    own: BTreeMap<u32, Option<[u8; 32]>>,
}

/// Why the validator may sign a ledger ([`Tally::next_to_sign`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// The trusted validations reaching us reached quorum on its hash.
    Quorum,
    /// They fell short, but .39 called it validated and our own hash of it is that hash.
    Witness,
    /// Our own hash of it, which the trusted validations reaching us reached quorum on too (independence 5a).
    Own,
}

/// Independence 5a's fallback: when the validator has had nothing of its own to sign for a while, it signs the
/// quorum's hash for a ledger it cannot make, and for one ws-sync has not landed while ledgers this many sequences
/// past the newest it did land are heard (a stall).
pub const STALL: u32 = 2;

/// One judged ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Agree { lead_ms: i64 },
    Differ { ours: [u8; 32] },
    Short { count: usize, quorum: usize },
}

impl Tally {
    /// The verified validator list: each master key and its current signing key (hex).
    pub fn set_unl(&mut self, entries: &[(String, Option<String>)]) {
        self.signer.clear();
        self.masters.clear();
        for (master, signing) in entries {
            let Ok(m) = hex::decode(master) else { continue };
            self.signer.insert(m.clone(), m.clone());
            if let Some(sk) = signing.as_deref().and_then(|s| hex::decode(s).ok()) {
                self.signer.insert(sk, m.clone());
            }
            self.masters.push(m);
        }
    }

    pub fn set_negative_unl(&mut self, masters: Vec<Vec<u8>>) {
        self.negative = masters.into_iter().collect();
    }

    pub fn unl_size(&self) -> usize {
        self.masters.len()
    }

    pub fn negative_size(&self) -> usize {
        self.masters.iter().filter(|m| self.negative.contains(*m)).count()
    }

    pub fn quorum(&self) -> usize {
        let effective = self.masters.len() - self.negative_size();
        crate::consensus::quorum::quorum(self.masters.len(), effective)
    }

    /// Trusted, full validations for `hash` at `seq`, less the negative UNL's.
    pub fn count(&self, seq: u32, hash: &[u8; 32]) -> usize {
        self.seqs.get(&seq).map_or(0, |s| {
            s.votes.iter().filter(|(m, (h, _))| h == hash && !self.negative.contains(*m)).count()
        })
    }

    /// One signature-checked validation. Returns what it judged (a pending ledger it settled), if anything.
    pub fn vote(&mut self, signing_key: &[u8], seq: u32, hash: [u8; 32], full: bool, now_ms: u64) -> Vec<(u32, Verdict)> {
        let st = stats();
        st.validations.fetch_add(1, Ordering::Relaxed);
        if !full {
            st.partial.fetch_add(1, Ordering::Relaxed);
            return Vec::new();
        }
        let Some(master) = self.signer.get(signing_key).cloned() else {
            st.untrusted.fetch_add(1, Ordering::Relaxed);
            return Vec::new();
        };
        if let Some(newest) = self.seqs.keys().next_back().copied() {
            if seq + KEEP < newest {
                return Vec::new(); // far too old to matter
            }
        }
        self.first_seq.get_or_insert(seq);
        let q = self.quorum();
        let negative = self.negative.contains(&master);
        let s = self.seqs.entry(seq).or_default();
        s.votes.insert(master, (hash, now_ms));
        if !negative && !s.reached.contains_key(&hash) {
            let n = s.votes.iter().filter(|(m, (h, _))| *h == hash && !self.negative.contains(*m)).count();
            if q > 0 && n >= q {
                s.reached.insert(hash, now_ms);
            }
        }
        while self.seqs.len() as u32 > KEEP {
            self.seqs.pop_first();
        }
        self.judge_pending(None)
    }

    /// The transaction set a full, trusted validation for `seq` says its validator accepted (`sfConsensusHash`).
    /// Kept beside the vote [`Self::vote`] just recorded, so only for a sequence the tally holds.
    pub fn note_set(&mut self, signing_key: &[u8], seq: u32, set: [u8; 32]) {
        let Some(master) = self.signer.get(signing_key).cloned() else { return };
        if let Some(s) = self.seqs.get_mut(&seq) {
            s.sets.insert(master, set);
        }
    }

    /// The transaction set the trusted validators accepted for `seq` (their `sfConsensusHash`): the consensus set
    /// rippled salts its canonical order with. More than half the trusted list (less the negative UNL) must name it,
    /// not a full quorum: two sets can never both have that many, and when a few validations reach us late the ones
    /// that do still name the one set (mainnet #107700728: only 27 of quorum 28 reached us on #107700727-734, so the
    /// order fell back to the ledger's own root, and #107700728's set held a transaction the ledger never filed).
    /// None until then.
    pub fn agreed_set(&self, seq: u32) -> Option<[u8; 32]> {
        let effective = self.masters.len() - self.negative_size();
        let s = self.seqs.get(&seq)?;
        let mut counts: HashMap<[u8; 32], usize> = HashMap::new();
        for (m, set) in &s.sets {
            if !self.negative.contains(m) {
                *counts.entry(*set).or_default() += 1;
            }
        }
        counts.into_iter().find(|(_, n)| effective > 0 && *n > effective / 2).map(|(set, _)| set)
    }

    /// .39 called ledger `seq` validated with `hash`. Judges it now if it can, and every pending ledger that is
    /// now too old to wait for.
    pub fn network_validated(&mut self, seq: u32, hash: [u8; 32], now_ms: u64) -> Vec<(u32, Verdict)> {
        // Nothing to judge against until the list has loaded and the first trusted validation arrived, and a
        // ledger older than that first one was voted on before we listened.
        if self.masters.is_empty() || self.first_seq.is_none_or(|f| seq <= f) {
            stats().unjudged.fetch_add(1, Ordering::Relaxed);
            return self.judge_pending(Some(seq));
        }
        self.pending.insert(seq, (hash, now_ms));
        self.judge_pending(Some(seq))
    }

    fn judge_pending(&mut self, newest: Option<u32>) -> Vec<(u32, Verdict)> {
        let newest = newest.or_else(|| self.pending.keys().next_back().copied()).unwrap_or(0);
        let mut out = Vec::new();
        let seqs: Vec<u32> = self.pending.keys().copied().collect();
        for seq in seqs {
            let (hash, net_ms) = self.pending[&seq];
            let s = self.seqs.get(&seq);
            let verdict = if let Some(at) = s.and_then(|s| s.reached.get(&hash)) {
                Some(Verdict::Agree { lead_ms: net_ms as i64 - *at as i64 })
            } else if let Some((other, _)) = s.and_then(|s| s.reached.iter().find(|(h, _)| **h != hash)) {
                Some(Verdict::Differ { ours: *other })
            } else if newest >= seq + GIVE_UP {
                Some(Verdict::Short { count: self.count(seq, &hash), quorum: self.quorum() })
            } else {
                None
            };
            if let Some(v) = verdict {
                self.pending.remove(&seq);
                if let Verdict::Agree { lead_ms } = v {
                    self.leads.push_back(lead_ms);
                    while self.leads.len() > 256 {
                        self.leads.pop_front();
                    }
                }
                out.push((seq, v));
            }
        }
        out
    }

    /// The oldest ledger after `after` that reached quorum here on one hash, with that hash. Only the last
    /// [`GIVE_UP`] sequences heard count: a ledger older than that is not signed late.
    pub fn next_quorum_after(&self, after: u32) -> Option<(u32, [u8; 32])> {
        let newest = *self.seqs.keys().next_back()?;
        let from = after.max(newest.saturating_sub(GIVE_UP)) + 1;
        self.seqs.range(from..).find_map(|(seq, s)| (s.reached.len() == 1).then(|| s.reached.keys().next().map(|h| (*seq, *h))).flatten())
    }

    /// .39 called ledger `seq` validated with `hash`, and our own engine computed that same hash.
    pub fn witness(&mut self, seq: u32, hash: [u8; 32]) {
        self.witnessed.insert(seq, hash);
        while self.witnessed.len() as u32 > KEEP {
            self.witnessed.pop_first();
        }
    }

    /// The oldest recent ledger after `after` the validator may sign, its hash and why: the quorum's, or with
    /// `witness` the hash .39 and our own engine both have when the trusted validations reaching us fall short of
    /// quorum (mainnet #107783807-814, 20 Oct: 26 or 27 of quorum 28 reached us, so cycle 183 signed none of the
    /// eight). Never a hash another quorum here contradicts. Only the last [`GIVE_UP`] sequences heard count.
    pub fn next_to_sign(&self, after: u32, witness: bool) -> Option<(u32, [u8; 32], Basis)> {
        let newest = self.seqs.keys().next_back().into_iter().chain(self.witnessed.keys().next_back()).max().copied()?;
        let from = after.max(newest.saturating_sub(GIVE_UP)) + 1;
        let quorum = self.next_quorum_after(after).filter(|(seq, _)| *seq >= from);
        let witnessed = witness
            .then(|| {
                self.witnessed.range(from..).find(|(seq, hash)| {
                    self.seqs.get(*seq).is_none_or(|s| s.reached.keys().all(|h| h == *hash))
                })
            })
            .flatten();
        match (quorum, witnessed) {
            (Some((q, _)), Some((w, h))) if *w < q => Some((*w, *h, Basis::Witness)),
            (Some((q, h)), _) => Some((q, h, Basis::Quorum)),
            (None, Some((w, h))) => Some((*w, *h, Basis::Witness)),
            (None, None) => None,
        }
    }

    /// ws-sync landed ledger `seq`: our own hash of it, or None when a part of it was borrowed.
    pub fn own_ledger(&mut self, seq: u32, ours: Option<[u8; 32]>) {
        self.own.insert(seq, ours);
        while self.own.len() as u32 > KEEP {
            self.own.pop_first();
        }
    }

    /// Independence 5a: the oldest recent ledger after `after` whose hash the validator computed itself and may
    /// sign: ours when the trusted validations reaching us reached quorum on it ([`Basis::Own`]), or, short of
    /// quorum and with `witness`, when .39 validated that hash too ([`Basis::Witness`]). It waits for a ledger
    /// ws-sync has not landed yet and passes over one it cannot sign (a borrowed part, or ours differs). With
    /// `breaker` (the validator has had nothing of its own to sign for a while) those it passes over, and those
    /// ws-sync stalls on ([`STALL`]), get the quorum's hash ([`Basis::Quorum`]); ours still goes first. Only the
    /// last [`GIVE_UP`] sequences heard count.
    pub fn next_own_to_sign(&self, after: u32, witness: bool, breaker: bool) -> Option<(u32, [u8; 32], Basis)> {
        let newest = [self.seqs.keys().next_back(), self.witnessed.keys().next_back(), self.own.keys().next_back()]
            .into_iter()
            .flatten()
            .max()
            .copied()?;
        let from = after.max(newest.saturating_sub(GIVE_UP)) + 1;
        // ws-sync lands a ledger a few tens of ms after the quorum on it: hearing of ledgers well past the newest it
        // landed means it has stalled.
        let stalled = self.own.keys().next_back().is_none_or(|l| newest > l + STALL);
        for seq in from..=newest {
            let s = self.seqs.get(&seq);
            let quorum = s.and_then(|s| (s.reached.len() == 1).then(|| s.reached.keys().next().copied()).flatten());
            let contested = s.is_some_and(|s| s.reached.len() > 1);
            match self.own.get(&seq) {
                Some(Some(ours)) => {
                    if quorum == Some(*ours) {
                        return Some((seq, *ours, Basis::Own));
                    }
                    if quorum.is_some() || contested {
                        if let (true, Some(q)) = (breaker, quorum) {
                            return Some((seq, q, Basis::Quorum));
                        }
                        continue; // ours differs from the quorum's
                    }
                    if self.witnessed.get(&seq) != Some(ours) {
                        continue; // ours is not .39's either
                    }
                    if witness {
                        return Some((seq, *ours, Basis::Witness));
                    }
                    return None; // .39 agrees with us: wait for the quorum
                }
                Some(None) => {
                    if let (true, Some(q)) = (breaker, quorum) {
                        return Some((seq, q, Basis::Quorum));
                    }
                }
                None => {
                    if !(breaker && stalled) {
                        return None; // not landed yet: wait for it
                    }
                    if let Some(q) = quorum {
                        return Some((seq, q, Basis::Quorum));
                    }
                }
            }
        }
        None
    }

    /// The median of the recent leads (ms we were ahead of .39; negative: behind).
    pub fn median_lead(&self) -> Option<i64> {
        let mut v: Vec<i64> = self.leads.iter().copied().collect();
        v.sort_unstable();
        v.get(v.len() / 2).copied()
    }
}

// ---- the live tally, counters, receipts, /api/engine and /metrics ---------------------------------------------

fn tally() -> &'static Mutex<Tally> {
    static T: OnceLock<Mutex<Tally>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(Tally::default()))
}

#[derive(Default)]
pub struct OwnStats {
    pub validations: AtomicU64,
    pub untrusted: AtomicU64,
    pub partial: AtomicU64,
    /// Ledgers .39 validated before the list loaded or before the first trusted validation came in: not judged.
    pub unjudged: AtomicU64,
    pub judged: AtomicU64,
    pub agree: AtomicU64,
    pub differ: AtomicU64,
    pub short: AtomicU64,
    pub last_seq: AtomicU64,
}

pub fn stats() -> &'static OwnStats {
    static S: OnceLock<OwnStats> = OnceLock::new();
    S.get_or_init(OwnStats::default)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// The verified validator list (live_viewer, when it loads).
pub fn set_unl(entries: &[crate::unl_fetch::UnlEntry]) {
    if !enabled() {
        return;
    }
    let e: Vec<(String, Option<String>)> = entries.iter().map(|e| (e.public_key.clone(), e.signing_key.clone())).collect();
    let mut t = tally().lock().unwrap_or_else(|p| p.into_inner());
    t.set_unl(&e);
    eprintln!("[own-validated] ON — {} trusted validators, quorum {}", t.unl_size(), t.quorum());
}

/// The negative UNL of the newest ledger written, from its NegativeUNL object's bytes (None: there is none).
pub fn set_negative_unl(sle: Option<&[u8]>) {
    if !enabled() {
        return;
    }
    let masters = sle
        .and_then(|b| xrpl_core::codec::decode::decode_transaction_binary(b).ok())
        .map(|v| crate::consensus::quorum::disabled_validators(&v))
        .unwrap_or_default();
    tally().lock().unwrap_or_else(|p| p.into_inner()).set_negative_unl(masters);
}

/// A signature-checked validation blob, from the peer loop.
pub fn on_validation(blob: &[u8]) {
    if !enabled() {
        return;
    }
    let now = now_ms();
    let Some(v) = crate::consensus_record::parse_validation(blob, now) else { return };
    let judged = {
        let mut t = tally().lock().unwrap_or_else(|p| p.into_inner());
        let judged = t.vote(&v.signing_key, v.ledger_seq, v.ledger_hash, v.full, now);
        if let (true, Some(set)) = (v.full, v.consensus_hash) {
            t.note_set(&v.signing_key, v.ledger_seq, set);
        }
        judged
    };
    settle(judged);
}

/// The hash our own tally reached quorum on for ledger `seq`, if one did (None also when off). Independence 5.5
/// holds our own ledger hash against it.
pub fn quorum_hash(seq: u32) -> Option<[u8; 32]> {
    if !enabled() {
        return None;
    }
    let t = tally().lock().unwrap_or_else(|p| p.into_inner());
    let s = t.seqs.get(&seq)?;
    // Two hashes at quorum for one sequence (never on a healthy network): no single answer.
    (s.reached.len() == 1).then(|| s.reached.keys().next().copied()).flatten()
}

/// The oldest recent ledger after `after` that the trusted validators reached quorum on, and its hash (None also
/// when off). With `XRPL_SIGN_ON_QUORUM` the validator signs that hash, not the first one a peer announces.
pub fn next_quorum_after(after: u32) -> Option<(u32, [u8; 32])> {
    if !enabled() {
        return None;
    }
    tally().lock().unwrap_or_else(|p| p.into_inner()).next_quorum_after(after)
}

/// .39 called ledger `seq` validated with `hash`, and our own engine computed that hash (independence 5.5, own_hash).
pub fn witness(seq: u32, hash: [u8; 32]) {
    if !enabled() {
        return;
    }
    tally().lock().unwrap_or_else(|p| p.into_inner()).witness(seq, hash);
}

/// The oldest recent ledger after `after` the validator may sign, its hash and why (None also when off): the
/// quorum's hash, or with `witness` the hash .39 and our own engine agree on when the quorum falls short here.
pub fn next_to_sign(after: u32, witness: bool) -> Option<(u32, [u8; 32], Basis)> {
    if !enabled() {
        return None;
    }
    tally().lock().unwrap_or_else(|p| p.into_inner()).next_to_sign(after, witness)
}

/// ws-sync landed ledger `seq`: our own hash of it, made from every part we compute, or None when a part was
/// borrowed (independence 5a, own_hash).
pub fn own_ledger(seq: u32, ours: Option<[u8; 32]>) {
    if !enabled() {
        return;
    }
    tally().lock().unwrap_or_else(|p| p.into_inner()).own_ledger(seq, ours);
}

/// Independence 5a: the oldest recent ledger after `after` whose hash we computed ourselves and may sign, its hash
/// and why ([`Tally::next_own_to_sign`]; None also when off).
pub fn next_own_to_sign(after: u32, witness: bool, breaker: bool) -> Option<(u32, [u8; 32], Basis)> {
    if !enabled() {
        return None;
    }
    tally().lock().unwrap_or_else(|p| p.into_inner()).next_own_to_sign(after, witness, breaker)
}

/// The transaction set more than half the trusted validators accepted for ledger `seq` (None also when off).
/// Independence 3.3 salts our canonical order with it, as rippled salts with the consensus set's root: a set can hold
/// transactions the ledger never filed, so the root of the ledger's own transactions is not always that salt.
pub fn agreed_set(seq: u32) -> Option<[u8; 32]> {
    if !enabled() {
        return None;
    }
    tally().lock().unwrap_or_else(|p| p.into_inner()).agreed_set(seq)
}

/// .39 called ledger `seq` validated (ws-sync, as it takes the ledger).
pub fn network_validated(seq: u32, hash: [u8; 32]) {
    if !enabled() || hash == [0; 32] {
        return;
    }
    let judged = tally().lock().unwrap_or_else(|p| p.into_inner()).network_validated(seq, hash, now_ms());
    settle(judged);
}

fn settle(judged: Vec<(u32, Verdict)>) {
    if judged.is_empty() {
        return;
    }
    let st = stats();
    let (q, unl, neg) = {
        let t = tally().lock().unwrap_or_else(|p| p.into_inner());
        (t.quorum(), t.unl_size(), t.negative_size())
    };
    for (seq, v) in judged {
        let n = st.judged.fetch_add(1, Ordering::Relaxed) + 1;
        st.last_seq.fetch_max(u64::from(seq), Ordering::Relaxed);
        let line = match v {
            Verdict::Agree { .. } => {
                st.agree.fetch_add(1, Ordering::Relaxed);
                None
            }
            Verdict::Differ { ours } => {
                st.differ.fetch_add(1, Ordering::Relaxed);
                Some(json!({"seq": seq, "differ": hex::encode_upper(ours), "quorum": q, "unl": unl, "negative_unl": neg}))
            }
            Verdict::Short { count, quorum } => {
                st.short.fetch_add(1, Ordering::Relaxed);
                Some(json!({"seq": seq, "short": count, "quorum": quorum, "unl": unl, "negative_unl": neg}))
            }
        };
        if let Some(l) = line {
            log_receipt(&l);
        }
        if n % 256 == 1 {
            eprintln!("[own-validated] heartbeat #{seq}: {}", summary());
        }
    }
}

fn log_receipt(line: &Value) {
    let path = std::env::var("XRPL_OWN_VALIDATED_LOG").unwrap_or_else(|_| "/mnt/xrpl-data/own_validated.jsonl".into());
    if std::fs::metadata(&path).map(|m| m.len() > 20 << 20).unwrap_or(false) {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{line}");
    }
}

pub fn summary() -> String {
    let st = stats();
    let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
    let t = tally().lock().unwrap_or_else(|p| p.into_inner());
    format!(
        "judged {} agree {} differ {} short {} | lead median {} ms | quorum {} of {} (negative UNL {}) | validations {} untrusted {} partial {}",
        l(&st.judged), l(&st.agree), l(&st.differ), l(&st.short),
        t.median_lead().map_or("-".into(), |m| m.to_string()), t.quorum(), t.unl_size(), t.negative_size(),
        l(&st.validations), l(&st.untrusted), l(&st.partial)
    )
}

/// The `/api/engine` section.
pub fn json() -> Value {
    let st = stats();
    let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
    let t = tally().lock().unwrap_or_else(|p| p.into_inner());
    json!({
        "enabled": enabled(), "last_seq": l(&st.last_seq), "judged": l(&st.judged), "agree": l(&st.agree),
        "differ": l(&st.differ), "short": l(&st.short), "lead_ms_median": t.median_lead(),
        "quorum": t.quorum(), "unl": t.unl_size(), "negative_unl": t.negative_size(),
        "validations": l(&st.validations), "untrusted": l(&st.untrusted), "partial": l(&st.partial),
        "unjudged": l(&st.unjudged),
    })
}

/// Extra /metrics lines.
pub fn metrics_text() -> String {
    let st = stats();
    let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
    let mut s = String::new();
    for (name, help, v) in [
        ("xrpl_own_validated_agree_total", "Ledgers we saw reach quorum on the hash .39 called validated.", l(&st.agree)),
        ("xrpl_own_validated_differ_total", "Ledgers where we saw quorum on another hash than .39's.", l(&st.differ)),
        ("xrpl_own_validated_short_total", "Ledgers .39 called validated that never reached quorum in our tally.", l(&st.short)),
        ("xrpl_own_validated_untrusted_total", "Validations from signing keys not on our list (rotated keys show here).", l(&st.untrusted)),
    ] {
        s.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n{name} {v}\n"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Validator i: master [i;33] (as hex), signing key [100+i;33].
    fn unl(n: u8) -> Vec<(String, Option<String>)> {
        (1..=n).map(|i| (hex::encode([i; 33]), Some(hex::encode([100 + i; 33])))).collect()
    }

    fn sk(i: u8) -> Vec<u8> {
        vec![100 + i; 33]
    }

    #[test]
    fn quorum_is_rippleds_with_the_negative_unl() {
        let mut t = Tally::default();
        t.set_unl(&unl(35));
        assert_eq!(t.quorum(), 28);
        t.set_negative_unl(vec![vec![1; 33]]);
        assert_eq!((t.negative_size(), t.quorum()), (1, 28), "0.8 × 34 = 27.2, up to 28");
        t.set_negative_unl((1..=7).map(|i| vec![i; 33]).collect());
        assert_eq!(t.quorum(), 23, "0.8 × 28 = 22.4 → 23, above 0.6 × 35 = 21");
        t.set_negative_unl(vec![vec![99; 33]]);
        assert_eq!(t.negative_size(), 0, "a key not on our list does not shrink it");
    }

    /// Mainnet #107652744: the validators' sfConsensusHash, not the root of the ledger's own transactions, is the set
    /// rippled salted its order with. More than half the trusted list must name it, short of a quorum if need be
    /// (#107700728); an untrusted key, or one on the negative UNL, never counts.
    #[test]
    fn the_agreed_set_is_the_consensus_hash_more_than_half_the_trusted_list_names() {
        let mut t = Tally::default();
        t.set_unl(&unl(35));
        let (ledger, set, other) = ([7u8; 32], [0xA5u8; 32], [0x5Au8; 32]);
        for i in 1..=17 {
            t.vote(&sk(i), 100, ledger, true, 0);
            t.note_set(&sk(i), 100, set);
        }
        t.vote(&[200; 33], 100, ledger, true, 0);
        t.note_set(&[200; 33], 100, set);
        assert_eq!(t.agreed_set(100), None, "17 trusted of 35 is not more than half, and an untrusted key adds nothing");
        t.vote(&sk(18), 100, ledger, true, 0);
        t.note_set(&sk(18), 100, other);
        assert_eq!(t.agreed_set(100), None, "18 voted, but only 17 named the set");
        t.vote(&sk(19), 100, ledger, true, 0);
        t.note_set(&sk(19), 100, set);
        assert_eq!(t.agreed_set(100), Some(set), "18 of 35 named it: more than half, though short of quorum 28");
        assert_eq!(t.agreed_set(101), None);
        t.set_negative_unl(vec![vec![1; 33]]);
        assert_eq!(t.agreed_set(100), None, "one of the 18 is on the negative UNL: 17 of 34 is not more than half");
        t.set_negative_unl(Vec::new());
        for i in 20..=35 {
            t.vote(&sk(i), 100, ledger, true, 0);
            t.note_set(&sk(i), 100, set);
        }
        assert_eq!(t.agreed_set(100), Some(set), "and a full quorum names it too");
    }

    #[test]
    fn a_ledger_agrees_once_the_quorum_of_trusted_full_validations_names_its_hash() {
        let mut t = Tally::default();
        t.set_unl(&unl(35));
        let h = [7u8; 32];
        t.vote(&sk(30), 99, [9; 32], true, 0); // listening since #99
        for i in 1..=27 {
            assert!(t.vote(&sk(i), 100, h, true, 1_000 + u64::from(i)).is_empty());
        }
        assert_eq!(t.network_validated(100, h, 2_000), vec![], "27 of 28: not yet");
        let v = t.vote(&sk(28), 100, h, true, 1_500);
        assert_eq!(v, vec![(100, Verdict::Agree { lead_ms: 500 })], "quorum at 1.5 s, .39 at 2.0 s: 500 ms ahead");
        assert_eq!(t.median_lead(), Some(500));
    }

    #[test]
    fn untrusted_partial_and_negative_unl_validations_do_not_count() {
        let mut t = Tally::default();
        t.set_unl(&unl(35));
        t.set_negative_unl(vec![vec![1; 33]]);
        let h = [7u8; 32];
        t.vote(&[200; 33], 5, h, true, 0); // not on the list
        t.vote(&sk(2), 5, h, false, 0); // not full
        t.vote(&sk(1), 5, h, true, 0); // on the negative UNL
        t.vote(&sk(3), 5, h, true, 0);
        assert_eq!(t.count(5, &h), 1);
        // A master key that signs for itself is trusted too.
        t.vote(&[4; 33], 5, h, true, 0);
        assert_eq!(t.count(5, &h), 2);
    }

    #[test]
    fn quorum_on_another_hash_differs_and_a_ledger_that_never_gets_there_is_short() {
        let mut t = Tally::default();
        t.set_unl(&unl(35));
        let (ours, theirs) = ([1u8; 32], [2u8; 32]);
        for i in 1..=28 {
            t.vote(&sk(i), 50, ours, true, 0);
        }
        assert_eq!(t.network_validated(50, theirs, 10), vec![], "the first voted sequence itself is not judged");
        for i in 1..=28 {
            t.vote(&sk(i), 51, ours, true, 0);
        }
        assert_eq!(t.network_validated(51, theirs, 10), vec![(51, Verdict::Differ { ours })]);
        assert!(t.network_validated(60, theirs, 10).is_empty(), "nobody voted yet: wait");
        assert!(t.network_validated(60 + GIVE_UP - 1, theirs, 10).iter().all(|(s, _)| *s != 60));
        let late = t.network_validated(60 + GIVE_UP, theirs, 10);
        assert!(late.contains(&(60, Verdict::Short { count: 0, quorum: 28 })), "{late:?}");
    }

    #[test]
    fn nothing_is_judged_before_the_list_loads_or_before_the_tally_listened() {
        let mut t = Tally::default();
        assert!(t.network_validated(10, [1; 32], 0).is_empty(), "no list yet");
        t.set_unl(&unl(35));
        assert!(t.network_validated(11, [1; 32], 0).is_empty(), "no trusted validation yet");
        t.vote(&sk(1), 20, [1; 32], true, 0);
        assert!(t.network_validated(19, [1; 32], 0).is_empty(), "voted on before we listened");
        assert!(t.network_validated(19 + 2 * GIVE_UP, [1; 32], 0).is_empty(), "and never judged short later");
        assert!(t.pending.is_empty() || !t.pending.contains_key(&19));
    }

    #[test]
    fn a_validator_counts_once_per_ledger_with_its_latest_hash() {
        let mut t = Tally::default();
        t.set_unl(&unl(35));
        t.vote(&sk(1), 9, [1; 32], true, 0);
        t.vote(&sk(1), 9, [1; 32], true, 1);
        assert_eq!(t.count(9, &[1; 32]), 1);
        t.vote(&sk(1), 9, [2; 32], true, 2);
        assert_eq!((t.count(9, &[1; 32]), t.count(9, &[2; 32])), (0, 1));
    }

    /// Mainnet #107746977 (16:37, 18 Oct): the first peer to announce it had built another ledger (0EFB756A…), and we
    /// signed that; the trusted validators validated 686B488A…. The ledger to sign comes from their quorum instead.
    #[test]
    fn the_ledger_to_sign_is_the_one_the_trusted_validators_reached_quorum_on() {
        let mut t = Tally::default();
        t.set_unl(&unl(35));
        let (peer_said, validated) = ([0x0Eu8; 32], [0x68u8; 32]);
        assert_eq!(t.next_quorum_after(0), None, "nothing heard yet");
        for i in 1..=3 {
            t.vote(&sk(i), 100, peer_said, true, 0);
        }
        for i in 4..=30 {
            t.vote(&sk(i), 100, validated, true, 0);
        }
        assert_eq!(t.next_quorum_after(99), None, "27 of quorum 28: not yet");
        t.vote(&sk(31), 100, validated, true, 0);
        assert_eq!(t.next_quorum_after(99), Some((100, validated)));
        assert_eq!(t.next_quorum_after(100), None, "already taken");
        t.vote(&[200; 33], 101, validated, true, 0);
        assert_eq!(t.next_quorum_after(100), None, "an untrusted key brings no quorum");
    }

    #[test]
    fn the_ledger_to_sign_is_the_oldest_recent_one_after_the_last() {
        let mut t = Tally::default();
        t.set_unl(&unl(35));
        for seq in [200, 201, 203] {
            for i in 1..=28 {
                t.vote(&sk(i), seq, [seq as u8; 32], true, 0);
            }
        }
        for i in 1..=10 {
            t.vote(&sk(i), 202, [202u8; 32], true, 0); // short of quorum so far
        }
        assert_eq!(t.next_quorum_after(199), Some((200, [200; 32])));
        assert_eq!(t.next_quorum_after(201), Some((203, [203; 32])), "#202 has no quorum: the next one that does");
        for i in 1..=28 {
            t.vote(&sk(i), 203 + GIVE_UP + 1, [9; 32], true, 0);
        }
        assert_eq!(t.next_quorum_after(0), Some((203 + GIVE_UP + 1, [9; 32])), "older than GIVE_UP behind the newest: not signed late");
    }

    /// Mainnet #107783807-814 (20 Oct): only 26 or 27 trusted validations of quorum 28 reached us for eight ledgers
    /// in a row, so cycle 183 signed none of them, though .39 validated each and our own hash of each was its hash.
    #[test]
    fn a_ledger_short_of_quorum_here_is_signed_on_39s_word_and_our_own_hash() {
        let mut t = Tally::default();
        t.set_unl(&unl(35));
        let validated = [0xC1u8; 32];
        for i in 1..=27 {
            t.vote(&sk(i), 300, validated, true, 0);
        }
        assert_eq!(t.next_to_sign(299, true), None, "27 of quorum 28, and no witness yet");
        t.witness(300, validated);
        assert_eq!(t.next_to_sign(299, true), Some((300, validated, Basis::Witness)));
        assert_eq!(t.next_to_sign(299, false), None, "the witness is off");
        assert_eq!(t.next_to_sign(300, true), None, "already taken");
        t.vote(&sk(28), 300, validated, true, 0);
        assert_eq!(t.next_to_sign(299, true), Some((300, validated, Basis::Quorum)), "the quorum, once it is there");
    }

    #[test]
    fn the_witness_never_signs_past_a_quorum_or_against_one() {
        let mut t = Tally::default();
        t.set_unl(&unl(35));
        for i in 1..=28 {
            t.vote(&sk(i), 401, [41; 32], true, 0);
            t.vote(&sk(i), 402, [42; 32], true, 0);
        }
        t.witness(400, [40; 32]);
        t.witness(402, [7; 32]);
        assert_eq!(t.next_to_sign(399, true), Some((400, [40; 32], Basis::Witness)), "the oldest first, whichever says so");
        assert_eq!(t.next_to_sign(400, true), Some((401, [41; 32], Basis::Quorum)));
        assert_eq!(t.next_to_sign(401, true), Some((402, [42; 32], Basis::Quorum)), "a quorum on another hash wins");
        t.vote(&sk(1), 404, [44; 32], true, 0);
        t.witness(403, [7; 32]);
        for i in 1..=28 {
            t.vote(&sk(i), 403, [43; 32], true, 0);
        }
        assert_eq!(t.next_to_sign(402, true), Some((403, [43; 32], Basis::Quorum)), "never against the quorum here");
        t.witness(404, [44; 32]);
        assert_eq!(t.next_to_sign(403, true), Some((404, [44; 32], Basis::Witness)));
        t.witness(404 + GIVE_UP + 1, [9; 32]);
        assert_eq!(t.next_to_sign(403, true), Some((404 + GIVE_UP + 1, [9; 32], Basis::Witness)),
            "#404 is more than GIVE_UP behind the newest: not signed late");
    }

    fn quorum_on(t: &mut Tally, seq: u32, hash: [u8; 32]) {
        for i in 1..=28 {
            t.vote(&sk(i), seq, hash, true, 0);
        }
    }

    /// Independence 5a: the validator signs a ledger only with a hash it made itself, once the quorum names it too.
    #[test]
    fn under_5a_a_ledger_is_signed_with_our_own_hash_once_it_lands_and_the_quorum_agrees() {
        let mut t = Tally::default();
        t.set_unl(&unl(35));
        own_and_quorum(&mut t, 499, [49; 32], Some([49; 32]));
        quorum_on(&mut t, 500, [50; 32]);
        assert_eq!(t.next_own_to_sign(499, true, false), None, "the quorum is in, our own hash not yet: wait");
        assert_eq!(t.next_own_to_sign(499, true, true), None, "waiting is not a stall, even with the breaker");
        t.own_ledger(500, Some([50; 32]));
        assert_eq!(t.next_own_to_sign(499, true, false), Some((500, [50; 32], Basis::Own)));
        own_and_quorum(&mut t, 501, [51; 32], Some([66; 32]));
        own_and_quorum(&mut t, 502, [52; 32], None);
        own_and_quorum(&mut t, 503, [53; 32], Some([53; 32]));
        assert_eq!(t.next_own_to_sign(500, true, false), Some((503, [53; 32], Basis::Own)),
            "#501 ours differs and #502 borrowed a part: neither is signed");
        assert_eq!(t.next_own_to_sign(500, true, true), Some((501, [51; 32], Basis::Quorum)),
            "with the breaker, the quorum's hash for a ledger we cannot make");
    }

    fn own_and_quorum(t: &mut Tally, seq: u32, quorum: [u8; 32], ours: Option<[u8; 32]>) {
        quorum_on(t, seq, quorum);
        t.own_ledger(seq, ours);
    }

    #[test]
    fn under_5a_short_of_quorum_our_hash_is_signed_only_when_39_validated_it_too() {
        let mut t = Tally::default();
        t.set_unl(&unl(35));
        for i in 1..=27 {
            t.vote(&sk(i), 600, [60; 32], true, 0);
            t.vote(&sk(i), 601, [61; 32], true, 0);
        }
        t.own_ledger(600, Some([60; 32]));
        t.witness(600, [60; 32]);
        t.own_ledger(601, Some([61; 32])); // .39 validated another hash: no witness
        assert_eq!(t.next_own_to_sign(599, true, false), Some((600, [60; 32], Basis::Witness)));
        assert_eq!(t.next_own_to_sign(599, false, false), None, "no witness: wait for the quorum");
        assert_eq!(t.next_own_to_sign(600, true, true), None, "ours is not .39's, and no quorum to fall back on");
    }

    #[test]
    fn under_5a_a_stalled_ws_sync_is_waited_for_until_the_breaker_signs_the_quorums_hash() {
        let mut t = Tally::default();
        t.set_unl(&unl(35));
        own_and_quorum(&mut t, 700, [70; 32], Some([70; 32]));
        for seq in 701..=704 {
            quorum_on(&mut t, seq, [seq as u8; 32]);
        }
        assert_eq!(t.next_own_to_sign(700, true, false), None, "never a hash we did not make");
        assert_eq!(t.next_own_to_sign(700, true, true), Some((701, [701u16 as u8; 32], Basis::Quorum)),
            "ledgers 4 past the newest landed: a stall");
        let mut fresh = Tally::default();
        fresh.set_unl(&unl(35));
        quorum_on(&mut fresh, 800, [80; 32]);
        assert_eq!(fresh.next_own_to_sign(0, true, false), None, "nothing landed since the start: wait");
        assert_eq!(fresh.next_own_to_sign(0, true, true), Some((800, [80; 32], Basis::Quorum)));
    }

    #[test]
    fn every_metrics_line_starts_at_the_margin() {
        for line in metrics_text().lines() {
            assert!(!line.starts_with(' '), "{line:?}");
        }
    }
}
