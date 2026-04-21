# Halcyon XRPL Validator — Progress Log

A running record of what's been built, what's been tried and didn't work,
and what's still broken. Every claim below is anchored to a commit hash
or a linked file — verifiable by anyone reading the repo.

**Source of truth:** `git log` on this repository. If something here
can't be verified against git history or the `audit/` folder, it
shouldn't be here.

---

## Known issues — in flight (as of 2026-04-21)

These are real, acknowledged, and being worked. Posted here first so no
reader has to piece them together from commits.

| Finding | Severity | Status |
|---|---|---|
| README overclaims "no code from rippled" — it's hybrid Rust + libxrpl FFI | SEV-1 | Rewrite queued as VALAUDIT Phase 10 |
| Validation signing not gated on independent verification (`live_viewer.rs:1182-1206` signs network hash regardless) | SEV-2 | VALAUDIT Phase 3 next |
| UNL publisher signature not verified (`unl_fetch.rs::fetch_unl`) | SEV-2 | VALAUDIT Phase 6 |
| Validator seed written with default umask; permission check only warns | SEV-2 | VALAUDIT Phase 4 |
| Master + signing keys derived from same 16-byte seed (`peer/identity.rs:49-62`) | SEV-2 | VALAUDIT Phase 9 — biggest single piece |
| `state.rocks` completeness gap — some owner-dir-referenced SLEs missing after bulk_sync | — | Narrow retry fix shipped (commit `6dd8f22`); proactive sweeper still pending |
| `LayeredProvider::succ()` + `RpcProvider::succ()` return `None` unconditionally (test-only path; production uses `OverlayedDbProvider`) | — | Separate card — needs RPC directory-walk helper |

Third-party audit: 24 findings files at `audit/` (if copied in) or USB
ENFAIN. SEV-1 and SEV-2 items above are from `audit/00_EXECUTIVE_SUMMARY.md`.

---

## 2026-04-21 — VALAUDIT Phase 2 + two production bug fixes

Spent the day on the audit integration track (Phase 2) and two bugs that
surfaced during the 24-hour watch after Phase 1's deploy.

- **Phase 2: release-safety test guard** (commit `cbda593`) — CI test
  that fails if any committed config ships an uncommented `[validation_seed]`,
  `[validator_token]`, hardcoded non-local IPv4, or a raw seed file.
  Adapted from xLedgRS's equivalent guard. No-op on current tree
  (no `cfg/` directory exists yet), forward-looking.

- **Bug fix: `OverlayedDbProvider::succ()` overlay consultation**
  (commit `69bc726`) — the prior `succ()` iterated only the RocksDB
  snapshot. If an earlier tx in the same ledger had modified or deleted
  an SLE in a directory the current tx was walking (offer-book traversal),
  libxrpl saw a stale RocksDB neighbor and crossed wrong offers. Three
  cascade divergences in mainnet ledger 103,679,277 correlated with
  32 directories touched by multiple Offer txs in that ledger. Fix
  collects both snapshot and overlay candidates, returns the smaller,
  skips tombstoned keys. 126/126 lib tests pass; new regression test
  covers tombstone + insert + bounds cases.

- **Bug fix: `tefBAD_LEDGER` AccountDelete recovery** (commit `6dd8f22`) —
  when AccountDelete walks an owner directory and hits a missing SLE
  (state.rocks completeness gap from bulk_sync), the fatal message names
  the missing key. Now parses the key, RPC-fetches at ledger_seq-1,
  injects into overlay, retries apply. 100% of observed Payment
  `terPRE_SEQ` divergences in the post-fix run correlated with an
  AccountDelete `tefBAD_LEDGER` in the same ledger (39/39 match) — so
  this one fix closes two cascade patterns. Caveat: does not fix the
  root state-gap cause, just recovers per-tx. Proactive sweeper still
  needed.

**Self-inflicted detour:** attempted a hot-swap deploy (no state.rocks
wipe) after `6dd8f22`. Validator instantly showed 115 divergences. Had
to do a proper wipe+resync. Saved memory note updated: "there is no
such thing as a safe no-wipe restart." Third re-violation on record.

---

## 2026-04-19 — VALAUDIT Phase 1 + succ() root-cause fix

Audit integration started. A third-party audit flagged the validator's
public README as claiming "no code from rippled" while the actual
architecture is hybrid Rust + libxrpl via FFI shim. Also surfaced
several correctness findings.

- **Phase 1: honesty pass** (commit `6775e43`) — 23 files changed.
  Fixed the stale `state_hash.rs` doc claiming "256-bucket hasher" (actual
  `NUM_BUCKETS=65536`). Resolved a LICENSE/Cargo conflict (LICENSE said
  proprietary, all three crate `Cargo.toml` files said MIT; flipped them
  to `LicenseRef-Proprietary`). Deleted an obsolete `ffi/Makefile` that
  the Rust `build.rs` couldn't consume. Added a DEAD CODE WARNING banner
  to 17 files containing ~6,800 lines of Rust transactor code that's
  gated behind `if false` at `live_viewer.rs:1049` — the production tx
  path goes through the libxrpl FFI shim, not these modules.

- **succ() implementation** (commit `0ba4b0c`) — the C++ shim's
  `CallbackReadView::succ()` was hardcoded to `nullopt`. When
  TicketCreate (and other tx types) checked owner directory capacity
  via successor-key traversal, they saw "no more pages" and either
  failed with `tec*` or produced wrong mutations. Cascaded to
  `terPRE_SEQ` for downstream txs from the same account. The fix wires
  a real `succ_fn` callback through Rust → C FFI → libxrpl's
  `CallbackReadView`; the Rust side walks the RocksDB snapshot. Root
  cause of 66 of the pre-audit divergences. (The 2026-04-21
  `OverlayedDbProvider::succ()` fix closes the overlay-consultation
  gap this commit didn't cover.)

---

## 2026-04-16 — Every-ledger hash verification + fast-path drift fix

Previously the validator only hash-checked at batch tips (every N ledgers);
gap-fill ledgers between tips were assumed correct. Commit `4f2122c`
turned on per-ledger hash verification and per-stage timing. This
immediately surfaced a recurring ~150-ledger mismatch cascade.

Root cause (commit `5957491`): the fast-path catchup updated `state.rocks`
but NOT the `FlatHasher`'s in-memory bucket cache. The hasher drifted from
the DB. Fixed by adding an `update_only` method that refreshes the hasher
without triggering a root recompute.

Also this day:
- `6cc3fb6` — fetch `account_hash` for every ledger (not just batch tail)
- `a779791` — stopped doing retry-and-full-scan on mismatch; just log and
  move on. Full-scans mask the real bug and cost minutes of compute.
- `e80aaad` / `5be9359` — overlay-diff dump on every shadow mismatch for
  diagnostic visibility.

---

## 2026-04-15 — libxrpl port to 3.1.2 + Stage 3 scaffold + 100k milestone

Biggest single-day push. Rebuilt the entire FFI shim against rippled
stable 3.1.2 after discovering the previous target (develop branch
3.2.0-b0) had an unstable `Application` API that caused amendment-apply
divergences.

Shim rebuild chain (16 commits, from `15840ec` up through `fc1947d`):
port to 3.1.2's new `Application` API, rename `header()` → `info()` for
the 3.1.2 ReadView interface, provide a `MinimalApp` stub for the
unused subsystems, exclude `Application.cpp` / `BookListeners.cpp` /
`Ledger.cpp` that pull in overlay+consensus+RPC deps we don't need,
add link paths for `ed25519-donna` and `secp256k1` from rippled's
`external/`, skip conan's OpenSSL (version conflict with system 3.x),
fix proto include path to `pb-xrpl.libpb`. The `MinimalApp` approach
lets us link the ~700k-LOC libxrpl as a library without dragging in
its service infrastructure.

Also this day:
- `d66055d` — Stage 3 scaffold: FFI overlay as primary source of truth
  for state writes, RPC fallback only for 5 protocol singletons. Behind
  `XRPL_FFI_STAGE3` env toggle, off by default.
- `7bae420` — New Next.js 15 real-time dashboard scaffold (deploys to
  halcyon-names.io). Replaces the prior static HTML dashboard.
- `448ba95` + `a62144b` — Reproducer test for the TicketCreate cascade
  bug in mainnet ledger 103,515,367. Downloads 67 tx blobs and replays
  via `apply_ledger_in_order` against s2.ripple.com RPC. Captures the
  cascade deterministically for offline debugging.
- `670f3c7` — 100k milestone: dashboard polish, system-metrics sidecar.

---

## 2026-04-10 → 2026-04-13 — Stage 3 prep + dashboard foundations

- `9d9f694` — Stage 3 prep: `[stage3]` config, batch helpers, hybrid
  singleton model for the 5 protocol SLEs that libxrpl doesn't emit
  through pseudo-tx mutation data.
- `0f8dcd7` — bulk_sync now verifies its built SHAMap root against
  rippled's `account_hash` for the seed ledger before writing
  `dl_done.txt`. Partial syncs refuse to mark themselves complete, so
  the next startup re-runs automatically.
- `2d8ecca` — centralized data paths in `xrpl_node::paths` module.
  Previously `/mnt/xrpl-data/...` was hardcoded in several places; now
  respects `XRPL_DATA_DIR` consistently.
- `e31417a` + earlier — Iterative dashboard work: cyberpunk PCB consensus
  board, sparkline transaction flow, per-ledger fee extraction.

---

## 2026-04-07 — Stage 2 shadow-hash verification lands

First version of running the FFI engine as a _shadow_ of the RPC-based
state writes, so we could see where libxrpl's output diverged from
mainnet's canonical `AffectedNodes` without affecting the validator's
signed state. Enabled systematic divergence hunting.

- `1a7c435` — Stage 2: shadow state hash from FFI mutations
- `a2feb29` — Shadow-hash must snapshot the hasher BEFORE process_ledger
  writes (fixed a double-apply bug)
- `bf46baf` — Merge protocol-level state changes into shadow overlay
- `1eaf2a1` — Shadow hash uses RPC canonical `node_binary` (fixes a
  serialization mismatch — we were re-encoding FFI mutations and drifting
  on edge cases like AMM bytes)
- `c88e6a8` — `terPRE_SEQ` recovery: fetch sender AccountRoot at current
  ledger when a tx hits terPRE_SEQ, inject into overlay, break downstream
  cascade. Covers simple cascades; the 2026-04-21 succ+recovery work
  closes deeper classes.
- `915782c` — Skip shadow hash until 5 consecutive state-hash matches.
  Prevents false mismatches during the settle window after bulk_sync.
- `9efb8cc` — UI: hero banner shows ours-vs-network shadow hash hex with
  match indicator.

---

## What's next

- **VALAUDIT Phase 3** — gate signing on independent verification.
  Queued once the current 20k-ledger watch (for commits `69bc726` +
  `6dd8f22`) completes cleanly. ~2h code + 7-day mainnet watch.
- Phases 4-10 then follow the audit roadmap — seed hygiene, independent
  ledger_hash signing, UNL signature verification, dynamic amendments,
  forensic divergence bundles, master/signing key split, README + docs
  rewrite.

Audit roadmap estimates 3-4 months for all 10 phases. We're a few days in.
