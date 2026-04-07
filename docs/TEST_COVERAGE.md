# Test Coverage Proof — Halcyon XRPL Validator

## Unit Tests (offline, repeatable)

| Crate | Tests | Status |
|-------|-------|--------|
| **xrpl-node** (lib) | 79 | **79/79 PASS** |
| **xrpl-ffi** (FFI bindings) | 4 | **4/4 PASS** |
| **xrpl-ledger** (SHAMap, codec) | 151 | **150/151 PASS** (1 known pre-existing) |
| **C++ shim** (test_shim) | 1 | **PASS** |
| **Total** | 235 | **234/235 PASS** |

### Key unit tests

- `owned_snapshot_isolates_from_concurrent_writes` — proves MVCC isolation for concurrent DB reads during FFI verify
- `owned_snapshot_is_send` — proves snapshot is thread-safe across `spawn_blocking` boundaries
- `owned_snapshot_outlives_original_arc_scope` — proves snapshot lifetime management is correct
- `apply_mainnet_payment_returns_mutations` — full round-trip: real mainnet tx bytes -> parse -> preflight -> apply -> verify 2 SLE mutations with correct balances
- `parse_mainnet_payment` / `preflight_mainnet_payment` — FFI bindings produce correct results against known mainnet tx
- `compute_subtree_deterministic` — SHAMap Merkle hash is reproducible across runs
- `ledger_hash_deterministic` / `tx_hash_deterministic` — hash computation matches expected values
- `sign_validation_produces_bytes` / `sign_validation_with_amendments` — validation signing produces valid output

## Live Production Proof

The validator runs continuously on mainnet, processing every ledger in real time. These metrics are from the live instance:

| Metric | Value | What it proves |
|--------|-------|----------------|
| **FFI TER Agreement** | 19,550 / 19,550 (100.0000%) | Every tx through libxrpl matches mainnet's result |
| **Divergences** | 0 | Zero disagreements with rippled |
| **Shadow State Hash** | 105 / 105 matched | FFI-derived state = rippled's state byte-for-byte |
| **Real State Hash** | 106 consecutive matches | Validator's DB matches network every ledger |
| **State Objects** | 18.8M | Full mainnet state synced and verified |
| **Consensus Tracking** | 35/35 UNL validators | Tracking all validators on the default UNL |

## What each layer proves

```
Layer 1: Unit Tests
  Code compiles. Functions produce correct output for known inputs.
  Deterministic, repeatable, no network required.

Layer 2: FFI TER Agreement (100%)
  libxrpl (rippled's C++ tx engine) called through our Rust FFI layer
  produces identical transaction result codes (TER) as mainnet for
  every transaction — Payments, OfferCreate, OfferCancel, TrustSet,
  NFToken ops, AMM ops, Checks, Tickets, OracleSet, AccountDelete.

Layer 3: Shadow State Hash (100%)
  The SLE mutations produced by FFI, when applied on top of the
  pre-ledger state, produce the exact same Merkle root (account_hash)
  as rippled computes. This proves state-level equivalence — not just
  "same result codes" but "same bytes in every modified object."

Layer 4: Real State Hash (100%)
  The validator's complete RocksDB state (18.8M objects) hashes to the
  same account_hash the network reports every single ledger. Computed
  via a 65536-bucket flat SHAMap hasher in under 15ms.
```

## Running the tests

```bash
# Clone and build
git clone https://github.com/ilubxrp589/xrpl-validator.git
cd xrpl-validator

# Unit tests (no network required)
cargo test --release -p xrpl-node --features ffi --lib
cargo test --release -p xrpl-ffi
cargo test --release -p xrpl-ledger

# C++ shim self-test (requires libxrpl build — see DEPLOY.md)
cd ffi/build && ./test_shim

# Live production metrics (requires running validator)
curl http://localhost:3777/api/engine | python3 -c "
import sys, json
f = json.load(sys.stdin).get('ffi_verifier', {})
att = f.get('live_apply_attempted', 0)
ok = f.get('live_apply_ok', 0) + f.get('live_apply_claimed', 0)
div = f.get('live_apply_diverged', 0)
sha = f.get('shadow_hash_matched', 0)
sha_t = f.get('shadow_hash_attempted', 0)
print(f'FFI Agreement: {ok}/{att} ({ok/att*100:.4f}%) diverged={div}')
print(f'Shadow Hash:   {sha}/{sha_t} matched')
"

curl http://localhost:3777/api/state-hash | python3 -c "
import sys, json
d = json.load(sys.stdin)
print(f'State Hash: {d.get(\"total_matches\",0)} matches, {d.get(\"consecutive_matches\",0)} consecutive')
"
```

## Architecture tested

```
Rust (29,800 lines)          C++ FFI (2,314 lines)
  |                            |
  Peer protocol                libxrpl 3.2.0-b0
  Consensus tracking           (rippled's tx engine)
  State storage (RocksDB)      |
  SHAMap hashing               preflight()
  Validation signing           apply()
  HTTP/WS server               mutations
  |                            |
  +--- FFI boundary -----------+
  |
  100% TER agreement
  100% shadow state hash match
  100% real state hash match
```
