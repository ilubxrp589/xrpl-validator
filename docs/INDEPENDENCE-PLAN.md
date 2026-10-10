# The plan: a fully independent validator

*Updated 9 October 2026. Replaces the July roadmap in `docs/STATE-2026-07.md`. Numbers measured on m3060 at
18:49 EDT that day.*

## 1. What "fully independent" means

Today m3060 checks its own work and then signs **the network's** hash. When it's fully independent, it will sign
**a ledger hash it worked out itself**, from data **it got from the network's peers**, with .39 only watching.

That's the money shot. At that point m3060's validation is a real second opinion: it agrees with mainnet because it
computed the same answer, not because it copied it.

**Done means** 30 days of m3060 signing hashes it computed itself, from peer data, with .39 only watching.

## 2. How a ledger gets its hash

Every ledger has a short header, and the ledger's hash is the fingerprint of that header. To sign its own hash,
m3060 has to produce every line of the header itself:

```
LEDGER HASH  =  fingerprint of the header:
  sequence number ............................. known
  total XRP in existence (after fees burned) .. borrowed today   -> Step 3
  parent ledger's hash ........................ borrowed today   -> Step 5
  TRANSACTION TREE root ....................... borrowed today   -> Step 2
      (every transaction + its record of what it changed)
  STATE TREE root ............................. OURS since 10-08 (Stage 4 Phase B)
      (every account, trust line, offer, AMM, ...)
  parent close time / close time / resolution / flags
                                              . borrowed today   -> Step 5
```

The state tree is the hard part, and it's done: the Rust engine writes it. The rest is either a different kind of
work (metadata, ordering) or plumbing (data from peers, close time, the switch).

## 3. Where we are today

| Measure (9 Oct, 18:49) | Value |
|---|---|
| Ledgers the Rust engine wrote since the 12:39 fix | **5,690**, with 0 refused and 0 hash failures |
| State hash matching mainnet, in a row | **5,764**, with 0 mismatches |
| Time to apply one ledger | **15–30 ms** |
| Signatures checked from other validators since 12:39 | **about 1.1 million**, with 0 failures |
| Direct peer connections | **about 55** |
| Validator list | **verified** against Ripple's publisher key (list #85, 35 validators, 0 rejected) |
| Voting | says yes **only** to rules the engine has built (since 26 Sep) |
| Live rules not yet in the engine | **none**: fixBatchV1_2 was ported in cycle 168 (live 10 Oct) |

**Built and running, but only observing (not enforcing):**
- signature checks on proposals and validations (S1/S2);
- the transaction-set fingerprint, done the way rippled does it (S3);
- validator-list verification (va-06);
- the July "lockstep" check, which rebuilds the ledger hash from the network's header plus our state root. It
  reads 100%.

## 4. What m3060 still borrows

| m3060 borrows | From | Fixed in |
|---|---|---|
| Every ledger's transactions, their records (metadata) and the header | .39, over its RPC and websocket | Steps 2, 4, 5 |
| The order transactions apply in | the position number in .39's metadata (`TransactionIndex`) | Step 3 |
| Each transaction's result, and the check on our state change | .39's metadata (the writer's `vouch`) | Step 3 |
| The hash m3060 signs | the network's announced hash, after 3 state-hash matches in a row | Step 5 |
| A safe copy when it's unsure | .39's version of the ledger (a writer refusal) | Step 5 |
| The C++ library | linked in for comparison only, but the writer can't build without it yet | Step 6 |

```
TODAY
  mainnet peers --(proposals, validations)--------------> m3060 --(signs network's hash)--> peers
  .39 (stock xrpld) --(every ledger: txs, metadata, header)--> m3060 --(Rust engine)--> state
                                                         checked against .39's metadata

TARGET
  mainnet peers --(proposals, validations, tx sets, ledger data)--> m3060
  m3060 --(Rust engine: order, results, metadata, state, header)--> OUR hash --(signs)--> peers
  .39 --(witness only: compared after the fact)
```

## 5. The steps

### Step 1: switch on what's already built
*Size: small, one deploy cycle. Your taps: two switch-ons.*

- **Signatures (enforce).** Throw out proposals whose signatures fail. About 1.1 million checks so far, 0 failures.
  Today nothing makes decisions from validations, so their check moves into Steps 4 and 5, where they will.
- **Validator list (fail closed).** It already verifies against Ripple's publisher key. We pin that key and refuse
  an unverified list, keeping the last good one if a fetch fails.
- **Amendment guard (new).** The voting side already says yes only to rules the engine has built. The guard watches
  the other side: what mainnet has actually switched on.
  - If mainnet switches on a rule the engine hasn't built, you get an alarm at once.
  - From Step 5 on, the guard also stops m3060 signing its own hash. It's our own version of "amendment blocked".
  - Its known-gap list starts empty: fixBatchV1_2, the rule that would have been on it, is ported.
- **fixBatchV1_2. Done.** Ported in cycle 168 (live 10 Oct 00:45), the night the 3.4.1 source was published.

### Step 2: our own transaction records (metadata)
*Size: large, the biggest build. No live risk: it runs alongside, checked against mainnet.*

**What metadata is.** Every transaction in a ledger carries a record of what it did: its result, its position, and
every ledger object it created, changed or deleted, with old and new values. A simple payment looks like this:

```
TransactionResult  tesSUCCESS          TransactionIndex  12
AffectedNodes:
  ModifiedNode  AccountRoot (sender)     Balance  100.000000 -> 89.999988   (fee + amount)
  ModifiedNode  AccountRoot (receiver)   Balance   50.000000 -> 60.000000
DeliveredAmount  10 XRP
```

**Why it matters.** The transaction tree holds every transaction **together with its metadata**. One wrong byte in
one record changes the tree's root, and with it the ledger hash. No metadata of our own means no hash of our own.

**How we build it.** The engine already applies each transaction in its own sandbox, so the before and after of
every object it touches already exists. We add rippled's rules for turning that into a record:
- **Which fields go where.**
  - "PreviousFields" lists only the fields that changed and are marked to be recorded.
  - "FinalFields" lists the fields marked "always" or "on change".
  - A new object's "NewFields" lists its non-default fields.
- **The order.** Objects are listed in order of their ledger key.
- **Threading.** The link from each object to the last transaction that touched it.
- **The extras.** DeliveredAmount, and ParentBatchID on Batch inner transactions.
- **The encoding.** Everything is written in the network's exact binary format.

**How we check it.** Mainnet's own metadata is the answer key for every transaction:
- **Per transaction:** our record against mainnet's, byte for byte. A mismatch names the exact field.
- **Per ledger:** our transaction-tree root against the header's. The code that builds that root is already
  tested against mainnet.
- It runs trailing, next to the writer, so nothing live changes. The probe, the fixtures and the campaigns we use
  for state today work the same way here.

**What can go wrong.** Many small rules: when an unchanged field still appears, how amounts are written, odd cases
like Batch and deletions. Each mismatch becomes a receipt, drilled like today's.

**Done when** weeks of every transaction's record match byte for byte.

### Step 3: our own order and results
*Size: medium. No live risk: checked against mainnet.*

**Today** the engine reads the order off .39 (each transaction's position number in its metadata). The writer then
checks each result against .39's.

**Rippled's rule** for building a ledger from the agreed set of transactions:
1. **Sort them in "canonical" order.** Rippled sorts by account, but first scrambles each account with a salt taken
   from the set's own fingerprint, so nobody can buy a good position. Within an account the sort goes by sequence
   number or ticket, then by transaction id.
2. **Apply them in up to 3 passes.** A transaction that can't apply yet gets a "retry" result, for example when
   it's waiting on an earlier one, and goes again in the next pass. Anything still stuck after the last pass is
   left out of the ledger.

Our `close::canonical_order` is a plain sort today, with no salt and no passes.

**Also in this step:**
- System transactions on flag ledgers (fee votes, amendment votes, the negative UNL) come from the set. The engine
  already applies them.
- Fees burned give the header's "total XRP", worked out by us.

**How we check it.** Every ledger, our order against mainnet's position numbers, and our results against mainnet's.
Both are exact answer keys. The rare retry cases will be the interesting receipts.

**Done when** weeks of every ledger's order and results match.

### Step 4: data from peers, not .39
*Size: medium to large. No live risk until the source switches, after weeks of comparing.*

**Today** every ledger's contents come from .39's RPC and websocket, and so does catch-up after a restart. The 55
peer connections carry the consensus chatter (proposals, validations, status), but m3060 fetches no ledger
contents over them yet.

**The peer protocol already allows this.** Any node can ask peers for a ledger's pieces by fingerprint
(`TMGetLedger`): the header, the transaction tree, pieces of the state tree, or a candidate transaction set while
consensus is running. Everything comes back checkable against its fingerprint, so a peer can't feed us a fake. It
can only refuse or be slow.

**Two uses:**
- **(a) Trailing:** for each validated ledger, fetch its header and transaction tree from peers. This replaces .39's
  feed.
- **(b) In the round (needed for Step 5):** the trusted validators' proposals name the transaction set they're
  agreeing on (S3 already computes those fingerprints the way rippled does). We fetch that set while consensus finishes.

**Catch-up.**
- A warm restart keeps the state, so only the few ledgers missed while down need fetching, and peers can supply
  them.
- A full rebuild from nothing (20 million objects) can still come from .39 or a peer. It's checked against the
  validated state fingerprint either way, so it doesn't weaken independence.

**What can go wrong: being throttled.** xrpld charges every requester a "resource fee" and cuts off any that ask too
much. .39's limits throttled m3060 that way on 4 Aug (over RPC, same mechanism). So we pace requests, spread them over many peers, and answer
requests too.

**How we check it.** For weeks the peer-fed copy and .39's copy are compared on every ledger. Then the source
switches, and .39 becomes the witness and backup. That also helps .39, whose ledger SSD is 45% worn.

### Step 5: build at the live edge and sign our own hash
*Size: a medium build, then weeks of shadow. Your taps: 5a, then later 5b.*

**Today** m3060 applies each ledger after mainnet has validated it. It checks its state hash, and once 3 in a row
match it signs the hash the network announced.

**Target.** As soon as consensus agrees on ledger N, m3060:
1. has the agreed transaction set (Step 4b);
2. applies it in canonical order with our own results and records (Steps 2 and 3), taking about 15–30 ms today;
3. builds the header:
   - the parent is our own hash for N−1;
   - the close time is the round's agreed close time (rippled rounds the validators' close-time votes to the
     current resolution, and marks the ledger "no agreed time" when they don't agree);
   - total XRP, the transaction root and the state root are ours;
4. takes its fingerprint. That's **our ledger hash**.

A round takes about 3–4 seconds, and our build takes well under one, so timing isn't the problem.

**Shadow first.** For weeks m3060 computes its in-round hash and only logs it, comparing with the hash mainnet
validates a moment later. The July lockstep gauge does this today with borrowed inputs. Here every input is ours.

**Then the switch, in two stages (my recommendation):**

| Stage | What m3060 signs | What the world sees if we're wrong |
|---|---|---|
| **5a: verify, then sign** | our own hash, but only once it also matches what the trusted validators are signing for N (about a second's wait) | nothing: we skip that ledger |
| **5b: sign on compute** | our own hash the moment it's built, like a stock validator | a public validation that disagrees with mainnet (tracking sites show agreement %) |

With 5a every signature is our own computation, but we never publish a hash mainnet disagrees with. 5b is the full
money shot and comes after a clean month of 5a.

**Stop on doubt (both stages).** Skip the validation if:
- the transaction set is incomplete;
- the engine refuses or errors;
- the amendment guard fires;
- the build runs out of time.

Never sign a hash we didn't compute. A breaker drops back to today's signing after repeated misses, and today's way
stays one switch away.

**Why the 31 May attempt failed and this won't.** The old "va-05" gate compared our state, which ran a few ledgers
behind, with the live header, so it mismatched every ledger. In the in-round form every input belongs to the same
ledger.

### Step 6: later, and not needed to call it independent
- **A Rust-only build:** the writer still needs the C++ library at compile time (the `ffi` feature). We decouple
  it and keep the C++ library as a test oracle in the gate, not in the shipped binary.
- **Proposing transaction sets** in consensus: deferred. As a non-UNL validator its proposals barely count.
- **Already done:** voting yes only on rules the engine has built.

## 6. Order, size and proof

| Step | Size | Live risk while building | Done when | Your tap |
|---|---|---|---|---|
| 1. Switch on what's built | small | low (one cycle) | 1-day soak clean | 2 switch-ons |
| 2. Our own metadata | **large** | none (trailing check) | weeks of byte-exact records | — |
| 3. Our own order and results | medium | none (trailing check) | weeks of matching order and results | — |
| 4. Data from peers | medium to large | none until the source switch | weeks of peer = .39, then the switch | source switch |
| 5. Build live and sign our own | medium + weeks of shadow | none in shadow | 5a clean month, then 5b | 5a, 5b |
| 6. Rust-only build | medium | none | the gate passes without the C++ library | — |

```
Step 1 --> ships after the soak running at the push
Step 2 --> Step 3 ---+
Step 4 --------------+--> Step 5 shadow --> 5a live --> (clean month) --> 5b live
         fixBatchV1_2 ported (done 10 Oct) --^
```

Steps 2 and 3 can be built while Step 4 is built. Most of the correctness is proven by trailing checks
**before anything live changes**.

## 7. Risks and how we handle them

| Risk | Handling |
|---|---|
| Mainnet switches on a rule before we've built it, or before its source is out (as fixBatchV1_2 was, 9–10 Oct) | The amendment guard alarms and skips signing. A rule needs 2 weeks of majority to switch on, and that's our porting deadline. |
| A wrong public validation | 5a never publishes a hash the trusted validators disagree with. 5b only after a clean month. |
| Peers throttle our requests | Pace, spread across peers, serve back. .39 stays as backup. |
| Tiny metadata rules | Mainnet's records are an answer key for every transaction, and each mismatch names the field. |
| Close-time edge cases ("no agreed time" ledgers, resolution changes) | Shadow catches them, and stop-on-doubt skips. |
| m3060 goes down | Same as today: it's one validator, and the network doesn't notice. Warm restart. |
| .39 fails (worn SSD) before Step 4 | Until Step 4, .39 is still a single point of failure. Step 4 removes that. |

## 8. What stays the same

- Every new path ships switched off and runs in shadow before it enforces.
- 1-day soak, and fixes ship together.
- You tap Yes before each switch-on.
- A live rule whose source isn't public yet is quarantined: its cases are logged by sequence and hash only, and never
  examined or posted, until the source is out.
- Today's signing stays the instant rollback.

## 9. What I need from you

Nothing yet. **Step 1 is next.** It can be built now and ship when the current soak ends. You'll get a Yes/No tap for
each switch-on.
