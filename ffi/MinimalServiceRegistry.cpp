// MinimalServiceRegistry.cpp — stubs + real impls for the Payment apply path

#include "MinimalServiceRegistry.h"

#include <xrpl/basics/chrono.h>

#include <stdexcept>

namespace xrpl {

MinimalServiceRegistry::MinimalServiceRegistry(std::uint32_t networkID)
    : networkIDService_(networkID)
    , hashRouter_(std::make_unique<HashRouter>(HashRouter::Setup{}, stopwatch()))
    , feeTrack_(std::make_unique<LoadFeeTrack>())
    , nullJournal_(beast::Journal::getNullSink())
    , trapTxID_(std::nullopt)
{
}

// === Throwing stubs ===

#define STUB_METHOD(RET, NAME) \
    RET MinimalServiceRegistry::NAME() \
    { \
        throw std::runtime_error("MinimalServiceRegistry::" #NAME " — not implemented"); \
    }

#define STUB_METHOD_CONST(RET, NAME) \
    RET MinimalServiceRegistry::NAME() const \
    { \
        throw std::runtime_error("MinimalServiceRegistry::" #NAME " — not implemented (const)"); \
    }

STUB_METHOD(CollectorManager&, getCollectorManager)
STUB_METHOD(Family&, getNodeFamily)
STUB_METHOD(TimeKeeper&, getTimeKeeper)
STUB_METHOD(JobQueue&, getJobQueue)
STUB_METHOD(NodeCache&, getTempNodeCache)
STUB_METHOD(CachedSLEs&, getCachedSLEs)
STUB_METHOD(AmendmentTable&, getAmendmentTable)
STUB_METHOD(LoadManager&, getLoadManager)
STUB_METHOD(RCLValidations&, getValidations)
STUB_METHOD(ValidatorList&, getValidators)
STUB_METHOD(ValidatorSite&, getValidatorSites)
STUB_METHOD(ManifestCache&, getValidatorManifests)
STUB_METHOD(ManifestCache&, getPublisherManifests)
STUB_METHOD(Overlay&, getOverlay)
STUB_METHOD(Cluster&, getCluster)
STUB_METHOD(PeerReservationTable&, getPeerReservations)
STUB_METHOD(Resource::Manager&, getResourceManager)
STUB_METHOD(NodeStore::Database&, getNodeStore)
STUB_METHOD(SHAMapStore&, getSHAMapStore)
STUB_METHOD(RelationalDatabase&, getRelationalDatabase)
STUB_METHOD(InboundLedgers&, getInboundLedgers)
STUB_METHOD(InboundTransactions&, getInboundTransactions)
STUB_METHOD(LedgerMaster&, getLedgerMaster)
STUB_METHOD(LedgerCleaner&, getLedgerCleaner)
STUB_METHOD(LedgerReplayer&, getLedgerReplayer)
STUB_METHOD(PendingSaves&, getPendingSaves)
STUB_METHOD(OpenLedger&, getOpenLedger)
STUB_METHOD(NetworkOPs&, getOPs)
// Real no-op implementation — below
STUB_METHOD(TransactionMaster&, getMasterTransaction)
STUB_METHOD(TxQ&, getTxQ)
STUB_METHOD(PathRequestManager&, getPathRequestManager)
STUB_METHOD(ServerHandler&, getServerHandler)
STUB_METHOD(perf::PerfLog&, getPerfLog)
STUB_METHOD(boost::asio::io_context&, getIOContext)
STUB_METHOD(Logs&, getLogs)
STUB_METHOD(DatabaseCon&, getWalletDB)
STUB_METHOD(Application&, getApp)

// Const version of getAcceptedLedgerCache — fix the macro issue with template comma
TaggedCache<uint256, AcceptedLedger>& MinimalServiceRegistry::getAcceptedLedgerCache()
{
    throw std::runtime_error("MinimalServiceRegistry::getAcceptedLedgerCache — not implemented");
}

// Const getOpenLedger
OpenLedger const& MinimalServiceRegistry::getOpenLedger() const
{
    throw std::runtime_error("MinimalServiceRegistry::getOpenLedger (const) — not implemented");
}

}  // namespace xrpl
