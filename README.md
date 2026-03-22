# XRPL Rust Validator

A from-scratch XRP Ledger validator node written in Rust. Currently operates as a **mainnet observer** — connects to XRPL peers, decodes all peer protocol messages, verifies transaction signatures, and tracks consensus in real time.

**Status: Early prototype. NOT production-ready. Do NOT use for mainnet validation without extensive testing.**

## Security Warnings

### Validator Keys

XRPL validator identity is based on a cryptographic keypair chain:

```
Validator Seed (secret) → Master Public Key → Ephemeral Signing Key → Manifest → Domain Verification
```

**Critical rules:**
- **NEVER** expose your validator seed or master private key
- **NEVER** log, print, or serialize private key material to stdout/stderr
- **NEVER** store keys in plaintext on network-accessible storage
- Generate keys on an air-gapped machine if possible
- Use `--generate-identity` to create keys, store the output file with `chmod 600`
- The seed controls your validator's identity permanently — if compromised, your validator must be replaced

### Network Safety

- This node is an **observer** — it watches consensus but does not participate
- Running as a full validator requires: complete ledger state sync, transaction application engine, and months of proven uptime
- **Do NOT** claim to be a validator on mainnet UNL lists until your implementation is thoroughly audited
- Testnet validation should be attempted first, verified against rippled for byte-compatibility

### Key Files That Must Be Protected

| File | Contains | Protection |
|------|----------|------------|
| Validator seed file | Master private key | chmod 600, never commit to git |
| config.toml | May reference key paths | chmod 600 |
| sled database | Ledger state (public data) | Normal permissions |

## Architecture

Two-crate workspace:

```
crates/
  xrpl-ledger/    — SHAMap, NodeStore, ledger state, transaction engine
  xrpl-node/      — Peer protocol, consensus, mempool, RPC, viewer
proto/
  xrpl.proto      — Protobuf definitions from rippled
```

### Dependencies

- **xrpl-core** — Types, binary codec, cryptography (git dep from [xrpl-sdk](https://github.com/ilubxrp589/xrpl-sdk))
- **OpenSSL** — Required for peer handshake (SSL_get_finished compatibility with rippled)
- **tokio** — Async runtime
- **sled** — Embedded key-value store for persistent state
- **axum** — HTTP server for RPC and viewer
- **prost** — Protocol buffer codegen

## What's Implemented

### Peer Protocol (M1)
- TLS connection with OpenSSL (required for rippled's session cookie)
- XRPL/2.2 handshake with session signature verification
- Binary message framing codec (4-byte length + 2-byte type + protobuf)
- All 21 peer message types decoded
- Auto peer discovery via TMEndpoints
- Multi-peer connections (up to 50, auto-discovery from seed peers)
- Message deduplication across peers

### Ledger Storage (M2)
- SHAMap (Merkle-Patricia trie, branching factor 16)
- 12 hash prefix constants verified against rippled
- Ledger header: 118-byte serialization, hash verified against live testnet (6/6 match)
- NodeStore trait with InMemory and sled backends
- Full ledger state sync from mainnet RPC (~1500 objects/sec)

### Transaction Engine (M3)
- Transaction validation with full signature verification
- Ed25519: verify against raw STX\0||bytes (Ed25519 does internal hashing)
- Secp256k1: verify against SHA512Half(STX\0||bytes)
- Fee-priority mempool with dedup and expiry
- Relay filter with atomic check-and-mark
- Canonical transaction ordering (account -> sequence -> hash)
- JSON-RPC server (submit, server_info, fee, ledger, peers)

### Consensus Engine (M4)
- RPCA state machine (Open -> Establish -> Accept)
- Dynamic threshold convergence (50% -> 60% -> 70% -> 80% -> 95%)
- Close time negotiation with resolution rounding
- Validation tracking (per-ledger, fully-validated detection)
- UNL management with manifest key rotation
- Amendment voting on flag ledgers
- Fee/reserve voting (median computation)

### Live Viewer
- Real-time feed dashboard at port 3777
- Consensus visualization with particle physics (canvas)
- Metrics dashboard with sparkline graphs
- Domain verification at `halcyon-names.io/.well-known/xrp-ledger.toml`
- Live at [rvd.halcyon-names.io](https://rvd.halcyon-names.io)

## Build & Run

### Prerequisites

```bash
# Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# OpenSSL development headers
sudo apt install libssl-dev pkg-config protobuf-compiler

# Build
cargo build --release
```

### Run the Live Viewer (Observer Mode)

```bash
cargo run --release -p xrpl-node --bin live_viewer
# Open http://localhost:3777
```

### Run the CLI Node

```bash
# Generate a new node identity
cargo run --release -p xrpl-node --bin xrpl_node -- --generate-identity

# Run with testnet
cargo run --release -p xrpl-node --bin xrpl_node -- --testnet

# Run with custom config
cargo run --release -p xrpl-node --bin xrpl_node -- --config ~/.xrpl-node/config.toml
```

### Sync Ledger State

```bash
# Downloads mainnet state objects to /mnt/xrpl-data/sync/
cargo run --release -p xrpl-node --bin sync_ledger
# Resumable — re-run to continue from where it left off
```

### Configuration

Create `~/.xrpl-node/config.toml`:

```toml
[node]
mode = "follower"           # observer | follower | validator
listen_port = 51235
rpc_port = 51234
data_dir = "/mnt/xrpl-data"
log_level = "info"

[peers]
seeds = ["s1.ripple.com:51235", "s2.ripple.com:51235"]
max_peers = 50

[validator]
# Only set if running as validator (DO NOT USE ON MAINNET YET)
# token = "base64-encoded-validator-token"

[unl]
# Trusted validator public keys
# keys = ["nHB..."]

[fees]
reference_fee = 10
account_reserve = 1000000
owner_reserve = 200000
```

## Testing

```bash
# Run all unit tests (144 tests)
cargo test --workspace

# Run live integration tests (requires network)
cargo test --workspace --features live-tests

# Tests include:
# - Codec roundtrip (encode/decode all message types)
# - SHAMap operations (insert/delete/lookup/hash)
# - Ledger header hash verification against testnet
# - Transaction signature verification against live data
# - Consensus threshold convergence
# - Canonical ordering
# - Node construction for all modes
```

## Roadmap

### Done
- [x] Peer protocol (handshake, codec, multi-peer, auto-discovery)
- [x] Ledger storage (SHAMap, sled, header hashing)
- [x] Transaction validation (decode, signature verification)
- [x] Consensus engine (RPCA, thresholds, UNL, voting)
- [x] Mainnet observer with live dashboards
- [x] Domain verification
- [x] Ledger state sync

### In Progress
- [ ] Transaction application engine (Payment, DEX, TrustSet)
- [ ] Byte-compatibility verification against rippled

### Future
- [ ] Full validator mode (propose, validate, vote)
- [ ] Testnet validation
- [ ] Mainnet stock validator
- [ ] UNL listing

## References

- [XRPL Documentation](https://xrpl.org)
- [Run rippled as a Validator](https://xrpl.org/run-rippled-as-a-validator.html)
- [rippled Source Code](https://github.com/XRPLF/rippled)
- [XRPL Binary Codec](https://xrpl.org/serialization.html)
- [Consensus Protocol](https://xrpl.org/consensus.html)
- [xrpl-sdk (Rust SDK)](https://github.com/ilubxrp589/xrpl-sdk)

## License

MIT — see [LICENSE](LICENSE)
