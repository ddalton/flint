#include "app.hpp"
#include "utils/config.hpp"
#include "logging.hpp"
#include <iostream>
#include <memory>
#include <unistd.h> // For getpid()
#include <thread>   // For std::this_thread::get_id()
#include <chrono>   // For std::chrono::steady_clock

// Global application instance for signal handling
static std::unique_ptr<spdk_flint::Application> g_app;

void printUsage() {
    std::cout << "SPDK Flint Node Agent - High-Performance Storage Node Agent with SPDK RPC Interface\n\n";
    std::cout << "Usage: spdk_flint [OPTIONS]\n\n";
    std::cout << "OPTIONS:\n";
    std::cout << "  --mode <mode>        Operating mode (only 'node-agent' supported)\n";
    std::cout << "  --log-level <level>  Log level (debug, info, warn, error)\n";
    std::cout << "  --rpc-socket <path>  SPDK RPC socket path (default: /var/tmp/spdk.sock)\n";
    std::cout << "  --help, -h           Show this help message\n";
    std::cout << "  --version, -v        Show version information\n\n";
    std::cout << "ENVIRONMENT VARIABLES:\n";
    std::cout << "  CSI_MODE             Operating mode (node-agent)\n";
    std::cout << "  NODE_ID              Node identifier\n";
    std::cout << "  LOG_LEVEL            Log level\n";
    std::cout << "  HEALTH_PORT          Health check port (default: 9809)\n";
    std::cout << "  NODE_AGENT_PORT      Node agent API port (default: 8090)\n";
    std::cout << "  TARGET_NAMESPACE     Kubernetes namespace (default: flint-system)\n";
    std::cout << "  SPDK_RPC_SOCKET      SPDK RPC socket path\n\n";
    std::cout << "EXAMPLES:\n";
    std::cout << "  spdk_flint                         # Start in node-agent mode\n";
    std::cout << "  spdk_flint --log-level debug       # Start with debug logging\n";
    std::cout << "  CSI_MODE=node-agent spdk_flint     # Start via environment variable\n\n";
    std::cout << "NOTE: spdk_flint connects to an external SPDK target via RPC.\n";
    std::cout << "      Ensure the SPDK target is running and accessible.\n";
}

void printVersion() {
    std::cout << "SPDK Flint Node Agent\n";
    std::cout << "Version: 1.0.0\n";
    std::cout << "SPDK Integration: RPC Interface\n";
    std::cout << "Architecture: Node Agent with SPDK RPC Client\n";
    std::cout << "Build: " << __DATE__ << " " << __TIME__ << "\n";
}

int main(int argc, char* argv[]) {
    auto main_start_time = std::chrono::steady_clock::now();
    
    std::string mode;
    std::string log_level = "info";
    std::string rpc_socket;
    
    std::cout << "SPDK Flint Node Agent - Starting up...\n";
    std::cout << "Process ID: " << getpid() << ", Thread ID: " << std::this_thread::get_id() << "\n";
    std::cout << "Command line: ";
    for (int i = 0; i < argc; ++i) {
        std::cout << argv[i];
        if (i < argc - 1) std::cout << " ";
    }
    std::cout << "\n";
    
    // Parse command line arguments
    for (int i = 1; i < argc; ++i) {
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
        } else if (arg == "--rpc-socket" && i + 1 < argc) {
            rpc_socket = argv[++i];
        } else {
            std::cerr << "Unknown argument: " << arg << "\n";
            printUsage();
            return 1;
        }
    }
    
    // Initialize logging first
    spdk_flint::Logger::initialize("spdk_flint_node_agent", log_level);
    
    // Log startup information
    spdk_flint::logger()->info("========================================");
    spdk_flint::logger()->info("Starting SPDK Flint Node Agent");
    spdk_flint::logger()->info("Version: 1.0.0 | Build: {} {}", __DATE__, __TIME__);
    spdk_flint::logger()->info("Process: PID={}, main_thread={}", getpid(), spdk_flint::current_thread_id());
    spdk_flint::logger()->info("========================================");
    
    // Log command line arguments
    spdk_flint::logger()->debug("[MAIN] Command line arguments parsed:");
    spdk_flint::logger()->debug("[MAIN]   --mode: '{}'", mode.empty() ? "not specified" : mode);
    spdk_flint::logger()->debug("[MAIN]   --log-level: '{}'", log_level);
    spdk_flint::logger()->debug("[MAIN]   --rpc-socket: '{}'", rpc_socket.empty() ? "not specified" : rpc_socket);
    
    try {
        // Load configuration from environment and command line
        spdk_flint::logger()->info("[MAIN] Loading application configuration");
        auto config = spdk_flint::loadConfigFromEnvironment();
        
        // Override with command line arguments
        if (!mode.empty()) {
            spdk_flint::logger()->debug("[MAIN] Overriding mode from command line: '{}'", mode);
            config.mode = spdk_flint::parseAppMode(mode);
        }
        if (!rpc_socket.empty()) {
            spdk_flint::logger()->debug("[MAIN] Overriding RPC socket from command line: '{}'", rpc_socket);
            config.spdk_rpc_socket = rpc_socket;
        }
        
        // Validate that we're in node-agent mode
        if (config.mode != spdk_flint::AppMode::NODE_AGENT) {
            spdk_flint::logger()->error("[MAIN] spdk_flint only supports node-agent mode");
            spdk_flint::logger()->error("[MAIN] Other CSI services should use spdk-csi-driver (Rust)");
            return 1;
        }
        
        spdk_flint::logger()->info("[MAIN] Configuration validated successfully");
        
        // Create and initialize the application
        spdk_flint::logger()->info("[MAIN] Creating SPDK Flint Node Agent application");
        g_app = std::make_unique<spdk_flint::Application>(config);
        
        auto init_start_time = std::chrono::steady_clock::now();
        if (!g_app->initialize()) {
            spdk_flint::logger()->error("[MAIN] Failed to initialize SPDK Flint Node Agent");
            return 1;
        }
        
        auto init_duration = std::chrono::duration_cast<std::chrono::milliseconds>(
            std::chrono::steady_clock::now() - init_start_time);
        spdk_flint::logger()->info("[MAIN] SPDK Flint Node Agent initialized successfully in {} ms", init_duration.count());
        
        auto startup_duration = std::chrono::duration_cast<std::chrono::milliseconds>(
            std::chrono::steady_clock::now() - main_start_time);
        spdk_flint::logger()->info("[MAIN] Total startup time: {} ms", startup_duration.count());
        spdk_flint::logger()->info("[MAIN] ========================================");
        spdk_flint::logger()->info("[MAIN] SPDK Flint Node Agent is now running");
        spdk_flint::logger()->info("[MAIN] Services: HTTP API, Health monitoring, SPDK RPC client");
        spdk_flint::logger()->info("[MAIN] Entering application event loop - blocking until shutdown");
        spdk_flint::logger()->info("[MAIN] ========================================");
        
        // Run the application - this will block until shutdown
        int exit_code = g_app->run();
        
        auto total_runtime = std::chrono::duration_cast<std::chrono::seconds>(
            std::chrono::steady_clock::now() - main_start_time);
        
        spdk_flint::logger()->info("[MAIN] ========================================");
        spdk_flint::logger()->info("[MAIN] SPDK Flint Node Agent finished with exit code: {}", exit_code);
        spdk_flint::logger()->info("[MAIN] Total runtime: {} seconds", total_runtime.count());
        spdk_flint::logger()->info("[MAIN] ========================================");
        
        // Cleanup
        g_app.reset();
        spdk_flint::Logger::shutdown();
        
        return exit_code;
        
    } catch (const std::exception& e) {
        if (spdk_flint::logger()) {
            spdk_flint::logger()->error("[MAIN] Fatal error: {}", e.what());
            spdk_flint::logger()->error("[MAIN] SPDK Flint Node Agent startup failed");
        } else {
            std::cerr << "Fatal error during startup: " << e.what() << std::endl;
        }
        
        // Attempt cleanup
        if (g_app) {
            g_app.reset();
        }
        spdk_flint::Logger::shutdown();
        
        return 1;
    } catch (...) {
        if (spdk_flint::logger()) {
            spdk_flint::logger()->error("[MAIN] Unknown fatal error occurred");
        } else {
            std::cerr << "Unknown fatal error during startup" << std::endl;
        }
        
        // Attempt cleanup
        if (g_app) {
            g_app.reset();
        }
        spdk_flint::Logger::shutdown();
        
        return 1;
    }
} 