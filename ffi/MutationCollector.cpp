// MutationCollector.cpp

#include "MutationCollector.h"

namespace ripple {

static std::vector<uint8_t> serialize_sle(SLE const& sle) {
    Serializer s;
    sle.add(s);
    std::vector<uint8_t> out(s.getDataLength());
    std::memcpy(out.data(), s.data(), s.getDataLength());
    return out;
}

void MutationCollector::rawErase(std::shared_ptr<SLE> const& sle) {
    sle_mutations_.push_back(Mutation{
        sle->key(),
        MutationKind::Deleted,
        {}
    });
}

void MutationCollector::rawInsert(std::shared_ptr<SLE> const& sle) {
    sle_mutations_.push_back(Mutation{
        sle->key(),
        MutationKind::Created,
        serialize_sle(*sle)
    });
}

void MutationCollector::rawReplace(std::shared_ptr<SLE> const& sle) {
    sle_mutations_.push_back(Mutation{
        sle->key(),
        MutationKind::Modified,
        serialize_sle(*sle)
    });
}

void MutationCollector::rawDestroyXRP(XRPAmount const& fee) {
    drops_destroyed_ += fee.drops();
}

void MutationCollector::rawTxInsert(
    ReadView::key_type const& key,
    std::shared_ptr<Serializer const> const& txn,
    std::shared_ptr<Serializer const> const& metaData) {
    TxMutation tm;
    tm.tx_hash = key;
    if (txn) {
        tm.tx_bytes.resize(txn->getDataLength());
        std::memcpy(tm.tx_bytes.data(), txn->data(), txn->getDataLength());
    }
    if (metaData) {
        tm.meta_bytes.resize(metaData->getDataLength());
        std::memcpy(tm.meta_bytes.data(), metaData->data(), metaData->getDataLength());
    }
    tx_mutations_.push_back(std::move(tm));
}

}  // namespace ripple
