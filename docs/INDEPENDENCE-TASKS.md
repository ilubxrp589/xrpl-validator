# Independence: the task breakdown

*9 October 2026. The tasks behind each step of `docs/INDEPENDENCE-PLAN.md`, with my recommendation for how to run
them once you give the push.*

## How to read this

- **Size** is my build effort, not counting soaks or shadow weeks:
  - **S** = 1–2 days
  - **M** = 3–5 days
  - **L** = 1–2 weeks
- **Check** is how we prove a task right before it touches anything live. Nearly every task has an answer key from
  mainnet: its metadata, its order, its headers.
- **Tap** marks a switch-on that waits for your Yes on Telegram.
- Every new path ships switched off and runs in shadow before it enforces. Fixes ship together after a 1-day soak.

## The recommendation in one page

1. **Week 1 of the push: Step 1, and start the two "answer key" jobs.**
   - Step 1 is small and ships in one cycle.
   - In the same week, start the metadata field table (2.1) and the close-time answer key (5.3). Both are pure code
     checked against mainnet history, so they carry no risk. They also take on the two things I'm least sure of
     early.
2. **Weeks 2–4: Step 2 (metadata), then Step 3 (order and results).**
   - Build offline first: the probe compares our records with mainnet's over the whole fixture library before
     anything is deployed.
   - Then a shadow leg on m3060.
3. **Weeks 4–6: Step 4 (data from peers),** run in "both" mode (peers and .39 side by side) until they agree for
   weeks.
4. **Weeks 6–8: Step 5 in shadow,** with m3060 computing its own hash at the live edge and only logging it.
5. **Then 5a live for a month (your tap), then 5b (your tap).**

**Rough calendar:** 5a live about 2 months after the push, and 5b about a month after that. Treat this as a guess:
Step 2's receipts and the live-edge change (5.4) are where it can stretch.

**Staffing:**
- I drive and build.
- Sonnet subagents do the mechanical parts: generating tables from rippled's source, and scanning history for
  answer keys.
- The agent team can stay paused. If receipts pile up during Step 2's shadow, unpausing it for drilling is the
  natural use.
- Builds use .220 with the lease, and I ask the other session first.

**Milestones you'll see:**
1. The first ledger whose transaction tree we built ourselves matches mainnet (Step 2).
2. A full day of 100% matching records.
3. m3060 fetches its first ledger from peers (Step 4).
4. The first in-round hash that matches mainnet (Step 5 shadow).
5. The first validation of our own hash (5a).

## Step 1: switch on what's already built

| # | Task | Size |
|---|---|---|
| 1.1 | Amendment guard | S |
| 1.2 | Enforce proposal signatures (**tap**) | S |
| 1.3 | Validator list fails closed (**tap**) | S |
| 1.4 | Ship as one cycle | S |
| 1.5 | Port fixBatchV1_2 (blocked on the source) | M? |

**1.1 Amendment guard.**
- **What:**
  - On every flag ledger, read the ledger's Amendments object (the engine already reads it) and compare it with a
    `ENGINE_KNOWN` list.
  - `ENGINE_KNOWN` is every amendment live on mainnet at deploy time, plus each new one as it's ported.
  - fixBatchV1_2 goes on a short "known gap" list until it's ported.
  - Anything enabled that's on neither list sends a ⚠️ ALARM to Telegram and shows in the API, the metrics and the TUI.
  - From Step 5 on, it also blocks signing our own hash.
- **Check:** unit tests with made-up Amendments objects. Replaying 9 October's activation ledger with fixBatchV1_2
  taken off the known-gap list must raise the alarm.

**1.2 Enforce proposal signatures.**
- **What:** set `XRPL_SIG_ENFORCE=1`, which drops proposals whose signatures fail, and show the dropped count.
- **Check:** already done. About 1.1 million checks, 0 failures.
- Validations aren't used for decisions yet, so their drop lands with their first user (4.5).

**1.3 Validator list fails closed.**
- **What:** pin Ripple's publisher key (`XRPL_UNL_PINNED_KEY`) and set `XRPL_UNL_ENFORCE=1`. An unverified list is
  refused and the last good one is kept.
- **Check:** it already verifies (list #85, 35 validators). Add tests for a tampered list, an expired list and an
  older sequence number. All three must be refused and keep the last good list.

**1.4 Ship.** One cycle (gate, windows, deploy, soak). The two switch-ons are your taps.

**1.5 Port fixBatchV1_2.**
- **What:** when the 3.4.1 source is public, read the change, port it, and check it with the probe against every
  Batch ledger since 9 October.
- **Size:** unknown until we see the source.
- This must be done before 5a.

## Step 2: our own transaction records (metadata)

| # | Task | Size |
|---|---|---|
| 2.1 | Field metadata table | S–M |
| 2.2 | Before-and-after capture per transaction | S–M |
| 2.3 | The AffectedNodes builder | M |
| 2.4 | Result fields: result, position, DeliveredAmount, ParentBatchID | M |
| 2.5 | Binary encoding of the record | S |
| 2.6 | Probe comparison over the fixture library | M |
| 2.7 | Shadow leg on m3060 (`XRPL_NATIVE_META=1`) | M |
| 2.8 | Drill the receipts | ongoing |

**2.1 Field metadata table.**
- **What:**
  - rippled marks every field with when it belongs in a record:
    - changed: goes in PreviousFields;
    - always or on change: goes in FinalFields;
    - at creation: goes in NewFields;
    - at deletion: goes in the final fields of a deleted object.
  - Nothing like this exists in our code yet.
  - Generate it from the rippled source with a script, the same way the amendment list is regenerated.
- **Check:** spot-check fields against mainnet records. Balance, Flags, OwnerCount and the threading fields each
  appear exactly where mainnet puts them.

**2.2 Before-and-after capture.**
- **What:**
  - Each transaction already runs in its own sandbox.
  - The sandbox holds the "after" of every object (created, modified or deleted).
  - We add the "before", read from the state the transaction started on.
  - We also handle the "touched but unchanged" cases, which follow rippled's rule (finding 335).
- **Check:** for fixture transactions, the set of touched keys equals the set of keys in mainnet's AffectedNodes.

**2.3 The AffectedNodes builder.**
- **What:**
  - Created objects get NewFields.
  - Modified objects get FinalFields, plus PreviousFields for the marked fields that changed.
  - Deleted objects get their final fields, plus any marked fields that changed before deletion.
  - Each node carries its type, its key and its threading link.
  - Nodes are listed in key order.
- **Check:** compare with mainnet's records as structured data first. That gives readable diffs, before we compare
  bytes.

**2.4 Result fields.**
- **What:**
  - TransactionResult.
  - TransactionIndex: in Step 2 still the network's position; Step 3 makes it ours.
  - DeliveredAmount, on the transactions where rippled records what was actually delivered. Each transactor reports
    what it delivered.
  - ParentBatchID on Batch inner transactions (the F417 knowledge).
- **Check:** per type, against mainnet.

**2.5 Encoding.**
- **What:** write the record in the network's exact binary format.
- **Check:** byte-for-byte against mainnet records in the fixture library.

**2.6 Probe comparison.**
- **What:** the differential probe compares our record with mainnet's for every fixture and campaign transaction.
  It reports the first field that differs and keeps counts per transaction type.
- **Why first:** most of the receipts show up here, offline, before anything is deployed.
- **Check:** this is the check. The target is 100% across the library.

**2.7 Shadow leg on m3060.**
- **What:**
  - Every ledger, build all the records and compute our transaction-tree root, using the code already tested
    against mainnet.
  - Compare it with the header's.
  - Counters, a receipts log, and a TUI line.
- **Check:** runs next to the writer, so nothing it does is used live.

**2.8 Drill.** Every mismatch names its transaction and field, and we drill it the same way as today's receipts.

**Step 2 is done when** every ledger's transaction tree matches for 2 weeks.

## Step 3: our own order and results

| # | Task | Size |
|---|---|---|
| 3.1 | Canonical order | S–M |
| 3.2 | The retry passes | M |
| 3.3 | Writer uses our order and results (shadow, then on) | S–M |
| 3.4 | Fees burned and total XRP | S |
| 3.5 | System transactions taken from the set | S |

**3.1 Canonical order.**
- **What:** port rippled's sort:
  1. each account scrambled with a salt taken from the set's fingerprint (S3 already computes that fingerprint);
  2. then sequence number or ticket;
  3. then transaction id.
- **Check:** on ledgers with no retries, our order must equal mainnet's position numbers exactly.

**3.2 The retry passes.**
- **What:** port rippled's loop:
  - up to 3 passes over the set;
  - an applied result (success or a claimed fee) is final;
  - a hard failure is dropped from the ledger;
  - a "retry" result goes again in the next pass;
  - the loop stops early when a pass changes nothing.
- **Check:** scan history for ledgers whose positions differ from the plain canonical order. Those are the retry
  cases. Replay them all.

**3.3 The writer uses our order and results.**
- **What:** shadow first, comparing our positions and results with mainnet's on every ledger. Then the writer
  applies in our order, still checking against mainnet's records.
- **Check:** counters and receipts, as in 2.7.

**3.4 Total XRP.**
- **What:** the parent's total minus the fees burned, compared with the header's every ledger.

**3.5 System transactions** (fee votes, amendment votes, the negative UNL on flag ledgers).
- **What:** the engine already applies them. Check that their position and results come out of our order correctly.

**Step 3 is done when** order and results match on every ledger for 2 weeks.

## Step 4: data from peers, not .39

| # | Task | Size |
|---|---|---|
| 4.1 | Ledger fetch client | M |
| 4.2 | Peer choice and pacing | M |
| 4.3 | "Both" mode, then switch the source (**tap**) | M |
| 4.4 | Catch-up from peers after a restart | M |
| 4.5 | Validation collector: "validated" without .39 | M |
| 4.6 | Fetch the candidate transaction set in the round | M |

**4.1 Ledger fetch client.**
- **What:**
  - Send `TMGetLedger` for a ledger's header and transaction tree.
  - Read the replies.
  - Rebuild the tree and check every piece by fingerprint, up to the header and the validated ledger hash.
- **Today:** m3060 already answers these requests for other peers, but has never sent one.
- **Check:** unit tests with recorded replies, then live fetches compared with .39's copy.

**4.2 Peer choice and pacing.**
- **What:**
  - Ask peers that say they hold the ledger, and spread the load.
  - Timeouts and retries, a cap on requests per second, and a score for each peer.
- **Why:** xrpld cuts off greedy requesters. .39's limits throttled m3060 on 4 Aug.
- **Check:** a week in "both" mode with no peer cutting us off.

**4.3 Both mode, then the switch.**
- **What:** a source setting (`peers`, `rpc` or `both`). "Both" compares every ledger's transactions and records
  byte for byte.
- **The switch:** after weeks of agreement, the source moves to peers (your tap). .39 becomes a witness and backup.

**4.4 Catch-up.**
- **What:** after a warm restart, fetch the missed ledgers from peers and apply them in order.
- A from-scratch rebuild may still use .39 or a peer. It's checked against the validated state fingerprint either
  way.

**4.5 Validation collector.**
- **What:**
  - From signature-checked validations (S2 enforced here), count the trusted validators that sign each ledger's
    hash.
  - 28 of the 35 on the verified list makes a ledger validated.
- **Why:** this replaces .39 telling m3060 which ledger is validated. 5a needs it too.
- **Check:** our "validated" against .39's, every ledger.

**4.6 Candidate set in the round.**
- **What:** fetch the transaction set the trusted validators' proposals name (`liTS_CANDIDATE`) while consensus is
  finishing. It's built here because it's the same plumbing as 4.1.
- **Check:** the set we fetched equals the transactions of the ledger that then validates.

## Step 5: build at the live edge and sign our own hash

| # | Task | Size |
|---|---|---|
| 5.1 | Audit the March consensus code | S–M |
| 5.2 | Agreed-set tracker | M |
| 5.3 | Close time: the answer key and the code | M |
| 5.4 | Build in the round | **M–L** |
| 5.5 | Shadow gauge | S |
| 5.6 | 5a: verify, then sign, with stop-on-doubt and a breaker (**tap**) | M |
| 5.7 | 5b: sign on compute (**tap**) | S |

**5.1 Audit the March consensus code.**
- **What:** there's a consensus scaffold from March: `consensus_engine.rs`, plus close time, thresholds and state
  in `consensus/`. One of its pieces, the tx-set fingerprint, was already found wrong and fixed (S3). Keep what
  matches rippled and list the rest.
- **Check:** tests against recorded mainnet rounds (their proposals and the header that followed).

**5.2 Agreed-set tracker.**
- **What:** from the trusted proposals in a round, find the transaction set the network accepts: the set a large
  majority holds in their final positions.
- **Check:** the set we pick equals the transactions of the ledger that validates, every round, in shadow.
- **This is one of my two biggest unknowns,** so it gets measured early, in shadow.

**5.3 Close time.**
- **What:** port rippled's rules:
  - round the validators' close-time votes to the current resolution;
  - the "no agreed time" flag;
  - the close time is always later than the parent's;
  - the resolution schedule.
- **Check:** months of mainnet headers are an exact answer key. Every header's close time, resolution and flags
  must come out of our code. **Can start in week 1**, because it's pure code with no live parts.

**5.4 Build in the round.**
- **What:**
  - When consensus accepts, fetch the set (4.6) and apply it (Steps 2–3).
  - Build the header: the parent is our N−1, plus total XRP, both roots, and the times.
  - Take its fingerprint: our ledger hash.
- **The big change:** the writer moves from "after mainnet validates" to "at the live edge". If our hash then
  disagrees with the validated one, we must undo cleanly. That's the second of my two biggest unknowns.
- **Check:** shadow (5.5).

**5.5 Shadow gauge.**
- **What:** our in-round hash against the validated hash (from 4.5), every ledger, with a metric, a TUI line and
  receipts. It replaces the July lockstep gauge.
- **Done when** the gauge holds 100% for weeks.

**5.6 5a: verify, then sign.**
- **What:**
  - Sign our own hash only when at least 28 trusted validations for ledger N name the same hash, waiting about 1–2
    seconds at most.
  - **Stop on doubt**, with a counter for each reason:
    - an incomplete set;
    - a refusal or engine error;
    - the amendment guard firing;
    - running out of time;
    - no quorum.
  - A breaker drops back to today's signing after repeated misses.
  - A setting picks the mode (`XRPL_SIGN_SOURCE=phase3|verified_own|own`).
- **Check:** a month live with every signature our own and 0 skips we can't explain.

**5.7 5b: sign on compute.**
- **What:** the same path without the wait. A later disagreement trips the breaker and sends an alarm.

## Step 6: a Rust-only build (later)

| # | Task | Size |
|---|---|---|
| 6.1 | The writer and shadow build without the `ffi` feature | S–M |
| 6.2 | The C++ library moves into the gate as a test oracle | S |
| 6.3 | Ship a binary without the C++ library, after a soak | S |

## Across all steps

- **Answer keys without touching .39's disk.** Fixtures and history scans come from a public full-history server or
  the RPC cache. There is no bulk reading on .39's worn ledger SSD.
- **An "independence" panel** on the TUI and on `/tui`: five lights (state, records, order, data, hash) showing which
  inputs are ours. Small, and it makes progress visible on your phone.
- **The plan doc stays current.** Each finished task updates `docs/INDEPENDENCE-PLAN.md` and the daily report.

## When you push

Say go and I'll start with **1.1–1.4** (shipping after the soak in progress then), plus **2.1** and **5.3** in parallel.
You'll get a tap for each switch-on, and a short Telegram line at each milestone.
