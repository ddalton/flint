#pragma once

#include <memory>
#include <string>
#include <vector>
#include <map>
#include <atomic>
#include "logging.hpp"

namespace spdk_flint {

enum class AppMode {
    CSI_DRIVER,
    CONTROLLER,
    DASHBOARD_BACKEND,
    NODE_AGENT,
    ALL
};

struct AppConfig {
    AppMode mode = AppMode::ALL;
    std::string node_id;
    std::string csi_endpoint = "unix:///csi/csi.sock";
    std::string spdk_rpc_url = "unix:///var/tmp/spdk.sock";
    std::string target_namespace;
    std::string nvmeof_transport = "tcp";
    int nvmeof_target_port = 4420;
    int dashboard_port = 8080;
    int health_port = 9809;
    int node_agent_port = 8090;
    std::string log_level = "info";
    bool auto_initialize_blobstore = true;
    int discovery_interval = 300;
    std::string backup_path = "/var/lib/spdk-csi/backups";
    std::map<std::string, std::string> spdk_node_urls;
};

// Forward declarations
class CSIService;
class DashboardService;
class NodeAgent;
class ControllerOperator;
class SpdkWrapper;

class Application {
public:
    explicit Application(const AppConfig& config);
    ~Application();

    // Initialize application components
    bool initialize();

    // Main entry point
    int run();

    // Graceful shutdown
    void shutdown();

    // Get configuration
    const AppConfig& config() const { return config_; }

    // Get SPDK wrapper
    std::shared_ptr<SpdkWrapper> spdk() const { return spdk_wrapper_; }

private:
    AppConfig config_;
    std::atomic<bool> running_{true};
    
    // Core components
    std::shared_ptr<SpdkWrapper> spdk_wrapper_;
    std::unique_ptr<CSIService> csi_service_;
    std::unique_ptr<DashboardService> dashboard_service_;
    std::unique_ptr<NodeAgent> node_agent_;
    std::unique_ptr<ControllerOperator> controller_operator_;

    // Initialization methods
    void setupLogging();
    void initializeComponents();
    
    // Mode-specific startup
    void startCSIMode();
    void startControllerMode();
    void startDashboardMode();
    void startNodeAgentMode();
    void startHealthServer();
    
    // Helper methods
    AppMode parseMode(const std::string& mode_str) const;
    std::string getCurrentNamespace() const;
    void waitForSpdkReady();
};

// Utility functions
AppConfig loadConfigFromEnvironment();
void printVersion();
void printUsage();

} // namespace spdk_flint 