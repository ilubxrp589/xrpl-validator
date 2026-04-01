# XRPL Rust Validator

A from-scratch XRP Ledger validator node written in Rust. Independently computes and verifies state hashes against network consensus every ledger — achieving **28,500+ consecutive matches** with zero mismatches on mainnet.

This is a fully independent implementation — no code from rippled. The validator maintains its own state tree (18.7M+ objects), computes SHA-512Half Merkle hashes, signs validations, and relays them to the network.

## Current Status

- **Independent state hash verification** every ledger (~3.5s intervals)
- **28,500+ consecutive matches** against network consensus (100% accuracy)
- **256-bucket parallel hash computation** via rayon (~2.1s on current hardware)
- **Self-healing retry** with exponential backoff on transient failures
- **Validation signing and relay** to multiple XRPL peers
- **Domain verified** at `halcyon-names.io`

## Security

### Validator Keys

XRPL validator identity is based on a cryptographic keypair chain:

```
Validator Seed (secret) -> Master Public Key -> Ephemeral Signing Key -> Manifest -> Domain Verification
```

**Critical rules:**
- **NEVER** expose your validator seed or master private key
- **NEVER** log, print, or serialize private key material
- **NEVER** store keys in plaintext on network-accessible storage
- The seed file (`validator_seed.hex`) must be `chmod 600`
- The seed controls your validator's identity permanently — if compromised, your validator must be replaced

### Key Files

| File | Contains | Protection |
|------|----------|------------|
| `validator_seed.hex` | Master private key | chmod 600, never commit to git |
| `state.rocks` | RocksDB ledger state (18.7M+ objects) | Normal permissions |
| `engine_state.json` | Ledger sequence, tx counts | Normal permissions |

## Architecture

Two-crate workspace:

```
crates/
  xrpl-ledger/    -- SHAMap, hash computation, ledger types
  xrpl-node/      -- Peer protocol, sync engine, consensus, RPC, live viewer
proto/
  xrpl.proto      -- Protobuf definitions (from rippled)
```

### Dependencies

- **xrpl-core** -- Types, binary codec, cryptography (Ed25519 + Secp256k1)
- **OpenSSL** -- Peer handshake (SSL_get_finished compatibility with rippled)
- **RocksDB** -- Persistent state storage (18.7M+ objects, ~4.4GB)
- **tokio** -- Async runtime
- **rayon** -- Parallel hash computation (256-bucket split)
- **axum** -- HTTP server for RPC and live viewer
- **prost** -- Protocol buffer codegen
- **jemalloc** -- Memory allocator (reduced fragmentation for large state)

## What's Implemented

### State Hash Verification
- SHAMap (16-ary Merkle-Patricia trie) with hash-only leaves
- 256-bucket parallel hash computation using `compute_subtree()` + rayon
- Incremental updates: only dirty buckets recomputed per ledger
- Depth-2 branch caching with dirty tracking
- Independent `account_hash` verified against network consensus every ledger
- Independent `ledger_hash` computation from header fields

### Sync Engine
- **Bulk sync**: 4 parallel download workers, ~170K objects/sec from network
- **WebSocket sync**: Real-time ledger tracking via rippled WebSocket subscription
- **Gap detection**: Automatic catch-up when ledgers are missed
- **Skip-ahead**: Jumps forward when >50 ledgers behind
- **Self-healing retry**: Exponential backoff (up to 5 attempts) on hash mismatch
- **Full scan fallback**: Comprehensive state comparison on persistent mismatch
- **Transaction ordering**: Sorted by `TransactionIndex` before applying `AffectedNodes`

### Protocol Singletons
Force-fetched every ledger to ensure correct state:
- LedgerHashes (+ sub-pages at `SHA512Half(0x0073 || seq/65536)`)
- NegativeUNL
- Amendments
- FeeSettings

### Peer Protocol
- TLS connection with OpenSSL (rippled's session cookie mechanism)
- XRPL/2.2 handshake with session signature verification
- Binary message framing (4-byte length + 2-byte type + protobuf)
- All 21 peer message types decoded
- Auto peer discovery via TMEndpoints
- Multi-peer connections with message deduplication

### Validation & Consensus
- Validation signing with Ed25519 ephemeral keys
- Manifest construction and broadcast to peers
- Validation relay to s1.ripple.com, s2.ripple.com, xrplcluster.com
- RPCA state machine tracking (Open -> Establish -> Accept)
- UNL management with manifest key rotation
- Amendment and fee voting

### Live Viewer
- Real-time dashboard at port 3777
- Consensus visualization with particle physics
- Metrics with sparkline graphs
- Domain verification at `halcyon-names.io/.well-known/xrp-ledger.toml`

## Build & Run

### Prerequisites

```bash
# Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# System dependencies
sudo apt install libssl-dev pkg-config protobuf-compiler libclang-dev

# Build (use target-cpu=native on AMD Ryzen for SHA-NI acceleration)
RUSTFLAGS="-C target-cpu=native" cargo build --release
```

### Run the Validator

```bash
# Set RocksDB path (default: /mnt/xrpl-data/sync/state.rocks)
export XRPL_ROCKS_PATH="/mnt/xrpl-data/sync/state.rocks"

# Run the live viewer (includes full sync + validation)
cargo run --release -p xrpl-node --bin live_viewer

# Dashboard: http://localhost:3777
# Peer port: 51235
```

### Configuration

Key environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `XRPL_ROCKS_PATH` | `/mnt/xrpl-data/sync/state.rocks` | RocksDB state database path |

The validator connects to a local rippled node for RPC queries and subscribes via WebSocket for real-time ledger updates. Peer connections are established to s1.ripple.com, s2.ripple.com, and discovered peers.

## Testing

```bash
cargo test --workspace    # 150+ tests
```

## Performance

Measured on Intel i5-9600K (6C/6T, no SHA-NI):

| Metric | Value |
|--------|-------|
| Initial state download | ~134s (18.7M objects, 4 workers) |
| SHAMap tree build | ~27s from RocksDB |
| Hash computation (256-bucket parallel) | ~2.1s |
| Incremental update per ledger | ~0.4s |
| Total per-ledger verification | ~2.5s |
| Ledger close interval | ~3.5s |
| State objects tracked | 18.7M+ |
| RocksDB size on disk | ~4.4 GB |

AMD Ryzen with SHA-NI hardware acceleration is expected to reduce hash computation to ~0.5-0.7s.

## Roadmap

### Done
- [x] Peer protocol (handshake, codec, multi-peer, auto-discovery)
- [x] SHAMap with incremental hash computation
- [x] 256-bucket parallel hash with dirty tracking
- [x] RocksDB persistent state (18.7M+ objects)
- [x] Independent state hash verification (28.5K+ consecutive matches)
- [x] Independent ledger hash computation
- [x] WebSocket real-time sync with self-healing retry
- [x] Bulk sync with 4 parallel download workers
- [x] Validation signing and relay
- [x] Domain verification
- [x] Live viewer dashboard
- [x] Protocol singleton handling (LedgerHashes, FeeSettings, etc.)

### Next
- [ ] Network resilience (rippled failover)
- [ ] Depth-2 branch caching optimization (target: <0.5s hash)
- [ ] Persist leaf cache to disk (instant restarts)
- [ ] Smarter failed-key retry (track and retry specific keys)
- [ ] Domain verification + uptime for trusted validator path
- [ ] Transaction application engine
- [ ] Hardware migration to AMD Ryzen (SHA-NI)

### Future
- [ ] Full transaction engine (Payment, DEX, TrustSet)
- [ ] Testnet validation
- [ ] UNL listing candidacy

## References

- [XRPL Documentation](https://xrpl.org)
- [Run rippled as a Validator](https://xrpl.org/run-rippled-as-a-validator.html)
- [rippled Source Code](https://github.com/XRPLF/rippled)
- [XRPL Binary Codec](https://xrpl.org/serialization.html)
- [Consensus Protocol](https://xrpl.org/consensus.html)

## License

Proprietary -- see [LICENSE](LICENSE)
