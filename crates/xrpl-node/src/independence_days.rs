//! The count the Independence page shows: "Done = 30 days signing a hash we computed ourselves, from peer data".
//!
//! Every signature the validator sends is tallied by its UTC day: whether the hash was our own (independence 5a,
//! or cycle 184's witness, both checked against our own hash of the ledger) or borrowed (signed without that
//! check), and whether that day ran our own order (Step 3, `XRPL_NATIVE_ORDER=on`) and peer data (Step 4's source
//! switch, [`crate::peer_fetch::is_source`]) the whole time. A day counts when every signature was our own, both
//! steps were on all day, and the signatures cover at least [`MIN_COVERAGE`] of the ledgers between the day's
//! first and last (a restart's warm-up and a short outage pass; a long one does not). The count is the run of
//! such days up to yesterday; today joins it once it is over.
//!
//! The days live in `XRPL_INDEPENDENCE_DAYS` (default /mnt/xrpl-data/independence_days.json, the last
//! [`KEEP_DAYS`]), so a restart keeps them.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The share of a day's ledgers it must have signed.
pub const MIN_COVERAGE: f64 = 0.98;
/// Days kept in the file.
pub const KEEP_DAYS: usize = 62;
/// Signatures between two writes of the file (about four minutes).
const SAVE_EVERY: u64 = 64;

/// One UTC day's signatures.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Day {
    pub signed: u64,
    /// Signed with our own hash of the ledger.
    pub own: u64,
    /// Signed without it.
    pub borrowed: u64,
    /// The first and last ledger signed.
    pub first: u32,
    pub last: u32,
    /// Our own order (Step 3) and peer data (Step 4) at every signature.
    pub order_on: bool,
    pub peers: bool,
}

impl Day {
    /// The share of the ledgers from the first to the last it signed.
    pub fn coverage(&self) -> f64 {
        if self.signed == 0 || self.last < self.first {
            return 0.0;
        }
        self.signed as f64 / f64::from(self.last - self.first + 1)
    }

    /// Why this day does not count (empty: it does).
    pub fn misses(&self) -> Vec<String> {
        let mut m = Vec::new();
        if self.signed == 0 {
            m.push("nothing signed".to_string());
            return m;
        }
        if self.borrowed > 0 {
            m.push(format!("{} signed without our own hash", self.borrowed));
        }
        if !self.order_on {
            m.push("our own order was off (Step 3)".to_string());
        }
        if !self.peers {
            m.push("the ledger data came from .39, not peers (Step 4)".to_string());
        }
        if self.coverage() < MIN_COVERAGE {
            m.push(format!("signed {:.1}% of its ledgers", self.coverage() * 100.0));
        }
        m
    }
}

/// The days, by UTC date ("2026-10-21").
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Days {
    pub days: BTreeMap<String, Day>,
}

impl Days {
    /// A signature on `date` for ledger `seq`.
    pub fn signed(&mut self, date: &str, seq: u32, own: bool, order_on: bool, peers: bool) {
        let d = self.days.entry(date.to_string()).or_insert_with(|| Day { first: seq, last: seq, order_on, peers, ..Day::default() });
        d.signed += 1;
        if own {
            d.own += 1;
        } else {
            d.borrowed += 1;
        }
        d.first = d.first.min(seq);
        d.last = d.last.max(seq);
        d.order_on &= order_on;
        d.peers &= peers;
        while self.days.len() > KEEP_DAYS {
            self.days.pop_first();
        }
    }

    /// The run of counted days that ends the day before `today`.
    pub fn streak(&self, today: &str) -> u32 {
        let mut n = 0;
        let mut want = previous_date(today);
        while let Some(d) = want.as_deref().and_then(|w| self.days.get(w)) {
            if !d.misses().is_empty() {
                break;
            }
            n += 1;
            want = want.as_deref().and_then(previous_date);
        }
        n
    }
}

/// The UTC date of a unix time, "YYYY-MM-DD" (Howard Hinnant's `civil_from_days`).
pub fn utc_date(unix_secs: u64) -> String {
    let z = (unix_secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The day before a "YYYY-MM-DD" date.
fn previous_date(date: &str) -> Option<String> {
    let mut parts = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (y, m, d) = (parts.next()??, parts.next()??, parts.next()??);
    // days_from_civil, then one day back
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from((days - 1) * 86_400).ok().map(utc_date)
}

// ---- the live tally, its file and /api/engine -----------------------------------------------------------------

struct Live {
    days: Days,
    since_save: u64,
}

fn path() -> String {
    std::env::var("XRPL_INDEPENDENCE_DAYS").unwrap_or_else(|_| format!("{}/independence_days.json", crate::paths::data_dir()))
}

fn live() -> &'static Mutex<Live> {
    static L: OnceLock<Mutex<Live>> = OnceLock::new();
    L.get_or_init(|| {
        let days = std::fs::read(path()).ok().and_then(|b| serde_json::from_slice::<Days>(&b).ok()).unwrap_or_default();
        Mutex::new(Live { days, since_save: 0 })
    })
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn save(days: &Days) {
    let p = path();
    let tmp = format!("{p}.tmp");
    if let Ok(b) = serde_json::to_vec_pretty(days) {
        if std::fs::write(&tmp, b).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

/// The validator signed ledger `seq`: `own` when with our own hash of it.
pub fn signed(seq: u32, own: bool) {
    let order_on = crate::native_order::mode() == crate::native_order::Mode::On;
    let peers = crate::peer_fetch::is_source();
    let date = utc_date(now_secs());
    let mut l = live().lock().unwrap_or_else(|p| p.into_inner());
    let new_day = !l.days.days.contains_key(&date);
    l.days.signed(&date, seq, own, order_on, peers);
    l.since_save += 1;
    if new_day || l.since_save >= SAVE_EVERY {
        l.since_save = 0;
        save(&l.days);
    }
}

/// The `/api/engine` section: the count, and today's and yesterday's tallies with what keeps each from counting.
pub fn json() -> Value {
    let today = utc_date(now_secs());
    let l = live().lock().unwrap_or_else(|p| p.into_inner());
    let day = |date: Option<String>| {
        date.and_then(|d| l.days.days.get(&d).map(|x| (d, x.clone()))).map(|(d, x)| {
            json!({"date": d, "signed": x.signed, "own": x.own, "borrowed": x.borrowed,
                   "coverage": (x.coverage() * 1000.0).round() / 1000.0, "order_on": x.order_on, "peers": x.peers,
                   "misses": x.misses()})
        })
    };
    json!({
        "days": l.days.streak(&today),
        "goal": 30,
        "today": day(Some(today.clone())),
        "yesterday": day(previous_date(&today)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_utc_and_step_back_across_months_and_years() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(1_792_434_496), "2026-10-19");
        assert_eq!(previous_date("2026-10-21").as_deref(), Some("2026-10-20"));
        assert_eq!(previous_date("2026-03-01").as_deref(), Some("2026-02-28"));
        assert_eq!(previous_date("2028-03-01").as_deref(), Some("2028-02-29"));
        assert_eq!(previous_date("2027-01-01").as_deref(), Some("2026-12-31"));
        assert_eq!(previous_date("not a date"), None);
    }

    fn full_day(d: &mut Days, date: &str, from: u32, own: bool, order_on: bool, peers: bool) {
        for seq in from..from + 100 {
            d.signed(date, seq, own, order_on, peers);
        }
    }

    #[test]
    fn a_day_counts_only_with_every_signature_ours_both_steps_on_and_its_ledgers_covered() {
        let mut d = Days::default();
        full_day(&mut d, "2026-11-01", 1_000, true, true, true);
        assert!(d.days["2026-11-01"].misses().is_empty());
        d.signed("2026-11-01", 1_100, false, true, true);
        assert_eq!(d.days["2026-11-01"].misses(), vec!["1 signed without our own hash".to_string()]);
        full_day(&mut d, "2026-11-02", 2_000, true, true, false);
        assert_eq!(d.days["2026-11-02"].misses(), vec!["the ledger data came from .39, not peers (Step 4)".to_string()]);
        full_day(&mut d, "2026-11-03", 3_000, true, true, true);
        d.signed("2026-11-03", 3_150, true, false, true);
        assert_eq!(d.days["2026-11-03"].misses(), vec![
            "our own order was off (Step 3)".to_string(),
            "signed 66.9% of its ledgers".to_string(),
        ]);
    }

    #[test]
    fn the_count_is_the_run_of_counted_days_up_to_yesterday() {
        let mut d = Days::default();
        for (i, date) in ["2026-11-28", "2026-11-29", "2026-11-30", "2026-12-01", "2026-12-02"].iter().enumerate() {
            full_day(&mut d, date, 10_000 * (i as u32 + 1), true, true, true);
        }
        assert_eq!(d.streak("2026-12-03"), 5);
        assert_eq!(d.streak("2026-12-02"), 4, "today is not over: it does not count yet");
        d.signed("2026-11-30", 30_100, false, true, true);
        assert_eq!(d.streak("2026-12-03"), 2, "a borrowed signature breaks the run");
        assert_eq!(d.streak("2026-12-05"), 0, "a day with nothing signed breaks it too");
    }

    #[test]
    fn the_file_keeps_its_days_and_the_oldest_go_first() {
        let mut d = Days::default();
        for i in 0..(KEEP_DAYS as u64 + 3) {
            d.signed(&utc_date(1_790_000_000 + i * 86_400), 1, true, true, true);
        }
        assert_eq!(d.days.len(), KEEP_DAYS);
        let back: Days = serde_json::from_slice(&serde_json::to_vec(&d).unwrap()).unwrap();
        assert_eq!(back, d);
    }
}
