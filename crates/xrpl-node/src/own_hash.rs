//! Independence task 5.5, first form: our own ledger hash, trailing the network.
//!
//! A ledger's hash is the fingerprint of its header (`calculateLedgerHash`, Ledger.cpp, 3.4.1): sha512Half over
//! the sequence, the total XRP, the parent's hash, the transaction tree's root, the state tree's root, the
//! parent's close time, the close time, its resolution and the close flags. Step 5 signs a hash we computed
//! ourselves; 5.4 will build it inside the consensus round and 5.5 gauges it. This is that gauge before the
//! round exists: once ws-sync has landed a ledger, it assembles the header from what this validator produced
//! itself, hashes it, and holds the hash against the network's:
//! - **state root**: our hasher's root over state.rocks after the ledger. Ours when every byte the ledger wrote
//!   was our engine's: the native writer's proposal, and the skip list and protocol singletons (which ws-sync
//!   still fetches) byte-equal to our mirror's own ([`crate::native_shadow`]).
//! - **transaction root**: the tree of our own records (Step 2, [`crate::native_meta`]). The records carry the
//!   network's positions, so the root is ours only when our order and results matched the network's on that
//!   ledger too (3.3, [`crate::native_order`]).
//! - **total XRP**: our previous total less the fees our engine burned (3.4).
//! - **parent hash**: our own hash of the previous ledger.
//! - **resolution** (5.3, `getNextLedgerTimeResolution`) and **parent close time**: from our previous ledger.
//! - **close time and close flags**: the network's. The validators vote them inside the round (5.2, 5.4), so a
//!   validator that trails the network cannot have its own.
//!
//! A part we could not make ourselves is taken from the network's header and named ([`Judged::borrowed`]), so
//! the gauge says what is still borrowed. The parent, total and times chain from our previous ledger only when
//! its hash matched the network's: one miss re-seeds the next ledger from the header instead of making every
//! later hash differ. With 4.5 on, each hash is also held against the one the trusted validators reached
//! quorum on ([`crate::own_validated::quorum_hash`]).
//!
//! What it decides: a hash .39 called validated that equals ours, made from every part, is a witness the
//! validator may sign on when the trusted validations reaching us fall short of quorum (cycle 184,
//! [`crate::own_validated::witness`]); and with independence 5a (`XRPL_SIGN_OWN=1`, cycle 185) the validator
//! signs a ledger only with this hash ([`crate::own_validated::own_ledger`]). `XRPL_OWN_HASH=1` switches it on
//! (off by default); differences go to
//! `XRPL_OWN_HASH_LOG` (default /mnt/xrpl-data/own_hash.jsonl, capped at 20 MB).

use std::io::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};
use xrpl_ledger::ledger::close_time::{close_agreed, next_resolution};

use crate::state_hash::compute_ledger_hash;
use crate::ws_sync::LedgerHeader;

/// `XRPL_OWN_HASH=1`, read once.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("XRPL_OWN_HASH").map(|v| v == "1").unwrap_or(false))
}

/// The parts of a header that can be borrowed, in the order the gauge lists them.
pub const PARTS: [&str; 6] = ["state", "transactions", "order", "total", "parent", "times"];

/// What this validator produced for one ledger.
#[derive(Debug, Clone, Default)]
pub struct Parts {
    /// Our hasher's root over state.rocks once the ledger landed.
    pub state_root: Option<[u8; 32]>,
    /// Why the bytes behind that root were not all our engine's (None: they were).
    pub state_not_ours: Option<String>,
    /// The root of the transaction tree built from our own records, when every record was built.
    pub tx_root: Option<[u8; 32]>,
    /// Our order and results matched the network's on this ledger (None: not compared, order off).
    pub order_ours: Option<bool>,
    /// The fees our engine burned applying the ledger.
    pub burned: Option<u64>,
}

/// One ledger's own hash, judged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judged {
    pub ours: [u8; 32],
    /// The parts taken from the network's header ([`PARTS`]); empty: all ours but the voted close time.
    pub borrowed: Vec<&'static str>,
    /// Ours is the network's hash.
    pub network: bool,
    /// Ours is the hash the trusted validators reached quorum on (None: no quorum hash known).
    pub validators: Option<bool>,
    /// When ours differs from the network's: the header fields it differs in.
    pub differs_in: Vec<&'static str>,
}

/// What the next ledger chains from: our previous ledger, kept only when its hash matched the network's.
#[derive(Debug, Clone)]
struct Prev {
    seq: u32,
    hash: [u8; 32],
    total: u64,
    close_time: u32,
    resolution: u8,
    flags: u8,
}

#[derive(Debug, Default)]
pub struct Tracker {
    prev: Option<Prev>,
    /// Consecutive ledgers whose hash, with nothing borrowed, matched the network's.
    pub streak: u64,
    pub best_streak: u64,
}

impl Tracker {
    /// Ledger `seq` landed: `net` is the network's header and `net_account` its state root, `parts` what we made,
    /// `validators` the hash our own tally reached quorum on.
    pub fn judge(
        &mut self,
        seq: u32,
        net: &LedgerHeader,
        net_account: &[u8; 32],
        parts: &Parts,
        validators: Option<[u8; 32]>,
    ) -> Judged {
        let prev = self.prev.take().filter(|p| p.seq.checked_add(1) == Some(seq));
        let mut borrowed = Vec::new();
        let parent = match &prev {
            Some(p) => p.hash,
            None => {
                borrowed.push("parent");
                net.parent_hash
            }
        };
        let total = match (prev.as_ref().map(|p| p.total), parts.burned) {
            (Some(t), Some(b)) if b <= t => t - b,
            _ => {
                borrowed.push("total");
                net.total_drops
            }
        };
        let (resolution, parent_close) = match &prev {
            Some(p) => {
                let r = next_resolution(u32::from(p.resolution), close_agreed(p.flags), seq);
                (u8::try_from(r).unwrap_or(p.resolution), p.close_time)
            }
            None => {
                borrowed.push("times");
                (net.close_resolution, net.parent_close_time)
            }
        };
        let tx = match parts.tx_root {
            Some(r) => {
                if parts.order_ours != Some(true) {
                    borrowed.push("order");
                }
                r
            }
            None => {
                borrowed.push("transactions");
                net.transaction_hash
            }
        };
        let account = match parts.state_root {
            Some(r) => {
                if parts.state_not_ours.is_some() {
                    borrowed.push("state");
                }
                r
            }
            None => {
                borrowed.push("state");
                *net_account
            }
        };
        borrowed.sort_by_key(|b| PARTS.iter().position(|p| p == b));
        let ours = compute_ledger_hash(
            seq, total, &parent, &tx, &account, parent_close, net.close_time, resolution, net.close_flags,
        )
        .0;
        let network = ours == net.ledger_hash;
        let mut differs_in = Vec::new();
        if !network {
            for (name, d) in [
                ("state", account != *net_account),
                ("transactions", tx != net.transaction_hash),
                ("total", total != net.total_drops),
                ("parent", parent != net.parent_hash),
                ("parent_close_time", parent_close != net.parent_close_time),
                ("resolution", resolution != net.close_resolution),
            ] {
                if d {
                    differs_in.push(name);
                }
            }
            if differs_in.is_empty() {
                differs_in.push("header"); // every part equal yet another hash: the network's header is not its hash
            }
        }
        if network && borrowed.is_empty() {
            self.streak += 1;
            self.best_streak = self.best_streak.max(self.streak);
        } else {
            self.streak = 0;
        }
        if network {
            self.prev = Some(Prev { seq, hash: ours, total, close_time: net.close_time, resolution, flags: net.close_flags });
        }
        Judged { ours, borrowed, network, validators: validators.map(|v| v == ours), differs_in }
    }
}

// ---- the live tracker, counters, receipts, /api/engine and /metrics -------------------------------------------

fn tracker() -> &'static Mutex<Tracker> {
    static T: OnceLock<Mutex<Tracker>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(Tracker::default()))
}

#[derive(Default)]
pub struct HashStats {
    pub ledgers: AtomicU64,
    /// Nothing borrowed, and the network's hash.
    pub own: AtomicU64,
    /// Something borrowed, and still the network's hash.
    pub partial: AtomicU64,
    pub differ: AtomicU64,
    pub validators_agree: AtomicU64,
    pub validators_differ: AtomicU64,
    /// Ledgers 4.5 had no quorum hash for (off, or not reached yet).
    pub validators_none: AtomicU64,
    /// How often each of [`PARTS`] was borrowed.
    pub borrowed: [AtomicU64; 6],
    pub streak: AtomicU64,
    pub best_streak: AtomicU64,
    pub last_seq: AtomicU64,
}

pub fn stats() -> &'static HashStats {
    static S: OnceLock<HashStats> = OnceLock::new();
    S.get_or_init(HashStats::default)
}

/// ws-sync landed ledger `seq` (the writer committed it): judge our own hash of it. `net_account` is the
/// network's state root as ws-sync fetched it (hex).
pub fn ledger(seq: u32, net: &LedgerHeader, net_account: &str, parts: &Parts) {
    if !enabled() || net.ledger_hash == [0; 32] {
        return;
    }
    let Some(acct) = hex::decode(net_account).ok().and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok()) else {
        return;
    };
    let validators = crate::own_validated::quorum_hash(seq);
    let (j, streak, best) = {
        let mut t = tracker().lock().unwrap_or_else(|p| p.into_inner());
        let j = t.judge(seq, net, &acct, parts, validators);
        (j, t.streak, t.best_streak)
    };
    // .39's validated hash, and ours from every part we make: the validator may sign it when the trusted
    // validations reaching us fall short of quorum (cycle 184).
    if j.network && j.borrowed.is_empty() {
        crate::own_validated::witness(seq, j.ours);
    }
    // Independence 5a: the validator signs a ledger only with a hash it made itself.
    crate::own_validated::own_ledger(seq, j.borrowed.is_empty().then_some(j.ours));
    let st = stats();
    let n = st.ledgers.fetch_add(1, Ordering::Relaxed) + 1;
    st.last_seq.store(u64::from(seq), Ordering::Relaxed);
    st.streak.store(streak, Ordering::Relaxed);
    st.best_streak.store(best, Ordering::Relaxed);
    for b in &j.borrowed {
        if let Some(i) = PARTS.iter().position(|p| p == b) {
            st.borrowed[i].fetch_add(1, Ordering::Relaxed);
        }
    }
    match (j.network, j.borrowed.is_empty()) {
        (true, true) => st.own.fetch_add(1, Ordering::Relaxed),
        (true, false) => st.partial.fetch_add(1, Ordering::Relaxed),
        (false, _) => st.differ.fetch_add(1, Ordering::Relaxed),
    };
    match j.validators {
        Some(true) => st.validators_agree.fetch_add(1, Ordering::Relaxed),
        Some(false) => st.validators_differ.fetch_add(1, Ordering::Relaxed),
        None => st.validators_none.fetch_add(1, Ordering::Relaxed),
    };
    if !j.network || j.validators == Some(false) {
        log_receipt(&json!({
            "seq": seq, "ours": hex::encode_upper(j.ours), "network": hex::encode_upper(net.ledger_hash),
            "validators": validators.map(hex::encode_upper), "differs_in": j.differs_in, "borrowed": j.borrowed,
            "state_not_ours": parts.state_not_ours,
        }));
    }
    if n % 256 == 1 {
        eprintln!("[own-hash] heartbeat #{seq}: {}", summary());
    }
}

fn log_receipt(line: &Value) {
    let path = std::env::var("XRPL_OWN_HASH_LOG").unwrap_or_else(|_| "/mnt/xrpl-data/own_hash.jsonl".into());
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
    let borrowed: Vec<String> = PARTS.iter().zip(&st.borrowed).map(|(p, a)| format!("{p} {}", l(a))).collect();
    format!(
        "ledgers {} | own {} (streak {}, best {}) partial {} differ {} | validators agree {} differ {} none {} | borrowed {}",
        l(&st.ledgers), l(&st.own), l(&st.streak), l(&st.best_streak), l(&st.partial), l(&st.differ),
        l(&st.validators_agree), l(&st.validators_differ), l(&st.validators_none), borrowed.join(" ")
    )
}

/// The `/api/engine` section.
pub fn json() -> Value {
    let st = stats();
    let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
    let borrowed: serde_json::Map<String, Value> = PARTS.iter().zip(&st.borrowed).map(|(p, a)| (p.to_string(), json!(l(a)))).collect();
    json!({
        "enabled": enabled(), "last_seq": l(&st.last_seq), "ledgers": l(&st.ledgers), "own": l(&st.own),
        "partial": l(&st.partial), "differ": l(&st.differ), "streak": l(&st.streak), "best_streak": l(&st.best_streak),
        "validators_agree": l(&st.validators_agree), "validators_differ": l(&st.validators_differ),
        "validators_none": l(&st.validators_none), "borrowed": borrowed,
    })
}

/// Extra /metrics lines.
pub fn metrics_text() -> String {
    let st = stats();
    let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
    let mut s = String::new();
    for (name, help, v) in [
        ("xrpl_own_hash_own_total", "Ledgers whose hash, from our own parts (close time aside), matched the network's.", l(&st.own)),
        ("xrpl_own_hash_partial_total", "Ledgers whose hash matched with some part taken from the network's header.", l(&st.partial)),
        ("xrpl_own_hash_differ_total", "Ledgers whose own hash differed from the network's.", l(&st.differ)),
        ("xrpl_own_hash_validators_agree_total", "Own hashes equal to the hash the trusted validators reached quorum on.", l(&st.validators_agree)),
        ("xrpl_own_hash_validators_differ_total", "Own hashes another than the trusted validators' quorum hash.", l(&st.validators_differ)),
    ] {
        s.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n{name} {v}\n"));
    }
    let name = "xrpl_own_hash_streak";
    s.push_str(&format!(
        "# HELP {name} Consecutive ledgers whose own hash, nothing borrowed, matched.\n# TYPE {name} gauge\n{name} {}\n",
        l(&st.streak)
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &str) -> [u8; 32] {
        hex::decode(s).unwrap().try_into().unwrap()
    }

    /// Mainnet #107640335 and #107640336 (.39's xrpld, 2026-10-13): the second's close time is its parent's
    /// plus one second (`effCloseTime`), and it burned 1,074 drops.
    fn mainnet() -> [(u32, LedgerHeader, [u8; 32]); 2] {
        [
            (
                107_640_335,
                LedgerHeader {
                    parent_hash: h("7078E88C5F879C4EF8B6590EB6814FE9313B99D666540F0F6BF18514478145C0"),
                    parent_close_time: 845_256_841,
                    total_drops: 99_985_591_659_309_732,
                    transaction_hash: h("F5C53A1A668B4DC7A8BA6518913B47F96955DD068B51F28284DCFD0056E99724"),
                    close_time: 845_256_850,
                    close_resolution: 10,
                    close_flags: 0,
                    ledger_hash: h("A6D3CD1939E46999635BF5B8DE2CD038C786BB96FA744CAA73384E516F909AE1"),
                },
                h("FEB6E364C5E255AB13B95D7C95F12ABEEC9DBAAC45783885A47D40A214A51499"),
            ),
            (
                107_640_336,
                LedgerHeader {
                    parent_hash: h("A6D3CD1939E46999635BF5B8DE2CD038C786BB96FA744CAA73384E516F909AE1"),
                    parent_close_time: 845_256_850,
                    total_drops: 99_985_591_659_308_658,
                    transaction_hash: h("D61E6ECEE75E5B973B711297BB77E91BCA0A373F16AEBB40D66BA8F47375FEB3"),
                    close_time: 845_256_851,
                    close_resolution: 10,
                    close_flags: 0,
                    ledger_hash: h("009838E5AC999D39814E9C77C4870F491EF61F5A83DB2484D28648E56B39075D"),
                },
                h("F7A174D37EADE94D82DBB9EF6549D49E3BFB1DD607AD3A9A0F355F2E5CB8C18F"),
            ),
        ]
    }

    /// Everything ours, each part equal to the network's.
    fn ours(net: &LedgerHeader, acct: [u8; 32], burned: u64) -> Parts {
        Parts { state_root: Some(acct), state_not_ours: None, tx_root: Some(net.transaction_hash), order_ours: Some(true), burned: Some(burned) }
    }

    #[test]
    fn two_mainnet_ledgers_chain_into_the_networks_hashes() {
        let [(s1, n1, a1), (s2, n2, a2)] = mainnet();
        let mut t = Tracker::default();
        let j1 = t.judge(s1, &n1, &a1, &ours(&n1, a1, 0), None);
        assert!(j1.network, "the first ledger, seeded from the header, is the network's");
        assert_eq!(j1.borrowed, vec!["total", "parent", "times"], "nothing to chain from yet");
        assert_eq!(t.streak, 0);
        let j2 = t.judge(s2, &n2, &a2, &ours(&n2, a2, 1_074), Some(n2.ledger_hash));
        assert_eq!(j2.ours, n2.ledger_hash, "our parent, total, resolution and parent close time: mainnet's hash");
        assert!(j2.borrowed.is_empty(), "{:?}", j2.borrowed);
        assert_eq!((j2.network, j2.validators, t.streak), (true, Some(true), 1));
    }

    #[test]
    fn a_wrong_burn_shows_in_the_total_and_the_next_ledger_reseeds() {
        let [(s1, n1, a1), (s2, n2, a2)] = mainnet();
        let mut t = Tracker::default();
        t.judge(s1, &n1, &a1, &ours(&n1, a1, 0), None);
        let j = t.judge(s2, &n2, &a2, &ours(&n2, a2, 1_000), None);
        assert_eq!((j.network, j.differs_in.clone()), (false, vec!["total"]));
        assert_eq!(t.streak, 0);
        // Ledger 3 after a miss: no previous to chain from, so it borrows instead of inheriting the miss.
        let mut n3 = n2.clone();
        n3.parent_hash = n2.ledger_hash;
        n3.ledger_hash = compute_ledger_hash(s2 + 1, n3.total_drops, &n3.parent_hash, &n3.transaction_hash, &a2, n3.parent_close_time, n3.close_time, 10, 0).0;
        let j3 = t.judge(s2 + 1, &n3, &a2, &ours(&n3, a2, 0), None);
        assert!(j3.network);
        assert_eq!(j3.borrowed, vec!["total", "parent", "times"]);
    }

    #[test]
    fn our_state_root_differing_is_named_and_borrowed_bytes_still_count_as_borrowed() {
        let [(s1, n1, a1), (s2, n2, a2)] = mainnet();
        let mut t = Tracker::default();
        t.judge(s1, &n1, &a1, &ours(&n1, a1, 0), None);
        let mut p = ours(&n2, a2, 1_074);
        p.state_root = Some([9; 32]);
        let j = t.judge(s2, &n2, &a2, &p, Some(n2.ledger_hash));
        assert_eq!((j.network, j.differs_in.clone(), j.validators), (false, vec!["state"], Some(false)));

        let mut t = Tracker::default();
        t.judge(s1, &n1, &a1, &ours(&n1, a1, 0), None);
        let mut p = ours(&n2, a2, 1_074);
        p.state_not_ours = Some("the network's bytes".into());
        let j = t.judge(s2, &n2, &a2, &p, None);
        assert!(j.network, "the root is right");
        assert_eq!(j.borrowed, vec!["state"], "but the bytes behind it were the network's");
        assert_eq!(t.streak, 0);
    }

    #[test]
    fn records_in_an_order_we_did_not_match_borrow_the_order_and_no_records_borrow_the_transactions() {
        let [(s1, n1, a1), (s2, n2, a2)] = mainnet();
        let mut t = Tracker::default();
        t.judge(s1, &n1, &a1, &ours(&n1, a1, 0), None);
        let mut p = ours(&n2, a2, 1_074);
        p.order_ours = None;
        assert_eq!(t.judge(s2, &n2, &a2, &p, None).borrowed, vec!["order"]);

        let mut t = Tracker::default();
        t.judge(s1, &n1, &a1, &ours(&n1, a1, 0), None);
        let mut p = ours(&n2, a2, 1_074);
        p.tx_root = None;
        let j = t.judge(s2, &n2, &a2, &p, None);
        assert_eq!((j.network, j.borrowed), (true, vec!["transactions"]));
    }

    #[test]
    fn a_gap_reseeds_and_an_unagreed_close_time_coarsens_the_next_resolution() {
        let [(s1, n1, a1), _] = mainnet();
        let mut t = Tracker::default();
        let mut n0 = n1.clone();
        n0.close_flags = 1; // the validators agreed to disagree on #s1's close time
        n0.ledger_hash = compute_ledger_hash(s1, n0.total_drops, &n0.parent_hash, &n0.transaction_hash, &a1, n0.parent_close_time, n0.close_time, 10, 1).0;
        assert!(t.judge(s1, &n0, &a1, &ours(&n0, a1, 0), None).network);
        // The next ledger's resolution must be one step coarser (10 → 20), its parent close time ours.
        let mut n = n0.clone();
        n.parent_hash = n0.ledger_hash;
        n.parent_close_time = n0.close_time;
        n.close_time = n0.close_time + 20;
        n.close_resolution = 20;
        n.close_flags = 0;
        n.ledger_hash = compute_ledger_hash(s1 + 1, n.total_drops, &n.parent_hash, &n.transaction_hash, &a1, n.parent_close_time, n.close_time, 20, 0).0;
        let j = t.judge(s1 + 1, &n, &a1, &ours(&n, a1, 0), None);
        assert!(j.network && j.borrowed.is_empty(), "{j:?}");
        // A sequence skipped: nothing to chain from.
        let j = t.judge(s1 + 5, &n, &a1, &ours(&n, a1, 0), None);
        assert!(j.borrowed.contains(&"parent"));
    }

    #[test]
    fn every_metrics_line_starts_at_the_margin() {
        for line in metrics_text().lines() {
            assert!(!line.starts_with(' '), "{line:?}");
        }
    }
}
