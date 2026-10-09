# Plan: a fully independent validator — 2026-10-09

Supersedes the roadmap section of `docs/STATE-2026-07.md` (approved 2026-07-04). Same goal, updated for
where the work actually is: m3060 signs a ledger hash **it computed itself**, from data **it got from
peers**, with the upstream xrpld (.39) only watching.

## Where we are (measured 2026-10-09)

- **The Rust engine writes the state.** Stage 4 Phase B (cycle 166, 10-08): `XRPL_NATIVE_WRITER=1`,
  state.rocks bytes come from our engine. Since the F417 deploy (cycle 167, 12:39): 5,343 ledgers written
  natively, 0 refused, 0 hash failures; 5,645 consecutive state-hash matches. Apply time ≈ 30 ms a ledger
  (`apply_ms_last`).
- **Built and observing, not enforcing:** S1/S2 signature checks (since the 12:39 restart: 450,670 proposals and
  626,521 validations verified, 0 failures); S3 tx-set hash as a SHAMap root (`43fdc99`); va-06
  publisher-verified UNL (`c258ed6`; no pin set, unverified fallback allowed); lockstep M1 ledger-hash
  shadow (gauge 1.000000).
- **Peer layer exists:** about 55 direct peer connections carry proposals and validations, and our
  validations go out through them.
- **Amendments:** refreshed every flag ledger. Every live rule is ported except **fixBatchV1_2**: the
  3.4.1 source is still unpublished, so it stays quarantined until the source lands.

## What m3060 still borrows

| Borrowed today | From | Replaced in |
|---|---|---|
| Transactions, metadata, ledger header | .39's RPC/WS (ws-sync) | Steps 2, 4, 5 |
| The order transactions apply in | each tx's `TransactionIndex` in the network's metadata (`native_shadow::apply_order`) | Step 3 |
| Each tx's result, and the check on our state change | the network's metadata (the writer's `vouch`) | Step 3 |
| The ledger hash it signs | the network's announced hash, after 3 consecutive account-hash matches (Phase-3 gate) | Step 5 |
| A safe copy when it doubts | .39's bytes (writer refusal) | Step 5 (stop on doubt) |
| Trust in the UNL | fetched and verified, but an unverified fallback is allowed | Step 1 |
| The C++ engine | libxrpl via FFI, comparison only; the native writer still builds only with the `ffi` feature | Step 6 |

## The steps

### Step 1: arm what's built (small, one cycle)
- `XRPL_SIG_ENFORCE=1`: drop proposals and validations whose signatures fail (over 1 million checks, 0 failures).
- va-06: set `XRPL_UNL_PINNED_KEY` to the publisher's master key. Watch several flag-ledger refreshes verify,
  then `XRPL_UNL_ENFORCE=1` (fail closed, keep the last good list).
- **Amendment guard (new, small):** a list of the amendments the Rust engine implements. If the network
  enables one that isn't on it, alarm at once. From Step 5 on, the guard also stops signing; that is our own
  amendment block.
- Port fixBatchV1_2 when the 3.4.1 source is public. This is a hard prerequisite for the Step 5 flip.

### Step 2: our own metadata (large; the biggest build)
- **Why:** the ledger hash covers the transaction tree, and every leaf is a transaction plus its metadata.
  Without our own metadata we have no ledger hash of our own.
- **Build:** produce rippled's metadata from the engine's before and after entries, byte-exact:
  - AffectedNodes in key order: CreatedNode/NewFields, ModifiedNode/FinalFields + PreviousFields, and
    DeletedNode/FinalFields (+ PreviousFields), chosen by each field's metadata flags.
  - The threading fields.
  - `TransactionIndex`, `TransactionResult`, `DeliveredAmount`, and `ParentBatchID` on Batch inners.
- **Check (trailing, no risk):** every ledger, compare our tx-tree root (`shamap::tx_tree::compute_tx_tree_root`,
  already tested against mainnet) with the header's `transaction_hash`, and each tx's metadata bytes with the
  network's. The dp, fixture and campaign loop we use for state today carries over unchanged.
- **Exit:** weeks of every ledger matching, with receipts drilled the same way as today.

### Step 3: our own order and results (medium)
- Rippled's consensus order: CanonicalTXSet sorts by account XORed with a salt taken from the set hash,
  then sequence or ticket, then id. Then come the retry passes: a retryable result goes again on the next
  pass, and after the last pass it is left out of the ledger. Today's `close::canonical_order` is a plain sort.
- The writer stops reading `TransactionIndex` and `TransactionResult` and computes them itself. A refusal
  becomes "our metadata or hash differs from the network's".
- Pseudo-transactions (`tx/pseudo.rs`) are taken from the set, and fees burned are computed (the header's
  `total_drops`).
- **Check:** trailing shadow against the network every ledger. Still no risk.

### Step 4: data from peers (medium to large)
- **M2a:**
  - Read the agreed tx-set hash from trusted proposals.
  - Fetch the set over gossip first, then `TMGetLedger liTS_CANDIDATE`.
  - Verify its SHAMap root with the S3 code.
- Fill gaps and recover after a restart from peers (ledger and state fetch), not from .39.
- **Check:** every ledger, compare the peer-fed set with the .39-fed one, then switch the source. .39 becomes
  a witness that is compared after the fact. A local xrpld on m3060 (old Track 3) becomes optional backup,
  not a requirement.

### Step 5: compute at the live edge and sign our own hash (medium; the flip)
- As soon as consensus closes, apply the agreed set and build our header:
  - the parent is our previous ledger;
  - the close time comes from the round's proposals, by rippled's rule;
  - the tx root comes from Step 2;
  - the account hash and total drops are ours.
  That gives **our ledger hash**.
- **Shadow:** compare our in-round hash with the network's validated hash, for weeks at 1.0 (the M1 gauge,
  now with every input ours).
- **Flip (James's tap):** sign our hash, using va-05 in the in-round form; this replaces the trailing form
  that failed on 05-31.
- **On any doubt, skip that validation:** a missing tx, an unported amendment, a refusal, an unready engine.
  Never sign a hash we didn't compute. Phase-3 signing stays as the instant rollback flag.

### Step 6: later (not needed to call it independent)
- A Rust-only build: decouple the native shadow and writer from the `ffi` feature, and keep libxrpl as a
  test oracle in the gate.
- Propose tx sets in consensus (deferred: non-UNL proposals barely count).
- Vote yes only on amendments we've ported.

## Order and rules

- **Order:** Step 1 now. Steps 2 → 3 and Step 4 run in parallel. Step 5 last.
- **Why this order:** Steps 2 and 3 are checked against mainnet on every ledger while still trailing, so most
  of the correctness is proven before anything live changes.
- **Standing rules:**
  - Every new path is env-gated and off by default.
  - Shadow before enforce.
  - A 1-day soak, shipping fixes together.
  - James taps each enforce flip.
  - The fixBatchV1_2 quarantine stands.
- **Done means:** 30 days of m3060 signing hashes it computed itself, from peer data, with .39 only watching.
