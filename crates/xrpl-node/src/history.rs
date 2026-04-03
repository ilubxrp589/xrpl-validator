//! Historical ledger round data — SQLite-backed time-series for uptime tracking.
//!
//! Stores one row per verified ledger round (~1 row every 3.5s).
//! Growth: ~2.5MB/day, ~900MB/year. Queryable for uptime %, avg round time, etc.

use rusqlite::{Connection, params};

/// A single ledger round record.
#[derive(Clone, serde::Serialize)]
pub struct LedgerRound {
    pub seq: u32,
    pub matched: bool,
    pub healed: bool,
    pub txs: u32,
    pub objs: u32,
    pub round_time_ms: u32,
    pub compute_time_ms: u32,
    pub peers: u32,
    pub timestamp: u64,
}

/// Aggregated summary over a time period.
#[derive(Clone, Default, serde::Serialize)]
pub struct HistorySummary {
    pub total_rounds: u64,
    pub total_matches: u64,
    pub total_mismatches: u64,
    pub total_healed: u64,
    pub uptime_pct: f64,
    pub avg_round_time_ms: f64,
    pub avg_compute_time_ms: f64,
    pub min_round_time_ms: u32,
    pub max_round_time_ms: u32,
    pub first_seq: u32,
    pub last_seq: u32,
    pub first_timestamp: u64,
    pub last_timestamp: u64,
}

/// SQLite-backed history store.
pub struct HistoryStore {
    conn: Connection,
    /// Counter for periodic pruning — prune every N inserts.
    insert_count: std::cell::Cell<u64>,
}

impl HistoryStore {
    /// Open or create the history database at the given path.
    pub fn open(path: &str) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| format!("sqlite open: {e}"))?;

        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS ledger_rounds (
                 seq            INTEGER PRIMARY KEY,
                 matched        INTEGER NOT NULL,
                 healed         INTEGER NOT NULL,
                 txs            INTEGER NOT NULL,
                 objs           INTEGER NOT NULL,
                 round_time_ms  INTEGER NOT NULL,
                 compute_time_ms INTEGER NOT NULL,
                 peers          INTEGER NOT NULL,
                 timestamp      INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_timestamp ON ledger_rounds(timestamp);"
        ).map_err(|e| format!("sqlite init: {e}"))?;

        eprintln!("[history] Opened SQLite database at {path}");
        Ok(Self { conn, insert_count: std::cell::Cell::new(0) })
    }

    /// Record a ledger round. Automatically prunes old data every 10000 inserts.
    pub fn record(&self, r: &LedgerRound) {
        if let Err(e) = self.conn.execute(
            "INSERT OR REPLACE INTO ledger_rounds (seq, matched, healed, txs, objs, round_time_ms, compute_time_ms, peers, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                r.seq,
                r.matched as i32,
                r.healed as i32,
                r.txs,
                r.objs,
                r.round_time_ms,
                r.compute_time_ms,
                r.peers,
                r.timestamp,
            ],
        ) {
            eprintln!("[history] SQLite write failed: {e}");
        }

        // SECURITY(4.4): Periodic pruning to prevent unbounded growth
        let count = self.insert_count.get() + 1;
        self.insert_count.set(count);
        if count % 10_000 == 0 {
            self.prune(90);
        }
    }

    /// Delete rows older than `keep_days` days. Default retention: 90 days.
    pub fn prune(&self, keep_days: u64) {
        let cutoff = now_secs().saturating_sub(keep_days * 86400);
        match self.conn.execute(
            "DELETE FROM ledger_rounds WHERE timestamp < ?1",
            params![cutoff],
        ) {
            Ok(deleted) => {
                if deleted > 0 {
                    eprintln!("[history] Pruned {deleted} rows older than {keep_days} days");
                }
            }
            Err(e) => eprintln!("[history] Prune failed: {e}"),
        }
    }

    /// Get summary statistics for a time period (in seconds from now).
    pub fn summary(&self, period_secs: u64) -> HistorySummary {
        let cutoff = now_secs().saturating_sub(period_secs);
        let mut stmt = match self.conn.prepare(
            "SELECT COUNT(*), SUM(matched), SUM(healed),
                    AVG(round_time_ms), AVG(compute_time_ms),
                    MIN(round_time_ms), MAX(round_time_ms),
                    MIN(seq), MAX(seq), MIN(timestamp), MAX(timestamp)
             FROM ledger_rounds WHERE timestamp >= ?1"
        ) {
            Ok(s) => s,
            Err(_) => return HistorySummary::default(),
        };

        stmt.query_row(params![cutoff], |row| {
            let total: u64 = row.get(0).unwrap_or(0);
            let matches: u64 = row.get(1).unwrap_or(0);
            let healed: u64 = row.get(2).unwrap_or(0);
            let avg_round: f64 = row.get(3).unwrap_or(0.0);
            let avg_compute: f64 = row.get(4).unwrap_or(0.0);
            let min_round: u32 = row.get(5).unwrap_or(0);
            let max_round: u32 = row.get(6).unwrap_or(0);
            let first_seq: u32 = row.get(7).unwrap_or(0);
            let last_seq: u32 = row.get(8).unwrap_or(0);
            let first_ts: u64 = row.get(9).unwrap_or(0);
            let last_ts: u64 = row.get(10).unwrap_or(0);
            let uptime = if total > 0 { matches as f64 / total as f64 * 100.0 } else { 0.0 };

            Ok(HistorySummary {
                total_rounds: total,
                total_matches: matches,
                total_mismatches: total - matches,
                total_healed: healed,
                uptime_pct: (uptime * 1000.0).round() / 1000.0,
                avg_round_time_ms: (avg_round * 10.0).round() / 10.0,
                avg_compute_time_ms: (avg_compute * 10.0).round() / 10.0,
                min_round_time_ms: min_round,
                max_round_time_ms: max_round,
                first_seq,
                last_seq,
                first_timestamp: first_ts,
                last_timestamp: last_ts,
            })
        }).unwrap_or(HistorySummary {
            total_rounds: 0, total_matches: 0, total_mismatches: 0, total_healed: 0,
            uptime_pct: 0.0, avg_round_time_ms: 0.0, avg_compute_time_ms: 0.0,
            min_round_time_ms: 0, max_round_time_ms: 0,
            first_seq: 0, last_seq: 0, first_timestamp: 0, last_timestamp: 0,
        })
    }

    /// Get recent ledger rounds.
    pub fn recent(&self, limit: usize) -> Vec<LedgerRound> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT seq, matched, healed, txs, objs, round_time_ms, compute_time_ms, peers, timestamp
             FROM ledger_rounds ORDER BY seq DESC LIMIT ?1"
        ) else { return Vec::new(); };

        stmt.query_map(params![limit as u32], |row| {
            Ok(LedgerRound {
                seq: row.get(0)?,
                matched: row.get::<_, i32>(1)? != 0,
                healed: row.get::<_, i32>(2)? != 0,
                txs: row.get(3)?,
                objs: row.get(4)?,
                round_time_ms: row.get(5)?,
                compute_time_ms: row.get(6)?,
                peers: row.get(7)?,
                timestamp: row.get(8)?,
            })
        }).map(|rows| rows.filter_map(|r| r.ok()).collect()).unwrap_or_default()
    }

    /// Total rounds ever recorded.
    pub fn total_rounds(&self) -> u64 {
        self.conn.query_row("SELECT COUNT(*) FROM ledger_rounds", [], |row| row.get(0)).unwrap_or(0)
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
