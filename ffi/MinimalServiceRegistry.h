// MinimalServiceRegistry.h — minimum ServiceRegistry needed to call preflight()/apply()
//
// Implements only the methods that the Payment tx code path actually touches.
// All other methods throw std::runtime_error when called.
//
// Real implementations:
//   getNetworkIDService() — fixed network ID (0 = mainnet)
//   getHashRouter()       — constructed with default Setup + stopwatch()
//   getFeeTrack()         — LoadFeeTrack with null journal
//   getJournal()          — null sink
//   getTrapTxID()         — always returns empty optional
//   isStopping()          — always false

#pragma once

#include <xrpl/core/HashRouter.h>
#include <xrpl/core/NetworkIDService.h>
#include <xrpl/core/ServiceRegistry.h>
#include <xrpl/server/LoadFeeTrack.h>
#include <xrpl/beast/utility/Journal.h>

#include <cstdint>
#include <memory>
#include <optional>

namespace xrpl {

/** Fixed-network-ID implementation. */
class FixedNetworkIDService final : public NetworkIDService
{
public:
    explicit FixedNetworkIDService(std::uint32_t id) : id_(id) {}
    std::uint32_t getNetworkID() const noexcept override { return id_; }
private:
    std::uint32_t id_;
};

/** Minimum viable ServiceRegistry for preflight/apply of Payment XRP. */
class MinimalServiceRegistry : public ServiceRegistry
{
public:
    explicit MinimalServiceRegistry(std::uint32_t networkID = 0);
    ~MinimalServiceRegistry() override = default;

    // === REAL implementations (called by preflight/apply path) ===

    NetworkIDService& getNetworkIDService() override { return networkIDService_; }
    HashRouter& getHashRouter() override { return *hashRouter_; }
    LoadFeeTrack& getFeeTrack() override { return *feeTrack_; }
    beast::Journal getJournal(std::string const& /*name*/) override { return nullJournal_; }
    std::optional<uint256> const& getTrapTxID() const override { return trapTxID_; }
    bool isStopping() const override { return false; }

    // === STUBS (throw if called — not on the Payment preflight path) ===
    // Defined out-of-line to keep header small.
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
    OrderBookDB& getOrderBookDB() override;
    TransactionMaster& getMasterTransaction() override;
    TxQ& getTxQ() override;
    PathRequestManager& getPathRequestManager() override;
    ServerHandler& getServerHandler() override;
    perf::PerfLog& getPerfLog() override;
    boost::asio::io_context& getIOContext() override;
    Logs& getLogs() override;
    DatabaseCon& getWalletDB() override;
    Application& getApp() override;

private:
    FixedNetworkIDService networkIDService_;
    std::unique_ptr<HashRouter> hashRouter_;
    std::unique_ptr<LoadFeeTrack> feeTrack_;
    beast::Journal nullJournal_;
    std::optional<uint256> trapTxID_;  // always empty
};

}  // namespace xrpl
