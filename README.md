# XRPL Rust Validator

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

An XRP Ledger validator written in Rust, with its own Rust transaction engine. On mainnet, the engine applies
every ledger and writes the validator's ledger state itself, and every ledger is checked against the network's
state hash before it counts.

The goal is a **fully independent validator**: one that signs a ledger hash it worked out itself, from data it
got from the network's peers. This README says what runs today, what is still borrowed, and how far along the
road we are.

## Where it stands (October 2026)

**What runs on mainnet today:**

| Piece | State |
|---|---|
| Peer protocol, networking, consensus tracking | Rust |
| SHAMap, ledger types, state-hash computation, storage (RocksDB) | Rust |
| **Transaction application** | **Rust**: the native engine (`crates/xrpl-ledger`) writes the ledger state, since 2026-10-08 |
| State check | Every ledger the engine writes must match mainnet's state hash. If the engine can't vouch for a ledger, it is written from the network's bytes instead, and the miss is drilled to a fix |
| Amendment guard | Live since 2026-10-13: it alarms if mainnet switches on a rule the engine has not built |
| Signature checks | Every peer proposal and validation is verified (millions so far, zero failures) |
| Validator list | Verified against the publisher's key (manifests, signatures, expiry, sequence). Refusing an unverified list outright is the next switch to go on |
| Signing | The validator signs the hash the network announced, once its own state hash has matched it 3 times in a row |
| libxrpl (rippled's C++ engine, linked via FFI) | **Cross-check only**: it applies each ledger beside the native engine for comparison, and the build still links it |

**What is still borrowed**, and the step of [the plan](docs/INDEPENDENCE-PLAN.md) that ends it:

| Borrowed | From | Ends at |
|---|---|---|
| Each ledger's transactions, their records and the header, and the catch-up after a restart | a stock `xrpld` node, over RPC and websocket | Step 4 |
| The transaction records (metadata) and the transaction tree | that node | Step 2 (built; shipping) |
| The order transactions apply in | the position numbers in that node's records | Step 3 (built offline) |
| The ledger hash it signs | the network's announcement | Step 5 |
| libxrpl at build time | the `ffi` feature | Step 6 |

## The road to independence

The full plan is [`docs/INDEPENDENCE-PLAN.md`](docs/INDEPENDENCE-PLAN.md); every task and its status is in
[`docs/INDEPENDENCE-TASKS.md`](docs/INDEPENDENCE-TASKS.md). The rule throughout: every new path ships switched
off, runs in shadow against mainnet's own answers, and is switched on only after a clean run.

| Step | What | Status |
|---|---|---|
| 1 | Switch on what's built: the amendment guard, signature enforcement, a validator list that fails closed | **Live** (2026-10-13); enforcement switches next |
| 2 | Our own transaction records and transaction tree | **Built.** Offline, with the fixes the checks turned up, every record is byte-identical to mainnet's on every test ledger (Batch and system transactions included), and the transaction tree built from them matches the header. The live shadow ships next |
| 3 | Our own order and results: rippled's canonical order and retry passes, fees burned, total XRP | **Built offline.** Rebuilt from the transaction set alone, the order and every result match mainnet on every ledger tested; total XRP matches 391 of 391 test ledgers |
| 4 | Ledger data from peers, not from a local `xrpld` | Next |
| 5 | Build the ledger in the consensus round and sign our own hash: first only when it matches the trusted validators', then on compute | Started: close-time rules ported (0 misses over 3,806 mainnet headers), a consensus round recorder |
| 6 | A Rust-only build, with libxrpl kept as a test oracle | Later |

**Done means** 30 days of signing hashes the validator computed itself, from peer data.

## How it stays honest

- **Mainnet is the answer key.** Every ledger's state, and now every transaction's record, is compared with what
  the network produced. A mismatch is a *receipt*: it is drilled until the cause is found and fixed.
- **The findings log.** Each divergence from rippled gets an F-number, a fix and (usually) a byte-exact regression
  vector. It reached **F420** on 2026-10-11.
- **The differential probe** (`differential_probe`) replays real mainnet ledgers through the engine: results,
  state changes, our own transaction records (`DX_META`) and our own order and passes (`DX_PASSES`), each against
  mainnet's.
- **Crafted campaigns** on testnet and devnet cover what mainnet rarely exercises: multisign, AMM bid/vote, the
  OfferCreate flag matrix, MPT, NFT modify/cancel, Oracle, DID, credentials, LedgerStateFix, permission
  delegation, Batch.
- **One-day soaks.** Every deploy runs a full day before the next one, with the fixes shipped together, after the
  test gate and two replay windows of historical ledgers.

## Progress history

Every number is from the git history or the logged runs. Daily reports:
[xrpl-validator-reports](https://github.com/ilubxrp589/xrpl-validator-reports).

| Date (2026) | Milestone |
|---|---|
| Mar 21 | Project start |
| by Apr 1 | **28,500+ consecutive state-hash matches** against mainnet, zero mismatches |
| Apr 5 | libxrpl via FFI reaches 100% mainnet agreement as the apply path |
| Jul 24 → 30 | Native engine on fresh mainnet ledgers: **18.14 → 2.39 divergences per 1,000 transactions** |
| Aug 29 | The live shadow lands: every mainnet ledger applied natively and compared |
| Sep 14 | The structural port begins: rippled's Number/STAmount arithmetic and flow engine, function for function |
| Sep 20 → 21 | **First full 24-hour run with zero divergences**: 22,985 ledgers, 2.2M transactions |
| Sep 22 → 23 | Second full 24-hour run with zero divergences |
| Oct 8 | **The native engine becomes the production apply path**: it writes the validator's ledger state |
| Oct 9 → 10 | Batch goes live on mainnet and its first ledgers are drilled the same day (F417); fixBatchV1_2 is ported the night rippled 3.4.1's source is published |
| Oct 11 | Findings log reaches F420 (fixCleanup3_4_0, ahead of its activation) |
| Oct 12 → 13 | Step 1 goes live. Offline, the engine's own transaction records and transaction trees match mainnet byte for byte, and it rebuilds ledgers in rippled's order from the transaction set alone |

## Transaction coverage

`crates/xrpl-ledger` has native transactors for every transaction type in use on mainnet, each differential-tested
against rippled:

- **Payments**: transfer fees, trust-line limits, path and AMM strands (rippled's flow engine, ported)
- **DEX**: OfferCreate / OfferCancel, book crossing, quality gates, permissioned domains
- **AMM**: create, deposit, withdraw, bid, vote, delete, clawback
- **Checks**, **Escrow** (XRP and tokens), **Payment channels**, **Tickets**, **TrustSet**
- **NFTokens**: mint, burn, modify, offers, pages
- **Credentials**, **Oracles**, **DID**, **DepositPreauth**, **AccountSet / AccountDelete / SetRegularKey**,
  **SignerListSet** / multisign
- **MPT**, **Clawback**, **LedgerStateFix**, **permission delegation**, **Batch** (all four modes)
- **System transactions**: amendments, fees, the negative UNL

## Repository layout

```
crates/
  xrpl-ledger/   the engine: SHAMap, ledger types, transactors, the flow engine, transaction records,
                 canonical order and apply passes, close time
  xrpl-node/     peer protocol, sync, consensus tracking, the native writer and its shadows, RPC,
                 live_viewer, and the differential_probe / parity_probe harnesses
  xrpl-ffi/      the libxrpl FFI shim (optional `ffi` feature)
docs/            the independence plan and its tasks, design notes
ffi/             FFI architecture and state-marshalling specs
scripts/         fixture and answer-key fetchers, the regression corpus
tools/xrpl-watch the terminal dashboard
proto/           protobuf definitions (from rippled's public .proto)
```

## Building

This is **not a turnkey build yet**. A Rust-only build is Step 6 of the plan; until then:

- **The SDK sits beside the repo.** The workspace takes `xrpl-core` from the public
  [xrpl-sdk](https://github.com/ilubxrp589/xrpl-sdk), cloned next to this repository as a directory named exactly
  `XRPL Rust SDK`.
- **The validator as it runs on mainnet needs the `ffi` feature.** The native writer and its shadows are built with
  it, and it links libxrpl (rippled 3.4.1) through the shim in `ffi/`, a full C++ build of rippled first. See
  [`ffi/ARCHITECTURE.md`](ffi/ARCHITECTURE.md). Without the feature you get the engine, the libraries and their
  tests, not the production validator.
- **Running it needs more than the binary:** validator keys and their manifest, a configuration, and (until Step 4)
  a local `xrpld` feeding it each ledger.

The toolchain is Rust 1.94 (`cargo +1.94.0`), plus `libssl-dev pkg-config protobuf-compiler libclang-dev`.
`live_viewer` (in `crates/xrpl-node`) is the validator binary; it serves its API and dashboard on port 3777.

## Testing

```bash
# The engine and node suites (no libxrpl needed; the SDK must sit beside the repo).
# A few tests reach testnet; to skip them, as the deploy gate does:
cargo test --workspace --exclude xrpl-ffi -- \
  --skip validate_10_consecutive_ledgers --skip verify_ledger_header_hash \
  --skip verify_payment_application_against_testnet --skip connect_to_testnet_peer \
  --skip validate_real_testnet_transaction --skip follow_consensus_10_ledgers
```

`differential_probe` (built with `--features ffi`) replays mainnet ledgers from fixtures that
`scripts/fetch_ledger_fixture.py` mints, and `scripts/corpus.sh` runs it across the regression corpus.

## Security

Validator identity is a keypair chain (seed → master key → ephemeral signing key → manifest → domain). **Never**
commit or log seed or private-key material; the seed file is `chmod 600` and never lives on network-accessible
storage.

The codebase went through a **security review conducted with the Fable 5 model (AI)** in April 2026 (1
high-severity and 4 medium-severity findings), and a fresh re-audit in June. The high-severity finding, a signing
gate that could silently stop the validator, was disarmed in June, and the re-audit's other findings were fixed the
same week. The validator list is verified against its publisher (failing closed switches on in October), and signing
a locally computed hash is Step 5 of the independence plan. Fixes ship before their line-level write-ups are
published; specifics of anything still open are withheld.

## License

MIT, with an attribution requirement: if you use, copy or modify this code, credit James Turner with a link to
https://github.com/ilubxrp589/xrpl-validator. See [LICENSE](LICENSE).

The optional `ffi` feature links [`libxrpl`](https://github.com/XRPLF/rippled) (ISC, compatible with MIT). This
repository contains no rippled source code.

## Acknowledgements

The validator began with transaction application delegated to libxrpl, on the advice of a RippleX engineer
(Mayukha Vadari, `@msvadari`): XRPL's transaction semantics carry ~15 years of subtle evolution, and any
divergence is a silent consensus-failure risk. That advice shaped everything here. The native engine was never
trusted on its own word; it earned the production path by matching rippled and mainnet, byte for byte, ledger
after ledger.

## References

- [XRP Ledger docs](https://xrpl.org) · [rippled](https://github.com/XRPLF/rippled)
- [Binary codec](https://xrpl.org/serialization.html) · [Consensus](https://xrpl.org/consensus.html)
