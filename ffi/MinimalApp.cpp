// MinimalApp.cpp — stubs + real impls for the apply()/preflight() path.

#include "MinimalApp.h"

#include <xrpld/app/misc/HashRouter.h>
#include <xrpld/app/misc/LoadFeeTrack.h>
#include <xrpl/basics/chrono.h>

#include <stdexcept>

namespace ripple {

MinimalApp::MinimalApp(std::uint32_t networkID)
    : config_(std::make_unique<Config>())
    , logs_(std::make_unique<Logs>(beast::severities::kFatal))
    , nullJournal_(beast::Journal::getNullSink())
    , trapTxID_(std::nullopt)
{
    // Network ID drives rules / fee calculation via Transactor.cpp:55.
    config_->NETWORK_ID = networkID;

    // HashRouter needs a Setup from config. setup_HashRouter reads
    // config.section("hashrouter") — a default Config has an empty
    // hashrouter section, so we get default Setup values (fine for
    // stateless replay; these control dedup TTLs which don't affect
    // apply correctness).
    auto const setup = setup_HashRouter(*config_);
    hashRouter_ = std::make_unique<HashRouter>(setup, stopwatch());

    // LoadFeeTrack with null journal — feeTrack is queried by scaleFeeLoad
    // in Transactor.cpp:355, but only to apply network fee escalation.
    // Default values yield base-fee scaling which matches mainnet under
    // normal load.
    feeTrack_ = std::make_unique<LoadFeeTrack>(nullJournal_);

    // NoopOrderBookDB is our no-op implementation; apply doesn't need real
    // book tracking for stateless replay (books are rebuilt from scratch
    // by the new tx's own applied mutations).
    orderBookDB_ = std::make_unique<NoopOrderBookDB>(*this);
}

// ============================================================
// STUBS — every one throws. None of these should be reached by
// the apply() / preflight() chain for regular transaction types.
// getAmendmentTable + getOPs are reached only by Change.cpp
// (pseudo-tx handlers), which our Rust caller filters out.
// ============================================================

#define STUB_METHOD(RET, NAME) \
    RET MinimalApp::NAME() \
    { \
        throw std::runtime_error("MinimalApp::" #NAME " — not implemented"); \
    }

#define STUB_METHOD_ARG(RET, NAME, ARG_T) \
    RET MinimalApp::NAME(ARG_T) \
    { \
        throw std::runtime_error("MinimalApp::" #NAME " — not implemented"); \
    }

#define STUB_METHOD_CONST(RET, NAME) \
    RET MinimalApp::NAME() const \
    { \
        throw std::runtime_error("MinimalApp::" #NAME " (const) — not implemented"); \
    }

bool MinimalApp::setup(boost::program_options::variables_map const&)
{
    throw std::runtime_error("MinimalApp::setup — not implemented");
}
void MinimalApp::start(bool)
{
    throw std::runtime_error("MinimalApp::start — not implemented");
}
void MinimalApp::run()
{
    throw std::runtime_error("MinimalApp::run — not implemented");
}
void MinimalApp::signalStop(std::string)
{
    throw std::runtime_error("MinimalApp::signalStop — not implemented");
}
std::uint64_t MinimalApp::instanceID() const
{
    throw std::runtime_error("MinimalApp::instanceID — not implemented");
}

STUB_METHOD(boost::asio::io_service&, getIOService)
STUB_METHOD(CollectorManager&, getCollectorManager)
STUB_METHOD(Family&, getNodeFamily)
STUB_METHOD(TimeKeeper&, timeKeeper)
STUB_METHOD(JobQueue&, getJobQueue)
STUB_METHOD(NodeCache&, getTempNodeCache)
STUB_METHOD(CachedSLEs&, cachedSLEs)
STUB_METHOD(AmendmentTable&, getAmendmentTable)
STUB_METHOD(LoadManager&, getLoadManager)
STUB_METHOD(Overlay&, overlay)
STUB_METHOD(TxQ&, getTxQ)
STUB_METHOD(ValidatorList&, validators)
STUB_METHOD(ValidatorSite&, validatorSites)
STUB_METHOD(ManifestCache&, validatorManifests)
STUB_METHOD(ManifestCache&, publisherManifests)
STUB_METHOD(Cluster&, cluster)
STUB_METHOD(PeerReservationTable&, peerReservations)
STUB_METHOD(RCLValidations&, getValidations)
STUB_METHOD(NodeStore::Database&, getNodeStore)
STUB_METHOD(InboundLedgers&, getInboundLedgers)
STUB_METHOD(InboundTransactions&, getInboundTransactions)
STUB_METHOD(LedgerMaster&, getLedgerMaster)
STUB_METHOD(LedgerCleaner&, getLedgerCleaner)
STUB_METHOD(LedgerReplayer&, getLedgerReplayer)
STUB_METHOD(NetworkOPs&, getOPs)
STUB_METHOD(ServerHandler&, getServerHandler)
STUB_METHOD(TransactionMaster&, getMasterTransaction)
STUB_METHOD(perf::PerfLog&, getPerfLog)
STUB_METHOD(Resource::Manager&, getResourceManager)
STUB_METHOD(PathRequests&, getPathRequests)
STUB_METHOD(SHAMapStore&, getSHAMapStore)
STUB_METHOD(PendingSaves&, pendingSaves)
STUB_METHOD(OpenLedger&, openLedger)
STUB_METHOD(RelationalDatabase&, getRelationalDatabase)
STUB_METHOD(DatabaseCon&, getWalletDB)
STUB_METHOD(LedgerIndex, getMaxDisallowedLedger)

// getAcceptedLedgerCache — macro can't handle the template-in-type signature
// because of the unprotected comma. Written out long-form.
TaggedCache<uint256, AcceptedLedger>& MinimalApp::getAcceptedLedgerCache()
{
    throw std::runtime_error("MinimalApp::getAcceptedLedgerCache — not implemented");
}

// const-qualified stubs
std::pair<PublicKey, SecretKey> const& MinimalApp::nodeIdentity()
{
    throw std::runtime_error("MinimalApp::nodeIdentity — not implemented");
}
std::optional<PublicKey const> MinimalApp::getValidationPublicKey() const
{
    throw std::runtime_error("MinimalApp::getValidationPublicKey — not implemented");
}
OpenLedger const& MinimalApp::openLedger() const
{
    throw std::runtime_error("MinimalApp::openLedger (const) — not implemented");
}
std::chrono::milliseconds MinimalApp::getIOLatency()
{
    throw std::runtime_error("MinimalApp::getIOLatency — not implemented");
}
bool MinimalApp::serverOkay(std::string&)
{
    throw std::runtime_error("MinimalApp::serverOkay — not implemented");
}
int MinimalApp::fdRequired() const
{
    throw std::runtime_error("MinimalApp::fdRequired — not implemented");
}

}  // namespace ripple
