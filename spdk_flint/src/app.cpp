#include "app.hpp"
#include "spdk/spdk_wrapper.hpp"
#include "utils/kube_client.hpp"
#include "utils/disk_manager.hpp"
#include <csignal>
#include <thread>
#include <chrono>
#include <future>
#include <crow.h>

namespace spdk_flint {

// ===== NODE AGENT SERVICE IMPLEMENTATION =====

class NodeAgentService {
public:
    explicit NodeAgentService(std::shared_ptr<spdk::SpdkWrapper> spdk, 
                             std::shared_ptr<kube::KubeClient> kube,
                             const AppConfig& config)
        : spdk_(spdk), kube_client_(kube), config_(config) {
        logger()->info("[NODE_AGENT] Creating Node Agent service");
        logger()->debug("[NODE_AGENT] Configuration: port={}, namespace='{}'", 
                       config_.node_agent_port, config_.target_namespace);
        
        // Initialize disk manager
        disk_manager_ = std::make_unique<DiskManager>(spdk_, config_.node_id, config_.target_namespace);
        logger()->debug("[NODE_AGENT] DiskManager initialized");
    }
    
    void start() {
        auto start_time = std::chrono::steady_clock::now();
        logger()->info("[NODE_AGENT] Starting SPDK Node Agent service on port {}", config_.node_agent_port);
        logger()->debug("[NODE_AGENT] Thread ID: {}", spdk_flint::current_thread_id());
        running_ = true;
        
        // Start HTTP API server for node agent operations
        startHttpServer();
        
        // Start disk discovery and monitoring loop
        startDiskMonitoring();
        
        auto duration = std::chrono::duration_cast<std::chrono::milliseconds>(
            std::chrono::steady_clock::now() - start_time);
        logger()->info("[NODE_AGENT] Node Agent service started successfully in {} ms", duration.count());
        logger()->debug("[NODE_AGENT] Services active: HTTP API server, disk monitoring");
    }
    
    void stop() {
        auto start_time = std::chrono::steady_clock::now();
        logger()->info("[NODE_AGENT] Stopping Node Agent service");
        running_ = false;
        
        if (http_server_thread_.joinable()) {
            logger()->debug("[NODE_AGENT] Stopping HTTP server thread");
            http_server_thread_.join();
        }
        
        if (disk_monitor_thread_.joinable()) {
            logger()->debug("[NODE_AGENT] Stopping disk monitoring thread");
            disk_monitor_thread_.join();
        }
        
        auto duration = std::chrono::duration_cast<std::chrono::milliseconds>(
            std::chrono::steady_clock::now() - start_time);
        logger()->info("[NODE_AGENT] Node Agent service stopped in {} ms", duration.count());
    }
    
    bool is_running() const { 
        bool running = running_;
        logger()->debug("[NODE_AGENT] Service status check: {}", running ? "running" : "stopped");
        return running;
    }

private:
    std::shared_ptr<spdk::SpdkWrapper> spdk_;
    std::shared_ptr<kube::KubeClient> kube_client_;
    std::unique_ptr<DiskManager> disk_manager_;
    AppConfig config_;
    std::atomic<bool> running_{false};
    std::thread http_server_thread_;
    std::thread disk_monitor_thread_;
    
    void startHttpServer() {
        logger()->info("[NODE_AGENT] Starting HTTP API server on port {}", config_.node_agent_port);
        
        http_server_thread_ = std::thread([this]() {
            try {
                                                  logger()->debug("[NODE_AGENT] HTTP server thread started: {}", spdk_flint::current_thread_id());
                 crow::SimpleApp app;
                 
                 // Note: CORS headers will be added manually in responses if needed
                
                // Disk discovery endpoint
                CROW_ROUTE(app, "/api/disks/uninitialized").methods("GET"_method)
                ([this](const crow::request& req) {
                    auto start_time = std::chrono::steady_clock::now();
                    logger()->info("[NODE_AGENT] HTTP GET /api/disks/uninitialized from {}", req.remote_ip_address);
                    auto response = handleGetUninitializedDisks();
                    auto duration = std::chrono::duration_cast<std::chrono::milliseconds>(
                        std::chrono::steady_clock::now() - start_time);
                    logger()->debug("[NODE_AGENT] GET /api/disks/uninitialized completed in {} ms (status: {})", 
                                   duration.count(), response.code);
                    return response;
                });
                
                // Disk setup endpoint
                CROW_ROUTE(app, "/api/disks/setup").methods("POST"_method)
                ([this](const crow::request& req) {
                    auto start_time = std::chrono::steady_clock::now();
                    logger()->info("[NODE_AGENT] HTTP POST /api/disks/setup from {} (body_size: {})", 
                                  req.remote_ip_address, req.body.size());
                    logger()->debug("[NODE_AGENT] Request body: {}", req.body.substr(0, 200) + (req.body.size() > 200 ? "..." : ""));
                    auto response = handleSetupDisks(req);
                    auto duration = std::chrono::duration_cast<std::chrono::milliseconds>(
                        std::chrono::steady_clock::now() - start_time);
                    logger()->debug("[NODE_AGENT] POST /api/disks/setup completed in {} ms (status: {})", 
                                   duration.count(), response.code);
                    return response;
                });
                
                // LVS operations
                CROW_ROUTE(app, "/api/lvs").methods("GET"_method)
                ([this](const crow::request& req) {
                    auto start_time = std::chrono::steady_clock::now();
                    logger()->info("[NODE_AGENT] HTTP GET /api/lvs from {}", req.remote_ip_address);
                    auto response = handleGetLvolStores();
                    auto duration = std::chrono::duration_cast<std::chrono::milliseconds>(
                        std::chrono::steady_clock::now() - start_time);
                    logger()->debug("[NODE_AGENT] GET /api/lvs completed in {} ms (status: {})", 
                                   duration.count(), response.code);
                    return response;
                });
                
                CROW_ROUTE(app, "/api/lvs").methods("POST"_method)
                ([this](const crow::request& req) {
                    auto start_time = std::chrono::steady_clock::now();
                    logger()->info("[NODE_AGENT] HTTP POST /api/lvs from {} (body_size: {})", 
                                  req.remote_ip_address, req.body.size());
                    logger()->debug("[NODE_AGENT] Request body: {}", req.body.substr(0, 200) + (req.body.size() > 200 ? "..." : ""));
                    auto response = handleCreateLvolStore(req);
                    auto duration = std::chrono::duration_cast<std::chrono::milliseconds>(
                        std::chrono::steady_clock::now() - start_time);
                    logger()->debug("[NODE_AGENT] POST /api/lvs completed in {} ms (status: {})", 
                                   duration.count(), response.code);
                    return response;
                });
                
                // Bdev operations
                CROW_ROUTE(app, "/api/bdevs").methods("GET"_method)
                ([this](const crow::request& req) {
                    auto start_time = std::chrono::steady_clock::now();
                    std::string filter = req.url_params.get("name") ? req.url_params.get("name") : "";
                    logger()->info("[NODE_AGENT] HTTP GET /api/bdevs from {} (filter: '{}')", 
                                  req.remote_ip_address, filter);
                    auto response = handleGetBdevs(filter);
                    auto duration = std::chrono::duration_cast<std::chrono::milliseconds>(
                        std::chrono::steady_clock::now() - start_time);
                    logger()->debug("[NODE_AGENT] GET /api/bdevs completed in {} ms (status: {})", 
                                   duration.count(), response.code);
                    return response;
                });
                
                // Health check
                CROW_ROUTE(app, "/health").methods("GET"_method)
                ([this](const crow::request& req) {
                    logger()->debug("[NODE_AGENT] HTTP GET /health from {}", req.remote_ip_address);
                    bool healthy = spdk_ && spdk_->isInitialized() && running_;
                    if (healthy) {
                        logger()->debug("[NODE_AGENT] Health check: OK");
                        return crow::response(200, "OK");
                    } else {
                        logger()->warn("[NODE_AGENT] Health check: FAIL (spdk_initialized={}, running={})", 
                                      spdk_ ? spdk_->isInitialized() : false, running_.load());
                        return crow::response(503, "Service Unavailable");
                    }
                });
                
                // Status endpoint with detailed information
                CROW_ROUTE(app, "/api/status").methods("GET"_method)
                ([this](const crow::request& req) {
                    logger()->debug("[NODE_AGENT] HTTP GET /api/status from {}", req.remote_ip_address);
                    return handleGetStatus();
                });
                
                logger()->info("[NODE_AGENT] HTTP server listening on port {}", config_.node_agent_port);
                app.port(config_.node_agent_port).multithreaded().run();
                
            } catch (const std::exception& e) {
                logger()->error("[NODE_AGENT] HTTP server thread exception: {}", e.what());
            }
            logger()->debug("[NODE_AGENT] HTTP server thread exiting");
        });
    }
    
    void startDiskMonitoring() {
        logger()->info("[NODE_AGENT] Starting disk monitoring (interval: {} seconds)", config_.discovery_interval);
        
        disk_monitor_thread_ = std::thread([this]() {
            try {
                                 logger()->debug("[NODE_AGENT] Disk monitoring thread started: {}", spdk_flint::current_thread_id());
                int cycle_count = 0;
                
                while (running_) {
                    try {
                        auto start_time = std::chrono::steady_clock::now();
                        cycle_count++;
                        
                        logger()->debug("[NODE_AGENT] Starting monitoring cycle #{}", cycle_count);
                        
                        // Periodic disk discovery and health monitoring
                        performDiskDiscovery();
                        updateDiskHealth();
                        
                        auto duration = std::chrono::duration_cast<std::chrono::milliseconds>(
                            std::chrono::steady_clock::now() - start_time);
                        logger()->debug("[NODE_AGENT] Monitoring cycle #{} completed in {} ms", cycle_count, duration.count());
                        
                        // Sleep for monitoring interval
                        for (uint32_t i = 0; i < config_.discovery_interval && running_; ++i) {
                            std::this_thread::sleep_for(std::chrono::seconds(1));
                        }
                        
                    } catch (const std::exception& e) {
                        logger()->error("[NODE_AGENT] Disk monitoring cycle #{} error: {}", cycle_count, e.what());
                        // Continue monitoring despite errors
                        std::this_thread::sleep_for(std::chrono::seconds(10));
                    }
                }
            } catch (const std::exception& e) {
                logger()->error("[NODE_AGENT] Disk monitoring thread exception: {}", e.what());
            }
            logger()->debug("[NODE_AGENT] Disk monitoring thread exiting");
        });
    }
    
    crow::response handleGetUninitializedDisks() {
        logger()->debug("[NODE_AGENT] Processing uninitialized disks request using DiskManager");
        
        try {
            // Use a future to make async call synchronous for HTTP response
            std::promise<crow::response> response_promise;
            auto response_future = response_promise.get_future();
            
            // Call DiskManager to discover all disks
            disk_manager_->discoverAllDisksAsync([this, &response_promise](const std::vector<DiskInfo>& disks, int error) {
                crow::json::wvalue result;
                
                if (error != 0) {
                    logger()->error("[NODE_AGENT] DiskManager discovery failed: {}", strerror(-error));
                    result["success"] = false;
                    result["error"] = strerror(-error);
                    result["count"] = 0;
                    result["disks"] = crow::json::load("[]");
                    result["timestamp"] = std::chrono::duration_cast<std::chrono::seconds>(
                        std::chrono::system_clock::now().time_since_epoch()).count();
                    response_promise.set_value(crow::response(500, result));
                    return;
                }
                
                logger()->info("[NODE_AGENT] DiskManager discovered {} disks", disks.size());
                
                result["success"] = true;
                result["disks"] = crow::json::load("[]");
                result["node"] = config_.node_id;
                
                // Convert discovered disks to JSON
                for (size_t i = 0; i < disks.size(); i++) {
                    const auto& disk = disks[i];
                    crow::json::wvalue disk_json;
                    
                    disk_json["pci_address"] = disk.pci_address;
                    disk_json["device_name"] = disk.device_name;
                    disk_json["driver"] = disk.driver;
                    disk_json["size_bytes"] = disk.size_bytes;
                    disk_json["size_mb"] = disk.size_bytes / (1024 * 1024);
                    disk_json["size_gb"] = disk.size_bytes / (1024 * 1024 * 1024);
                    disk_json["model"] = disk.model;
                    disk_json["vendor_id"] = disk.vendor_id;
                    disk_json["device_id"] = disk.device_id;
                    disk_json["is_system_disk"] = disk.is_system_disk;
                    disk_json["spdk_ready"] = disk.spdk_ready;
                    
                    // Add mounted partitions array
                    disk_json["mounted_partitions"] = crow::json::load("[]");
                    for (size_t j = 0; j < disk.mounted_partitions.size(); j++) {
                        disk_json["mounted_partitions"][j] = disk.mounted_partitions[j];
                    }
                    
                    result["disks"][i] = std::move(disk_json);
                    
                    logger()->debug("[NODE_AGENT] Disk {}: PCI={}, Name={}, Driver={}, System={}, SPDK Ready={}, Size={}GB", 
                                   i+1, disk.pci_address, disk.device_name, disk.driver, 
                                   disk.is_system_disk, disk.spdk_ready, disk.size_bytes / (1024*1024*1024));
                }
                
                result["count"] = disks.size();
                result["timestamp"] = std::chrono::duration_cast<std::chrono::seconds>(
                    std::chrono::system_clock::now().time_since_epoch()).count();
                
                logger()->info("[NODE_AGENT] Returning {} discovered disks", disks.size());
                response_promise.set_value(crow::response(200, result));
            });
            
            // Wait for the async operation to complete (with timeout)
            auto status = response_future.wait_for(std::chrono::seconds(30));
            if (status == std::future_status::timeout) {
                logger()->error("[NODE_AGENT] Disk discovery timed out after 30 seconds");
                crow::json::wvalue error;
                error["success"] = false;
                error["error"] = "Disk discovery timed out";
                error["timestamp"] = std::chrono::duration_cast<std::chrono::seconds>(
                    std::chrono::system_clock::now().time_since_epoch()).count();
                return crow::response(504, error);
            }
            
            return response_future.get();
            
        } catch (const std::exception& e) {
            logger()->error("[NODE_AGENT] Exception in handleGetUninitializedDisks: {}", e.what());
            crow::json::wvalue error;
            error["success"] = false;
            error["error"] = e.what();
            error["timestamp"] = std::chrono::duration_cast<std::chrono::seconds>(
                std::chrono::system_clock::now().time_since_epoch()).count();
            return crow::response(500, error);
        }
    }
    
    crow::response handleSetupDisks(const crow::request& req) {
        logger()->debug("[NODE_AGENT] Processing disk setup request");
        
        try {
            auto body = crow::json::load(req.body);
            if (!body) {
                logger()->error("[NODE_AGENT] Disk setup failed: invalid JSON in request body");
                return crow::response(400, "Invalid JSON");
            }
            
            std::vector<std::string> pci_addresses;
            if (body.has("pci_addresses")) {
                for (const auto& addr : body["pci_addresses"]) {
                    pci_addresses.push_back(addr.s());
                }
            }
            
            logger()->info("[NODE_AGENT] Setting up {} PCI devices", pci_addresses.size());
            for (const auto& addr : pci_addresses) {
                logger()->debug("[NODE_AGENT] PCI address to setup: {}", addr);
            }
            
            // TODO: Implement disk setup logic using SPDK direct calls
            // This would involve:
            // 1. Validate PCI addresses
            // 2. Attach NVMe controllers if needed
            // 3. Create appropriate bdevs
            // 4. Update Kubernetes custom resources
            
            crow::json::wvalue result;
            result["success"] = true;
            result["setup_disks"] = crow::json::load("[]");
            result["message"] = "Disk setup not yet implemented";
            result["pci_addresses_requested"] = pci_addresses.size();
            result["timestamp"] = std::chrono::duration_cast<std::chrono::seconds>(
                std::chrono::system_clock::now().time_since_epoch()).count();
            
            logger()->warn("[NODE_AGENT] Disk setup not yet fully implemented - returning placeholder");
            return crow::response(200, result);
            
        } catch (const std::exception& e) {
            logger()->error("[NODE_AGENT] Error setting up disks: {}", e.what());
            crow::json::wvalue error;
            error["error"] = e.what();
            error["timestamp"] = std::chrono::duration_cast<std::chrono::seconds>(
                std::chrono::system_clock::now().time_since_epoch()).count();
            return crow::response(500, error);
        }
    }
    
    crow::response handleGetLvolStores() {
        logger()->debug("[NODE_AGENT] Processing LVol stores request");
        
        try {
            auto lvs_stores = spdk_->getLvolStores();
            logger()->debug("[NODE_AGENT] Retrieved {} LVol stores", lvs_stores.size());
            
            crow::json::wvalue result;
            result["lvol_stores"] = crow::json::load("[]");
            
            uint64_t total_capacity = 0;
            uint64_t total_used = 0;
            
            for (const auto& lvs : lvs_stores) {
                crow::json::wvalue store;
                store["uuid"] = lvs.uuid;
                store["name"] = lvs.name;
                store["base_bdev"] = lvs.base_bdev;
                store["total_clusters"] = lvs.total_clusters;
                store["free_clusters"] = lvs.free_clusters;
                store["used_clusters"] = lvs.total_clusters - lvs.free_clusters;
                store["cluster_size"] = lvs.cluster_size;
                store["block_size"] = lvs.block_size;
                
                uint64_t total_size = lvs.total_clusters * lvs.cluster_size;
                uint64_t used_size = (lvs.total_clusters - lvs.free_clusters) * lvs.cluster_size;
                double usage_percent = lvs.total_clusters > 0 ? 
                    100.0 * (lvs.total_clusters - lvs.free_clusters) / lvs.total_clusters : 0.0;
                
                store["total_size_bytes"] = total_size;
                store["used_size_bytes"] = used_size;
                store["total_size_mb"] = total_size / (1024 * 1024);
                store["used_size_mb"] = used_size / (1024 * 1024);
                store["usage_percent"] = usage_percent;
                
                total_capacity += total_size;
                total_used += used_size;
                
                result["lvol_stores"][result["lvol_stores"].size()] = std::move(store);
                
                logger()->debug("[NODE_AGENT] LVS: {} ({:.1f}% used, {} MB total)", 
                               lvs.name, usage_percent, total_size / (1024 * 1024));
            }
            
            result["count"] = lvs_stores.size();
            result["total_capacity_mb"] = total_capacity / (1024 * 1024);
            result["total_used_mb"] = total_used / (1024 * 1024);
            result["overall_usage_percent"] = total_capacity > 0 ? 100.0 * total_used / total_capacity : 0.0;
            result["timestamp"] = std::chrono::duration_cast<std::chrono::seconds>(
                std::chrono::system_clock::now().time_since_epoch()).count();
            
            logger()->info("[NODE_AGENT] Returned {} LVol stores (total: {} MB, used: {:.1f}%)", 
                          lvs_stores.size(), total_capacity / (1024 * 1024), 
                          total_capacity > 0 ? 100.0 * total_used / total_capacity : 0.0);
            return crow::response(200, result);
            
        } catch (const std::exception& e) {
            logger()->error("[NODE_AGENT] Error getting LVol stores: {}", e.what());
            crow::json::wvalue error;
            error["error"] = e.what();
            error["timestamp"] = std::chrono::duration_cast<std::chrono::seconds>(
                std::chrono::system_clock::now().time_since_epoch()).count();
            return crow::response(500, error);
        }
    }
    
    crow::response handleCreateLvolStore(const crow::request& req) {
        logger()->debug("[NODE_AGENT] Processing LVol store creation request");
        
        try {
            auto body = crow::json::load(req.body);
            if (!body) {
                logger()->error("[NODE_AGENT] LVS creation failed: invalid JSON in request body");
                return crow::response(400, "Invalid JSON");
            }
            
            std::string bdev_name;
            std::string lvs_name;
            std::string clear_method = "unmap";  // default
            
            if (body.has("bdev_name")) {
                bdev_name = body["bdev_name"].s();
            }
            if (body.has("lvs_name")) {
                lvs_name = body["lvs_name"].s();
            }
            if (body.has("clear_method")) {
                clear_method = body["clear_method"].s();
            }
            uint32_t cluster_sz = body.has("cluster_sz") ? body["cluster_sz"].u() : 0;
            
            logger()->info("[NODE_AGENT] Creating LVS: name='{}', bdev='{}', clear='{}', cluster={}",
                          lvs_name, bdev_name, clear_method, cluster_sz);
            
            // Validate required parameters
            if (bdev_name.empty()) {
                logger()->error("[NODE_AGENT] LVS creation failed: missing bdev_name");
                return crow::response(400, "Missing required parameter: bdev_name");
            }
            if (lvs_name.empty()) {
                logger()->error("[NODE_AGENT] LVS creation failed: missing lvs_name");
                return crow::response(400, "Missing required parameter: lvs_name");
            }
            
            // Use async SPDK call with promise/future for synchronous HTTP response
            std::promise<std::pair<std::string, int>> promise;
            auto future = promise.get_future();
            
            auto request_start = std::chrono::steady_clock::now();
            
            spdk_->createLvolStoreAsync(bdev_name, lvs_name, clear_method, cluster_sz,
                [&promise, request_start](const std::string& uuid, int error) {
                    auto duration = std::chrono::duration_cast<std::chrono::milliseconds>(
                        std::chrono::steady_clock::now() - request_start);
                    spdk_flint::logger()->debug("[NODE_AGENT] LVS creation callback received after {} ms", duration.count());
                    promise.set_value({uuid, error});
                });
            
            logger()->debug("[NODE_AGENT] Waiting for LVS creation to complete...");
            auto result_pair = future.get();
            
            auto total_duration = std::chrono::duration_cast<std::chrono::milliseconds>(
                std::chrono::steady_clock::now() - request_start);
            
            if (result_pair.second == 0) {
                crow::json::wvalue result;
                result["uuid"] = result_pair.first;
                result["success"] = true;
                result["lvs_name"] = lvs_name;
                result["bdev_name"] = bdev_name;
                result["clear_method"] = clear_method;
                result["cluster_size"] = cluster_sz;
                result["creation_time_ms"] = total_duration.count();
                result["timestamp"] = std::chrono::duration_cast<std::chrono::seconds>(
                    std::chrono::system_clock::now().time_since_epoch()).count();
                
                logger()->info("[NODE_AGENT] Successfully created LVS '{}' with UUID '{}' in {} ms", 
                              lvs_name, result_pair.first, total_duration.count());
                return crow::response(200, result);
            } else {
                crow::json::wvalue error;
                error["error"] = "Failed to create LVol store";
                error["errno"] = result_pair.second;
                error["error_message"] = strerror(-result_pair.second);
                error["lvs_name"] = lvs_name;
                error["bdev_name"] = bdev_name;
                error["creation_time_ms"] = total_duration.count();
                error["timestamp"] = std::chrono::duration_cast<std::chrono::seconds>(
                    std::chrono::system_clock::now().time_since_epoch()).count();
                
                logger()->error("[NODE_AGENT] Failed to create LVS '{}' after {} ms: {} ({})", 
                               lvs_name, total_duration.count(), result_pair.second, strerror(-result_pair.second));
                return crow::response(500, error);
            }
        } catch (const std::exception& e) {
            logger()->error("[NODE_AGENT] Error creating LVol store: {}", e.what());
            crow::json::wvalue error;
            error["error"] = e.what();
            error["timestamp"] = std::chrono::duration_cast<std::chrono::seconds>(
                std::chrono::system_clock::now().time_since_epoch()).count();
            return crow::response(500, error);
        }
    }
    
    crow::response handleGetBdevs(const std::string& filter = "") {
        logger()->debug("[NODE_AGENT] Processing bdevs request (filter: '{}')", filter);
        
        try {
            auto bdevs = spdk_->getBdevs(filter);
            logger()->debug("[NODE_AGENT] Retrieved {} block devices", bdevs.size());
            
            crow::json::wvalue result;
            result["bdevs"] = crow::json::load("[]");
            
            uint64_t total_storage = 0;
            int claimed_count = 0;
            
            for (const auto& bdev : bdevs) {
                crow::json::wvalue device;
                device["name"] = bdev.name;
                device["uuid"] = bdev.uuid;
                device["product_name"] = bdev.product_name;
                device["block_size"] = bdev.block_size;
                device["num_blocks"] = bdev.num_blocks;
                device["claimed"] = bdev.claimed;
                
                uint64_t size_bytes = bdev.num_blocks * bdev.block_size;
                device["size_bytes"] = size_bytes;
                device["size_mb"] = size_bytes / (1024 * 1024);
                device["size_gb"] = size_bytes / (1024 * 1024 * 1024);
                
                total_storage += size_bytes;
                if (bdev.claimed) claimed_count++;
                
                result["bdevs"][result["bdevs"].size()] = std::move(device);
                
                logger()->debug("[NODE_AGENT] bdev: {} ({} MB, claimed: {})", 
                               bdev.name, size_bytes / (1024 * 1024), bdev.claimed);
            }
            
            result["count"] = bdevs.size();
            result["claimed_count"] = claimed_count;
            result["unclaimed_count"] = bdevs.size() - claimed_count;
            result["total_storage_mb"] = total_storage / (1024 * 1024);
            result["total_storage_gb"] = total_storage / (1024 * 1024 * 1024);
            result["filter"] = filter;
            result["timestamp"] = std::chrono::duration_cast<std::chrono::seconds>(
                std::chrono::system_clock::now().time_since_epoch()).count();
            
            logger()->info("[NODE_AGENT] Returned {} block devices (total: {} GB, claimed: {}, unclaimed: {})", 
                          bdevs.size(), total_storage / (1024 * 1024 * 1024), claimed_count, bdevs.size() - claimed_count);
            return crow::response(200, result);
            
        } catch (const std::exception& e) {
            logger()->error("[NODE_AGENT] Error getting bdevs: {}", e.what());
            crow::json::wvalue error;
            error["error"] = e.what();
            error["filter"] = filter;
            error["timestamp"] = std::chrono::duration_cast<std::chrono::seconds>(
                std::chrono::system_clock::now().time_since_epoch()).count();
            return crow::response(500, error);
        }
    }
    
    crow::response handleGetStatus() {
        logger()->debug("[NODE_AGENT] Processing status request");
        
        try {
            crow::json::wvalue status;
            
            // Service status
            status["service"]["running"] = running_.load();
            status["service"]["name"] = "spdk-flint-node-agent";
            status["service"]["version"] = "1.0.0";
            status["service"]["node_id"] = config_.node_id;
            status["service"]["namespace"] = config_.target_namespace;
            status["service"]["port"] = config_.node_agent_port;
            
            // SPDK status
            bool spdk_initialized = spdk_ && spdk_->isInitialized();
            status["spdk"]["initialized"] = spdk_initialized;
            status["spdk"]["version"] = spdk_ ? spdk_->getVersion() : "unknown";
            
            // Resource counts
            if (spdk_initialized) {
                try {
                    auto bdevs = spdk_->getBdevs();
                    auto lvs_stores = spdk_->getLvolStores();
                    
                    status["resources"]["bdevs"]["total"] = bdevs.size();
                    status["resources"]["bdevs"]["claimed"] = std::count_if(bdevs.begin(), bdevs.end(), 
                        [](const auto& bdev) { return bdev.claimed; });
                    status["resources"]["lvol_stores"]["total"] = lvs_stores.size();
                    
                    uint64_t total_storage = 0;
                    for (const auto& bdev : bdevs) {
                        total_storage += bdev.num_blocks * bdev.block_size;
                    }
                    status["resources"]["total_storage_gb"] = total_storage / (1024 * 1024 * 1024);
                    
                } catch (const std::exception& e) {
                    logger()->warn("[NODE_AGENT] Could not get resource counts: {}", e.what());
                    status["resources"]["error"] = e.what();
                }
            }
            
            // Uptime
            static auto start_time = std::chrono::steady_clock::now();
            auto uptime = std::chrono::duration_cast<std::chrono::seconds>(
                std::chrono::steady_clock::now() - start_time);
            status["uptime_seconds"] = uptime.count();
            
            status["timestamp"] = std::chrono::duration_cast<std::chrono::seconds>(
                std::chrono::system_clock::now().time_since_epoch()).count();
            
            logger()->debug("[NODE_AGENT] Status: running={}, spdk_initialized={}, uptime={}s", 
                           running_.load(), spdk_initialized, uptime.count());
            
            return crow::response(200, status);
            
        } catch (const std::exception& e) {
            logger()->error("[NODE_AGENT] Error getting status: {}", e.what());
            crow::json::wvalue error;
            error["error"] = e.what();
            error["timestamp"] = std::chrono::duration_cast<std::chrono::seconds>(
                std::chrono::system_clock::now().time_since_epoch()).count();
            return crow::response(500, error);
        }
    }
    
    void performDiskDiscovery() {
        logger()->debug("[NODE_AGENT] Starting disk discovery cycle");
        
        // Use async SPDK calls to discover new devices
        spdk_->getBdevsAsync("", 0, [this](const std::vector<spdk::BdevInfo>& bdevs, int error) {
            if (error == 0) {
                logger()->debug("[NODE_AGENT] Discovered {} block devices", bdevs.size());
                
                // Count device types for monitoring
                int nvme_count = 0, aio_count = 0, uring_count = 0, other_count = 0;
                uint64_t total_capacity = 0;
                
                for (const auto& bdev : bdevs) {
                    uint64_t size = bdev.num_blocks * bdev.block_size;
                    total_capacity += size;
                    
                    if (bdev.name.find("nvme") != std::string::npos) nvme_count++;
                    else if (bdev.name.find("aio") != std::string::npos) aio_count++;
                    else if (bdev.name.find("uring") != std::string::npos) uring_count++;
                    else other_count++;
                }
                
                logger()->info("[NODE_AGENT] Discovery summary: {} devices ({} GB total), "
                              "NVMe: {}, AIO: {}, uring: {}, other: {}",
                              bdevs.size(), total_capacity / (1024 * 1024 * 1024),
                              nvme_count, aio_count, uring_count, other_count);
                
                // TODO: Update Kubernetes custom resources with discovered devices
                
            } else {
                logger()->error("[NODE_AGENT] Error during disk discovery: {} ({})", error, strerror(-error));
            }
        });
    }
    
    void updateDiskHealth() {
        logger()->debug("[NODE_AGENT] Updating disk health status");
        
        // TODO: Implement health monitoring using SPDK callbacks
        // This would include:
        // - Check I/O error rates
        // - Monitor temperature if available
        // - Verify connectivity to NVMe devices
        // - Update health status in Kubernetes resources
        
        static int health_check_count = 0;
        health_check_count++;
        
        if (health_check_count % 10 == 0) {  // Log every 10th health check
            logger()->info("[NODE_AGENT] Health monitoring active (check #{})", health_check_count);
        }
    }
};

// ===== APPLICATION IMPLEMENTATION =====

Application::Application(const AppConfig& config) 
    : config_(config), running_(false) {
    logger()->info("[APP] Creating SPDK Flint Application in Node Agent mode");
    logger()->info("[APP] Configuration summary - Mode: node-agent, Log Level: {}, Node ID: '{}'", 
                  config.log_level, config.node_id);
    logger()->debug("[APP] Detailed config - Health port: {}, Node agent port: {}, Target namespace: '{}'",
                   config.health_port, config.node_agent_port, config.target_namespace);
    logger()->debug("[APP] SPDK config - Discovery interval: {}s, Auto init blobstore: {}, Backup path: '{}'",
                   config.discovery_interval, config.auto_initialize_blobstore, config.backup_path);
}

Application::~Application() {
    logger()->debug("[APP] Destroying application");
    shutdown();
}

int Application::run() {
    auto app_start_time = std::chrono::steady_clock::now();
    
    try {
        logger()->info("[APP] Starting SPDK Flint Node Agent application");
        
        if (!initialize()) {
            logger()->error("[APP] Failed to initialize application");
            return 1;
        }
        
        auto init_duration = std::chrono::duration_cast<std::chrono::milliseconds>(
            std::chrono::steady_clock::now() - app_start_time);
        logger()->info("[APP] SPDK Flint Node Agent started successfully in {} ms", init_duration.count());
        logger()->info("[APP] Application running - SPDK managing event loop");
        logger()->debug("[APP] Services active: Node Agent HTTP API, Health server, Disk monitoring");
        
        // This will block until SPDK shuts down
        // The actual event processing happens in SPDK's internal event loop
        return 0;
        
    } catch (const std::exception& e) {
        logger()->error("[APP] Application failed: {}", e.what());
        return 1;
    }
}

bool Application::initialize() {
    auto start_time = std::chrono::steady_clock::now();
    
    try {
        logger()->info("[APP] Initializing SPDK Flint Node Agent components");
        
        setupLogging();
        initializeComponents();
        
        // Only start node agent mode since spdk_flint is node-agent only
        if (config_.mode == AppMode::NODE_AGENT) {
            startNodeAgentMode();
        } else {
            logger()->error("[APP] spdk_flint only supports node-agent mode. Other services should use Rust RPC clients.");
            return false;
        }
        
        // Start health server
        startHealthServer();
        
        running_ = true;
        
        auto duration = std::chrono::duration_cast<std::chrono::milliseconds>(
            std::chrono::steady_clock::now() - start_time);
        logger()->info("[APP] Application initialized successfully in {} ms", duration.count());
        return true;
        
    } catch (const std::exception& e) {
        logger()->error("[APP] Application initialization failed: {}", e.what());
        return false;
    }
}

void Application::shutdown() {
    if (!running_.exchange(false)) {
        logger()->debug("[APP] Shutdown called but application not running - skipping");
        return; // Already shutting down
    }
    
    auto start_time = std::chrono::steady_clock::now();
    logger()->info("[APP] Shutting down SPDK Flint Node Agent");
    
    // Stop node agent service
    if (node_agent_) {
        logger()->debug("[APP] Stopping node agent service");
        node_agent_->stop();
    }
    
    // Stop health server
    if (health_thread_.joinable()) {
        logger()->debug("[APP] Stopping health server thread");
        health_thread_.join();
    }
    
    // Shutdown SPDK - this will properly flush and cleanup
    if (spdk_wrapper_) {
        logger()->debug("[APP] Shutting down embedded SPDK");
        spdk_wrapper_->shutdown();
    }
    
    auto duration = std::chrono::duration_cast<std::chrono::milliseconds>(
        std::chrono::steady_clock::now() - start_time);
    logger()->info("[APP] SPDK Flint Node Agent shutdown complete in {} ms", duration.count());
}

void Application::setupLogging() {
    // Logging is already initialized in main.cpp
    logger()->debug("[APP] Logging system already configured");
}

void Application::initializeComponents() {
    auto start_time = std::chrono::steady_clock::now();
    logger()->info("[APP] Initializing SPDK Flint Node Agent components");
    
    // Initialize SPDK wrapper with embedded SPDK
    logger()->debug("[APP] Creating embedded SPDK wrapper");
    spdk_wrapper_ = std::make_shared<spdk_flint::spdk::SpdkWrapper>(config_.config_file);
    if (!spdk_wrapper_->initialize()) {
        throw std::runtime_error("Failed to initialize embedded SPDK");
    }
    logger()->info("[APP] Embedded SPDK initialized successfully");
    
    // Initialize Kubernetes client
    logger()->debug("[APP] Initializing Kubernetes client");
    auto kube_client = kube::KubeClient::create_incluster();
    if (!kube_client) {
        logger()->warn("[APP] Failed to create in-cluster client, trying kubeconfig");
        kube_client = kube::KubeClient::create_from_kubeconfig();
    }
    if (!kube_client) {
        throw std::runtime_error("Failed to initialize Kubernetes client");
    }
    logger()->info("[APP] Kubernetes client initialized successfully");
    
    // Create node agent service instance
    logger()->debug("[APP] Creating node agent service");
    node_agent_ = std::make_unique<NodeAgentService>(spdk_wrapper_, kube_client, config_);
    
    auto duration = std::chrono::duration_cast<std::chrono::milliseconds>(
        std::chrono::steady_clock::now() - start_time);
    logger()->info("[APP] SPDK Flint Node Agent components initialized successfully in {} ms", duration.count());
}

void Application::startNodeAgentMode() {
    logger()->info("[APP] Starting SPDK Flint Node Agent mode");
    logger()->debug("[APP] Node agent configuration: port={}, discovery_interval={}s", 
                   config_.node_agent_port, config_.discovery_interval);
    node_agent_->start();
    logger()->info("[APP] Node Agent service started");
}

void Application::startHealthServer() {
    logger()->info("[APP] Starting health server on port {}", config_.health_port);
    
    // Start a simple health server
    health_thread_ = std::thread([this]() {
        try {
                                      logger()->debug("[APP] Health server thread started: {}", spdk_flint::current_thread_id());
             crow::SimpleApp app;
             
             // Note: Simple HTTP server for health checks
            
            CROW_ROUTE(app, "/health").methods("GET"_method)
            ([this](const crow::request& req) {
                logger()->debug("[APP] Health check from {}", req.remote_ip_address);
                if (node_agent_ && node_agent_->is_running() && spdk_wrapper_ && spdk_wrapper_->isInitialized()) {
                    logger()->debug("[APP] Health check: OK");
                    return crow::response(200, "OK");
                } else {
                    logger()->warn("[APP] Health check: FAIL (agent_running={}, spdk_initialized={})", 
                                  node_agent_ ? node_agent_->is_running() : false,
                                  spdk_wrapper_ ? spdk_wrapper_->isInitialized() : false);
                    return crow::response(503, "Service Unavailable");
                }
            });
            
            CROW_ROUTE(app, "/ready").methods("GET"_method)
            ([this](const crow::request& req) {
                logger()->debug("[APP] Readiness check from {}", req.remote_ip_address);
                if (running_ && spdk_wrapper_ && spdk_wrapper_->isInitialized()) {
                    logger()->debug("[APP] Readiness check: Ready");
                    return crow::response(200, "Ready");
                } else {
                    logger()->warn("[APP] Readiness check: Not Ready (running={}, spdk_initialized={})", 
                                  running_.load(), spdk_wrapper_ ? spdk_wrapper_->isInitialized() : false);
                    return crow::response(503, "Not Ready");
                }
            });
            
            CROW_ROUTE(app, "/version").methods("GET"_method)
            ([this](const crow::request& req) {
                logger()->debug("[APP] Version request from {}", req.remote_ip_address);
                crow::json::wvalue version;
                version["application"] = "spdk-flint-node-agent";
                version["version"] = "1.0.0";
                version["spdk_version"] = spdk_wrapper_ ? spdk_wrapper_->getVersion() : "unknown";
                version["build_date"] = __DATE__;
                version["build_time"] = __TIME__;
                return crow::response(200, version);
            });
            
            logger()->info("[APP] Health server listening on port {}", config_.health_port);
            app.port(config_.health_port).multithreaded().run();
            
        } catch (const std::exception& e) {
            logger()->error("[APP] Health server thread exception: {}", e.what());
        }
        logger()->debug("[APP] Health server thread exiting");
    });
    
    logger()->info("[APP] Health server started successfully");
}

void Application::waitForSpdkReady() {
    logger()->debug("[APP] Waiting for SPDK to be ready");
    
    auto start_time = std::chrono::steady_clock::now();
    int attempts = 0;
    
    // Wait for SPDK to be ready
    while (!spdk_wrapper_ || !spdk_wrapper_->isInitialized()) {
        attempts++;
        
        if (attempts % 10 == 0) {  // Log every second
            auto duration = std::chrono::duration_cast<std::chrono::milliseconds>(
                std::chrono::steady_clock::now() - start_time);
            logger()->debug("[APP] Waiting for SPDK (attempt {}, {}ms elapsed)", attempts, duration.count());
        }
        
        std::this_thread::sleep_for(std::chrono::milliseconds(100));
        
        // Timeout after 30 seconds
        if (attempts > 300) {
            logger()->error("[APP] Timeout waiting for SPDK to initialize");
            throw std::runtime_error("SPDK initialization timeout");
        }
    }
    
    auto duration = std::chrono::duration_cast<std::chrono::milliseconds>(
        std::chrono::steady_clock::now() - start_time);
    logger()->info("[APP] SPDK ready after {} attempts ({} ms)", attempts, duration.count());
}

} // namespace spdk_flint 