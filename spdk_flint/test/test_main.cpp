#include <catch2/catch_test_macros.hpp>
#include <catch2/catch_session.hpp>

#include "logging.hpp"
#include "app.hpp"

using namespace spdk_flint;

// Basic application configuration tests for node agent only
TEST_CASE("Node Agent Configuration", "[config]") {
    SECTION("Default configuration values") {
        AppConfig config;
        REQUIRE(config.mode == AppMode::NODE_AGENT);
        REQUIRE(config.health_port == 9809);
        REQUIRE(config.node_agent_port == 8090);
        REQUIRE(config.log_level == "info");
        REQUIRE(config.auto_initialize_blobstore == true);
        REQUIRE(config.discovery_interval == 300);
        REQUIRE(config.target_namespace == "flint-system");
        REQUIRE(config.backup_path == "/var/lib/spdk-csi/backups");
    }
    
    SECTION("Load configuration from environment") {
        // Set some environment variables
        setenv("CSI_MODE", "node-agent", 1);
        setenv("LOG_LEVEL", "debug", 1);
        setenv("NODE_AGENT_PORT", "8091", 1);
        setenv("HEALTH_PORT", "9810", 1);
        setenv("NODE_ID", "test-node", 1);
        setenv("TARGET_NAMESPACE", "test-namespace", 1);
        setenv("DISK_DISCOVERY_INTERVAL", "120", 1);
        
        AppConfig config = loadConfigFromEnvironment();
        
        REQUIRE(config.mode == AppMode::NODE_AGENT);
        REQUIRE(config.log_level == "debug");
        REQUIRE(config.node_agent_port == 8091);
        REQUIRE(config.health_port == 9810);
        REQUIRE(config.node_id == "test-node");
        REQUIRE(config.target_namespace == "test-namespace");
        REQUIRE(config.discovery_interval == 120);
        
        // Clean up
        unsetenv("CSI_MODE");
        unsetenv("LOG_LEVEL");
        unsetenv("NODE_AGENT_PORT");
        unsetenv("HEALTH_PORT");
        unsetenv("NODE_ID");
        unsetenv("TARGET_NAMESPACE");
        unsetenv("DISK_DISCOVERY_INTERVAL");
    }
    
    SECTION("Invalid mode rejection") {
        setenv("CSI_MODE", "controller", 1);
        
        REQUIRE_THROWS_AS(loadConfigFromEnvironment(), std::invalid_argument);
        
        unsetenv("CSI_MODE");
    }
}

// Logging system tests
TEST_CASE("Logging System", "[logging]") {
    SECTION("Logger initialization") {
        // Initialize with debug level
        REQUIRE_NOTHROW(Logger::initialize("spdk_flint_node_agent", "debug"));
        
        auto logger = Logger::get();
        REQUIRE(logger != nullptr);
        
        // Test basic logging calls
        REQUIRE_NOTHROW(logger->info("Test info message"));
        REQUIRE_NOTHROW(logger->debug("Test debug message"));
        REQUIRE_NOTHROW(logger->warn("Test warning message"));
        REQUIRE_NOTHROW(logger->error("Test error message"));
        
        Logger::shutdown();
    }
    
    SECTION("Multiple initialization") {
        // Should be able to initialize multiple times safely
        REQUIRE_NOTHROW(Logger::initialize("spdk_flint_node_agent", "info"));
        REQUIRE_NOTHROW(Logger::initialize("spdk_flint_node_agent", "debug"));
        
        auto logger = Logger::get();
        REQUIRE(logger != nullptr);
        
        Logger::shutdown();
    }
}

// Application mode parsing tests
TEST_CASE("Application Mode Parsing", "[mode]") {
    SECTION("Valid node-agent mode") {
        REQUIRE(parseAppMode("node-agent") == AppMode::NODE_AGENT);
        REQUIRE(parseAppMode("") == AppMode::NODE_AGENT);  // Default
    }
    
    SECTION("Invalid modes rejected") {
        REQUIRE_THROWS_AS(parseAppMode("controller"), std::invalid_argument);
        REQUIRE_THROWS_AS(parseAppMode("dashboard-backend"), std::invalid_argument);
        REQUIRE_THROWS_AS(parseAppMode("csi-driver"), std::invalid_argument);
        REQUIRE_THROWS_AS(parseAppMode("all"), std::invalid_argument);
        REQUIRE_THROWS_AS(parseAppMode("invalid"), std::invalid_argument);
    }
}

// Basic application lifecycle tests
TEST_CASE("Node Agent Application", "[app]") {
    SECTION("Application creation") {
        AppConfig config;
        config.mode = AppMode::NODE_AGENT;
        config.log_level = "debug";
        
        REQUIRE_NOTHROW(Application app(config));
    }
    
    // NOTE: Cannot test full initialization without SPDK setup
    // These would require a proper SPDK test environment
}

int main(int argc, char* argv[]) {
    // Initialize logging for tests
    spdk_flint::Logger::initialize("spdk_flint_test", "debug");
    
    int result = Catch::Session().run(argc, argv);
    
    spdk_flint::Logger::shutdown();
    
    return result;
} 