//! Independence Step 9 (`XRPL_UNL_REFRESH=1`): the validator list refreshed while running.
//!
//! The list (vl.ripple.com, then unl.xrplf.org) was fetched and verified once, at start, so a long uptime could
//! outlive it: a validator added or dropped, a rotated signing key or the list's own expiry went unseen until a
//! restart. rippled's `ValidatorSite` fetches each site again on a timer: every 5 minutes by default, or the
//! site's `refresh_interval` (minutes, clamped to 1 minute..24 hours; ValidatorSite.cpp:34, :481-489). Here the
//! default is an hour ([`DEFAULT_REFRESH`]), since the publishers change their lists a few times a year. Each
//! refresh goes through the start's checks (`unl_fetch::refresh_verified`: the pin, both signatures, the
//! expiry, the per-validator manifests, the sequence gate); a fetch that fails or does not verify keeps the list
//! already trusted and is retried with the start's back-off. A list that differs reaches every consumer the start
//! fed: the consensus engine's trusted keys, the own-validated tally and the round recorder.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use crate::consensus_engine::ConsensusEngine;
use crate::unl_fetch::UnlEntry;

/// The refresh period when the list document names none.
pub const DEFAULT_REFRESH: Duration = Duration::from_secs(3600);

/// Seconds between the Unix and Ripple epochs (2000-01-01).
const RIPPLE_EPOCH: u64 = 946_684_800;

/// `XRPL_UNL_REFRESH=1`.
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("XRPL_UNL_REFRESH").as_deref() == Ok("1"))
}

/// The startup line (nothing when the switch is off).
pub fn log_switch() {
    if enabled() {
        eprintln!(
            "[unl-refresh] ON — the validator list fetched and verified again every {} min (or the list's refresh_interval) once it has loaded; a failed or unverified fetch keeps the current list",
            DEFAULT_REFRESH.as_secs() / 60
        );
    }
}

/// When to fetch again after a list that verified: the document's `refresh_interval` (minutes, clamped to
/// 1 minute..24 hours as rippled clamps it), else [`DEFAULT_REFRESH`]; and never later than halfway to the
/// list's expiry (Ripple-epoch seconds, 0 = none), with a one-minute floor.
pub fn next_refresh_delay(refresh_interval_min: Option<u64>, expiration: u64, now_ripple: u64) -> Duration {
    let base = refresh_interval_min.map_or(DEFAULT_REFRESH.as_secs(), |m| m.clamp(1, 24 * 60) * 60);
    let secs = if expiration > now_ripple { base.min(((expiration - now_ripple) / 2).max(60)) } else { base };
    Duration::from_secs(secs)
}

/// What a list says to trust: each master key with its signing key. Two lists with the same pairs need no
/// change downstream.
pub fn fingerprint(entries: &[UnlEntry]) -> BTreeSet<(String, Option<String>)> {
    entries.iter().map(|e| (e.public_key.to_uppercase(), e.signing_key.as_ref().map(|k| k.to_uppercase()))).collect()
}

/// The keys the consensus engine trusts for a list: each validator's signing key (proposals are signed with
/// it), or its master key when its manifest gave none — the start's rule (live_viewer's UNL loader).
pub fn engine_keys(entries: &[UnlEntry]) -> Vec<String> {
    entries.iter().map(|e| e.signing_key.clone().unwrap_or_else(|| e.public_key.clone())).collect()
}

/// Hand a new list to every consumer the start fed.
fn apply(consensus: &parking_lot::Mutex<ConsensusEngine>, entries: &[UnlEntry]) {
    crate::consensus_record::record_unl(entries);
    crate::own_validated::set_unl(entries);
    consensus.lock().unl.set_trusted(engine_keys(entries));
}

fn now_ripple() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().saturating_sub(RIPPLE_EPOCH))
        .unwrap_or(0)
}

/// Keep the list fresh, from the list the start loaded (`loaded`), for the life of the process. Returns at once
/// when the switch is off.
pub async fn run(consensus: Arc<parking_lot::Mutex<ConsensusEngine>>, loaded: Vec<UnlEntry>) {
    if !enabled() {
        return;
    }
    let mut current = fingerprint(&loaded);
    let mut delay = DEFAULT_REFRESH;
    let mut failures = 0u32;
    let mut n = 0u64;
    loop {
        tokio::time::sleep(delay).await;
        n += 1;
        match crate::unl_fetch::refresh_verified().await {
            Ok(f) => {
                failures = 0;
                let now = now_ripple();
                delay = next_refresh_delay(f.refresh_interval_min, f.list.expiration, now);
                let fp = fingerprint(&f.list.entries);
                let what = if fp == current {
                    "unchanged".to_string()
                } else {
                    let (added, dropped) = (fp.difference(&current).count(), current.difference(&fp).count());
                    apply(&consensus, &f.list.entries);
                    current = fp;
                    format!("CHANGED ({added} entries new, {dropped} gone): applied")
                };
                let expiry = if f.list.expiration > now {
                    format!("expires in {} d", (f.list.expiration - now) / 86_400)
                } else {
                    "no expiry".to_string()
                };
                eprintln!(
                    "[unl-refresh] #{n} {}: seq {}, {} validators, {expiry} (ripple time {}); {what}; next in {} min",
                    f.url,
                    f.list.sequence,
                    f.list.entries.len(),
                    f.list.expiration,
                    delay.as_secs() / 60
                );
            }
            Err(e) => {
                delay = crate::unl_fetch::unl_retry_delay(failures);
                failures = failures.saturating_add(1);
                eprintln!(
                    "[unl-refresh] #{n} failed: {e} — keeping the current list ({} validators); retry in {} s",
                    current.len(),
                    delay.as_secs()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(master: &str, signing: Option<&str>) -> UnlEntry {
        UnlEntry { public_key: master.to_string(), signing_key: signing.map(str::to_string) }
    }

    #[test]
    fn the_refresh_follows_the_list_hint_or_the_hour() {
        let now = 800_000_000;
        let far = now + 180 * 86_400;
        assert_eq!(next_refresh_delay(None, far, now), DEFAULT_REFRESH);
        assert_eq!(next_refresh_delay(None, 0, now), DEFAULT_REFRESH, "no expiry");
        assert_eq!(next_refresh_delay(Some(5), far, now), Duration::from_secs(300), "rippled's own default");
        assert_eq!(next_refresh_delay(Some(0), far, now), Duration::from_secs(60), "clamped to a minute");
        assert_eq!(next_refresh_delay(Some(100_000), far, now), Duration::from_secs(86_400), "clamped to a day");
    }

    #[test]
    fn a_list_near_its_expiry_is_fetched_by_halfway() {
        let now = 800_000_000;
        assert_eq!(next_refresh_delay(None, now + 1_800, now), Duration::from_secs(900));
        assert_eq!(next_refresh_delay(None, now + 30, now), Duration::from_secs(60), "a one-minute floor");
    }

    #[test]
    fn the_fingerprint_sees_a_rotated_key_and_ignores_order_and_case() {
        let a = vec![entry("ED01", Some("02AA")), entry("ED02", None)];
        let reordered = vec![entry("ed02", None), entry("ED01", Some("02aa"))];
        assert_eq!(fingerprint(&a), fingerprint(&reordered));
        let rotated = vec![entry("ED01", Some("03BB")), entry("ED02", None)];
        assert_ne!(fingerprint(&a), fingerprint(&rotated));
        let dropped = vec![entry("ED01", Some("02AA"))];
        assert_ne!(fingerprint(&a), fingerprint(&dropped));
    }

    #[test]
    fn the_engine_trusts_signing_keys_and_falls_back_to_the_master() {
        let a = vec![entry("ED01", Some("02AA")), entry("ED02", None)];
        assert_eq!(engine_keys(&a), vec!["02AA".to_string(), "ED02".to_string()]);
    }

    /// The captured vl.ripple.com document (sequence 85, 35 validators) carries no refresh_interval, so the
    /// hour applies, and it expires in 2027, well past the hour.
    #[test]
    fn the_live_list_has_no_refresh_hint() {
        let body: serde_json::Value = serde_json::from_str(include_str!("../tests/vectors/vl_ripple_2026-08-31.json")).unwrap();
        assert!(body.get("refresh_interval").is_none());
        let v = crate::unl_verify::verify_vl_at(&body, None, 800_000_000).unwrap();
        assert_eq!(next_refresh_delay(None, v.expiration, 800_000_000), DEFAULT_REFRESH);
        let keys = engine_keys(&v.entries);
        assert_eq!(keys.len(), 35);
        let mut unl = crate::consensus::UNL::empty();
        unl.set_trusted(keys);
        assert_eq!(unl.trusted_count(), 35);
    }
}
