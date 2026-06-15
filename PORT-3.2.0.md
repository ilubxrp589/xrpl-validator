# xrpld 3.2.0 port — execution plan

**Status (2026-06-15):** waiting on the **stable** 3.2.0 release. Only betas exist on
GitHub (`3.2.0-b1…b7`); the apt stable channel has no 3.2.0 deb, and the source node
is `3.1.3, full, not amendment-blocked`. Nothing forces an upgrade yet — amendment
activation runs on a ~2-week-after-majority clock that starts *after* release. Prep is
staged on `deploy/3.2.0-prep`; the build itself is deferred to stable.

Reference for everything below: `~/Desktop/xrpl-validator-reaudit-2026-06-10.md` §6.

## Already done (this branch)
- Reaudit fixes: F1 (disarm va-05 lockstep wiring — **must stay disarmed**, or the first
  3.2.0 binary re-arms the trap), F2, F5, F6/va-07 (dynamic amendments), F7, F4/val-110,
  F8a/F8b.
- **F8c** — `ffi/xrpl_shim.cpp` version comments corrected 3.1.2 → 3.1.3 (now consistent
  with `MinimalApp.h:3`). Bump `SHIM_VERSION` → `0.3.0` when the shim is ported to 3.2.0.

## 1. Amendment voting list — REGENERATE (do not hand-patch)
`crates/xrpl-node/src/validation.rs:44-82` `supported_amendments()` is a static name list;
each ID is pure `SHA-512Half(name)` (no libxrpl dependency). Consumers: flag-ledger voting
only (`live_viewer.rs:1386`, `validation.rs:342`). This node is observer-grade (not in a
UNL per va-06), so votes are currently **inert** — correctness here is telemetry/readiness,
not signing safety. Still, regenerate it at port time:

- The current list is **stale beyond 3.2.0**. Measured against the b7 `features.macro`
  (`include/xrpl/protocol/detail/features.macro`):
  - **3.2.0 retires ~42** long-activated amendments (they become baseline, leave the
    votable set): DisallowIncoming, NegativeUNL, Checks, DepositAuth, MultiSign, Flow,
    ExpandedSignerList, TicketBatch, HardenedValidations, … and many `fix*`.
  - **New in 3.2.0:** `MPTokensV2`, `PermissionDelegationV1_1`, `fixCleanup3_2_0`.
    ⚠️ `Example` appears too — that's rippled's **test** amendment; **never** add it.
  - Already-missing 3.1.x amendments the list never got: Batch, LendingProtocol,
    TokenEscrow, PermissionedDEX, MPTokensV1, SingleAssetVault, DynamicNFT, DynamicMPT,
    and assorted `fix*`.
- **Action at stable port:** rebuild the list from the **stable** 3.2.0 `features.macro`
  (FEATURE→name, FIX→`fix`+name, drop RETIRE, drop `Example`).
- **Governance decision (ASK THE OPERATOR):** the function blanket-votes YES on every
  listed name; rippled distinguishes `VoteBehavior::DefaultYes` (mostly fixes) from
  `DefaultNo` (most new feature amendments — operator opts in). Decide whether to vote YES
  on all, or only DefaultYes/fixes. Until decided + in a UNL, this stays inert.

## 2. FFI shim + CMake re-link (needs the 3.2.0 source tree → m3060)
- **18 `extern "C"` symbols** (`ffi/xrpl_shim.h:49-117`) re-link against 3.2.0 libxrpl.
  Exception walls already fail-closed (`xrpl_shim.cpp:164,215-225,331-336,446-451,493`).
- **`ffi/MinimalApp.h`** is the highest-probability compile break — it hand-mirrors
  3.1.3's `Application.h` shape; any 3.2.0 reshuffle breaks it first. Budget time here.
- **CMake globs:** `ffi/CMakeLists.txt:59-96` `GLOB_RECURSE` + 18 filename exclusions —
  re-derive against the 3.2.0 tree (upstream file moves land silently in/out).
- **build.rs target names:** `crates/xrpl-ffi/build.rs:28-30` links `xrpl.libxrpl`,
  `xrpl.libpb` — upstream cmake-internal names that can rename. Verify, watch for the
  stale-libxrpl linkage trap (linking cached 3.1.3 instead of fresh 3.2.0).
- The rippled→xrpld **rename is a non-event for this repo's code** (no binary-name
  coupling); only `.39`/m3060 deploy scripts + systemd units need the name check.

## 3. Build + verify order (proven runbook — do not shortcut)
1. Build on **m3060** (`.39` lacks libxrpl.a + Boost incompat). m3060 also runs the live
   validator → build in an isolated dir, throttled (`nice`/`-j2`), keep the current 3.1.x
   validator up; watch the box (COP `host_memory` signal — OOM here caused past halts).
2. Shadow-run the new binary; require **100% hash-match across a multi-hundred-ledger
   window** (not a few — new tx types/amendments are the divergence risk). The rebuilt
   match streak IS the proof the 3.2.0 build is byte-correct.
3. Only then: upgrade `.39`'s stock rippled to stable 3.2.0 (**keep it alive** — it's the
   validator's source; OOM-starvation of it caused the 2026-05 halts), then cut the
   validator over. One node at a time; source node stays up.
4. During the shadow window, stand up va-08 forensic divergence bundles — first
   post-upgrade mismatch is far cheaper to diagnose with a bundle in hand.

## 4. Release watch
`scripts/watch-320-release.mjs` (cron) checks GitHub releases + the apt stable channel for
a non-prerelease 3.2.0 and pings Telegram once when it lands. Then execute §1–§3.
