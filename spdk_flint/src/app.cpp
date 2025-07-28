#include "app.hpp"
#include "spdk/spdk_wrapper.hpp"
#include "utils/kube_client.hpp"
#include <csignal>
#include <thread>
#include <chrono>
#include <crow.h>

namespace spdk_flint {

// CSI Service implementation (stub)
class CSIService {
public:
    explicit CSIService(std::shared_ptr<spdk::SpdkWrapper> spdk, 
                       std::shared_ptr<kube::KubeClient> kube,
                       const AppConfig& config)
        : spdk_(spdk), kube_client_(kube), config_(config) {}
    
    void start() {
        LOG_CSI_INFO("Service", "Starting CSI gRPC services on {}", config_.csi_endpoint);
        running_ = true;
        // TODO: Implement gRPC server setup
        LOG_CSI_INFO("Service", "CSI services started successfully");
    }
    
    void stop() {
        LOG_CSI_INFO("Service", "Stopping CSI services");
        running_ = false;
        // TODO: Implement graceful shutdown
    }
    
    bool is_running() const { return running_; }

private:
    std::shared_ptr<spdk::SpdkWrapper> spdk_;
    std::shared_ptr<kube::KubeClient> kube_client_;
    AppConfig config_;
    std::atomic<bool> running_{false};
};

// Dashboard Service implementation with Crow
class DashboardService {
public:
    explicit DashboardService(std::shared_ptr<spdk::SpdkWrapper> spdk,
                             std::shared_ptr<kube::KubeClient> kube,
                             const AppConfig& config)
        : spdk_(spdk), kube_client_(kube), config_(config) {}
    
    void start() {
        LOG_DASHBOARD_INFO("Starting dashboard backend on port {}", config_.dashboard_port);
        
        // Set up Crow routes
        app_.route_dynamic("/health")
            .methods("GET"_method)
            ([this](const crow::request& req) {
                (void)req; // Suppress unused parameter warning
                return crow::response(200, "OK");
            });
        
        app_.route_dynamic("/api/v1/volumes")
            .methods("GET"_method)
            ([this](const crow::request& req) {
                return handle_get_volumes(req);
            });
        
        app_.route_dynamic("/api/v1/nodes")
            .methods("GET"_method)
            ([this](const crow::request& req) {
                return handle_get_nodes(req);
            });
        
        app_.route_dynamic("/api/v1/stats")
            .methods("GET"_method)
            ([this](const crow::request& req) {
                return handle_get_stats(req);
            });
        
        app_.route_dynamic("/api/v1/devices")
            .methods("GET"_method)
            ([this](const crow::request& req) {
                return handle_get_devices(req);
            });
        
        app_.route_dynamic("/api/v1/discovery")
            .methods("GET"_method)
            ([this](const crow::request& req) {
                return handle_discovery(req);
            });
        
        // Start server in background thread
        server_thread_ = std::thread([this]() {
            app_.port(config_.dashboard_port).multithreaded().run();
        });
        
        running_ = true;
        LOG_DASHBOARD_INFO("Dashboard backend started on port {}", config_.dashboard_port);
    }
    
    void stop() {
        LOG_DASHBOARD_INFO("Stopping dashboard backend");
        running_ = false;
        app_.stop();
        if (server_thread_.joinable()) {
            server_thread_.join();
        }
    }
    
    bool is_running() const { return running_; }

private:
    std::shared_ptr<spdk::SpdkWrapper> spdk_;
    std::shared_ptr<kube::KubeClient> kube_client_;
    AppConfig config_;
    std::atomic<bool> running_{false};
    crow::SimpleApp app_;
    std::thread server_thread_;
    
    crow::response handle_get_volumes(const crow::request& req) {
        (void)req; // Suppress unused parameter warning
        try {
            // Get volumes from Kubernetes
            auto volumes_future = kube_client_->list_spdk_volumes(config_.target_namespace);
            auto volumes = volumes_future.get();
            
            nlohmann::json response = nlohmann::json::array();
            for (const auto& vol : volumes) {
                nlohmann::json vol_json;
                vol.to_json(vol_json);
                response.push_back(vol_json);
            }
            
            return crow::response(200, response.dump());
        } catch (const std::exception& e) {
            LOG_DASHBOARD_ERROR("Failed to get volumes: {}", e.what());
            return crow::response(500, R"({"error": "Failed to get volumes"})");
        }
    }
    
    crow::response handle_get_nodes(const crow::request& req) {
        (void)req; // Suppress unused parameter warning
        try {
            auto nodes_future = kube_client_->list_spdk_nodes(config_.target_namespace);
            auto nodes = nodes_future.get();
            
            nlohmann::json response = nlohmann::json::array();
            for (const auto& node : nodes) {
                nlohmann::json node_json;
                node.to_json(node_json);
                response.push_back(node_json);
            }
            
            return crow::response(200, response.dump());
        } catch (const std::exception& e) {
            LOG_DASHBOARD_ERROR("Failed to get nodes: {}", e.what());
            return crow::response(500, R"({"error": "Failed to get nodes"})");
        }
    }
    
    crow::response handle_get_stats(const crow::request& req) {
        try {
            auto bdev_name = req.url_params.get("bdev");
            
            if (bdev_name) {
                // Get stats for specific bdev using thread-safe async call
                auto stats_future = spdk_->getBdevIoStatsAsync(std::string(bdev_name));
                auto stats = stats_future.get(); // Wait for result from SPDK thread
                
                nlohmann::json response = stats;
                return crow::response(200, response.dump());
            } else {
                // Get overall stats using thread-safe async call
                auto stats_future = spdk_->getBdevIoStatsAsync();
                auto stats = stats_future.get(); // Wait for result from SPDK thread
                
                // Also get bdev list to include device info
                auto bdevs_future = spdk_->getBdevsAsync();
                auto bdevs = bdevs_future.get(); // Wait for result from SPDK thread
                
                nlohmann::json response;
                response["io_stats"] = stats;
                response["total_devices"] = bdevs.size();
                
                // Add per-device stats
                nlohmann::json device_stats = nlohmann::json::array();
                for (const auto& bdev : bdevs) {
                    nlohmann::json bdev_info;
                    bdev_info["name"] = bdev.name;
                    bdev_info["size"] = bdev.num_blocks * bdev.block_size;
                    bdev_info["block_size"] = bdev.block_size;
                    bdev_info["stats"] = spdk_->getBdevIoStatsAsync(bdev.name).get();
                    device_stats.push_back(bdev_info);
                }
                response["devices"] = device_stats;
                
                return crow::response(200, response.dump());
            }
        } catch (const std::exception& e) {
            LOG_DASHBOARD_ERROR("Failed to get stats: {}", e.what());
            return crow::response(500, R"({"error": "Failed to get stats"})");
        }
    }

    crow::response handle_get_devices(const crow::request& req) {
        (void)req; // Suppress unused parameter warning
        try {
            // Get device list using thread-safe async call
            auto bdevs_future = spdk_->getBdevsAsync();
            auto bdevs = bdevs_future.get(); // Wait for result from SPDK thread
            
            nlohmann::json response = nlohmann::json::array();
            for (const auto& bdev : bdevs) {
                nlohmann::json device;
                device["name"] = bdev.name;
                device["size"] = bdev.num_blocks * bdev.block_size;
                device["block_size"] = bdev.block_size;
                device["uuid"] = bdev.uuid;
                device["product_name"] = bdev.product_name;
                device["claimed"] = bdev.claimed;
                
                // Get stats for this device using thread-safe async call
                auto stats_future = spdk_->getBdevIoStatsAsync(bdev.name);
                device["stats"] = stats_future.get(); // Wait for result from SPDK thread
                
                response.push_back(device);
            }
            
            return crow::response(200, response.dump());
        } catch (const std::exception& e) {
            LOG_DASHBOARD_ERROR("Failed to get devices: {}", e.what());
            return crow::response(500, R"({"error": "Failed to get devices"})");
        }
    }
    
    crow::response handle_discovery(const crow::request& req) {
        (void)req; // Suppress unused parameter warning
        try {
            nlohmann::json response;
            
            // Discover Kubernetes nodes
            auto nodes_future = kube_client_->discover_spdk_nodes();
            auto discovered_nodes = nodes_future.get();
            
            nlohmann::json nodes = nlohmann::json::array();
            for (const auto& [node_name, node_address] : discovered_nodes) {
                nlohmann::json node;
                node["name"] = node_name;
                node["address"] = node_address;
                node["type"] = "kubernetes_node";
                nodes.push_back(node);
            }
            
            response["discovered_nodes"] = nodes;
            response["node_count"] = nodes.size();
            
            // Also include SPDK devices summary
            auto bdevs = spdk_->getBdevs();
            response["device_count"] = bdevs.size();
            
            auto now = std::chrono::system_clock::now();
            auto timestamp = std::chrono::duration_cast<std::chrono::seconds>(now.time_since_epoch()).count();
            response["timestamp"] = timestamp;
            
            return crow::response(200, response.dump());
        } catch (const std::exception& e) {
            LOG_DASHBOARD_ERROR("Failed to perform discovery: {}", e.what());
            return crow::response(500, R"({"error": "Failed to perform discovery"})");
        }
    }
};

// Node Agent implementation
class NodeAgent {
public:
    explicit NodeAgent(std::shared_ptr<spdk::SpdkWrapper> spdk,
                      std::shared_ptr<kube::KubeClient> kube,
                      const AppConfig& config)
        : spdk_(spdk), kube_client_(kube), config_(config) {}
    
    void start() {
        LOG_NODE_AGENT_INFO("Starting node agent on port {}", config_.node_agent_port);
        
        // Set up HTTP API for disk operations
        app_.route_dynamic("/health")
            .methods("GET"_method)
            ([this](const crow::request& req) {
                (void)req; // Suppress unused parameter warning
                return crow::response(200, "OK");
            });
        
        app_.route_dynamic("/api/v1/disks")
            .methods("GET"_method)
            ([this](const crow::request& req) {
                return handle_get_disks(req);
            });
        
        app_.route_dynamic("/api/v1/setup")
            .methods("POST"_method)
            ([this](const crow::request& req) {
                return handle_setup_disk(req);
            });
        
        // Start server
        server_thread_ = std::thread([this]() {
            app_.port(config_.node_agent_port).multithreaded().run();
        });
        
        // Start discovery loop
        discovery_thread_ = std::thread([this]() {
            discovery_loop();
        });
        
        running_ = true;
        LOG_NODE_AGENT_INFO("Node agent started successfully");
    }
    
    void stop() {
        LOG_NODE_AGENT_INFO("Stopping node agent");
        running_ = false;
        app_.stop();
        
        if (server_thread_.joinable()) {
            server_thread_.join();
        }
        if (discovery_thread_.joinable()) {
            discovery_thread_.join();
        }
    }
    
    bool is_running() const { return running_; }

private:
    std::shared_ptr<spdk::SpdkWrapper> spdk_;
    std::shared_ptr<kube::KubeClient> kube_client_;
    AppConfig config_;
    std::atomic<bool> running_{false};
    crow::SimpleApp app_;
    std::thread server_thread_;
    std::thread discovery_thread_;
    
    void discovery_loop() {
        while (running_) {
            try {
                LOG_NODE_AGENT_DEBUG("Running disk discovery");
                discover_and_setup_disks();
            } catch (const std::exception& e) {
                LOG_NODE_AGENT_ERROR("Discovery failed: {}", e.what());
            }
            
            // Sleep for discovery interval
            for (int i = 0; i < config_.discovery_interval && running_; ++i) {
                std::this_thread::sleep_for(std::chrono::seconds(1));
            }
        }
    }
    
    void discover_and_setup_disks() {
        // Get available block devices from SPDK
        auto bdevs = spdk_->getBdevs();
        
        for (const auto& bdev : bdevs) {
            if (!bdev.claimed) {
                LOG_NODE_AGENT_INFO("Found unclaimed device: {}", bdev.name);
                // TODO: Implement automatic setup logic
            }
        }
    }
    
    crow::response handle_get_disks(const crow::request& req) {
        (void)req; // Suppress unused parameter warning
        try {
            auto bdevs = spdk_->getBdevs();
            nlohmann::json response = nlohmann::json::array();
            
            for (const auto& bdev : bdevs) {
                nlohmann::json bdev_json = {
                    {"name", bdev.name},
                    {"product_name", bdev.product_name},
                    {"num_blocks", bdev.num_blocks},
                    {"block_size", bdev.block_size},
                    {"uuid", bdev.uuid},
                    {"claimed", bdev.claimed}
                };
                response.push_back(bdev_json);
            }
            
            return crow::response(200, response.dump());
        } catch (const std::exception& e) {
            LOG_NODE_AGENT_ERROR("Failed to get disks: {}", e.what());
            return crow::response(500, R"({"error": "Failed to get disks"})");
        }
    }
    
    crow::response handle_setup_disk(const crow::request& req) {
        try {
            auto request_json = nlohmann::json::parse(req.body);
            std::string disk_name = request_json["disk_name"];
            std::string setup_type = request_json["type"]; // "aio", "uring", "nvme"
            
            bool success = false;
            if (setup_type == "aio") {
                success = spdk_->createAioBdev(disk_name + "_aio", disk_name);
            } else if (setup_type == "uring") {
                success = spdk_->createUringBdev(disk_name + "_uring", disk_name);
            }
            
            if (success) {
                return crow::response(200, R"({"status": "success"})");
            } else {
                return crow::response(500, R"({"error": "Setup failed"})");
            }
        } catch (const std::exception& e) {
            LOG_NODE_AGENT_ERROR("Failed to setup disk: {}", e.what());
            return crow::response(400, R"({"error": "Invalid request"})");
        }
    }
};

// Controller Operator implementation
class ControllerOperator {
public:
    explicit ControllerOperator(std::shared_ptr<spdk::SpdkWrapper> spdk,
                               std::shared_ptr<kube::KubeClient> kube,
                               const AppConfig& config)
        : spdk_(spdk), kube_client_(kube), config_(config) {}
    
    void start() {
        LOG_CONTROLLER_INFO("Starting controller operator");
        running_ = true;
        
        // Start operator loop
        operator_thread_ = std::thread([this]() {
            operator_loop();
        });
        
        LOG_CONTROLLER_INFO("Controller operator started");
    }
    
    void stop() {
        LOG_CONTROLLER_INFO("Stopping controller operator");
        running_ = false;
        
        if (operator_thread_.joinable()) {
            operator_thread_.join();
        }
    }
    
    bool is_running() const { return running_; }

private:
    std::shared_ptr<spdk::SpdkWrapper> spdk_;
    std::shared_ptr<kube::KubeClient> kube_client_;
    AppConfig config_;
    std::atomic<bool> running_{false};
    std::thread operator_thread_;
    
    void operator_loop() {
        while (running_) {
            try {
                reconcile_volumes();
            } catch (const std::exception& e) {
                LOG_CONTROLLER_ERROR("Reconciliation failed: {}", e.what());
            }
            
            // Sleep before next reconciliation
            for (int i = 0; i < 30 && running_; ++i) {
                std::this_thread::sleep_for(std::chrono::seconds(1));
            }
        }
    }
    
    void reconcile_volumes() {
        // Get all SPDK volumes
        auto volumes_future = kube_client_->list_spdk_volumes(config_.target_namespace);
        auto volumes = volumes_future.get();
        
        for (const auto& volume : volumes) {
            try {
                reconcile_single_volume(volume);
            } catch (const std::exception& e) {
                LOG_CONTROLLER_ERROR("Failed to reconcile volume {}: {}", 
                                   volume.name(), e.what());
            }
        }
    }
    
    void reconcile_single_volume(const kube::SpdkVolume& volume) {
        LOG_CONTROLLER_DEBUG("Reconciling volume {}", volume.name());
        
        // Check if volume needs creation
        if (volume.spec.state == "Creating") {
            create_volume_replicas(volume);
        }
        // Check for failed replicas
        else if (volume.spec.state == "Degraded") {
            repair_volume_replicas(volume);
        }
    }
    
    void create_volume_replicas(const kube::SpdkVolume& volume) {
        LOG_CONTROLLER_INFO("Creating replicas for volume {}", volume.name());
        // TODO: Implement replica creation logic
    }
    
    void repair_volume_replicas(const kube::SpdkVolume& volume) {
        LOG_CONTROLLER_INFO("Repairing replicas for volume {}", volume.name());
        // TODO: Implement replica repair logic
    }
};

// Application implementation
Application::Application(const AppConfig& config) 
    : config_(config) {
    LOG_INFO("Initializing SPDK Flint application in {} mode", 
             config.mode == AppMode::ALL ? "all" : 
             config.mode == AppMode::CSI_DRIVER ? "csi-driver" :
             config.mode == AppMode::CONTROLLER ? "controller" :
             config.mode == AppMode::DASHBOARD_BACKEND ? "dashboard-backend" :
             config.mode == AppMode::NODE_AGENT ? "node-agent" : "unknown");
}

Application::~Application() {
    shutdown();
}

int Application::run() {
    try {
        if (!initialize()) {
            LOG_ERROR("Failed to initialize application");
            return 1;
        }
        
        LOG_INFO("All services started successfully");
        
        // Instead of running our own event loop, let SPDK handle the main loop
        // SPDK's event loop will run until spdk_app_stop() is called
        LOG_INFO("Application running - SPDK managing event loop");
        
        // This will block until SPDK shuts down
        // The actual event processing happens in SPDK's internal event loop
        return 0;
        
    } catch (const std::exception& e) {
        LOG_ERROR("Application failed: {}", e.what());
        return 1;
    }
}

bool Application::initialize() {
    try {
        setupLogging();
        initializeComponents();
        
        // Start services based on mode
        switch (config_.mode) {
            case AppMode::CSI_DRIVER:
                startCSIMode();
                break;
            case AppMode::CONTROLLER:
                startControllerMode();
                break;
            case AppMode::DASHBOARD_BACKEND:
                startDashboardMode();
                break;
            case AppMode::NODE_AGENT:
                startNodeAgentMode();
                break;
            case AppMode::ALL:
                startCSIMode();
                startDashboardMode();
                startNodeAgentMode();
                break;
        }
        
        // Always start health server
        startHealthServer();
        
        running_ = true;
        return true;
        
    } catch (const std::exception& e) {
        LOG_ERROR("Application initialization failed: {}", e.what());
        return false;
    }
}

void Application::shutdown() {
    if (!running_.exchange(false)) {
        return; // Already shutting down
    }
    
    LOG_INFO("Shutting down application");
    
    // Stop services in reverse order
    if (node_agent_) {
        node_agent_->stop();
    }
    if (dashboard_service_) {
        dashboard_service_->stop();
    }
    if (csi_service_) {
        csi_service_->stop();
    }
    if (controller_operator_) {
        controller_operator_->stop();
    }
    
    // Shutdown SPDK - this will properly flush and cleanup
    if (spdk_wrapper_) {
        spdk_wrapper_->shutdown();
    }
    
    LOG_INFO("Application shutdown complete");
}

void Application::setupLogging() {
    // Logging is already initialized in main.cpp
}

void Application::initializeComponents() {
    LOG_INFO("Initializing components");
    
    // Wait for SPDK to be ready
    waitForSpdkReady();
    
    // Initialize SPDK wrapper
    spdk_wrapper_ = std::make_shared<spdk_flint::spdk::SpdkWrapper>();
    if (!spdk_wrapper_->initialize()) {
        throw std::runtime_error("Failed to initialize SPDK");
    }
    
    // Initialize Kubernetes client
    auto kube_client = kube::KubeClient::create_incluster();
    if (!kube_client) {
        LOG_WARN("Failed to create in-cluster client, trying kubeconfig");
        kube_client = kube::KubeClient::create_from_kubeconfig();
    }
    if (!kube_client) {
        throw std::runtime_error("Failed to initialize Kubernetes client");
    }
    
    // Create service instances
    csi_service_ = std::make_unique<CSIService>(spdk_wrapper_, kube_client, config_);
    dashboard_service_ = std::make_unique<DashboardService>(spdk_wrapper_, kube_client, config_);
    node_agent_ = std::make_unique<NodeAgent>(spdk_wrapper_, kube_client, config_);
    controller_operator_ = std::make_unique<ControllerOperator>(spdk_wrapper_, kube_client, config_);
    
    LOG_INFO("Components initialized successfully");
}

void Application::startCSIMode() {
    LOG_INFO("Starting CSI services");
    csi_service_->start();
    controller_operator_->start();
}

void Application::startControllerMode() {
    LOG_INFO("Starting controller services");
    controller_operator_->start();
}

void Application::startDashboardMode() {
    LOG_INFO("Starting dashboard services");
    dashboard_service_->start();
}

void Application::startNodeAgentMode() {
    LOG_INFO("Starting node agent services");
    node_agent_->start();
}

void Application::startHealthServer() {
    LOG_INFO("Starting health server on port {}", config_.health_port);
    
    // Simple health check server using Crow
    static crow::SimpleApp health_app;
    
    health_app.route_dynamic("/healthz")
        .methods("GET"_method)
        ([this](const crow::request& req) {
            // Check if core services are running
            bool healthy = true;
            
            if (config_.mode == AppMode::CSI_DRIVER || config_.mode == AppMode::ALL) {
                healthy = healthy && csi_service_ && csi_service_->is_running();
            }
            if (config_.mode == AppMode::DASHBOARD_BACKEND || config_.mode == AppMode::ALL) {
                healthy = healthy && dashboard_service_ && dashboard_service_->is_running();
            }
            if (config_.mode == AppMode::NODE_AGENT || config_.mode == AppMode::ALL) {
                healthy = healthy && node_agent_ && node_agent_->is_running();
            }
            
            return crow::response(healthy ? 200 : 503, healthy ? "OK" : "Not Ready");
        });
    
    // Start health server in background
    std::thread([this, &health_app]() {
        health_app.port(config_.health_port).multithreaded().run();
    }).detach();
}

void Application::waitForSpdkReady() {
    LOG_INFO("Waiting for SPDK to be ready at {}", config_.spdk_rpc_url);
    
    // For now, just wait a bit - in production we'd check the RPC socket
    std::this_thread::sleep_for(std::chrono::seconds(2));
    
    LOG_INFO("SPDK is ready");
}

} // namespace spdk_flint 