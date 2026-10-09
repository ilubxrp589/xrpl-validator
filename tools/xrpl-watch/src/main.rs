//! xrpl-watch — HALCYON, the live validator's terminal dashboard (the Rust successor of scripts/watch_engine.py).
//!
//! One screen, fixed panels:
//!   * LEDGER TAPE — one column per ledger, coloured by whose bytes were written for it and whether the hash held;
//!   * WHO WRITES THE LEDGER DATABASE — the Rust engine (XRPL_NATIVE_WRITER), the C++ core (Stage 3) or the
//!     upstream xrpld, and what the others are doing; the big counter is the Rust engine's ledgers;
//!   * SIGNING GATE — the state-hash streak our own SHAMap computes after every ledger, as a pulse;
//!   * RUST ENGINE and C++ COPY — the two transaction engines, each against mainnet and against each other;
//!   * UPSTREAM + AMENDMENTS — the xrpld we pull from, and the amendments on their way to activation;
//!   * CONSENSUS — the UNL's votes, one dot per trusted validator;
//!   * EVERY TRANSACTION TYPE and EVERY RESULT CODE since the validator started, and THIS LEDGER's mix;
//!   * the latest event, one line: receipts, writer decisions, disagreements, milestones.
//!
//! It reads the validator's API (VALIDATOR_BASE, default http://localhost:3777), the upstream's server_info and
//! feature (UPSTREAM_RPC, else the validator's XRPL_RPC_URL), and two files on the validator host: the native
//! shadow's receipts and the writer's decisions. It writes nothing.
//!
//!   xrpl-watch                         the live screen (q quits, p pauses)
//!   xrpl-watch --once [--size 160x48]  one frame as ANSI text on stdout
//!   xrpl-watch --record DIR [--frames N] [--interval S] [--size WxH]
//!                                      N frames, one every S seconds, as DIR/frame_00001.ans … (for videos)
//!   --basic                            16 colours instead of 24-bit (old terminals)
//!   keys: q quit · p pause

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------------------------------------------
// Palette: HALCYON synthwave (24-bit), or the 16 terminal colours with --basic

struct Pal {
    bg: Color,
    panel: Color,
    ink: Color,
    dim: Color,
    good: Color,
    warn: Color,
    bad: Color,
    rust: Color,
    cpp: Color,
    feed: Color,
    pink: Color,
    violet: Color,
    gold: Color,
}

static PAL: OnceLock<Pal> = OnceLock::new();

fn pal() -> &'static Pal {
    PAL.get_or_init(|| truecolor_pal())
}

fn truecolor_pal() -> Pal {
    Pal {
        bg: Color::Rgb(13, 8, 30),
        panel: Color::Rgb(18, 12, 40),
        ink: Color::Rgb(232, 228, 255),
        dim: Color::Rgb(120, 112, 160),
        good: Color::Rgb(1, 255, 137),
        warn: Color::Rgb(255, 196, 0),
        bad: Color::Rgb(255, 42, 109),
        rust: Color::Rgb(1, 255, 137),
        cpp: Color::Rgb(211, 0, 197),
        feed: Color::Rgb(5, 217, 232),
        pink: Color::Rgb(255, 42, 109),
        violet: Color::Rgb(185, 103, 255),
        gold: Color::Rgb(255, 220, 120),
    }
}

fn basic_pal() -> Pal {
    Pal {
        bg: Color::Reset,
        panel: Color::Reset,
        ink: Color::White,
        dim: Color::DarkGray,
        good: Color::LightGreen,
        warn: Color::Yellow,
        bad: Color::LightRed,
        rust: Color::LightGreen,
        cpp: Color::Magenta,
        feed: Color::Cyan,
        pink: Color::LightMagenta,
        violet: Color::Magenta,
        gold: Color::Yellow,
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Configuration

struct Cfg {
    base: String,
    upstream: String,
    receipts: String,
    writer_log: String,
    interval: Duration,
}

impl Cfg {
    fn from_env() -> Self {
        let validator = validator_proc();
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let upstream = env("UPSTREAM_RPC")
            .or_else(|| env("XRPL_RPC_URL"))
            .or_else(|| validator.as_ref().and_then(|p| p.env("XRPL_RPC_URL")))
            .map(|v| v.split(',').next().unwrap_or("").to_string())
            .unwrap_or_else(|| "http://localhost:5005".to_string());
        let receipts = env("SHADOW_RECEIPTS")
            .or_else(|| validator.as_ref().and_then(|p| p.env("XRPL_NATIVE_SHADOW_LOG")))
            .unwrap_or_else(|| "/mnt/xrpl-data/native_shadow.jsonl".to_string());
        let writer_log = env("WRITER_LOG")
            .or_else(|| validator.as_ref().and_then(|p| p.env("XRPL_NATIVE_WRITER_LOG")))
            .unwrap_or_else(|| {
                std::path::Path::new(&receipts)
                    .parent()
                    .map(|d| d.join("native_writer.jsonl").display().to_string())
                    .unwrap_or_else(|| "native_writer.jsonl".to_string())
            });
        Cfg {
            base: env("VALIDATOR_BASE").unwrap_or_else(|| "http://localhost:3777".to_string()),
            upstream,
            receipts,
            writer_log,
            interval: Duration::from_secs(1),
        }
    }
}

/// The running live_viewer on this host: its pid and environment.
struct ValidatorProc {
    pid: u32,
    environ: Vec<(String, String)>,
}

impl ValidatorProc {
    fn env(&self, k: &str) -> Option<String> {
        self.environ.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    }
    fn rss_gb(&self) -> Option<f64> {
        let s = fs::read_to_string(format!("/proc/{}/status", self.pid)).ok()?;
        let kb: f64 = s.lines().find(|l| l.starts_with("VmRSS:"))?.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb / 1024.0 / 1024.0)
    }
}

fn validator_proc() -> Option<ValidatorProc> {
    for e in fs::read_dir("/proc").ok()?.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let comm = fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        if comm.trim() != "live_viewer" {
            continue;
        }
        let raw = fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
        let environ = raw
            .split(|b| *b == 0)
            .filter_map(|kv| {
                let s = String::from_utf8_lossy(kv);
                s.split_once('=').map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect();
        return Some(ValidatorProc { pid, environ });
    }
    None
}

// ---------------------------------------------------------------------------------------------------------------
// One look at the validator

#[derive(Clone, Default)]
struct Snap {
    eng: Value,
    cons: Value,
    sh: Value,
    up: Value,
    api_err: Option<String>,
    up_err: Option<String>,
    pid: Option<u32>,
    rss_gb: Option<f64>,
    flow_engine: Option<String>,
    receipts: Option<(usize, usize)>, // (receipts, canary lines) this soak
    receipts_age_h: Option<f64>,
    last_decision: Option<String>,
    ter_miss_top: Vec<(String, usize)>,
    amendments: Vec<(String, i64, bool)>, // (name, seconds to activation, supported upstream)
    at: Option<SystemTime>,
}

fn get_json(url: &str) -> Result<Value, String> {
    ureq::get(url)
        .timeout(Duration::from_secs(2))
        .call()
        .map_err(|e| e.to_string())?
        .into_json::<Value>()
        .map_err(|e| e.to_string())
}

fn rpc(url: &str, method: &str) -> Result<Value, String> {
    let v: Value = ureq::post(url)
        .timeout(Duration::from_secs(3))
        .send_json(json!({"method": method, "params": [{}]}))
        .map_err(|e| e.to_string())?
        .into_json()
        .map_err(|e| e.to_string())?;
    Ok(v["result"].clone())
}

/// The amendments with a majority, soonest first: (name, seconds until activation, supported by the upstream).
/// A majority held for two weeks activates; `majority` is the time it was gained, in Ripple-epoch seconds.
fn pending_amendments(features: &Value) -> Vec<(String, i64, bool)> {
    const RIPPLE_EPOCH: i64 = 946_684_800;
    const TWO_WEEKS: i64 = 14 * 24 * 3600;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0) - RIPPLE_EPOCH;
    let mut v: Vec<(String, i64, bool)> = features
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, f)| !f["enabled"].as_bool().unwrap_or(false))
        .filter_map(|(_, f)| {
            let m = f["majority"].as_i64()?;
            Some((f["name"].as_str().unwrap_or("?").to_string(), m + TWO_WEEKS - now, f["supported"].as_bool().unwrap_or(false)))
        })
        .collect();
    v.sort_by_key(|a| a.1);
    v
}

/// Appended lines of a JSONL file between looks (resets when the file is cut, as at a soak start).
struct Tail {
    path: String,
    pos: u64,
    primed: bool,
}

impl Tail {
    fn new(path: &str) -> Self {
        Tail { path: path.to_string(), pos: 0, primed: false }
    }
    /// New complete lines since the last call. The first call only finds the end (no history replayed).
    fn new_lines(&mut self) -> Vec<String> {
        let Ok(mut f) = fs::File::open(&self.path) else { return Vec::new() };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if !self.primed {
            self.primed = true;
            self.pos = len;
            return Vec::new();
        }
        if len < self.pos {
            self.pos = 0;
        }
        if f.seek(SeekFrom::Start(self.pos)).is_err() {
            return Vec::new();
        }
        let mut buf = String::new();
        let _ = f.take(4_000_000).read_to_string(&mut buf);
        let complete = match buf.rfind('\n') {
            Some(i) => &buf[..=i],
            None => "",
        };
        self.pos += complete.len() as u64;
        complete.lines().filter(|l| !l.trim().is_empty()).map(|l| l.to_string()).collect()
    }
}

#[derive(Clone)]
struct Ev {
    at: String,
    tone: Tone,
    text: String,
}

#[derive(Clone, Copy, PartialEq)]
enum Tone {
    Good,
    Info,
    Warn,
    Bad,
}

/// Whose bytes were written for one ledger, and how its state hash went.
#[derive(Clone, Copy, PartialEq)]
enum Who {
    Rust,
    Cpp,
    Feed,
    Before, // before this screen started: the writer is not known
}

#[derive(Clone, Copy)]
struct Cell {
    seq: u64,
    who: Who,
    /// Our bytes failed the hash and the ledger was redone from the network's (Phase B's quiet retry).
    redone: bool,
    /// The final hash check of the ledger failed (a rolled-back mismatch that never matched).
    missed: bool,
}

fn hhmmss(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0) + local_offset_secs();
    let s = secs.rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

/// The local UTC offset, read once from `date +%z`.
fn local_offset_secs() -> i64 {
    static OFF: OnceLock<i64> = OnceLock::new();
    *OFF.get_or_init(|| {
        let out = std::process::Command::new("date").arg("+%z").output().ok();
        let z = out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
        if z.len() == 5 {
            let sign = if z.starts_with('-') { -1 } else { 1 };
            let h: i64 = z[1..3].parse().unwrap_or(0);
            let m: i64 = z[3..5].parse().unwrap_or(0);
            sign * (h * 3600 + m * 60)
        } else {
            0
        }
    })
}

/// Everything the screen needs, refreshed by the collector thread.
#[derive(Clone)]
struct Shared {
    snap: Snap,
    events: VecDeque<Ev>,
    apply_ms: VecDeque<u64>,
    tape: VecDeque<Cell>,
    /// When each new ledger was first seen, for the ledger cadence.
    closes: VecDeque<Instant>,
}

const EVENTS_MAX: usize = 200;
const SPARK_MAX: usize = 240;
const TAPE_MAX: usize = 600;

fn push_ev(sh: &mut Shared, tone: Tone, text: String) {
    let at = hhmmss(SystemTime::now());
    sh.events.push_front(Ev { at, tone, text });
    sh.events.truncate(EVENTS_MAX);
}

fn get<'a>(v: &'a Value, path: &[&str]) -> &'a Value {
    let mut cur = v;
    for p in path {
        cur = &cur[*p];
    }
    cur
}
fn u(v: &Value, path: &[&str]) -> u64 {
    get(v, path).as_u64().unwrap_or(0)
}
fn b(v: &Value, path: &[&str]) -> bool {
    get(v, path).as_bool().unwrap_or(false)
}
fn s<'a>(v: &'a Value, path: &[&str]) -> &'a str {
    get(v, path).as_str().unwrap_or("")
}

/// The receipts file: (receipts, canary lines) and its age, plus the ter-miss pairs of this run.
fn receipts_summary(path: &str, run_start: u64) -> (Option<(usize, usize)>, Option<f64>, Vec<(String, usize)>) {
    let Ok(raw) = fs::read(path) else { return (None, None, Vec::new()) };
    let age = fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .map(|d| d.as_secs_f64() / 3600.0);
    let (mut n_rec, mut n_can) = (0, 0);
    let mut pairs: BTreeMap<String, usize> = BTreeMap::new();
    for ln in raw.split(|c| *c == b'\n') {
        if ln.iter().all(|c| c.is_ascii_whitespace()) {
            continue;
        }
        let txt = String::from_utf8_lossy(ln);
        if txt.contains("\"canary\"") {
            n_can += 1;
            continue;
        }
        n_rec += 1;
        if !txt.contains("\"ter_mismatch\":[\"") && !txt.contains("\"ter_mismatch\": [\"") {
            continue;
        }
        let Ok(d) = serde_json::from_str::<Value>(&txt) else { continue };
        if d["seq"].as_u64().unwrap_or(0) < run_start {
            continue;
        }
        for e in d["ter_mismatch"].as_array().into_iter().flatten() {
            let e = e.as_str().unwrap_or("");
            let (head, rest) = e.split_once(' ').unwrap_or((e, ""));
            let ty = head.split_once(':').map(|x| x.1).unwrap_or("?");
            let vs = rest.split(" STALE").next().unwrap_or("").split(" FIELDS").next().unwrap_or("").trim();
            *pairs.entry(format!("{ty}  {vs}")).or_default() += 1;
        }
    }
    let mut top: Vec<(String, usize)> = pairs.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1));
    top.truncate(3);
    (Some((n_rec, n_can)), age, top)
}

/// The final state-hash verdict per ledger in /api/state-hash's sync log (the last entry for a seq wins).
fn sync_verdicts(sh: &Value) -> BTreeMap<u64, (bool, bool)> {
    // seq -> (matched at last, ever failed)
    let mut m: BTreeMap<u64, (bool, bool)> = BTreeMap::new();
    for e in sh["sync_log"].as_array().into_iter().flatten() {
        let seq = e["seq"].as_u64().unwrap_or(0);
        let ok = e["matched"].as_bool().unwrap_or(false);
        let ent = m.entry(seq).or_insert((ok, !ok));
        ent.0 = ok;
        ent.1 |= !ok;
    }
    m
}

/// The collector: one look per interval; the tape and the events come from what changed.
fn collect(cfg: Arc<Cfg>, shared: Arc<Mutex<Shared>>, frames_left: Option<Arc<Mutex<usize>>>) {
    let mut receipts_tail = Tail::new(&cfg.receipts);
    let mut writer_tail = Tail::new(&cfg.writer_log);
    let mut prev: Option<Snap> = None;
    let mut last_decision: Option<String> = None;
    let mut amendments: Vec<(String, i64, bool)> = Vec::new();
    let mut amend_at: Option<Instant> = None;
    loop {
        let t0 = Instant::now();
        let mut sn = Snap::default();
        match get_json(&format!("{}/api/engine", cfg.base)) {
            Ok(v) => sn.eng = v,
            Err(e) => sn.api_err = Some(e),
        }
        sn.cons = get_json(&format!("{}/api/consensus", cfg.base)).unwrap_or(Value::Null);
        sn.sh = get_json(&format!("{}/api/state-hash", cfg.base)).unwrap_or(Value::Null);
        match rpc(&cfg.upstream, "server_info") {
            Ok(v) => sn.up = v["info"].clone(),
            Err(e) => sn.up_err = Some(e),
        }
        if amend_at.map_or(true, |t| t.elapsed() > Duration::from_secs(60)) {
            if let Ok(v) = rpc(&cfg.upstream, "feature") {
                amendments = pending_amendments(&v["features"]);
                amend_at = Some(Instant::now());
            }
        }
        // Count down between fetches.
        let since = amend_at.map(|t| t.elapsed().as_secs() as i64).unwrap_or(0);
        sn.amendments = amendments.iter().map(|(n, e, s)| (n.clone(), e - since, *s)).collect();
        if let Some(p) = validator_proc() {
            sn.pid = Some(p.pid);
            sn.rss_gb = p.rss_gb();
            sn.flow_engine = p.env("XRPL_FLOW_ENGINE");
        }
        let led = u(&sn.eng, &["native_shadow", "ledgers"]);
        let run_start = u(&sn.eng, &["ledger_seq"]).saturating_sub(led + 2);
        let (rc, age, top) = receipts_summary(&cfg.receipts, run_start);
        sn.receipts = rc;
        sn.receipts_age_h = age;
        sn.ter_miss_top = top;
        sn.at = Some(SystemTime::now());

        let mut g = shared.lock().unwrap();
        // ---- the tape: one cell per ledger ws-sync wrote ----
        let verdicts = sync_verdicts(&sn.sh);
        let cur = u(&sn.sh, &["ledger_seq"]);
        match &prev {
            None => {
                // Seed with the sync log's history. Who wrote those ledgers is known only when the writer's counters
                // leave no doubt: with no refusal, retry or failure since its copy loaded, the last `native` ledgers
                // were the Rust engine's and the `unready` ones before them .39's (the loading window).
                let ns = &sn.eng["native_shadow"];
                let (wn, wu) = (u(ns, &["writer", "native"]), u(ns, &["writer", "unready"]));
                let clean = b(ns, &["writer", "enabled"])
                    && u(ns, &["writer", "refused"]) + u(ns, &["writer", "distrusted"]) + u(ns, &["writer", "mismatch"]) == 0;
                let seqs: Vec<(u64, bool)> = verdicts.iter().map(|(q, (ok, _))| (*q, *ok)).collect();
                let n = seqs.len() as u64;
                for (i, (seq, ok)) in seqs.into_iter().enumerate() {
                    let from_end = n - i as u64; // 1 = the newest
                    let who = if !clean {
                        Who::Before
                    } else if from_end <= wn {
                        Who::Rust
                    } else if from_end <= wn + wu {
                        Who::Feed
                    } else {
                        Who::Before
                    };
                    g.tape.push_back(Cell { seq, who, redone: false, missed: !ok });
                }
            }
            Some(p) => {
                let last = g.tape.back().map(|c| c.seq).unwrap_or(u(&p.sh, &["ledger_seq"]));
                if cur > last && cur - last < 500 {
                    let w = |path: &[&str]| u(&sn.eng, path).saturating_sub(u(&p.eng, path));
                    let mut rust = w(&["native_shadow", "writer", "native"]);
                    let mut redone = w(&["native_shadow", "writer", "mismatch"]);
                    let default = match writer_mode(&sn) {
                        Writer::Cpp => Who::Cpp,
                        Writer::RustWriting => Who::Rust,
                        _ => Who::Feed,
                    };
                    let writer_on = b(&sn.eng, &["native_shadow", "writer", "enabled"]);
                    for seq in last + 1..=cur {
                        let who = if writer_on {
                            if rust > 0 {
                                rust -= 1;
                                Who::Rust
                            } else {
                                Who::Feed
                            }
                        } else {
                            default
                        };
                        let r = redone > 0 && who == Who::Feed;
                        if r {
                            redone -= 1;
                        }
                        let missed = verdicts.get(&seq).map(|v| !v.0).unwrap_or(false);
                        g.tape.push_back(Cell { seq, who, redone: r, missed });
                        g.closes.push_back(Instant::now());
                    }
                    while g.tape.len() > TAPE_MAX {
                        g.tape.pop_front();
                    }
                    while g.closes.len() > 40 {
                        g.closes.pop_front();
                    }
                }
            }
        }

        // ---- events: new receipts and writer decisions ----
        for ln in receipts_tail.new_lines() {
            let Ok(d) = serde_json::from_str::<Value>(&ln) else { continue };
            let seq = d["seq"].as_u64().unwrap_or(0);
            if let Some(c) = d.get("canary") {
                let ok = c["detected"].as_bool().unwrap_or(false);
                push_ev(
                    &mut g,
                    if ok { Tone::Good } else { Tone::Bad },
                    format!("canary #{seq}: a planted 1-drop diff {}", if ok { "was flagged ✓ — the compare can see" } else { "was MISSED — the compare is blind" }),
                );
                continue;
            }
            let n = |k: &str| d[k].as_array().map(|a| a.len()).unwrap_or(0);
            let ter: Vec<String> = d["ter_mismatch"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|e| e.as_str())
                .map(|e| e.split(" FIELDS").next().unwrap_or(e).split(" STALE").next().unwrap_or(e).to_string())
                .collect();
            let what = if ter.is_empty() {
                format!("{} missing · {} extra · {} byte diffs vs the C++ copy", n("missing"), n("extra"), n("byte_diff"))
            } else {
                format!("result {}", ter[0])
            };
            push_ev(&mut g, Tone::Warn, format!("receipt #{seq}: {what}"));
        }
        for ln in writer_tail.new_lines() {
            let Ok(d) = serde_json::from_str::<Value>(&ln) else { continue };
            let seq = d["seq"].as_u64().unwrap_or(0);
            let (tone, text) = if d.get("breaker").is_some() {
                (Tone::Bad, format!("WRITER BREAKER at #{seq}: repeated hash failures — .39 writes until restart"))
            } else if d.get("mismatch").is_some() {
                (Tone::Bad, format!("#{seq}: our bytes failed the state hash — rolled back, redone from .39"))
            } else if let Some(why) = d["refused"].as_str() {
                (Tone::Warn, format!("#{seq}: the engine could not vouch — {}", why.chars().take(90).collect::<String>()))
            } else {
                (Tone::Info, format!("#{seq}: {}", ln.chars().take(100).collect::<String>()))
            };
            last_decision = Some(format!("{} {}", hhmmss(SystemTime::now()), text));
            push_ev(&mut g, tone, text);
        }
        sn.last_decision = last_decision.clone();

        // ---- events: what changed since the last look ----
        if let Some(p) = &prev {
            let mm = u(&sn.sh, &["total_mismatches"]).saturating_sub(u(&p.sh, &["total_mismatches"]));
            if mm > 0 {
                push_ev(&mut g, Tone::Bad, format!("STATE HASH MISMATCH ×{mm} at #{} — rolled back", commas(cur)));
            }
            if let (Some(now), Some(was)) = (
                sn.eng["ffi_verifier"]["live_diverged_by_type"].as_object(),
                p.eng["ffi_verifier"]["live_diverged_by_type"].as_object(),
            ) {
                for (k, v) in now {
                    let inc = v.as_u64().unwrap_or(0).saturating_sub(was.get(k).and_then(|x| x.as_u64()).unwrap_or(0));
                    if inc > 0 {
                        push_ev(&mut g, Tone::Warn, format!("the C++ copy disagreed with mainnet: {k} ×{inc} (comparison only — not written)"));
                    }
                }
            }
            let hydrated = b(&sn.eng, &["native_shadow", "hydrated"]);
            if hydrated && !b(&p.eng, &["native_shadow", "hydrated"]) {
                push_ev(
                    &mut g,
                    Tone::Good,
                    format!(
                        "the Rust engine loaded its copy of the state: {} objects in {}s — it writes from here",
                        commas(u(&sn.eng, &["native_shadow", "hydrate_objects"])),
                        u(&sn.eng, &["native_shadow", "hydrate_ms"]) / 1000
                    ),
                );
            }
            if !hydrated && b(&p.eng, &["native_shadow", "hydrated"]) {
                push_ev(&mut g, Tone::Warn, "the Rust engine dropped its copy of the state — reloading; .39 writes meanwhile".to_string());
            }
            let wn = u(&sn.eng, &["native_shadow", "writer", "native"]);
            let wn0 = u(&p.eng, &["native_shadow", "writer", "native"]);
            for m in [100u64, 500, 1_000, 5_000, 10_000, 50_000, 100_000, 500_000, 1_000_000] {
                if wn0 < m && wn >= m {
                    push_ev(&mut g, Tone::Good, format!("★ {} ledgers written by the Rust engine", commas(m)));
                }
            }
            let st = s(&sn.up, &["server_state"]);
            let st0 = s(&p.up, &["server_state"]);
            if !st.is_empty() && !st0.is_empty() && st != st0 {
                push_ev(&mut g, if st == "full" { Tone::Good } else { Tone::Warn }, format!("upstream xrpld: {st0} → {st}"));
            }
            if sn.api_err.is_some() && p.api_err.is_none() {
                push_ev(&mut g, Tone::Bad, "the validator's API is unreachable".to_string());
            }
            if sn.pid != p.pid {
                match sn.pid {
                    Some(pid) => push_ev(&mut g, Tone::Info, format!("validator process: pid {pid}")),
                    None => push_ev(&mut g, Tone::Bad, "no live_viewer process on this host".to_string()),
                }
            }
        } else {
            push_ev(
                &mut g,
                Tone::Info,
                match sn.pid {
                    Some(pid) => format!("watching the validator (pid {pid}) — the tape fills as ledgers close"),
                    None => "watching (no live_viewer process on this host)".to_string(),
                },
            );
        }
        if prev.as_ref().map_or(true, |p| u(&p.eng, &["native_shadow", "ledgers"]) != led) {
            let ms = u(&sn.eng, &["native_shadow", "apply_ms_last"]);
            g.apply_ms.push_back(ms);
            while g.apply_ms.len() > SPARK_MAX {
                g.apply_ms.pop_front();
            }
        }
        g.snap = sn.clone();
        drop(g);
        prev = Some(sn);
        if let Some(left) = &frames_left {
            if *left.lock().unwrap() == 0 {
                return;
            }
        }
        let spent = t0.elapsed();
        if spent < cfg.interval {
            std::thread::sleep(cfg.interval - spent);
        }
    }
}

fn commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn pct(a: u64, b: u64) -> f64 {
    if b == 0 { 0.0 } else { a as f64 * 100.0 / b as f64 }
}

fn dur(secs: i64) -> String {
    if secs <= 0 {
        return "now".to_string();
    }
    let (d, h, m) = (secs / 86_400, (secs / 3600) % 24, (secs / 60) % 60);
    if d > 0 { format!("{d}d {h:02}h") } else if h > 0 { format!("{h}h {m:02}m") } else { format!("{m}m {:02}s", secs % 60) }
}

// ---------------------------------------------------------------------------------------------------------------
// Drawing helpers

fn sp<'a>(t: impl Into<String>, c: Color) -> Span<'a> {
    Span::styled(t.into(), Style::new().fg(c))
}
fn bold<'a>(t: impl Into<String>, c: Color) -> Span<'a> {
    Span::styled(t.into(), Style::new().fg(c).add_modifier(Modifier::BOLD))
}
fn dim<'a>(t: impl Into<String>) -> Span<'a> {
    Span::styled(t.into(), Style::new().fg(pal().dim))
}
fn ink<'a>(t: impl Into<String>) -> Span<'a> {
    Span::styled(t.into(), Style::new().fg(pal().ink))
}
fn label<'a>(t: &str) -> Span<'a> {
    Span::styled(format!("{t:<11}"), Style::new().fg(pal().violet).add_modifier(Modifier::BOLD))
}

fn panel<'a>(title: Vec<Span<'a>>, color: Color) -> Block<'a> {
    let mut t = vec![Span::styled("━ ", Style::new().fg(color))];
    t.extend(title);
    t.push(Span::raw(" "));
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(color))
        .style(Style::new().bg(pal().panel))
        .title(Line::from(t))
}

/// Big digits, three rows tall, from half blocks.
fn big_digits(n: &str) -> [String; 3] {
    let glyph = |c: char| -> [&'static str; 3] {
        match c {
            '0' => ["█▀█", "█ █", "▀▀▀"],
            '1' => ["▄█ ", " █ ", "▀▀▀"],
            '2' => ["▀▀█", "█▀▀", "▀▀▀"],
            '3' => ["▀▀█", " ▀█", "▀▀▀"],
            '4' => ["█ █", "▀▀█", "  ▀"],
            '5' => ["█▀▀", "▀▀█", "▀▀▀"],
            '6' => ["█▀▀", "█▀█", "▀▀▀"],
            '7' => ["▀▀█", "  █", "  ▀"],
            '8' => ["█▀█", "█▀█", "▀▀▀"],
            '9' => ["█▀█", "▀▀█", "▀▀▀"],
            ',' => ["   ", " ▄ ", "▀  "],
            _ => ["   ", "   ", "   "],
        }
    };
    let mut rows = [String::new(), String::new(), String::new()];
    for c in n.chars() {
        let g = glyph(c);
        for (i, r) in rows.iter_mut().enumerate() {
            r.push_str(g[i]);
            if c != ',' {
                r.push(' ');
            }
        }
    }
    rows
}

/// What state.rocks is written from right now.
#[derive(PartialEq, Clone, Copy)]
enum Writer {
    RustWriting,
    RustLoading,
    RustBreaker,
    Cpp,
    Upstream,
}

fn writer_mode(sn: &Snap) -> Writer {
    let ns = &sn.eng["native_shadow"];
    if b(ns, &["writer", "enabled"]) {
        if b(ns, &["writer", "breaker"]) {
            Writer::RustBreaker
        } else if !b(ns, &["hydrated"]) {
            Writer::RustLoading
        } else {
            Writer::RustWriting
        }
    } else if b(&sn.eng, &["ffi_verifier", "stage3_enabled"]) {
        Writer::Cpp
    } else {
        Writer::Upstream
    }
}

/// The problems that make the header say ATTENTION, in plain words.
fn problems(sn: &Snap) -> Vec<String> {
    let mut p = Vec::new();
    if sn.api_err.is_some() {
        p.push("validator API unreachable".to_string());
        return p;
    }
    if sn.up_err.is_some() {
        p.push("upstream xrpld unreachable".to_string());
    } else {
        let st = s(&sn.up, &["server_state"]);
        if !["full", "proposing", "validating"].contains(&st) {
            p.push(format!("upstream not serving ({st})"));
        }
    }
    let mm = u(&sn.sh, &["total_mismatches"]);
    if mm > 0 {
        p.push(format!("{mm} state-hash mismatch"));
    }
    let w = writer_mode(sn);
    if !matches!(w, Writer::RustWriting | Writer::RustLoading | Writer::RustBreaker) {
        let div = u(&sn.eng, &["ffi_verifier", "live_apply_diverged"]);
        if div > 0 {
            p.push(format!("{div} tx diverged"));
        }
    }
    if w == Writer::RustBreaker {
        p.push("Rust writer breaker tripped".to_string());
    } else {
        let wm = u(&sn.eng, &["native_shadow", "writer", "mismatch"]);
        if wm > 0 {
            p.push(format!("{wm} hash failure(s) on our bytes"));
        }
    }
    p
}

// ---------------------------------------------------------------------------------------------------------------
// The screen

fn draw(f: &mut Frame, g: &Shared, paused: bool) {
    let p = pal();
    let area = f.area();
    f.render_widget(Block::new().style(Style::new().bg(p.bg)), area);
    let rows = Layout::vertical([
        Constraint::Length(1),  // header
        Constraint::Length(4),  // ledger tape
        Constraint::Length(8),  // who writes + signing gate
        Constraint::Length(11), // rust engine + C++ copy
        Constraint::Length(7),  // upstream + amendments, consensus
        Constraint::Min(5),     // every type, every result, this ledger
        Constraint::Length(1),  // the latest event
        Constraint::Length(1),  // footer
    ])
    .split(area);
    draw_header(f, rows[0], &g.snap);
    draw_tape(f, rows[1], g);
    let top = Layout::horizontal([Constraint::Percentage(64), Constraint::Percentage(36)]).split(rows[2]);
    draw_roles(f, top[0], &g.snap);
    draw_gate(f, top[1], g);
    let mid = Layout::horizontal([Constraint::Percentage(56), Constraint::Percentage(44)]).split(rows[3]);
    draw_rust(f, mid[0], g);
    draw_cpp(f, mid[1], &g.snap);
    let low = Layout::horizontal([Constraint::Percentage(56), Constraint::Percentage(44)]).split(rows[4]);
    draw_upstream(f, low[0], &g.snap);
    draw_consensus(f, low[1], &g.snap);
    let bottom = Layout::horizontal([Constraint::Percentage(36), Constraint::Percentage(44), Constraint::Percentage(20)]).split(rows[5]);
    draw_counts(f, bottom[0], &g.snap, true);
    draw_counts(f, bottom[1], &g.snap, false);
    draw_ledger_mix(f, bottom[2], &g.snap);
    draw_ticker(f, rows[6], g);
    draw_footer(f, rows[7], &g.snap, paused);
}

fn draw_header(f: &mut Frame, r: Rect, sn: &Snap) {
    let p = pal();
    let w = writer_mode(sn);
    let probs = problems(sn);
    let ready = b(&sn.sh, &["ready_to_sign"]);
    let consec = u(&sn.sh, &["consecutive_matches"]);
    let verdict = if !probs.is_empty() {
        Span::styled(format!(" ✗ ATTENTION: {} ", probs.join("; ")), Style::new().fg(Color::White).bg(p.bad).add_modifier(Modifier::BOLD))
    } else if ready {
        Span::styled(format!(" ✓ ALL GOOD · signing · {} in a row ", commas(consec)), Style::new().fg(p.bg).bg(p.feed).add_modifier(Modifier::BOLD))
    } else {
        Span::styled(" … warming up — verifying before signing ".to_string(), Style::new().fg(p.bg).bg(p.warn).add_modifier(Modifier::BOLD))
    };
    let stage = match w {
        Writer::RustWriting => Span::styled(" ★ STAGE 4 · RUST ENGINE WRITES THE LEDGER ", Style::new().fg(p.bg).bg(p.rust).add_modifier(Modifier::BOLD)),
        Writer::RustLoading => Span::styled(" STAGE 4 · Rust engine loading — .39 writes meanwhile ", Style::new().fg(p.bg).bg(p.warn).add_modifier(Modifier::BOLD)),
        Writer::RustBreaker => Span::styled(" STAGE 4 · WRITER OFF (breaker) ", Style::new().fg(Color::White).bg(p.bad).add_modifier(Modifier::BOLD)),
        Writer::Cpp => Span::styled(" ★ STAGE 3 · the C++ core writes ", Style::new().fg(p.bg).bg(p.cpp).add_modifier(Modifier::BOLD)),
        Writer::Upstream => Span::styled(" shadow mode · .39's bytes are written ", Style::new().fg(p.bg).bg(p.dim)),
    };
    let ledger = u(&sn.eng, &["ledger_seq"]);
    let up_secs = sn.at.and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64).unwrap_or(0)
        - get(&sn.eng, &["start_time_ms"]).as_i64().unwrap_or(0);
    let t = sn.at.map(hhmmss).unwrap_or_default();
    let mut spans = vec![Span::styled(" ◢◤ ", Style::new().fg(p.pink).add_modifier(Modifier::BOLD))];
    spans.extend(logo_spans(sn.at));
    spans.extend([
        Span::styled(" ◥◣ ", Style::new().fg(p.pink).add_modifier(Modifier::BOLD)),
        dim("m3060 "),
        bold(format!("#{}", commas(ledger)), p.ink),
        dim(format!(" · up {} ", dur(up_secs / 1000))),
        stage,
        Span::raw(" "),
        verdict,
        dim(format!(" {t}")),
    ]);
    let line = Line::from(spans);
    f.render_widget(Paragraph::new(line), r);
}

/// "HALCYON", one letter at a time, in a gradient that drifts a step every half second.
fn logo_spans(at: Option<SystemTime>) -> Vec<Span<'static>> {
    let p = pal();
    let ramp = [p.pink, p.violet, p.feed, p.good, p.feed, p.violet];
    let step = at.and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| (d.as_millis() / 500) as usize).unwrap_or(0);
    "HALCYON"
        .chars()
        .enumerate()
        .map(|(i, c)| Span::styled(format!("{c} "), Style::new().fg(ramp[(i + step) % ramp.len()]).add_modifier(Modifier::BOLD)))
        .collect()
}

fn who_color(c: &Cell) -> Color {
    let p = pal();
    if c.missed {
        return p.bad;
    }
    if c.redone {
        return p.warn;
    }
    match c.who {
        Who::Rust => p.rust,
        Who::Cpp => p.cpp,
        Who::Feed => p.feed,
        Who::Before => p.dim,
    }
}

fn draw_tape(f: &mut Frame, r: Rect, g: &Shared) {
    let p = pal();
    let block = panel(
        vec![
            bold("LEDGER TAPE", p.ink),
            dim(" — one column per ledger, newest at the right: "),
            sp("█ Rust wrote", p.rust),
            dim(" · "),
            sp("█ .39 wrote", p.feed),
            dim(" · "),
            sp("█ C++ wrote", p.cpp),
            dim(" · "),
            sp("█ redone", p.warn),
            dim(" · "),
            sp("█ hash miss", p.bad),
            dim(" · "),
            sp("█ before this screen", p.dim),
        ],
        p.violet,
    );
    let inner = block.inner(r);
    f.render_widget(block, r);
    let w = inner.width as usize;
    let cells: Vec<&Cell> = g.tape.iter().rev().take(w).collect::<Vec<_>>().into_iter().rev().collect();
    let pad = w.saturating_sub(cells.len());
    let mut top: Vec<Span> = vec![Span::raw(" ".repeat(pad))];
    let mut ticks = vec![' '; w];
    for (i, c) in cells.iter().enumerate() {
        let glyph = if c.missed { "▼" } else if c.redone { "▲" } else { "▊" };
        top.push(Span::styled(glyph, Style::new().fg(who_color(c))));
        if c.seq % 50 == 0 {
            let lab = format!("┊{}", commas(c.seq));
            for (j, ch) in lab.chars().enumerate() {
                if pad + i + j < w {
                    ticks[pad + i + j] = ch;
                }
            }
        }
    }
    let tick_line = Line::from(Span::styled(ticks.into_iter().collect::<String>(), Style::new().fg(p.dim)));
    f.render_widget(Paragraph::new(vec![Line::from(top), tick_line]), inner);
}

fn draw_roles(f: &mut Frame, r: Rect, sn: &Snap) {
    let p = pal();
    let ns = &sn.eng["native_shadow"];
    let ffi = &sn.eng["ffi_verifier"];
    let w = writer_mode(sn);
    let wn = u(ns, &["writer", "native"]);
    let wu = u(ns, &["writer", "unready"]);
    let wr = u(ns, &["writer", "refused"]);
    let wd = u(ns, &["writer", "distrusted"]);
    let since_load = wn + wr + wd;
    let ver = s(ffi, &["libxrpl_version"]).to_string();
    let net = s(&sn.up, &["build_version"]).to_string();
    let block = panel(vec![bold("WHO WRITES THE LEDGER DATABASE", p.ink), dim(" (state.rocks)")], p.ink);
    let inner = block.inner(r);
    f.render_widget(block, r);
    let cols = Layout::horizontal([Constraint::Min(50), Constraint::Length(24)]).split(inner);

    let rust = match w {
        Writer::RustWriting => vec![bold("● ACTIVELY WRITING", p.rust), sp(format!(" · {:.1}% since it loaded", pct(wn, since_load)), p.rust)],
        Writer::RustLoading => vec![bold("◌ LOADING its copy of the state", p.warn), dim("  — writes when loaded")],
        Writer::RustBreaker => vec![bold("✗ OFF — breaker tripped", p.bad), dim("  — .39 writes until restart")],
        _ if b(ns, &["enabled"]) => vec![sp("○ checking beside the writer", p.rust), dim("  — its bytes are not written")],
        _ => vec![dim("— not running")],
    };
    let cpp = match w {
        Writer::Cpp => vec![bold("● WRITING (Stage 3)", p.cpp), dim("  — its overlay is what state.rocks stores")],
        _ => vec![sp("○ COMPARING ONLY", p.cpp), dim(" · nothing it computes is written")],
    };
    let feed = match w {
        Writer::Upstream => vec![bold("● WRITING", p.feed), dim("  — every ledger's bytes come from its RPC")],
        _ => vec![sp("◐ DATA FEED", p.feed), dim(format!(" · ledgers + the hash to match · wrote {}", commas(wu + wr + wd)))],
    };
    let flow = sn.flow_engine.clone().unwrap_or_else(|| "model".to_string()).to_uppercase();
    let mut strip = vec![
        dim("flow "),
        sp(flow, if sn.flow_engine.as_deref() == Some("port") { p.good } else { p.warn }),
        dim(" · writer "),
        if b(ns, &["writer", "enabled"]) { sp("ON", p.good) } else { dim("off") },
        dim(" · Stage 3 "),
        if b(ffi, &["stage3_enabled"]) { sp("ON", p.cpp) } else { dim("off") },
        dim(" · "),
        sp(format!("libxrpl {ver}"), p.cpp),
    ];
    if !ver.is_empty() && !net.is_empty() && ver != net {
        strip.push(sp(format!(" ⚠ net {net}"), p.warn));
    }
    let lines = vec![
        Line::from([vec![bold("Rust engine (ours)    ", p.rust)], rust].concat()),
        Line::from([vec![bold(format!("C++ libxrpl {ver:<10}"), p.cpp)], cpp].concat()),
        Line::from([vec![bold(format!("xrpld .39 {net:<12}"), p.feed)], feed].concat()),
        Line::from(""),
        Line::from(strip),
    ];
    f.render_widget(Paragraph::new(lines), cols[0]);

    // The big counter: ledgers written by the Rust engine (or the signing streak when it does not write).
    let (n, cap1, cap2, col) = if b(ns, &["writer", "enabled"]) {
        (wn, "LEDGERS WRITTEN", "BY OUR RUST ENGINE", p.rust)
    } else {
        (u(&sn.sh, &["consecutive_matches"]), "LEDGERS IN A ROW", "STATE HASH = MAINNET", p.feed)
    };
    let digits = big_digits(&commas(n));
    let big: Vec<Line> = vec![
        Line::from(bold(digits[0].clone(), col)),
        Line::from(bold(digits[1].clone(), col)),
        Line::from(bold(digits[2].clone(), col)),
        Line::from(sp(cap1, p.gold)),
        Line::from(dim(cap2)),
    ];
    f.render_widget(Paragraph::new(big), cols[1]);
}

/// The validator's pulse: one beat per ledger, newest at the right, red where the hash failed.
fn pulse_lines(cells: &[&Cell], width: usize, color_ok: Color) -> [Line<'static>; 2] {
    let p = pal();
    const BEAT: usize = 5;
    let n = (width / BEAT).max(1);
    let shown: Vec<&&Cell> = cells.iter().rev().take(n).collect::<Vec<_>>().into_iter().rev().collect();
    let pad = width.saturating_sub(shown.len() * BEAT);
    let mut top: Vec<Span<'static>> = vec![Span::raw(" ".repeat(pad))];
    let mut bot: Vec<Span<'static>> = vec![Span::styled("─".repeat(pad), Style::new().fg(p.dim))];
    for c in shown {
        let col = if c.missed { p.bad } else if c.redone { p.warn } else { color_ok };
        if c.missed {
            top.push(Span::styled("  ╳  ", Style::new().fg(col).add_modifier(Modifier::BOLD)));
            bot.push(Span::styled("──┴──", Style::new().fg(col)));
        } else {
            top.push(Span::styled("  ╭╮ ", Style::new().fg(col)));
            bot.push(Span::styled("──╯╰─", Style::new().fg(col)));
        }
    }
    [Line::from(top), Line::from(bot)]
}

fn draw_gate(f: &mut Frame, r: Rect, g: &Shared) {
    let p = pal();
    let sn = &g.snap;
    let consec = u(&sn.sh, &["consecutive_matches"]);
    let m = u(&sn.sh, &["total_matches"]);
    let mm = u(&sn.sh, &["total_mismatches"]);
    let ready = b(&sn.sh, &["ready_to_sign"]);
    let nr = u(&sn.sh, &["validations_skipped_not_ready"]);
    let zh = u(&sn.sh, &["validations_skipped_zero_hash"]);
    let block = panel(vec![bold("SIGNING GATE", p.rust), dim(" our Rust SHAMap vs mainnet")], p.rust);
    let inner = block.inner(r);
    f.render_widget(block, r);
    let cadence = if g.closes.len() >= 3 {
        let span = g.closes.back().unwrap().duration_since(*g.closes.front().unwrap()).as_secs_f64();
        Some(span / (g.closes.len() - 1) as f64)
    } else {
        None
    };
    let mut lines = vec![
        Line::from(vec![
            bold(format!("{} in a row", commas(consec)), if mm == 0 { p.good } else { p.warn }),
            Span::raw("  "),
            if ready { bold("✓ SIGNING", p.good) } else { bold("not signing yet", p.warn) },
            dim(match cadence {
                Some(c) => format!("  ♥ {c:.1}s/ledger"),
                None => "  ♥ …".to_string(),
            }),
        ]),
        Line::from(vec![
            sp(format!("{} matched", commas(m)), p.good),
            dim(" · "),
            sp(format!("{} missed", commas(mm)), if mm > 0 { p.bad } else { p.good }),
            dim(format!(" · {} warm-up skips", commas(nr))),
            if zh > 0 { sp(format!(" + {zh} zero-hash"), p.bad) } else { dim("") },
        ]),
        Line::from(""),
    ];
    let cells: Vec<&Cell> = g.tape.iter().collect();
    let [a, b2] = pulse_lines(&cells, inner.width as usize, p.good);
    lines.push(a);
    lines.push(b2);
    lines.push(Line::from(dim("one beat per ledger: our whole database hashed = mainnet's")));
    f.render_widget(Paragraph::new(lines), inner);
}

fn spark(data: &[u64], width: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let tail: Vec<u64> = data.iter().rev().take(width).rev().copied().collect();
    let max = tail.iter().copied().max().unwrap_or(1).max(1);
    tail.iter().map(|v| BARS[((*v as f64 / max as f64) * 7.0).round() as usize]).collect()
}

fn draw_rust(f: &mut Frame, r: Rect, g: &Shared) {
    let p = pal();
    let sn = &g.snap;
    let ns = &sn.eng["native_shadow"];
    let w = writer_mode(sn);
    let (title, color) = match w {
        Writer::RustWriting => ("RUST ENGINE — ACTIVELY WRITING STATE", p.rust),
        Writer::RustLoading => ("RUST ENGINE — LOADING (writes when loaded)", p.warn),
        Writer::RustBreaker => ("RUST ENGINE — WRITER OFF (breaker)", p.bad),
        _ => ("RUST ENGINE — shadow (checks beside the writer)", p.rust),
    };
    let block = panel(vec![bold(title, color), dim(" [OURS]")], color);
    let wn = u(ns, &["writer", "native"]);
    let wu = u(ns, &["writer", "unready"]);
    let wr = u(ns, &["writer", "refused"]);
    let wd = u(ns, &["writer", "distrusted"]);
    let wm = u(ns, &["writer", "mismatch"]);
    let live = wn + wr + wd;
    let (ta, tm, tmm, bim) = (u(ns, &["txs_applied"]), u(ns, &["ter_matched"]), u(ns, &["ter_mismatched"]), u(ns, &["batch_inner_ter_mm"]));
    let (led, fm, dv) = (u(ns, &["ledgers"]), u(ns, &["full_match"]), u(ns, &["overlay_diverged"]));
    let (km, ke, kb) = (u(ns, &["key_missing"]), u(ns, &["key_extra"]), u(ns, &["byte_mismatch"]));
    let mut lines: Vec<Line> = Vec::new();
    if b(ns, &["writer", "enabled"]) {
        lines.push(Line::from(vec![
            label("WRITER"),
            bold(format!("{:.2}%", pct(wn, live)), if wm > 0 { p.bad } else if wr > 0 { p.warn } else { p.good }),
            ink(format!(" of ledgers since it loaded ({}/{})", commas(wn), commas(live))),
            dim(format!(" · {} since restart", commas(wn + wu + wr + wd))),
        ]));
        lines.push(Line::from(vec![
            label(""),
            dim(format!("from .39: loading {} · ", commas(wu))),
            sp(format!("disagreed {}", commas(wr)), if wr > 0 { p.warn } else { p.good }),
            dim(format!(" · retries {} · ", commas(wd))),
            sp(format!("hash failures {wm}"), if wm > 0 { p.bad } else { p.good }),
        ]));
    }
    lines.push(Line::from(vec![
        label("vs MAINNET"),
        sp(format!("{}/{} results match", commas(tm), commas(ta)), if tmm > 0 { p.bad } else { p.good }),
        dim(" · "),
        sp(format!("ter-miss {tmm}"), if tmm > 0 { p.bad } else { p.good }),
        if bim > 0 { sp(format!(" · batch-inner {bim}"), p.bad) } else { dim("") },
    ]));
    for (k, n) in &sn.ter_miss_top {
        lines.push(Line::from(vec![label(""), sp(format!("{n:>4}× "), p.bad), ink(k.clone())]));
    }
    lines.push(Line::from(vec![
        label("vs C++"),
        sp(format!("{}/{} ledgers agree byte-for-byte", commas(fm), commas(led)), if dv > 0 { p.warn } else { p.good }),
        if km + ke + kb > 0 { sp(format!(" · keys −{km} +{ke} ≠{kb}"), p.warn) } else { dim("") },
    ]));
    match sn.receipts {
        Some((n, c)) => {
            let age = sn.receipts_age_h.map(|h| format!(", last {h:.1}h ago")).unwrap_or_default();
            lines.push(Line::from(vec![
                label(""),
                dim("receipts this soak "),
                sp(commas(n as u64), if n == 0 { p.good } else { p.warn }),
                dim(format!(" (Rust vs C++, triaged on .39{age})")),
                if c > 0 { dim(format!(" · {c} canary")) } else { dim("") },
            ]));
        }
        None => lines.push(Line::from(vec![label(""), dim("receipts file not on this host")])),
    }
    let (cf, cd) = (u(ns, &["canary_fired"]), u(ns, &["canary_detected"]));
    if cf > 0 {
        lines.push(Line::from(vec![
            label("canary"),
            sp(format!("{cd}/{cf} planted diffs flagged"), if cd < cf { p.bad } else { p.good }),
            if cd < cf { bold("  THE COMPARE IS BLIND", p.bad) } else { dim("") },
        ]));
    }
    let objs = u(ns, &["hydrate_objects"]);
    let rss = sn.rss_gb.map(|g| format!(" · live_viewer {g:.1} GB")).unwrap_or_default();
    if objs > 0 {
        let (bad, rebad) = (u(ns, &["hydrate_decode_err"]), u(ns, &["hydrate_reencode_bad"]));
        lines.push(Line::from(vec![
            label("its copy"),
            ink(format!("{} objects", commas(objs))),
            dim(format!(" · loaded in {}s · ", u(ns, &["hydrate_ms"]) / 1000)),
            if bad + rebad == 0 { sp("clean", p.good) } else { sp(format!("{bad} undecodable, {rebad} re-encode-bad"), p.warn) },
            dim(rss),
        ]));
    } else if !b(ns, &["hydrated"]) {
        lines.push(Line::from(vec![label("its copy"), sp("loading from state.rocks…", p.warn), dim(rss)]));
    }
    lines.push(Line::from(vec![
        label("last"),
        match &sn.last_decision {
            Some(d) => ink(d.clone()),
            None => dim("no refusals or failures since this screen started"),
        },
    ]));
    let sw = (r.width as usize).saturating_sub(30);
    lines.push(Line::from(vec![
        label("apply"),
        sp(format!("{:>3} ms ", u(ns, &["apply_ms_last"])), p.good),
        sp(spark(&g.apply_ms.iter().copied().collect::<Vec<_>>(), sw), p.rust),
    ]));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), r);
}

fn draw_cpp(f: &mut Frame, r: Rect, sn: &Snap) {
    let p = pal();
    let ffi = &sn.eng["ffi_verifier"];
    let w = writer_mode(sn);
    let ver = s(ffi, &["libxrpl_version"]);
    let net = s(&sn.up, &["build_version"]);
    let title = if w == Writer::Cpp { format!("C++ CORE libxrpl {ver} — WRITING (Stage 3)") } else { format!("C++ COPY libxrpl {ver} — COMPARISON ONLY") };
    let block = panel(vec![bold(title, p.cpp), dim(" [RIPPLED]")], p.cpp);
    let attempted = u(ffi, &["live_apply_attempted"]);
    let ok = u(ffi, &["live_apply_ok"]);
    let claimed = u(ffi, &["live_apply_claimed"]);
    let div = u(ffi, &["live_apply_diverged"]);
    let agreed = ok + claimed;
    let mut lines: Vec<Line> = Vec::new();
    if !ver.is_empty() && !net.is_empty() && ver != net {
        lines.push(Line::from(vec![bold("⚠ SKEW     ", p.warn), dim(format!("net {net} · copy {ver} — refuses newer amendments"))]));
    }
    lines.push(Line::from(vec![
        label("vs MAINNET"),
        sp(format!("{:.2}%", pct(agreed, attempted)), if div > 0 { p.warn } else { p.good }),
        ink(format!(" ({}/{})", commas(agreed), commas(attempted))),
        sp(format!(" diverged {div}"), if div > 0 { p.warn } else { p.good }),
    ]));
    lines.push(Line::from(vec![
        label(""),
        sp(format!("tesSUCCESS {}", commas(ok)), p.good),
        dim(" · "),
        sp(format!("tec claimed {}", commas(claimed)), p.feed),
    ]));
    if let Some(m) = ffi["live_diverged_by_type"].as_object() {
        let mut v: Vec<(&String, u64)> = m.iter().map(|(k, n)| (k, n.as_u64().unwrap_or(0))).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        for (k, n) in v.into_iter().take(3) {
            lines.push(Line::from(vec![label("disagreed"), sp(format!("{n}× {k}"), p.warn)]));
        }
    }
    let (sil, mutd) = (u(ffi, &["live_apply_silent_diverged"]), u(ffi, &["live_apply_mutation_diverged"]));
    if sil + mutd > 0 {
        lines.push(Line::from(vec![label(""), sp(format!("silent {sil} · mutation-set {mutd}"), p.warn)]));
    }
    let (sa, sm, smm) = (u(ffi, &["shadow_hash_attempted"]), u(ffi, &["shadow_hash_matched"]), u(ffi, &["shadow_hash_mismatched"]));
    lines.push(Line::from(vec![
        label("its root"),
        sp(format!("{}/{} state roots = network", commas(sm), commas(sa)), if smm > 0 { p.warn } else { p.good }),
    ]));
    let (hits, fb) = (u(ffi, &["db_hits"]), u(ffi, &["db_rpc_fallbacks"]));
    lines.push(Line::from(vec![label("reads"), ink(format!("{:.2}% over RPC", pct(fb, hits + fb))), dim(" — keys our DB lacked")]));
    lines.push(Line::from(vec![label("ledgers"), ink(format!("{} applied since start", commas(u(ffi, &["ledgers_applied"]))))]));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), r);
}

/// A JSON {name: count} map, largest first.
fn counts(v: &Value) -> Vec<(String, u64)> {
    let mut out: Vec<(String, u64)> = v.as_object().into_iter().flatten().map(|(k, n)| (k.clone(), n.as_u64().unwrap_or(0))).collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

/// A name made to fit `w` cells: long words shortened, then the middle elided.
fn fit_name(k: &str, w: usize) -> String {
    let mut n = k.to_string();
    for (a, b) in [("Permissioned", "Perm"), ("Issuance", "Iss"), ("Authorize", "Auth"), ("INSUFFICIENT", "INSUF"), ("Credential", "Cred")] {
        if n.chars().count() > w {
            n = n.replace(a, b);
        }
    }
    let len = n.chars().count();
    if len <= w || w < 4 {
        return n.chars().take(w).collect();
    }
    let head = (w - 1) / 2;
    let tail = w - 1 - head;
    let c: Vec<char> = n.chars().collect();
    format!("{}…{}", c[..head].iter().collect::<String>(), c[len - tail..].iter().collect::<String>())
}

/// Every transaction type (or every result code) since the validator started, packed in columns: a magnitude
/// glyph (log-scaled), the name, the count. When they outgrow the panel, the last cell says how many are left.
fn draw_counts(f: &mut Frame, r: Rect, sn: &Snap, types: bool) {
    let p = pal();
    let ffi = &sn.eng["ffi_verifier"];
    let items = counts(&ffi[if types { "apply_by_type" } else { "live_apply_ter_counts" }]);
    let total: u64 = items.iter().map(|x| x.1).sum();
    let (title, sub, col) = if types {
        ("EVERY TX TYPE", format!(" {} · {} txs since start", items.len(), commas(total)), p.feed)
    } else {
        let ok = items.iter().find(|x| x.0 == "tesSUCCESS").map(|x| x.1).unwrap_or(0);
        ("EVERY RESULT CODE", format!(" {} · {:.1}% tesSUCCESS · tec = failed, fee paid", items.len(), pct(ok, total)), p.good)
    };
    let block = panel(vec![bold(title, col), dim(sub)], col);
    let inner = block.inner(r);
    f.render_widget(block, r);
    let color = |k: &str| {
        if types {
            if k.starts_with("Offer") || k.starts_with("AMM") {
                p.pink
            } else if k.starts_with("Payment") || k.starts_with("Check") {
                p.feed
            } else if k.starts_with("NFToken") {
                p.violet
            } else {
                p.gold
            }
        } else if k == "tesSUCCESS" {
            p.good
        } else if k.starts_with("tec") {
            p.warn
        } else {
            p.bad
        }
    };
    const GLYPH: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
    let max = items.first().map(|x| x.1).unwrap_or(1).max(1) as f64;
    let count_w = items.iter().map(|x| commas(x.1).chars().count()).max().unwrap_or(1);
    let rows = (inner.height as usize).max(1);
    let min_col = 2 + 12 + 1 + count_w + 1;
    let ncol = ((inner.width as usize) / min_col).clamp(1, items.len().div_ceil(rows).max(1));
    let colw = inner.width as usize / ncol;
    let name_w = colw.saturating_sub(2 + 1 + count_w + 1);
    let cap = ncol * rows;
    let cols = Layout::horizontal(vec![Constraint::Length(colw as u16); ncol]).split(inner);
    for c in 0..ncol {
        let mut lines: Vec<Line> = Vec::new();
        for i in c * rows..((c + 1) * rows).min(items.len()) {
            if items.len() > cap && i == cap - 1 {
                lines.push(Line::from(Span::styled(format!("  +{} more", items.len() - i), Style::new().fg(p.violet))));
                break;
            }
            let (k, n) = &items[i];
            let g = ((((*n as f64).ln_1p() / max.ln_1p()) * 7.0).round() as usize).min(7);
            let kc = color(k);
            lines.push(Line::from(vec![
                Span::styled(format!("{} ", GLYPH[g]), Style::new().fg(kc)),
                Span::styled(format!("{:<w$} ", fit_name(k, name_w), w = name_w), Style::new().fg(kc)),
                Span::styled(format!("{:>w$}", commas(*n), w = count_w), Style::new().fg(p.ink)),
            ]));
        }
        f.render_widget(Paragraph::new(lines), cols[c]);
    }
}

/// The event feed, cut to its newest line.
fn draw_ticker(f: &mut Frame, r: Rect, g: &Shared) {
    let p = pal();
    let line = match g.events.front() {
        Some(e) => {
            let (c, mark) = match e.tone {
                Tone::Good => (p.good, "✓"),
                Tone::Info => (p.ink, "·"),
                Tone::Warn => (p.warn, "!"),
                Tone::Bad => (p.bad, "✗"),
            };
            Line::from(vec![
                Span::styled(" ▸ latest ", Style::new().fg(p.violet).add_modifier(Modifier::BOLD)),
                dim(format!("{} ", e.at)),
                bold(format!("{mark} "), c),
                sp(e.text.clone(), c),
            ])
        }
        None => Line::from(dim(" ▸ latest — nothing yet")),
    };
    f.render_widget(Paragraph::new(line), r);
}

fn draw_upstream(f: &mut Frame, r: Rect, sn: &Snap) {
    let p = pal();
    let w = writer_mode(sn);
    let title = if w == Writer::Upstream { "UPSTREAM xrpld .39 — WRITING (RPC bytes)" } else { "UPSTREAM xrpld .39 — DATA FEED · AMENDMENTS" };
    let block = panel(vec![bold(title, p.feed), dim(" [RIPPLED]")], p.feed);
    let mut lines: Vec<Line> = Vec::new();
    if let Some(e) = &sn.up_err {
        lines.push(Line::from(vec![bold("UNREACHABLE", p.bad), dim(format!(" — {}", e.chars().take(60).collect::<String>()))]));
    } else {
        let st = s(&sn.up, &["server_state"]);
        let serving = ["full", "proposing", "validating"].contains(&st);
        let up_seq = sn.up["validated_ledger"]["seq"].as_u64();
        let ws = sn.sh["ledger_seq"].as_u64();
        let lag = match (up_seq, ws) {
            (Some(a), Some(b2)) => {
                let lag = a.saturating_sub(b2);
                sp(format!("{lag} behind"), if lag <= 5 { p.good } else if lag <= 50 { p.warn } else { p.bad })
            }
            _ => sp("lag unknown", p.warn),
        };
        lines.push(Line::from(vec![
            label("feed"),
            sp(s(&sn.up, &["build_version"]).to_string(), p.feed),
            dim(" · "),
            sp(st.to_string(), if serving { p.good } else { p.bad }),
            dim(format!(" · {} peers · ws-sync ", sn.up["peers"].as_u64().unwrap_or(0))),
            lag,
        ]));
        let cl = s(&sn.up, &["complete_ledgers"]);
        lines.push(Line::from(vec![label("history"), sp(cl.to_string(), if cl.is_empty() || cl == "empty" { p.bad } else { p.good })]));
    }
    if sn.amendments.is_empty() {
        lines.push(Line::from(vec![label("amendments"), dim("none with a majority")]));
    }
    for (name, eta, supported) in sn.amendments.iter().take(3) {
        lines.push(Line::from(vec![
            label("activates"),
            bold(format!("{name:<22}"), p.gold),
            sp(format!(" in {:>8}", dur(*eta)), p.warn),
            dim(if *supported { "  · the network's node supports it" } else { "  · NOT supported by .39" }),
        ]));
    }
    f.render_widget(Paragraph::new(lines).block(block), r);
}

fn draw_consensus(f: &mut Frame, r: Rect, sn: &Snap) {
    let p = pal();
    let c = &sn.cons;
    let block = panel(vec![bold("CONSENSUS", p.violet), dim(" the UNL's votes [OURS]")], p.violet);
    let unl = u(c, &["unl_size"]);
    let tracked = u(c, &["monitor", "tracked_validators"]);
    let props = u(c, &["monitor", "total_proposals"]);
    let ag = &c["monitor"]["agreement"];
    let count = ag["count"].as_u64().unwrap_or(0);
    let ratio = ag["pct"].as_f64().unwrap_or(0.0).clamp(0.0, 1.0);
    let col = if ratio >= 0.8 { p.good } else if ratio >= 0.6 { p.warn } else { p.bad };
    let gap = if (unl as u16) * 2 <= r.width.saturating_sub(2) { " " } else { "" };
    let mut dots: Vec<Span> = Vec::new();
    for i in 0..unl {
        dots.push(if i < count {
            Span::styled(format!("●{gap}"), Style::new().fg(col))
        } else if i < tracked {
            Span::styled(format!("○{gap}"), Style::new().fg(p.dim))
        } else {
            Span::styled(format!("·{gap}"), Style::new().fg(p.dim))
        });
    }
    let lines = vec![
        Line::from(dots),
        Line::from(vec![
            bold(format!("{count}/{unl} agree"), col),
            dim(format!(" on {}… · {:.0}% · 80% closes a ledger", s(ag, &["tx_hash"]).chars().take(10).collect::<String>(), ratio * 100.0)),
        ]),
        Line::from(vec![label("UNL"), ink(format!("{unl} trusted · {tracked} heard · {} proposals", commas(props)))]),
        Line::from(vec![label("round"), dim(format!("{} · mempool {} · candidate {}", s(c, &["phase"]), u(c, &["mempool_size"]), u(c, &["candidate_set_size"])))]),
    ];
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), r);
}

/// The ledger just applied: its transaction mix as bars, and the fees it burned.
fn draw_ledger_mix(f: &mut Frame, r: Rect, sn: &Snap) {
    let p = pal();
    let ffi = &sn.eng["ffi_verifier"];
    let seq = u(ffi, &["round_ledger_seq"]);
    let n = u(ffi, &["round_tx_count"]);
    let block = panel(vec![bold("THIS LEDGER", p.gold), dim(format!(" {n} txs"))], p.gold);
    let _ = seq;
    let inner = block.inner(r);
    f.render_widget(block, r);
    let mut v: Vec<(String, u64)> = ffi["round_tx_types"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, c)| (k.clone(), c.as_u64().unwrap_or(0)))
        .collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    let max = v.first().map(|x| x.1).unwrap_or(1).max(1);
    let name_w = v.iter().map(|x| x.0.chars().count()).max().unwrap_or(8).min((inner.width as usize).saturating_sub(10));
    let bar_w = (inner.width as usize).saturating_sub(name_w + 5);
    let rows = (inner.height as usize).saturating_sub(2);
    let mut lines: Vec<Line> = v
        .iter()
        .take(rows)
        .map(|(k, c)| {
            let col = if k.starts_with("Offer") || k.starts_with("AMM") {
                p.pink
            } else if k.starts_with("Payment") || k.starts_with("Check") {
                p.feed
            } else if k.starts_with("NFToken") {
                p.violet
            } else {
                p.gold
            };
            let len = ((*c as f64 / max as f64) * bar_w as f64).ceil() as usize;
            Line::from(vec![
                Span::styled(format!("{:<w$}", fit_name(k, name_w), w = name_w), Style::new().fg(col)),
                Span::styled(format!("{c:>3} "), Style::new().fg(p.ink)),
                Span::styled("▆".repeat(len.max(1)), Style::new().fg(col)),
            ])
        })
        .collect();
    while lines.len() < rows {
        lines.push(Line::from(""));
    }
    let fees = u(ffi, &["round_fees_drops"]);
    let total = u(ffi, &["total_fees_burned_drops"]);
    lines.push(Line::from(vec![dim("burned "), sp(format!("{} drops", commas(fees)), p.gold)]));
    lines.push(Line::from(dim(format!("{:.3} XRP since start", total as f64 / 1e6))));
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_footer(f: &mut Frame, r: Rect, sn: &Snap, paused: bool) {
    let p = pal();
    let mut v = vec![dim(" q quit · p pause · 1 s refresh · counts since the validator started · xrpl-watch, the Rust dashboard")];
    if paused {
        v.push(bold("   ⏸ PAUSED", p.warn));
    }
    if let Some(e) = &sn.api_err {
        v.push(sp(format!("   API: {}", e.chars().take(60).collect::<String>()), p.bad));
    }
    f.render_widget(Paragraph::new(Line::from(v)), r);
}

// ---------------------------------------------------------------------------------------------------------------
// ANSI frames (for --once and --record)

fn ansi_color(c: Color, fg: bool) -> Option<String> {
    let base = if fg { 30 } else { 40 };
    let n = match c {
        Color::Reset => return None,
        Color::Black => base,
        Color::Red => base + 1,
        Color::Green => base + 2,
        Color::Yellow => base + 3,
        Color::Blue => base + 4,
        Color::Magenta => base + 5,
        Color::Cyan => base + 6,
        Color::Gray => base + 7,
        Color::DarkGray => base + 60,
        Color::LightRed => base + 61,
        Color::LightGreen => base + 62,
        Color::LightYellow => base + 63,
        Color::LightBlue => base + 64,
        Color::LightMagenta => base + 65,
        Color::LightCyan => base + 66,
        Color::White => base + 67,
        Color::Rgb(r, g, b) => return Some(format!("{};2;{r};{g};{b}", if fg { 38 } else { 48 })),
        Color::Indexed(i) => return Some(format!("{};5;{i}", if fg { 38 } else { 48 })),
    };
    Some(n.to_string())
}

fn buffer_to_ansi(buf: &Buffer) -> String {
    let mut out = String::new();
    let a = buf.area;
    for y in a.top()..a.bottom() {
        let mut last: Option<(Color, Color, Modifier)> = None;
        for x in a.left()..a.right() {
            let cell = &buf[(x, y)];
            let st = (cell.fg, cell.bg, cell.modifier);
            if last != Some(st) {
                let mut codes = vec!["0".to_string()];
                if cell.modifier.contains(Modifier::BOLD) {
                    codes.push("1".to_string());
                }
                if cell.modifier.contains(Modifier::DIM) {
                    codes.push("2".to_string());
                }
                if let Some(c) = ansi_color(cell.fg, true) {
                    codes.push(c);
                }
                if let Some(c) = ansi_color(cell.bg, false) {
                    codes.push(c);
                }
                out.push_str(&format!("\x1b[{}m", codes.join(";")));
                last = Some(st);
            }
            out.push_str(cell.symbol());
        }
        out.push_str("\x1b[0m\n");
    }
    out
}

fn arg_value(args: &[String], k: &str) -> Option<String> {
    args.iter().position(|a| a == k).and_then(|i| args.get(i + 1).cloned())
}

fn parse_size(args: &[String]) -> (u16, u16) {
    let v = arg_value(args, "--size").unwrap_or_else(|| "160x48".to_string());
    let (w, h) = v.split_once('x').unwrap_or(("160", "48"));
    (w.parse().unwrap_or(160), h.parse().unwrap_or(48))
}

// ---------------------------------------------------------------------------------------------------------------

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        let doc: Vec<&str> = include_str!("main.rs").lines().take_while(|l| l.starts_with("//!")).collect();
        for l in doc {
            println!("{}", l.trim_start_matches("//!").strip_prefix(' ').unwrap_or(l.trim_start_matches("//!")));
        }
        return Ok(());
    }
    let basic = args.iter().any(|a| a == "--basic")
        || std::env::var("COLORTERM").map(|v| !(v.contains("truecolor") || v.contains("24bit"))).unwrap_or(false) && std::env::var("XRPL_WATCH_TRUECOLOR").is_err();
    let _ = PAL.set(if basic { basic_pal() } else { truecolor_pal() });
    let mut cfg = Cfg::from_env();
    if let Some(s) = arg_value(&args, "--interval").and_then(|v| v.parse::<f64>().ok()) {
        cfg.interval = Duration::from_secs_f64(s.max(0.2));
    }
    let cfg = Arc::new(cfg);
    let shared = Arc::new(Mutex::new(Shared {
        snap: Snap::default(),
        events: VecDeque::new(),
        apply_ms: VecDeque::new(),
        tape: VecDeque::new(),
        closes: VecDeque::new(),
    }));

    let once = args.iter().any(|a| a == "--once");
    let record = arg_value(&args, "--record");
    if once || record.is_some() {
        let (w, h) = parse_size(&args);
        let frames: usize = if once { 1 } else { arg_value(&args, "--frames").and_then(|v| v.parse().ok()).unwrap_or(60) };
        let left = Arc::new(Mutex::new(frames));
        {
            let (c, s2, l) = (cfg.clone(), shared.clone(), left.clone());
            std::thread::spawn(move || collect(c, s2, Some(l)));
        }
        let mut term = Terminal::new(TestBackend::new(w, h)).map_err(io::Error::other)?;
        if let Some(dir) = &record {
            fs::create_dir_all(dir)?;
        }
        let t_start = Instant::now();
        while shared.lock().unwrap().snap.at.is_none() && t_start.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(50));
        }
        for i in 1..=frames {
            let tick = Instant::now();
            {
                let g = shared.lock().unwrap();
                term.draw(|f| draw(f, &g, false)).map_err(io::Error::other)?;
            }
            let ansi = buffer_to_ansi(term.backend().buffer());
            match &record {
                Some(dir) => {
                    fs::write(format!("{dir}/frame_{i:05}.ans"), &ansi)?;
                    eprint!("\rframe {i}/{frames}");
                }
                None => io::stdout().write_all(ansi.as_bytes())?,
            }
            *left.lock().unwrap() -= 1;
            if i < frames {
                let spent = tick.elapsed();
                if spent < cfg.interval {
                    std::thread::sleep(cfg.interval - spent);
                }
            }
        }
        if record.is_some() {
            eprintln!();
        }
        return Ok(());
    }

    {
        let (c, s2) = (cfg.clone(), shared.clone());
        std::thread::spawn(move || collect(c, s2, None));
    }
    let mut terminal = ratatui::init();
    let mut paused = false;
    let mut frozen: Option<Shared> = None;
    let res = (|| -> io::Result<()> {
        loop {
            {
                let g = shared.lock().unwrap();
                if paused {
                    let fz = frozen.get_or_insert_with(|| g.clone());
                    terminal.draw(|f| draw(f, fz, true))?;
                } else {
                    frozen = None;
                    terminal.draw(|f| draw(f, &g, false))?;
                }
            }
            if event::poll(Duration::from_millis(250))? {
                if let Event::Key(k) = event::read()? {
                    if k.kind == KeyEventKind::Press {
                        match k.code {
                            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                            KeyCode::Char('p') => paused = !paused,
                            _ => {}
                        }
                    }
                }
            }
        }
    })();
    ratatui::restore();
    res
}
