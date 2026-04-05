// CallbackReadView.h — minimal ReadView that proxies SLE lookups to a C callback.
//
// Design: Rust owns RocksDB. When libxrpl needs an SLE, it calls read(Keylet),
// which calls our callback (a function pointer + user_data), which queries
// Rust and returns raw SLE bytes. We deserialize via STLedgerEntry and return.
//
// Iteration methods throw — they're not needed for single-tx apply.

#pragma once

#include <xrpl/ledger/ReadView.h>
#include <xrpl/protocol/LedgerHeader.h>
#include <xrpl/protocol/Rules.h>
#include <xrpl/protocol/Fees.h>

#include <functional>
#include <optional>

namespace xrpl {

/** Callback signature for SLE lookup.
 *  Returns true if found, fills out_data and out_len.
 *  The pointer must remain valid for the duration of the apply() call.
 */
using SleLookupCallback = std::function<bool(
    uint256 const& key,
    uint8_t const** out_data,
    size_t* out_len)>;

class CallbackReadView : public ReadView
{
public:
    CallbackReadView(
        LedgerHeader const& header,
        Rules const& rules,
        Fees const& fees,
        bool open,
        SleLookupCallback lookup);

    ~CallbackReadView() override = default;

    // === Header / metadata ===
    LedgerHeader const& header() const override { return header_; }
    bool open() const override { return open_; }
    Fees const& fees() const override { return fees_; }
    Rules const& rules() const override { return rules_; }

    // === SLE lookup (via callback) ===
    bool exists(Keylet const& k) const override;
    std::shared_ptr<SLE const> read(Keylet const& k) const override;
    std::optional<key_type> succ(
        key_type const& key,
        std::optional<key_type> const& last = std::nullopt) const override;

    // === txRead / txExists — not needed for apply ===
    bool txExists(key_type const& key) const override;
    tx_type txRead(key_type const& key) const override;

    // === Balance hook (not typically overridden for apply) ===
    STAmount balanceHook(
        AccountID const& account,
        AccountID const& issuer,
        STAmount const& amount) const override { return amount; }

    std::uint32_t ownerCountHook(
        AccountID const& /*account*/,
        std::uint32_t count) const override { return count; }

    // === Iteration — THROWS (not needed for single-tx apply) ===
    std::unique_ptr<sles_type::iter_base> slesBegin() const override;
    std::unique_ptr<sles_type::iter_base> slesEnd() const override;
    std::unique_ptr<sles_type::iter_base> slesUpperBound(key_type const& key) const override;
    std::unique_ptr<txs_type::iter_base> txsBegin() const override;
    std::unique_ptr<txs_type::iter_base> txsEnd() const override;

private:
    LedgerHeader header_;
    Rules rules_;
    Fees fees_;
    bool open_;
    SleLookupCallback lookup_;
};

}  // namespace xrpl
