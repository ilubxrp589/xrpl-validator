//! Warm restart (docs/superpowers/specs/2026-09-24-warm-restart-design.md).
//!
//! A clean stop parks ws-sync at a ledger boundary and writes a resume ticket naming the last
//! ledger that landed and verified. A warm start takes the ticket, checks that `state.rocks`'
//! bookmark (`meta:last_seq`, written in the same batch as every ledger's state) names the same
//! ledger, caps the catch-up gap, rebuilds the state root and compares it with the network's
//! account hash for that ledger. Any doubt falls back to the cold path: the process exits with
//! `FALLBACK_EXIT_CODE` and the operator tooling relaunches with a full wipe and resync.
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Exit status of a refused warm start; the deploy step relaunches cold on it.
pub const FALLBACK_EXIT_CODE: i32 = 75;

/// Largest gap (network validated ledger − bookmark) a warm start catches up. Catch-up costs about
/// 80 ms per ledger, one ledger at a time; beyond about an hour the cold download is faster.
pub const MAX_WARM_GAP: u32 = 1_000;

/// Written by a clean stop that parked ws-sync at a verified ledger.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ResumeTicket {
    pub seq: u32,
    pub account_hash: String,
    pub written_at_unix: u64,
}

/// Write the ticket atomically: a temporary file in the same directory, then a rename.
pub fn write_ticket(path: &Path, ticket: &ResumeTicket) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec(ticket).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

/// Read the ticket and delete it, so a start that dies mid-resume leaves no ticket behind.
/// `None` when there is no ticket or it does not parse.
pub fn take_ticket(path: &Path) -> Option<ResumeTicket> {
    let body = std::fs::read(path).ok()?;
    let _ = std::fs::remove_file(path);
    serde_json::from_slice(&body).ok()
}

/// `meta:last_seq`: the ledger `state.rocks` holds (u32 little-endian, written by ws-sync in the
/// same `WriteBatch` as that ledger's state changes and rewound by its rollbacks).
pub fn read_bookmark(db: &rocksdb::DB) -> Option<u32> {
    let value = db.get(b"meta:last_seq").ok()??;
    let bytes: [u8; 4] = value.as_slice().try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

/// The ticket must name the ledger the store holds.
pub fn check_ticket(ticket: Option<&ResumeTicket>, bookmark: Option<u32>) -> Result<u32, String> {
    let t = ticket.ok_or("no resume ticket (the previous process did not park at a verified ledger)")?;
    match bookmark {
        None => Err("state.rocks has no meta:last_seq bookmark".to_string()),
        Some(b) if b != t.seq => Err(format!("bookmark #{b} does not match the ticket #{}", t.seq)),
        Some(b) => Ok(b),
    }
}

/// The catch-up from `seq` to the network's validated ledger must stay within `MAX_WARM_GAP`.
pub fn check_gap(seq: u32, network_validated: Option<u32>) -> Result<u32, String> {
    let validated = network_validated.ok_or("network validated ledger unavailable")?;
    let gap = validated.saturating_sub(seq);
    if gap > MAX_WARM_GAP {
        return Err(format!(
            "state is {gap} ledgers behind the network (#{seq} vs #{validated}); over {MAX_WARM_GAP} the cold download is faster"
        ));
    }
    Ok(gap)
}

/// The rebuilt root must equal the network's account hash for `seq`, and the ticket's when given.
pub fn check_root(seq: u32, root: &str, network_hash: Option<&str>, ticket_hash: Option<&str>) -> Result<(), String> {
    let net = network_hash.ok_or_else(|| format!("network account_hash for #{seq} unavailable"))?;
    if !root.eq_ignore_ascii_case(net) {
        return Err(format!("root {} != network account_hash {} at #{seq}", short(root), short(net)));
    }
    if let Some(t) = ticket_hash {
        if !root.eq_ignore_ascii_case(t) {
            return Err(format!("root {} != ticket account_hash {} at #{seq}", short(root), short(t)));
        }
    }
    Ok(())
}

fn short(hash: &str) -> &str {
    &hash[..hash.len().min(16)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket(seq: u32, hash: &str) -> ResumeTicket {
        ResumeTicket { seq, account_hash: hash.to_string(), written_at_unix: 1_758_700_000 }
    }

    #[test]
    fn ticket_round_trips_and_is_consumed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resume_ticket.json");
        let t = ticket(107_195_123, "AB12");
        write_ticket(&path, &t).unwrap();
        assert!(!dir.path().join("resume_ticket.json.tmp").exists(), "no temporary file left behind");
        assert_eq!(take_ticket(&path), Some(t));
        assert!(!path.exists(), "taking the ticket deletes it");
        assert_eq!(take_ticket(&path), None);
    }

    #[test]
    fn garbage_ticket_is_none_and_consumed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resume_ticket.json");
        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(take_ticket(&path), None);
        assert!(!path.exists());
    }

    #[test]
    fn bookmark_reads_meta_last_seq() {
        let dir = tempfile::tempdir().unwrap();
        let db = rocksdb::DB::open_default(dir.path()).unwrap();
        assert_eq!(read_bookmark(&db), None);
        db.put(b"meta:last_seq", 107_195_123u32.to_le_bytes()).unwrap();
        assert_eq!(read_bookmark(&db), Some(107_195_123));
        db.put(b"meta:last_seq", [1u8, 2, 3]).unwrap();
        assert_eq!(read_bookmark(&db), None, "a malformed bookmark is no bookmark");
    }

    #[test]
    fn check_ticket_requires_the_bookmark_to_name_the_ticket() {
        assert!(check_ticket(None, Some(500)).unwrap_err().contains("no resume ticket"));
        assert!(check_ticket(Some(&ticket(500, "AA")), None).unwrap_err().contains("no meta:last_seq"));
        let e = check_ticket(Some(&ticket(500, "AA")), Some(499)).unwrap_err();
        assert!(e.contains("#499") && e.contains("#500"), "{e}");
        assert_eq!(check_ticket(Some(&ticket(500, "AA")), Some(500)), Ok(500));
    }

    #[test]
    fn check_gap_caps_the_catch_up() {
        assert!(check_gap(500, None).is_err());
        assert_eq!(check_gap(500, Some(500)), Ok(0));
        assert_eq!(check_gap(500, Some(1_500)), Ok(1_000));
        assert!(check_gap(500, Some(1_501)).unwrap_err().contains("1001 ledgers behind"));
        assert_eq!(check_gap(500, Some(490)), Ok(0), "a source behind the bookmark is not a gap");
    }

    #[test]
    fn check_root_requires_network_and_ticket_agreement() {
        let root = "9F".repeat(32);
        assert!(check_root(500, &root, None, None).unwrap_err().contains("unavailable"));
        assert!(check_root(500, &root, Some(&"00".repeat(32)), None).unwrap_err().contains("network"));
        assert_eq!(check_root(500, &root, Some(&root.to_lowercase()), None), Ok(()), "hex case does not matter");
        assert!(check_root(500, &root, Some(&root), Some(&"11".repeat(32))).unwrap_err().contains("ticket"));
        assert_eq!(check_root(500, &root, Some(&root), Some(&root)), Ok(()));
    }
}
