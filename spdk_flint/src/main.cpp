#include "app.hpp"
#include "logging.hpp"
#include <iostream>
#include <csignal>
#include <memory>

using namespace spdk_flint;

std::unique_ptr<Application> g_app;

void signalHandler(int signal) {
    LOG_INFO("Received signal {}, shutting down gracefully", signal);
    if (g_app) {
        g_app->shutdown();
    }
}

void printVersion() {
    std::cout << "SPDK Flint CSI Driver v1.0.0\n";
    std::cout << "Built with C++20 and direct SPDK integration\n";
    std::cout << "Supports modes: csi-driver, controller, dashboard-backend, node-agent\n";
}

void printUsage() {
    std::cout << "Usage: spdk_flint [options]\n\n";
    std::cout << "Options:\n";
    std::cout << "  --help, -h              Show this help message\n";
    std::cout << "  --version, -v           Show version information\n";
    std::cout << "  --mode <mode>           Operating mode (csi-driver|controller|dashboard-backend|node-agent|all)\n";
    std::cout << "  --log-level <level>     Log level (trace|debug|info|warn|error|critical)\n";
    std::cout << "  --config <file>         Configuration file path\n\n";
    std::cout << "Environment Variables:\n";
    std::cout << "  CSI_MODE                Operating mode (default: all)\n";
    std::cout << "  CSI_ENDPOINT            CSI socket endpoint (default: unix:///csi/csi.sock)\n";
    std::cout << "  NODE_ID                 Node identifier\n";
    std::cout << "  SPDK_RPC_URL           SPDK RPC endpoint (default: unix:///var/tmp/spdk.sock)\n";
    std::cout << "  TARGET_NAMESPACE        Kubernetes namespace for custom resources\n";
    std::cout << "  NVMEOF_TRANSPORT        NVMe-oF transport (tcp|rdma|fc, default: tcp)\n";
    std::cout << "  NVMEOF_TARGET_PORT      NVMe-oF target port (default: 4420)\n";
    std::cout << "  DASHBOARD_PORT          Dashboard HTTP port (default: 8080)\n";
    std::cout << "  HEALTH_PORT             Health check port (default: 9809)\n";
    std::cout << "  NODE_AGENT_PORT         Node agent HTTP port (default: 8090)\n";
    std::cout << "  LOG_LEVEL               Log level (default: info)\n";
    std::cout << "  AUTO_INITIALIZE_BLOBSTORE  Auto-initialize blobstore (default: true)\n";
    std::cout << "  DISCOVERY_INTERVAL      Disk discovery interval in seconds (default: 300)\n";
    std::cout << "  BACKUP_PATH             Disk backup path (default: /var/lib/spdk-csi/backups)\n";
}

int main(int argc, char** argv) {
    try {
        // Parse command line arguments
        std::string mode;
        std::string log_level;
        std::string config_file;
        
        for (int i = 1; i < argc; i++) {
            std::string arg = argv[i];
            
            if (arg == "--help" || arg == "-h") {
                printUsage();
                return 0;
            } else if (arg == "--version" || arg == "-v") {
                printVersion();
                return 0;
            } else if (arg == "--mode" && i + 1 < argc) {
                mode = argv[++i];
            } else if (arg == "--log-level" && i + 1 < argc) {
                log_level = argv[++i];
            } else if (arg == "--config" && i + 1 < argc) {
                config_file = argv[++i];
            } else {
                std::cerr << "Unknown argument: " << arg << "\n";
                printUsage();
                return 1;
            }
        }
        
        // Load configuration from environment
        AppConfig config = loadConfigFromEnvironment();
        
        // Override with command line arguments
        if (!mode.empty()) {
            if (mode == "csi-driver") config.mode = AppMode::CSI_DRIVER;
            else if (mode == "controller") config.mode = AppMode::CONTROLLER;
            else if (mode == "dashboard-backend") config.mode = AppMode::DASHBOARD_BACKEND;
            else if (mode == "node-agent") config.mode = AppMode::NODE_AGENT;
            else if (mode == "all") config.mode = AppMode::ALL;
            else {
                std::cerr << "Invalid mode: " << mode << "\n";
                return 1;
            }
        }
        
        if (!log_level.empty()) {
            config.log_level = log_level;
        }
        
        // Initialize logging first
        Logger::initialize(config.log_level);
        
        LOG_INFO("Starting SPDK Flint CSI Driver v1.0.0");
        LOG_INFO("Mode: {}", mode.empty() ? "all" : mode);
        LOG_INFO("Node ID: {}", config.node_id);
        LOG_INFO("Log level: {}", config.log_level);
        
        // Setup signal handlers
        std::signal(SIGINT, signalHandler);
        std::signal(SIGTERM, signalHandler);
        
        // Create and run the application
        g_app = std::make_unique<Application>(config);
        int result = g_app->run();
        
        LOG_INFO("Application shutdown complete");
        Logger::shutdown();
        
        return result;
        
    } catch (const std::exception& e) {
        std::cerr << "Fatal error: " << e.what() << std::endl;
        if (g_app) {
            g_app->shutdown();
        }
        Logger::shutdown();
        return 1;
    }
} 