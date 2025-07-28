#include "app.hpp"
#include "logging.hpp"
#include <iostream>
#include <string>
#include <memory>

using namespace spdk_flint;

std::unique_ptr<Application> g_app;

int main(int argc, char** argv) {
    std::string mode = "all";
    std::string log_level = "info";
    std::string config_file;

    // Parse command line arguments
    for (int i = 1; i < argc; ++i) {
        std::string arg = argv[i];
        
        if (arg == "--help" || arg == "-h") {
            spdk_flint::printUsage();
            return 0;
        } else if (arg == "--version" || arg == "-v") {
            spdk_flint::printVersion();
            return 0;
        } else if (arg == "--mode" && i + 1 < argc) {
            mode = argv[++i];
        } else if (arg == "--log-level" && i + 1 < argc) {
            log_level = argv[++i];
        } else if (arg == "--config" && i + 1 < argc) {
            config_file = argv[++i];
        } else {
            std::cerr << "Unknown argument: " << arg << "\n";
            spdk_flint::printUsage();
            return 1;
        }
    }

    try {
        // Load configuration from environment variables (override with CLI args)
        auto config = spdk_flint::loadConfigFromEnvironment();
        
        // Override with command line arguments
        if (!mode.empty()) config.mode = spdk_flint::parseAppMode(mode);
        if (!config_file.empty()) config.config_file = config_file;
        
        // Initialize logging first
        spdk_flint::Logger::initialize("spdk_flint", log_level);
        
        spdk_flint::logger()->info("Starting SPDK Flint CSI Driver");
        spdk_flint::logger()->info("Mode: {}, Log Level: {}", mode, log_level);
        
        // Create and run the application
        // Note: SPDK will handle signal management internally
        g_app = std::make_unique<spdk_flint::Application>(config);
        
        if (!g_app->initialize()) {
            spdk_flint::logger()->error("Failed to initialize application");
            return 1;
        }
        
        spdk_flint::logger()->info("Application initialized successfully");
        
        // Run the application - this will block until shutdown
        int exit_code = g_app->run();
        
        spdk_flint::logger()->info("Application finished with exit code: {}", exit_code);
        
        // Cleanup
        g_app->shutdown();
        g_app.reset();
        
        spdk_flint::Logger::shutdown();
        
        return exit_code;
        
    } catch (const std::exception& e) {
        std::cerr << "Fatal error: " << e.what() << std::endl;
        if (g_app) {
            g_app->shutdown();
            g_app.reset();
        }
        return 1;
    }
} 