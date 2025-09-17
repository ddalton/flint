#include "spdk/spdk_wrapper.hpp"
#include "spdk/rpc_client.hpp"
#include "logging.hpp"
#include <future>
#include <thread>
#include <chrono>
#include <sstream>
#include <sys/stat.h>

namespace spdk_flint {
namespace spdk {

// Error conversion utilities
SpdkError SpdkWrapper::convertErrno(int err) {
    switch (err) {
        case 0: return SpdkError::SUCCESS;
        case -EINVAL: return SpdkError::INVALID_PARAM;
        case -ENOMEM: return SpdkError::NO_MEMORY;
        case -ENOENT: return SpdkError::NOT_FOUND;
        case -EEXIST: return SpdkError::ALREADY_EXISTS;
        case -EIO: return SpdkError::IO_ERROR;
        case -ETIMEDOUT: return SpdkError::TIMEOUT;
        case -EBUSY: return SpdkError::BUSY;
        default: return SpdkError::UNKNOWN;
    }
}

void SpdkWrapper::throwOnError(int rc, const std::string& operation) {
    if (rc != 0) {
        auto error = convertErrno(rc);
        std::string message = fmt::format("{} failed with error code {}", operation, rc);
        spdk_flint::logger()->error("[SPDK] {}", message);
        throw SpdkException(error, message);
    }
}

// Constructor/Destructor
SpdkWrapper::SpdkWrapper(const std::string& config_file)
    : config_file_(config_file), opts_(nullptr) {
    spdk_flint::logger()->info("[SPDK] Creating SPDK wrapper with RPC interface");
    spdk_flint::logger()->debug("[SPDK] Configuration file: '{}'", config_file_.empty() ? "none" : config_file_);

    // Initialize RPC client for all operations
    rpc_client_ = std::make_unique<RpcClient>("/var/tmp/spdk.sock");
}

SpdkWrapper::~SpdkWrapper() {
    spdk_flint::logger()->debug("[SPDK] Destroying SPDK wrapper");
    shutdown();
}

// Initialization
bool SpdkWrapper::initialize() {
    spdk_flint::logger()->info("[SPDK] Initializing SPDK wrapper with RPC interface");

    // Connect to SPDK RPC socket
    if (!rpc_client_->connect()) {
        spdk_flint::logger()->error("[SPDK] Failed to connect to SPDK RPC socket");
        return false;
    }

    spdk_flint::logger()->info("[SPDK] SPDK wrapper initialized successfully via RPC");
    return true;
}

void SpdkWrapper::shutdown() {
    spdk_flint::logger()->info("[SPDK] Shutting down SPDK wrapper");

    // Clean up any pending callbacks
    {
        std::lock_guard<std::mutex> lock(callback_mutex_);
        if (!pending_callbacks_.empty()) {
            spdk_flint::logger()->debug("[SPDK] Cleaning up {} pending callbacks", pending_callbacks_.size());
            for (auto& [id, ctx] : pending_callbacks_) {
                if (ctx.cleanup) {
                    ctx.cleanup();
                }
            }
            pending_callbacks_.clear();
        }
    }

    // Disconnect RPC client
    if (rpc_client_ && rpc_client_->isConnected()) {
        rpc_client_->disconnect();
    }

    spdk_flint::logger()->info("[SPDK] SPDK wrapper shutdown complete");
}

bool SpdkWrapper::isInitialized() const {
    bool connected = rpc_client_ && rpc_client_->isConnected();
    spdk_flint::logger()->debug("[SPDK] Initialization status check: {}", connected ? "connected" : "disconnected");
    return connected;
}

std::string SpdkWrapper::getVersion() const {
    return "SPDK RPC Interface";
}

// ===== LVol Store Operations via RPC =====

void SpdkWrapper::getLvolStoresAsync(
    const std::string& uuid,
    const std::string& lvs_name,
    std::function<void(const std::vector<LvolStoreInfo>&, int)> callback) {

    auto start_time = std::chrono::steady_clock::now();
    spdk_flint::logger()->info("[SPDK] Getting LVol stores via RPC - UUID: '{}', Name: '{}'", uuid, lvs_name);

    if (!rpc_client_->isConnected()) {
        spdk_flint::logger()->error("[SPDK] RPC client not connected");
        if (callback) callback({}, -ENOTCONN);
        return;
    }

    // Use async RPC call
    rpc_client_->callRpcAsync("bdev_lvol_get_lvstores",
        json{{"uuid", uuid}, {"lvs_name", lvs_name}},
        [callback, start_time](const json& result, int error) {
            std::vector<LvolStoreInfo> stores;

            if (error == 0) {
                try {
                    if (result.is_array()) {
                        for (const auto& store : result) {
                            LvolStoreInfo info;
                            info.uuid = store.value("uuid", "");
                            info.name = store.value("name", "");
                            info.base_bdev = store.value("base_bdev", "");
                            info.total_clusters = store.value("total_data_clusters", 0);
                            info.free_clusters = store.value("free_clusters", 0);
                            info.cluster_size = store.value("cluster_size", 0);
                            info.block_size = store.value("block_size", 0);
                            stores.push_back(info);
                        }
                    }

                    auto duration = std::chrono::duration_cast<std::chrono::milliseconds>(
                        std::chrono::steady_clock::now() - start_time);
                    spdk_flint::logger()->info("[SPDK] Found {} LVol stores in {} ms", stores.size(), duration.count());
                } catch (const std::exception& e) {
                    spdk_flint::logger()->error("[SPDK] Error parsing LVol store response: {}", e.what());
                    error = -EINVAL;
                }
            }

            if (callback) callback(stores, error);
        });
}

void SpdkWrapper::createLvolStoreAsync(
    const std::string& bdev_name,
    const std::string& lvs_name,
    const std::string& clear_method,
    uint32_t cluster_sz,
    LvolStoreCreateCallback callback) {

    spdk_flint::logger()->info("[SPDK] Creating LVol store via RPC - Bdev: '{}', Name: '{}', Clear: '{}', Cluster: {}",
                              bdev_name, lvs_name, clear_method, cluster_sz);

    if (!rpc_client_->isConnected()) {
        spdk_flint::logger()->error("[SPDK] RPC client not connected");
        if (callback) callback("", -ENOTCONN);
        return;
    }

    json params;
    params["bdev_name"] = bdev_name;
    params["lvs_name"] = lvs_name;
    if (!clear_method.empty()) {
        params["clear_method"] = clear_method;
    }
    if (cluster_sz > 0) {
        params["cluster_sz"] = cluster_sz;
    }

    rpc_client_->callRpcAsync("bdev_lvol_create_lvstore", params,
        [callback, lvs_name](const json& result, int error) {
            if (error == 0) {
                std::string uuid = result.value("uuid", "");
                spdk_flint::logger()->info("[SPDK] LVol store '{}' created successfully, UUID: {}", lvs_name, uuid);
                if (callback) callback(uuid, 0);
            } else {
                spdk_flint::logger()->error("[SPDK] Failed to create LVol store '{}'", lvs_name);
                if (callback) callback("", error);
            }
        });
}

void SpdkWrapper::deleteLvolStoreAsync(
    const std::string& uuid,
    const std::string& lvs_name,
    LvolStoreDeleteCallback callback) {

    spdk_flint::logger()->info("[SPDK] Deleting LVol store via RPC - UUID: '{}', Name: '{}'", uuid, lvs_name);

    if (!rpc_client_->isConnected()) {
        spdk_flint::logger()->error("[SPDK] RPC client not connected");
        if (callback) callback(-ENOTCONN);
        return;
    }

    json params;
    if (!uuid.empty()) {
        params["uuid"] = uuid;
    } else if (!lvs_name.empty()) {
        params["lvs_name"] = lvs_name;
    } else {
        spdk_flint::logger()->error("[SPDK] Neither UUID nor LVS name specified for deletion");
        if (callback) callback(-EINVAL);
        return;
    }

    rpc_client_->callRpcAsync("bdev_lvol_delete_lvstore", params,
        [callback](const json& result, int error) {
            if (error == 0) {
                spdk_flint::logger()->info("[SPDK] LVol store deleted successfully");
            } else {
                spdk_flint::logger()->error("[SPDK] Failed to delete LVol store");
            }
            if (callback) callback(error);
        });
}

// ===== Block Device Creation via RPC =====

void SpdkWrapper::createAioBdevAsync(
    const std::string& name,
    const std::string& filename,
    uint32_t block_size,
    bool readonly,
    bool fallocate,
    const std::string& uuid,
    BdevCreateCallback callback) {

    spdk_flint::logger()->info("[SPDK] Creating AIO bdev via RPC - Name: '{}', File: '{}', BlockSize: {}, RO: {}, Fallocate: {}",
                              name, filename, block_size, readonly, fallocate);

    if (!rpc_client_->isConnected()) {
        spdk_flint::logger()->error("[SPDK] RPC client not connected");
        if (callback) callback("", -ENOTCONN);
        return;
    }

    json params;
    params["name"] = name;
    params["filename"] = filename;
    params["block_size"] = block_size;
    params["readonly"] = readonly;
    params["fallocate"] = fallocate;
    if (!uuid.empty()) {
        params["uuid"] = uuid;
    }

    rpc_client_->callRpcAsync("bdev_aio_create", params,
        [callback, name](const json& result, int error) {
            if (error == 0) {
                spdk_flint::logger()->info("[SPDK] AIO bdev '{}' created successfully", name);
                if (callback) callback(name, 0);
            } else {
                spdk_flint::logger()->error("[SPDK] Failed to create AIO bdev '{}'", name);
                if (callback) callback("", error);
            }
        });
}

void SpdkWrapper::createUringBdevAsync(
    const std::string& name,
    const std::string& filename,
    uint32_t block_size,
    const std::string& uuid,
    BdevCreateCallback callback) {

    spdk_flint::logger()->info("[SPDK] Creating uring bdev via RPC - Name: '{}', File: '{}', BlockSize: {}",
                              name, filename, block_size);

    if (!rpc_client_->isConnected()) {
        spdk_flint::logger()->error("[SPDK] RPC client not connected");
        if (callback) callback("", -ENOTCONN);
        return;
    }

    json params;
    params["name"] = name;
    params["filename"] = filename;
    params["block_size"] = block_size;
    if (!uuid.empty()) {
        params["uuid"] = uuid;
    }

    rpc_client_->callRpcAsync("bdev_uring_create", params,
        [callback, name](const json& result, int error) {
            if (error == 0) {
                spdk_flint::logger()->info("[SPDK] uring bdev '{}' created successfully", name);
                if (callback) callback(name, 0);
            } else {
                spdk_flint::logger()->error("[SPDK] Failed to create uring bdev '{}'", name);
                if (callback) callback("", error);
            }
        });
}

// ===== NVMe Operations via RPC =====

void SpdkWrapper::getNvmeControllersAsync(
    const std::string& name,
    std::function<void(const std::vector<NvmeControllerInfo>&, int)> callback) {

    spdk_flint::logger()->info("[SPDK] Getting NVMe controllers via RPC - Name: '{}'", name);

    if (!rpc_client_->isConnected()) {
        spdk_flint::logger()->error("[SPDK] RPC client not connected");
        if (callback) callback({}, -ENOTCONN);
        return;
    }

    json params;
    if (!name.empty()) {
        params["name"] = name;
    }

    rpc_client_->callRpcAsync("bdev_nvme_get_controllers", params,
        [callback](const json& result, int error) {
            std::vector<NvmeControllerInfo> controllers;

            if (error == 0) {
                try {
                    if (result.is_array()) {
                        for (const auto& ctrl : result) {
                            NvmeControllerInfo info;
                            info.name = ctrl.value("name", "");
                            info.trtype = ctrl.value("trtype", "");
                            info.traddr = ctrl.value("traddr", "");
                            info.state = ctrl.value("state", "");

                            if (ctrl.contains("bdevs") && ctrl["bdevs"].is_array()) {
                                for (const auto& bdev : ctrl["bdevs"]) {
                                    info.bdevs.push_back(bdev.get<std::string>());
                                }
                            }

                            controllers.push_back(info);
                        }
                    }
                    spdk_flint::logger()->info("[SPDK] Found {} NVMe controllers", controllers.size());
                } catch (const std::exception& e) {
                    spdk_flint::logger()->error("[SPDK] Error parsing NVMe controller response: {}", e.what());
                    error = -EINVAL;
                }
            }

            if (callback) callback(controllers, error);
        });
}

void SpdkWrapper::attachNvmeControllerAsync(
    const std::string& name,
    const std::string& trtype,
    const std::string& traddr,
    const std::string& adrfam,
    const std::string& trsvcid,
    uint32_t priority,
    const std::string& subnqn,
    const std::string& hostnqn,
    const std::string& hostaddr,
    const std::string& hostsvcid,
    bool multipath,
    uint32_t num_io_queues,
    uint32_t ctrlr_loss_timeout_sec,
    uint32_t reconnect_delay_sec,
    uint32_t fast_io_fail_timeout_sec,
    NvmeAttachCallback callback) {

    spdk_flint::logger()->info("[SPDK] Attaching NVMe controller via RPC - Name: '{}', Type: '{}', Addr: '{}'",
                              name, trtype, traddr);

    if (!rpc_client_->isConnected()) {
        spdk_flint::logger()->error("[SPDK] RPC client not connected");
        if (callback) callback({}, -ENOTCONN);
        return;
    }

    json params;
    params["name"] = name;
    params["trtype"] = trtype;
    params["traddr"] = traddr;
    if (!adrfam.empty()) params["adrfam"] = adrfam;
    if (!trsvcid.empty()) params["trsvcid"] = trsvcid;
    if (priority > 0) params["priority"] = priority;
    if (!subnqn.empty()) params["subnqn"] = subnqn;
    if (!hostnqn.empty()) params["hostnqn"] = hostnqn;
    if (!hostaddr.empty()) params["hostaddr"] = hostaddr;
    if (!hostsvcid.empty()) params["hostsvcid"] = hostsvcid;
    params["multipath"] = multipath;
    if (num_io_queues > 0) params["num_io_queues"] = num_io_queues;
    if (ctrlr_loss_timeout_sec > 0) params["ctrlr_loss_timeout_sec"] = ctrlr_loss_timeout_sec;
    if (reconnect_delay_sec > 0) params["reconnect_delay_sec"] = reconnect_delay_sec;
    if (fast_io_fail_timeout_sec > 0) params["fast_io_fail_timeout_sec"] = fast_io_fail_timeout_sec;

    rpc_client_->callRpcAsync("bdev_nvme_attach_controller", params,
        [callback, name](const json& result, int error) {
            std::vector<std::string> bdev_names;

            if (error == 0) {
                try {
                    if (result.is_array()) {
                        for (const auto& bdev : result) {
                            bdev_names.push_back(bdev.get<std::string>());
                        }
                    }
                    spdk_flint::logger()->info("[SPDK] NVMe controller '{}' attached, {} bdevs created", name, bdev_names.size());
                } catch (const std::exception& e) {
                    spdk_flint::logger()->error("[SPDK] Error parsing NVMe attach response: {}", e.what());
                    error = -EINVAL;
                }
            } else {
                spdk_flint::logger()->error("[SPDK] Failed to attach NVMe controller '{}'", name);
            }

            if (callback) callback(bdev_names, error);
        });
}

// ===== Block Device Enumeration via RPC =====

void SpdkWrapper::getBdevsAsync(
    const std::string& name,
    uint32_t timeout,
    std::function<void(const std::vector<BdevInfo>&, int)> callback) {

    spdk_flint::logger()->info("[SPDK] Getting block devices via RPC - Name: '{}'", name);

    if (!rpc_client_->isConnected()) {
        spdk_flint::logger()->error("[SPDK] RPC client not connected");
        if (callback) callback({}, -ENOTCONN);
        return;
    }

    json params;
    if (!name.empty()) {
        params["name"] = name;
    }
    if (timeout > 0) {
        params["timeout"] = timeout;
    }

    rpc_client_->callRpcAsync("bdev_get_bdevs", params,
        [callback](const json& result, int error) {
            std::vector<BdevInfo> bdevs;

            if (error == 0) {
                try {
                    if (result.is_array()) {
                        for (const auto& bdev : result) {
                            BdevInfo info;
                            info.name = bdev.value("name", "");
                            info.uuid = bdev.value("uuid", "");
                            info.product_name = bdev.value("product_name", "");
                            info.block_size = bdev.value("block_size", 512);
                            info.num_blocks = bdev.value("num_blocks", 0);
                            info.claimed = bdev.value("claimed", false);

                            if (bdev.contains("aliases") && bdev["aliases"].is_array()) {
                                for (const auto& alias : bdev["aliases"]) {
                                    info.aliases.push_back(alias.get<std::string>());
                                }
                            }

                            if (bdev.contains("driver_specific")) {
                                info.driver_specific = bdev["driver_specific"].dump();
                            }

                            bdevs.push_back(info);
                        }
                    }
                    spdk_flint::logger()->info("[SPDK] Found {} block devices", bdevs.size());
                } catch (const std::exception& e) {
                    spdk_flint::logger()->error("[SPDK] Error parsing bdev response: {}", e.what());
                    error = -EINVAL;
                }
            }

            if (callback) callback(bdevs, error);
        });
}

// ===== Process Control =====

void SpdkWrapper::stopApplicationAsync(std::function<void(int)> callback) {
    spdk_flint::logger()->info("[SPDK] Stopping SPDK application via RPC");

    // For RPC interface, we just disconnect
    if (rpc_client_ && rpc_client_->isConnected()) {
        rpc_client_->disconnect();
    }

    if (callback) {
        callback(0);
    }
}

// ===== Event Loop Management =====

void SpdkWrapper::processEvents() {
    // No events to process in RPC mode
}

void SpdkWrapper::runEventLoop() {
    event_loop_running_ = true;
    spdk_flint::logger()->info("[SPDK] Starting RPC event loop");

    while (event_loop_running_ && !shutdown_requested_) {
        // Keep connection alive
        if (rpc_client_ && !rpc_client_->isConnected()) {
            spdk_flint::logger()->debug("[SPDK] Attempting to reconnect to SPDK RPC");
            rpc_client_->connect();
        }

        std::this_thread::sleep_for(std::chrono::seconds(1));
    }

    spdk_flint::logger()->info("[SPDK] RPC event loop stopped");
}

void SpdkWrapper::stopEventLoop() {
    spdk_flint::logger()->info("[SPDK] Requesting RPC event loop stop");
    event_loop_running_ = false;
    shutdown_requested_ = true;
}

// ===== UBLK Operations via RPC =====

bool SpdkWrapper::ensureUblkTarget() {
    if (ublk_target_initialized_.load()) {
        spdk_flint::logger()->debug("[SPDK] Ublk target already initialized");
        return true;
    }

    spdk_flint::logger()->info("[SPDK] Initializing ublk target via RPC");

    if (!rpc_client_->isConnected()) {
        if (!rpc_client_->connect()) {
            spdk_flint::logger()->error("[SPDK] Failed to connect to SPDK RPC socket");
            return false;
        }
    }

    try {
        rpc_client_->createUblkTarget();
        ublk_target_initialized_.store(true);
        spdk_flint::logger()->info("[SPDK] Ublk target initialized successfully");
        return true;
    } catch (const std::exception& e) {
        // Check if error is because target already exists
        std::string error_msg = e.what();
        if (error_msg.find("already exists") != std::string::npos ||
            error_msg.find("File exists") != std::string::npos) {
            spdk_flint::logger()->info("[SPDK] Ublk target already exists, continuing");
            ublk_target_initialized_.store(true);
            return true;
        }
        spdk_flint::logger()->error("[SPDK] Failed to create ublk target: {}", e.what());
        return false;
    }
}

std::string SpdkWrapper::createUblkDevice(int ublk_id, const std::string& bdev_name) {
    spdk_flint::logger()->info("[SPDK] Creating ublk device via RPC: id={}, bdev={}", ublk_id, bdev_name);

    // Ensure ublk target is initialized
    if (!ensureUblkTarget()) {
        throw std::runtime_error("Failed to ensure ublk target is initialized");
    }

    if (!rpc_client_->isConnected()) {
        if (!rpc_client_->connect()) {
            throw std::runtime_error("Failed to connect to SPDK RPC socket");
        }
    }

    try {
        auto result = rpc_client_->startUblkDisk(ublk_id, bdev_name);
        std::string device_path = "/dev/ublkb" + std::to_string(ublk_id);

        // Wait for device to appear
        int max_retries = 30;
        for (int i = 0; i < max_retries; i++) {
            struct stat st;
            if (stat(device_path.c_str(), &st) == 0) {
                spdk_flint::logger()->info("[SPDK] Ublk device created successfully: {}", device_path);
                return device_path;
            }
            std::this_thread::sleep_for(std::chrono::milliseconds(100));
        }

        spdk_flint::logger()->warn("[SPDK] Ublk device created but not yet visible: {}", device_path);
        return device_path;

    } catch (const std::exception& e) {
        spdk_flint::logger()->error("[SPDK] Failed to create ublk device: {}", e.what());
        throw;
    }
}

bool SpdkWrapper::deleteUblkDevice(int ublk_id) {
    spdk_flint::logger()->info("[SPDK] Deleting ublk device via RPC: id={}", ublk_id);

    if (!rpc_client_->isConnected()) {
        if (!rpc_client_->connect()) {
            spdk_flint::logger()->error("[SPDK] Failed to connect to SPDK RPC socket");
            return false;
        }
    }

    try {
        rpc_client_->stopUblkDisk(ublk_id);

        // Wait for device to disappear
        std::string device_path = "/dev/ublkb" + std::to_string(ublk_id);
        int max_retries = 30;
        for (int i = 0; i < max_retries; i++) {
            struct stat st;
            if (stat(device_path.c_str(), &st) != 0) {
                spdk_flint::logger()->info("[SPDK] Ublk device deleted successfully: {}", device_path);
                return true;
            }
            std::this_thread::sleep_for(std::chrono::milliseconds(100));
        }

        spdk_flint::logger()->warn("[SPDK] Ublk device deleted but still visible: {}", device_path);
        return true;

    } catch (const std::exception& e) {
        spdk_flint::logger()->error("[SPDK] Failed to delete ublk device: {}", e.what());
        return false;
    }
}

std::vector<UblkDiskInfo> SpdkWrapper::getUblkDevices() {
    spdk_flint::logger()->debug("[SPDK] Getting ublk devices via RPC");

    if (!rpc_client_->isConnected()) {
        if (!rpc_client_->connect()) {
            spdk_flint::logger()->error("[SPDK] Failed to connect to SPDK RPC socket");
            return {};
        }
    }

    std::vector<UblkDiskInfo> devices;

    try {
        auto result = rpc_client_->getUblkDisks();

        if (result.is_array()) {
            for (const auto& disk : result) {
                UblkDiskInfo info;
                info.ublk_id = disk.value("id", -1);
                info.bdev_name = disk.value("bdev_name", "");
                info.device_path = "/dev/ublkb" + std::to_string(info.ublk_id);

                // Check if device exists
                struct stat st;
                info.active = (stat(info.device_path.c_str(), &st) == 0);

                devices.push_back(info);
            }
        }
    } catch (const std::exception& e) {
        spdk_flint::logger()->error("[SPDK] Failed to get ublk devices: {}", e.what());
    }

    return devices;
}

void SpdkWrapper::createUblkDeviceAsync(int ublk_id, const std::string& bdev_name, UblkCallback callback) {
    std::thread([this, ublk_id, bdev_name, callback]() {
        try {
            std::string device_path = createUblkDevice(ublk_id, bdev_name);
            if (callback) {
                callback(device_path, 0);
            }
        } catch (const std::exception& e) {
            spdk_flint::logger()->error("[SPDK] Async ublk device creation failed: {}", e.what());
            if (callback) {
                callback("", -1);
            }
        }
    }).detach();
}

void SpdkWrapper::deleteUblkDeviceAsync(int ublk_id, std::function<void(int)> callback) {
    std::thread([this, ublk_id, callback]() {
        bool success = deleteUblkDevice(ublk_id);
        if (callback) {
            callback(success ? 0 : -1);
        }
    }).detach();
}

// ===== Volume Operations via RPC =====

std::string SpdkWrapper::createVolume(const std::string& lvs_name, const std::string& volume_name, uint64_t size_bytes) {
    spdk_flint::logger()->info("[SPDK] Creating volume via RPC: lvs={}, name={}, size={}", lvs_name, volume_name, size_bytes);

    if (!rpc_client_->isConnected()) {
        if (!rpc_client_->connect()) {
            throw std::runtime_error("Failed to connect to SPDK RPC socket");
        }
    }

    try {
        auto result = rpc_client_->createLvolBdev(lvs_name, volume_name, size_bytes);
        std::string uuid = result.value("uuid", "");
        spdk_flint::logger()->info("[SPDK] Volume created successfully: name={}, uuid={}", volume_name, uuid);
        return uuid;
    } catch (const std::exception& e) {
        spdk_flint::logger()->error("[SPDK] Failed to create volume: {}", e.what());
        throw;
    }
}

bool SpdkWrapper::deleteVolume(const std::string& volume_name) {
    spdk_flint::logger()->info("[SPDK] Deleting volume via RPC: {}", volume_name);

    if (!rpc_client_->isConnected()) {
        if (!rpc_client_->connect()) {
            return false;
        }
    }

    try {
        rpc_client_->deleteBdev(volume_name);
        spdk_flint::logger()->info("[SPDK] Volume deleted successfully: {}", volume_name);
        return true;
    } catch (const std::exception& e) {
        spdk_flint::logger()->error("[SPDK] Failed to delete volume: {}", e.what());
        return false;
    }
}

bool SpdkWrapper::resizeVolume(const std::string& volume_name, uint64_t new_size_bytes) {
    spdk_flint::logger()->info("[SPDK] Resizing volume via RPC: name={}, new_size={}", volume_name, new_size_bytes);

    if (!rpc_client_->isConnected()) {
        if (!rpc_client_->connect()) {
            return false;
        }
    }

    try {
        rpc_client_->resizeLvolBdev(volume_name, new_size_bytes);
        spdk_flint::logger()->info("[SPDK] Volume resized successfully: {}", volume_name);
        return true;
    } catch (const std::exception& e) {
        spdk_flint::logger()->error("[SPDK] Failed to resize volume: {}", e.what());
        return false;
    }
}

std::optional<BdevInfo> SpdkWrapper::getVolumeInfo(const std::string& volume_name) {
    spdk_flint::logger()->debug("[SPDK] Getting volume info via RPC: {}", volume_name);

    if (!rpc_client_->isConnected()) {
        if (!rpc_client_->connect()) {
            return std::nullopt;
        }
    }

    try {
        auto bdevs = rpc_client_->getBdevs(volume_name);

        if (bdevs.is_array() && !bdevs.empty()) {
            auto& bdev = bdevs[0];
            BdevInfo info;
            info.name = bdev.value("name", "");
            info.uuid = bdev.value("uuid", "");
            info.product_name = bdev.value("product_name", "");
            info.block_size = bdev.value("block_size", 512);
            info.num_blocks = bdev.value("num_blocks", 0);
            info.claimed = bdev.value("claimed", false);

            if (bdev.contains("aliases") && bdev["aliases"].is_array()) {
                for (const auto& alias : bdev["aliases"]) {
                    info.aliases.push_back(alias.get<std::string>());
                }
            }

            return info;
        }
    } catch (const std::exception& e) {
        spdk_flint::logger()->error("[SPDK] Failed to get volume info: {}", e.what());
    }

    return std::nullopt;
}

// ===== Synchronous convenience methods =====

std::vector<BdevInfo> SpdkWrapper::getBdevs(const std::string& name) const {
    spdk_flint::logger()->debug("[SPDK] Synchronous getBdevs wrapper called for: '{}'", name);

    std::vector<BdevInfo> result;
    std::promise<void> promise;
    auto future = promise.get_future();

    // Cast away const for the async call
    const_cast<SpdkWrapper*>(this)->getBdevsAsync(name, 0,
        [&result, &promise](const std::vector<BdevInfo>& bdevs, int error) {
            if (error == 0) {
                result = bdevs;
                spdk_flint::logger()->debug("[SPDK] Synchronous getBdevs completed: {} devices", bdevs.size());
            } else {
                spdk_flint::logger()->error("[SPDK] Synchronous getBdevs failed: error {}", error);
            }
            promise.set_value();
        });

    future.wait();
    return result;
}

std::vector<LvolStoreInfo> SpdkWrapper::getLvolStores(const std::string& uuid, const std::string& lvs_name) const {
    spdk_flint::logger()->debug("[SPDK] Synchronous getLvolStores wrapper called - UUID: '{}', Name: '{}'", uuid, lvs_name);

    std::vector<LvolStoreInfo> result;
    std::promise<void> promise;
    auto future = promise.get_future();

    const_cast<SpdkWrapper*>(this)->getLvolStoresAsync(uuid, lvs_name,
        [&result, &promise](const std::vector<LvolStoreInfo>& stores, int error) {
            if (error == 0) {
                result = stores;
                spdk_flint::logger()->debug("[SPDK] Synchronous getLvolStores completed: {} stores", stores.size());
            } else {
                spdk_flint::logger()->error("[SPDK] Synchronous getLvolStores failed: error {}", error);
            }
            promise.set_value();
        });

    future.wait();
    return result;
}

std::string SpdkWrapper::getVersionSync() const {
    return getVersion();
}

// ===== Callback management helpers =====

uint64_t SpdkWrapper::registerCallback(std::function<void()> cleanup) {
    std::lock_guard<std::mutex> lock(callback_mutex_);
    uint64_t id = next_callback_id_++;
    pending_callbacks_[id] = {id, cleanup};
    spdk_flint::logger()->debug("[SPDK] Registered callback #{} (total: {})", id, pending_callbacks_.size());
    return id;
}

void SpdkWrapper::unregisterCallback(uint64_t callback_id) {
    std::lock_guard<std::mutex> lock(callback_mutex_);
    auto it = pending_callbacks_.find(callback_id);
    if (it != pending_callbacks_.end()) {
        if (it->second.cleanup) {
            it->second.cleanup();
        }
        pending_callbacks_.erase(it);
        spdk_flint::logger()->debug("[SPDK] Unregistered callback #{} (remaining: {})", callback_id, pending_callbacks_.size());
    } else {
        spdk_flint::logger()->warn("[SPDK] Attempted to unregister non-existent callback #{}", callback_id);
    }
}

} // namespace spdk
} // namespace spdk_flint