# Amendment vote board — design (v1, read-only)

**Date:** 2026-06-15 · **Status:** design approved, parked for build (does NOT block on 3.2.0).

## Goal
A read-only dashboard section showing every amendment the validator votes on, joined to
live network status — making "what we vote / where we're behind" **visible** instead of
buried in the compiled-in `supported_amendments()` Rust array. Directly surfaces the
"silently voting against everything new" risk from the reaudit (§6 item 5).

## Scope (v1)
- **READ-ONLY.** No vote control. (Interactive control is a separate, later project: votes
  are compiled-in today, so it needs runtime config + reload + a write API + it mutates the
  signing/consensus path → auth + a 7-day watch. Explicitly out of v1.)
- **Self-contained.** No external data sources. Network-wide support-% and activation ETA
  are **OUT** of v1 — they aren't in this node's `feature` output and would need an external
  registry (xrpl.org / validator list). Deferred.

## Architecture (matches existing split: copilot = data, dashboard = render)
- **Copilot serves it** — new **free** `GET /amendments` endpoint (generic → public repo).
  Joins two things:
  - *Network status* — from the node's `feature` data the copilot already reads
    (`getAmendments` in `tools.mjs`). NOTE: today `getAmendments` returns only the
    *unsupported* subset + counts; v1 extends it to return the **full** amendment set
    (enabled / in-voting+majority / not-yet), since the board needs all rows.
  - *Our stance* — each amendment marked `YES` (in our vote list) / `abstain` (not in it).
- **Single source of truth for the vote list** — extract `supported_amendments()`'s names
  into one committed data file (e.g. `crates/xrpl-node/src/amendments.list`, pulled via
  `include_str!`); the copilot reads the same file. Kills copilot↔binary drift, and becomes
  the artifact we **regenerate at the 3.2.0 port** (ties this to the PORT-3.2.0.md work).
- **Dashboard renders** — new `<AmendmentBoard/>` component (local, like `copilot-panel`),
  fetches `/amendments`, renders the table + summary row.

## v1 truth caveat (be honest in the UI)
The running binary does **not** expose its live votes (compiled in; no rebuild until 3.2.0),
so v1 shows the **configured** list (from the committed file), captioned as such. At the
3.2.0 build we add a validator FFI endpoint exposing the *actual* live votes → the board
switches to that truth-source.

## Layout
```
AMENDMENTS — votes & network status                         generic+ffi
──────────────────────────────────────────────────────────────────────
                                            our vote    network
  Credentials                                ✓ YES      ● enabled
  PriceOracle                                ✓ YES      ● enabled
  MPTokensV2                   (3.2.0)       — abstain   ◐ voting · majority
  PermissionDelegationV1_1     (3.2.0)       — abstain   ○ not yet
  Batch                                      — abstain   ● enabled   ⚠ active, we don't vote it
──────────────────────────────────────────────────────────────────────
  76 YES · N abstain · 0 amendment-blocked · quorum 28 · 35 trusted keys
```
Columns: name · `(3.2.0)` tag if newly introduced · our vote (YES/abstain) · network
(enabled / voting+majority / not-yet) · `⚠` if active-on-network but we abstain. Summary:
YES count · abstain count · amendment-blocked count · quorum · trusted-key count.

## Implementation plan
1. **(copilot) Vote-list source** — extract the names into a committed data file; add
   `getVoteList()` reading it. No validator behavior change now (the binary keeps its
   current compiled list; it adopts `include_str!` of the same file at the 3.2.0 build).
2. **(copilot) Endpoint** — extend the `feature` read to the full amendment set; add
   `amendmentBoard()` that joins network set × `getVoteList()` → `[{name, new_in_320,
   our_vote, enabled, supported, voting, majority, flag}]` + summary; serve as free
   `GET /amendments` (like `/health`). Add a `get_amendment_board` tool + 1-line
   system-prompt/README note.
3. **(dashboard) Wiring** — `/amendments` rewrite in `next.config.js` (fast/cacheable →
   rewrite is fine here; unlike `/investigate` it isn't long-running).
4. **(dashboard) Component** — `<AmendmentBoard/>`: Halcyon-styled table + summary, polls
   the endpoint (~30s); place on the main page near the copilot panel.
5. **Build + verify** — `npm run build` (BUILD_ID check), plain `pm2 restart` (never
   `--update-env`), confirm the board renders with live data.

## Evolves at the 3.2.0 build
- Regenerate the committed vote list from **stable** `features.macro` (drop retirements +
  the `Example` test amendment; decide DefaultYes/DefaultNo per the governance call).
- Validator exposes its **actual** live votes via the FFI API → board reads truth-source.
- Later/optional: external-registry enrichment (support-% + activation ETA); interactive
  vote control (separate authed, signing-path project + 7-day watch).
