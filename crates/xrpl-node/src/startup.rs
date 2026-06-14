//! Startup state-integrity assessment (reaudit 2026-06-10 finding F5).
//!
//! The validator's crash-consistency contract — "on an unclean shutdown, wipe
//! state.rocks + markers and bulk-resync before restarting" — historically lived
//! only in operator discipline. RocksDB commits each ledger atomically, so a
//! *clean* shutdown leaves consistent state; but an OOM kill / power loss / SIGKILL
//! can leave on-disk state torn. On the next boot the in-memory hasher is rebuilt
//! from that torn state, diverges from the network account_hash, takes 3
//! consecutive strikes, and halts — a failure that looks mysterious because the
//! cause (the crash) is long past.
//!
//! This encodes the contract mechanically: a clean shutdown writes a marker and
//! boot consumes it. Marker present at boot => previous shutdown was clean. Marker
//! absent while state exists => unclean => refuse to start (NO auto-recovery, by
//! design — auto-wipe/auto-heal causes death spirals). The operator runs the
//! wipe+resync ritual, or sets `XRPL_FORCE_START=1` to resume deliberately.

/// How to proceed at boot, given on-disk state and the clean-shutdown marker.
#[derive(Debug, PartialEq, Eq)]
pub enum StartupIntegrity {
    /// No persisted state (first boot or already wiped) — safe to sync from scratch.
    FreshStart,
    /// Prior shutdown wrote the clean marker — state is consistent, safe to resume.
    CleanResume,
    /// State present but no clean marker — prior shutdown was unclean; refuse to start.
    UncleanRefuse,
    /// Unclean, but the operator forced startup via `XRPL_FORCE_START=1`.
    ForcedResume,
}

/// Pure boot decision (reaudit F5). See module docs.
pub fn assess_startup_integrity(
    rocks_exists: bool,
    clean_marker_exists: bool,
    force_start: bool,
) -> StartupIntegrity {
    if !rocks_exists {
        return StartupIntegrity::FreshStart;
    }
    if clean_marker_exists {
        return StartupIntegrity::CleanResume;
    }
    if force_start {
        StartupIntegrity::ForcedResume
    } else {
        StartupIntegrity::UncleanRefuse
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_state_is_fresh_start_regardless_of_marker_or_force() {
        assert_eq!(assess_startup_integrity(false, false, false), StartupIntegrity::FreshStart);
        assert_eq!(assess_startup_integrity(false, true, false), StartupIntegrity::FreshStart);
        assert_eq!(assess_startup_integrity(false, false, true), StartupIntegrity::FreshStart);
    }

    #[test]
    fn state_with_clean_marker_resumes() {
        assert_eq!(assess_startup_integrity(true, true, false), StartupIntegrity::CleanResume);
    }

    #[test]
    fn state_without_marker_refuses() {
        assert_eq!(assess_startup_integrity(true, false, false), StartupIntegrity::UncleanRefuse);
    }

    #[test]
    fn unclean_with_force_resumes() {
        assert_eq!(assess_startup_integrity(true, false, true), StartupIntegrity::ForcedResume);
    }

    #[test]
    fn clean_marker_takes_precedence_over_force() {
        // A clean marker means consistent state; force is irrelevant.
        assert_eq!(assess_startup_integrity(true, true, true), StartupIntegrity::CleanResume);
    }
}
