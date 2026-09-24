# Warm restart for `live_viewer` — design

**Date:** 2026-09-24 · **Status:** approved design, pending implementation · **Crate:** `xrpl-node`

## Goal

A deploy replaces the running `live_viewer` binary. Today every start wipes the sync directory and bulk-downloads
the full ledger state (about 19.9 million objects) from the reference rippled node. That takes 330–410 s on a quiet
source, during which the network advances, so the node starts 90–111 ledgers behind and must catch up. A warm restart
keeps the state across the binary swap and resumes from the exact ledger it stopped at, so a deploy costs one short
restart: a target start gap under 60 ledgers, expected about 15.

Non-goals: loading `leaf_cache.bin` (never read today; the hasher rebuild from the database takes about 22 s, which is
acceptable); changing the cold path; changing crash or halt recovery, which stays a full wipe and resync.

## Why earlier no-wipe restarts failed

Three restarts that kept the state failed (2026-04-15, 2026-04-21, 2026-08-17; the last halted the node on three
consecutive state-hash mismatches). In each, the new process resumed from the wrong ledger: the only restart path,
the `sync_complete.marker` branch, backfills from `dl_done.txt`, which records the original bulk-download ledger and is
never advanced by ws-sync, and it then starts ws-sync from the current validated ledger rather than from the state's
true position. A clean-shutdown marker proves the state is not torn; it does not prove the state's ledger.

Since 2026-08-31 every ws-sync ledger writes `meta:last_seq` into `state.rocks` in the same `WriteBatch` as that
ledger's state changes (ws_sync.rs, `process_ledger`), and the mismatch rollback rewinds it. Nothing reads it at startup.
It is the bookmark this design uses.

## Design

### 1. Clean stop at a ledger boundary

- ws-sync gains a stop control shared with the SIGTERM handler: a stop-requested flag, and a parked slot holding the
  last ledger that landed and verified (sequence and account hash).
- The ws-sync loop checks the flag before starting each ledger. When set, it records the parked slot from the last
  verified ledger and processes nothing further.
- The SIGTERM handler requests the stop, waits up to 20 s for the loop to park, flushes `state.rocks` (so the next open
  does not depend on WAL replay), writes the **resume ticket** only if the loop parked at a verified ledger, writes the
  clean-shutdown marker as today, and exits.
- The shutdown path no longer saves `leaf_cache.bin`: nothing reads it, and the save holds the hasher lock while about
  1.3 GB is written — the window in which a ledger could land without being verified before exit.

**Resume ticket:** `{sync}/resume_ticket.json`, `{"seq": N, "account_hash": "<hex>", "written_at_unix": <seconds>}`,
written atomically (temporary file, then rename). No ticket means no warm restart.

### 2. Proven resume

A new startup branch, taken only when `XRPL_WARM_RESUME=1` is set (checked before the existing branches, so the legacy
`sync_complete.marker` branch never runs in warm mode). If the F5 integrity check did not return `CleanResume`, it falls
back at once. Otherwise:

0. Disable the legacy incremental syncer first, exactly as the bulk branch does before it touches the state: it is
   triggered by peer messages and would otherwise write later ledgers into `state.rocks` while the checks run.
1. Read the ticket, then delete it (a crash during the resume leaves no ticket, so the next start is cold).
2. Read `meta:last_seq` from `state.rocks`; it must equal the ticket's `seq`.
3. Build the ws-sync hasher (`FlatHasher`) from `state.rocks` once and take its root.
4. Fetch the network's `account_hash` for ledger `seq` from the RPC source (three attempts).
5. The network's validated ledger must be no more than 1,000 ledgers past `seq` (about an hour). A warm catch-up
   costs about 80 ms per ledger, one ledger at a time; past that gap the cold download is faster and lighter on the
   source node.
6. The root must equal the network hash and the ticket's hash. Only then: rewrite `sync_complete.marker` with the
   current entry count, log
   `[resume] OK: #N verified (root …) — ws-sync from #N+1`, and start ws-sync with `last_synced = N`.

From there the node runs the same catch-up code every cold start uses after its bulk download: each ledger from `N+1`
is fetched, written atomically with its bookmark, and verified against the network's account hash.

**Fallback:** any failed check or error logs `[resume] FALLBACK: <reason>` and exits with status 75. The resume logic
lives in a new module (`resume.rs`) as functions over an injected hash fetcher, so every decision is unit-testable.

### 3. Launcher and deploy step (operator tooling outside the repository)

- The production launcher gains `--warm`: if the sync directory holds both a resume ticket and the clean-shutdown
  marker, it keeps the directory and sets `XRPL_WARM_RESUME=1`; otherwise it takes the cold path. Without `--warm` the
  launcher is unchanged — a full wipe and resync — and remains the recovery path after any crash or halt.
- The deploy step launches with `--warm` and waits up to 180 s for `[resume] OK` (its start-up verdict) or
  `[resume] FALLBACK`. On a fallback, on the process exiting, or on neither line appearing in time, it stops the process
  if it is still running and relaunches cold. The existing retry for the bulk-sync seed race is kept.

### 4. Fix found on the way

The "no hash root after write" rollback in `process_ledger` restores the pre-images but does not rewind
`meta:last_seq`, leaving the bookmark one ledger ahead of the state. It will rewind it like the mismatch rollback does.

## Testing

- **Unit:** ticket round-trip and atomic write; every resume decision (no ticket, bookmark ≠ ticket, root ≠ network
  hash, network unavailable, gap over 1,000 ledgers, success); the stop control (request, park, parked slot); both rollbacks rewind the bookmark.
- **Synthetic database:** a temporary `state.rocks` with a few ledger objects and a bookmark — the resume check accepts
  the true root and rejects any other.
- **Rehearsal on real data (before any deploy):** a small `resume_check` binary opens a kept `state.rocks` read-only,
  reads its bookmark, rebuilds the root and compares it with the network's account hash for that ledger, reporting open
  and scan times. It runs against the two states the launcher kept from the 2026-09-23 deploys (stopped at 20:50 and
  23:02 EDT). They predate the clean stop, so this is a stricter test than a real warm restart.
- **Launcher:** the `--warm` branch logic against a temporary directory and a stub binary.
- **Gate:** the workspace battery, clippy and the FFI build check, then the next deploy cycle's gate and replay windows.

## Acceptance

- The rehearsal reports a root equal to the network's account hash for both kept states (or explains any difference
  before the work proceeds).
- At the first warm deploy: `[resume] OK`, then ws-sync matches with zero mismatches, a start gap under 60 ledgers, and
  the deploy's health verdict OK. A fallback, if it happens, ends in today's cold start with no manual step.
- The standing wipe-on-restart rule is updated only after that deploy succeeds.
