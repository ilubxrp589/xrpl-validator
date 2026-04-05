/*
 * xrpl_shim.h — C API shim over libxrpl for the Rust validator.
 *
 * Design goal: minimum surface area needed to apply a transaction and extract
 * state mutations. Rust owns RocksDB and provides SLE lookups via a callback;
 * C++ owns the transaction engine and returns mutations for Rust to commit.
 *
 * Status: DRAFT (2026-04-05). First pass — will be refined as we build.
 *
 * Architecture:
 *   Rust (RocksDB state, tx_engine Rust side)
 *     ↓  raw tx bytes + SLE lookup callback
 *   C API (this shim)
 *     ↓  constructs STTx, OpenView, Sandbox
 *   libxrpl::apply() → libxrpl transactors → runs preflight/preclaim/doApply
 *     ↓  extract mutations + TER + TxMeta
 *   C API returns
 *     ↓  mutations
 *   Rust commits to RocksDB
 */

#ifndef XRPL_SHIM_H
#define XRPL_SHIM_H

#include <stdint.h>
#include <stddef.h>
#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ============================================================
 * Opaque handle types
 * ============================================================ */

typedef struct XrplEngine XrplEngine;
typedef struct XrplLedger XrplLedger;
typedef struct XrplApplyResult XrplApplyResult;

/* ============================================================
 * Callback for SLE lookup (Rust side provides this)
 *
 * Called by C++ when it needs to read a ledger object.
 * Returns true if found. Rust fills out_data (must be valid until
 * xrpl_ledger_destroy is called) and out_len.
 *
 * Rust owns the memory — do not free from C.
 * ============================================================ */

typedef bool (*XrplSleLookupFn)(
    void *user_data,
    const uint8_t key[32],
    const uint8_t **out_data,
    size_t *out_len
);

/* ============================================================
 * Engine lifecycle
 *
 * Create once at startup. Holds: HashRouter, LoadFeeTrack, ServiceRegistry.
 * Thread-safe after creation (but applying txs requires ordering).
 * ============================================================ */

XrplEngine *xrpl_engine_create(void);
void xrpl_engine_destroy(XrplEngine *engine);

/* Version info for compatibility checking with Rust bindings */
const char *xrpl_shim_version(void);      /* e.g. "0.1.0" */
const char *xrpl_rippled_version(void);   /* e.g. "3.0.0" */

/* ============================================================
 * Ledger construction
 *
 * Build an OpenView that Rust can apply transactions against.
 * The lookup_fn is called by C++ when it needs SLEs from state.
 *
 * rules_bytes: active amendments as 32-byte feature IDs concatenated.
 * ============================================================ */

XrplLedger *xrpl_ledger_create(
    XrplEngine *engine,
    uint32_t ledger_seq,
    uint32_t parent_close_time,
    uint32_t close_time_resolution,
    uint64_t total_coins,
    const uint8_t parent_hash[32],
    const uint8_t *rules_bytes, size_t rules_len,
    XrplSleLookupFn lookup_fn,
    void *lookup_user_data
);

void xrpl_ledger_destroy(XrplLedger *ledger);

/* ============================================================
 * Transaction application
 *
 * Calls libxrpl::apply() internally (preflight→preclaim→doApply).
 * Returns opaque result handle. Caller must destroy.
 *
 * ApplyFlags bitmask (matches rippled's):
 *   tapNONE        = 0x00
 *   tapRETRY       = 0x01
 *   tapPREFER_QUEUE= 0x02
 *   tapUNLIMITED   = 0x04
 *   tapFAIL_HARD   = 0x10
 * ============================================================ */

XrplApplyResult *xrpl_apply_tx(
    XrplLedger *ledger,
    const uint8_t *tx_bytes, size_t tx_len,
    uint32_t apply_flags
);

/* ============================================================
 * Result inspection
 *
 * TER codes (matches rippled's TER enum):
 *   Positive small = success (tesSUCCESS = 0)
 *   Negative = failure
 *   See rippled TER.h for full enum
 * ============================================================ */

int32_t xrpl_result_ter(const XrplApplyResult *result);
bool xrpl_result_applied(const XrplApplyResult *result);

/* Get human-readable result string like "tesSUCCESS" or "tecUNFUNDED" */
const char *xrpl_result_ter_name(const XrplApplyResult *result);

/* TxMeta (AffectedNodes + DeliveredAmount + TransactionResult) as XRPL binary */
size_t xrpl_result_meta_size(const XrplApplyResult *result);
void xrpl_result_meta_bytes(const XrplApplyResult *result, uint8_t *out_buf, size_t buf_len);

/* ============================================================
 * State mutation extraction
 *
 * After apply succeeds, extract the SLE changes for Rust to commit.
 *
 * mutation_kind:
 *   0 = Created  (new SLE)
 *   1 = Modified (updated SLE)
 *   2 = Deleted  (SLE removed)
 * ============================================================ */

size_t xrpl_result_mutation_count(const XrplApplyResult *result);

bool xrpl_result_mutation_at(
    const XrplApplyResult *result,
    size_t index,
    uint8_t out_key[32],
    uint8_t *out_kind,
    const uint8_t **out_data, size_t *out_data_len  /* null if Deleted */
);

void xrpl_result_destroy(XrplApplyResult *result);

/* ============================================================
 * Utility: parse raw tx bytes, return tx hash + type name
 *
 * Useful for validating/inspecting before full apply.
 * ============================================================ */

bool xrpl_tx_parse(
    const uint8_t *tx_bytes, size_t tx_len,
    uint8_t out_hash[32],
    char *out_type_name, size_t type_name_buf_len
);

/* ============================================================
 * Signature check only (no state access)
 * ============================================================ */

bool xrpl_tx_check_signature(
    const uint8_t *tx_bytes, size_t tx_len,
    const uint8_t *rules_bytes, size_t rules_len  /* active amendments */
);

/* ============================================================
 * preflight — tx format + signature validation (no ledger state)
 *
 * Calls libxrpl::preflight() which runs:
 *   - Basic format checks (fee > 0, fields present, etc.)
 *   - Signature verification against tx's public key
 *
 * Returns the TER result code. Positive/0 = success (tesSUCCESS),
 * Negative = failure. See TER.h for full enum.
 *
 * Does not need ledger state — just Rules (amendments) and the tx.
 * ============================================================ */

int32_t xrpl_preflight(
    const uint8_t *tx_bytes, size_t tx_len,
    const uint8_t *amendments_bytes, size_t amendments_len,  /* concatenated 32-byte feature IDs */
    uint32_t apply_flags,
    uint32_t network_id,
    /* Output */
    char *out_ter_name, size_t ter_name_buf_len
);

/* ============================================================
 * apply — full transaction application against a ledger view.
 *
 * Runs: preflight → preclaim → doApply through libxrpl.
 *
 * Needs a callback to read SLEs from Rust-owned state.
 * The callback's returned data pointer must remain valid for the duration
 * of this function call (lives in an arena managed by Rust).
 *
 * Returns TER code. out_applied says whether the tx was "applied" (fee claimed).
 * ============================================================ */

int32_t xrpl_apply(
    /* Transaction */
    const uint8_t *tx_bytes, size_t tx_len,
    /* Rules */
    const uint8_t *amendments_bytes, size_t amendments_len,
    /* Ledger header */
    uint32_t ledger_seq,
    uint32_t parent_close_time,
    uint64_t total_drops,
    const uint8_t parent_hash[32],
    /* Fees for this ledger */
    uint64_t base_fee_drops,
    uint64_t reserve_drops,
    uint64_t increment_drops,
    /* Application params */
    uint32_t apply_flags,
    uint32_t network_id,
    /* SLE lookup callback (Rust side) */
    XrplSleLookupFn lookup_fn,
    void *lookup_user_data,
    /* Output */
    char *out_ter_name, size_t ter_name_buf_len,
    bool *out_applied
);

/* ============================================================
 * Apply with mutations — same as xrpl_apply() but returns an
 * XrplApplyResult handle that you can iterate for mutations.
 *
 * Caller MUST call xrpl_result_destroy() to free it.
 * Returns NULL on internal error.
 * ============================================================ */

XrplApplyResult *xrpl_apply_with_mutations(
    const uint8_t *tx_bytes, size_t tx_len,
    const uint8_t *amendments_bytes, size_t amendments_len,
    uint32_t ledger_seq,
    uint32_t parent_close_time,
    uint64_t total_drops,
    const uint8_t parent_hash[32],
    uint64_t base_fee_drops,
    uint64_t reserve_drops,
    uint64_t increment_drops,
    uint32_t apply_flags,
    uint32_t network_id,
    XrplSleLookupFn lookup_fn,
    void *lookup_user_data
);

int64_t xrpl_result_drops_destroyed(const XrplApplyResult *result);

#ifdef __cplusplus
}  /* extern "C" */
#endif

#endif /* XRPL_SHIM_H */
