//! A ledger's close time (independence task 5.3), as rippled 3.4.1 sets it:
//! - include/xrpl/ledger/LedgerTiming.h: the resolutions, `getNextLedgerTimeResolution`, `roundCloseTime`,
//!   `effCloseTime`;
//! - src/libxrpl/ledger/Ledger.cpp: a new ledger's resolution comes from its parent's resolution, whether the
//!   parent's close time was agreed (`getCloseAgree`), and the new sequence; `setAccepted` writes the close
//!   time, the resolution and `sLCF_NoConsensusTime`;
//! - src/xrpld/app/consensus/RCLConsensus.cpp, `doAccept`: an agreed time goes through `effCloseTime`; the zero
//!   time, "agree to disagree", gives the parent's close time plus one second and the flag;
//! - include/xrpl/consensus/Consensus.h (`updateOurPositions`) and ConsensusParms.h: the close-time vote.
//!
//! Times are NetClock seconds, counted from the Ripple epoch (2000-01-01), as a header carries them.

use std::collections::BTreeMap;

/// The close-time resolutions in seconds, finest first (`kLedgerPossibleTimeResolutions`).
pub const RESOLUTIONS: [u32; 6] = [10, 20, 30, 60, 90, 120];
/// `kLedgerDefaultTimeResolution`.
pub const DEFAULT_RESOLUTION: u32 = 30;
/// `kLedgerGenesisTimeResolution`.
pub const GENESIS_RESOLUTION: u32 = 10;
/// After an agreed close time, the resolution gets one step finer only on a sequence divisible by this
/// (`kIncreaseLedgerTimeResolutionEvery`).
pub const INCREASE_EVERY: u32 = 8;
/// After a close time not agreed, it gets one step coarser on a sequence divisible by this
/// (`kDecreaseLedgerTimeResolutionEvery`): every ledger.
pub const DECREASE_EVERY: u32 = 1;
/// `sLCF_NoConsensusTime`: the close flag of a ledger whose validators agreed to disagree on the close time.
pub const NO_CONSENSUS_TIME: u8 = 0x01;
/// `avCtConsensusPct`: the share of the participants that must vote for one close time to agree on it.
pub const CLOSE_TIME_CONSENSUS_PCT: usize = 75;

/// `getNextLedgerTimeResolution`: ledger `seq`'s resolution, from its parent's and whether the parent's close
/// time was agreed. A resolution not in [`RESOLUTIONS`] stays as it is.
pub fn next_resolution(previous: u32, previous_agreed: bool, seq: u32) -> u32 {
    let Some(i) = RESOLUTIONS.iter().position(|&r| r == previous) else {
        return previous;
    };
    if !previous_agreed && seq % DECREASE_EVERY == 0 {
        if let Some(&coarser) = RESOLUTIONS.get(i + 1) {
            return coarser;
        }
    }
    if previous_agreed && seq % INCREASE_EVERY == 0 && i > 0 {
        return RESOLUTIONS[i - 1];
    }
    previous
}

/// `getCloseAgree`: whether a header's close time was agreed.
pub fn close_agreed(close_flags: u8) -> bool {
    close_flags & NO_CONSENSUS_TIME == 0
}

/// `roundCloseTime`: to the nearest multiple of `resolution`, a half rounding up. Zero, no time, stays zero.
pub fn round_close_time(time: u32, resolution: u32) -> u32 {
    if time == 0 || resolution == 0 {
        return time;
    }
    let t = time + resolution / 2;
    t - t % resolution
}

/// `effCloseTime`: the rounded time, but always at least a second after the prior close time. Zero stays zero.
pub fn effective_close_time(time: u32, resolution: u32, prior: u32) -> u32 {
    if time == 0 {
        return 0;
    }
    round_close_time(time, resolution).max(prior + 1)
}

/// `doAccept` with `Ledger::setAccepted`: the header's close time and close flags, from the close time the round
/// agreed on (0 when the validators agreed to disagree), the ledger's resolution and its parent's close time.
pub fn accepted_close(agreed: u32, resolution: u32, parent_close: u32) -> (u32, u8) {
    if agreed == 0 {
        (parent_close + 1, NO_CONSENSUS_TIME)
    } else {
        (effective_close_time(agreed, resolution, parent_close), 0)
    }
}

/// `participantsNeeded` (Consensus.h): how many of `participants` reach `percent`, rounded, and at least one.
pub fn participants_needed(participants: usize, percent: usize) -> usize {
    ((participants * percent + percent / 2) / 100).max(1)
}

/// The avalanche states of a vote (`ConsensusParms::AvalancheState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Avalanche {
    Init,
    Mid,
    Late,
    Stuck,
}

impl Avalanche {
    /// `ConsensusParms::avalancheCutoffs`: the share of the previous round's time after which the state starts,
    /// the share of votes it needs, and the state after it.
    pub const fn cutoff(self) -> (u32, usize, Avalanche) {
        match self {
            Avalanche::Init => (0, 50, Avalanche::Mid),
            Avalanche::Mid => (50, 65, Avalanche::Late),
            Avalanche::Late => (85, 70, Avalanche::Stuck),
            Avalanche::Stuck => (200, 95, Avalanche::Stuck),
        }
    }
}

/// `getNeededWeight` as the close-time vote calls it, with no rounds: the share of votes needed now, and the
/// state to move to once `percent_time` (the round's time as a share of the previous round's) reaches the next.
pub fn needed_weight(state: Avalanche, percent_time: u32) -> (usize, Option<Avalanche>) {
    let (_, pct, next) = state.cutoff();
    if next != state {
        let (next_time, next_pct, _) = next.cutoff();
        if percent_time >= next_time {
            return (next_pct, Some(next));
        }
    }
    (pct, None)
}

/// The close-time part of `Consensus::updateOurPositions`, over counted votes. `votes` holds each close time
/// already rounded to the resolution (`asCloseTime`) with how many voted for it, and `participants` how many
/// voted. Walking from the latest time, one with at least the needed weight becomes our position, and the
/// count to beat; so among times tied for the most votes, the earliest wins. One with
/// [`CLOSE_TIME_CONSENSUS_PCT`] is consensus. Returns the time for our position (0 when none qualifies) and
/// whether the round has close-time consensus. A round that agrees on 0 agreed to disagree.
pub fn close_time_vote(votes: &BTreeMap<u32, usize>, participants: usize, needed_weight: usize) -> (u32, bool) {
    let mut thresh_vote = participants_needed(participants, needed_weight);
    let thresh_consensus = participants_needed(participants, CLOSE_TIME_CONSENSUS_PCT);
    let (mut time, mut consensus) = (0, false);
    for (&t, &v) in votes.iter().rev() {
        if v >= thresh_vote {
            time = t;
            thresh_vote = v;
            if thresh_vote >= thresh_consensus {
                consensus = true;
            }
        }
    }
    (time, consensus)
}

/// A round's whole close-time vote: the fresh trusted proposals' close times, and our position's, which votes
/// only when we propose. Every time is rounded to the resolution first. With no peers, our own time is the
/// consensus.
pub fn vote_close_time(
    peer_times: &[u32],
    our_position: u32,
    proposing: bool,
    resolution: u32,
    needed_weight: usize,
) -> (u32, bool) {
    let ours = round_close_time(our_position, resolution);
    if peer_times.is_empty() {
        return (ours, true);
    }
    let mut votes = BTreeMap::new();
    for &t in peer_times {
        *votes.entry(round_close_time(t, resolution)).or_insert(0) += 1;
    }
    let mut participants = peer_times.len();
    if proposing {
        *votes.entry(ours).or_insert(0) += 1;
        participants += 1;
    }
    close_time_vote(&votes, participants, needed_weight)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_resolution_steps_finer_every_eighth_agreed_ledger_and_coarser_after_disagreement() {
        assert_eq!(next_resolution(20, true, 107_596_008), 10, "agreed, on a multiple of 8: finer");
        assert_eq!(next_resolution(20, true, 107_596_009), 20, "agreed, off a multiple of 8: as it was");
        assert_eq!(next_resolution(10, true, 107_596_008), 10, "already the finest");
        assert_eq!(next_resolution(10, false, 107_596_009), 20, "not agreed: coarser, on any sequence");
        assert_eq!(next_resolution(120, false, 7), 120, "already the coarsest");
        assert_eq!(next_resolution(15, false, 8), 15, "an unknown resolution stays");
    }

    #[test]
    fn close_times_round_to_the_nearest_step_with_halves_up() {
        assert_eq!(round_close_time(100, 30), 90);
        assert_eq!(round_close_time(105, 30), 120);
        assert_eq!(round_close_time(842_441_334, 10), 842_441_330);
        assert_eq!(round_close_time(842_441_335, 10), 842_441_340);
        assert_eq!(round_close_time(0, 10), 0, "no time stays no time");
    }

    #[test]
    fn the_effective_close_time_is_after_the_parent() {
        assert_eq!(effective_close_time(842_441_334, 10, 842_441_320), 842_441_330);
        assert_eq!(effective_close_time(842_441_334, 10, 842_441_330), 842_441_331, "the rounded time is not later");
        assert_eq!(effective_close_time(0, 10, 842_441_330), 0);
    }

    #[test]
    fn agreeing_to_disagree_gives_the_parent_plus_a_second_and_the_flag() {
        assert_eq!(accepted_close(0, 10, 842_441_330), (842_441_331, NO_CONSENSUS_TIME));
        assert_eq!(accepted_close(842_441_344, 10, 842_441_330), (842_441_340, 0));
        assert!(close_agreed(0) && !close_agreed(NO_CONSENSUS_TIME));
    }

    #[test]
    fn participants_needed_rounds_and_needs_at_least_one() {
        assert_eq!(participants_needed(5, 70), 3, "Consensus.h's own example");
        assert_eq!(participants_needed(35, 75), 26);
        assert_eq!(participants_needed(35, 50), 17);
        assert_eq!(participants_needed(0, 75), 1);
    }

    #[test]
    fn the_needed_weight_climbs_through_the_avalanche() {
        assert_eq!(needed_weight(Avalanche::Init, 0), (50, None));
        assert_eq!(needed_weight(Avalanche::Init, 60), (65, Some(Avalanche::Mid)));
        assert_eq!(needed_weight(Avalanche::Mid, 90), (70, Some(Avalanche::Late)));
        assert_eq!(needed_weight(Avalanche::Late, 150), (70, None));
        assert_eq!(needed_weight(Avalanche::Late, 200), (95, Some(Avalanche::Stuck)));
        assert_eq!(needed_weight(Avalanche::Stuck, 500), (95, None));
    }

    #[test]
    fn the_vote_takes_a_time_with_enough_weight_and_the_earliest_of_a_tie() {
        let votes = |v: &[(u32, usize)]| v.iter().copied().collect::<BTreeMap<_, _>>();
        // 35 participants: 17 to move our position at 50%, 26 for consensus.
        assert_eq!(close_time_vote(&votes(&[(100, 20), (110, 15)]), 35, 50), (100, false));
        assert_eq!(close_time_vote(&votes(&[(100, 30), (110, 5)]), 35, 50), (100, true));
        assert_eq!(close_time_vote(&votes(&[(100, 18), (110, 18)]), 35, 50), (100, false), "the earliest of a tie");
        assert_eq!(close_time_vote(&votes(&[(100, 10), (110, 10), (120, 15)]), 35, 50), (0, false), "none qualifies");
        assert_eq!(close_time_vote(&votes(&[(0, 30), (100, 5)]), 35, 50), (0, true), "agreeing to disagree");
    }

    #[test]
    fn a_round_votes_rounded_times_and_counts_ours_only_when_proposing() {
        // Three peers whose times all round to ...330; ours rounds to ...350. Proposing, 3 of 4 is 75%.
        let agreeing = [842_441_334, 842_441_333, 842_441_332];
        assert_eq!(vote_close_time(&agreeing, 842_441_349, true, 10, 50), (842_441_330, true));
        // Peers split ...330 / ...340 and ours ...350: one vote each of 3, the earliest moves our position, and
        // 1 of 3 is no consensus (2 needed).
        let split = [842_441_334, 842_441_336];
        assert_eq!(vote_close_time(&split, 842_441_349, true, 10, 50), (842_441_330, false));
        // Not proposing, ours does not vote: 2 participants, and 1 vote is enough for both thresholds.
        assert_eq!(vote_close_time(&split, 842_441_349, false, 10, 50), (842_441_330, true));
        assert_eq!(vote_close_time(&[], 842_441_349, true, 10, 50), (842_441_350, true), "no peers: our own time");
    }

    /// Independence task 5.3's check against mainnet headers. For every ledger N whose parent N−1 is also in the
    /// store: N's parent_hash is N−1's hash and its parent_close_time N−1's close time; N's resolution comes out
    /// of [`next_resolution`] from N−1's; and N's close time is the parent's plus one second when its flag says
    /// the validators agreed to disagree, else a multiple of its resolution after the parent's, or the parent's
    /// plus one second where `effCloseTime`'s floor took over. The votes behind a close time are not in a header,
    /// so the vote itself waits for live proposals.
    ///
    ///   XRPL_HEADER_DIRS=<dir>:<dir>... cargo test -p xrpl-ledger --lib close_time_against -- --ignored --nocapture
    /// (each dir holds `ledger` answers with ledger_data: fetch_meta_ledgers.py's headers/, --range's, or bin/).
    #[test]
    #[ignore]
    fn close_time_against_mainnet() {
        use crate::ledger::header::LedgerHeader;
        let dirs = std::env::var("XRPL_HEADER_DIRS").expect("XRPL_HEADER_DIRS: dirs of `ledger` answers, ':'-separated");
        let mut headers = BTreeMap::<u32, LedgerHeader>::new();
        for dir in dirs.split(':').filter(|d| !d.is_empty()) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.extension().is_none_or(|x| x != "json") {
                    continue;
                }
                let doc: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                let doc = doc.get("result").unwrap_or(&doc);
                let Some(data) = doc["ledger"]["ledger_data"].as_str() else { continue };
                let h = LedgerHeader::deserialize(&hex::decode(data).unwrap()).unwrap();
                headers.insert(h.sequence, h);
            }
        }
        let (mut pairs, mut rounded, mut floored, mut no_consensus, mut changes) = (0usize, 0usize, 0usize, 0usize, 0usize);
        let mut resolutions = BTreeMap::<(u32, u8), usize>::new();
        let mut misses = Vec::new();
        for (&seq, h) in &headers {
            *resolutions.entry((u32::from(h.close_time_resolution), h.close_flags)).or_default() += 1;
            let Some(parent) = headers.get(&(seq - 1)) else { continue };
            pairs += 1;
            let mut miss = |what: String| misses.push(format!("#{seq}: {what}"));
            if h.parent_hash != parent.hash() {
                miss("parent_hash is not the parent's hash".into());
            }
            if h.parent_close_time != parent.close_time {
                miss(format!("parent_close_time {} != the parent's close time {}", h.parent_close_time, parent.close_time));
            }
            let res = u32::from(h.close_time_resolution);
            let want = next_resolution(u32::from(parent.close_time_resolution), close_agreed(parent.close_flags), seq);
            if res != want {
                miss(format!("resolution {res}, ours {want} (parent {} flags {})", parent.close_time_resolution, parent.close_flags));
            }
            if res != u32::from(parent.close_time_resolution) {
                changes += 1;
            }
            if !close_agreed(h.close_flags) {
                no_consensus += 1;
                if h.close_time != parent.close_time + 1 {
                    miss(format!("no consensus, close time {} != parent + 1 ({})", h.close_time, parent.close_time + 1));
                }
            } else if h.close_time == parent.close_time + 1 && h.close_time % res != 0 {
                floored += 1;
            } else if h.close_time % res == 0 && h.close_time > parent.close_time {
                rounded += 1;
            } else {
                miss(format!("close time {} is neither a multiple of {res} after {} nor its + 1", h.close_time, parent.close_time));
            }
        }
        eprintln!(
            "close time: {} headers, {pairs} parent/child pairs; (resolution, flags) seen {resolutions:?}; {changes} \
             resolution changes, {no_consensus} without consensus; close times {rounded} rounded, {floored} at the \
             parent + 1 floor; {} misses",
            headers.len(),
            misses.len()
        );
        for m in misses.iter().take(30) {
            eprintln!("  {m}");
        }
        assert!(pairs > 0, "no parent/child pairs in {dirs}");
        assert!(misses.is_empty(), "{} headers miss our close-time rules", misses.len());
    }
}
