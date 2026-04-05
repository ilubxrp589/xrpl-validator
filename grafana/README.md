# Grafana Dashboard — XRPL FFI

`xrpl-ffi-dashboard.json` — imports into Grafana via Dashboards → Import.

## Prerequisites

- Prometheus scraping the FFI `/metrics` endpoint (port 3778 by default).
- Grafana with a Prometheus data source named as the `DS_PROMETHEUS` variable at import time.

### Minimal Prometheus scrape config

```yaml
scrape_configs:
  - job_name: xrpl-ffi
    scrape_interval: 15s
    static_configs:
      - targets:
          - localai:3778     # or m3060:3778, etc
```

## Panels

| # | Panel | Query |
|---|-------|-------|
| 1 | **MAINNET AGREEMENT** | `xrpl_ffi_mainnet_agreement_ratio` (goal: 1.0) |
| 2 | Apply Totals | `xrpl_ffi_apply_attempted_total`, `…_total{result="diverged"}`, `…_ledgers_applied_total` |
| 3 | Position | `xrpl_ffi_live_ledger_seq`, `xrpl_ffi_build_info` |
| 4 | Apply Rate by Result | `rate(xrpl_ffi_apply_total[1m])` stacked |
| 5 | Apply Latency (p50/p95/p99) | `histogram_quantile(0.99, rate(xrpl_ffi_apply_duration_seconds_bucket[1m]))` |
| 6 | TER Distribution | `topk(10, rate(xrpl_ffi_apply_ter_total[1m]))` stacked |
| 7 | Apply Rate by Tx Type | `topk(10, rate(xrpl_ffi_apply_by_type_total[1m]))` stacked |
| 8 | RPC Provider Hit Rate | `hits / (hits + misses)` |
| 9 | Divergences table | `topk(20, xrpl_ffi_diverged_total)` — should be empty |

The dashboard has a single `instance` template variable for filtering when multiple FFI nodes are scraped.
