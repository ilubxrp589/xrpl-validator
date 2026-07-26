# XRPL Rust Validator

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

A from-scratch XRP Ledger validator written in Rust, plus a native Rust
transaction engine that is validated **byte-for-byte against rippled** on
real mainnet ledgers.

Two things live in this repo:

1. **A working validator** — it independently recomputes and verifies mainnet
   state hashes every ledger, signs validations, and relays them to the
   network. In a multi-week mainnet run it held **28,500+ consecutive
   `account_hash` matches with zero mismatches.**
2. **A native Rust transaction engine** under active development, checked for
   exactness against rippled through a differential harness — it reached
   **100% attempted-transaction parity on a 19-ledger mainnet corpus** and
   every change since is gated on not regressing it (reproducible via
   `scripts/corpus.sh`).

## Honest architecture (read this first)

This validator is a **hybrid**, by deliberate design:

| Component | Implementation |
|---|---|
| Networking, peer protocol, consensus state machine | **Rust** |
| SHAMap, state-hash computation, ledger types | **Rust** |
| Storage (RocksDB), validator identity, RPC, metrics, dashboard | **Rust** |
| **Transaction application (production path)** | **libxrpl** — rippled's own C++ engine, in-process via FFI |

Transaction application is delegated to rippled's engine on purpose. XRPL
transaction semantics carry ~15 years of subtle evolution across 60+ tx
types and ongoing amendments — a RippleX engineer (Mayukha Vadari, `@msvadari`)
recommended FFI over a from-scratch reimplementation, because any divergence
is a silent consensus-failure risk. rippled 3.0.0+ ships its ledger component as the
first-class `libxrpl` library, which makes this clean. See
[`ffi/ARCHITECTURE.md`](ffi/ARCHITECTURE.md).

**This repo contains no rippled source code.** The optional `ffi` feature
links against `libxrpl` at build time; rippled is © its authors (ISC).

### The independence track (why the native engine exists)

In parallel, `crates/xrpl-ledger` is a **from-scratch Rust transaction
engine** — the long-term path to a fully independent validator. Rather than
trust it, every change is checked against rippled: the differential harness
replays real mainnet transactions through the native engine and compares
each result to the canonical outcome (the FFI/libxrpl path, which already
matches mainnet, is the oracle). A change ships only if the corpus match
count does not drop. It is **not yet the production apply path** — it earns
that once it holds at parity on a much larger corpus.

## Status — what's what, where things are at

| Piece | State |
|---|---|
| Independent every-ledger state-hash verification | **Working** — 28.5K+ consecutive mainnet matches (documented run) |
| Validation signing + relay | Working; **signing does not yet strictly gate on local verification** (hardening in progress) |
| Production transaction apply | **Hybrid** — libxrpl via FFI |
| Native Rust transaction engine (`xrpl-ledger`) | reached **100% attempted-tx parity** on a 19-ledger mainnet corpus (regression-gated); not yet production |
| Differential harness (`differential_probe` + `scripts/corpus.sh`) | Working; the regression gate for engine changes |
| Security review (Fable 5 model, AI) | **In progress** — findings being addressed; specifics withheld |

## Native engine — transaction coverage

`crates/xrpl-ledger` implements native Rust transactors for, and
differential-tests against rippled:

- **Payments** (incl. transfer fees, trust-line limits, path/AMM strands)
- **DEX** — OfferCreate / OfferCancel, book crossing, quality gates
- **AMM** — create, deposit, withdraw (incl. `tfWithdrawAll` teardown), swap
- **Checks** — create / cash / cancel
- **Escrow**, **PayChannel**, **Tickets**, **TrustSet**
- **NFTokens** — mint / offers / pages
- **Credentials**, **Oracles**, **AccountSet / AccountDelete**

Each of those has a git-anchored history of specific parity fixes against
rippled (e.g. issuer `TransferRate` on delivery, `keepRoot` on directory
teardown, `CheckCancel` dual-directory unlink, underfunded-maker crossing).
`git log` is the source of truth.

## Repository layout

```
crates/
  xrpl-ledger/   -- native engine: SHAMap, ledger types, transactors, differential apply
  xrpl-node/     -- peer protocol, sync, consensus, RPC, live viewer, FFI integration,
                    differential_probe / parity_probe harness binaries
  xrpl-ffi/      -- the libxrpl FFI shim (optional `ffi` feature)
ffi/             -- FFI architecture + state-marshaling specs
scripts/         -- corpus.sh (differential regression gate), fixture fetchers
proto/           -- protobuf definitions (from rippled's public .proto)
```

## Build & run

```bash
# Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
sudo apt install libssl-dev pkg-config protobuf-compiler libclang-dev

# Validator (target-cpu=native gets SHA-NI on Ryzen)
RUSTFLAGS="-C target-cpu=native" cargo build --release
cargo run --release -p xrpl-node --bin live_viewer     # dashboard on :3777

# Differential corpus (needs the ffi feature + a libxrpl build)
cd crates/xrpl-node && cargo build --features ffi --bin differential_probe
scripts/corpus.sh                                       # prints CORPUS TOTAL: matched/attempted
```

## Testing

```bash
cargo test --workspace          # unit + regression suite
scripts/corpus.sh               # native engine vs rippled, across the mainnet corpus
```

## Security

Validator identity is a keypair chain (seed → master key → ephemeral signing
key → manifest → domain). **Never** commit or log seed/private-key material;
the seed file is `chmod 600` and never lives on network-accessible storage.

The codebase has been through a **security review conducted with the Fable 5
model (AI)**, which surfaced 1 high-severity and 4 medium-severity findings,
now being worked through on a dedicated track. Fixes ship before their
line-level write-ups are published; specifics of unfixed findings are withheld.

## License

[MIT](LICENSE). The optional `ffi` feature links
[`libxrpl`](https://github.com/XRPLF/rippled) (ISC, compatible with MIT).
This repository contains no rippled source code.

## References

- [XRP Ledger docs](https://xrpl.org) · [rippled](https://github.com/XRPLF/rippled)
- [Binary codec](https://xrpl.org/serialization.html) · [Consensus](https://xrpl.org/consensus.html)
