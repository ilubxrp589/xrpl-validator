// MinimalApp.cpp — minimal xrpl::Application for the apply()/preflight() path (rippled 3.2.0).

#include "MinimalApp.h"

#include <stdexcept>

namespace xrpl {

// Application's ctor is defined in src/xrpld/app/main/Application.cpp, which the shim
// does not compile; provide it so MinimalApp's base links. Matches the real def
// (ServiceRegistry — the other base — is default-constructed).
Application::Application() : beast::PropertyStream::Source("app") {}

// 3.2.0 made OrderBookDB a pure-virtual interface (the concrete OrderBookDBImpl
// needs a full ServiceRegistry we don't have). Single-tx apply crosses offers
// against the ledger View's book-directory SLEs, not this in-memory cache, so a
// no-op impl is hash-safe; we only need getOrderBookDB() to return a valid ref.
class NoopOrderBookDB : public OrderBookDB
{
public:
    void setup(std::shared_ptr<ReadView const> const&) override {}
    void addOrderBook(Book const&) override {}
    std::vector<Book> getBooksByTakerPays(Asset const&, std::optional<Domain> const&) override { return {}; }
    int getBookSize(Asset const&, std::optional<Domain> const&) override { return 0; }
    bool isBookToXRP(Asset const&, std::optional<Domain> const&) override { return false; }
};

MinimalApp::MinimalApp(std::uint32_t networkID)
    : config_(std::make_unique<Config>())
    , logs_(std::make_unique<Logs>(beast::Severity::Fatal))
    , nullJournal_(beast::Journal::getNullSink())
{
    config_->networkId = networkID;  // drives rules / fee calc in the apply path
    networkIDService_ = std::make_unique<NetworkIDServiceImpl>(config_->networkId);

    // Real services the apply()/preflight() chain reaches (matches the <=3.1.x shim):
    //   HashRouter   — dedup/relay TTLs (default Setup; doesn't affect apply correctness)
    //   LoadFeeTrack — fee escalation (null journal; default => base-fee scaling)
    //   OrderBookDB  — offer crossing
    // 3.2.0's setup_HashRouter lives in xrpld (the daemon), not libxrpl. A default
    // Setup is fine here: its fields are only dedup/relay TTLs, irrelevant to the
    // deterministic result of a single-tx apply.
    HashRouter::Setup const hrSetup{};
    hashRouter_ = std::make_unique<HashRouter>(hrSetup, stopwatch());
    feeTrack_ = std::make_unique<LoadFeeTrack>(nullJournal_);
    orderBookDB_ = std::make_unique<NoopOrderBookDB>();
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
STUB(AmendmentTable&, getAmendmentTable)
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
STUB(TransactionMaster&, getMasterTransaction)
STUB(TxQ&, getTxQ)
STUB(PathRequestManager&, getPathRequestManager)
STUB(ServerHandler&, getServerHandler)
STUB(perf::PerfLog&, getPerfLog)
STUB(boost::asio::io_context&, getIOContext)
STUB(DatabaseCon&, getWalletDB)
#undef STUB

// Long-form stubs the macro can't express (const overload; comma in template type):
OpenLedger const& MinimalApp::getOpenLedger() const { throw std::runtime_error("MinimalApp::getOpenLedger (const) — not implemented"); }
TaggedCache<uint256, AcceptedLedger>& MinimalApp::getAcceptedLedgerCache() { throw std::runtime_error("MinimalApp::getAcceptedLedgerCache — not implemented"); }

}  // namespace xrpl
