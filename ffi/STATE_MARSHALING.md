# FFI State Marshaling Design

**Status: DRAFT (2026-04-05)**

Cross-boundary data flow between Rust (owns RocksDB state) and C++ (owns libxrpl tx engine).

## Core decision: callback-based lazy lookup

Rust passes a **function pointer** to C++ that, when called with a 32-byte keylet, returns the SLE data for that object. C++ uses this callback to populate its Sandbox lazily as the transactor needs SLEs.

**Why not eager (pre-load all SLEs)?**
- We don't know which SLEs a tx will touch until we run it (keylet computation happens inside the transactor)
- Over-fetching is wasteful; under-fetching forces a second round-trip
- Some transactors (AMM, path payments, offer crossing) touch many dynamically-computed keylets

**Why not shared memory / direct RocksDB access?**
- C++ linking against rocksdb creates a build-time dependency we don't want
- Rust should own ALL storage access (single source of truth for persistence)
- Callback pattern lets us swap storage backends without touching C++

## Data flow

```
┌───────────────────────────────────────────────────────────────┐
│ Rust side (tokio task applying a tx)                          │
├───────────────────────────────────────────────────────────────┤
│                                                               │
│ 1. Take RocksDB snapshot                                      │
│    let snapshot = db.snapshot();                              │
│                                                               │
│ 2. Build lookup closure with snapshot                         │
│    let lookup = |key: &[u8;32]| snapshot.get(key).ok()        │
│                                                               │
│ 3. Pass into C++                                              │
│    let ledger = xrpl_ledger_create(engine, seq, ...,          │
│        lookup_fn, &lookup as *const _ as *mut c_void);        │
│                                                               │
│ 4. Apply tx                                                   │
│    let result = xrpl_apply_tx(ledger, tx_bytes, flags);       │
│                   │                                           │
│                   ▼                                           │
└──────────────┬────────────────────────────────────────────────┘
               │
               │ FFI boundary
               ▼
┌───────────────────────────────────────────────────────────────┐
│ C++ side (libxrpl apply)                                      │
├───────────────────────────────────────────────────────────────┤
│                                                               │
│ 5. apply() starts → constructs Sandbox                        │
│                                                               │
│ 6. Transactor calls view.peek(keylet)                         │
│    → Sandbox not found locally                                │
│      → calls into our RustBackedReadView::read(key)           │
│        → calls the lookup_fn callback with user_data          │
│                   │                                           │
│                   ▼                                           │
└──────────────┬────────────────────────────────────────────────┘
               │
               │ FFI boundary (callback)
               ▼
┌───────────────────────────────────────────────────────────────┐
│ Rust side (callback fires)                                    │
├───────────────────────────────────────────────────────────────┤
│                                                               │
│ 7. Rust fn lookup_fn(user_data, key, out_data, out_len) -> bool│
│    - Cast user_data back to &Closure                          │
│    - Call closure with key                                    │
│    - Get Option<Vec<u8>> from RocksDB                         │
│    - Stash bytes in thread-local arena (see MEMORY below)     │
│    - Write pointer + len to out_data, out_len                 │
│    - Return true (found) or false (not found)                 │
│                                                               │
└──────────────┬────────────────────────────────────────────────┘
               │
               │ returns to C++
               ▼
┌───────────────────────────────────────────────────────────────┐
│ C++ side (continues apply)                                    │
├───────────────────────────────────────────────────────────────┤
│                                                               │
│ 8. C++ deserializes bytes → STLedgerEntry                     │
│ 9. Caches in Sandbox                                          │
│ 10. Transactor mutates SLE                                    │
│ 11. apply() finishes, returns ApplyResult {TER, applied, meta}│
│ 12. Sandbox contains mutations — serialize to output buffer   │
│                                                               │
└──────────────┬────────────────────────────────────────────────┘
               │
               │ FFI boundary (return)
               ▼
┌───────────────────────────────────────────────────────────────┐
│ Rust side (processes result)                                  │
├───────────────────────────────────────────────────────────────┤
│                                                               │
│ 13. Extract TER, applied, meta via shim APIs                  │
│ 14. Iterate mutations:                                        │
│     for i in 0..xrpl_result_mutation_count(result):           │
│         let (key, kind, data) = xrpl_result_mutation_at(...); │
│         match kind {                                          │
│             Created | Modified => batch.put(key, data),       │
│             Deleted => batch.delete(key),                     │
│         }                                                     │
│ 15. db.write(batch)                                           │
│ 16. Destroy result + ledger, drop snapshot                    │
│                                                               │
└───────────────────────────────────────────────────────────────┘
```

## Memory ownership rules

| Buffer | Owner | Lifetime |
|--------|-------|----------|
| tx_bytes (input) | Rust | Until xrpl_apply_tx returns |
| SLE bytes via callback | Rust (arena) | Until xrpl_apply_tx returns |
| XrplEngine | C++ | Until xrpl_engine_destroy |
| XrplLedger | C++ | Until xrpl_ledger_destroy |
| XrplApplyResult | C++ | Until xrpl_result_destroy |
| Mutation out_data | C++ (inside result) | Until xrpl_result_destroy |
| Meta bytes | C++ (inside result) | Until xrpl_result_destroy |

**Key invariant:** `out_data` pointers returned from the callback must remain valid for the **entire duration of `xrpl_apply_tx`**, not just the callback call. This is because C++ may cache the raw bytes or slice view into its Sandbox.

**Rust implementation strategy:** The closure holds a `RefCell<Vec<Vec<u8>>>` arena. Each lookup pushes the SLE bytes into the arena and returns a pointer that's valid until the arena is dropped (end of closure scope = end of `xrpl_apply_tx` call).

```rust
struct LookupArena<'a> {
    snapshot: &'a rocksdb::Snapshot<'a>,
    // Arena keeps SLE bytes alive for the duration of apply
    stash: RefCell<Vec<Vec<u8>>>,
}

extern "C" fn lookup_fn(
    user_data: *mut c_void,
    key: *const u8,  // [u8; 32]
    out_data: *mut *const u8,
    out_len: *mut usize,
) -> bool {
    let arena = unsafe { &*(user_data as *const LookupArena) };
    let key_slice = unsafe { std::slice::from_raw_parts(key, 32) };
    match arena.snapshot.get(key_slice) {
        Ok(Some(bytes)) => {
            let mut stash = arena.stash.borrow_mut();
            stash.push(bytes);
            let last = stash.last().unwrap();
            unsafe {
                *out_data = last.as_ptr();
                *out_len = last.len();
            }
            true
        }
        _ => false,
    }
}
```

## Threading model

**Single-threaded apply:** one tx at a time through the C++ engine. No concurrency inside apply(). This matches rippled's own apply pipeline.

Rust can:
- Apply txs in parallel across DIFFERENT ledger views (each with its own XrplLedger)
- Serialize apply calls on the SAME ledger view

The callback runs on the SAME THREAD that called apply — no locking needed inside the callback.

## What about ServiceRegistry dependencies?

`apply()` takes a `ServiceRegistry&` which holds singletons: `HashRouter`, `LoadFeeTrack`, `TxQ`, etc. These need to be constructed inside `xrpl_engine_create()` and persist for the engine's lifetime.

**`HashRouter`** — deduplicates txs/proposals. Stateful but can be empty at start.
**`LoadFeeTrack`** — tracks network load for dynamic fee calculation. Rust feeds it current load stats.
**`TxQ`** — transaction queue for open ledgers. Not strictly needed for validator signing path.

Minimum viable engine: construct HashRouter, LoadFeeTrack with defaults; skip TxQ for validator use.

## Rules / Amendments

`rules_bytes` in `xrpl_ledger_create` is a concatenation of 32-byte amendment IDs that are active on this ledger. Rust fetches them from rippled's `feature` RPC or the Amendments singleton in state. C++ uses them to know which code paths to activate (e.g. AMMv2, Credentials, etc.).

## Error handling

All errors fall into one of:

1. **Callback returned false** → SLE not found → transactor sees empty view for that key → tec* result likely
2. **Malformed tx bytes** → tx_parse() would fail OR apply returns tem* result
3. **Internal C++ exception** → caught in shim, returned as NULL ApplyResult with error info
4. **Invariant check failure** → tef* result in ApplyResult

Rust must check `xrpl_result_applied()` and `xrpl_result_ter()` after every apply call.

## Performance considerations

- Each callback crosses the FFI boundary (~1-10ns on modern CPUs, negligible)
- Typical tx touches 2-10 SLEs (Payment: 2; OfferCreate: 4-8; AMM: 10+)
- Expect <100 callback calls per tx average
- Serialization overhead: SLE bytes are already in wire format, no conversion needed
- Batch commit to RocksDB after apply (not during)
