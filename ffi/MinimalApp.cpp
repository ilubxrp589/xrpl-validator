// MinimalApp.cpp — minimal xrpl::Application for the apply()/preflight() path (rippled 3.2.0).

#include "MinimalApp.h"

#include <stdexcept>

namespace xrpl {

// Application's ctor is defined in src/xrpld/app/main/Application.cpp, which the shim
// does not compile; provide it so MinimalApp's base links. Matches the real def
// (ServiceRegistry — the other base — is default-constructed).
Application::Application() : beast::PropertyStream::Source("app") {}

MinimalApp::MinimalApp(std::uint32_t networkID)
    : config_(std::make_unique<Config>())
{
    config_->networkId = networkID;  // drives rules / fee calc in the apply path
}

// --- Application throw-stubs ---
bool MinimalApp::setup(boost::program_options::variables_map const&) { throw std::runtime_error("MinimalApp::setup — not implemented"); }
void MinimalApp::start(bool) { throw std::runtime_error("MinimalApp::start — not implemented"); }
void MinimalApp::run() { throw std::runtime_error("MinimalApp::run — not implemented"); }
void MinimalApp::signalStop(std::string) { throw std::runtime_error("MinimalApp::signalStop — not implemented"); }
std::uint64_t MinimalApp::instanceID() const { throw std::runtime_error("MinimalApp::instanceID — not implemented"); }
std::pair<PublicKey, SecretKey> const& MinimalApp::nodeIdentity() { throw std::runtime_error("MinimalApp::nodeIdentity — not implemented"); }
std::optional<PublicKey const> MinimalApp::getValidationPublicKey() const { throw std::runtime_error("MinimalApp::getValidationPublicKey — not implemented"); }
std::chrono::milliseconds MinimalApp::getIOLatency() { throw std::runtime_error("MinimalApp::getIOLatency — not implemented"); }
bool MinimalApp::serverOkay(std::string&) { throw std::runtime_error("MinimalApp::serverOkay — not implemented"); }
int MinimalApp::fdRequired() const { throw std::runtime_error("MinimalApp::fdRequired — not implemented"); }
LedgerIndex MinimalApp::getMaxDisallowedLedger() { throw std::runtime_error("MinimalApp::getMaxDisallowedLedger — not implemented"); }

// --- ServiceRegistry throw-stubs (not used by single-tx apply; loud if reached) ---
#define STUB(RET, NAME) RET MinimalApp::NAME() { throw std::runtime_error("MinimalApp::" #NAME " — not implemented"); }
STUB(CollectorManager&, getCollectorManager)
STUB(Family&, getNodeFamily)
STUB(TimeKeeper&, getTimeKeeper)
STUB(JobQueue&, getJobQueue)
STUB(NodeCache&, getTempNodeCache)
STUB(CachedSLEs&, getCachedSLEs)
STUB(NetworkIDService&, getNetworkIDService)
STUB(AmendmentTable&, getAmendmentTable)
STUB(HashRouter&, getHashRouter)
STUB(LoadFeeTrack&, getFeeTrack)
STUB(LoadManager&, getLoadManager)
STUB(RCLValidations&, getValidations)
STUB(ValidatorList&, getValidators)
STUB(ValidatorSite&, getValidatorSites)
STUB(ManifestCache&, getValidatorManifests)
STUB(ManifestCache&, getPublisherManifests)
STUB(Overlay&, getOverlay)
STUB(Cluster&, getCluster)
STUB(PeerReservationTable&, getPeerReservations)
STUB(Resource::Manager&, getResourceManager)
STUB(NodeStore::Database&, getNodeStore)
STUB(SHAMapStore&, getSHAMapStore)
STUB(RelationalDatabase&, getRelationalDatabase)
STUB(InboundLedgers&, getInboundLedgers)
STUB(InboundTransactions&, getInboundTransactions)
STUB(LedgerMaster&, getLedgerMaster)
STUB(LedgerCleaner&, getLedgerCleaner)
STUB(LedgerReplayer&, getLedgerReplayer)
STUB(PendingSaves&, getPendingSaves)
STUB(OpenLedger&, getOpenLedger)
STUB(NetworkOPs&, getOPs)
STUB(OrderBookDB&, getOrderBookDB)
STUB(TransactionMaster&, getMasterTransaction)
STUB(TxQ&, getTxQ)
STUB(PathRequestManager&, getPathRequestManager)
STUB(ServerHandler&, getServerHandler)
STUB(perf::PerfLog&, getPerfLog)
STUB(Logs&, getLogs)
STUB(boost::asio::io_context&, getIOContext)
STUB(DatabaseCon&, getWalletDB)
#undef STUB

// Long-form stubs the macro can't express (const overload; comma in template type):
OpenLedger const& MinimalApp::getOpenLedger() const { throw std::runtime_error("MinimalApp::getOpenLedger (const) — not implemented"); }
TaggedCache<uint256, AcceptedLedger>& MinimalApp::getAcceptedLedgerCache() { throw std::runtime_error("MinimalApp::getAcceptedLedgerCache — not implemented"); }

}  // namespace xrpl
