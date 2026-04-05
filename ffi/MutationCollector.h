// MutationCollector.h — captures state mutations from xrpl::apply() result.
//
// After apply() runs, call OpenView::apply(collector) to flush mutations here.
// Each SLE change becomes a (key, kind, bytes) tuple that Rust can commit.

#pragma once

#include <xrpl/ledger/RawView.h>
#include <xrpl/protocol/STLedgerEntry.h>
#include <xrpl/protocol/Serializer.h>
#include <xrpl/protocol/XRPAmount.h>

#include <cstdint>
#include <utility>
#include <vector>

namespace xrpl {

/** How an SLE was changed. Matches the shim C API: 0=Created, 1=Modified, 2=Deleted. */
enum class MutationKind : uint8_t {
    Created = 0,
    Modified = 1,
    Deleted = 2,
};

struct Mutation {
    uint256 key;
    MutationKind kind;
    /// Serialized SLE bytes (empty for Deleted).
    std::vector<uint8_t> data;
};

struct TxMutation {
    uint256 tx_hash;
    std::vector<uint8_t> tx_bytes;
    std::vector<uint8_t> meta_bytes;
};

/** Collects mutations flushed from an OpenView via OpenView::apply(TxsRawView&). */
class MutationCollector final : public TxsRawView
{
public:
    MutationCollector() = default;
    ~MutationCollector() override = default;

    // === RawView interface ===
    void rawErase(std::shared_ptr<SLE> const& sle) override;
    void rawInsert(std::shared_ptr<SLE> const& sle) override;
    void rawReplace(std::shared_ptr<SLE> const& sle) override;
    void rawDestroyXRP(XRPAmount const& fee) override;

    // === TxsRawView interface ===
    void rawTxInsert(
        ReadView::key_type const& key,
        std::shared_ptr<Serializer const> const& txn,
        std::shared_ptr<Serializer const> const& metaData) override;

    // === Accessors ===
    std::vector<Mutation> const& sle_mutations() const { return sle_mutations_; }
    std::vector<TxMutation> const& tx_mutations() const { return tx_mutations_; }
    std::int64_t drops_destroyed() const { return drops_destroyed_; }

private:
    std::vector<Mutation> sle_mutations_;
    std::vector<TxMutation> tx_mutations_;
    std::int64_t drops_destroyed_ = 0;
};

}  // namespace xrpl
