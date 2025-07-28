#include "app.hpp"
#include "logging.hpp"
#include <cstdlib>
#include <fstream>
#include <sstream>

namespace spdk_flint {

namespace {
    std::string getEnvVar(const char* name, const std::string& defaultValue = "") {
        const char* value = std::getenv(name);
        return value ? std::string(value) : defaultValue;
    }
    
    int getEnvInt(const char* name, int defaultValue) {
        const char* value = std::getenv(name);
        if (!value) return defaultValue;
        
        try {
            return std::stoi(value);
        } catch (const std::exception&) {
            return defaultValue;
        }
    }
    
    bool getEnvBool(const char* name, bool defaultValue) {
        const char* value = std::getenv(name);
        if (!value) return defaultValue;
        
        std::string str = value;
        std::transform(str.begin(), str.end(), str.begin(), ::tolower);
        
        if (str == "true" || str == "1" || str == "yes" || str == "on") {
            return true;
        } else if (str == "false" || str == "0" || str == "no" || str == "off") {
            return false;
        }
        
        return defaultValue;
    }
    
    AppMode parseAppMode(const std::string& mode_str) {
        if (mode_str == "csi-driver") return AppMode::CSI_DRIVER;
        if (mode_str == "controller") return AppMode::CONTROLLER;
        if (mode_str == "dashboard-backend") return AppMode::DASHBOARD_BACKEND;
        if (mode_str == "node-agent") return AppMode::NODE_AGENT;
        if (mode_str == "all") return AppMode::ALL;
        
        // Default to ALL if unknown
        return AppMode::ALL;
    }
    
    std::map<std::string, std::string> parseNodeUrls(const std::string& node_urls_str) {
        std::map<std::string, std::string> result;
        
        if (node_urls_str.empty()) {
            return result;
        }
        
        std::istringstream stream(node_urls_str);
        std::string pair;
        
        while (std::getline(stream, pair, ',')) {
            auto equals_pos = pair.find('=');
            if (equals_pos != std::string::npos) {
                std::string node = pair.substr(0, equals_pos);
                std::string url = pair.substr(equals_pos + 1);
                
                // Trim whitespace
                node.erase(0, node.find_first_not_of(" \t"));
                node.erase(node.find_last_not_of(" \t") + 1);
                url.erase(0, url.find_first_not_of(" \t"));
                url.erase(url.find_last_not_of(" \t") + 1);
                
                result[node] = url;
            }
        }
        
        return result;
    }
    
    std::string getCurrentNamespace() {
        // Try to read from service account token first (Kubernetes)
        std::ifstream namespace_file("/var/run/secrets/kubernetes.io/serviceaccount/namespace");
        if (namespace_file.is_open()) {
            std::string namespace_str;
            std::getline(namespace_file, namespace_str);
            if (!namespace_str.empty()) {
                return namespace_str;
            }
        }
        
        // Fall back to environment variable
        return getEnvVar("TARGET_NAMESPACE", "default");
    }
    
    std::string getNodeId() {
        std::string node_id = getEnvVar("NODE_ID");
        if (node_id.empty()) {
            node_id = getEnvVar("HOSTNAME");
        }
        if (node_id.empty()) {
            node_id = "unknown-node";
        }
        return node_id;
    }
}

AppConfig loadConfigFromEnvironment() {
    AppConfig config;
    
    // Basic application settings
    config.mode = parseAppMode(getEnvVar("CSI_MODE", "all"));
    config.node_id = getNodeId();
    config.log_level = getEnvVar("LOG_LEVEL", "info");
    
    // Network endpoints
    config.csi_endpoint = getEnvVar("CSI_ENDPOINT", "unix:///csi/csi.sock");
    config.spdk_rpc_url = getEnvVar("SPDK_RPC_URL", "unix:///var/tmp/spdk.sock");
    
    // Kubernetes namespace
    config.target_namespace = getCurrentNamespace();
    
    // NVMe-oF settings
    config.nvmeof_transport = getEnvVar("NVMEOF_TRANSPORT", "tcp");
    config.nvmeof_target_port = getEnvInt("NVMEOF_TARGET_PORT", 4420);
    
    // Port configurations
    config.dashboard_port = getEnvInt("DASHBOARD_PORT", 8080);
    config.health_port = getEnvInt("HEALTH_PORT", 9809);
    config.node_agent_port = getEnvInt("NODE_AGENT_PORT", 8090);
    
    // Node agent specific settings
    config.auto_initialize_blobstore = getEnvBool("AUTO_INITIALIZE_BLOBSTORE", true);
    config.discovery_interval = getEnvInt("DISCOVERY_INTERVAL", 300);
    config.backup_path = getEnvVar("BACKUP_PATH", "/var/lib/spdk-csi/backups");
    
    // SPDK node URLs for dashboard
    config.spdk_node_urls = parseNodeUrls(getEnvVar("SPDK_NODE_URLS"));
    
    // Validate transport
    if (config.nvmeof_transport != "tcp" && 
        config.nvmeof_transport != "rdma" && 
        config.nvmeof_transport != "fc") {
        LOG_WARN("Unknown NVMe-oF transport '{}', using 'tcp'", config.nvmeof_transport);
        config.nvmeof_transport = "tcp";
    }
    
    return config;
}

void printVersion() {
    std::cout << "SPDK Flint CSI Driver v1.0.0\n";
    std::cout << "Built with C++20 and direct SPDK integration\n";
    std::cout << "Supports modes: csi-driver, controller, dashboard-backend, node-agent\n";
    std::cout << "Copyright (c) 2024\n";
}

void printUsage() {
    std::cout << "Usage: spdk_flint [options]\n\n";
    std::cout << "A unified SPDK-based CSI driver supporting multiple operational modes.\n\n";
    
    std::cout << "Options:\n";
    std::cout << "  --help, -h              Show this help message\n";
    std::cout << "  --version, -v           Show version information\n";
    std::cout << "  --mode <mode>           Operating mode (csi-driver|controller|dashboard-backend|node-agent|all)\n";
    std::cout << "  --log-level <level>     Log level (trace|debug|info|warn|error|critical)\n";
    std::cout << "  --config <file>         Configuration file path\n\n";
    
    std::cout << "Operational Modes:\n";
    std::cout << "  csi-driver              Run as CSI driver (both controller and node)\n";
    std::cout << "  controller              Run only CSI controller service\n";
    std::cout << "  dashboard-backend       Run dashboard backend API server\n";
    std::cout << "  node-agent              Run node agent for disk management\n";
    std::cout << "  all                     Run all services (default)\n\n";
    
    std::cout << "Environment Variables:\n";
    std::cout << "  CSI_MODE                Operating mode (default: all)\n";
    std::cout << "  CSI_ENDPOINT            CSI socket endpoint (default: unix:///csi/csi.sock)\n";
    std::cout << "  NODE_ID                 Node identifier (default: $HOSTNAME)\n";
    std::cout << "  SPDK_RPC_URL            SPDK RPC endpoint (default: unix:///var/tmp/spdk.sock)\n";
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
    std::cout << "  SPDK_NODE_URLS          Comma-separated node=url pairs for dashboard\n\n";
    
    std::cout << "Examples:\n";
    std::cout << "  spdk_flint --mode controller --log-level debug\n";
    std::cout << "  CSI_MODE=dashboard-backend DASHBOARD_PORT=9090 spdk_flint\n";
    std::cout << "  spdk_flint --mode node-agent\n\n";
}

} // namespace spdk_flint 