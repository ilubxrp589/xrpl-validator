// MinimalApp.h — minimum ripple::Application needed to call apply()/preflight().
//
// Built against rippled 3.1.2 stable (Application.h). Implements only the
// methods the tx apply path actually touches. Everything else throws.
//
// Real implementations (called by the apply chain in Transactor.cpp / Payment.cpp):
//   config()           — Config with NETWORK_ID=0 (mainnet)
//   logs()             — default Logs at kFatal threshold
//   getHashRouter()    — setup_HashRouter(config) backed HashRouter
//   getFeeTrack()      — LoadFeeTrack with null journal
//   getOrderBookDB()   — NoopOrderBookDB (all methods no-op / empty)
//   journal(name)      — null-sink beast::Journal regardless of name
//   trapTxID()         — always returns empty optional
//
// Stubbed (throws std::runtime_error if called):
//   Everything else on ripple::Application.
//
// The getAmendmentTable() and getOPs() stubs are only reachable via the Change
// transaction handler (pseudo-transactions — EnableAmendment / SetFee /
// UNLModify). Our Rust apply_ledger_in_order filters pseudo-txs out before
// calling into libxrpl, so those code paths are unreachable in practice.

#pragma once

#include <xrpld/app/main/Application.h>
#include <xrpld/app/misc/HashRouter.h>
#include <xrpld/app/misc/LoadFeeTrack.h>
#include <xrpld/app/ledger/OrderBookDB.h>
#include <xrpld/core/Config.h>
#include <xrpl/basics/Log.h>
#include <xrpl/basics/chrono.h>
#include <xrpl/beast/utility/Journal.h>

#include <cstdint>
#include <memory>
#include <mutex>
#include <optional>

namespace ripple {

/** Minimum viable Application for running ripple::apply() in a standalone
 *  replay context. Owns real Config / Logs / HashRouter / LoadFeeTrack
 *  instances, stubs everything else. OrderBookDB is used directly (concrete
 *  class in 3.1.2, no virtuals) — never calling setup() keeps it empty. */
class MinimalApp : public Application
{
public:
    explicit MinimalApp(std::uint32_t networkID = 0);
    ~MinimalApp() override = default;

    // ============================================================
    // REAL implementations (exercised by apply / preflight / doApply)
    // ============================================================

    Config& config() override { return *config_; }
    Logs& logs() override { return *logs_; }
    HashRouter& getHashRouter() override { return *hashRouter_; }
    LoadFeeTrack& getFeeTrack() override { return *feeTrack_; }
    OrderBookDB& getOrderBookDB() override { return *orderBookDB_; }
    beast::Journal journal(std::string const& /*name*/) override { return nullJournal_; }
    std::optional<uint256> const& trapTxID() const override { return trapTxID_; }
    bool isStopping() const override { return false; }
    bool checkSigs() const override { return true; }
    void checkSigs(bool) override {}
    std::recursive_mutex& getMasterMutex() override { return masterMutex_; }

    // ============================================================
    // STUBS — throw if called.  Defined out-of-line in MinimalApp.cpp
    // to keep this header light.
    // ============================================================

    bool setup(boost::program_options::variables_map const&) override;
    void start(bool) override;
    void run() override;
    void signalStop(std::string) override;
    std::uint64_t instanceID() const override;

    boost::asio::io_service& getIOService() override;
    CollectorManager& getCollectorManager() override;
    Family& getNodeFamily() override;
    TimeKeeper& timeKeeper() override;
    JobQueue& getJobQueue() override;
    NodeCache& getTempNodeCache() override;
    CachedSLEs& cachedSLEs() override;
    AmendmentTable& getAmendmentTable() override;
    LoadManager& getLoadManager() override;
    Overlay& overlay() override;
    TxQ& getTxQ() override;
    ValidatorList& validators() override;
    ValidatorSite& validatorSites() override;
    ManifestCache& validatorManifests() override;
    ManifestCache& publisherManifests() override;
    Cluster& cluster() override;
    PeerReservationTable& peerReservations() override;
    RCLValidations& getValidations() override;
    NodeStore::Database& getNodeStore() override;
    InboundLedgers& getInboundLedgers() override;
    InboundTransactions& getInboundTransactions() override;
    TaggedCache<uint256, AcceptedLedger>& getAcceptedLedgerCache() override;
    LedgerMaster& getLedgerMaster() override;
    LedgerCleaner& getLedgerCleaner() override;
    LedgerReplayer& getLedgerReplayer() override;
    NetworkOPs& getOPs() override;
    ServerHandler& getServerHandler() override;
    TransactionMaster& getMasterTransaction() override;
    perf::PerfLog& getPerfLog() override;
    std::pair<PublicKey, SecretKey> const& nodeIdentity() override;
    std::optional<PublicKey const> getValidationPublicKey() const override;
    Resource::Manager& getResourceManager() override;
    PathRequests& getPathRequests() override;
    SHAMapStore& getSHAMapStore() override;
    PendingSaves& pendingSaves() override;
    OpenLedger& openLedger() override;
    OpenLedger const& openLedger() const override;
    RelationalDatabase& getRelationalDatabase() override;
    std::chrono::milliseconds getIOLatency() override;
    bool serverOkay(std::string&) override;
    int fdRequired() const override;
    DatabaseCon& getWalletDB() override;
    LedgerIndex getMaxDisallowedLedger() override;

private:
    std::unique_ptr<Config> config_;
    std::unique_ptr<Logs> logs_;
    std::unique_ptr<HashRouter> hashRouter_;
    std::unique_ptr<LoadFeeTrack> feeTrack_;
    std::unique_ptr<OrderBookDB> orderBookDB_;
    beast::Journal nullJournal_;
    std::optional<uint256> trapTxID_;  // always nullopt
    std::recursive_mutex masterMutex_;
};

}  // namespace ripple
