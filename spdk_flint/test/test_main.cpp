#include <catch2/catch_test_macros.hpp>
#include <catch2/catch_session.hpp>

#include "logging.hpp"
#include "app.hpp"

using namespace spdk_flint;

// Basic application configuration tests
TEST_CASE("Application Configuration", "[config]") {
    SECTION("Default configuration values") {
        AppConfig config;
        REQUIRE(config.mode == AppMode::ALL);
        REQUIRE(config.csi_endpoint == "unix:///csi/csi.sock");
        REQUIRE(config.spdk_rpc_url == "unix:///var/tmp/spdk.sock");
        REQUIRE(config.nvmeof_transport == "tcp");
        REQUIRE(config.nvmeof_target_port == 4420);
        REQUIRE(config.dashboard_port == 8080);
        REQUIRE(config.health_port == 9809);
        REQUIRE(config.log_level == "info");
        REQUIRE(config.auto_initialize_blobstore == true);
        REQUIRE(config.discovery_interval == 300);
    }
    
    SECTION("Load configuration from environment") {
        // Set some environment variables
        setenv("CSI_MODE", "controller", 1);
        setenv("LOG_LEVEL", "debug", 1);
        setenv("NVMEOF_TARGET_PORT", "5420", 1);
        
        AppConfig config = loadConfigFromEnvironment();
        
        REQUIRE(config.mode == AppMode::CONTROLLER);
        REQUIRE(config.log_level == "debug");
        REQUIRE(config.nvmeof_target_port == 5420);
        
        // Clean up
        unsetenv("CSI_MODE");
        unsetenv("LOG_LEVEL");
        unsetenv("NVMEOF_TARGET_PORT");
    }
}

// Logging system tests
TEST_CASE("Logging System", "[logging]") {
    SECTION("Logger initialization") {
        // Initialize with debug level
        REQUIRE_NOTHROW(Logger::initialize("test_logger", "debug"));
        
        auto logger = Logger::get();
        REQUIRE(logger != nullptr);
        
        // Test different log levels
        REQUIRE_NOTHROW(spdk_flint::logger()->debug("Debug message: {}", 42));
        REQUIRE_NOTHROW(spdk_flint::logger()->info("Info message: {}", "test"));
        REQUIRE_NOTHROW(spdk_flint::logger()->warn("Warning message"));
        REQUIRE_NOTHROW(spdk_flint::logger()->error("Error message"));
        
        // Test context-aware logging
        REQUIRE_NOTHROW(spdk_flint::logger()->info("[CSI] Creating volume {}", "test-vol"));
        REQUIRE_NOTHROW(spdk_flint::logger()->info("[SPDK] Initializing SPDK"));
        REQUIRE_NOTHROW(spdk_flint::logger()->info("[DASHBOARD] Starting dashboard on port {}", 8080));
        
        Logger::shutdown();
    }
    
    SECTION("Log level setting") {
        Logger::initialize("test_logger", "info");
        
        // This should work
        REQUIRE_NOTHROW(Logger::setLevel("debug"));
        REQUIRE_NOTHROW(Logger::setLevel("error"));
        REQUIRE_NOTHROW(Logger::setLevel("info"));
        
        Logger::shutdown();
    }
}

// Mode parsing tests
TEST_CASE("Application Mode Parsing", "[app][mode]") {
    Application app(AppConfig{});
    
    SECTION("Valid mode strings") {
        // These would normally be private, but we can test the public interface
        // by setting environment variables and checking the config loading
        
        setenv("CSI_MODE", "csi-driver", 1);
        auto config1 = loadConfigFromEnvironment();
        REQUIRE(config1.mode == AppMode::CSI_DRIVER);
        
        setenv("CSI_MODE", "controller", 1);
        auto config2 = loadConfigFromEnvironment();
        REQUIRE(config2.mode == AppMode::CONTROLLER);
        
        setenv("CSI_MODE", "dashboard-backend", 1);
        auto config3 = loadConfigFromEnvironment();
        REQUIRE(config3.mode == AppMode::DASHBOARD_BACKEND);
        
        setenv("CSI_MODE", "node-agent", 1);
        auto config4 = loadConfigFromEnvironment();
        REQUIRE(config4.mode == AppMode::NODE_AGENT);
        
        setenv("CSI_MODE", "all", 1);
        auto config5 = loadConfigFromEnvironment();
        REQUIRE(config5.mode == AppMode::ALL);
        
        unsetenv("CSI_MODE");
    }
}

// Utility function tests
TEST_CASE("Utility Functions", "[utils]") {
    SECTION("Version and usage printing") {
        // These functions print to stdout, so we just ensure they don't crash
        REQUIRE_NOTHROW(printVersion());
        REQUIRE_NOTHROW(printUsage());
    }
} 