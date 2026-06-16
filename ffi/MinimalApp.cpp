// MinimalApp.cpp — minimal xrpl::Application for the apply()/preflight() path (rippled 3.2.0).

#include "MinimalApp.h"

#include <stdexcept>

namespace xrpl {

// Application's constructor is defined in src/xrpld/app/main/Application.cpp, which
// the shim does not compile; provide it so MinimalApp's base links. Matches the real
// definition (ServiceRegistry, the other base, is default-constructed).
Application::Application() : beast::PropertyStream::Source("app") {}

MinimalApp::MinimalApp(std::uint32_t networkID)
    : config_(std::make_unique<Config>())
{
    // networkId drives rules / fee calculation in the apply path.
    config_->networkId = networkID;
}

bool MinimalApp::setup(boost::program_options::variables_map const&)
{ throw std::runtime_error("MinimalApp::setup — not implemented"); }
void MinimalApp::start(bool)
{ throw std::runtime_error("MinimalApp::start — not implemented"); }
void MinimalApp::run()
{ throw std::runtime_error("MinimalApp::run — not implemented"); }
void MinimalApp::signalStop(std::string)
{ throw std::runtime_error("MinimalApp::signalStop — not implemented"); }
std::uint64_t MinimalApp::instanceID() const
{ throw std::runtime_error("MinimalApp::instanceID — not implemented"); }
std::pair<PublicKey, SecretKey> const& MinimalApp::nodeIdentity()
{ throw std::runtime_error("MinimalApp::nodeIdentity — not implemented"); }
std::optional<PublicKey const> MinimalApp::getValidationPublicKey() const
{ throw std::runtime_error("MinimalApp::getValidationPublicKey — not implemented"); }
std::chrono::milliseconds MinimalApp::getIOLatency()
{ throw std::runtime_error("MinimalApp::getIOLatency — not implemented"); }
bool MinimalApp::serverOkay(std::string&)
{ throw std::runtime_error("MinimalApp::serverOkay — not implemented"); }
int MinimalApp::fdRequired() const
{ throw std::runtime_error("MinimalApp::fdRequired — not implemented"); }
LedgerIndex MinimalApp::getMaxDisallowedLedger()
{ throw std::runtime_error("MinimalApp::getMaxDisallowedLedger — not implemented"); }

}  // namespace xrpl
