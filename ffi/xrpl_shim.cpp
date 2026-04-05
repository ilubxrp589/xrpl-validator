// xrpl_shim.cpp — minimal C++ implementation linking against libxrpl.a
// Status: SCAFFOLDING (2026-04-05)
//
// First milestone: verify we can compile against libxrpl headers, link
// against libxrpl.a, and expose a version string via extern "C".

#include "xrpl_shim.h"

#include <xrpl/protocol/BuildInfo.h>
#include <xrpl/protocol/STTx.h>
#include <xrpl/protocol/Serializer.h>
#include <xrpl/protocol/TxFormats.h>

#include <cstring>
#include <stdexcept>
#include <string>

namespace {
constexpr const char *SHIM_VERSION = "0.1.0";
}

// Note: This stub intentionally uses only header-only parts of libxrpl for now.
// We'll add real engine construction in the next milestone.

struct XrplEngine {
    // Empty for now — will hold ServiceRegistry, HashRouter, LoadFeeTrack
    int placeholder;
};

struct XrplLedger {
    int placeholder;
};

struct XrplApplyResult {
    int32_t ter;
    bool applied;
};

extern "C" {

const char *xrpl_shim_version(void) {
    return SHIM_VERSION;
}

const char *xrpl_rippled_version(void) {
    // Pulled from libxrpl BuildInfo
    static std::string v = xrpl::BuildInfo::getVersionString();
    return v.c_str();
}

XrplEngine *xrpl_engine_create(void) {
    return new XrplEngine{0};
}

void xrpl_engine_destroy(XrplEngine *engine) {
    delete engine;
}

// Stubs for other functions — will be implemented in next milestones
XrplLedger *xrpl_ledger_create(
    XrplEngine *, uint32_t, uint32_t, uint32_t, uint64_t,
    const uint8_t[32], const uint8_t *, size_t,
    XrplSleLookupFn, void *) {
    return new XrplLedger{0};
}

void xrpl_ledger_destroy(XrplLedger *ledger) {
    delete ledger;
}

XrplApplyResult *xrpl_apply_tx(
    XrplLedger *, const uint8_t *, size_t, uint32_t) {
    // Stub: return a fake "not implemented" result
    return new XrplApplyResult{-399, false};  // -399 = tefINTERNAL
}

int32_t xrpl_result_ter(const XrplApplyResult *r) { return r->ter; }
bool xrpl_result_applied(const XrplApplyResult *r) { return r->applied; }
const char *xrpl_result_ter_name(const XrplApplyResult *) { return "tefINTERNAL"; }

size_t xrpl_result_meta_size(const XrplApplyResult *) { return 0; }
void xrpl_result_meta_bytes(const XrplApplyResult *, uint8_t *, size_t) {}

size_t xrpl_result_mutation_count(const XrplApplyResult *) { return 0; }
bool xrpl_result_mutation_at(
    const XrplApplyResult *, size_t, uint8_t[32],
    uint8_t *, const uint8_t **, size_t *) {
    return false;
}

void xrpl_result_destroy(XrplApplyResult *r) { delete r; }

bool xrpl_tx_parse(
    const uint8_t *tx_bytes, size_t tx_len,
    uint8_t out_hash[32],
    char *out_type_name, size_t type_name_buf_len) {
    try {
        xrpl::SerialIter sit(tx_bytes, tx_len);
        xrpl::STTx tx(sit);

        // Write tx hash (32 bytes)
        auto const id = tx.getTransactionID();
        std::memcpy(out_hash, id.data(), 32);

        // Look up type name via TxFormats
        xrpl::TxType type = tx.getTxnType();
        auto const *item = xrpl::TxFormats::getInstance().findByType(type);
        std::string name = item ? item->getName() : "Unknown";

        // Copy type name (null-terminated, truncate if needed)
        if (out_type_name && type_name_buf_len > 0) {
            size_t copy_len = std::min(name.size(), type_name_buf_len - 1);
            std::memcpy(out_type_name, name.data(), copy_len);
            out_type_name[copy_len] = '\0';
        }
        return true;
    } catch (...) {
        return false;
    }
}

bool xrpl_tx_check_signature(const uint8_t *, size_t, const uint8_t *, size_t) {
    return false;
}

}  // extern "C"
