// xrpl_shim.cpp — C ABI over rippled 3.1.2's xrpl::apply / preflight path.
//
// Namespace note: rippled 3.1.2 uses `namespace ripple`. Our helpers
// (MinimalApp, MutationCollector, CallbackReadView) live in the same
// namespace for ADL friendliness and so we don't need an alias.

#include "xrpl_shim.h"
#include "MinimalApp.h"
#include "CallbackReadView.h"
#include "MutationCollector.h"

#include <xrpl/protocol/BuildInfo.h>
#include <xrpl/protocol/LedgerHeader.h>
#include <xrpl/protocol/Rules.h>
#include <xrpl/protocol/STTx.h>
#include <xrpl/protocol/Serializer.h>
#include <xrpl/protocol/TER.h>
#include <xrpl/protocol/TxFormats.h>
#include <xrpl/protocol/XRPAmount.h>
#include <xrpl/protocol/Fees.h>
#include <xrpl/ledger/OpenView.h>
#include <xrpld/app/tx/apply.h>
#include <xrpld/app/tx/applySteps.h>
#include <xrpl/beast/utility/Journal.h>

#include <cstdio>
#include <cstring>
#include <stdexcept>
#include <string>
#include <unordered_set>

namespace {
constexpr const char *SHIM_VERSION = "0.2.0";  // bumped for 3.1.2 Application port
}

struct XrplEngine {
    int placeholder;
};

struct XrplLedger {
    int placeholder;
};

struct XrplApplyResult {
    int32_t ter;
    bool applied;
    std::string ter_name;
    std::string last_fatal;
    ripple::MutationCollector collector;
};

// Journal sink that captures error-and-above messages into a std::string.
class CapturingSink : public beast::Journal::Sink {
public:
    CapturingSink(std::string& dest)
        : beast::Journal::Sink(beast::severities::kFatal, false), dest_(dest) {}
    void write(beast::severities::Severity level, std::string const& text) override {
        if (level >= beast::severities::kError) {
            if (!dest_.empty()) dest_.append(" | ");
            dest_.append(text);
        }
    }
    void writeAlways(beast::severities::Severity level, std::string const& text) override {
        write(level, text);
    }
private:
    std::string& dest_;
};

extern "C" {

const char *xrpl_shim_version(void) {
    return SHIM_VERSION;
}

const char *xrpl_rippled_version(void) {
    static std::string v = ripple::BuildInfo::getVersionString();
    return v.c_str();
}

XrplEngine *xrpl_engine_create(void) {
    return new XrplEngine{0};
}

void xrpl_engine_destroy(XrplEngine *engine) {
    delete engine;
}

// Stubs (not exercised by the Rust apply_ledger_in_order path)
XrplLedger *xrpl_ledger_create(
    XrplEngine *, uint32_t, uint32_t, uint32_t, uint64_t,
    const uint8_t[32], const uint8_t *, size_t,
    XrplSleLookupFn, void *) {
    return new XrplLedger{0};
}
void xrpl_ledger_destroy(XrplLedger *ledger) { delete ledger; }

XrplApplyResult *xrpl_apply_tx(XrplLedger *, const uint8_t *, size_t, uint32_t) {
    auto *r = new XrplApplyResult{};
    r->ter = -399;
    r->applied = false;
    r->ter_name = "tefINTERNAL";
    return r;
}

int32_t xrpl_result_ter(const XrplApplyResult *r) { return r->ter; }
bool xrpl_result_applied(const XrplApplyResult *r) { return r->applied; }
const char *xrpl_result_ter_name(const XrplApplyResult *r) { return r->ter_name.c_str(); }

size_t xrpl_result_meta_size(const XrplApplyResult *) { return 0; }
void xrpl_result_meta_bytes(const XrplApplyResult *, uint8_t *, size_t) {}

size_t xrpl_result_mutation_count(const XrplApplyResult *r) {
    return r ? r->collector.sle_mutations().size() : 0;
}

bool xrpl_result_mutation_at(
    const XrplApplyResult *r,
    size_t index,
    uint8_t out_key[32],
    uint8_t *out_kind,
    const uint8_t **out_data,
    size_t *out_data_len) {
    if (!r || index >= r->collector.sle_mutations().size()) return false;
    auto const& m = r->collector.sle_mutations()[index];
    if (out_key) std::memcpy(out_key, m.key.data(), 32);
    if (out_kind) *out_kind = static_cast<uint8_t>(m.kind);
    if (out_data) *out_data = m.data.empty() ? nullptr : m.data.data();
    if (out_data_len) *out_data_len = m.data.size();
    return true;
}

int64_t xrpl_result_drops_destroyed(const XrplApplyResult *r) {
    return r ? r->collector.drops_destroyed() : 0;
}

const char *xrpl_result_last_fatal(const XrplApplyResult *r) {
    return r ? r->last_fatal.c_str() : "";
}

void xrpl_result_destroy(XrplApplyResult *r) { delete r; }

bool xrpl_tx_parse(
    const uint8_t *tx_bytes, size_t tx_len,
    uint8_t out_hash[32],
    char *out_type_name, size_t type_name_buf_len) {
    try {
        ripple::SerialIter sit(tx_bytes, tx_len);
        ripple::STTx tx(sit);

        auto const id = tx.getTransactionID();
        std::memcpy(out_hash, id.data(), 32);

        ripple::TxType type = tx.getTxnType();
        auto const *item = ripple::TxFormats::getInstance().findByType(type);
        std::string name = item ? item->getName() : "Unknown";

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
        ripple::SerialIter sit(tx_bytes, tx_len);
        ripple::STTx tx(sit);

        std::unordered_set<ripple::uint256, beast::uhash<>> presets;
        if (amendments_bytes && amendments_len > 0 && amendments_len % 32 == 0) {
            size_t n = amendments_len / 32;
            for (size_t i = 0; i < n; i++) {
                ripple::uint256 feature;
                std::memcpy(feature.data(), amendments_bytes + i * 32, 32);
                presets.insert(feature);
            }
        }
        ripple::Rules rules(presets);

        ripple::MinimalApp app(network_id);
        beast::Journal journal(beast::Journal::getNullSink());

        auto result = ripple::preflight(
            app,
            rules,
            tx,
            static_cast<ripple::ApplyFlags>(apply_flags),
            journal);

        ripple::TER ter = result.ter;
        int32_t ter_code = TERtoInt(ter);

        if (out_ter_name && ter_name_buf_len > 0) {
            std::string name = ripple::transToken(ter);
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
        ripple::SerialIter sit(tx_bytes, tx_len);
        ripple::STTx tx(sit);

        std::unordered_set<ripple::uint256, beast::uhash<>> presets;
        if (amendments_bytes && amendments_len > 0 && amendments_len % 32 == 0) {
            size_t n = amendments_len / 32;
            for (size_t i = 0; i < n; i++) {
                ripple::uint256 feature;
                std::memcpy(feature.data(), amendments_bytes + i * 32, 32);
                presets.insert(feature);
            }
        }
        ripple::Rules rules(presets);

        ripple::LedgerHeader header;
        header.seq = ledger_seq > 0 ? ledger_seq - 1 : 0;
        // header represents ledger N-1 (the parent of the ledger being applied
        // to). When OpenView wraps it via the open_ledger overload, it copies
        // header.closeTime into info_.parentCloseTime (see OpenView.cpp:105).
        // Time-based checks during apply (offer Expiration, escrow FinishAfter,
        // check Expiration, loan due-date) all read view.parentCloseTime(), so
        // header.closeTime MUST be the parent ledger's close_time. We don't
        // separately need to set header.parentCloseTime — OpenView discards
        // base.parentCloseTime — but we set both for diagnostic readers that
        // might inspect the LedgerHeader directly.
        header.closeTime = ripple::NetClock::time_point(ripple::NetClock::duration(parent_close_time));
        header.parentCloseTime = ripple::NetClock::time_point(ripple::NetClock::duration(parent_close_time));
        std::memcpy(header.parentHash.data(), parent_hash, 32);
        header.drops = ripple::XRPAmount(static_cast<std::int64_t>(total_drops));
        header.closeTimeResolution = ripple::NetClock::duration(10);

        ripple::Fees fees;
        fees.base = ripple::XRPAmount(static_cast<std::int64_t>(base_fee_drops));
        fees.reserve = ripple::XRPAmount(static_cast<std::int64_t>(reserve_drops));
        fees.increment = ripple::XRPAmount(static_cast<std::int64_t>(increment_drops));

        ripple::SleLookupCallback cpp_lookup =
            [lookup_fn, lookup_user_data](ripple::uint256 const& key, uint8_t const** out_data, size_t* out_len) -> bool {
                return lookup_fn(lookup_user_data, key.data(), out_data, out_len);
            };

        // Replay path — closed-ledger semantics so ApplyStateTable::apply()
        // takes the meta-build branch (see OpenView.cpp + ApplyStateTable.cpp).
        // open=false makes view.open() return false; OpenView's non-open_ledger
        // ctor inherits that, gating meta-build on `if (!to.open() ...)` (line 127).
        // Meta-build runs threadItem + threadOwners, which add issuer/counterparty
        // AccountRoot mutations our open-ledger path was silently dropping.
        auto read_view = std::make_shared<ripple::CallbackReadView>(
            header, rules, fees, /*open=*/false, cpp_lookup);

        ripple::OpenView open_view(read_view.get(), read_view);

        ripple::MinimalApp app(network_id);
        beast::Journal journal(beast::Journal::getNullSink());

        auto result = ripple::apply(
            app,
            open_view,
            tx,
            static_cast<ripple::ApplyFlags>(apply_flags),
            journal);

        if (out_applied) *out_applied = result.applied;

        int32_t ter_code = TERtoInt(result.ter);

        if (out_ter_name && ter_name_buf_len > 0) {
            std::string name = ripple::transToken(result.ter);
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
    void *lookup_user_data,
    XrplSuccFn succ_fn,
    void *succ_user_data) {
    auto *result = new XrplApplyResult{};
    try {
        ripple::SerialIter sit(tx_bytes, tx_len);
        ripple::STTx tx(sit);

        std::unordered_set<ripple::uint256, beast::uhash<>> presets;
        if (amendments_bytes && amendments_len > 0 && amendments_len % 32 == 0) {
            size_t n = amendments_len / 32;
            for (size_t i = 0; i < n; i++) {
                ripple::uint256 feature;
                std::memcpy(feature.data(), amendments_bytes + i * 32, 32);
                presets.insert(feature);
            }
        }
        ripple::Rules rules(presets);

        ripple::LedgerHeader header;
        header.seq = ledger_seq > 0 ? ledger_seq - 1 : 0;
        // header represents ledger N-1 (the parent of the ledger being applied
        // to). When OpenView wraps it via the open_ledger overload, it copies
        // header.closeTime into info_.parentCloseTime (see OpenView.cpp:105).
        // Time-based checks during apply (offer Expiration, escrow FinishAfter,
        // check Expiration, loan due-date) all read view.parentCloseTime(), so
        // header.closeTime MUST be the parent ledger's close_time. We don't
        // separately need to set header.parentCloseTime — OpenView discards
        // base.parentCloseTime — but we set both for diagnostic readers that
        // might inspect the LedgerHeader directly.
        header.closeTime = ripple::NetClock::time_point(ripple::NetClock::duration(parent_close_time));
        header.parentCloseTime = ripple::NetClock::time_point(ripple::NetClock::duration(parent_close_time));
        std::memcpy(header.parentHash.data(), parent_hash, 32);
        header.drops = ripple::XRPAmount(static_cast<std::int64_t>(total_drops));
        header.closeTimeResolution = ripple::NetClock::duration(10);

        ripple::Fees fees;
        fees.base = ripple::XRPAmount(static_cast<std::int64_t>(base_fee_drops));
        fees.reserve = ripple::XRPAmount(static_cast<std::int64_t>(reserve_drops));
        fees.increment = ripple::XRPAmount(static_cast<std::int64_t>(increment_drops));

        ripple::SleLookupCallback cpp_lookup =
            [lookup_fn, lookup_user_data](ripple::uint256 const& key, uint8_t const** out_data, size_t* out_len) -> bool {
                return lookup_fn(lookup_user_data, key.data(), out_data, out_len);
            };

        ripple::SleSuccCallback cpp_succ;
        if (succ_fn) {
            cpp_succ = [succ_fn, succ_user_data](ripple::uint256 const& key, ripple::uint256 const* last, ripple::uint256& out) -> bool {
                return succ_fn(succ_user_data, key.data(), last ? last->data() : nullptr, out.data());
            };
        }

        // Replay path — closed-ledger semantics so ApplyStateTable::apply()
        // takes the meta-build branch. Same rationale as the apply path above.
        // open=false → view.open()=false → meta-build runs → threadItem +
        // threadOwners populate items_ with issuer/counterparty AccountRoot
        // entries that our MutationCollector then receives via the outer
        // apply(RawView&) flush below.
        auto read_view = std::make_shared<ripple::CallbackReadView>(
            header, rules, fees, /*open=*/false, cpp_lookup, cpp_succ);
        ripple::OpenView open_view(read_view.get(), read_view);

        ripple::MinimalApp app(network_id);
        CapturingSink capturing_sink(result->last_fatal);
        beast::Journal journal(capturing_sink);

        auto apply_result = ripple::apply(
            app, open_view, tx,
            static_cast<ripple::ApplyFlags>(apply_flags), journal);

        result->ter = TERtoInt(apply_result.ter);
        result->applied = apply_result.applied;
        result->ter_name = ripple::transToken(apply_result.ter);

        open_view.apply(result->collector);

        return result;
    } catch (std::exception const& e) {
        result->ter = -399;
        result->applied = false;
        result->ter_name = std::string("EXCEPTION: ") + e.what();
        return result;
    } catch (...) {
        result->ter = -399;
        result->applied = false;
        result->ter_name = "UNKNOWN_EXCEPTION";
        return result;
    }
}

bool xrpl_test_callback_read(
    uint16_t keylet_type,
    const uint8_t key[32],
    const uint8_t *sle_bytes,
    size_t sle_len) {
    try {
        std::unordered_set<ripple::uint256, beast::uhash<>> presets;
        ripple::Rules rules(presets);

        ripple::LedgerHeader header;
        header.seq = 0;
        header.closeTimeResolution = ripple::NetClock::duration(10);

        ripple::Fees fees;
        fees.base = ripple::XRPAmount(static_cast<std::int64_t>(10));
        fees.reserve = ripple::XRPAmount(static_cast<std::int64_t>(1'000'000));
        fees.increment = ripple::XRPAmount(static_cast<std::int64_t>(200'000));

        ripple::SleLookupCallback cpp_lookup =
            [sle_bytes, sle_len](ripple::uint256 const&, uint8_t const** out_data, size_t* out_len) -> bool {
                if (!sle_bytes || sle_len == 0) return false;
                *out_data = sle_bytes;
                *out_len = sle_len;
                return true;
            };
        ripple::SleSuccCallback cpp_succ;  // null — read path doesn't need succ

        ripple::CallbackReadView view(header, rules, fees, /*open=*/true, cpp_lookup, cpp_succ);

        ripple::uint256 k;
        std::memcpy(k.data(), key, 32);
        ripple::Keylet keylet{static_cast<ripple::LedgerEntryType>(keylet_type), k};

        return view.read(keylet) != nullptr;
    } catch (...) {
        return false;
    }
}

}  // extern "C"
