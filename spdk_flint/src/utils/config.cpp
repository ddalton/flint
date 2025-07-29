#include "utils/config.hpp"
#include "app.hpp"
#include "logging.hpp"
#include <iostream>
#include <cstdlib>
#include <stdexcept>
#include <fstream> // Added for file existence check
#include <algorithm> // Added for std::transform
#include <chrono> // Added for timing

namespace spdk_flint {

AppMode parseAppMode(const std::string& mode_str) {
    logger()->debug("[CONFIG] Parsing application mode: '{}'", mode_str);
    
    if (mode_str.empty() || mode_str == "node-agent") {
        logger()->debug("[CONFIG] Mode parsed successfully: node-agent");
        return AppMode::NODE_AGENT;
    }
    
    // spdk_flint only supports node-agent mode
    logger()->error("[CONFIG] Invalid mode '{}' - spdk_flint only supports 'node-agent' mode", mode_str);
    logger()->error("[CONFIG] Other services (controller, dashboard) use Rust RPC clients in spdk-csi-driver");
    
    throw std::invalid_argument("Only 'node-agent' mode is supported by spdk_flint");
}

AppConfig loadConfigFromEnvironment() {
    logger()->info("[CONFIG] Loading configuration from environment variables");
    auto start_time = std::chrono::steady_clock::now();
    
    AppConfig config;
    
    // Parse mode - only node-agent supported
    const char* mode_env = std::getenv("CSI_MODE");
    logger()->debug("[CONFIG] Environment variable CSI_MODE: '{}'", mode_env ? mode_env : "unset");
    
    if (mode_env) {
        try {
            config.mode = parseAppMode(std::string(mode_env));
            logger()->info("[CONFIG] Application mode set to: node-agent (from CSI_MODE)");
        } catch (const std::exception& e) {
            logger()->error("[CONFIG] Invalid CSI_MODE environment variable '{}': {}", mode_env, e.what());
            throw;
        }
    } else {
        // Default to node-agent mode
        config.mode = AppMode::NODE_AGENT;
        logger()->info("[CONFIG] Application mode defaulted to: node-agent (CSI_MODE not set)");
    }
    
    // Node identification
    const char* node_id = std::getenv("NODE_ID");
    const char* hostname = std::getenv("HOSTNAME");
    logger()->debug("[CONFIG] Environment variables - NODE_ID: '{}', HOSTNAME: '{}'", 
                   node_id ? node_id : "unset", hostname ? hostname : "unset");
    
    if (node_id) {
        config.node_id = std::string(node_id);
        logger()->info("[CONFIG] Node ID set to: '{}' (from NODE_ID)", config.node_id);
    } else {
        // Try HOSTNAME as fallback
        if (hostname) {
            config.node_id = std::string(hostname);
            logger()->info("[CONFIG] Node ID set to: '{}' (from HOSTNAME fallback)", config.node_id);
        } else {
            config.node_id = "unknown-node";
            logger()->warn("[CONFIG] Node ID defaulted to: '{}' (neither NODE_ID nor HOSTNAME set)", config.node_id);
        }
    }
    
    // Logging configuration
    const char* log_level = std::getenv("LOG_LEVEL");
    logger()->debug("[CONFIG] Environment variable LOG_LEVEL: '{}'", log_level ? log_level : "unset");
    
    if (log_level) {
        config.log_level = std::string(log_level);
        logger()->info("[CONFIG] Log level set to: '{}' (from LOG_LEVEL)", config.log_level);
    } else {
        logger()->debug("[CONFIG] Log level remains default: '{}'", config.log_level);
    }
    
    // Network ports
    const char* health_port = std::getenv("HEALTH_PORT");
    const char* node_agent_port = std::getenv("NODE_AGENT_PORT");
    logger()->debug("[CONFIG] Port environment variables - HEALTH_PORT: '{}', NODE_AGENT_PORT: '{}'",
                   health_port ? health_port : "unset", node_agent_port ? node_agent_port : "unset");
    
    if (health_port) {
        try {
            config.health_port = static_cast<uint16_t>(std::stoi(health_port));
            logger()->info("[CONFIG] Health port set to: {} (from HEALTH_PORT)", config.health_port);
        } catch (const std::exception& e) {
            logger()->error("[CONFIG] Invalid HEALTH_PORT value '{}': {}", health_port, e.what());
            logger()->warn("[CONFIG] Using default health port: {}", config.health_port);
        }
    } else {
        logger()->debug("[CONFIG] Health port remains default: {}", config.health_port);
    }
    
    if (node_agent_port) {
        try {
            config.node_agent_port = static_cast<uint16_t>(std::stoi(node_agent_port));
            logger()->info("[CONFIG] Node agent port set to: {} (from NODE_AGENT_PORT)", config.node_agent_port);
        } catch (const std::exception& e) {
            logger()->error("[CONFIG] Invalid NODE_AGENT_PORT value '{}': {}", node_agent_port, e.what());
            logger()->warn("[CONFIG] Using default node agent port: {}", config.node_agent_port);
        }
    } else {
        logger()->debug("[CONFIG] Node agent port remains default: {}", config.node_agent_port);
    }
    
    // Kubernetes namespace
    const char* target_namespace = std::getenv("TARGET_NAMESPACE");
    logger()->debug("[CONFIG] Environment variable TARGET_NAMESPACE: '{}'", target_namespace ? target_namespace : "unset");
    
    if (target_namespace) {
        config.target_namespace = std::string(target_namespace);
        logger()->info("[CONFIG] Target namespace set to: '{}' (from TARGET_NAMESPACE)", config.target_namespace);
    } else {
        logger()->debug("[CONFIG] Target namespace remains default: '{}'", config.target_namespace);
    }
    
    // SPDK configuration
    const char* discovery_interval = std::getenv("DISK_DISCOVERY_INTERVAL");
    const char* auto_init = std::getenv("AUTO_INITIALIZE_BLOBSTORE");
    const char* backup_path = std::getenv("DISK_BACKUP_PATH");
    const char* config_file = std::getenv("SPDK_CONFIG_FILE");
    
    logger()->debug("[CONFIG] SPDK environment variables - DISK_DISCOVERY_INTERVAL: '{}', AUTO_INITIALIZE_BLOBSTORE: '{}', "
                   "DISK_BACKUP_PATH: '{}', SPDK_CONFIG_FILE: '{}'",
                   discovery_interval ? discovery_interval : "unset",
                   auto_init ? auto_init : "unset",
                   backup_path ? backup_path : "unset",
                   config_file ? config_file : "unset");
    
    if (discovery_interval) {
        try {
            config.discovery_interval = static_cast<uint32_t>(std::stoi(discovery_interval));
            logger()->info("[CONFIG] Discovery interval set to: {} seconds (from DISK_DISCOVERY_INTERVAL)", config.discovery_interval);
        } catch (const std::exception& e) {
            logger()->error("[CONFIG] Invalid DISK_DISCOVERY_INTERVAL value '{}': {}", discovery_interval, e.what());
            logger()->warn("[CONFIG] Using default discovery interval: {} seconds", config.discovery_interval);
        }
    } else {
        logger()->debug("[CONFIG] Discovery interval remains default: {} seconds", config.discovery_interval);
    }
    
    if (auto_init) {
        std::string auto_init_str = std::string(auto_init);
        // Convert to lowercase for comparison
        std::transform(auto_init_str.begin(), auto_init_str.end(), auto_init_str.begin(), ::tolower);
        
        if (auto_init_str == "true" || auto_init_str == "1" || auto_init_str == "yes" || auto_init_str == "on") {
            config.auto_initialize_blobstore = true;
        } else if (auto_init_str == "false" || auto_init_str == "0" || auto_init_str == "no" || auto_init_str == "off") {
            config.auto_initialize_blobstore = false;
        } else {
            logger()->warn("[CONFIG] Invalid AUTO_INITIALIZE_BLOBSTORE value '{}', using default: {}", 
                          auto_init, config.auto_initialize_blobstore);
        }
        logger()->info("[CONFIG] Auto initialize blobstore set to: {} (from AUTO_INITIALIZE_BLOBSTORE)", 
                      config.auto_initialize_blobstore);
    } else {
        logger()->debug("[CONFIG] Auto initialize blobstore remains default: {}", config.auto_initialize_blobstore);
    }
    
    if (backup_path) {
        config.backup_path = std::string(backup_path);
        logger()->info("[CONFIG] Backup path set to: '{}' (from DISK_BACKUP_PATH)", config.backup_path);
    } else {
        logger()->debug("[CONFIG] Backup path remains default: '{}'", config.backup_path);
    }
    
    // SPDK configuration file (optional)
    if (config_file) {
        config.config_file = std::string(config_file);
        logger()->info("[CONFIG] SPDK config file set to: '{}' (from SPDK_CONFIG_FILE)", config.config_file);
        
        // Check if the file exists and is readable
        std::ifstream file(config.config_file);
        if (file.good()) {
            logger()->debug("[CONFIG] SPDK config file verified: exists and readable");
        } else {
            logger()->warn("[CONFIG] SPDK config file '{}' may not exist or is not readable", config.config_file);
        }
    } else {
        logger()->debug("[CONFIG] No SPDK config file specified (SPDK_CONFIG_FILE not set)");
    }
    
    auto duration = std::chrono::duration_cast<std::chrono::milliseconds>(
        std::chrono::steady_clock::now() - start_time);
    
    // Log the loaded configuration summary
    logger()->info("[CONFIG] Configuration loaded successfully in {} ms", duration.count());
    logger()->info("[CONFIG] ===== Configuration Summary =====");
    logger()->info("[CONFIG]   Mode: node-agent (embedded SPDK)");
    logger()->info("[CONFIG]   Node ID: '{}'", config.node_id);
    logger()->info("[CONFIG]   Log Level: '{}'", config.log_level);
    logger()->info("[CONFIG]   Health Port: {}", config.health_port);
    logger()->info("[CONFIG]   Node Agent Port: {}", config.node_agent_port);
    logger()->info("[CONFIG]   Target Namespace: '{}'", config.target_namespace);
    logger()->info("[CONFIG]   Discovery Interval: {} seconds", config.discovery_interval);
    logger()->info("[CONFIG]   Auto Initialize Blobstore: {}", config.auto_initialize_blobstore);
    logger()->info("[CONFIG]   Backup Path: '{}'", config.backup_path);
    if (!config.config_file.empty()) {
        logger()->info("[CONFIG]   SPDK Config File: '{}'", config.config_file);
    }
    logger()->info("[CONFIG] ================================");
    
    // Log configuration validation
    if (config.health_port == config.node_agent_port) {
        logger()->warn("[CONFIG] Health port and node agent port are the same ({}), this may cause conflicts", config.health_port);
    }
    
    if (config.discovery_interval < 30) {
        logger()->warn("[CONFIG] Discovery interval ({} seconds) is very low, this may impact performance", config.discovery_interval);
    }
    
    if (config.node_id == "unknown-node") {
        logger()->warn("[CONFIG] Node ID is 'unknown-node' - consider setting NODE_ID environment variable");
    }
    
    return config;
}

} // namespace spdk_flint 