// MinimalApp.h — minimum xrpl::Application needed to call apply()/preflight().
//
// rippled 3.2.0 consolidated the service accessors into a ServiceRegistry base
// (46 pure virtuals) and slimmed Application proper to 16. MinimalApp implements
// all of them: config() is real (carries the network id used by rules/fees); a few
// infra accessors are made safe so apply-path logging/lifecycle calls don't throw
// (getJournal -> null journal, isStopping -> false, getApp -> *this, getTrapTxID ->
// empty); everything else is a throw-stub (unreached by the single-tx apply path —
// if any IS reached, the shadow hash-match window surfaces it loudly and we make it
// real then, the way the <=3.1.x shim made getHashRouter/getFeeTrack/getOrderBookDB
// real).

#pragma once

#include <xrpld/app/main/Application.h>
#include <xrpl/core/HashRouter.h>
#include <xrpl/server/LoadFeeTrack.h>
#include <xrpl/ledger/OrderBookDB.h>
#include <xrpld/core/Config.h>
#include <xrpl/core/NetworkIDService.h>
#include <xrpld/core/NetworkIDServiceImpl.h>
#include <xrpl/basics/Log.h>
#include <xrpl/basics/chrono.h>
#include <xrpl/beast/utility/Journal.h>

#include <chrono>
#include <cstddef>
#include <cstdint>
#include <memory>
#include <mutex>
#include <optional>
#include <string>
#include <utility>

namespace xrpl {

class MinimalApp : public Application
{
public:
    explicit MinimalApp(std::uint32_t networkID = 0);
    ~MinimalApp() override = default;

    // ===== Application (16 pure virtuals) =====
    Config& config() override { return *config_; }
    MutexType& getMasterMutex() override { return masterMutex_; }
    bool checkSigs() const override { return true; }
    void checkSigs(bool) override {}
    std::size_t getNumberOfThreads() const override { return 1; }
    bool setup(boost::program_options::variables_map const&) override;
    void start(bool) override;
    void run() override;
    void signalStop(std::string) override;
    std::uint64_t instanceID() const override;
    std::pair<PublicKey, SecretKey> const& nodeIdentity() override;
    std::optional<PublicKey const> getValidationPublicKey() const override;
    std::chrono::milliseconds getIOLatency() override;
    bool serverOkay(std::string&) override;
    int fdRequired() const override;
    LedgerIndex getMaxDisallowedLedger() override;

    // ===== ServiceRegistry — infra kept safe (apply path may touch) =====
    bool isStopping() const override { return false; }
    Application& getApp() override { return *this; }
    beast::Journal getJournal(std::string const&) override { return beast::Journal{beast::Journal::getNullSink()}; }
    std::optional<uint256> const& getTrapTxID() const override { return trapTxID_; }
    // Real: 3.2.0 preflight routes the tx NetworkID check through this service.
    NetworkIDService& getNetworkIDService() override { return *networkIDService_; }
    // Real: the apply()/preflight() chain reaches these (matches the <=3.1.x shim).
    HashRouter& getHashRouter() override { return *hashRouter_; }
    LoadFeeTrack& getFeeTrack() override { return *feeTrack_; }
    OrderBookDB& getOrderBookDB() override { return *orderBookDB_; }
    Logs& getLogs() override { return *logs_; }

    // ===== ServiceRegistry — throw-stubs =====
    CollectorManager& getCollectorManager() override;
    Family& getNodeFamily() override;
    TimeKeeper& getTimeKeeper() override;
    JobQueue& getJobQueue() override;
    NodeCache& getTempNodeCache() override;
    CachedSLEs& getCachedSLEs() override;
    AmendmentTable& getAmendmentTable() override;
    LoadManager& getLoadManager() override;
    RCLValidations& getValidations() override;
    ValidatorList& getValidators() override;
    ValidatorSite& getValidatorSites() override;
    ManifestCache& getValidatorManifests() override;
    ManifestCache& getPublisherManifests() override;
    Overlay& getOverlay() override;
    Cluster& getCluster() override;
    PeerReservationTable& getPeerReservations() override;
    Resource::Manager& getResourceManager() override;
    NodeStore::Database& getNodeStore() override;
    SHAMapStore& getSHAMapStore() override;
    RelationalDatabase& getRelationalDatabase() override;
    InboundLedgers& getInboundLedgers() override;
    InboundTransactions& getInboundTransactions() override;
    TaggedCache<uint256, AcceptedLedger>& getAcceptedLedgerCache() override;
    LedgerMaster& getLedgerMaster() override;
    LedgerCleaner& getLedgerCleaner() override;
    LedgerReplayer& getLedgerReplayer() override;
    PendingSaves& getPendingSaves() override;
    OpenLedger& getOpenLedger() override;
    OpenLedger const& getOpenLedger() const override;
    NetworkOPs& getOPs() override;
    TransactionMaster& getMasterTransaction() override;
    TxQ& getTxQ() override;
    PathRequestManager& getPathRequestManager() override;
    ServerHandler& getServerHandler() override;
    perf::PerfLog& getPerfLog() override;
    boost::asio::io_context& getIOContext() override;
    DatabaseCon& getWalletDB() override;

private:
    std::unique_ptr<Config> config_;
    std::unique_ptr<Logs> logs_;
    beast::Journal nullJournal_;
    std::unique_ptr<NetworkIDService> networkIDService_;
    std::unique_ptr<HashRouter> hashRouter_;
    std::unique_ptr<LoadFeeTrack> feeTrack_;
    std::unique_ptr<OrderBookDB> orderBookDB_;
    MutexType masterMutex_;
    std::optional<uint256> trapTxID_;
};

}  // namespace xrpl
