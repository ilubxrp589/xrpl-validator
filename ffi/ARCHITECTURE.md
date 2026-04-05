# XRPL Rust Validator — FFI Architecture Spec

**Status: SCOPING COMPLETE (2026-04-05)**

## Executive summary

The XRPL Rust validator will delegate transaction application to rippled's C++ engine via in-process FFI. Rust owns everything else: networking, peer protocol, consensus state machine, SHAMap, storage (RocksDB), validator identity, API, metrics. One source of truth for consensus-critical semantics.

**Why this architecture:** Mayukha Vadari (RippleX engineer, @msvadari) explicitly recommended FFI over reimplementing the tx engine in Rust. XRPL tx semantics have 15 years of subtle evolution across 60+ tx types and ongoing amendments. Any divergence = silent consensus failure risk.

## The good news: libxrpl is ready

Ripple's own modularization effort has delivered exactly what we need:

- **rippled 2.6.1 (Oct 2025)** — ledger component decoupled from `xrpld/app`
- **rippled 3.0.0 (Dec 2025)** — ledger component moved into libxrpl
- **libxrpl profile renamed to "default"** — first-class build target

`include/xrpl/` now contains the entire public API we need:

| Module | Contents |
|--------|----------|
| `tx/` | `apply.h`, `applySteps.h`, `Transactor.h`, `ApplyContext.h`, 17 transactor categories |
| `ledger/` | `ApplyView.h`, `ReadView.h`, `Sandbox.h`, `OpenView.h`, `PaymentSandbox.h` |
| `protocol/` | STObject, STTx, STLedgerEntry, canonical binary format |
| `shamap/` | Reference SHAMap implementation |
| `crypto/` | Key crypto primitives |

## The canonical entry point

```cpp
namespace xrpl {
    ApplyResult apply(
        ServiceRegistry& registry,
        OpenView& view,
        STTx const& tx,
        ApplyFlags flags,
        beast::Journal journal
    );
}

struct ApplyResult {
    TER ter;                            // Transaction result code
    bool applied;                       // Did it apply?
    std::optional<TxMeta> metadata;     // AffectedNodes etc.
};
```

Runs `preflight` → `preclaim` → `doApply` internally. Production-tested.

## The 17 transactor categories (all covered)

account, bridge, check, credentials, delegate, dex, did, escrow, lending, nft, oracle, payment, payment_channel, permissioned_domain, system, token, vault

Every XRPL tx type we built stubs for in `dispatch.rs` has a corresponding battle-tested C++ transactor in libxrpl.

## Ownership boundaries

### Rust owns
- **Storage** — RocksDB state DB, leaf cache, SHAMap
- **Networking** — peer protocol, WebSocket to rippled, UNL fetching
- **Consensus** — state machine, proposal tracking, hash-level agreement (Phase A done)
- **Identity** — validator key, manifest, validation signing
- **API/Metrics** — HTTP endpoints, dashboards, Prometheus
- **Orchestration** — tx engine lifecycle, apply ordering, batch commits

### C++ (libxrpl) owns
- **Tx application** — all 17 transactor categories, preflight/preclaim/apply
- **Amendment logic** — feature flags, version-dependent code paths
- **Invariant checks** — ledger integrity guarantees
- **Path finding** — Payment path computation
- **Offer crossing** — DEX order book logic
- **Canonical binary format** — STObject/STTx encoding

## Build model

```
xrpl-validator/
├── crates/
│   ├── xrpl-node/           (Rust — networking, consensus, RPC)
│   ├── xrpl-ledger/         (Rust — types, keylets, legacy tx_engine)
│   └── xrpl-ffi/            (NEW — Rust bindings to xrpl_shim)
├── ffi/
│   ├── xrpl_shim.h          (C API header — this spec)
│   ├── xrpl_shim.cpp        (C++ implementation linking libxrpl)
│   ├── CMakeLists.txt       (builds libxrpl_shim.a)
│   └── vendor/
│       └── rippled/         (git submodule, pinned version)
```

**Build steps:**
1. Vendor rippled as git submodule at known good version
2. Build libxrpl (target from rippled's CMake)
3. Build xrpl_shim.cpp linking against libxrpl → libxrpl_shim.a (or .so)
4. Rust `xrpl-ffi` crate uses `bindgen` or hand-written `extern "C"` blocks to call shim
5. Link into xrpl-node binary

## Migration path

**Phase A (DONE):** Consensus Phase A — peer proposals, UNL, agreement monitor. Rust consensus state machine wired up and running on mainnet.

**Phase B (current → FFI):**
1. Vendor rippled + build libxrpl + build shim (C++ scaffolding)
2. Write xrpl-ffi Rust bindings + state marshaling callback
3. Write integration tests: parse tx, apply via shim, verify TER matches expected
4. Run shim in parallel with existing Rust tx_engine (observer mode) — compare results
5. Once shim matches rippled's own output on 10k+ txs, flip signing path to use shim
6. Delete Rust tx_engine from consensus path (keep as scratchpad only)

**Phase C (production):**
7. Sign validations using FFI-computed ledger hash
8. Track libxrpl releases, update submodule regularly
9. Apply for UNL inclusion

## Minimum viable shim (first milestone)

Enough to apply a single Payment tx end-to-end:

- `xrpl_engine_create` / `_destroy`
- `xrpl_ledger_create` / `_destroy` (with SLE lookup callback)
- `xrpl_apply_tx`
- `xrpl_result_ter` / `_applied` / `_ter_name`
- `xrpl_result_mutation_count` / `_mutation_at`
- `xrpl_result_destroy`

Skip for MVP: `xrpl_tx_parse`, `xrpl_tx_check_signature`, `xrpl_result_meta_bytes`.

## Risk assessment

| Risk | Mitigation |
|------|-----------|
| libxrpl API changes between rippled versions | Pin to specific version; update deliberately |
| C++ exceptions crossing FFI boundary | Wrap all C++ calls in try/catch, return NULL/error code |
| Memory ownership bugs at FFI boundary | Strict ownership rules (see STATE_MARSHALING.md), valgrind/ASan |
| Performance overhead from callbacks | Benchmark early; optimize with prefetch if needed |
| Build complexity (C++ toolchain + Rust) | Docker build image with both; CI must build both |
| ServiceRegistry dependencies we don't understand | Start with minimal registry, add as needed |

## Open questions (to answer during implementation)

1. Does `ServiceRegistry` need a real `Application` pointer or can it use a stub? (check rippled NetworkOPs.cpp)
2. How do we construct `OpenView` without a full `Ledger` (which needs a parent)? (likely needs a `ReadView` adapter)
3. What's the minimum `Rules` object we need to construct for amendments? (check `Rules.h`)
4. Are SLE bytes we store compatible with `SerialIter` deserialization? (yes — same binary format as rippled)
5. Can we skip `HashRouter` (we don't need dedup for validator signing path)?

## Estimated effort

- **Week 1-2**: Vendor rippled, build libxrpl locally, build empty shim
- **Week 3-4**: Implement MVP shim (Payment only), first Rust→C++ apply call
- **Week 5-6**: All 17 transactor categories, state callback, mutation extraction
- **Week 7-8**: Integration tests, parallel observer mode against Rust tx_engine
- **Week 9-12**: Performance tuning, edge cases, production hardening
- **Month 4-6**: Ledger close via FFI, independent hash computation, UNL application prep

## Success metrics

1. Shim applies a Payment tx and returns `tesSUCCESS` with correct mutations
2. 100% match rate against rippled's own apply on a 10k tx sample
3. Validator signs using FFI-computed hash (replaces `src=network`)
4. libxrpl_shim compiles on Linux x86_64 + ARM64
5. Tx apply latency < 5ms p99 (matches rippled's own perf)

## What this unlocks

- **True validator independence** — our validator computes consensus without trusting rippled
- **UNL credibility** — Ripple would build this architecture themselves; the only path to being trusted
- **Amendment compatibility** — auto-updates as libxrpl does
- **One source of truth** — no divergence from mainnet semantics, ever

## What we keep from current work

- All of Phase A (consensus wiring) — unchanged
- All peer protocol + P2P code — unchanged
- All state storage + RocksDB — unchanged
- SHAMap computation — we keep our Rust impl, use libxrpl's as reference only
- tx_engine.rs — kept as scratchpad/learning tool, NOT consensus authority

## References

- [XRPLF/rippled GitHub](https://github.com/XRPLF/rippled)
- [rippled 3.0.0 release notes](https://xrpl.org/blog/2025/rippled-3.0.0)
- [rippled 2.6.1 release notes](https://xrpl.org/blog/2025/rippled-2.6.1)
- [XLS-42 Plugin API](https://github.com/XRPLF/XRPL-Standards/discussions/116)
- `ffi/xrpl_shim.h` — C API header (draft)
- `ffi/STATE_MARSHALING.md` — cross-boundary data flow design
