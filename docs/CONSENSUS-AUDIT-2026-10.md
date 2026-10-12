# Consensus audit: the March code against rippled 3.4.1

Independence task 5.1 (`docs/INDEPENDENCE-TASKS.md`). Written 12 Oct 2026 on branch `audit-5.1`, which starts from `closetime-5.3` (bf9999c).

- **Ours:** paths are under `crates/xrpl-node/src/` unless a path says otherwise.
- **rippled:** tag 3.4.1 (00e640751460), the copy on .220 at `~/ffi-build/rippled-3.4.1`. Its paths start at the root of that tree.
- **Scope:**
  - `consensus_engine.rs`;
  - the nine modules in `consensus/`;
  - `ledger_close.rs` and `lockstep.rs`;
  - what `bin/live_viewer.rs` and `ws_sync.rs` feed into them, including live_viewer's own `ConsensusMonitor`.
- **Method:** reading only. No code changed and no test changed; [Tests](#tests) says why.

## The answer in one table

| # | Piece | Live today | Verdict | Step 5 needs it |
|---|---|---|---|---|
| 1 | `consensus_engine.rs` (Mempool, ConsensusEngine) | half-wired, frozen since the first close; only its UNL is used | **replace** | no: 5.2 grows from #15 |
| 2 | `consensus/state.rs` | no | **replace** | 5.2 needs rippled's consensus rule, not this |
| 3 | `consensus/threshold.rs` | no | **replace** | 5.2 needs only the observer half |
| 4 | `consensus/proposal.rs`: `compute_tx_set_hash` | no live caller | **keep** (correct since 08-16) | yes: 4.6, 5.2, 5.4 |
| 5 | `consensus/proposal.rs`: `Proposal` | unused | **fix** (or delete) | 5.2 needs a position record |
| 6 | `consensus/close_time.rs` | unused | **delete** | no: 5.4 uses 5.3's `xrpl_ledger::ledger::close_time` |
| 7 | `consensus/amendment_vote.rs`: voting and flag ledgers | yes | **keep** | yes, as today (our validations) |
| 8 | `consensus/amendment_vote.rs`: `AmendmentTracker` | unused | **delete** | no |
| 9 | `consensus/fee_vote.rs` | unused | **delete** | no |
| 10 | `consensus/unl.rs` | yes (the trust set) | **fix** | yes: trust (5.2), quorum (4.5, 5.6) |
| 11 | `consensus/validation.rs` (ValidationCollector) | unused | **replace** (as 4.5) | yes, through 4.5 |
| 12 | `consensus/sig_verify.rs` (S1, S2) | yes | **keep** | yes |
| 13 | `ledger_close.rs` | dead (`if false`) | **delete** | no |
| 14 | `lockstep.rs` | yes, behind `XRPL_LOCKSTEP_SHADOW=1` | **keep until 5.5**, then delete | 5.5 replaces it |
| 15 | live_viewer's `ConsensusMonitor` | yes (display only) | **fix: it becomes 5.2** | yes |

## Three things Step 5 should know

1. **5.2 has an exact answer key every round.**
   - Every rippled validation carries `sfConsensusHash`, the root of the agreed set (`src/xrpld/app/consensus/RCLConsensus.cpp:822`).
   - So 5.2 can check its pick against the hash that the trusted validations of that ledger name. That works every round, with no recordings needed.
   - The task list's check, "the set equals the transactions of the ledger that validates", fails on honest rounds. A transaction that fails, or still wants a retry after the last pass, stays out of the ledger (`src/xrpld/app/ledger/detail/BuildLedger.cpp:96-169`). The ledger's transactions are a subset of the set.
   - 4.6's check has the same flaw. It should compare the root of the fetched set with the agreed hash.
2. **Nothing in our tree computes the quorum.**
   - rippled takes max(ceil(0.8 × effective UNL), ceil(0.6 × UNL)) (`src/xrpld/app/misc/detail/ValidatorList.cpp:1796`). The effective UNL leaves out the negative-UNL entries. For today's 35 with an empty negative UNL, that's 28.
   - Every "80%" in the March code is a float share of the whole UNL. The ValidationCollector also counts a repeated validation twice.
   - 4.5 and 5.6 need one integer quorum function.
3. **Trust is a snapshot.**
   - live_viewer trusts the signing keys it saw at its first good list fetch and never fetches again (`bin/live_viewer.rs:999-1018`).
   - A validator that rotates its key stops counting until we restart.
   - rippled maps each message's signing key to its master key through verified manifests (`ValidatorList.cpp:1346-1353`). 5.2 needs trust by master key.

## compute_tx_set_hash: every caller uses compute_tx_set_root

`compute_tx_set_hash` (`consensus/proposal.rs:21-24`) only wraps `compute_tx_set_root` (`crates/xrpl-ledger/src/shamap/tx_set.rs:54`). These are all the callers of either name in the workspace:

| Caller | Calls | On the live path |
|---|---|---|
| `consensus_engine.rs:167` (`agreed_set_hash`) | `compute_tx_set_hash` | no: nothing calls `agreed_set_hash` |
| `consensus/threshold.rs:88`, `:92` (`is_consensus_reached`) | `compute_set_hash`, its re-export (:100) | no |
| `crates/xrpl-ledger/src/ledger/close.rs:58` (`consensus_salt`, 3.1's order) | `compute_tx_set_root` | not yet (3.3) |
| `crates/xrpl-ledger/src/shamap/tx_set.rs:46` (`compute_tx_set_root_from_blobs`) | `compute_tx_set_root` | no |
| tests: `proposal.rs:48-62`, `threshold.rs:189`, `:196`, `tx_set.rs:72-111` | both | — |

There is no other set-hash function. I searched every crate for `fn *_set_hash`, `fn *_set_root` and `fn *_position_hash`.

**Result: every caller uses the right root, and none is on the live path yet.**

One gap remains:

- The tests pin properties: empty is zero, order doesn't matter, membership does. They also pin the one-leaf structure (`tx_set.rs:111`).
- No test ties a multi-transaction root to a real mainnet hash.
- `sfConsensusHash` closes the gap: take one recorded trusted validation, and compare its `ConsensusHash` with our root of that ledger's transaction ids, on a round where no transaction was left out.

## Piece by piece

Each piece has four lines:

- **(a)** what ours claims;
- **(b)** what rippled 3.4.1 does;
- **(c)** the verdict;
- **(d)** whether Step 5 needs it (5.2: the agreed-set tracker; 5.4: build in the round).

### 1. `consensus_engine.rs`: Mempool and ConsensusEngine (:22-210)

- **(a)** It claims to run the round:
  - it collects transactions while Open (`on_transaction` :89);
  - on CLOSING it takes the mempool as its position (`on_close_trigger` :97);
  - it takes peers' unverified proposals as full lists (`on_peer_proposal` :122);
  - it converges and accepts (`converge` :132), names the set (`agreed_set_hash` :166) and resets (`next_round` :171).

  **Live:**
  - live_viewer creates it (:988), loads the verified list into its UNL (:999-1018) and feeds `on_transaction` (:1289) and `on_close_trigger` (:1318).
  - Nothing anywhere calls `converge`, `on_peer_proposal` or `next_round`. After the first close it sits in `establish(round=1)` for good: the mempool stops filling, `ledger_seq` freezes, and the web status (:1931) shows that frozen state.
  - Its one live job is holding the UNL that the ConsensusMonitor reads (:1067, :1086).
- **(b)** `Consensus<Adaptor>` (`include/xrpl/consensus/Consensus.h`) moves open → establish → accepted on a timer (`phaseEstablish` :1352, `updateOurPositions` :1475, `haveConsensus` :1663).
  - The first position is the open ledger's transactions (`RCLConsensus.cpp:341-355`).
  - When proposing (:358), it adds the pseudo-transactions: the fee and amendment votes on flag ledgers (:360-370) and the negative-UNL vote on voting ledgers (:373-378).
  - Peers' positions are set hashes, and the sets are fetched separately. Disagreements are tracked per transaction (`include/xrpl/consensus/DisputedTx.h`).
- **(c)** Replace. First give the UNL its own `Arc`; after that nothing live needs the engine.
- **(d)** Not needed. 5.2 grows from #15, and 5.4 starts on 5.2's "accepted".

### 2. `consensus/state.rs`: Phase and ConsensusState (:12-108)

- **(a)** Open → Establish{round} → Accept (:12-19).
  - `converge_round` runs one `threshold::update_position` per call and counts the rounds where nothing changed (:63-86).
  - Consensus means at least 2 unchanged rounds plus `threshold::is_consensus_reached` (:89-93).
- **(b)** rippled has the same three phases (`include/xrpl/consensus/ConsensusTypes.h:100`), but consensus is timed, not counted in rounds.
  - `haveConsensus` counts the current proposers who share our position (`Consensus.h:1663-1689`).
  - `checkConsensus` (`src/libxrpl/consensus/Consensus.cpp:160-247`) then decides:
    - never before `ledgerMinConsensus` 1950 ms (`ConsensusParms.h:79`);
    - wait longer if fewer than 3/4 of last round's proposers are present (:188);
    - Yes when agree × 100 / proposers ≥ 80, in integers (:144; `minConsensusPct`, `ConsensusParms.h:69`);
    - MovedOn when 80% have validated a later ledger (:230);
    - Expired when the round runs too long (:241).
  - There is no "unchanged rounds" rule. There is a "stalled" rule (`Consensus.h:1694-1700`).
- **(c)** Replace: port `checkConsensus` and `checkConsensusReached` as pure integer functions.
- **(d)** 5.2 needs `checkConsensus`'s rule in its observer form, not this state machine.
  - `node.rs:55-56` also builds a ConsensusState in Validator mode that nothing reads. That's the `xrpl_node` binary, which no script in the repo runs. Drop the field when state.rs goes.

### 3. `consensus/threshold.rs` (:15-100)

- **(a)** Thresholds go by round: 50/60/70/80/95% (:15-23).
  - A transaction stays in when its trusted support ÷ UNL size reaches the threshold. Our own vote is not counted (:31-74).
  - Consensus means at least 80% of the UNL hold our set hash, as a float (:78-97).
- **(b)** `DisputedTx::updateVote` (`DisputedTx.h:288-330`):
  - **Proposing:** weight = (yays × 100 + (our vote ? 100 : 0)) / (nays + yays + 1) (:316). Our vote flips when the weight is above the needed % (:318).
  - **Observing:** our vote = yays > nays (:324).
  - **The needed %** moves by share of the previous round's time, not by round number: 50%, then 65% at 50%, 70% at 85% and 95% at 200% (`ConsensusParms.h:145-160`; `getNeededWeight` :182).
    - Each state lasts at least 2 rounds (`avMinRounds` :172).
    - A vote counts as stalled after 4 (`avStalledRounds` :178).
  - **Consensus** counts current proposers, not the UNL. It adds one for ourselves only when proposing, and divides in integers (`Consensus.cpp:92-150`).
- **(c)** Replace: port `updateVote`, `getNeededWeight` and `checkConsensusReached` verbatim.
- **(d)** 5.2 needs only the observer half: yays > nays, and `checkConsensusReached` with count_self = false. The proposing half matters only if we ever propose, and no step in the plan does.

### 4. `consensus/proposal.rs`: `compute_tx_set_hash` (:21-24)

- **(a)** The hash a proposal commits to is `compute_tx_set_root` of the ids. It was a placeholder until 2026-08-16 (S3).
- **(b)** The consensus set is a TRANSACTION SHAMap of `TnTransactionNm` leaves keyed by transaction id (`RCLConsensus.cpp:343-355`).
  - Such a leaf hashes to sha512Half(TransactionId prefix, blob), which is the id itself (`include/xrpl/shamap/SHAMapTxLeafNode.h:51-53`). So the root depends only on the ids, and that's what we compute.
  - Validations carry the same root as `sfConsensusHash` (:822).
- **(c)** Keep.
- **(d)** Yes:
  - 4.6 checks a fetched set against it;
  - 5.2 names the set by it;
  - 5.4 salts the canonical order with it (3.1's `consensus_salt`).
  - Note for 5.4: this is the root without metadata. The header's `transaction_hash` is the tree with metadata, a different hash, and 5.4 needs both.

### 5. `consensus/proposal.rs`: `Proposal` (:28-35)

- **(a)** A proposal record: set hash, close time, ledger sequence, previous ledger, propose seq and node key. Nothing uses it.
- **(b)** `ConsensusProposal` (`include/xrpl/consensus/ConsensusProposal.h`) holds:
  - prevLedger, position and closeTime;
  - proposeSeq (`kSeqJoin` = 0 at :47, `kSeqLeave` = 0xffffffff at :50);
  - nodeID and the time it was seen (`isStale` :160).
  - It signs (proposeSeq, closeTime, prevLedger, position) (`signingHash` :239-251). TMProposeSet carries no ledger sequence.
- **(c)** Fix:
  - drop `ledger_sequence`, which isn't on the wire;
  - add the seen time and a bow-out test (`isBowOut` :151);
  - make it 5.2's position record.

  Or delete it and let 5.2 define its own.
- **(d)** 5.2 needs a position record.

### 6. `consensus/close_time.rs` (:6-63)

- **(a)** The resolution goes by round: 30, 20, 10, then 1 s (:15-22).
  - The close time is the most common rounded vote, ours included, if it reaches 50% of the UNL.
  - Otherwise it's our own time plus the no-consensus flag (:28-63).
- **(b)** The resolution belongs to the ledger, not the round. It is one of 10/20/30/60/90/120 s (`include/xrpl/ledger/LedgerTiming.h:17`).
  - `getNextLedgerTimeResolution` (:71) makes it coarser after a ledger without close-time agreement (:43, :94) and finer every 8 agreed ledgers (:38).
  - Votes round to it (`roundCloseTime` :121).
  - Agreement needs 75% of participants (`avCtConsensusPct`, `ConsensusParms.h:165`). `participantsNeeded` rounds that to the nearest whole vote (`Consensus.h:1466-1471`, :1572-1603).
  - The header gets max(rounded, parent + 1 s) (`effCloseTime`, `LedgerTiming.h:145-156`). With no agreement, it gets parent + 1 s and the flag (`RCLConsensus.cpp:466-486`).
- **(c)** Delete. 5.3 replaced it with `xrpl_ledger::ledger::close_time` (branch `closetime-5.3`, checked against the 3,400-header answer key).
- **(d)** 5.4 needs 5.3's code, not this.

### 7. `consensus/amendment_vote.rs`: `is_voting_ledger`, `is_flag_ledger` (:12-20)

- **(a)** A voting ledger is (seq + 1) % 256 == 0. A flag ledger is seq % 256 == 0.
- **(b)** `Ledger::isVotingLedger()` is `isVotingLedger(seq + 1)` (`src/libxrpl/ledger/Ledger.cpp:886-888`). Both free functions are x % 256 == 0 (`src/libxrpl/protocol/Protocol.cpp:5-13`). A voting ledger's validations carry the fee and amendment votes (`RCLConsensus.cpp:838`, :851).
- **(c)** Keep. It matches rippled and is used live (`bin/live_viewer.rs:1479`, `validation.rs:305`).
- **(d)** Yes, as today: 5.6 and 5.7 sign our own validations with the same votes.

### 8. `consensus/amendment_vote.rs`: `AmendmentTracker` (:23-79)

- **(a)** It enables an amendment after 1,344 straight flag ledgers above 80% of the UNL, as a float. One dip resets the count (:45-73). Nothing uses it.
- **(b)** In `src/xrpld/app/misc/detail/AmendmentTable.cpp`:
  - the threshold is max(1, trusted validations × 80 / 100), in integers (:342-346; `kAmendmentMajorityCalcThreshold`, `SystemParameters.h:76`). An amendment passes when its votes are above the threshold (:362).
  - a validator's last vote is kept for 24 h when it misses a flag ledger (`TrustedVotes::recordVotes` :162; the 24 h at :176-179);
  - `doVoting` (:813) emits got/lost majority (:871, :877). It enables the amendment once the majority time + 2 weeks (`kDefaultAmendmentMajorityTime`, `SystemParameters.h:81`) ≤ the close time (:880). That's clock time, not a count of flag ledgers.
- **(c)** Delete. If we ever vote ourselves, port `TrustedVotes` and `doVoting`.
- **(d)** No. EnableAmendment arrives in the agreed set, and 3.5 applies it.

### 9. `consensus/fee_vote.rs` (:8-55)

- **(a)** Network fees are the median of each field. The defaults are 10 drops / 10 XRP / 2 XRP (:17-25). Nothing uses it.
- **(b)** `VotableValue::getVotes` takes the most-voted value between current and target, inclusive (`src/xrpld/app/misc/FeeVoteImpl.cpp:74-90`). `doVoting` uses it on flag ledgers (:175, :263-265). The defaults are stale too: mainnet's reserves are now 1 XRP / 0.2 XRP.
- **(c)** Delete. If we ever vote on fees, port `VotableValue`.
- **(d)** No. SetFee arrives in the set (3.5).

### 10. `consensus/unl.rs` (:10-96)

- **(a)** A set of trusted hex keys (`from_keys` :26, `is_trusted` :43, `trusted_count` :53).
  - It takes manifests without checking the master signature (SECURITY 7.5, :62-83), but nothing live calls `process_manifest`.

  **Live:**
  - It holds the verified list's signing keys, or the master key where the list gave none (`bin/live_viewer.rs:1006-1012`).
  - The keys are loaded once: the fetch loop exits at :1018.
- **(b)** `ValidatorList` trusts master keys from signed publisher lists.
  - It maps each message's signing key to its master key through verified manifests (`ValidatorList.cpp:1346-1353`).
  - Quorum is max(ceil(0.8 × effective UNL), ceil(0.6 × UNL)) (`calculateQuorum` :1708, :1796).
  - A ledger is validated when its trusted validations reach the quorum (`LedgerMaster.cpp:944-946`, :964-968).
- **(c)** Fix:
  - add the quorum function: the integer form of :1796, with the negative UNL read from the ledger;
  - delete `process_manifest`, since Step 1's `unl_verify.rs` is the verified path;
  - key trust by master key, and update signing keys as new verified manifests arrive.
- **(d)** Yes. 5.2 needs the trust, and 4.5 and 5.6 need the quorum (28 of 35 today).

### 11. `consensus/validation.rs`: ValidationCollector (:21-91)

- **(a)** It pushes each validator key into a list per ledger hash (:35-45), so a repeat counts twice.
  - Fully validated means trusted entries ÷ UNL ≥ 0.80, as a float (:48-66).
  - `prune` drops the least-validated ledgers first, not the oldest (:76-90).
  - Nothing uses it. `bin/live_viewer.rs:2231` mentions it as future work.
- **(b)** In `include/xrpl/consensus/Validations.h`:
  - `add` keeps one current validation per node (`current_.emplace` :690) and sorts the rest into Stale, BadSeq, Multiple and Conflicting (:631-713);
  - a validation counts only while `isCurrent` holds, which bounds its sign and first-seen times (:146-161; `validationCurrentWall` 5 min, `Local` 3 min and `Early` 3 min at :46-63);
  - `numTrustedForLedger` counts per ledger (:1031), and `LedgerMaster::checkAccept` compares that count with the quorum (:950-968).
- **(c)** Replace, as 4.5: one current validation per trusted master key, `isCurrent`, the integer quorum from #10 and S2 signatures.
- **(d)** Yes, through 4.5. 5.5's gauge and 5.6's wait both read "validated".

### 12. `consensus/sig_verify.rs`: S1 and S2 (:26-296)

- **(a)** It provides:
  - the proposal signing hash (:84) and its check (:101);
  - the validation split (:230) and its check (:267);
  - counters, metrics and the enforce switch (:35-76, :296).
- **(b)**
  - Proposals: `verifyDigest(publicKey, signingHash, sig, false)` (`src/xrpld/app/consensus/RCLCxPeerPos.cpp:36-38`), run before the trust routing (`src/xrpld/overlay/detail/PeerImp.cpp:3183-3195`).
  - Validations: `STValidation::isValid` (`src/libxrpl/protocol/STValidation.cpp:101-129`).
- **(c)** Keep. It runs live with 0 failures (1.08 M checks at last count).
- **(d)** Yes: 5.2 should take only checked proposals, and 4.5 needs S2. Until 1.2 is switched on, a proposal that fails its check still reaches the monitor (observe mode, `bin/live_viewer.rs:2215`).

### 13. `ledger_close.rs` (:1-293)

- **(a)** It builds a ledger from raw transactions:
  - it loads only the senders' and Payment destinations' AccountRoots from RocksDB, through a decode that strips trailing bytes (:110-117);
  - it runs the JSON apply pipeline with the resolution hard-coded to 10 (:248) and the header's close time set to the parent's (:191).
  - It is marked DEAD CODE (:11-22). Its only call sits inside `if false {}` (`bin/live_viewer.rs:1210-1222`).
- **(b)** `buildLedgerImpl` and `applyTransactions` (`BuildLedger.cpp:38`, :96-169) apply the agreed set to the whole parent state, in canonical order with retry passes. The header's close time comes from the vote (`RCLConsensus.cpp:466-486`).
- **(c)** Delete, with the `if false` block. That block is in the live binary, so the change rides a normal cycle.
- **(d)** No. 5.4 builds on Steps 2–3 and on 5.3's close time.

### 14. `lockstep.rs` (:1-61)

- **(a)** `evaluate_ledger` recomputes the ledger hash from the network's header fields plus our account hash, and compares it with the network's (:19-36).
  - `LockstepMeter` keeps the match share over the last 1,000 ledgers (:41-61).
  - It's used by `ws_sync.rs:25-27`, :535 and :576, behind `XRPL_LOCKSTEP_SHADOW=1` (:104), and by `tests/lockstep.rs`.
- **(b)** The hash is `calculateLedgerHash` over the header (`src/libxrpl/ledger/Ledger.cpp:247`). Every input except the account hash comes from the network, so this checks our state, not our round.
- **(c)** Keep until 5.5 replaces it (the task list says so), then delete.
- **(d)** 5.5 replaces it.

### 15. live_viewer's `ConsensusMonitor` (`bin/live_viewer.rs:80-161`; fed at :1057-1100 and :2191-2226)

- **(a)** For each trusted validator, it keeps the latest position, propose seq, close time and seen time (:86, :102-113).
  - It drops entries older than 15 s (:115-122).
  - It reports the most-held position as count ÷ UNL size (:124-152) and logs that every second (:1079-1099).
  - It's fed signature-checked trusted proposals (:2197-2222, :1062-1073). It's display only.
- **(b)** `peerProposalInternal` (`Consensus.h:753-805`):
  - drops a proposal on another prevLedger (:765);
  - ignores nodes that have bowed out (:774);
  - keeps a proposal only if its propose seq is higher (:786);
  - removes a node that bows out (`kSeqLeave`, :792-802).

  Positions go stale 20 s after they were seen (`proposeFRESHNESS`, `ConsensusParms.h:55`; removed at `Consensus.h:1495`). Agreement counts current positions against ours (:1663-1689), then `checkConsensus` decides.
- **(c)** Fix; this becomes 5.2. What's wrong today:
  1. **No prevLedger.** `PeerProposal` carries `previous_ledger` (:177, :2221), but `record` drops it. A late proposal from the last round, or one on another prevLedger, counts toward this round.
  2. **The seq reset.** A position is kept only if `propose_seq >= entry.1` (:110). Every round restarts at 0 (`kSeqJoin`), so a validator's new position is ignored until its seq catches up with last round's or the old entry ages out.
  3. **Bow-outs.** A bow-out has seq 0xffffffff, so it passes `>=` and counts as a position. It then blocks that validator's next positions until it ages out.
  4. **Freshness.** 15 s here; rippled uses 20 s.
  5. **Denominator.** The UNL size, as a float, instead of the current proposers. There's no 1950 ms rule and no 3/4 rule.
  6. **Trust.** It's the snapshot from #10.
- **(d)** Yes: it's the seed of 5.2. Its close times can also feed 5.3's vote for 5.4.

## What 5.2 should start with

- **Record first.**
  - The task list's 5.1 check, "tests against recorded mainnet rounds", needs recordings that don't exist yet. No proposals are recorded in the repo or in .220's answer-key folders.
  - So make recording 5.2's first piece, run in shadow. For each round it records:
    - every checked trusted proposal: master key, prevLedger, position, close time, seq and seen time;
    - every trusted validation's ledger hash and `ConsensusHash`;
    - the header that followed.
  - A few days of recordings give fixtures for:
    - the tracker;
    - the set-hash test above;
    - a check of 5.3's vote against real votes (5.3 so far checks the headers, not the votes).
- **Then the rule, in rippled's observer form:**
  1. Keep one position per trusted master key for the current prevLedger.
     - Ignore an equal or lower seq (`Consensus.h:786`).
     - When a validator bows out, drop it for the rest of the round (:792-802).
     - Drop a position 20 s after it was seen.
  2. The agreed set is the position where agree × 100 / current positions ≥ 80 (`Consensus.cpp:144`).
     - Never call it before 1950 ms after the close (the CLOSING that live_viewer already sees, :1318).
     - Wait if fewer than 3/4 of last round's proposers are present (`Consensus.cpp:188`).
  3. Every round, check the pick against the `ConsensusHash` in the quorum of trusted validations for that ledger.

## Tests

- I changed no tests. I found none that is wrong as a test.
- The tests in `threshold.rs`, `close_time.rs` and `fee_vote.rs`, and the AmendmentTracker ones, assert the March rules.
  - They pass because they agree with the March code, not with rippled.
  - They go when their modules go. Rewriting them now would only turn them red against code that is being replaced.
- The set-hash tests are right as far as they go; the gap is above.

## Clean-up list for a normal cycle (none of it done here)

1. **live_viewer:** give the UNL its own `Arc`, stop feeding the engine, and drop or label its frozen status (:1931). Then delete `consensus_engine.rs`.
2. **Dead code:** delete `ledger_close.rs` and the `if false` block (`bin/live_viewer.rs:1210-1222`).
3. **Unused modules:** delete `consensus/close_time.rs`, `consensus/fee_vote.rs` and `AmendmentTracker`.
4. **When 5.2 needs them:** replace `state.rs` and `threshold.rs` with the `checkConsensus` and `updateVote` ports, and drop `node.rs`'s ConsensusState field at the same time.
5. **Trust and validations:**
   - `unl.rs`: add the quorum function, key trust by master key, and remove `process_manifest`.
   - `consensus/validation.rs`: replace it in 4.5.
