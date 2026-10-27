# Step 4 Peer Switch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `XRPL_LEDGER_SOURCE=peers` makes live_viewer take every ledger from mainnet peers instead of .39, so .39 can be switched off and the 30-day count can start. A non-signing twin proves it before the live validator is touched.

**Architecture:** Our own tally of trusted validations (own_validated) says which ledger is validated, and which hash. peer_fetch fetches that ledger (header plus transaction tree, proven to the hash), and peer_source turns it into the exact inputs ws-sync takes from .39 today (Stage A has held them equal on 42,106 of 42,106 ledgers). ws-sync's loop runs as before. Peer mode changes only four things:
- its trigger is our quorum, not .39's `ledgerClosed`;
- each ledger's inputs come from peers;
- the system objects (skip list, fees, amendments, negative UNL) come from our own engine;
- any ledger the engine can't vouch for is written from state objects proven by peers, never from RPC.

A warm restart catches up the same way. A cold start loads a state tree that peers sent (Step 8's tool). .39 stays on only as an optional witness and a rollback (`XRPL_LEDGER_SOURCE=rpc`).

**Tech Stack:** Rust 1.94 (`cargo +1.94.0`), tokio, rocksdb, prost (peer protocol), the existing xrpl-ledger SHAMap wire code. Builds and tests run on .220 only (never on .39).

**Spec:** `docs/INDEPENDENCE-PLAN.md` § "Step 4: data from peers, not .39" and `docs/INDEPENDENCE-TASKS.md` 4.2–4.4. Stage A (4.1, 4.5) is live since cycle 186: `peer_source.rs`, `peer_fetch.rs`, `own_validated.rs`.

## Global Constraints

- **Default is unchanged.** With `XRPL_LEDGER_SOURCE` unset or `rpc`, every code path behaves byte for byte as on ddb157a. Peer-mode code sits behind `ledger_source::peers()`.
- **Zero RPC in peer mode.**
  - Peer mode makes no request to `XRPL_RPC_URL`, `XRPL_WS_URL` or the hardcoded public fallbacks (`rippled_client.rs` lines 9-17).
  - Allowed traffic: the peer protocol, the validator list publisher (vl.ripple.com), SNTP, and `XRPL_WITNESS_RPC` when it is set.
- **Never skip a ledger.**
  - A ledger peers can't deliver holds position and retries, as `meta_ok == false` does today (ws_sync.rs ~:688).
  - The state hash still checks every ledger against the header's `account_hash`.
- **Trust comes from the quorum.**
  - A ledger is taken only by a hash the trusted quorum named, or by a hash proven to be the parent of one.
  - Every header must hash to the ledger hash. Every tree node must be the one its parent names.
- **Peer politeness.** Per-peer pacing stays `PEER_GAP` = 8 s. No new request type is sent faster than that.
- **Signing rules don't change.** 5a still signs only our own hash once the quorum names it. Peer mode removes the WITNESS basis unless `XRPL_WITNESS_RPC` is set.
- **New env switches**, all off by default:
  - `XRPL_LEDGER_SOURCE=rpc|peers`
  - `XRPL_WITNESS_RPC=<url>`
  - `XRPL_OBSERVE=1`
- **Tests:**
  - every new test passes in `cargo +1.94.0 test -p xrpl-node --lib`, and in `--features ffi` where it touches ffi-gated code;
  - clippy on changed files shows no warnings the base doesn't already have (base count 137 for `-p xrpl-ledger`; record the node baseline in Task 1).
- **Commits** end with the session's Co-Authored-By and Claude-Session lines. Push only to `buildbox:xrpl-build/xrpl-validator`, never origin, until the cycle ships.

## Review Focus

1. **The newest ledger isn't on peers yet** (quorum reached, peers still closing it): the fetch retries other peers and the loop holds the seq, so no ledger is skipped and none is fetched twice into the state. Test is in Task 7 (`a_ledger_peers_cannot_deliver_holds_the_loop_at_that_seq`).
2. **Quorum short on some sequences** (27 of 28 seen, as on 10-20): their hashes come from the next quorum ledger's parent chain, and they are applied but never signed. Test is in Task 5 (`a_seq_without_quorum_takes_its_hash_from_the_next_quorum_ledgers_parent`).
3. **A flag ledger** (negative UNL rotation, amendment pseudo-transactions, the 65,536-group LedgerHashes):
   - the engine's own system objects must make the state root;
   - if they don't, the keyed peer fetch writes the network's.
   - Test is in Task 6 (`singleton_writes_cover_every_changed_system_object`), plus the twin's soak.
4. **Restart in peer mode before peers or the list are up:** warm resume waits up to 120 s for a quorum, then exits 75. In peer mode the launcher must then run the peer cold start, never the .39 bulk sync. Test is in Task 8 (`peer_network_without_a_quorum_gives_none_after_the_wait`).
5. **Low memory at start-up** (hydrate skipped): every ledger falls back to the keyed peer fetch and still lands, slower. Test is in Task 7 (`an_unready_writer_takes_the_keyed_peer_fetch`).

---

## File Structure

| File | Role |
|---|---|
| `crates/xrpl-node/src/ledger_source.rs` (new) | The switch: `mode()`, `peers()`, counters, `/api/engine` + metrics section |
| `crates/xrpl-node/src/peer_ledgers.rs` (new) | Hash for a seq (quorum or parent chain), one ledger's inputs from peers, the quorum trigger stream |
| `crates/xrpl-node/src/peer_fetch.rs` | Driver generalised: awaited jobs (ledger, header, state keys) beside Stage A's background compare |
| `crates/xrpl-node/src/peer_state.rs` | `KeyedStateFetch`: only the state objects named, proven to `account_hash` |
| `crates/xrpl-node/src/own_validated.rs` | `newest_quorum()`, `QUORUM_MOVED` notify |
| `crates/xrpl-node/src/native_shadow.rs` | `singleton_writes()` |
| `crates/xrpl-node/src/ws_sync.rs` | Peer-mode trigger, inputs, overlays; `process_ledger(rpc: Option<..>)` |
| `crates/xrpl-node/src/resume.rs` | `PeerNetwork` |
| `crates/xrpl-node/src/rippled_client.rs` | Refuse and count in peer mode |
| `crates/xrpl-node/src/ffi_verifier.rs`, `ffi_engine.rs` | Amendments from state.rocks; consult budget 0 in peer mode |
| `crates/xrpl-node/src/witness.rs` (new) | Optional `XRPL_WITNESS_RPC` cross-check |
| `crates/xrpl-node/src/observe.rs` (new) | `XRPL_OBSERVE`: no seed, nothing signed or sent |
| `crates/xrpl-node/src/bin/live_viewer.rs` | Wiring: resume network, no RPC pollers in peer mode, observe gates |
| `crates/xrpl-node/src/bin/peer_state_sync.rs` | Also writes `DIR/header.bin` |
| `crates/xrpl-node/src/bin/leaves_to_rocks.rs` (new) | Peer cold start: leaves → state.rocks + bookmark + ticket + markers |
| `scripts/twin/run-twin.sh`, `scripts/twin/twin_check.py`, `scripts/peer_cold_start.sh` (new) | The twin rig and the cold start |

---

### Task 1: The source switch and the RPC guard

**Files:**
- Create: `crates/xrpl-node/src/ledger_source.rs` (add `pub mod ledger_source;` to `lib.rs`)
- Modify: `crates/xrpl-node/src/peer_fetch.rs:286-288` (`is_source`), `crates/xrpl-node/src/rippled_client.rs` (`rpc_request`, `ws_url`)

**Interfaces:**
- Produces:
  - `pub enum Source { Rpc, Peers }`;
  - `pub fn parse(v: Option<&str>) -> Source` (unset/`rpc` → Rpc, `peers` → Peers, anything else → Rpc with one `eprintln!` warning);
  - `pub fn mode() -> Source` (env read once, `OnceLock`);
  - `pub fn peers() -> bool`;
  - `pub struct SourceStats { pub from_peers, pub keyed_fallbacks, pub chain_walks, pub holds, pub rpc_refused: AtomicU64 }`, `pub fn stats() -> &'static SourceStats`;
  - `pub fn json() -> Value` (`{"mode": "peers"|"rpc", ...counters}`), `pub fn metrics_text() -> String` (`xrpl_ledger_source_*` lines).
- `peer_fetch::is_source()` returns `ledger_source::peers()`.

- [ ] **Step 1:** Record the clippy baseline: `cargo +1.94.0 clippy -p xrpl-node --features ffi 2>&1 | grep -c '^warning'`. Write the number into the task's commit message.
- [ ] **Step 2: Write the failing tests** in `ledger_source.rs`:
  - `parse_reads_the_three_spellings`: `parse(None)==Rpc`, `parse(Some("rpc"))==Rpc`, `parse(Some("peers"))==Peers`, `parse(Some("PEERS "))==Peers`, `parse(Some("both"))==Rpc`.
  - In `rippled_client.rs`, `peer_mode_refuses_every_call_and_counts_it`. Use the existing `RippledClient` with a test hook `set_peer_mode_for_test(true)`. Then `rpc_request(&json!({"method":"server_info"}))` returns `Err` containing `"peer source: no RPC"`, and `stats().rpc_refused` grows by 1.
- [ ] **Step 3:** Run `cargo +1.94.0 test -p xrpl-node --lib ledger_source rippled_client`. Expect a FAIL (no module).
- [ ] **Step 4: Implement** (the guard reads `ledger_source::peers()`, or the test hook):
  - `rpc_request` returns `Err("peer source: no RPC (…method…)")` before any network I/O;
  - `ws_url()` in peer mode returns the primary only and never rotates.
- [ ] **Step 5:** Run the tests. Expect a PASS. Also run `cargo +1.94.0 test -p xrpl-node --lib` with no new failures.
- [ ] **Step 6:** Commit `feat(step4): XRPL_LEDGER_SOURCE switch; peer mode refuses and counts every RPC call`.

### Task 2: own_validated tells when the quorum moves

**Files:** Modify `crates/xrpl-node/src/own_validated.rs`

**Interfaces:**
- Produces:
  - `impl Tally { pub fn newest_quorum(&self) -> Option<(u32, [u8; 32])> }`: the highest seq whose `reached` holds exactly one hash.
  - Module fns `pub fn newest_quorum() -> Option<(u32, [u8; 32])>` and `pub static QUORUM_MOVED: tokio::sync::Notify`. `notify_waiters()` fires when `vote()` makes a seq reach quorum that is newer than the previous newest.

- [ ] **Step 1: Write the failing tests:**
  - `newest_quorum_follows_the_highest_reached_seq`: in a Tally with a 4-validator list (quorum 4 by the existing `quorum()`), 4 votes for #10 give `Some((10,h10))`; 3 votes for #11 still give #10; the 4th gives #11.
  - `#[tokio::test] the_notify_fires_when_a_newer_seq_reaches_quorum`: spawn a waiter on `QUORUM_MOVED.notified()`, drive the module-level vote path, and the waiter completes within 100 ms.
- [ ] **Step 2:** Run `cargo +1.94.0 test -p xrpl-node --lib own_validated`. Expect a FAIL.
- [ ] **Step 3:** Implement. Notify only on a strictly newer quorum seq: no wake per vote.
- [ ] **Step 4:** Run the tests. Expect a PASS.
- [ ] **Step 5:** Commit `feat(step4): own_validated says when a newer ledger reaches quorum`.

### Task 3: peer_fetch serves awaited jobs

**Files:** Modify `crates/xrpl-node/src/peer_fetch.rs` (the driver at :469-560 and `Event` at :372)

**Interfaces:**
- Consumes: `LedgerFetch` (as is), `peer_state::KeyedStateFetch` (Task 4; until then the state job is stubbed).
- Produces:
  - `pub async fn fetch_ledger(seq: u32, hash: [u8; 32]) -> Result<(LedgerHeader, Vec<(Vec<u8>, Vec<u8>)>), String>`: header plus pairs in tree order, header proven to `hash` and `header.sequence == seq`.
  - `pub async fn fetch_header(hash: [u8; 32]) -> Result<LedgerHeader, String>`: liBASE only.
  - `pub async fn fetch_state_keys(hash: [u8; 32], account_hash: [u8; 32], keys: Vec<[u8; 32]>) -> Result<std::collections::HashMap<[u8; 32], Option<Vec<u8>>>, String>`.
  - Each returns `Err` after its deadline: `LEDGER_DEADLINE` (45 s) for the ledger and the state keys, 15 s for a header.
- Driver rules:
  - Jobs carry a `oneshot::Sender`.
  - Awaited jobs are never dropped by `QUEUE_CAP`, which still caps Stage A's `Want`s.
  - Up to `pub const MAX_ACTIVE: usize = 4` jobs run at once.
  - A reply is routed by `(ledger_hash, itype)`: liBASE 0, liTX_NODE 1, liAS_NODE 2.
  - A base reply goes to every active job for that hash.
  - Peer choice stays `Pacer::pick` with `PEER_GAP`.

- [ ] **Step 1: Write the failing async tests** (the in-file `Peer` fake from the existing tests; a test helper `serve(addr, peer)` reads frames from `register(addr)`, decodes the `TmGetLedger`, and answers through `on_ledger_data`):
  - `two_ledgers_fetch_at_once_and_each_is_its_own`: two fakes with different ledgers. Both `fetch_ledger` calls return their own pairs (`compare(...) == None`).
  - `a_header_job_takes_one_base_request`: `fetch_header` returns the header, and `stats().requests` grows by 1.
  - `an_awaited_job_is_not_dropped_by_the_stage_a_cap`: 6 `offer_network` wants plus 1 `fetch_ledger`. The awaited one returns Ok; `skipped` counts only Stage A wants.
  - `a_job_no_peer_answers_ends_in_err_at_its_deadline`: use `tokio::time::pause` and a fake that never answers. The call returns `Err` containing "deadline".
- [ ] **Step 2:** Run `cargo +1.94.0 test -p xrpl-node --lib peer_fetch`. Expect a FAIL.
- [ ] **Step 3:** Implement. Keep `finish()`'s Stage A path for `Want`. Awaited jobs skip the compare.
- [ ] **Step 4:** Run the tests, plus every existing `peer_fetch` test. Expect a PASS.
- [ ] **Step 5:** Commit `feat(step4): peer_fetch serves awaited ledger, header and state jobs`.

### Task 4: KeyedStateFetch, only the objects named

**Files:** Modify `crates/xrpl-node/src/peer_state.rs`

**Interfaces:**
- Produces: `pub struct KeyedStateFetch` with:
  - `pub fn new(account_hash: [u8; 32], keys: &[[u8; 32]]) -> Self`
  - `pub fn next_request(&mut self, ledger_hash: &[u8; 32]) -> Option<TmGetLedger>`: liAS_NODE, every wanted position on the keys' paths, query depth 0, at most `peer_fetch::MAX_IDS` positions.
  - `pub fn offer(&mut self, nodes: impl IntoIterator<Item = (NodeId, Vec<u8>)>) -> Offered`: each node through `check_node` against the hash its parent named.
  - `pub fn is_done(&self) -> bool`
  - `pub fn into_result(self) -> HashMap<[u8; 32], Option<Vec<u8>>>`: a key is `None` when its path ends at an empty branch, or at a leaf holding another key.

- [ ] **Step 1: Write the failing tests** (reuse `leaves()`, `state_tree()` and the `Peer` fake from this file's tests):
  - `keyed_fetch_returns_the_named_objects_and_none_for_absent_ones`: a tree of 2,000 leaves, asking for 3 present keys and 1 absent key. The present keys get their exact bytes; the absent one gets `None`; done in at most 8 requests.
  - `a_forged_node_on_a_key_path_is_refused_and_asked_again`: flip one byte of a depth-2 inner node. `Offered.rejected == 1`, and the next request names that position again.
  - `keys_sharing_a_path_share_its_requests`: 50 keys under one depth-1 branch take as many requests as 1 key, give or take the depth of the deepest key.
- [ ] **Step 2:** Run `cargo +1.94.0 test -p xrpl-node --lib peer_state`. Expect a FAIL.
- [ ] **Step 3:** Implement. Walk down by `position_of(key, depth)`, asking only for children named by a proven inner node on some key's path.
- [ ] **Step 4:** Run the tests. Expect a PASS. Then wire `fetch_state_keys` (Task 3's stub) to it, and run the `peer_fetch` tests again.
- [ ] **Step 5:** Commit `feat(step4): keyed state fetch — the objects a ledger changed, proven to its account hash`.

### Task 5: peer_ledgers, hash for a seq and one ledger's inputs

**Files:** Create `crates/xrpl-node/src/peer_ledgers.rs`. Modify `ws_sync.rs` to make `extract_affected_nodes` and `count_wallet_delta` `pub(crate)`.

**Interfaces:**
- Consumes:
  - `own_validated::{quorum_hash, newest_quorum, QUORUM_MOVED}`;
  - `peer_fetch::{fetch_ledger, fetch_header}`;
  - `peer_source::assemble`.
- Produces:
  - `pub struct PeerLedger { pub header: crate::ws_sync::LedgerHeader, pub account_hash: String /* upper hex */, pub txs: Vec<Value>, pub pairs: Vec<(Vec<u8>, Vec<u8>)>, pub modified: HashSet<String>, pub deleted: HashSet<String>, pub tx_count: u32, pub wallet_delta: i64 }`
  - `pub trait Headers { fn header(&self, hash: [u8; 32]) -> impl Future<Output = Result<LedgerHeader, String>> + Send; }`, with `PeerHeaders` (calls `fetch_header`) for live and a map fake for tests.
  - `pub async fn hash_for<H: Headers>(h: &H, seq: u32) -> Result<[u8; 32], String>`:
    - the quorum hash when there is one;
    - else a walk back by `parent_hash` from the nearest newer known hash: a quorum seq, or an entry in the chain cache;
    - every step is cached in a `BTreeMap<u32, [u8; 32]>` capped at `resume::MAX_WARM_GAP` + 256 entries;
    - `Err` if no newer quorum ledger exists yet.
  - `pub async fn ledger(seq: u32) -> Result<PeerLedger, String>`: counts `ledger_source::stats().from_peers` on Ok.
  - `pub fn trigger() -> impl futures_util::Stream<Item = u32>`: yields `newest_quorum().0` each time it rises, waiting on `QUORUM_MOVED` with a 1 s tick.

- [ ] **Step 1: Write the failing tests:**
  - `a_seq_without_quorum_takes_its_hash_from_the_next_quorum_ledgers_parent`: a fake chain #100..#105. The tally has quorum only at #105, so `hash_for(102)` equals header #103's `parent_hash`; 3 header calls, then 0 for `hash_for(101)` (cache).
  - `a_header_that_breaks_the_chain_is_an_error`: the fake returns a header whose hash isn't the asked hash, so the result is `Err`.
  - `peer_ledger_key_sets_match_ws_syncs`: the `peer_source::fixtures::ledger()` assembled, then `PeerLedger` fields built by the same helpers. `modified`/`deleted`/`wallet_delta` equal what `extract_affected_nodes`/`count_wallet_delta` give over `theirs(&a).2`.
- [ ] **Step 2:** Run `cargo +1.94.0 test -p xrpl-node --lib peer_ledgers`. Expect a FAIL.
- [ ] **Step 3:** Implement. `ledger()` makes `hash_for`, then `fetch_ledger`, then checks `header.sequence == seq`, then `assemble`, then the key sets.
- [ ] **Step 4:** Run the tests. Expect a PASS.
- [ ] **Step 5:** Commit `feat(step4): peer_ledgers — the validated hash for any seq and its inputs from peers`.

### Task 6: The engine's own system objects

**Files:** Modify `crates/xrpl-node/src/native_shadow.rs` (after `propose`, ~:1820)

**Interfaces:**
- Produces: `pub fn singleton_writes(&self, seq: u32) -> LedgerOverlay`. After `propose` for `seq`, this is every key with `is_singleton_key(k, seq)` or in `stage3::singleton_keys_for(seq)` whose mirror bytes differ from the pre-state (`applied.undo`), encoded with `encode_entry`; `None` for a deleted one. Empty when no proposal is held for `seq`.

- [ ] **Step 1: Write the failing test** `singleton_writes_cover_every_changed_system_object`. Use the existing native_shadow test fixture that applies a ledger to a hydrated test mirror (see `the_canary_plants_one_drop…` for setup):
  - the skip list key is present, with the mirror's encoded bytes;
  - the Amendments key is absent (unchanged);
  - on a seq ≡ 0 (mod 256) fixture, the group LedgerHashes key `stage3::singleton_keys_for(seq)[4]` is present.
- [ ] **Step 2:** Run `cargo +1.94.0 test --features ffi -p xrpl-node --lib native_shadow::tests::singleton_writes`. Expect a FAIL.
- [ ] **Step 3:** Implement. Read the pre-state from `self.held`'s undo map.
- [ ] **Step 4:** Run the test. Expect a PASS.
- [ ] **Step 5:** Commit `feat(step4): the native writer hands over its own system-object writes`.

### Task 7: ws-sync in peer mode

**Files:** Modify `crates/xrpl-node/src/ws_sync.rs` (the loop at :186-962 and `process_ledger` at :1353)

**Interfaces:**
- Consumes: Tasks 1-6.
- Produces:
  - `process_ledger(rpc: Option<&RippledClient>, …)`. With `None`, a key in `modified ∪ deleted` that is missing from the overlay is skipped if it is a singleton key (unchanged), else the ledger fails (hold), with no RPC.
  - `pub(crate) fn fallback_keys(seq: u32, modified: &HashSet<String>, deleted: &HashSet<String>) -> Vec<[u8; 32]>`: the metadata's keys plus `stage3::singleton_keys_for(seq)`, deduplicated and sorted.
- **Peer-mode behaviour in the loop** (the rpc path stays as it is):
  1. **Messages:** the RPC watchdog poller and the WebSocket aren't started. The message source is `peer_ledgers::trigger()`, mapped to `{"type":"ledgerClosed","ledger_index":N}`, so the loop body runs unchanged.
  2. **Inputs:** the `fetch_account_hash` / `fetch_ledger_metadata` sites take one `peer_ledgers::ledger(seq)` result. An `Err` takes the existing `meta_ok = false` hold path (`stats().holds += 1`).
  3. **System objects:** the five static singleton keys aren't added to `acc_modified`.
  4. **FFI cross-check:**
     - `tx_blobs` come from the pairs' transaction bytes;
     - the `proto_modified` RPC block is skipped (`shadow_fetch_ok = false`);
     - the tx gate checks `compute_tx_tree_root(pairs)` against the header, as today;
     - `offer_network` isn't called;
     - `own_validated::network_validated` isn't called here (Task 10 feeds it).
  5. **Writes:** `authoritative` = proposal overlay ∪ `shadow.singleton_writes(seq)`.
  6. **Fallback:** on a refusal, an unready writer, or a rolled-back trial, the overlay is `peer_fetch::fetch_state_keys(hash, account_hash, fallback_keys(..))` (`keyed_fallbacks += 1`), and `process_ledger` runs with `rpc = None`.
  7. **Watchdog:** the live edge comes from `own_validated::newest_quorum()`. Lag over 50 only logs: no reconnect, no ws fallback logic.
  8. **Start-up:** before the first ledger, the native mirror hydrates at `last_synced`. It waits for `is_active()` with no lag gate, still honouring `XRPL_SHADOW_HYDRATE_MIN_GB`. When hydration is skipped, the keyed fallback lands the ledgers.

- [ ] **Step 1: Write the failing tests:**
  - `fallback_keys_add_the_system_objects`: for seq 107,937,280 (≡ 0 mod 256), `fallback_keys` holds the metadata keys and all 5 `singleton_keys_for` keys, sorted, with no duplicates.
  - `a_ledger_peers_cannot_deliver_holds_the_loop_at_that_seq`: test the loop's per-seq decision helper. Extract `fn next_after_inputs(seq, result: &Result<_, String>) -> Step { Hold(seq - 1) | Go }` and assert that `Err` gives `Hold(seq - 1)`.
  - `an_unready_writer_takes_the_keyed_peer_fetch`: test the decision helper `fn source_of_bytes(peers: bool, proposal: Option<&Proposal>, trial_failed: bool) -> Bytes { Ours | PeerKeys | Rpc }` with peers=true and None, so the result is `PeerKeys`; with peers=false and None, it is `Rpc`.
  - `process_ledger_without_rpc_skips_unchanged_system_objects`: in a temp rocksdb holding one account object, with the skip list key in `modified` but not in the overlay and `rpc = None`, the call lands (returns true when the root equals the given account_hash).
- [ ] **Step 2:** Run `cargo +1.94.0 test --features ffi -p xrpl-node --lib ws_sync`. Expect a FAIL.
- [ ] **Step 3:** Implement the helpers, then the loop branches. Every rpc-mode line stays as it is (`git diff` shows only additions inside `if peers` branches and the `Option` wrap).
- [ ] **Step 4:** Run the tests, plus the whole `xrpl-node --lib` with and without `ffi`. Expect a PASS. Run `cargo +1.94.0 check --features ffi -p xrpl-node --bin live_viewer --bin differential_probe`.
- [ ] **Step 5:** Commit `feat(step4): ws-sync takes each ledger from peers when XRPL_LEDGER_SOURCE=peers`.

### Task 8: Warm restart from peers; cold start refuses

**Files:** Modify `crates/xrpl-node/src/resume.rs` and `crates/xrpl-node/src/bin/live_viewer.rs` (:753-781, the cold branches)

**Interfaces:**
- Produces: `pub struct PeerNetwork { pub wait: Duration }` implementing `Network`:
  - `validated_seq()` polls `own_validated::newest_quorum()` until `wait` (120 s live) and gives `None` after that;
  - `account_hash(seq)` = `hash_for(&PeerHeaders, seq)` → `fetch_header` → upper hex of `account_hash`.
- In `live_viewer`, peer mode uses `PeerNetwork` instead of `RpcNetwork`. A cold start in peer mode (no warm request, or a refused warm start) prints `[startup] peer source: no state to resume — run scripts/peer_cold_start.sh` and exits `FALLBACK_EXIT_CODE`. It never calls `bulk_syncer.start`.

- [ ] **Step 1: Write the failing tests:**
  - `peer_network_gives_the_newest_quorum_seq`;
  - `#[tokio::test(start_paused = true)] peer_network_without_a_quorum_gives_none_after_the_wait` (wait 120 s → `None`);
  - `peer_network_account_hash_walks_to_the_seq`: uses the Task 5 fake `Headers` through a test constructor `PeerNetwork::with_headers`.
- [ ] **Step 2:** Run `cargo +1.94.0 test -p xrpl-node --lib resume`. Expect a FAIL.
- [ ] **Step 3:** Implement and wire.
- [ ] **Step 4:** Run the tests. Expect a PASS. Run the live_viewer check.
- [ ] **Step 5:** Commit `feat(step4): a warm restart verifies and catches up from peers; a cold start in peer mode never asks .39`.

### Task 9: The last .39 reads

**Files:** Modify `crates/xrpl-node/src/ffi_verifier.rs:60-115`, `crates/xrpl-node/src/bin/live_viewer.rs` (:970-1000 the total-coins poller, :840-900 the backfill), `crates/xrpl-node/src/ffi_engine.rs` (the consult budget)

**Interfaces:**
- Produces: `pub fn amendments_from_state(db: &rocksdb::DB) -> Vec<[u8; 32]>` in `ffi_engine.rs`. It reads key `7DB0788C020F02780A673DC74757F23823FA3014C1866E72CC4CD8B226CD6EF4`, decodes it with the SDK codec, and returns the `Amendments` array (empty when absent).
- In peer mode:
  - the FFI verifier takes its amendments from `amendments_from_state`, refreshed every flag ledger;
  - the state.rocks consult budget is 0;
  - the total-coins poller isn't spawned (ws-sync sets the engine's network total coins from each landed header's `total_drops`);
  - the backfill block isn't reached.

- [ ] **Step 1: Write the failing test** `amendments_from_state_reads_the_enabled_list`. In a temp rocksdb holding an Amendments object built with the codec (3 known amendment hashes), the result is those 3 in order.
- [ ] **Step 2:** Run `cargo +1.94.0 test --features ffi -p xrpl-node --lib amendments_from_state`. Expect a FAIL.
- [ ] **Step 3:** Implement and wire.
- [ ] **Step 4:** Run the test and the live_viewer check. Expect a PASS.
- [ ] **Step 5:** Commit `feat(step4): peer mode reads amendments and total coins from its own state`.

### Task 10: The optional witness

**Files:** Create `crates/xrpl-node/src/witness.rs`. Modify `ws_sync.rs`: after a ledger lands in peer mode, call `witness::landed(seq, hash)`.

**Interfaces:**
- Produces:
  - `pub fn enabled() -> bool` (`XRPL_WITNESS_RPC` set);
  - `pub fn landed(seq: u32, hash: [u8; 32])`: a try_send to a background task that asks that server for `ledger` seq (no transactions) with a 5 s timeout, then calls `own_validated::network_validated(seq, their_hash)` and counts agree/differ/unreachable.
  - `pub fn parse_hash(body: &Value) -> Option<[u8; 32]>`: only from a `validated: true` reply.
  - `pub fn json() -> Value`.
- It never feeds the data path and never blocks the loop.

- [ ] **Step 1: Write the failing tests:** `parse_hash_takes_only_validated_replies` (validated true gives Some; false or missing gives None) and `landed_is_a_noop_when_disabled`.
- [ ] **Step 2:** Run `cargo +1.94.0 test -p xrpl-node --lib witness`. Expect a FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the tests. Expect a PASS.
- [ ] **Step 5:** Commit `feat(step4): XRPL_WITNESS_RPC — a server that only checks, never feeds`.

### Task 11: Observe mode for the twin

**Files:** Create `crates/xrpl-node/src/observe.rs`. Modify `bin/live_viewer.rs`: the identity at :361-383, the relay hub senders at :405-490, the manifest broadcast at :494-503, the peer session manifest at :563, the tx relay, and the signing send at :1622-1645.

**Interfaces:**
- Produces:
  - `pub fn enabled() -> bool` (`XRPL_OBSERVE=1`);
  - `pub fn validator_identity(observe: bool, seed_path: &str) -> Result<(NodeIdentity, bool /*created*/), String>`: observe → `NodeIdentity::generate()`, and the seed file is never opened or created;
  - `pub static WOULD_SIGN: AtomicU64`;
  - `pub fn json() -> Value` (`{"observe": true, "would_sign": n}`).
- With observe on:
  - no relay hub task is spawned;
  - no manifest goes out (broadcast or handshake);
  - peer transactions aren't relayed;
  - at the signing point, everything up to `sign_validation` runs, then `WOULD_SIGN += 1` and `continue` (no frame is built or sent, and `SIGNED` stays 0);
  - the independence tally records nothing (it counts real signatures only).

- [ ] **Step 1: Write the failing test** `observe_never_touches_the_seed_file`. With a temp dir and observe true, `validator_identity` returns `Ok((_, false))` and the seed path doesn't exist afterwards; observe false creates it (0600).
- [ ] **Step 2:** Run `cargo +1.94.0 test -p xrpl-node --lib observe`. Expect a FAIL.
- [ ] **Step 3:** Implement and wire every gate listed.
- [ ] **Step 4:** Run the test and the live_viewer check. Expect a PASS. `grep -n "observe::enabled()" crates/xrpl-node/src/bin/live_viewer.rs` should show the 5 gates.
- [ ] **Step 5:** Commit `feat(step4): XRPL_OBSERVE — a full validator that signs and sends nothing`.

### Task 12: Cold start from peers

**Files:**
- Modify `crates/xrpl-node/src/bin/peer_state_sync.rs`: write `DIR/header.bin` (the proven 118-byte header) when it prints `ROOT PROVEN`.
- Create `crates/xrpl-node/src/bin/leaves_to_rocks.rs` (`[[bin]]` in `crates/xrpl-node/Cargo.toml`, no ffi) and `scripts/peer_cold_start.sh`.

**Interfaces:**
- `leaves_to_rocks --dir DIR --rocks PATH --data-dir D`:
  - parses `header.bin` and checks that `recheck(DIR/leaves.bin)`'s root equals its `account_hash`;
  - writes every leaf `key → object` into a NEW rocksdb at PATH (refusing an existing one) in batches of 50,000;
  - writes `meta:last_seq` = header seq (u32 LE);
  - writes the resume ticket `{seq, account_hash, written_at_unix}` to `paths::resume_ticket_path()`, plus `clean_shutdown.marker` and `sync_complete.marker` (entry count) under D;
  - prints `COLD READY #seq root …`.
- `scripts/peer_cold_start.sh DATA_DIR`:
  - runs `peer_state_sync --out DATA_DIR/peer-state --fresh`, then `leaves_to_rocks`;
  - prints the env line for a warm start (`XRPL_WARM_RESUME=1 XRPL_LEDGER_SOURCE=peers`).

- [ ] **Step 1: Write the failing test** `tests/leaves_to_rocks.rs`, `a_leaf_file_becomes_a_store_that_resumes`. Use 1,000 leaves written with `LeafFile`, and a header whose `account_hash` is `state_root` of them. Run the bin's `fn convert(...)` (in the bin and re-exported for tests), then check:
  - `resume::read_bookmark` = seq;
  - `check_ticket(take_ticket(..), bookmark)` = Ok(seq);
  - `rebuild_root` = account_hash.
- [ ] **Step 2:** Run `cargo +1.94.0 test -p xrpl-node --test leaves_to_rocks`. Expect a FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the test. Expect a PASS.
- [ ] **Step 5:** Commit `feat(step8): cold start from peers — proven leaves become a store live_viewer resumes`.

### Task 13: The twin rig and its check

**Files:** Create `scripts/twin/run-twin.sh`, `scripts/twin/twin_check.py`. Modify `bin/live_viewer.rs` to add `ledger_source::json()`, `witness::json()` and `observe::json()` to `/api/engine`, and `metrics_text()` to `/metrics`.

**Interfaces:**
- `run-twin.sh HOST_DATA_DIR` sets:
  - `XRPL_DATA_DIR=HOST_DATA_DIR`, `FFI_PORT=3787`, `XRPL_OBSERVE=1`, `XRPL_LEDGER_SOURCE=peers`, `XRPL_WARM_RESUME=1`;
  - `XRPL_RPC_URL=http://127.0.0.1:9`, `XRPL_RPC_URLS=http://127.0.0.1:9`, `XRPL_WS_URL=ws://127.0.0.1:9`;
  - the live engine switches (`XRPL_NATIVE_SHADOW`, `XRPL_NATIVE_WRITER`, `XRPL_NATIVE_META`, `XRPL_NATIVE_ORDER=on`, `XRPL_OWN_VALIDATED`, `XRPL_OWN_HASH`, `XRPL_SIGN_OWN`, `XRPL_PEER_FETCH`, `XRPL_PEER_PONG`, `XRPL_NODE_IDENTITY`, `XRPL_PEER_PRIVATE`, `XRPL_UNL_PINNED_KEY` from the live launcher, `XRPL_UNL_ENFORCE`, `XRPL_CLOCK_CHECK`, `XRPL_FLOW_ENGINE=port`);
  - max peers 40.
- It runs under `systemd-run --user --scope -p MemoryMax=48G nice -n 19 ionice -c3`, and logs to `HOST_DATA_DIR/twin.log`. It refuses to start if `HOST_DATA_DIR` is `/mnt/xrpl-data`, `~/xrpl-data`, or holds a `validator_seed.hex`.
- `twin_check.py --twin URL --live URL` prints one line:
  - the twin's `ledger_source` (`from_peers`, `holds`, `keyed_fallbacks`, `rpc_refused`);
  - `own_hash` (`ledgers`, `own`, `differ`, `streak`) and `would_sign`;
  - the live validator's same seq range.
  - It exits non-zero on `rpc_refused > 0`, `own_hash.differ` growing, or the twin's last seq more than 10 behind the live validator's.

- [ ] **Step 1: Write the failing test** `scripts/twin/test_twin_check.py`. Feed two canned `/api/engine` JSONs: a healthy one exits 0; `rpc_refused: 3` exits 1 and names it.
- [ ] **Step 2:** Run `python3 scripts/twin/test_twin_check.py` (standard library unittest: neither .39 nor .220 has pytest). Expect a FAIL.
- [ ] **Step 3:** Implement both scripts and the `/api/engine` sections.
- [ ] **Step 4:** Run the test. Expect a PASS. Also run `bash -n scripts/twin/run-twin.sh`.
- [ ] **Step 5:** Commit `feat(step4): the twin — a peer-fed, non-signing copy and its daily check`.

---

## After the tasks (outside this plan's code)

1. **The .220 gate.** Run the full suites, port, HIST60/WIN98 and FFI the same way as a cycle gate, with `XRPL_LEDGER_SOURCE` unset. This proves the default path didn't change.
2. **Twin run.** This needs James's choice of host. Steps: peer cold start, then a warm start in peer mode, then `twin_check.py` hourly for at least 48 h. Pass criteria:
   - `rpc_refused 0`;
   - `own_hash differ 0` (apart from known engine receipts, which get drilled);
   - `would_sign` within 1% of the live validator's signed count over the same range;
   - no hold lasting over 60 s.
3. **Day 7 (about 11-03): cycle 190** = this branch, with batch-meta-190 already under it. The launcher gets `XRPL_LEDGER_SOURCE=peers` and `XRPL_WITNESS_RPC=http://<old-server>:5005`. James's tap is needed. Rollback = `XRPL_LEDGER_SOURCE=rpc` plus a warm restart.
4. **Rehearsal, then .39 off.** The Rust SDK session stops .39's xrpld for an hour, with James's tap. Then the witness URL is dropped and .39 is switched off.
5. **Monitoring off .39.** step_watch, the health checks, the soak verdict and post_deploy's references are rewritten against public servers or the validator's own numbers. This is a separate small change to the ops scripts.

## As built (2026-10-27): where the build departs from the plan

- **Task 7, start-up.** No hydrate-first step. The mirror loads as it does today (at the live edge). Until it is
  loaded, each ledger lands by the keyed peer fetch (`peer_state_bytes`), exactly where the rpc path used .39's
  bytes. Review Focus 5 then needs no special case.
- **Task 7, tests.** The loop's decisions sit inline, as in the rpc path, so the two decision-helper tests became
  `fallback_keys_add_the_system_objects` and `ws_syncs_system_objects_are_stage3s`. The twin run covers the hold and
  fallback paths live.
- **Signing basis.** In peer mode own_hash still calls `own_validated::witness` when our hash equals the header's.
  The header's hash is the quorum's, or proven as the parent of a quorum ledger, so a WITNESS signature there means
  "our own hash, of a ledger the network validated". That is sound, and the GIVE_UP window still keeps it recent.
- **Task 2.** `quorum_moved()` returns a `&'static tokio::sync::Notify` (one permit), not a `pub static`.
- **Task 5.** `hash_for(seq)` is a module function over a shared `Chain` (the walk's hashes serve the warm start and
  the loop alike); `Chain::hash_for` takes the `Headers` source and the quorum lookup, for tests.
- **Task 8.** The quorum-wait tests run on real time (the crate has no tokio `test-util`).
- **Task 12.** The conversion lives in the library (`cold_start::convert`, tested there); the bin is a thin wrapper.
  It reads proven.json beside header.bin.
- **Added: the data directory.** Nine library defaults (consensus record, native meta, independence days, native
  order, UNL last-good, native shadow, own hash, own validated, peer fetch) were fixed `/mnt/xrpl-data` paths. They
  now follow `XRPL_DATA_DIR` (unchanged live, where it is unset), so a twin on m3060 can never write over the live
  validator's files.
- **Added:** `XRPL_MAX_PEERS` (default 125; the twin uses 40).
