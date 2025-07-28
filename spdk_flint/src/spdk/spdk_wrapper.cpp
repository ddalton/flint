#include "spdk/spdk_wrapper.hpp"
#include "logging.hpp"
#include <future>
#include <thread>
#include <chrono>
#include <cstring>
#include <atomic>

namespace spdk_flint {
namespace spdk {

// Global state for SPDK application
static std::atomic<bool> g_spdk_initialized{false};
static std::atomic<bool> g_spdk_shutdown_requested{false};
static struct spdk_app_opts* g_spdk_opts = nullptr;

// SPDK application callbacks
static void spdk_app_started(void* arg) {
    auto* wrapper = static_cast<SpdkWrapper*>(arg);
    LOG_SPDK_INFO("SPDK application started successfully");
    g_spdk_initialized = true;
}

static void spdk_app_stopped(void* arg, int rc) {
    LOG_SPDK_INFO("SPDK application stopped with code {}", rc);
    g_spdk_initialized = false;
}

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
    if (initialized_) {
        LOG_SPDK_WARN("SPDK already initialized");
        return true;
    }
    
    try {
        LOG_SPDK_INFO("Initializing SPDK");
        
        // Allocate SPDK options
        opts_ = SPDK_CALLOC(1, sizeof(struct spdk_app_opts), SPDK_MEMPOOL_DEFAULT_CACHE_SIZE, SPDK_ENV_SOCKET_ID_ANY);
        if (!opts_) {
            LOG_SPDK_ERROR("Failed to allocate SPDK options");
            return false;
        }
        
        // Initialize default options
        spdk_app_opts_init(opts_, sizeof(*opts_));
        
        // Configure SPDK options
        opts_->name = "spdk_flint";
        opts_->config_file = config_file_.empty() ? nullptr : config_file_.c_str();
        opts_->rpc_addr = "/var/tmp/spdk.sock";  // Enable RPC for compatibility
        opts_->reactor_mask = "0x1";  // Use single core for simplicity
        opts_->mem_size = 1024;  // 1GB memory pool
        
        // Start SPDK application
        LOG_SPDK_INFO("Starting SPDK application");
        int rc = spdk_app_start(opts_, spdk_app_started, this);
        if (rc != 0) {
            LOG_SPDK_ERROR("Failed to start SPDK application: {}", rc);
            return false;
        }
        
        // Wait for SPDK to be ready
        auto start_time = std::chrono::steady_clock::now();
        while (!g_spdk_initialized.load() && 
               std::chrono::steady_clock::now() - start_time < std::chrono::seconds(30)) {
            std::this_thread::sleep_for(std::chrono::milliseconds(100));
        }
        
        if (!g_spdk_initialized.load()) {
            LOG_SPDK_ERROR("SPDK initialization timeout");
            return false;
        }
        
        initialized_ = true;
        LOG_SPDK_INFO("SPDK initialized successfully");
        return true;
        
    } catch (const std::exception& e) {
        LOG_SPDK_ERROR("SPDK initialization failed: {}", e.what());
        return false;
    }
}

void SpdkWrapper::shutdown() {
    if (!initialized_) {
        return;
    }
    
    LOG_SPDK_INFO("Shutting down SPDK");
    
    g_spdk_shutdown_requested = true;
    spdk_app_stop(0);
    
    // Wait for shutdown
    auto start_time = std::chrono::steady_clock::now();
    while (g_spdk_initialized.load() && 
           std::chrono::steady_clock::now() - start_time < std::chrono::seconds(10)) {
        std::this_thread::sleep_for(std::chrono::milliseconds(100));
    }
    
    if (opts_) {
        spdk_free(opts_);
        opts_ = nullptr;
    }
    
    initialized_ = false;
    LOG_SPDK_INFO("SPDK shutdown complete");
}

// Version information
std::string SpdkWrapper::getVersion() const {
    // Direct SPDK version query - replace spdk_get_version RPC
    return "SPDK " SPDK_VERSION_STRING;
}

// Block device operations
std::vector<BdevInfo> SpdkWrapper::getBdevs(const std::string& name) const {
    std::vector<BdevInfo> result;
    
    if (!initialized_) {
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
                struct spdk_uuid uuid;
                if (spdk_bdev_get_uuid(bdev, &uuid) == 0) {
                    char uuid_str[SPDK_UUID_STRING_LEN];
                    spdk_uuid_fmt_lower(uuid_str, sizeof(uuid_str), &uuid);
                    info.uuid = uuid_str;
                }
                
                info.claimed = spdk_bdev_is_claimed(bdev);
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
                
                struct spdk_uuid uuid;
                if (spdk_bdev_get_uuid(bdev, &uuid) == 0) {
                    char uuid_str[SPDK_UUID_STRING_LEN];
                    spdk_uuid_fmt_lower(uuid_str, sizeof(uuid_str), &uuid);
                    info.uuid = uuid_str;
                }
                
                info.claimed = spdk_bdev_is_claimed(bdev);
                result.push_back(info);
            }
        }
        
        LOG_RPC_SUCCESS("bdev_get_bdevs", "found {} bdevs", result.size());
        
    } catch (const std::exception& e) {
        LOG_RPC_ERROR("bdev_get_bdevs", e.what());
    }
    
    return result;
}

bool SpdkWrapper::createAioBdev(const std::string& name, const std::string& filename, uint32_t block_size) {
    if (!initialized_) {
        LOG_SPDK_ERROR("SPDK not initialized");
        return false;
    }
    
    LOG_RPC_CALL("bdev_aio_create", "name={}, filename={}, block_size={}", name, filename, block_size);
    
    try {
        // Direct SPDK bdev creation - replaces bdev_aio_create RPC
        struct spdk_bdev* bdev = spdk_bdev_aio_create(filename.c_str(), name.c_str(), block_size);
        if (!bdev) {
            LOG_RPC_ERROR("bdev_aio_create", "Failed to create AIO bdev");
            return false;
        }
        
        LOG_RPC_SUCCESS("bdev_aio_create", "created AIO bdev {}", name);
        return true;
        
    } catch (const std::exception& e) {
        LOG_RPC_ERROR("bdev_aio_create", e.what());
        return false;
    }
}

bool SpdkWrapper::createUringBdev(const std::string& name, const std::string& filename, uint32_t block_size) {
    if (!initialized_) {
        LOG_SPDK_ERROR("SPDK not initialized");
        return false;
    }
    
    LOG_RPC_CALL("bdev_uring_create", "name={}, filename={}, block_size={}", name, filename, block_size);
    
    try {
        // Direct SPDK uring bdev creation - replaces bdev_uring_create RPC
        int rc = spdk_bdev_uring_create(filename.c_str(), name.c_str(), block_size);
        if (rc != 0) {
            LOG_RPC_ERROR("bdev_uring_create", "Failed with code {}", rc);
            return false;
        }
        
        LOG_RPC_SUCCESS("bdev_uring_create", "created uring bdev {}", name);
        return true;
        
    } catch (const std::exception& e) {
        LOG_RPC_ERROR("bdev_uring_create", e.what());
        return false;
    }
}

bool SpdkWrapper::deleteBdev(const std::string& name) {
    if (!initialized_) {
        LOG_SPDK_ERROR("SPDK not initialized");
        return false;
    }
    
    LOG_RPC_CALL("bdev_delete", "name={}", name);
    
    try {
        struct spdk_bdev* bdev = spdk_bdev_get_by_name(name.c_str());
        if (!bdev) {
            LOG_RPC_ERROR("bdev_delete", "Bdev {} not found", name);
            return false;
        }
        
        // Delete bdev
        spdk_bdev_unregister(bdev, nullptr, nullptr);
        
        LOG_RPC_SUCCESS("bdev_delete", "deleted bdev {}", name);
        return true;
        
    } catch (const std::exception& e) {
        LOG_RPC_ERROR("bdev_delete", e.what());
        return false;
    }
}

// NVMe operations
bool SpdkWrapper::attachNvmeController(const std::string& name, const std::string& traddr, const std::string& trtype) {
    if (!initialized_) {
        LOG_SPDK_ERROR("SPDK not initialized");
        return false;
    }
    
    LOG_RPC_CALL("bdev_nvme_attach_controller", "name={}, traddr={}, trtype={}", name, traddr, trtype);
    
    try {
        struct spdk_nvme_transport_id trid = {};
        
        // Parse transport type
        if (trtype == "PCIe") {
            trid.trtype = SPDK_NVME_TRANSPORT_PCIE;
        } else if (trtype == "TCP") {
            trid.trtype = SPDK_NVME_TRANSPORT_TCP;
        } else if (trtype == "RDMA") {
            trid.trtype = SPDK_NVME_TRANSPORT_RDMA;
        } else {
            LOG_RPC_ERROR("bdev_nvme_attach_controller", "Unsupported transport type: {}", trtype);
            return false;
        }
        
        // Set transport address
        snprintf(trid.traddr, sizeof(trid.traddr), "%s", traddr.c_str());
        snprintf(trid.subnqn, sizeof(trid.subnqn), SPDK_NVMF_DISCOVERY_NQN);
        
        // Attach controller
        int rc = spdk_bdev_nvme_create(&trid, name.c_str(), nullptr, 0, nullptr, 0, 0, nullptr, nullptr, false);
        if (rc != 0) {
            LOG_RPC_ERROR("bdev_nvme_attach_controller", "Failed with code {}", rc);
            return false;
        }
        
        LOG_RPC_SUCCESS("bdev_nvme_attach_controller", "attached controller {}", name);
        return true;
        
    } catch (const std::exception& e) {
        LOG_RPC_ERROR("bdev_nvme_attach_controller", e.what());
        return false;
    }
}

bool SpdkWrapper::detachNvmeController(const std::string& name) {
    if (!initialized_) {
        LOG_SPDK_ERROR("SPDK not initialized");
        return false;
    }
    
    LOG_RPC_CALL("bdev_nvme_detach_controller", "name={}", name);
    
    try {
        int rc = spdk_bdev_nvme_delete(name.c_str(), nullptr, nullptr);
        if (rc != 0) {
            LOG_RPC_ERROR("bdev_nvme_detach_controller", "Failed with code {}", rc);
            return false;
        }
        
        LOG_RPC_SUCCESS("bdev_nvme_detach_controller", "detached controller {}", name);
        return true;
        
    } catch (const std::exception& e) {
        LOG_RPC_ERROR("bdev_nvme_detach_controller", e.what());
        return false;
    }
}

std::vector<std::string> SpdkWrapper::getNvmeControllers() const {
    std::vector<std::string> result;
    
    if (!initialized_) {
        LOG_SPDK_ERROR("SPDK not initialized");
        return result;
    }
    
    LOG_RPC_CALL("bdev_nvme_get_controllers", "");
    
    try {
        // TODO: Implement NVMe controller enumeration
        // This would require accessing SPDK's internal NVMe controller list
        LOG_RPC_SUCCESS("bdev_nvme_get_controllers", "found {} controllers", result.size());
        
    } catch (const std::exception& e) {
        LOG_RPC_ERROR("bdev_nvme_get_controllers", e.what());
    }
    
    return result;
}

// Logical Volume Store operations
std::string SpdkWrapper::createLvstore(const std::string& bdev_name, const std::string& lvs_name,
                                      uint32_t cluster_size, const std::string& clear_method) {
    if (!initialized_) {
        LOG_SPDK_ERROR("SPDK not initialized");
        return "";
    }
    
    LOG_RPC_CALL("bdev_lvol_create_lvstore", "bdev_name={}, lvs_name={}, cluster_size={}", 
                 bdev_name, lvs_name, cluster_size);
    
    try {
        struct spdk_bdev* bdev = spdk_bdev_get_by_name(bdev_name.c_str());
        if (!bdev) {
            LOG_RPC_ERROR("bdev_lvol_create_lvstore", "Base bdev {} not found", bdev_name);
            return "";
        }
        
        // Create lvstore
        struct spdk_lvol_store* lvs = nullptr;
        int rc = spdk_lvs_init(bdev, cluster_size, lvs_name.c_str(), 100, &lvs);
        if (rc != 0) {
            LOG_RPC_ERROR("bdev_lvol_create_lvstore", "Failed with code {}", rc);
            return "";
        }
        
        // Get UUID
        struct spdk_uuid uuid = spdk_lvs_get_uuid(lvs);
        char uuid_str[SPDK_UUID_STRING_LEN];
        spdk_uuid_fmt_lower(uuid_str, sizeof(uuid_str), &uuid);
        
        LOG_RPC_SUCCESS("bdev_lvol_create_lvstore", "created lvstore {} with UUID {}", lvs_name, uuid_str);
        return std::string(uuid_str);
        
    } catch (const std::exception& e) {
        LOG_RPC_ERROR("bdev_lvol_create_lvstore", e.what());
        return "";
    }
}

// Event handling
void SpdkWrapper::processEvents() {
    if (initialized_ && !g_spdk_shutdown_requested.load()) {
        // Process a single round of SPDK events
        spdk_thread_poll(spdk_get_thread(), 0, 0);
    }
}

void SpdkWrapper::runEventLoop() {
    LOG_SPDK_INFO("Starting SPDK event loop");
    while (initialized_ && !g_spdk_shutdown_requested.load()) {
        processEvents();
        std::this_thread::sleep_for(std::chrono::microseconds(100));
    }
    LOG_SPDK_INFO("SPDK event loop stopped");
}

void SpdkWrapper::stopEventLoop() {
    g_spdk_shutdown_requested = true;
}

// Logical Volume operations - stubs for now
std::string SpdkWrapper::createLvol(const std::string& lvs_name, const std::string& lvol_name,
                                   uint64_t size_mib, bool thin_provision, const std::string& clear_method) {
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
    
    if (!initialized_) {
        LOG_SPDK_ERROR("SPDK not initialized");
        return stats;
    }
    
    try {
        if (!bdev_name.empty()) {
            // Get stats for specific bdev
            struct spdk_bdev* bdev = spdk_bdev_get_by_name(bdev_name.c_str());
            if (bdev) {
                struct spdk_bdev_io_stat io_stat;
                spdk_bdev_get_io_stat(bdev, &io_stat);
                
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
                LOG_RPC_ERROR("bdev_get_iostat", "bdev {} not found", bdev_name);
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
                spdk_bdev_get_io_stat(bdev, &io_stat);
                
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
        LOG_RPC_ERROR("bdev_get_iostat", e.what());
    }
    
    return stats;
}

} // namespace spdk
} // namespace spdk_flint 