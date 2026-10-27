//! Independence Step 8's switch: a cold start from peers, not from .39.
//!
//! `peer_state_sync` fetches a ledger's whole account state from mainnet peers, every node proven to the hash its
//! parent names, and recomputes the root from the leaves it wrote ([`crate::peer_state`]). [`convert`] turns that
//! run's directory into the store live_viewer resumes from, exactly as a clean stop leaves it:
//! - `state.rocks` holding every leaf (key → object) and the bookmark `meta:last_seq`;
//! - the resume ticket naming that ledger and its account hash;
//! - `clean_shutdown.marker`, and `sync_complete.marker` with the object count.
//!
//! A warm start then rebuilds the root from the store and holds it against the network's account hash for that
//! ledger (from peers too, with `XRPL_LEDGER_SOURCE=peers`), and catches up from peers. The run is checked once more
//! here before anything is written: the header hashes to the ledger hash and names the account hash, and the leaves
//! recompute to it with none repeated.

use std::path::{Path, PathBuf};

use serde_json::Value;
use xrpl_ledger::ledger::header::LedgerHeader;

/// Objects written per RocksDB batch.
pub const BATCH: usize = 50_000;

/// Where the converted store goes.
#[derive(Debug, Clone)]
pub struct Targets {
    pub rocks: PathBuf,
    pub ticket: PathBuf,
    pub clean_marker: PathBuf,
    pub sync_marker: PathBuf,
}

impl Targets {
    /// live_viewer's own paths (`XRPL_DATA_DIR` and friends, [`crate::paths`]).
    pub fn from_env() -> Self {
        Targets {
            rocks: crate::paths::state_rocks_path().into(),
            ticket: crate::paths::resume_ticket_path().into(),
            clean_marker: crate::paths::clean_shutdown_marker_path().into(),
            sync_marker: crate::paths::sync_complete_marker_path().into(),
        }
    }
}

/// A store ready for a warm start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColdReady {
    pub seq: u32,
    pub account_hash: [u8; 32],
    pub objects: u64,
}

fn h32(v: &Value, what: &str) -> Result<[u8; 32], String> {
    v.as_str()
        .and_then(|s| hex::decode(s).ok())
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| format!("proven.json: no {what}"))
}

/// `peer_state_sync`'s run in `dir` (proven.json, header.bin, leaves.bin) into the store at `to`. Refuses a store
/// that already holds anything.
pub fn convert(dir: &Path, to: &Targets) -> Result<ColdReady, String> {
    let proven: Value = serde_json::from_slice(&std::fs::read(dir.join("proven.json")).map_err(|e| format!("proven.json: {e}"))?)
        .map_err(|e| format!("proven.json: {e}"))?;
    let seq = proven["seq"].as_u64().ok_or("proven.json: no seq")? as u32;
    let ledger_hash = h32(&proven["ledger_hash"], "ledger_hash")?;
    let account_hash = h32(&proven["account_hash"], "account_hash")?;
    let objects = proven["objects"].as_u64().ok_or("proven.json: no objects")?;
    let header = LedgerHeader::deserialize(&std::fs::read(dir.join("header.bin")).map_err(|e| format!("header.bin: {e}"))?)
        .map_err(|e| format!("header.bin: {e}"))?;
    if header.hash().0 != ledger_hash || header.sequence != seq || header.account_hash.0 != account_hash {
        return Err(format!("header.bin is not ledger #{seq} {} with that account hash", &hex::encode_upper(ledger_hash)[..16]));
    }
    let leaves_path = dir.join("leaves.bin");
    let r = crate::peer_state::recheck(&leaves_path, objects).map_err(|e| format!("leaves.bin: {e}"))?;
    if r.root != account_hash || r.leaves != objects || r.repeated != 0 {
        return Err(format!(
            "leaves.bin: {} leaves ({} repeated) make root {}, not the account hash",
            r.leaves,
            r.repeated,
            &hex::encode_upper(r.root)[..16]
        ));
    }
    if to.rocks.read_dir().map(|mut d| d.next().is_some()).unwrap_or(false) {
        return Err(format!("{} already holds a store: move it aside first", to.rocks.display()));
    }
    let mut opts = rocksdb::Options::default();
    opts.create_if_missing(true);
    let db = rocksdb::DB::open(&opts, &to.rocks).map_err(|e| format!("open {}: {e}", to.rocks.display()))?;
    let mut batch = rocksdb::WriteBatch::default();
    let mut err = None;
    crate::peer_state::read_leaves(&leaves_path, |k, item| {
        if err.is_some() {
            return;
        }
        batch.put(k, item);
        if batch.len() >= BATCH {
            if let Err(e) = db.write(std::mem::take(&mut batch)) {
                err = Some(e.to_string());
            }
        }
    })
    .map_err(|e| format!("leaves.bin: {e}"))?;
    if let Some(e) = err {
        return Err(format!("write: {e}"));
    }
    batch.put(b"meta:last_seq", seq.to_le_bytes());
    db.write(batch).map_err(|e| format!("write: {e}"))?;
    db.flush().map_err(|e| format!("flush: {e}"))?;
    drop(db);
    let ticket = crate::resume::ResumeTicket {
        seq,
        account_hash: hex::encode_upper(account_hash),
        written_at_unix: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()),
    };
    crate::resume::write_ticket(&to.ticket, &ticket).map_err(|e| format!("ticket: {e}"))?;
    std::fs::write(&to.clean_marker, "clean").map_err(|e| format!("clean marker: {e}"))?;
    std::fs::write(&to.sync_marker, objects.to_string()).map_err(|e| format!("sync marker: {e}"))?;
    Ok(ColdReady { seq, account_hash, objects })
}

#[cfg(test)]
mod tests {
    use super::*;
    use xrpl_core::types::Hash256;

    /// A peer_state_sync run directory for `leaves` at ledger `seq`; its ledger hash.
    fn run_dir(dir: &Path, leaves: &[([u8; 32], Vec<u8>)], seq: u32) -> [u8; 32] {
        let mut f = crate::peer_state::LeafFile::open(&dir.join("leaves.bin"), 0).unwrap();
        for (k, item) in leaves {
            f.append(k, item).unwrap();
        }
        f.sync().unwrap();
        let mut sorted: Vec<([u8; 32], [u8; 32])> = leaves.iter().map(|(k, i)| (*k, crate::peer_state::leaf_hash(k, i))).collect();
        sorted.sort();
        let root = crate::peer_state::state_root(&sorted);
        let header = LedgerHeader {
            sequence: seq,
            total_coins: 99_985_000_000_000_000,
            parent_hash: Hash256([1; 32]),
            transaction_hash: Hash256([0; 32]),
            account_hash: Hash256(root),
            parent_close_time: 814_000_000,
            close_time: 814_000_004,
            close_time_resolution: 10,
            close_flags: 0,
        };
        std::fs::write(dir.join("header.bin"), header.serialize()).unwrap();
        let proven = serde_json::json!({"seq": seq, "ledger_hash": hex::encode_upper(header.hash().0), "account_hash": hex::encode_upper(root), "objects": leaves.len()});
        std::fs::write(dir.join("proven.json"), proven.to_string()).unwrap();
        header.hash().0
    }

    fn targets(dir: &Path) -> Targets {
        Targets {
            rocks: dir.join("state.rocks"),
            ticket: dir.join("resume_ticket.json"),
            clean_marker: dir.join("clean_shutdown.marker"),
            sync_marker: dir.join("sync_complete.marker"),
        }
    }

    #[test]
    fn a_leaf_file_becomes_a_store_that_resumes() {
        let (run, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let leaves = crate::peer_state::tests::leaves(1_000);
        run_dir(run.path(), &leaves, 107_950_000);
        let to = targets(data.path());
        let ready = convert(run.path(), &to).unwrap();
        assert_eq!((ready.seq, ready.objects), (107_950_000, leaves.len() as u64));
        let db = rocksdb::DB::open_default(&to.rocks).unwrap();
        assert_eq!(crate::resume::read_bookmark(&db), Some(107_950_000));
        for (k, item) in leaves.iter().step_by(97) {
            assert_eq!(db.get(k).unwrap().as_deref(), Some(item.as_slice()));
        }
        let ticket = crate::resume::take_ticket(&to.ticket);
        assert_eq!(crate::resume::check_ticket(ticket.as_ref(), crate::resume::read_bookmark(&db)), Ok(107_950_000));
        assert_eq!(ticket.unwrap().account_hash, hex::encode_upper(ready.account_hash));
        assert!(to.clean_marker.exists() && to.sync_marker.exists());
        drop(db);
        assert!(convert(run.path(), &to).unwrap_err().contains("already holds a store"), "never over a store");
    }

    #[test]
    fn a_run_whose_leaves_do_not_make_the_root_is_refused() {
        let (run, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut leaves = crate::peer_state::tests::leaves(200);
        run_dir(run.path(), &leaves, 5);
        leaves[3].1[0] ^= 1;
        let mut f = crate::peer_state::LeafFile::open(&run.path().join("leaves.bin"), 0).unwrap();
        for (k, item) in &leaves {
            f.append(k, item).unwrap();
        }
        f.sync().unwrap();
        let e = convert(run.path(), &targets(data.path())).unwrap_err();
        assert!(e.contains("not the account hash"), "{e}");
        assert!(!data.path().join("state.rocks").exists(), "nothing written");
    }
}
