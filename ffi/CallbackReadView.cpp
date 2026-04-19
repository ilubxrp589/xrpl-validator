// CallbackReadView.cpp — implementations

#include "CallbackReadView.h"

#include <xrpl/protocol/STLedgerEntry.h>
#include <xrpl/protocol/Serializer.h>

#include <stdexcept>

namespace ripple {

CallbackReadView::CallbackReadView(
    LedgerHeader const& header,
    Rules const& rules,
    Fees const& fees,
    bool open,
    SleLookupCallback lookup,
    SleSuccCallback succ)
    : header_(header)
    , rules_(rules)
    , fees_(fees)
    , open_(open)
    , lookup_(std::move(lookup))
    , succ_(std::move(succ))
{
}

bool
CallbackReadView::exists(Keylet const& k) const
{
    uint8_t const* data = nullptr;
    size_t len = 0;
    return lookup_(k.key, &data, &len) && len > 0;
}

std::shared_ptr<SLE const>
CallbackReadView::read(Keylet const& k) const
{
    uint8_t const* data = nullptr;
    size_t len = 0;
    if (!lookup_(k.key, &data, &len) || len == 0)
        return nullptr;

    try {
        SerialIter sit(data, len);
        auto sle = std::make_shared<STLedgerEntry>(sit, k.key);
        // Validate keylet type matches
        if (k.type != ltANY && sle->getType() != k.type)
            return nullptr;
        return sle;
    } catch (std::exception const&) {
        return nullptr;
    }
}

std::optional<ReadView::key_type>
CallbackReadView::succ(
    key_type const& key,
    std::optional<key_type> const& last) const
{
    if (!succ_)
        return std::nullopt;

    uint256 out;
    bool found = succ_(key, last ? &*last : nullptr, out);
    if (found)
        return out;
    return std::nullopt;
}

bool
CallbackReadView::txExists(key_type const& /*key*/) const
{
    return false;
}

ReadView::tx_type
CallbackReadView::txRead(key_type const& /*key*/) const
{
    throw std::runtime_error("CallbackReadView::txRead — not implemented");
}

std::unique_ptr<ReadView::sles_type::iter_base>
CallbackReadView::slesBegin() const
{
    throw std::runtime_error("CallbackReadView::slesBegin — not implemented");
}

std::unique_ptr<ReadView::sles_type::iter_base>
CallbackReadView::slesEnd() const
{
    throw std::runtime_error("CallbackReadView::slesEnd — not implemented");
}

std::unique_ptr<ReadView::sles_type::iter_base>
CallbackReadView::slesUpperBound(key_type const& /*key*/) const
{
    throw std::runtime_error("CallbackReadView::slesUpperBound — not implemented");
}

std::unique_ptr<ReadView::txs_type::iter_base>
CallbackReadView::txsBegin() const
{
    throw std::runtime_error("CallbackReadView::txsBegin — not implemented");
}

std::unique_ptr<ReadView::txs_type::iter_base>
CallbackReadView::txsEnd() const
{
    throw std::runtime_error("CallbackReadView::txsEnd — not implemented");
}

}  // namespace ripple
