// MinimalApp.h — minimum xrpl::Application needed to call apply()/preflight().
//
// rippled 3.2.0 slimmed Application to 16 pure virtuals (mostly lifecycle plus a
// few accessors); the heavy service accessors (overlay / validators / hashRouter /
// ledgerMaster / openLedger / timeKeeper / logs / journal / ...) are no longer part
// of the interface. We implement config() for real (it carries the network id used
// by rules/fees) and the trivial getMasterMutex()/checkSigs()/getNumberOfThreads();
// everything else is a stub that throws (unreached by the single-tx apply path).

#pragma once

#include <xrpld/app/main/Application.h>
#include <xrpld/core/Config.h>

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

    // === Real implementations ===
    Config& config() override { return *config_; }
    MutexType& getMasterMutex() override { return masterMutex_; }
    bool checkSigs() const override { return true; }
    void checkSigs(bool) override {}
    std::size_t getNumberOfThreads() const override { return 1; }

    // === Stubs — throw if reached (not used by single-tx apply/preflight) ===
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

private:
    std::unique_ptr<Config> config_;
    MutexType masterMutex_;
};

}  // namespace xrpl
