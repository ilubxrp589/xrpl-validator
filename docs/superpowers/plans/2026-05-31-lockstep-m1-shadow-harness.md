# Lockstep M1 — Shadow Harness — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An env-gated shadow harness that, each ledger, recomputes the full `ledger_hash` from our independently-applied state and diffs it against the network's — reporting a match-ratio meter — without touching signing or `state.rocks`.

**Architecture:** Compose the existing FFI-shadow machinery. The `XRPL_FFI_STAGE3` path in `ws_sync.rs` already takes a pre-ledger `OwnedSnapshot`, applies the ledger's tx set via FFI, and computes a shadow `account_hash`. M1 adds, behind a new `XRPL_LOCKSTEP_SHADOW` flag: (1) extend `LedgerHeader` with the remaining hash inputs, (2) at the shadow hook, feed our shadow `account_hash` + header into `compute_ledger_hash`, (3) diff vs the network `ledger_hash`, (4) record a rolling match-ratio gauge + divergence log. M1 uses the RPC-sourced (known-correct) tx ordering — proving the pipeline before M2 reproduces ordering in-round.

**Tech Stack:** Rust, libxrpl FFI, RocksDB snapshots, parking_lot, the existing `state_hash` FlatHasher + `rpc/metrics.rs` Prometheus exposition.

**Spec:** `docs/superpowers/specs/2026-05-31-lockstep-m1-shadow-harness-design.md`

---

### Task 1: `lockstep` module — pure ledger-hash evaluation

**Files:**
- Create: `crates/xrpl-node/src/lockstep.rs`
- Modify: `crates/xrpl-node/src/lib.rs` (add `pub mod lockstep;` next to the other `pub mod` lines)
- Test: `crates/xrpl-node/tests/lockstep.rs`

- [ ] **Step 1: Write the failing test**

```rust
// crates/xrpl-node/tests/lockstep.rs
//! VALAUDIT lockstep M1 — shadow harness unit tests.
use xrpl_node::lockstep::{evaluate_ledger, LockstepOutcome};
use xrpl_node::state_hash::compute_ledger_hash;

const SEQ: u32 = 104_600_000;
const DROPS: u64 = 99_987_000_000;
const PARENT: [u8; 32] = [0x11; 32];
const TXH: [u8; 32] = [0x22; 32];
const ACCT: [u8; 32] = [0x33; 32];
const PCT: u32 = 770_000_000;
const CT: u32 = 770_000_003;
const RES: u8 = 10;
const FLAGS: u8 = 0;

#[test]
fn matches_when_our_hash_equals_network() {
    let net = compute_ledger_hash(SEQ, DROPS, &PARENT, &TXH, &ACCT, PCT, CT, RES, FLAGS).0;
    let out = evaluate_ledger(SEQ, DROPS, &PARENT, &TXH, &ACCT, PCT, CT, RES, FLAGS, &net);
    assert!(out.matched, "correct inputs must match");
    assert_eq!(out.our_ledger_hash, net, "our hash must equal the network hash on a match");
}

#[test]
fn mismatch_when_a_header_field_is_wrong() {
    let net = compute_ledger_hash(SEQ, DROPS, &PARENT, &TXH, &ACCT, PCT, CT, RES, FLAGS).0;
    // close_time off by one -> different hash -> no match.
    let out = evaluate_ledger(SEQ, DROPS, &PARENT, &TXH, &ACCT, PCT, CT + 1, RES, FLAGS, &net);
    assert!(!out.matched, "a wrong close_time must not match");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd /home/localai/Desktop/xrpl-validator && cargo test -p xrpl-node --test lockstep`
Expected: FAIL — `error[E0432]: unresolved import xrpl_node::lockstep`.

- [ ] **Step 3: Write minimal implementation**

```rust
// crates/xrpl-node/src/lockstep.rs
//! VALAUDIT lockstep (M1 — shadow harness).
//!
//! Pure evaluation of whether our independently-applied ledger reproduces the
//! network's `ledger_hash`. Used by the env-gated shadow harness in `ws_sync`.
//! M1 feeds it our shadow account_hash + the network's header fields; M2+ will
//! replace the RPC-sourced ordering that produced that account_hash.
use crate::state_hash::compute_ledger_hash;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockstepOutcome {
    pub our_ledger_hash: [u8; 32],
    pub matched: bool,
}

/// Recompute the ledger_hash from header fields + our account_hash and compare
/// to the network-reported ledger_hash.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_ledger(
    ledger_seq: u32,
    total_drops: u64,
    parent_hash: &[u8; 32],
    tx_hash: &[u8; 32],
    account_hash: &[u8; 32],
    parent_close_time: u32,
    close_time: u32,
    close_resolution: u8,
    close_flags: u8,
    network_ledger_hash: &[u8; 32],
) -> LockstepOutcome {
    let our = compute_ledger_hash(
        ledger_seq, total_drops, parent_hash, tx_hash, account_hash,
        parent_close_time, close_time, close_resolution, close_flags,
    );
    LockstepOutcome {
        our_ledger_hash: our.0,
        matched: our.0 == *network_ledger_hash,
    }
}
```

Then add to `crates/xrpl-node/src/lib.rs`: `pub mod lockstep;` (place it alphabetically among the existing `pub mod` declarations — open the file and match the existing style).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p xrpl-node --test lockstep`
Expected: PASS — 2 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/xrpl-node/src/lockstep.rs crates/xrpl-node/src/lib.rs crates/xrpl-node/tests/lockstep.rs
git commit -m "feat(lockstep): pure ledger-hash evaluation (M1 task 1)"
```

---

### Task 2: `LockstepMeter` — rolling match-ratio window

**Files:**
- Modify: `crates/xrpl-node/src/lockstep.rs`
- Test: `crates/xrpl-node/tests/lockstep.rs`

- [ ] **Step 1: Write the failing test** (append to `tests/lockstep.rs`)

```rust
use xrpl_node::lockstep::LockstepMeter;

#[test]
fn meter_ratio_is_one_with_no_mismatches() {
    let m = LockstepMeter::new();
    for _ in 0..10 { m.record(true); }
    assert_eq!(m.match_ratio(), 1.0);
    assert_eq!(m.observed(), 10);
}

#[test]
fn meter_ratio_drops_and_window_rolls() {
    let m = LockstepMeter::new();
    for _ in 0..3 { m.record(true); }
    m.record(false);
    assert!((m.match_ratio() - 0.75).abs() < 1e-9); // 3 of 4
    let m2 = LockstepMeter::new();
    m2.record(false);
    for _ in 0..1000 { m2.record(true); }
    assert_eq!(m2.match_ratio(), 1.0, "old mismatch rolls out of the 1000-window");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p xrpl-node --test lockstep`
Expected: FAIL — `cannot find type LockstepMeter`.

- [ ] **Step 3: Write minimal implementation** (append to `src/lockstep.rs`)

```rust
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;

/// Rolling window (last 1000) of lockstep match results, for the
/// `xrpl_lockstep_shadow_match_ratio` gauge. Mirrors the phase-5
/// `ledger_hash_window` on StateHashComputer.
#[derive(Clone, Default)]
pub struct LockstepMeter {
    window: Arc<Mutex<VecDeque<bool>>>,
}

impl LockstepMeter {
    pub fn new() -> Self {
        Self { window: Arc::new(Mutex::new(VecDeque::with_capacity(1000))) }
    }
    pub fn record(&self, matched: bool) {
        let mut w = self.window.lock();
        if w.len() >= 1000 { w.pop_front(); }
        w.push_back(matched);
    }
    /// Fraction matched over the window; 1.0 when empty (no divergence yet).
    pub fn match_ratio(&self) -> f64 {
        let w = self.window.lock();
        if w.is_empty() { return 1.0; }
        w.iter().filter(|&&m| m).count() as f64 / w.len() as f64
    }
    pub fn observed(&self) -> usize { self.window.lock().len() }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p xrpl-node --test lockstep`
Expected: PASS — 4 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/xrpl-node/src/lockstep.rs crates/xrpl-node/tests/lockstep.rs
git commit -m "feat(lockstep): rolling match-ratio meter (M1 task 2)"
```

---

### Task 3: Extend `LedgerHeader` with the remaining ledger-hash inputs

The shadow hook has a `LedgerHeader` (parent_hash, parent_close_time, total_drops, transaction_hash) but lacks `close_time`, `close_time_resolution`, `close_flags`, and the network `ledger_hash`. Add them so the hook can call `evaluate_ledger`. (`compute_ledger_hash` is verified correct — a python repro reproduced a mainnet ledger_hash byte-exact from these exact fields.)

**Files:**
- Modify: `crates/xrpl-node/src/ws_sync.rs` — the `LedgerHeader` struct (~line 739) and `fetch_ledger_metadata` (~line 748, where `parent_hash`/`total_coins`/`transaction_hash` are parsed and the struct is built ~line 787).

- [ ] **Step 1: Add the fields to `LedgerHeader`**

Open `crates/xrpl-node/src/ws_sync.rs`, find `pub struct LedgerHeader`, and add four fields:

```rust
    /// VALAUDIT lockstep M1: remaining inputs to compute_ledger_hash.
    pub close_time: u32,
    pub close_resolution: u8,
    pub close_flags: u8,
    pub ledger_hash: [u8; 32],
```

- [ ] **Step 2: Parse them in `fetch_ledger_metadata`**

In `fetch_ledger_metadata`, alongside the existing `let parent_close_time = ledger["parent_close_time"]...` parses, add (use the existing 32-byte hex-decode helper pattern already in that function for `ledger_hash`):

```rust
    let close_time = ledger["close_time"].as_u64().unwrap_or(0) as u32;
    let close_resolution = ledger["close_time_resolution"].as_u64().unwrap_or(0) as u8;
    let close_flags = ledger["close_flags"].as_u64().unwrap_or(0) as u8;
    let ledger_hash = ledger["ledger_hash"].as_str()
        .and_then(|s| hex::decode(s).ok())
        .and_then(|b| if b.len() == 32 { let mut a = [0u8; 32]; a.copy_from_slice(&b); Some(a) } else { None })
        .unwrap_or([0u8; 32]);
```

Then extend the struct construction (`let header = LedgerHeader { ... };`) to include `close_time, close_resolution, close_flags, ledger_hash`.

- [ ] **Step 3: Build-check (no ffi, on .39 — sibling SDK resolves there)**

Run: `cargo check -p xrpl-node --bin live_viewer 2>&1 | tail -3`
Expected: `Finished` (no errors). Fix any field-name mismatch the compiler flags.

- [ ] **Step 4: Commit**

```bash
git add crates/xrpl-node/src/ws_sync.rs
git commit -m "feat(lockstep): carry close_time/resolution/flags/ledger_hash in LedgerHeader (M1 task 3)"
```

---

### Task 4: Wire lockstep evaluation into the FFI-shadow hook (env-gated)

Hook into the existing FFI-shadow block in `ws_sync.rs` (~lines 412–463, the `steady`/`spawn_blocking` path that computes the shadow account_hash via `check_shadow_hash`). When `XRPL_LOCKSTEP_SHADOW=1`, after the shadow `account_hash` is available, evaluate the ledger_hash and record the meter. **Read the surrounding ~60 lines first to confirm the exact local variable names** (`hdr`, `seq`, the shadow account_hash, the verifier `v`), since this composes existing code.

**Files:**
- Modify: `crates/xrpl-node/src/ws_sync.rs` (the shadow hook + a module-level `LockstepMeter` + env-flag read near the ws-sync task setup)
- Modify: `crates/xrpl-node/src/ffi_engine.rs` if `check_shadow_hash` doesn't already return/expose the computed shadow `account_hash` (it currently returns `bool`). If needed, add a sibling that returns `Option<[u8;32]>` (the shadow root) so lockstep can use it; otherwise reuse `StateHashComputer::shadow_hash_from_snapshot` directly on the overlay.

- [ ] **Step 1: Add a process-wide meter + flag**

Near where the ws-sync task initializes (module scope), add:

```rust
use once_cell::sync::Lazy; // confirm once_cell is a dep; else use std::sync::OnceLock
pub static LOCKSTEP_METER: Lazy<crate::lockstep::LockstepMeter> =
    Lazy::new(crate::lockstep::LockstepMeter::new);
```

and read the flag once where other `XRPL_*` flags are read:

```rust
let lockstep_shadow = std::env::var("XRPL_LOCKSTEP_SHADOW").map(|v| v == "1").unwrap_or(false);
```

- [ ] **Step 2: Evaluate + record at the shadow hook**

After the shadow overlay + shadow `account_hash` are computed (inside the `if shadow_ok ...` block where `check_shadow_hash` runs), add — guarded by `lockstep_shadow`:

```rust
if lockstep_shadow {
    // our shadow account_hash for THIS ledger (from the overlay applied to the
    // pre-ledger hasher snapshot) — reuse the value computed for check_shadow_hash,
    // or compute via StateHashComputer::shadow_hash_from_snapshot(hs, &shadow_overlay).
    if let Some(our_acct) = shadow_account_hash {            // [u8;32]
        let out = crate::lockstep::evaluate_ledger(
            seq, hdr.total_drops, &hdr.parent_hash, &hdr.transaction_hash,
            &our_acct, hdr.parent_close_time, hdr.close_time,
            hdr.close_resolution, hdr.close_flags, &hdr.ledger_hash,
        );
        LOCKSTEP_METER.record(out.matched);
        if !out.matched {
            eprintln!("[lockstep] #{seq} MISMATCH ours={} network={}",
                hex::encode(&out.our_ledger_hash[..8]), hex::encode(&hdr.ledger_hash[..8]));
        }
    }
}
```

(If `check_shadow_hash` only returns `bool`, in Step where the shadow hash is computed, also bind the `[u8;32]` root via `StateHashComputer::shadow_hash_from_snapshot(hasher_snap, &shadow_overlay)` and pass it as `shadow_account_hash`.)

- [ ] **Step 3: Build-check with ffi (on m3060)**

Run on m3060: `cd ~/xrpl-validator-ffi && RUSTFLAGS='-C link-arg=-lz' cargo check -p xrpl-node --bin live_viewer --features ffi 2>&1 | tail -3`
Expected: `Finished`. Fix any borrow/Send issues (this block runs inside a spawned task — keep no parking_lot guard across `.await`; here it's sync inside `spawn_blocking`, so fine).

- [ ] **Step 4: Commit**

```bash
git add crates/xrpl-node/src/ws_sync.rs crates/xrpl-node/src/ffi_engine.rs
git commit -m "feat(lockstep): env-gated ledger_hash shadow eval at the FFI hook (M1 task 4)"
```

---

### Task 5: Expose `xrpl_lockstep_shadow_match_ratio` on /metrics

**Files:**
- Modify: `crates/xrpl-node/src/rpc/metrics.rs` — add the field to `ValidatorMetricsSnapshot`, a gauge block in `render_prometheus` (+ format arg), and the field in the internal `render_prometheus_format` test (mirror exactly how `ledger_hash_match_ratio` was added).
- Modify: `crates/xrpl-node/src/bin/live_viewer.rs` — at the snapshot construction (~line 1345), add `lockstep_shadow_match_ratio: xrpl_node::ws_sync::LOCKSTEP_METER.match_ratio(),`.

- [ ] **Step 1: Add the snapshot field** (in `metrics.rs`, after `ledger_hash_match_ratio`):

```rust
    /// VALAUDIT lockstep M1 shadow match ratio (rolling 1000).
    pub lockstep_shadow_match_ratio: f64,
```

- [ ] **Step 2: Render it** — after the `ledger_hash_match_ratio` gauge block, add:

```text
         # HELP xrpl_lockstep_shadow_match_ratio Shadow lockstep ledger_hash match ratio, RPC-ordered (va-05 M1)\n\
         # TYPE xrpl_lockstep_shadow_match_ratio gauge\n\
         xrpl_lockstep_shadow_match_ratio {:.6}\n\
         \n\
```

and add `snap.lockstep_shadow_match_ratio,` to the format args (right after `snap.ledger_hash_match_ratio,`). Add `lockstep_shadow_match_ratio: 1.0,` to the test snapshot and an assert `assert!(output.contains("xrpl_lockstep_shadow_match_ratio 1.000000"));`.

- [ ] **Step 3: Wire the live value** — in `live_viewer.rs` snapshot construction, add the field as above.

- [ ] **Step 4: Test + build-check**

Run: `cargo test -p xrpl-node --lib rpc::metrics` → PASS.
Run on m3060: `RUSTFLAGS='-C link-arg=-lz' cargo check -p xrpl-node --bin live_viewer --features ffi 2>&1 | tail -3` → `Finished`.

- [ ] **Step 5: Commit**

```bash
git add crates/xrpl-node/src/rpc/metrics.rs crates/xrpl-node/src/bin/live_viewer.rs
git commit -m "feat(lockstep): expose xrpl_lockstep_shadow_match_ratio gauge (M1 task 5)"
```

---

### Task 6: Integration run on m3060 (env-gated, no production impact)

- [ ] **Step 1:** Build release on m3060: `cd ~/xrpl-validator-ffi && RUSTFLAGS='-C link-arg=-lz' cargo build --release --bin live_viewer --features ffi` → `Finished`.
- [ ] **Step 2:** Back up current binary (`cp target/release/live_viewer target/release/live_viewer.pre-lockstep`).
- [ ] **Step 3:** Canonical wipe+resync (per `feedback_wipe_resync_on_restart`) but ALSO export `XRPL_LOCKSTEP_SHADOW=1` and `XRPL_FFI_STAGE3=1` (the shadow apply must run). Confirm `.39` rippled is `full`+caught-up first. Phase-3 signing stays unaffected.
- [ ] **Step 4:** After spin-up, watch for ~a day: `curl -s localhost:3777/metrics | grep lockstep_shadow_match_ratio` should climb toward **1.0**; `grep "\[lockstep\] .*MISMATCH" /tmp/lv_ffi.log` should be empty/rare. Persistent <1.0 with RPC ordering = a harness bug (diff the divergence output) — fix before declaring M1 done.
- [ ] **Step 5:** Record the result in `project_valaudit.md` and the taskboard.

**M1 Definition of Done:** ratio ≥ ~0.99 over a day on mainnet with RPC ordering; apply+hash timing within the round; gauge + log live; zero production-behavior change (phase-3 still the signing gate).

---

## Self-review notes
- **Spec coverage:** harness (T1,T4), match-ratio meter (T2,T5), divergence log (T4 eprintln + reuse FfiVerifier's existing dump_overlay_diff), timing (inherited from the FFI-shadow path's existing instrumentation), isolation/env-gating (T4 flag), testing (T1,T2,T5 unit + T6 integration). ✓
- **Known open detail for the executor:** Task 4 depends on binding the shadow `account_hash` as `[u8;32]` at the hook. If `check_shadow_hash` returns only `bool`, compute the root via `StateHashComputer::shadow_hash_from_snapshot(hasher_snap, &shadow_overlay)` (already exists, returns `Option<Hash256>`) and use `.0`. This is the one spot requiring reading the exact hook locals — flagged explicitly, not a placeholder.
- M2 (in-round canonical ordering) will replace the RPC-sourced `tx_blobs`/ordering feeding the shadow apply; the meter from M1 measures its accuracy.
