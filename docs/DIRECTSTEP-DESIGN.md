# DirectStepI — rippling through accounts, composed with book steps

Status: DESIGN (2026-08-19). Target: the 7-specimen `Payment
tecPATH_DRY-v-tesSUCCESS` family — 106102038 `5B97B89E`, 106206499
`3B4F9C9AEF`, 106311829 `9684A861` + `D2EB36BA`, 106373989 `8CAD0435`,
106374244 `7511A01A` (+ the doc's earlier 106146562-class refusals, which
this build must keep refusing for the right reason).

Everything below is grounded in (a) the 3.2.1 vendored sources —
`libxrpl/tx/paths/PaySteps.cpp` (`toStrand`, `toStep`),
`libxrpl/tx/paths/DirectStep.cpp` — and (b) FFI traces of all six
specimens with `XRPL_FFI_TRACE`, which print every step's rev/fwd numbers.
No behavior in this design is inferred without one of those two receipts.

## 1. What the six real strands look like (from the traces)

| specimen | NumSteps | shape |
|---|---|---|
| 106102038 | 2 | `D(src→rKiCet) D(rKiCet→dst)` — pure ripple, CNY through one gateway |
| 106373989 | 4 | `D D D D` — USDC.rGm7 → USDC.rcEG, zero books, rate 1.003 charged on the last hop |
| 106311829 `9684` | 5 | `X B(XRP→USD.rhub8) D D D` — rate 1.002 charged where the book feeds rhub8's gateway hop |
| 106311829 `D2EB` | 5 | `D D D B(USD→XRP) X` |
| 106374244 | 6 | `D D B(USDC→URL) D B(URL→RLUSD) D`-ish (books between two direct runs) |
| 106206499 | 6 | `D D D B(BEAR→LUMOS) B(LUMOS→XRP)? X` — partial: iter 1 goes dry, 1249 of 1874 delivered |

`D` = DirectStepI, `B` = BookStep, `X` = XRPEndpointStep. General form:
**a strand is an alternating sequence of DIRECT RUNS and BOOK HOPS**, with
XRP endpoints at the rims. Our engine today models only the book hops
(`Vec<Leg>`, adjacent pairs = books) and drops any path with a
non-re-anchor account element — hence all seven refusals.

## 2. The DirectStepI contract (DirectStep.cpp, confirmed by traces)

One step moves value across the MUTUAL trust line `line(src, dst, cur)` —
NOT via each party's issuer line (`move_leg` is the wrong primitive; a new
`direct_ripple_credit` adjusts the shared line, sign by low/high).

Per pass, with `out` requested (reverse) or `in` offered (forward):

- **debt direction**: `srcOwed = accountHolds(src, cur, dst, IgnoreFreeze)`;
  `> 0` ⇒ src REDEEMS, else src ISSUES. (:478-500)
- **maxSrcToDst**: redeem ⇒ `srcOwed`; issue ⇒
  `creditLimit2(dst, src, cur) + srcOwed` (dst's limit minus what dst
  already holds). Traces show the 1e80-limit lines bots use — `Me`'s
  (u128, i32) holds them.
- **qualities** (:735-790), the part that is NOT symmetric:
  - src REDEEMS ⇒ `srcQOut = max(prevStep.lineQualityIn, own QualityOut)`,
    `dstQIn = QUALITY_ONE`.
  - src ISSUES ⇒ `srcQOut = transferRate(src)` **iff the previous step
    REDEEMS** else `QUALITY_ONE`; `dstQIn = line QualityIn` (clamped to ≤1
    on the last step). This is where the gateway fee lives — traces:
    `srcQOut 1002000000` (rhub8), `1003000000` (rcEG); no prev step ⇒
    Issues, so a strand-head issuer hop charges nothing.
  - QualityIn/Out read from the MUTUAL line's per-side fields
    (sfLow/HighQualityIn/Out, dst side for In, src side for Out; 0/absent
    ⇒ 1e9). Ported because it is only the ratio inputs; every specimen
    reads 1e9.
- **reverse math** (:503-568): `srcToDst = mulRatio(out, 1e9, dstQIn, UP)`;
  non-limiting ⇒ `in = mulRatio(srcToDst, srcQOut, 1e9, UP)`; limiting ⇒
  cap at maxSrcToDst, `in` as above on the cap,
  `actualOut = mulRatio(cap, dstQIn, 1e9, DOWN)`.
- **forward math** (:617-700): `srcToDst = mulRatio(in, 1e9, srcQOut,
  DOWN)`; non-limiting `out = mulRatio(srcToDst, dstQIn, 1e9, DOWN)`;
  limiting mirrored with UP on actualIn. Forward may not exceed the
  reverse cache (`setCacheLimiting`) — our walker's existing
  rev-then-fwd-per-pass shape already enforces this by construction.
- **mutation**: `directSendNoFee(src, dst, srcToDst)` on the mutual line —
  both passes write; the engine sandbox dedupes to final state.
- `mulRatio` on IOU = our `me_muldiv` (16-digit decimal, survived the AMM
  calibrations).

## 3. Construction — the toStrand port (PaySteps.cpp:170-567)

`normPath` normalization, in exact order:
1. head element = `src` with asset = SendMax(else Amount) re-anchored to
   issuer `src`;
2. implied SendMax-issuer account element when `SendMax.issuer != src` and
   path[0] is not already that account;
3. the tx's own elements;
4. implied terminal OFFER element when the last asset-bearing element ≠
   deliver asset (payments compare full Issue; only crossing compares
   currency-only — this subsumes today's `terminal_is_ripple_step`);
5. implied deliver-issuer account element unless it is already the tail or
   `dst == deliver.issuer`;
6. `dst` tail.

Then pairwise emission (:380-505): account→account ⇒ DirectStepI;
account/offer boundaries insert the implied `curAsset.issuer` account and
the offer→account transition emits `DirectStepI(curAsset.issuer → next)`
— or an XRPEndpoint when `curAsset` is XRP at the tail. `curAsset.account`
re-anchors on every account element (which is why a mid-strand book's IN
issue is the PRECEDING account, exactly as our chains already model).

**Construction-time checks** (DirectIPaymentStep::check, :418-464 — these
DROP the path, mapping to tecPATH_DRY when nothing else flows, i.e. our
existing refusal plumbing):
- no mutual line ⇒ `terNO_LINE` (today's 106336831-class stays refused,
  now by the general rule);
- issuer `lsfRequireAuth` + unauthorized zero line ⇒ `terNO_AUTH`;
- previous step is a BOOK and src's side of the line carries NoRipple ⇒
  `terNO_RIPPLE` (the general form of db5e428's special case — keep both
  until a gate proves the special case redundant);
- `owed ≤ 0 && −owed ≥ limit` ⇒ dry precheck.
- loop dedup: an account may appear at most twice per asset in a strand
  (`seenDirectAssets`), else `temBAD_PATH_LOOP` ⇒ path dropped.

`tfNoDirectRipple` suppression of the default path is already modeled and
untouched.

## 4. Integration — typed hops, one normalizer, two stages

```rust
enum Hop {
    Book { from: Leg, to: Leg },        // existing cross_engine_to walk
    Direct(Vec<DirectHop>),             // a maximal run of DirectStepI
}
struct DirectHop { src: [u8;20], dst: [u8;20], cur: [u8;20] }
```

The strand normalizer lives in `xrpl-ledger` and is called by BOTH the
engine and the probe's hydration (single source — the institutionalized
lesson from tonight's three unhydrated-line regressions: **the check and
its hydration must not drift**). The probe loads, per strand: every
DirectHop's mutual line + both AccountRoots (TransferRate), plus the book
prefetches it already does.

**Stage 1 — pure-direct strands** (clears 106102038, 106373989):
a self-contained executor for strands with no Book hops: rev right-to-left
sizing per §2, fwd left-to-right, `direct_ripple_credit` per step, wired
into the existing round loop (partial payments, DeliverMin, delivered
accounting come free). Plus: normalizer, checks, hydration, unit tests
against the traced numbers (0.01672194676077877 = 0.01667193096787513 ×
1.003 round-up is a fixture assertion). Gate.

**Stage 2 — mixed strands** (clears the other 5): generalize
`strand_pass`'s hop loop and `reverse_requirements` from `&[&Leg]` to
`&[Hop]` — Book arms call the code that exists today, Direct arms call the
stage-1 math. The existing calibrations (per-pass sizing, transfer-fee
ownership in the walk, carry measurement) stay inside the Book arm
untouched. Gate.

**Out of scope, stated**: MPTEndpointStep (parked with the MPT build),
offer-crossing DirectStepI (payments only), `checkStrand` invariants
beyond what construction already guarantees, multi-strand interactions
beyond what the round loop does today.

## 5. Risks and their receipts

- *Transfer-rate double-charge*: the rate lives in the DirectHop's srcQOut
  ONLY (per §2); the Book arm's existing per-fill debit is untouched, and
  the two never own the same hop. The #105795329 lesson says measure this:
  the stage-2 gate's census (DX_VALCHECK) is the instrument.
- *Rounding drift*: every mulRatio site carries rippled's UP/DOWN choice
  in §2; specimen traces pin four of them to exact 16-digit values.
- *Behavioral widening*: strands that used to be dropped now execute. The
  gates + a 60-ledger fresh-rate batch after stage 2 are the guard; any
  new refusing-direction divergence is a stage-2 stop-the-line.
