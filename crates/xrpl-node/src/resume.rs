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
use crate::state_hash::StateHashComputer;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

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

/// The network facts a warm start is checked against.
pub trait Network {
    fn validated_seq(&self) -> impl Future<Output = Option<u32>> + Send;
    fn account_hash(&self, seq: u32) -> impl Future<Output = Option<String>> + Send;
}

/// The RPC source ws-sync uses (`XRPL_RPC_URL`, with the client's failover).
pub struct RpcNetwork {
    rpc: crate::rippled_client::RippledClient,
}

impl RpcNetwork {
    pub fn new() -> Self {
        Self { rpc: crate::rippled_client::RippledClient::new() }
    }
}

impl Default for RpcNetwork {
    fn default() -> Self {
        Self::new()
    }
}

impl Network for RpcNetwork {
    async fn validated_seq(&self) -> Option<u32> {
        let body = self.rpc.call("ledger", serde_json::json!({"ledger_index": "validated"})).await.ok()?;
        body["result"]["ledger_index"]
            .as_u64()
            .map(|v| v as u32)
            .or_else(|| body["result"]["ledger"]["ledger_index"].as_str().and_then(|s| s.parse().ok()))
    }

    async fn account_hash(&self, seq: u32) -> Option<String> {
        crate::ws_sync::fetch_account_hash(&self.rpc, seq).await
    }
}

/// What a successful check verified.
#[derive(Debug, Clone, PartialEq)]
pub struct Verified {
    pub seq: u32,
    pub root: String,
    pub gap: u32,
    pub entries: u64,
    pub build_secs: f64,
}

/// Build the ws-sync hasher from `state.rocks` (installing it in `hash_comp`) and take its root.
/// Returns the root (hex), the entry count and the build time in seconds.
pub async fn rebuild_root(db: &Arc<rocksdb::DB>, hash_comp: &Arc<StateHashComputer>) -> Result<(String, u64, f64), String> {
    let (d, hc) = (db.clone(), hash_comp.clone());
    let t0 = std::time::Instant::now();
    let root = tokio::task::spawn_blocking(move || hc.update_and_hash(&d, &[]))
        .await
        .map_err(|e| format!("hasher build task failed: {e}"))?
        .ok_or("hasher produced no root")?;
    let entries = hash_comp.hasher_entries().unwrap_or(0) as u64;
    Ok((hex::encode(root.0), entries, t0.elapsed().as_secs_f64()))
}

/// The network's account hash for `seq`, three attempts two seconds apart.
async fn account_hash_with_retries<N: Network>(net: &N, seq: u32) -> Option<String> {
    for attempt in 0..3 {
        if let Some(h) = net.account_hash(seq).await {
            return Some(h);
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
    None
}

/// The rehearsal check (no ticket): the bookmark's rebuilt root against the network's account
/// hash. The gap is reported, not capped.
pub async fn verify_state_at_bookmark<N: Network>(net: &N, db: &Arc<rocksdb::DB>, hash_comp: &Arc<StateHashComputer>) -> Result<Verified, String> {
    let seq = read_bookmark(db).ok_or("state.rocks has no meta:last_seq bookmark")?;
    let gap = net.validated_seq().await.map(|v| v.saturating_sub(seq)).unwrap_or(0);
    let (root, entries, build_secs) = rebuild_root(db, hash_comp).await?;
    check_root(seq, &root, account_hash_with_retries(net, seq).await.as_deref(), None)?;
    Ok(Verified { seq, root, gap, entries, build_secs })
}

/// The warm start (spec §2, steps 1-6; step 0 — the incremental syncer — is the caller's).
/// Any `Err` is a fallback reason. On `Ok` the hasher in `hash_comp` holds the verified state, its
/// wallet count is scanned (as both cold branches do before ws-sync adjusts it), and ws-sync may
/// start with `last_synced = seq`.
pub async fn run_warm_resume<N: Network>(
    net: &N,
    db: &Arc<rocksdb::DB>,
    hash_comp: &Arc<StateHashComputer>,
    clean: bool,
    ticket_path: &Path,
) -> Result<Verified, String> {
    let ticket = take_ticket(ticket_path);
    if !clean {
        return Err("the F5 integrity check did not report a clean prior shutdown".to_string());
    }
    let seq = check_ticket(ticket.as_ref(), read_bookmark(db))?;
    let gap = check_gap(seq, net.validated_seq().await)?;
    // The wallet-count scan runs beside the (longer) hasher rebuild: no added restart time, and
    // ws-sync never adjusts an unscanned counter (a first deletion would wrap it to 2^64 - 1).
    let (wdb, whc) = (db.clone(), hash_comp.clone());
    let wallets = tokio::task::spawn_blocking(move || whc.scan_wallet_count(wdb.as_ref()));
    let (root, entries, build_secs) = rebuild_root(db, hash_comp).await?;
    wallets.await.map_err(|e| format!("wallet-count scan task failed: {e}"))?;
    let network_hash = account_hash_with_retries(net, seq).await;
    check_root(seq, &root, network_hash.as_deref(), ticket.as_ref().map(|t| t.account_hash.as_str()))?;
    Ok(Verified { seq, root, gap, entries, build_secs })
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

    use crate::state_hash::StateHashComputer;
    use std::sync::Arc;

    struct FakeNet {
        validated: Option<u32>,
        hash: Option<String>,
    }
    impl Network for FakeNet {
        async fn validated_seq(&self) -> Option<u32> {
            self.validated
        }
        async fn account_hash(&self, _seq: u32) -> Option<String> {
            self.hash.clone()
        }
    }

    /// A tiny state.rocks at ledger #500 and its true root (computed by an independent hasher).
    fn store_at_500() -> (tempfile::TempDir, Arc<rocksdb::DB>, String) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(rocksdb::DB::open_default(dir.path().join("state.rocks")).unwrap());
        for b in [0x11u8, 0x5A, 0xC3] {
            db.put([b; 32], vec![b; 40]).unwrap();
        }
        db.put(b"meta:last_seq", 500u32.to_le_bytes()).unwrap();
        let root = StateHashComputer::new().update_and_hash(&db, &[]).map(|r| hex::encode(r.0)).unwrap();
        (dir, db, root)
    }

    fn net(validated: u32, hash: &str) -> FakeNet {
        FakeNet { validated: Some(validated), hash: Some(hash.to_string()) }
    }

    #[tokio::test]
    async fn warm_resume_accepts_the_verified_store() {
        let (dir, db, root) = store_at_500();
        let path = dir.path().join("resume_ticket.json");
        write_ticket(&path, &ticket(500, &root)).unwrap();
        let hc = Arc::new(StateHashComputer::new());
        let v = run_warm_resume(&net(510, &root), &db, &hc, true, &path).await.unwrap();
        assert_eq!((v.seq, v.gap, v.entries), (500, 10, 3));
        assert!(v.root.eq_ignore_ascii_case(&root));
        assert!(!path.exists(), "the ticket is consumed");
        assert_eq!(hc.hasher_entries(), Some(3), "the verified hasher is installed for ws-sync");
    }

    #[tokio::test]
    async fn unclean_start_falls_back_and_consumes_the_ticket() {
        let (dir, db, root) = store_at_500();
        let path = dir.path().join("resume_ticket.json");
        write_ticket(&path, &ticket(500, &root)).unwrap();
        let e = run_warm_resume(&net(510, &root), &db, &Arc::new(StateHashComputer::new()), false, &path).await.unwrap_err();
        assert!(e.contains("clean"), "{e}");
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn bookmark_missing_falls_back() {
        let (dir, db, root) = store_at_500();
        db.delete(b"meta:last_seq").unwrap();
        let path = dir.path().join("resume_ticket.json");
        write_ticket(&path, &ticket(500, &root)).unwrap();
        let e = run_warm_resume(&net(510, &root), &db, &Arc::new(StateHashComputer::new()), true, &path).await.unwrap_err();
        assert!(e.contains("meta:last_seq"), "{e}");
    }

    #[tokio::test]
    async fn stale_ticket_falls_back() {
        let (dir, db, root) = store_at_500();
        let path = dir.path().join("resume_ticket.json");
        write_ticket(&path, &ticket(499, &root)).unwrap();
        let e = run_warm_resume(&net(510, &root), &db, &Arc::new(StateHashComputer::new()), true, &path).await.unwrap_err();
        assert!(e.contains("does not match"), "{e}");
    }

    #[tokio::test]
    async fn over_the_gap_cap_falls_back() {
        let (dir, db, root) = store_at_500();
        let path = dir.path().join("resume_ticket.json");
        write_ticket(&path, &ticket(500, &root)).unwrap();
        let e = run_warm_resume(&net(1_501, &root), &db, &Arc::new(StateHashComputer::new()), true, &path).await.unwrap_err();
        assert!(e.contains("ledgers behind"), "{e}");
    }

    #[tokio::test]
    async fn wrong_root_falls_back() {
        let (dir, db, root) = store_at_500();
        let path = dir.path().join("resume_ticket.json");
        write_ticket(&path, &ticket(500, &root)).unwrap();
        let other = "00".repeat(32);
        let e = run_warm_resume(&net(510, &other), &db, &Arc::new(StateHashComputer::new()), true, &path).await.unwrap_err();
        assert!(e.contains("network account_hash"), "{e}");
    }

    #[tokio::test]
    async fn network_unavailable_falls_back() {
        let (dir, db, root) = store_at_500();
        let path = dir.path().join("resume_ticket.json");
        write_ticket(&path, &ticket(500, &root)).unwrap();
        let n = FakeNet { validated: Some(510), hash: None };
        let e = run_warm_resume(&n, &db, &Arc::new(StateHashComputer::new()), true, &path).await.unwrap_err();
        assert!(e.contains("unavailable"), "{e}");
    }

    #[tokio::test]
    async fn rehearsal_check_needs_no_ticket() {
        let (_dir, db, root) = store_at_500();
        let v = verify_state_at_bookmark(&net(900, &root), &db, &Arc::new(StateHashComputer::new())).await.unwrap();
        assert_eq!((v.seq, v.gap), (500, 400));
    }

    /// Final-review finding (2026-09-24): both cold branches scan the wallet count before ws-sync
    /// adjusts it; a warm start that skipped the scan left it at 0, and the first deleted
    /// AccountRoot wrapped the counter to 2^64 - 1 on the dashboard.
    #[tokio::test]
    async fn warm_resume_scans_the_wallet_count() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(rocksdb::DB::open_default(dir.path().join("state.rocks")).unwrap());
        let account_root = |b: u8| {
            let mut v = vec![0x11, 0x00, 0x61];
            v.extend_from_slice(&[b; 37]);
            v
        };
        db.put([0x21u8; 32], account_root(0x21)).unwrap();
        db.put([0x42u8; 32], account_root(0x42)).unwrap();
        db.put([0x77u8; 32], vec![0x11, 0x00, 0x64, 7, 7]).unwrap(); // a DirectoryNode, not a wallet
        db.put(b"meta:last_seq", 500u32.to_le_bytes()).unwrap();
        let root = StateHashComputer::new().update_and_hash(&db, &[]).map(|r| hex::encode(r.0)).unwrap();
        let path = dir.path().join("resume_ticket.json");
        write_ticket(&path, &ticket(500, &root)).unwrap();
        let hc = Arc::new(StateHashComputer::new());
        run_warm_resume(&net(510, &root), &db, &hc, true, &path).await.unwrap();
        assert_eq!(hc.wallet_count.load(std::sync::atomic::Ordering::Relaxed), 2);
    }
}
