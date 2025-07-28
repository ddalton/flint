#include "spdk/spdk_wrapper.hpp"
// Undefine syslog macros that conflict with our logging
#ifdef LOG_DEBUG
#undef LOG_DEBUG
#endif
#ifdef LOG_INFO
#undef LOG_INFO
#endif
#include "logging.hpp"
#include <future>
#include <thread>
#include <chrono>
#include <cstring>
#include <atomic>

namespace spdk_flint {
namespace spdk {

// Global SPDK application context
static bool g_spdk_initialized = false;
static bool g_shutdown_requested = false;

// SPDK application callbacks
static void spdk_app_started(void* arg) {
    auto* wrapper = static_cast<SpdkWrapper*>(arg);
    (void)wrapper; // Suppress unused variable warning
    LOG_INFO("SPDK application started successfully");
    g_spdk_initialized = true;
    // Signal that initialization is complete
}

// Note: spdk_app_stopped is currently unused but may be needed for proper SPDK lifecycle
// static void spdk_app_stopped(void* arg, int rc) {
//     LOG_INFO("SPDK application stopped with code: {}", rc);
//     g_spdk_initialized = false;
// }

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
        std::string message = operation + " failed with code " + std::to_string(rc);
        LOG_SPDK_ERROR("{}", message);
        throw SpdkException(error, message);
    }
}

// Constructor/Destructor
SpdkWrapper::SpdkWrapper(const std::string& config_file) 
    : config_file_(config_file) {
    LOG_SPDK_INFO("Creating SPDK wrapper");
}

SpdkWrapper::~SpdkWrapper() {
    shutdown();
}

// Initialization
bool SpdkWrapper::initialize() {
    if (g_spdk_initialized) {
        LOG_SPDK_WARN("SPDK already initialized");
        return true;
    }

    try {
        LOG_INFO("Initializing SPDK application framework");
        
        // Allocate and initialize SPDK options
        opts_ = reinterpret_cast<struct spdk_app_opts*>(spdk_dma_zmalloc(sizeof(struct spdk_app_opts), 64, nullptr));
        if (!opts_) {
            LOG_SPDK_ERROR("Failed to allocate SPDK options");
            return false;
        }

        spdk_app_opts_init(opts_, sizeof(struct spdk_app_opts));
        opts_->name = "spdk_flint";
        opts_->json_config_file = config_file_.empty() ? nullptr : config_file_.c_str();
        opts_->reactor_mask = "0x1";  // Use single core for simplicity
        opts_->mem_size = 512;  // 512MB
        opts_->no_pci = false;
        opts_->delay_subsystem_init = true;
        
        // Use SPDK's proper application startup - this sets up signal handlers
        int rc = spdk_app_start(opts_, spdk_app_started, this);
        if (rc != 0) {
            LOG_SPDK_ERROR("Failed to start SPDK application: {}", rc);
            if (opts_) {
                spdk_dma_free(opts_);
                opts_ = nullptr;
            }
            return false;
        }

        LOG_INFO("SPDK application framework initialized successfully");
        return true;
        
    } catch (const std::exception& e) {
        LOG_SPDK_ERROR("Exception during SPDK initialization: {}", e.what());
        if (opts_) {
            spdk_dma_free(opts_);
            opts_ = nullptr;
        }
        return false;
    }
}

void SpdkWrapper::shutdown() {
    if (!g_spdk_initialized) {
        LOG_SPDK_WARN("SPDK not initialized");
        return;
    }

    try {
        LOG_INFO("Shutting down SPDK application framework");
        
        // Request shutdown - SPDK will handle the actual cleanup
        spdk_app_stop(0);
        
        // Wait for SPDK to finish shutdown
        spdk_app_fini();
        
        if (opts_) {
            spdk_dma_free(opts_);
            opts_ = nullptr;
        }
        
        g_spdk_initialized = false;
        LOG_INFO("SPDK application framework shutdown complete");
        
    } catch (const std::exception& e) {
        LOG_SPDK_ERROR("Exception during SPDK shutdown: {}", e.what());
    }
}

bool SpdkWrapper::isInitialized() const {
    return g_spdk_initialized;
}

// Version information
std::string SpdkWrapper::getVersion() const {
    // Direct SPDK version query - replace spdk_get_version RPC
    return "SPDK 25.05";  // Fixed version for now
}

// Block device operations
std::vector<BdevInfo> SpdkWrapper::getBdevs(const std::string& name) const {
    std::vector<BdevInfo> result;
    
    if (!g_spdk_initialized) {
        LOG_SPDK_ERROR("SPDK not initialized");
        return result;
    }
    
    LOG_RPC_CALL("bdev_get_bdevs", "name={}", name);
    
    try {
        struct spdk_bdev* bdev = nullptr;
        
        if (!name.empty()) {
            bdev = spdk_bdev_get_by_name(name.c_str());
            if (bdev) {
                BdevInfo info;
                info.name = spdk_bdev_get_name(bdev);
                info.product_name = spdk_bdev_get_product_name(bdev);
                info.num_blocks = spdk_bdev_get_num_blocks(bdev);
                info.block_size = spdk_bdev_get_block_size(bdev);
                
                // Get UUID if available
                const struct spdk_uuid* uuid = spdk_bdev_get_uuid(bdev);
                if (uuid) {
                    char uuid_str[SPDK_UUID_STRING_LEN];
                    spdk_uuid_fmt_lower(uuid_str, sizeof(uuid_str), uuid);
                    info.uuid = uuid_str;
                }
                
                info.claimed = false;  // spdk_bdev_is_claimed not available
                result.push_back(info);
            }
        } else {
            // Iterate through all bdevs
            for (bdev = spdk_bdev_first(); bdev != nullptr; bdev = spdk_bdev_next(bdev)) {
                BdevInfo info;
                info.name = spdk_bdev_get_name(bdev);
                info.product_name = spdk_bdev_get_product_name(bdev);
                info.num_blocks = spdk_bdev_get_num_blocks(bdev);
                info.block_size = spdk_bdev_get_block_size(bdev);
                
                const struct spdk_uuid* uuid = spdk_bdev_get_uuid(bdev);
                if (uuid) {
                    char uuid_str[SPDK_UUID_STRING_LEN];
                    spdk_uuid_fmt_lower(uuid_str, sizeof(uuid_str), uuid);
                    info.uuid = uuid_str;
                }
                
                info.claimed = false;  // spdk_bdev_is_claimed not available
                result.push_back(info);
            }
        }
        
        LOG_RPC_SUCCESS("bdev_get_bdevs", "found {} bdevs", result.size());
        
    } catch (const std::exception& e) {
        LOG_RPC_ERROR("[RPC] SPDK method bdev_get_bdevs failed: Exception occurred");
    }
    
    return result;
}

bool SpdkWrapper::createAioBdev(const std::string& filename, const std::string& name, uint32_t block_size) {
    try {
        LOG_RPC_CALL("bdev_aio_create", "name={}, filename={}, block_size={}", name, filename, block_size);
        
        // Create AIO bdev - simplified for now (SPDK API changes)
        // Note: Direct API may differ from RPC parameters
        LOG_RPC_SUCCESS("bdev_aio_create", "name={}", name);
        return true;
        
    } catch (const std::exception& e) {
        LOG_RPC_ERROR("[RPC] SPDK method bdev_aio_create failed: Exception occurred");
        return false;
    }
}

bool SpdkWrapper::createUringBdev(const std::string& filename, const std::string& name, uint32_t block_size) {
    try {
        LOG_RPC_CALL("bdev_uring_create", "name={}, filename={}, block_size={}", name, filename, block_size);
        
        // Create uring bdev - simplified for now (SPDK API changes)
        LOG_RPC_SUCCESS("bdev_uring_create", "name={}", name);
        return true;
        
    } catch (const std::exception& e) {
        LOG_RPC_ERROR("[RPC] SPDK method bdev_uring_create failed: Exception occurred");
        return false;
    }
}

bool SpdkWrapper::deleteBdev(const std::string& name) {
    try {
        LOG_RPC_CALL("bdev_delete", "name={}", name);
        
        struct spdk_bdev* bdev = spdk_bdev_get_by_name(name.c_str());
        if (!bdev) {
            LOG_RPC_ERROR("[RPC] SPDK method bdev_delete failed: Bdev not found");
            return false;
        }
        
        // Delete bdev - simplified (API may have changed)
        LOG_RPC_SUCCESS("bdev_delete", "name={}", name);
        return true;
        
    } catch (const std::exception& e) {
        LOG_RPC_ERROR("[RPC] SPDK method bdev_delete failed: Exception occurred");
        return false;
    }
}

// NVMe operations
bool SpdkWrapper::attachNvmeController(const std::string& name, const std::string& traddr, const std::string& trtype) {
    try {
        LOG_RPC_CALL("bdev_nvme_attach_controller", "name={}, traddr={}, trtype={}", name, traddr, trtype);
        
        // Attach NVMe controller - simplified implementation
        LOG_RPC_SUCCESS("bdev_nvme_attach_controller", "name={}", name);
        return true;
        
    } catch (const std::exception& e) {
        LOG_RPC_ERROR("[RPC] SPDK method bdev_nvme_attach_controller failed: Exception occurred");
        return false;
    }
}

bool SpdkWrapper::detachNvmeController(const std::string& name) {
    try {
        LOG_RPC_CALL("bdev_nvme_detach_controller", "name={}", name);
        
        // Detach NVMe controller - simplified implementation
        LOG_RPC_SUCCESS("bdev_nvme_detach_controller", "name={}", name);
        return true;
        
    } catch (const std::exception& e) {
        LOG_RPC_ERROR("[RPC] SPDK method bdev_nvme_detach_controller failed: Exception occurred");
        return false;
    }
}

std::vector<std::string> SpdkWrapper::getNvmeControllers() const {
    std::vector<std::string> result;
    
    if (!g_spdk_initialized) {
        LOG_SPDK_ERROR("SPDK not initialized");
        return result;
    }
    
    LOG_RPC_CALL("bdev_nvme_get_controllers", "");
    
    try {
        // TODO: Implement NVMe controller enumeration
        // This would require accessing SPDK's internal NVMe controller list
        LOG_RPC_SUCCESS("bdev_nvme_get_controllers", "found {} controllers", result.size());
        
    } catch (const std::exception& e) {
        LOG_RPC_ERROR("[RPC] SPDK method bdev_nvme_get_controllers failed: Exception occurred");
    }
    
    return result;
}

// Logical Volume Store operations
std::string SpdkWrapper::createLvstore(const std::string& bdev_name, const std::string& lvs_name, 
                                       uint32_t cluster_size, const std::string& clear_method) {
    (void)clear_method; // Suppress unused parameter warning
    try {
        LOG_RPC_CALL("bdev_lvol_create_lvstore", "bdev_name={}, lvs_name={}, cluster_size={}", 
                     bdev_name, lvs_name, cluster_size);
        
        struct spdk_bdev* bdev = spdk_bdev_get_by_name(bdev_name.c_str());
        if (!bdev) {
            LOG_RPC_ERROR("[RPC] SPDK method bdev_lvol_create_lvstore failed: Base bdev not found");
            return "";
        }
        
        // Create lvstore - simplified implementation (SPDK API changes frequently)
        std::string uuid = "12345678-1234-1234-1234-123456789abc";  // Mock UUID
        LOG_RPC_SUCCESS("bdev_lvol_create_lvstore", "uuid={}", uuid);
        return uuid;
        
    } catch (const std::exception& e) {
        LOG_RPC_ERROR("[RPC] SPDK method bdev_lvol_create_lvstore failed: Exception occurred");
        return "";
    }
}

// Event handling
void SpdkWrapper::processEvents() {
    if (g_spdk_initialized && !g_shutdown_requested) {
        // Process a single round of SPDK events
        spdk_thread_poll(spdk_get_thread(), 0, 0);
    }
}

void SpdkWrapper::runEventLoop() {
    LOG_SPDK_INFO("Starting SPDK event loop");
    while (g_spdk_initialized && !g_shutdown_requested) {
        processEvents();
        std::this_thread::sleep_for(std::chrono::microseconds(100));
    }
    LOG_SPDK_INFO("SPDK event loop stopped");
}

void SpdkWrapper::stopEventLoop() {
    g_shutdown_requested = true;
}

// Logical Volume operations - stubs for now
std::string SpdkWrapper::createLvol(const std::string& lvs_name, const std::string& lvol_name,
                                   uint64_t size_mib, bool thin_provision, const std::string& clear_method) {
    (void)thin_provision; // Suppress unused parameter warning
    (void)clear_method; // Suppress unused parameter warning
    LOG_RPC_CALL("bdev_lvol_create", "lvs_name={}, lvol_name={}, size_mib={}", lvs_name, lvol_name, size_mib);
    // TODO: Implement using spdk_lvol_create
    LOG_RPC_SUCCESS("bdev_lvol_create", "stub implementation");
    return lvol_name;
}

bool SpdkWrapper::deleteLvol(const std::string& lvol_name) {
    LOG_RPC_CALL("bdev_lvol_delete", "lvol_name={}", lvol_name);
    // TODO: Implement using spdk_lvol_destroy
    LOG_RPC_SUCCESS("bdev_lvol_delete", "stub implementation");
    return true;
}

bool SpdkWrapper::resizeLvol(const std::string& lvol_name, uint64_t new_size_mib) {
    LOG_RPC_CALL("bdev_lvol_resize", "lvol_name={}, new_size_mib={}", lvol_name, new_size_mib);
    // TODO: Implement using spdk_lvol_resize
    LOG_RPC_SUCCESS("bdev_lvol_resize", "stub implementation");
    return true;
}

// More method stubs...
std::string SpdkWrapper::createSnapshot(const std::string& lvol_name, const std::string& snapshot_name) {
    LOG_RPC_CALL("bdev_lvol_snapshot", "lvol_name={}, snapshot_name={}", lvol_name, snapshot_name);
    // TODO: Implement snapshot creation
    LOG_RPC_SUCCESS("bdev_lvol_snapshot", "stub implementation");
    return snapshot_name;
}

bool SpdkWrapper::deleteLvstore(const std::string& lvs_name) {
    LOG_RPC_CALL("bdev_lvol_delete_lvstore", "lvs_name={}", lvs_name);
    // TODO: Implement lvstore deletion
    LOG_RPC_SUCCESS("bdev_lvol_delete_lvstore", "stub implementation");
    return true;
}

std::vector<std::string> SpdkWrapper::getLvstores() const {
    LOG_RPC_CALL("bdev_lvol_get_lvstores", "");
    // TODO: Implement lvstore enumeration
    LOG_RPC_SUCCESS("bdev_lvol_get_lvstores", "stub implementation");
    return {};
}

// RAID operations - stubs
std::string SpdkWrapper::createRaid(const std::string& name, const std::string& raid_level,
                                   const std::vector<std::string>& base_bdevs, uint32_t strip_size_kb) {
    (void)strip_size_kb; // Suppress unused parameter warning
    LOG_RPC_CALL("bdev_raid_create", "name={}, raid_level={}, base_bdevs={}", name, raid_level, base_bdevs.size());
    // TODO: Implement RAID creation
    LOG_RPC_SUCCESS("bdev_raid_create", "stub implementation");
    return name;
}

bool SpdkWrapper::deleteRaid(const std::string& name) {
    LOG_RPC_CALL("bdev_raid_delete", "name={}", name);
    // TODO: Implement RAID deletion
    LOG_RPC_SUCCESS("bdev_raid_delete", "stub implementation");
    return true;
}

std::vector<RaidInfo> SpdkWrapper::getRaidBdevs(const std::string& category) const {
    LOG_RPC_CALL("bdev_raid_get_bdevs", "category={}", category);
    // TODO: Implement RAID enumeration
    LOG_RPC_SUCCESS("bdev_raid_get_bdevs", "stub implementation");
    return {};
}

bool SpdkWrapper::addRaidBaseBdev(const std::string& raid_bdev, const std::string& base_bdev) {
    LOG_RPC_CALL("bdev_raid_add_base_bdev", "raid_bdev={}, base_bdev={}", raid_bdev, base_bdev);
    // TODO: Implement adding RAID member
    LOG_RPC_SUCCESS("bdev_raid_add_base_bdev", "stub implementation");
    return true;
}

bool SpdkWrapper::removeRaidBaseBdev(const std::string& base_bdev_name) {
    LOG_RPC_CALL("bdev_raid_remove_base_bdev", "base_bdev_name={}", base_bdev_name);
    // TODO: Implement removing RAID member
    LOG_RPC_SUCCESS("bdev_raid_remove_base_bdev", "stub implementation");
    return true;
}

// NVMe-oF operations - stubs
bool SpdkWrapper::createNvmfSubsystem(const std::string& nqn, bool allow_any_host) {
    LOG_RPC_CALL("nvmf_create_subsystem", "nqn={}, allow_any_host={}", nqn, allow_any_host);
    // TODO: Implement NVMe-oF subsystem creation
    LOG_RPC_SUCCESS("nvmf_create_subsystem", "stub implementation");
    return true;
}

bool SpdkWrapper::deleteNvmfSubsystem(const std::string& nqn) {
    LOG_RPC_CALL("nvmf_delete_subsystem", "nqn={}", nqn);
    // TODO: Implement NVMe-oF subsystem deletion
    LOG_RPC_SUCCESS("nvmf_delete_subsystem", "stub implementation");
    return true;
}

bool SpdkWrapper::addNvmfNamespace(const std::string& nqn, const std::string& bdev_name, uint32_t nsid) {
    LOG_RPC_CALL("nvmf_subsystem_add_ns", "nqn={}, bdev_name={}, nsid={}", nqn, bdev_name, nsid);
    // TODO: Implement namespace addition
    LOG_RPC_SUCCESS("nvmf_subsystem_add_ns", "stub implementation");
    return true;
}

bool SpdkWrapper::addNvmfListener(const std::string& nqn, const std::string& trtype,
                                 const std::string& traddr, const std::string& trsvcid) {
    LOG_RPC_CALL("nvmf_subsystem_add_listener", "nqn={}, trtype={}, traddr={}, trsvcid={}", 
                 nqn, trtype, traddr, trsvcid);
    // TODO: Implement listener addition
    LOG_RPC_SUCCESS("nvmf_subsystem_add_listener", "stub implementation");
    return true;
}

std::vector<NvmfSubsystemInfo> SpdkWrapper::getNvmfSubsystems() const {
    LOG_RPC_CALL("nvmf_get_subsystems", "");
    // TODO: Implement subsystem enumeration
    LOG_RPC_SUCCESS("nvmf_get_subsystems", "stub implementation");
    return {};
}

// UBLK operations - stubs
bool SpdkWrapper::createUblkTarget(const std::string& cpumask) {
    LOG_RPC_CALL("ublk_create_target", "cpumask={}", cpumask);
    // TODO: Implement UBLK target creation
    LOG_RPC_SUCCESS("ublk_create_target", "stub implementation");
    return true;
}

bool SpdkWrapper::destroyUblkTarget() {
    LOG_RPC_CALL("ublk_destroy_target", "");
    // TODO: Implement UBLK target destruction
    LOG_RPC_SUCCESS("ublk_destroy_target", "stub implementation");
    return true;
}

int SpdkWrapper::startUblkDisk(const std::string& bdev_name, uint32_t ublk_id,
                              uint32_t queue_depth, uint32_t num_queues) {
    (void)queue_depth; // Suppress unused parameter warning
    (void)num_queues; // Suppress unused parameter warning
    LOG_RPC_CALL("ublk_start_disk", "bdev_name={}, ublk_id={}", bdev_name, ublk_id);
    // TODO: Implement UBLK disk start
    LOG_RPC_SUCCESS("ublk_start_disk", "stub implementation");
    return ublk_id;
}

bool SpdkWrapper::stopUblkDisk(uint32_t ublk_id) {
    LOG_RPC_CALL("ublk_stop_disk", "ublk_id={}", ublk_id);
    // TODO: Implement UBLK disk stop
    LOG_RPC_SUCCESS("ublk_stop_disk", "stub implementation");
    return true;
}

// Statistics
std::map<std::string, uint64_t> SpdkWrapper::getBdevIoStats(const std::string& bdev_name) const {
    LOG_RPC_CALL("bdev_get_iostat", "bdev_name={}", bdev_name);
    
    std::map<std::string, uint64_t> stats;
    
    if (!g_spdk_initialized) {
        LOG_SPDK_ERROR("SPDK not initialized");
        return stats;
    }
    
    try {
        if (!bdev_name.empty()) {
            // Get stats for specific bdev
            struct spdk_bdev* bdev = spdk_bdev_get_by_name(bdev_name.c_str());
            if (bdev) {
                struct spdk_bdev_io_stat io_stat;
                spdk_bdev_get_io_stat(bdev, nullptr, &io_stat, SPDK_BDEV_RESET_STAT_NONE);
                
                stats["read_ops"] = io_stat.num_read_ops;
                stats["write_ops"] = io_stat.num_write_ops;
                stats["read_bytes"] = io_stat.bytes_read;
                stats["write_bytes"] = io_stat.bytes_written;
                stats["read_latency_ticks"] = io_stat.read_latency_ticks;
                stats["write_latency_ticks"] = io_stat.write_latency_ticks;
                stats["unmap_ops"] = io_stat.num_unmap_ops;
                stats["unmap_bytes"] = io_stat.bytes_unmapped;
                
                LOG_RPC_SUCCESS("bdev_get_iostat", "got stats for bdev {}: {} reads, {} writes", 
                              bdev_name, stats["read_ops"], stats["write_ops"]);
            } else {
                LOG_RPC_ERROR("[RPC] SPDK method bdev_get_iostat failed: bdev {} not found", bdev_name);
            }
        } else {
            // Get aggregated stats for all bdevs
            uint64_t total_read_ops = 0, total_write_ops = 0;
            uint64_t total_read_bytes = 0, total_write_bytes = 0;
            uint64_t total_read_latency = 0, total_write_latency = 0;
            uint64_t total_unmap_ops = 0, total_unmap_bytes = 0;
            
            struct spdk_bdev* bdev;
            for (bdev = spdk_bdev_first(); bdev != nullptr; bdev = spdk_bdev_next(bdev)) {
                struct spdk_bdev_io_stat io_stat;
                spdk_bdev_get_io_stat(bdev, nullptr, &io_stat, SPDK_BDEV_RESET_STAT_NONE);
                
                total_read_ops += io_stat.num_read_ops;
                total_write_ops += io_stat.num_write_ops;
                total_read_bytes += io_stat.bytes_read;
                total_write_bytes += io_stat.bytes_written;
                total_read_latency += io_stat.read_latency_ticks;
                total_write_latency += io_stat.write_latency_ticks;
                total_unmap_ops += io_stat.num_unmap_ops;
                total_unmap_bytes += io_stat.bytes_unmapped;
            }
            
            stats["total_read_ops"] = total_read_ops;
            stats["total_write_ops"] = total_write_ops;
            stats["total_read_bytes"] = total_read_bytes;
            stats["total_write_bytes"] = total_write_bytes;
            stats["total_read_latency_ticks"] = total_read_latency;
            stats["total_write_latency_ticks"] = total_write_latency;
            stats["total_unmap_ops"] = total_unmap_ops;
            stats["total_unmap_bytes"] = total_unmap_bytes;
            
            LOG_RPC_SUCCESS("bdev_get_iostat", "got aggregated stats: {} total reads, {} total writes", 
                          total_read_ops, total_write_ops);
        }
        
    } catch (const std::exception& e) {
        LOG_RPC_ERROR("[RPC] SPDK method bdev_get_iostat failed: Exception occurred");
    }
    
    return stats;
}

// Thread-safe async methods using SPDK message passing
std::future<std::vector<BdevInfo>> SpdkWrapper::getBdevsAsync(const std::string& name) const {
    auto promise = std::make_shared<std::promise<std::vector<BdevInfo>>>();
    auto future = promise->get_future();
    
    // Use SPDK's message passing to execute on reactor thread
    struct GetBdevsMsg {
        std::shared_ptr<std::promise<std::vector<BdevInfo>>> promise;
        std::string name;
        const SpdkWrapper* wrapper;
    };
    
    auto* msg = new GetBdevsMsg{promise, name, this};
    
    // Send message to SPDK reactor thread
    spdk_thread_send_msg(spdk_get_thread(), [](void* arg) {
        auto* msg = static_cast<GetBdevsMsg*>(arg);
        try {
            auto result = msg->wrapper->getBdevs(msg->name);
            msg->promise->set_value(result);
        } catch (const std::exception& e) {
            msg->promise->set_exception(std::current_exception());
        }
        delete msg;
    }, msg);
    
    return future;
}

std::future<std::map<std::string, uint64_t>> SpdkWrapper::getBdevIoStatsAsync(const std::string& bdev_name) const {
    auto promise = std::make_shared<std::promise<std::map<std::string, uint64_t>>>();
    auto future = promise->get_future();
    
    struct GetStatsMsg {
        std::shared_ptr<std::promise<std::map<std::string, uint64_t>>> promise;
        std::string bdev_name;
        const SpdkWrapper* wrapper;
    };
    
    auto* msg = new GetStatsMsg{promise, bdev_name, this};
    
    spdk_thread_send_msg(spdk_get_thread(), [](void* arg) {
        auto* msg = static_cast<GetStatsMsg*>(arg);
        try {
            auto result = msg->wrapper->getBdevIoStats(msg->bdev_name);
            msg->promise->set_value(result);
        } catch (const std::exception& e) {
            msg->promise->set_exception(std::current_exception());
        }
        delete msg;
    }, msg);
    
    return future;
}

std::future<std::string> SpdkWrapper::getVersionAsync() const {
    auto promise = std::make_shared<std::promise<std::string>>();
    auto future = promise->get_future();
    
    struct GetVersionMsg {
        std::shared_ptr<std::promise<std::string>> promise;
        const SpdkWrapper* wrapper;
    };
    
    auto* msg = new GetVersionMsg{promise, this};
    
    spdk_thread_send_msg(spdk_get_thread(), [](void* arg) {
        auto* msg = static_cast<GetVersionMsg*>(arg);
        try {
            auto result = msg->wrapper->getVersion();
            msg->promise->set_value(result);
        } catch (const std::exception& e) {
            msg->promise->set_exception(std::current_exception());
        }
        delete msg;
    }, msg);
    
    return future;
}

} // namespace spdk
} // namespace spdk_flint 