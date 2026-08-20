# The flow driver — per-iteration strand competition, faithful to StrandFlow

Status: COMPLETE (2026-08-20). All nine specimens clean; gate flowdrv2 at
full baseline. What the receipts proved: the skeleton was already faithful
(ub-order first-success, one pass per strand, persistent fib) and the class
fell to FOUR receipt-pinned deltas — the fib pace (§5.1, one counter tick
per WINNING round, not per pool consumption), the zero-flow TER (§5.2,
PATH_PARTIAL unless partialPayment), the consumed-trim stream continuation
(§5.4a: BookStep.cpp:1062 returns fullyConsumed as the CONTINUE flag, so a
trimmed fill that exhausts the offer keeps reaping; and single_pass must
sweep trailing dead levels BEFORE its early return), and probe hydration
for all-dead book levels (§5.4b: `book_offers` omits fully-unfunded offers
and never names their pages — enumerate trailing pages with a seeded
`ledger_data` marker, hydrate each page's Indexes + maker root + gets-line +
owner dir; `XRPL_PROBE_FALLBACK` covers pre-window fixtures for spot runs).

Original target: the last coherent divergence class — every remaining
Payment on the board. 6 both-tesSUCCESS mutation diffs and 3 TER diffs,
all of them multi-strand/pool payments:

| fixture | tx | shape | first read |
|---|---|---|---|
| 106014566 | `06E7EE6F` | 15v7, extra ×8 incl a CREATE | we cross more |
| 106030404 | `50E1F824` | 7v11, missing ×4 incl deletions | we cross less |
| 106156904 | `34103010` | 12v7, extra | we split across two strands where mainnet used one (already partially treated by ub-ordering, residue remains) |
| 106259217 | `35DC9C5E` | 14v7, extra incl a CREATE | we cross more |
| 106360400 | `E133BD25` | 11v8, extra = rrhXifo PHNIX line+root+offer | leg-1 takes the book head one round where rippled's pick is the pool |
| 106365730 | `EDE4A5CE` | 11v8, same extras as E133BD25 | same bot, same book, same mechanism |
| 106217562 | `F9097D18` | tecPATH_PARTIAL v tesSUCCESS, 1v9 | we under-deliver; rippled fills across TWO strands, no limitQuality, many alternating iterations with pools on both |
| 106267220 | `F328A94C` | tecPATH_PARTIAL v tesSUCCESS, 1v15 | same family, bigger |
| 106071067 | `C98FD7C2` | tecPATH_DRY v tecPATH_PARTIAL, 1v1 | rippled finds SOME liquidity (then fails partial); we find none |

Everything in §1–§4 is grounded in the vendored 3.2.1 sources
(`include/xrpl/tx/paths/detail/StrandFlow.h`,
`src/libxrpl/tx/paths/{Flow,RippleCalc}.cpp`,
`include/xrpl/tx/transactors/dex/AMMContext.h`), re-read today — line
references are to that tree. §5 carries the per-specimen FFI receipts.
No behavior below is inferred without one of those two sources.

## 1. The driver contract (StrandFlow.h `flow()`, :600–800)

Entry (RippleCalc.cpp:57–90): `defaultPaths = !tfNoRippleDirect`,
`partialPayment = tfPartialPayment`, `limitQuality` only when
tfLimitQuality AND SendMax > 0 (= Quality{deliver/sendMax}); DeliverMin is
the caller's post-check, not the driver's. `AMMContext ammContext(src,
false)` lives for the WHOLE flow; `setMultiPath(strands.size() > 1)` seeds
it (Flow.cpp:106).

Loop (`while remainingOut > 0 && (!sendMax || remainingIn > 0)`, ≤1000
tries, ≤1500 offers considered):

1. **`activateNext(sb, limitQuality)`** (:465–520): `cur_ ← next_` sorted
   by `qualityUpperBound(sb, strand)` DESC, **stable** sort (ties keep
   insertion order = construction order). While sorting (only when
   `next_.size() > 1`): a strand with no bound (dry) is dropped; a strand
   with `ub < limitQuality` is dropped **permanently**. A SINGLE remaining
   strand is neither sorted, nor bounded, nor dropped — it just runs.
2. **`setMultiPath(activeStrands.size() > 1)`** — re-evaluated every
   iteration.
3. **`limitRemainingOut`** = `limitOut(...)` ONLY when exactly one active
   strand AND limitQuality (:681–689); `withinRelativeDistance(out,
   remainingOut, 1e-9) → remainingOut` (adjustedRemOut=false).
4. **Strand loop, first-success-wins** (:695–755): for each strand in
   `cur_` order: `ammContext.clear()`; [crossing-only: re-check ub ≥
   limitQuality]; run the FULL per-strand `flow<>` (rev+fwd, §2) on a CHILD
   sandbox; `setUnion(ofrsToRm, f.ofrsToRm)` **even on failure**; skip
   failed/zero strands (they are NOT re-added — permanently dropped); on
   success compute `q = out/in` and reject on
   `limitQuality && q < limit && (!adjustedRemOut ||
   !withinRelativeDistance(q, limit, 1e-7))` (`continue` — also permanently
   dropped); otherwise `best = this`, push the strand back (unless
   `f.inactive`), push the UNVISITED tail (`pushRemainingCurToNext`),
   **break**. rippled never compares realized results across strands — the
   ordering IS the competition.
5. **Apply**: `best->sb.apply(sb)`; `ammContext.update()` (counts an AMM
   iteration iff `setAMMUsed()` fired during the winning pass);
   `savedIns/savedOuts` are flat_multisets and `remainingOut = outReq −
   sum(savedOuts)` — **summed smallest-first each iteration**, not a
   running subtraction.
6. **`ofrsToRm` applied to the OUTER sb every iteration** (offerDelete,
   :787–795) — from ALL strands visited this iteration, winners and losers.
7. No best ⇒ "All strands dry" ⇒ break.

Termination (:800–850): `actualOut < outReq && !partialPayment` ⇒
tecPATH_PARTIAL (with actualIn/Out); `partialPayment && actualOut == 0` ⇒
tecPATH_DRY.

## 2. The per-strand pass (`flow<>`, :82–270)

- REV right-to-left from `stepOut = requested out`. When a step LIMITS
  (`!equalOut(r.second, stepOut)`): **throw away sb AND afView**, re-run
  that step as the new anchor (`limitingStep = i`, `stepOut = r.second`),
  keep going left. `maxIn` cap at step 0 re-executes step 0 as FWD.
- FWD from `limitingStep + 1` with `stepIn = limitStepOut`.
- `afView` = balances before the strand executes (funding checks); reset
  with sb at each re-anchor.
- `inactive` = any step reports inactive (a consumed self-offer marks the
  strand for removal without killing this pass's result).

## 3. AMMContext (AMMContext.h)

`kMaxIterations = 30`. `ammIters_` advances once per DRIVER iteration whose
winning pass used the AMM (`update()` after apply). `multiPath()` decides
the offer shape in AMMLiquidity::getOffer: fib slices when true, anchored/
maxOffer when false. `clear()` before every strand attempt.

## 4. Our skeleton (payment.rs round loop) — correspondence

Already faithful: ub-order + first-to-survive (`order` vec), per-round
`multi_now = order.len() > 1`, one PASS per strand call (`single_pass`),
snapshot/rollback per attempt, persistent `AmmFib { init, iters }`,
strand-level limitQuality judge with `continue`, dual net/gross remainder
bookkeeping, three strand bands (books / dstrands / mstrands).

Known structural deltas to check against receipts:
- **(a) rem_out recomputation**: ours subtracts per round; rippled re-sums
  `outReq − sum(savedOuts)` smallest-first. One-ulp drift per round is
  possible on IOU deliver targets.
- **(b) permanent drops**: rippled drops a failed/rejected strand FOREVER
  (not re-added to next_). Our `order` recomputes from ALL strands every
  round — a strand that failed in round N is retried in round N+1. If its
  failure was stateful (e.g. its book head died), retrying is harmless; if
  it failed by limitQuality/zero-out, rippled would never look again.
- **(c) losers' ofrsToRm**: rippled REAPS dead offers found by LOSING
  strand attempts (and by the winner's rev walk) into the outer view each
  iteration. Our loop restores the snapshot on a failed attempt — reaps
  from losing attempts are rolled back with it.
- **(d) the 1500-offer / 30-AMM-iteration caps**: ours has rounds=32 and
  no offersConsidered cap; AmmFib.iters exists but nothing enforces 30.
- **(e) single-strand limitOut** (driver step 3): exists in the crossing
  walk; whether the payment path sizes by it when ONE strand + tfLimitQuality.
- **(f) strand_upper_bound fidelity**: composedQuality with DebtDirection
  threaded (transfer fee only when src issues after a redeem);
  our per-band ub functions must match rippled's per-step
  `qualityUpperBound(v, dir)` composition.

## 5. Specimen receipts (FFI traces, /tmp/fd_*.log on m3060)

Attribution reminder: fprintf probes and the flow driver's JLOG both print
UNPREFIXED (payments log through the unnamed registry journal); only
RippleCalc's own lines carry the hash prefix, and the whole captured block
prints at the `=== FFI_TRACE` banner AFTER the apply. A specimen's stream
is the block immediately before its banner.

### 5.1 THE FIB-PACE BUG — E133BD25, EDE4A5CE, 06E7EE6F, 34103010 (+ fib
portions of F328A94C, 35DC9C5E)

E133BD25 (XRP→PHNIX→McRib through two pools, 2 strands, limit=none):
rippled runs 8 iterations, ALL through the better-ub strand, both legs
pool-served, and the McRib deliveries are EXACT fib multiples of the base
slice 4452.8508806: ×1,1,2,3,5,8,13, then clamp. The XRP/PHNIX pool's
synthetic offers (owner rLJMi56… IS the pool account) grow by consecutive
fib sums (406.8→661.9→1071.3M drops, ratio →φ). The CLOB tip at lob
8.105717362565245 is never consumed — every slice prices 7.99–8.05.

OURS: round 0 is byte-identical (773 → 4452.850880600). Then the DX_AMM
`fib iter=` counter reads 0,0,0,1 within round 0, then 2,2,2,3 in round 1,
4,4,4,5 in round 2 — TWO increments per round, one per pool consumption
(amm_turn's `if used { f.iters += 1 }`), where rippled's AMMContext
advances ONCE per winning driver iteration (`update()` after apply, however
many pools the strand touched). Our slice sequence is fib(0),fib(2),fib(4),
fib(6) = ×1,2,5,13 — skipping 1,3,8 — so we deliver in 5 rounds what
rippled spreads over 8, and the mis-paced sizes brush CLOB offers rippled
never touches (the rrhXifo extras). EDE4A5CE: same bot, fib ×1,1,2,3,5 +
clamp. 06E7EE6F: 4 iters ×1,1,2 + clamp. 34103010: 3 iters ×1,1 + clamp.

FIX (stage 1a): the fib counter advances once per ROUND whose winning pass
used any pool — a used-FLAG set inside consume_fib/amm_turn, committed +1
at the round level. Applies to the payment round loop AND cross_bridged's
rounds (same AMMContext semantics; crossing gates will verify no
regression).

### 5.2 THE ZERO-DELIVERY TER — C98FD7C2

rippled: "All strands dry." at iteration 0, `Total flow: in: 0 out: 0`,
and the ending is `actualOut != outReq && !partialPayment ⇒ tecPATH_PARTIAL`
(:823) — tecPATH_DRY needs `partialPayment && actualOut == 0` (:840). We
returned tecPATH_DRY for a NON-partial payment that flowed nothing.

FIX (stage 1b): the TER for a zero-flow payment is tecPATH_PARTIAL unless
tfPartialPayment is set. One mapping rule in the payment epilogue.

### 5.3 ZERO-LIQUIDITY DISCOVERY — F9097D18 (1v9), F328A94C (1v15)

rippled runs 12 / 7 iterations, TWO strands ALTERNATING as the ub-sort
flips while pools drain (F909: small IOU fib slices 467.73/762.06…
interleaved with huge book fills 465586364/1426001810). Our engine flows
NOTHING (1 mutation = fee only): the strands never produce a first pass.
Not the fib pace — a construction or hydration gap for these path shapes.
NEEDS its own dig once stage 1 lands (the fib fix changes nothing about a
strand that never flows).

### 5.4 SINGLE-ITERATION COMPOSITION — 50E1F824 (7v11)

rippled fills everything in ONE iteration (26366653 → 28.59217928509999);
we cross LESS inside that single pass (missing two deletions + mods). A
strand-internal rev/fwd composition delta, not a driver delta. Dig after
stage 1 (its window is small).

### 5.5 35DC9C5E (14v7)

3 iterations, first two nearly equal (2.1276/2.1274 — fib ×1,1 through a
pool with drift) + clamp. Expect stage 1a to move it; re-probe before
digging further.

## 6. Stages

- **Stage 1 (NOW)**: (a) fib pace — once per winning round, flag+commit;
  (b) zero-flow TER mapping. Fix → re-probe all 9 → full psweep gate →
  commit.
- **Stage 2**: re-probe survivors; expected residue = F909/F328 (discovery
  gap — trace WHY our strands produce no pass: construction, hydration, or
  a first-pass gate) and 50E1F824's single-pass composition.
- **Stage 3**: driver-fidelity cleanups only if a specimen demands:
  permanent strand drops (rippled never retries a failed strand; we retry
  every round), losers' ofrsToRm surviving the snapshot rollback,
  `remainingOut = outReq − sum(savedOuts)` smallest-first re-summation,
  the 1500-offer / 30-AMM-iteration caps.
- Probe-determinism (the l106203258 flake) stays on the board as its own
  item — it gates TRUST in sweeps, not this build specifically.
Each stage: fix → gate (`./psweep.sh <tag> 10`) → commit, one at a time.
