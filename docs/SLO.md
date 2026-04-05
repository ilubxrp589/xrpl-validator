# Service Level Objectives — libxrpl FFI Engine

These define the production targets for the FFI tx-application engine. They are consumed by the alert rules in `alerts/xrpl-ffi.rules.yml` and track against the Prometheus metrics exposed at `/metrics`.

## SLOs

### 1. Correctness — zero tolerance

**Target:** 100% mainnet agreement on every apply call. Error budget = 0.

**Metric:** `xrpl_ffi_mainnet_agreement_ratio` (gauge)
**Definition:** `(tesSUCCESS + tec_claimed) / attempted` — the fraction of FFI apply results that match what rippled returned for the same tx on mainnet.
**Condition:** any single divergence is a hard failure. Pages on-call immediately.

This is the primary product guarantee: if we diverge from mainnet, the engine is wrong and any validation we sign is suspect. There is no budget for divergences.

### 2. Availability

**Target:** 99.95% uptime (≈ 21 m downtime / month).

**Metric:** liveness probe to `/health` + scrape success on `/metrics`.
**Alert:** `up{job="xrpl-ffi"} == 0 for 2m` → page.

### 3. Apply latency

**Target:** p99 apply duration < 100 ms over 5-minute windows.

**Metric:** `xrpl_ffi_apply_duration_seconds` (histogram)
**Alert:** `histogram_quantile(0.99, rate(xrpl_ffi_apply_duration_seconds_bucket[5m])) > 0.1 for 10m` → warn.

Latency over this threshold usually means RPC backing store (rippled) is struggling, or we're network-starved.

### 4. RPC provider efficiency

**Target:** ≥ 90% hit rate on SLE lookups.

**Metric:** `xrpl_ffi_rpc_sle_lookups_total` (counter by result).
**Alert:** `rate(miss) / rate(hit + miss) > 0.15 for 5m` → warn.

Miss rate spikes usually mean rippled pruned history we needed, or we're querying at a stale pre-ledger index.

### 5. Ledger lag

**Target:** last-applied ledger within 16 s (≈ 4 validated ledgers) of mainnet head.

**Metric:** `xrpl_ffi_live_ledger_seq` (gauge, set to most recent applied ledger).
**Alert:** (requires external source of current mainnet head)
`(xrpl_mainnet_head_seq - xrpl_ffi_live_ledger_seq) * 4 > 16 for 5m` → warn.

## Alerting stance

- **Page** on correctness or availability breaches (SLO 1 & 2).
- **Warn** on latency / miss / lag breaches (SLO 3, 4, 5).
- Correctness breach = capture the tx hash from `logs/divergences.jsonl`, look it up on mainnet, reproduce locally before merging any revert.

## Verification surfaces beyond metrics

- `logs/divergences.jsonl` — append-only, every disagreement is a line with
  `{ts, ledger_seq, tx_hash, tx_type, our_ter, duration_ms, fatal}`. Tail it
  during incidents.
- Structured JSON logs to stderr (via `tracing`, controlled by `LOG_FORMAT=json|text`).
- `/api/ffi-status` — JSON snapshot of internal counters, for humans and the
  dev CLI dashboard.

## Why zero budget for correctness

A validator is a cryptographic witness. If it signs a state hash derived from
an incorrect tx engine, the signature certifies the wrong thing. Unlike
availability (we can tolerate downtime) or latency (we can tolerate slowness),
correctness cannot be partially satisfied without becoming a security issue.

We hold zero budget because the invariant is provable: the same tx, applied
to the same pre-state, through the same C++ engine, MUST produce the same
result as rippled did on mainnet. Any divergence is a bug in our FFI layer,
our state provider, or our amendment/ledger-header construction — never an
acceptable tail event.
