# Lockstep M1 — Shadow Harness — Design Spec

**Date:** 2026-05-31
**Project:** Lockstep state computation (so the validator can sign its OWN in-round-computed `ledger_hash` — VALAUDIT phase-5 done properly).
**Milestone:** M1 of 5. See `~/Desktop/xrpl-validator-lockstep-research-2026-05-31.md` for the feasibility research that motivates this.

## Why M1 exists
Full lockstep (compute `account_hash(N)` in-round at close) is gated on reproducing rippled's canonical tx ordering + pseudo-txs — a months-scale, determinism-critical effort. We will not attempt that blind. M1 builds the **measurement scaffold** first: a fully isolated shadow loop that runs the in-round `snapshot → apply → hash → diff` pipeline every ledger and reports a **match-ratio meter**. M2–M4 then iterate the hard ordering/pseudo-tx work *against that meter*; M5 promotes to signing.

## Goals
- Prove the shadow `snapshot → FFI-apply → hash → diff-vs-network` pipeline is correct and produces a byte-exact `ledger_hash` when fed a **known-correct tx ordering**.
- Establish the instrumentation everything else iterates against: a `xrpl_lockstep_shadow_match_ratio` gauge, a divergence log, and a per-stage timing histogram.
- Empirically confirm the apply + hash cost fits inside the ~3.5 s consensus round.

## Non-goals (M1)
- **No in-round canonical ordering.** M1 sources the ordered tx set from the RPC `ledger` call (the same known-correct order the trailing path uses). Reproducing the order in-round is **M2**.
- **No pseudo-tx / protocol-singleton reproduction** (M3).
- **No signing or `state.rocks` impact.** Observation only.
- **No promotion to phase-5** (M5).

## Scoping rationale
By feeding M1 the correct ordering, the shadow ratio should sit at ~100%. That is intentional: a ~100% baseline *validates the harness itself* (apply + hash + diff + snapshot timing all correct) before we introduce the hard ordering problem. When M2 swaps RPC ordering for in-round reproduction, the ratio drop is a clean, quantified measure of the remaining work.

## Architecture
- **New module** `crates/xrpl-node/src/lockstep.rs` — the harness orchestration + the pure attempt function + the rolling-ratio metric.
- **Spawned** as an async task from `live_viewer.rs`, **gated by env `XRPL_LOCKSTEP_SHADOW`** (default off — same opt-in pattern as the FFI shadow / tx-gate). When off, zero code runs and there is no behavior change.
- **Trigger:** per validated/closed ledger N (driven off the same WS/peer close signal the existing loops use). The harness runs entirely on its own task; it never blocks the live signing or ws-sync loops.

## Components (each one purpose, testable boundary)
1. **`attempt_ledger(...) -> LockstepAttempt`** — the testable core. Given `(seq, header fields, ordered tx blobs, a pre-ledger DB snapshot, the network-reported account_hash + ledger_hash)`, it: applies the txs via `apply_ledger_in_order`, computes our `account_hash` (FlatHasher over the overlay) and `ledger_hash` (`compute_ledger_hash`), and returns `{ our_account_hash, our_ledger_hash, matched: bool, stage_timings }`. Pure given its inputs → unit-testable exactly like `verify_ledger_for_signing`.
2. **`LockstepShadow`** — the orchestrator/task. Owns the loop: on ledger N, fetch N's ordered tx set + header + validated hashes (RPC, M1), take the pre-N snapshot, call `attempt_ledger`, then record the result. Depends on: a `rocksdb::DB` handle (snapshot), an RPC client, the FFI verifier, and the metric/log sinks.
3. **Rolling match-ratio metric** — reuse/generalize the `ledger_hash_window` (VecDeque<bool>, last 1000) already added for phase-5; expose `xrpl_lockstep_shadow_match_ratio` via the existing `rpc/metrics.rs` exposition.
4. **Divergence log** — reuse the `DivergenceLog` pattern: on mismatch, append `(seq, our_account_hash, net_account_hash, our_ledger_hash, net_ledger_hash, stage timings)` to a `lockstep-divergences.jsonl` for offline analysis.
5. **Timing histogram** — record snapshot/apply/hash durations (reuse the FFI `APPLY_LATENCY_BUCKETS_MS` style) to verify the round budget.

## Data flow (per ledger N)
1. Close signal for N arrives.
2. Fetch N's `ledger` (RPC): ordered txs (by `TransactionIndex`), header fields, network `account_hash` + `ledger_hash`.
3. Take a pre-N DB snapshot (`OwnedSnapshot`).
4. `attempt_ledger(...)` → our `account_hash` + `ledger_hash` + timings.
5. Diff our `ledger_hash` vs network's → matched? Record into the rolling window, the gauge, and (on mismatch) the divergence log.

## Error handling (fail-safe, observation-only)
- Any failure (RPC fetch, snapshot, FFI apply, hash) → log + count the ledger as **skipped** (a distinct counter; *not* a mismatch — mismatches mean "we computed a different hash," skips mean "we couldn't compute one"). Never panic.
- The harness must **never** write `state.rocks`, **never** sign, and **never** block or slow the live loops. It runs on its own task at low priority; if it falls behind, it drops ledgers rather than back-pressuring anything.
- Bounded resource use: the divergence log is append-only with rotation; the rolling window is fixed-size.

## Testing
- **Unit (no ffi, runs on .39):** `attempt_ledger` with constructed inputs — correct inputs → `matched=true`; a perturbed header field or a dropped/reordered tx → `matched=false`. Plus the rolling-ratio edge cases (empty, mixed, window roll) — extend the existing phase-5 ratio tests.
- **Integration (m3060, --features ffi, env-gated):** run the shadow against live mainnet with `XRPL_LOCKSTEP_SHADOW=1`. **Success criteria:** match ratio ≥ ~99% over a day (proving the pipeline with RPC ordering), and the timing histogram shows snapshot+apply+hash comfortably within the ~3.5 s round. Any persistent <100% with correct ordering is a harness bug to fix before M2.

## Risks
- **State.rocks completeness:** the shadow apply needs a complete pre-N snapshot; gaps force FFI→RPC fallback (budgeted, same as today's apply). M1 inherits the existing completeness guarantees; if gaps surface, the divergence log will show them.
- **Snapshot alignment:** the snapshot must be the pre-N state. M1 validates this alignment empirically (a misaligned snapshot shows up as a mismatch even with correct ordering).
- **Added RPC load:** M1 adds one `ledger` fetch per ledger to local rippled (acceptable; ~tens of ms, ledgers are ~3.5 s apart). Mergeable with the ws-sync fetch later if needed.

## Definition of done (M1)
Env-gated shadow harness merged; unit tests green; a day+ of mainnet shadow run on m3060 at ≥99% match ratio with RPC ordering; timing histogram confirms the round budget; divergence log + gauge live on `/metrics`. Phase-3 remains the production signing gate throughout — M1 changes no production behavior.
