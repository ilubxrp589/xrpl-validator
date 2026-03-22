//! Close time negotiation between validators.

use super::unl::UNL;

/// Round a close time to the nearest resolution.
pub fn round_close_time(time: u32, resolution: u32) -> u32 {
    if resolution == 0 {
        return time;
    }
    ((time + resolution / 2) / resolution) * resolution
}

/// Get the effective close time resolution for a given consensus round.
/// Resolution decreases over rounds to force agreement.
pub fn effective_resolution(round: u32) -> u32 {
    match round {
        1 => 30,
        2 => 20,
        3 => 10,
        _ => 1,
    }
}

/// Negotiate a close time from peer proposals.
///
/// Returns (agreed_time, no_consensus_flag).
/// The flag is true if no majority was reached and we used our own time.
pub fn negotiate_close_time(
    our_time: u32,
    peer_times: &[(String, u32)],
    unl: &UNL,
    round: u32,
) -> (u32, bool) {
    let resolution = effective_resolution(round);

    // Round all times
    let our_rounded = round_close_time(our_time, resolution);

    // Count frequency of each rounded time among trusted validators
    let mut counts: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
    for (key, time) in peer_times {
        if unl.is_trusted(key) {
            let rounded = round_close_time(*time, resolution);
            *counts.entry(rounded).or_default() += 1;
        }
    }

    // Include our own vote
    *counts.entry(our_rounded).or_default() += 1;

    let total = unl.trusted_count().max(1);

    // Find the mode (most common rounded time)
    if let Some((&mode_time, &mode_count)) = counts.iter().max_by_key(|(_, &c)| c) {
        let pct = mode_count as f64 / total as f64;
        if pct >= 0.50 {
            return (mode_time, false);
        }
    }

    // No majority — use our own time, set flag
    (our_rounded, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_time() {
        assert_eq!(round_close_time(100, 30), 90);
        assert_eq!(round_close_time(115, 30), 120);
        assert_eq!(round_close_time(15, 10), 20);
        assert_eq!(round_close_time(14, 10), 10);
        assert_eq!(round_close_time(5, 1), 5);
    }

    #[test]
    fn resolutions() {
        assert_eq!(effective_resolution(1), 30);
        assert_eq!(effective_resolution(2), 20);
        assert_eq!(effective_resolution(3), 10);
        assert_eq!(effective_resolution(4), 1);
        assert_eq!(effective_resolution(99), 1);
    }

    #[test]
    fn negotiate_majority_agrees() {
        let unl = UNL::from_keys(&["V1".into(), "V2".into(), "V3".into()]);
        let peers = vec![
            ("V1".into(), 100u32),
            ("V2".into(), 101),
            ("V3".into(), 102),
        ];
        // Round 1, resolution 30: all round to 90 or 120 depending on value
        // 100→90, 101→90, 102→90 (with res=30, (100+15)/30*30=90)
        let (time, no_consensus) = negotiate_close_time(100, &peers, &unl, 1);
        assert!(!no_consensus);
        assert_eq!(time, 90); // all agree on 90
    }

    #[test]
    fn negotiate_no_majority() {
        let unl = UNL::from_keys(&["V1".into(), "V2".into(), "V3".into()]);
        let peers = vec![
            ("V1".into(), 100u32),
            ("V2".into(), 200),
            ("V3".into(), 300),
        ];
        // With resolution 1 (round 4+), all different
        let (time, no_consensus) = negotiate_close_time(400, &peers, &unl, 4);
        assert!(no_consensus);
        assert_eq!(time, 400); // our own time
    }

    #[test]
    fn ignores_untrusted() {
        let unl = UNL::from_keys(&["V1".into(), "V2".into()]);
        let peers = vec![
            ("V1".into(), 100u32),
            ("V2".into(), 100),
            ("UNTRUSTED".into(), 999),
        ];
        let (time, no_consensus) = negotiate_close_time(100, &peers, &unl, 3);
        assert!(!no_consensus);
        // V1 and V2 agree, untrusted ignored
        assert_eq!(time, round_close_time(100, 10));
    }
}
