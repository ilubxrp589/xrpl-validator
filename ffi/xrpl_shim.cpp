// xrpl_shim.cpp — minimal C++ implementation linking against libxrpl.a
// Status: SCAFFOLDING (2026-04-05)
//
// First milestone: verify we can compile against libxrpl headers, link
// against libxrpl.a, and expose a version string via extern "C".

#include "xrpl_shim.h"
#include "MinimalServiceRegistry.h"
#include "CallbackReadView.h"

#include <xrpl/protocol/BuildInfo.h>
#include <xrpl/protocol/LedgerHeader.h>
#include <xrpl/protocol/Rules.h>
#include <xrpl/protocol/STTx.h>
#include <xrpl/protocol/Serializer.h>
#include <xrpl/protocol/TER.h>
#include <xrpl/protocol/TxFormats.h>
#include <xrpl/protocol/XRPAmount.h>
#include <xrpl/ledger/OpenView.h>
#include <xrpl/tx/apply.h>
#include <xrpl/tx/applySteps.h>
#include <xrpl/beast/utility/Journal.h>

#include <cstdio>
#include <cstring>
#include <stdexcept>
#include <string>
#include <unordered_set>

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

int32_t xrpl_preflight(
    const uint8_t *tx_bytes, size_t tx_len,
    const uint8_t *amendments_bytes, size_t amendments_len,
    uint32_t apply_flags,
    uint32_t network_id,
    char *out_ter_name, size_t ter_name_buf_len) {
    try {
        // Parse tx
        xrpl::SerialIter sit(tx_bytes, tx_len);
        xrpl::STTx tx(sit);

        // Build Rules from amendment list (each 32 bytes, concatenated)
        std::unordered_set<xrpl::uint256, beast::uhash<>> presets;
        if (amendments_bytes && amendments_len > 0 && amendments_len % 32 == 0) {
            size_t n = amendments_len / 32;
            for (size_t i = 0; i < n; i++) {
                xrpl::uint256 feature;
                std::memcpy(feature.data(), amendments_bytes + i * 32, 32);
                presets.insert(feature);
            }
        }
        xrpl::Rules rules(presets);

        // Construct MinimalServiceRegistry
        xrpl::MinimalServiceRegistry registry(network_id);

        // Null journal
        beast::Journal journal(beast::Journal::getNullSink());

        // Run preflight
        auto result = xrpl::preflight(
            registry,
            rules,
            tx,
            static_cast<xrpl::ApplyFlags>(apply_flags),
            journal);

        // Extract TER (TERtoInt is a hidden friend, found via ADL)
        xrpl::TER ter = result.ter;
        int32_t ter_code = TERtoInt(ter);

        // Copy TER name string
        if (out_ter_name && ter_name_buf_len > 0) {
            std::string name = xrpl::transToken(ter);
            size_t copy_len = std::min(name.size(), ter_name_buf_len - 1);
            std::memcpy(out_ter_name, name.data(), copy_len);
            out_ter_name[copy_len] = '\0';
        }

        return ter_code;
    } catch (std::exception const& e) {
        if (out_ter_name && ter_name_buf_len > 0) {
            std::snprintf(out_ter_name, ter_name_buf_len, "EXCEPTION: %s", e.what());
        }
        return -399;  // tefINTERNAL
    } catch (...) {
        if (out_ter_name && ter_name_buf_len > 0) {
            std::snprintf(out_ter_name, ter_name_buf_len, "UNKNOWN_EXCEPTION");
        }
        return -399;
    }
}

int32_t xrpl_apply(
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
    void *lookup_user_data,
    char *out_ter_name, size_t ter_name_buf_len,
    bool *out_applied) {
    if (out_applied) *out_applied = false;
    try {
        // Parse tx
        xrpl::SerialIter sit(tx_bytes, tx_len);
        xrpl::STTx tx(sit);

        // Build Rules
        std::unordered_set<xrpl::uint256, beast::uhash<>> presets;
        if (amendments_bytes && amendments_len > 0 && amendments_len % 32 == 0) {
            size_t n = amendments_len / 32;
            for (size_t i = 0; i < n; i++) {
                xrpl::uint256 feature;
                std::memcpy(feature.data(), amendments_bytes + i * 32, 32);
                presets.insert(feature);
            }
        }
        xrpl::Rules rules(presets);

        // Build LedgerHeader
        xrpl::LedgerHeader header;
        header.seq = ledger_seq;
        header.parentCloseTime = xrpl::NetClock::time_point(xrpl::NetClock::duration(parent_close_time));
        std::memcpy(header.parentHash.data(), parent_hash, 32);
        header.drops = xrpl::XRPAmount(static_cast<std::int64_t>(total_drops));
        header.closeTimeResolution = xrpl::NetClock::duration(10);

        // Build Fees
        xrpl::Fees fees;
        fees.base = xrpl::XRPAmount(static_cast<std::int64_t>(base_fee_drops));
        fees.reserve = xrpl::XRPAmount(static_cast<std::int64_t>(reserve_drops));
        fees.increment = xrpl::XRPAmount(static_cast<std::int64_t>(increment_drops));

        // Wrap C callback in std::function
        xrpl::SleLookupCallback cpp_lookup =
            [lookup_fn, lookup_user_data](xrpl::uint256 const& key, uint8_t const** out_data, size_t* out_len) -> bool {
                return lookup_fn(lookup_user_data, key.data(), out_data, out_len);
            };

        // Construct CallbackReadView
        auto read_view = std::make_shared<xrpl::CallbackReadView>(
            header, rules, fees, /*open=*/true, cpp_lookup);

        // Construct OpenView (wraps the ReadView for writable apply)
        xrpl::OpenView open_view(xrpl::open_ledger, rules, read_view);

        // Construct ServiceRegistry
        xrpl::MinimalServiceRegistry registry(network_id);

        // Null journal
        beast::Journal journal(beast::Journal::getNullSink());

        // Call apply()
        auto result = xrpl::apply(
            registry,
            open_view,
            tx,
            static_cast<xrpl::ApplyFlags>(apply_flags),
            journal);

        if (out_applied) *out_applied = result.applied;

        // Extract TER
        int32_t ter_code = TERtoInt(result.ter);

        // Copy TER name
        if (out_ter_name && ter_name_buf_len > 0) {
            std::string name = xrpl::transToken(result.ter);
            size_t copy_len = std::min(name.size(), ter_name_buf_len - 1);
            std::memcpy(out_ter_name, name.data(), copy_len);
            out_ter_name[copy_len] = '\0';
        }

        return ter_code;
    } catch (std::exception const& e) {
        if (out_ter_name && ter_name_buf_len > 0) {
            std::snprintf(out_ter_name, ter_name_buf_len, "EXCEPTION: %s", e.what());
        }
        return -399;
    } catch (...) {
        if (out_ter_name && ter_name_buf_len > 0) {
            std::snprintf(out_ter_name, ter_name_buf_len, "UNKNOWN_EXCEPTION");
        }
        return -399;
    }
}

}  // extern "C"
