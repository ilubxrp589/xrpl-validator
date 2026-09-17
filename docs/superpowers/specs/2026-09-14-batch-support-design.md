# Batch (BatchV1_1) support — design

**Date:** 2026-09-14 · **Branch:** t0-batch · **Status:** Leg A deployed (cycles
129/130). Leg B implemented on branch t0-batch-b (plan
docs/superpowers/plans/2026-09-17-batch-leg-b-native-transactor.md):
BatchTransactor + shadow attribution; devnet vectors byte-exact; dp on both
devnet Batch ledgers (l5309670: 6/6 attempted txs MATCH, outer
0DB84681FAAD1C… verdict=MATCH our_ter=net_ter=tesSUCCESS our_muts=net_muts=3;
l5309584: 3/3 attempted txs MATCH, outer C2E675EEF5AD8… verdict=MATCH
our_ter=net_ter=tesSUCCESS our_muts=net_muts=3), zero DIVERGE-TER/DIVERGE-MUT
on either ledger, no threading-fallback receipts. Awaiting the next deploy
cycle.

## Why now

BatchV1_1 (amendment 9F287AED…) is in rippled 3.3.0, enabled on devnet, not yet
enabled on testnet or mainnet. Mainnet is short of majority by a couple of
validators; activation follows majority by two weeks. The .39 node currently
vetoes it (our vote), which does not stop the network.

## The validator-critical finding

Our Stage 3 state leg applies each transaction of a validated ledger through
the shim (`xrpl_apply` / `xrpl_apply_with_mutations` → `xrpl::apply`). rippled
records a Batch in the ledger as the outer `Batch` transaction **plus each inner
transaction as its own entry** (own hash, own meta carrying `ParentBatchID`,
consecutive `TransactionIndex`). The feed (`ws_sync::fetch_tx_blobs`) sorts by
`TransactionIndex` and applies every entry.

Two things go wrong at once:

1. `xrpl::apply` on the outer Batch charges its fee and sequence and returns
   `tesSUCCESS` — it does **not** apply the inner transactions. That happens in
   `applyTransaction` → `applyBatchTransactions` (apply.cpp), which the shim
   never calls.
2. Each inner entry is then applied standalone. `Transactor::preflight1`
   rejects `tfInnerBatchTxn` without a parent batch id:
   `temINVALID_INNER_BATCH`.

Net effect: the inner transactions' state changes are never produced, the
computed account-state hash disagrees with the network's, and the validator
stops matching on the first Batch ledger after activation.

Evidence — devnet ledger 5309670 (one Batch, two inner Payments, `tfUntilFailure`),
fixture fetched with `fetch_ledger_fixture.py --rpc https://s.devnet.rippletest.net:51234`,
probed on m3060 with the current shim:

```
attempted 8, ok 6, diverged 2
[ter] Payment/temINVALID_INNER_BATCH F835E19C…
[ter] Payment/temINVALID_INNER_BATCH C7A3E441…
PROBE: DIVERGENT
```

(The outer's own mutation set matched — its meta holds only the fee node — so
the per-transaction mutation check does not see the loss; the state-hash leg
would.)

## rippled semantics (3.3.0, Batch.cpp / apply.cpp / Transactor.cpp)

- Outer: exactly one of `tfAllOrNothing` (0x10000), `tfOnlyOne` (0x20000),
  `tfUntilFailure` (0x40000), `tfIndependent` (0x80000); `tfInnerBatchTxn`
  (0x40000000) forbidden. 2..8 `RawTransactions`, unique by hash; inner types
  in `kDisabledTxTypes` (Batch itself and pseudo types) rejected. Each inner:
  `tfInnerBatchTxn` set, `Fee` = 0, empty `SigningPubKey`, no `TxnSignature` /
  `Signers`, exactly one of `Sequence` / `TicketSequence`, and must pass
  `preflight(…, parentBatchId, TapBatch)`. Under AllOrNothing / UntilFailure the
  (account, sequence-or-ticket) pairs must be unique across inners.
- Fee: `base + Transactor::calculateBaseFee(outer)` + Σ `calculateBaseFee(inner)`
  + `base × signerCount` (BatchSigners: one per single-signed signer, or the
  nested `Signers` count).
- Signers: every inner `Account` ≠ outer account (and counterparty / sponsor
  where present) must appear exactly once, sorted, in `BatchSigners`; each is
  checked with `Transactor::checkSign` against the batch signing payload
  (`HashPrefix::Batch`, outer account, outer sequence value, flags, inner ids).
- Apply: outer `doApply` is empty. `applyTransaction` then runs
  `applyBatchTransactions` on a `kBatchView` over the ledger view: for each
  inner, a per-transaction `kBatchView`, `apply(registry, view, parentBatchId,
  inner, TapBatch, j)`; a tes/tec result is folded into the whole-batch view.
  Mode: AllOrNothing — any non-tes aborts the whole batch (nothing folded);
  UntilFailure — stop at the first non-tes, keep earlier; OnlyOne — stop after
  the first tes; Independent — run all. The whole-batch view is folded into the
  ledger only if at least one inner applied.
- Inner under `TapBatch`: no fee check beyond `Fee == 0`, no signature check,
  sequence/ticket consumed normally, meta gets `ParentBatchID`.

## Design — two legs

### Leg A: FFI (validator safety) — this branch

1. **Shim**: after `xrpl::apply` in both apply entry points, when
   `applied && tesSUCCESS && ttBATCH`, run a copy of `applyBatchTransactions`
   (it is `static` in apply.cpp; the `apply(registry, view, parentBatchId, tx,
   flags, j)` overload it needs has external linkage and is declared locally).
   The whole-batch view folds into the shim's `OpenView` before the mutation
   collector reads it, so the outer's mutation set carries the inners' changes.
2. **Feed**: `apply_ledger_in_order_with_net` builds the set of inner
   transaction ids from every `ttBATCH` blob (new export
   `xrpl_tx_batch_inner_ids`) and skips those blobs — exactly what rippled's
   `OpenLedger` does with `tfInnerBatchTxn` entries. For the per-transaction
   checks the skipped inners' expected outcomes stay unused and their expected
   mutation sets are folded into the outer's, so the comparison remains exact.
3. Everything else (transaction tree from fetched tx+meta pairs, header,
   state-hash compare) is unchanged.

Verification: devnet fixtures 5309670 and 5309584 through `parity_probe`
(drill tree build) — every attempted transaction MATCH with the inners folded
into their outer. Then the usual cycle (gate, HIST60/WIN98, deploy, health).

### Leg B: native engine (shadow parity) — after review

- `tx/batch.rs`: preflight (flags, counts, inner field rules, uniqueness),
  fee formula, BatchSigners structural checks (signature verification stays
  where the engine leaves it today), `apply` running the inners on nested
  sandboxes with the four modes, sequence/ticket consumption per inner.
- Native replay and shadow (`native_shadow.rs`): skip inner entries, attribute
  the outer's mutation set against outer ∪ inner metas; receipts name the
  outer with the inner results listed.
- Vectors: the devnet fixtures as bundles, one per mode as they appear.

## Open questions for James

- Vote: keep vetoing BatchV1_1 on .39 until both legs are live, or vote yes once
  leg A is deployed? (Our single vote does not move the network; the veto is
  a statement of readiness.)
- Leg B priority relative to the arithmetic port (track 1) and the structural
  flow-engine port (track 2).
