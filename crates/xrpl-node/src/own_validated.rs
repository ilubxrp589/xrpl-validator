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
//! It decides nothing. `XRPL_OWN_VALIDATED=1` switches it on (off by default); differences go to
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
}

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
    let judged = tally().lock().unwrap_or_else(|p| p.into_inner()).vote(&v.signing_key, v.ledger_seq, v.ledger_hash, v.full, now);
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

    #[test]
    fn every_metrics_line_starts_at_the_margin() {
        for line in metrics_text().lines() {
            assert!(!line.starts_with(' '), "{line:?}");
        }
    }
}
