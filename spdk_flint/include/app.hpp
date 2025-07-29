#pragma once

#include <string>
#include <memory>
#include <atomic>
#include <thread>

namespace spdk_flint {

// Forward declarations
namespace spdk {
    class SpdkWrapper;
}

namespace kube {
    class KubeClient;
}

class NodeAgentService;

// Application modes - spdk_flint only supports NODE_AGENT
enum class AppMode {
    NODE_AGENT  // Only supported mode - embedded SPDK for node operations
};

// Application configuration
struct AppConfig {
    AppMode mode = AppMode::NODE_AGENT;
    std::string log_level = "info";
    std::string config_file;
    
    // Network configuration
    std::string node_id;
    uint16_t health_port = 9809;
    uint16_t node_agent_port = 8090;
    
    // Kubernetes integration
    std::string target_namespace = "flint-system";
    
    // SPDK configuration (embedded)
    uint32_t discovery_interval = 300; // seconds
    bool auto_initialize_blobstore = true;
    std::string backup_path = "/var/lib/spdk-csi/backups";
};

// Main application class - Node Agent with embedded SPDK only
class Application {
public:
    explicit Application(const AppConfig& config);
    ~Application();

    // Main application lifecycle
    int run();
    bool initialize();
    void shutdown();

private:
    AppConfig config_;
    std::atomic<bool> running_{false};
    
    // Core components
    std::shared_ptr<spdk::SpdkWrapper> spdk_wrapper_;
    std::unique_ptr<NodeAgentService> node_agent_;
    
    // Health server thread
    std::thread health_thread_;
    
    // Internal methods
    void setupLogging();
    void initializeComponents();
    void startNodeAgentMode();
    void startHealthServer();
    void waitForSpdkReady();
};

// Configuration parsing helpers
AppMode parseAppMode(const std::string& mode_str);
AppConfig loadConfigFromEnvironment();

} // namespace spdk_flint 