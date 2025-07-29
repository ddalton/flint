#include "spdk/spdk_wrapper.hpp"
// Undefine syslog macros that conflict with our logging system
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
#include <sys/stat.h>    // For stat()
#include <sstream>       // For string concatenation

// Additional SPDK headers for direct C API calls
extern "C" {
#include <spdk/bdev_module.h>
#include <spdk/vmd.h>
#include <spdk/nvme_spec.h>
#include <spdk/version.h>
#include <spdk/blobstore.h>

// Internal SPDK headers for complete struct definitions
// From https://raw.githubusercontent.com/spdk/spdk/refs/heads/v25.05.x/include/spdk_internal/lvolstore.h
#include "spdk_internal/lvolstore.h"
// From https://raw.githubusercontent.com/spdk/spdk/refs/heads/v25.05.x/module/bdev/lvol/vbdev_lvol.h
// This provides lvol_store_bdev struct with lvs pointer
#include "spdk/bdev_module.h"

// Forward declarations for NVMe controller functions
// These are internal SPDK functions from module/bdev/nvme/bdev_nvme_rpc.c
struct nvme_bdev_ctrlr;
extern struct nvme_bdev_ctrlr* nvme_bdev_ctrlr_get_by_name(const char* name);
extern struct nvme_bdev_ctrlr* nvme_bdev_ctrlr_first(void);
extern struct nvme_bdev_ctrlr* nvme_bdev_ctrlr_next(struct nvme_bdev_ctrlr* ctrlr);

// LVol store functions - these are confirmed to exist in SPDK source
// From module/bdev/lvol/vbdev_lvol_rpc.c: vbdev_get_lvol_store_by_uuid_xor_name
extern struct spdk_lvol_store* vbdev_get_lvol_store_by_uuid(const struct spdk_uuid* uuid);
extern struct spdk_lvol_store* vbdev_get_lvol_store_by_name(const char* name);

// Iterator functions from spdk_internal/lvolstore.h - working with lvol_store_bdev
// From https://raw.githubusercontent.com/spdk/spdk/refs/heads/v25.05.x/module/bdev/lvol/vbdev_lvol.h
struct lvol_store_bdev;
extern struct lvol_store_bdev* vbdev_lvol_store_first(void);
extern struct lvol_store_bdev* vbdev_lvol_store_next(struct lvol_store_bdev* prev);

// LVol store creation/destruction functions
extern int vbdev_lvs_create_ext(const char* base_bdev_name, const char* name, 
                               uint32_t cluster_sz, enum lvs_clear_method clear_method,
                               uint32_t num_md_pages_per_cluster_ratio, uint32_t md_page_size,
                               void (*cb_fn)(void*, struct spdk_lvol_store*, int), void* cb_arg);
extern void vbdev_lvs_destruct(struct spdk_lvol_store* lvs, 
                              void (*cb_fn)(void*, int), void* cb_arg);

// Helper function to get lvol_store_bdev from spdk_lvol_store (from vbdev_lvol.h)
extern struct lvol_store_bdev* vbdev_get_lvs_bdev_by_lvs(struct spdk_lvol_store* lvs);

// NVMe controller attach function from module/bdev/nvme/bdev_nvme_rpc.c
extern int bdev_nvme_attach_controller(const char* name, const char* trtype, const char* traddr,
                                      const char* adrfam, const char* trsvcid, const char* priority,
                                      const char* subnqn, const char* hostnqn,
                                      const char* hostaddr, const char* hostsvcid,
                                      bool multipath, uint32_t num_io_queues,
                                      uint32_t ctrlr_loss_timeout_sec, uint32_t reconnect_delay_sec,
                                      uint32_t fast_io_fail_timeout_sec,
                                      void (*cb_fn)(void*, size_t, int), void* cb_arg);

// Clear method enum (from SPDK source)
enum lvs_clear_method {
    LVS_CLEAR_WITH_NONE,
    LVS_CLEAR_WITH_UNMAP,
    LVS_CLEAR_WITH_WRITE_ZEROES,
};
}

namespace spdk_flint {
namespace spdk {

// Global SPDK application context
static bool g_spdk_initialized = false;

// SPDK application callbacks
static void spdk_app_started(void* arg) {
    auto* wrapper = static_cast<SpdkWrapper*>(arg);
    (void)wrapper; // Suppress unused variable warning
    spdk_flint::logger()->info("[SPDK] Application started successfully - reactor framework active");
    spdk_flint::logger()->debug("[SPDK] SPDK version: {}", spdk_version_string());
    spdk_flint::logger()->debug("[SPDK] Current thread: {}", fmt::ptr(spdk_get_thread()));
    g_spdk_initialized = true;
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
        std::string message = fmt::format("{} failed with errno {} ({})", operation, rc, strerror(-rc));
        spdk_flint::logger()->error("[SPDK] {}", message);
        spdk_flint::logger()->debug("[SPDK] Error details - Operation: '{}', Return code: {}, SPDK error: {}", 
                                   operation, rc, static_cast<int>(error));
        throw SpdkException(error, message);
    }
}

// Constructor/Destructor
SpdkWrapper::SpdkWrapper(const std::string& config_file) 
    : config_file_(config_file) {
    spdk_flint::logger()->info("[SPDK] Creating SPDK wrapper for Node Agent");
    spdk_flint::logger()->debug("[SPDK] Configuration file: '{}'", config_file_.empty() ? "none" : config_file_);
    spdk_flint::logger()->debug("[SPDK] Thread ID: {}", spdk_flint::current_thread_id());
}

SpdkWrapper::~SpdkWrapper() {
    spdk_flint::logger()->debug("[SPDK] Destroying SPDK wrapper");
    shutdown();
}

// Initialization
bool SpdkWrapper::initialize() {
    auto start_time = std::chrono::steady_clock::now();
    
    if (g_spdk_initialized) {
        spdk_flint::logger()->warn("[SPDK] SPDK already initialized - skipping duplicate initialization");
        return true;
    }

    try {
        spdk_flint::logger()->info("[SPDK] Initializing embedded SPDK application framework");
        spdk_flint::logger()->debug("[SPDK] Process ID: {}, Thread ID: {}", getpid(), spdk_flint::current_thread_id());
        
        // Allocate and initialize SPDK options
        spdk_flint::logger()->debug("[SPDK] Allocating SPDK application options structure");
        opts_ = reinterpret_cast<struct spdk_app_opts*>(spdk_dma_zmalloc(sizeof(struct spdk_app_opts), 64, nullptr));
        if (!opts_) {
            spdk_flint::logger()->error("[SPDK] Failed to allocate SPDK options - out of memory");
            return false;
        }

        spdk_app_opts_init(opts_, sizeof(struct spdk_app_opts));
        opts_->name = "spdk_flint_node_agent";
        opts_->json_config_file = config_file_.empty() ? nullptr : config_file_.c_str();
        opts_->reactor_mask = "0x1";  // Use single core for simplicity
        opts_->mem_size = 512;  // 512MB
        opts_->no_pci = false;
        opts_->delay_subsystem_init = true;
        
        spdk_flint::logger()->debug("[SPDK] SPDK options configured:");
        // Copy packed fields to temporary variables for logging
        const char* name = opts_->name;
        const char* config_file = opts_->json_config_file;
        const char* reactor_mask = opts_->reactor_mask;
        int mem_size = opts_->mem_size;
        
        spdk_flint::logger()->debug("[SPDK]   - Name: {}", name ? name : "null");
        spdk_flint::logger()->debug("[SPDK]   - Config file: {}", config_file ? config_file : "none");
        spdk_flint::logger()->debug("[SPDK]   - Reactor mask: {}", reactor_mask ? reactor_mask : "null");
        spdk_flint::logger()->debug("[SPDK]   - Memory size: {} MB", mem_size);
        spdk_flint::logger()->debug("[SPDK]   - PCI access: {}", opts_->no_pci ? "disabled" : "enabled");
        
        // Use SPDK's proper application startup - this sets up signal handlers
        spdk_flint::logger()->info("[SPDK] Starting SPDK application with embedded signal handling");
        int rc = spdk_app_start(opts_, spdk_app_started, this);
        if (rc != 0) {
            spdk_flint::logger()->error("[SPDK] Failed to start SPDK application: {} ({})", rc, strerror(-rc));
            if (opts_) {
                spdk_dma_free(opts_);
                opts_ = nullptr;
            }
            return false;
        }

        auto duration = std::chrono::duration_cast<std::chrono::milliseconds>(
            std::chrono::steady_clock::now() - start_time);
        spdk_flint::logger()->info("[SPDK] Embedded SPDK application framework initialized successfully in {} ms", duration.count());
        spdk_flint::logger()->debug("[SPDK] Available subsystems: bdev, nvmf, ublk");
        spdk_flint::logger()->debug("[SPDK] Signal handling: SPDK-managed (SIGTERM, SIGINT)");
        return true;
        
    } catch (const std::exception& e) {
        spdk_flint::logger()->error("[SPDK] Exception during SPDK initialization: {}", e.what());
        if (opts_) {
            spdk_dma_free(opts_);
            opts_ = nullptr;
        }
        return false;
    }
}

void SpdkWrapper::shutdown() {
    if (!g_spdk_initialized) {
        spdk_flint::logger()->debug("[SPDK] Shutdown called but SPDK not initialized - skipping");
        return;
    }

    auto start_time = std::chrono::steady_clock::now();

    try {
        spdk_flint::logger()->info("[SPDK] Initiating embedded SPDK shutdown sequence");
        
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
        
        // Request shutdown - SPDK will handle the actual cleanup
        spdk_flint::logger()->debug("[SPDK] Requesting SPDK application stop");
        spdk_app_stop(0);
        
        // Wait for SPDK to finish shutdown
        spdk_flint::logger()->debug("[SPDK] Waiting for SPDK application finalization");
        spdk_app_fini();
        
        if (opts_) {
            spdk_dma_free(opts_);
            opts_ = nullptr;
        }
        
        g_spdk_initialized = false;
        auto duration = std::chrono::duration_cast<std::chrono::milliseconds>(
            std::chrono::steady_clock::now() - start_time);
        spdk_flint::logger()->info("[SPDK] Embedded SPDK shutdown complete in {} ms", duration.count());
        
    } catch (const std::exception& e) {
        spdk_flint::logger()->error("[SPDK] Exception during SPDK shutdown: {}", e.what());
    }
}

bool SpdkWrapper::isInitialized() const {
    bool initialized = g_spdk_initialized;
    spdk_flint::logger()->debug("[SPDK] Initialization status check: {}", initialized ? "initialized" : "not initialized");
    return initialized;
}

std::string SpdkWrapper::getVersion() const {
    std::string version = "SPDK 25.05";
    spdk_flint::logger()->debug("[SPDK] Version query: {}", version);
    return version;
}

// ===== NODE AGENT DIRECT SPDK C API IMPLEMENTATIONS =====

// Helper function to get LVS by UUID or name (equivalent to vbdev_get_lvol_store_by_uuid_xor_name)
static struct spdk_lvol_store* get_lvol_store_by_uuid_or_name(const std::string& uuid, const std::string& lvs_name) {
    spdk_flint::logger()->debug("[SPDK] Looking up LVS - UUID: '{}', Name: '{}'", uuid, lvs_name);
    
    if (uuid.empty() && lvs_name.empty()) {
        spdk_flint::logger()->error("[SPDK] LVS lookup failed: neither UUID nor name specified");
        return nullptr;
    } else if (!uuid.empty() && !lvs_name.empty()) {
        spdk_flint::logger()->error("[SPDK] LVS lookup failed: both UUID '{}' and name '{}' specified", uuid, lvs_name);
        return nullptr;
    }
    
    struct spdk_lvol_store* lvs = nullptr;
    if (!uuid.empty()) {
        // Parse UUID and get LVS by UUID
        struct spdk_uuid spdk_uuid;
        if (spdk_uuid_parse(&spdk_uuid, uuid.c_str()) != 0) {
            spdk_flint::logger()->error("[SPDK] LVS lookup failed: invalid UUID format '{}'", uuid);
            return nullptr;
        }
        spdk_flint::logger()->debug("[SPDK] Searching LVS by UUID: {}", uuid);
        lvs = vbdev_get_lvol_store_by_uuid(&spdk_uuid);
        if (lvs == nullptr) {
            spdk_flint::logger()->warn("[SPDK] LVS with UUID '{}' not found", uuid);
        } else {
            // Log basic info - name field access from spdk_internal/lvolstore.h
            spdk_flint::logger()->debug("[SPDK] Found LVS by UUID: {} (name: '{}')", uuid, lvs->name);
        }
    } else {
        spdk_flint::logger()->debug("[SPDK] Searching LVS by name: {}", lvs_name);
        lvs = vbdev_get_lvol_store_by_name(lvs_name.c_str());
        if (lvs == nullptr) {
            spdk_flint::logger()->warn("[SPDK] LVS with name '{}' not found", lvs_name);
        } else {
            // Log UUID info - UUID field access from spdk_internal/lvolstore.h
            char uuid_str[SPDK_UUID_STRING_LEN];
            spdk_uuid_fmt_lower(uuid_str, sizeof(uuid_str), &lvs->uuid);
            spdk_flint::logger()->debug("[SPDK] Found LVS by name: {} (UUID: {})", lvs_name, uuid_str);
        }
    }
    
    return lvs;
}

// LVol Store Operations Implementation

void SpdkWrapper::getLvolStoresAsync(
    const std::string& uuid,
    const std::string& lvs_name,
    std::function<void(const std::vector<LvolStoreInfo>&, int)> callback) {
    
    auto start_time = std::chrono::steady_clock::now();
    spdk_flint::logger()->info("[SPDK] Getting LVol stores - UUID: '{}', Name: '{}'", uuid, lvs_name);
    spdk_flint::logger()->debug("[SPDK] Callback provided: {}", callback ? "yes" : "no");
    
    // Context for async operation
    struct GetLvolStoresCtx {
        std::function<void(const std::vector<LvolStoreInfo>&, int)> callback;
        std::string uuid;
        std::string lvs_name;
        std::chrono::steady_clock::time_point start_time;
    };
    
    auto* ctx = new GetLvolStoresCtx{callback, uuid, lvs_name, start_time};
    spdk_flint::logger()->debug("[SPDK] Created async context for LVS enumeration");
    
    // Submit to SPDK reactor thread
    spdk_thread_send_msg(spdk_get_thread(), [](void* arg) {
        auto* ctx = static_cast<GetLvolStoresCtx*>(arg);
        std::vector<LvolStoreInfo> lvs_list;
        int error = 0;
        
        spdk_flint::logger()->debug("[SPDK] Executing LVS enumeration on reactor thread");
        
        try {
            if (ctx->uuid.empty() && ctx->lvs_name.empty()) {
                // Return all LVS stores using vbdev_lvol_store_first() and vbdev_lvol_store_next()
                spdk_flint::logger()->debug("[SPDK] Enumerating all LVol stores using iterator");
                
                // Enumerate all LVol stores using lvol_store_bdev iterator
                // From https://raw.githubusercontent.com/spdk/spdk/refs/heads/v25.05.x/module/bdev/lvol/vbdev_lvol.h
                spdk_flint::logger()->debug("[SPDK] Enumerating all LVol stores using lvol_store_bdev iterator");
                
                struct lvol_store_bdev* lvs_bdev = vbdev_lvol_store_first();
                int count = 0;
                while (lvs_bdev != nullptr) {
                    // Access spdk_lvol_store through lvol_store_bdev->lvs pointer
                    struct spdk_lvol_store* lvs = lvs_bdev->lvs;
                    
                    if (lvs != nullptr) {
                        LvolStoreInfo info;
                        
                        // Extract LVS information (using complete struct from spdk_internal/lvolstore.h)
                        // From https://raw.githubusercontent.com/spdk/spdk/refs/heads/v25.05.x/include/spdk_internal/lvolstore.h
                        char uuid_str[SPDK_UUID_STRING_LEN];
                        spdk_uuid_fmt_lower(uuid_str, sizeof(uuid_str), &lvs->uuid);
                        info.uuid = std::string(uuid_str);
                        
                        // Get name from struct (char name[SPDK_LVS_NAME_MAX])
                        info.name = std::string(lvs->name);
                        
                        // Try to get base bdev name through lvol_store_bdev->bdev
                        if (lvs_bdev->bdev) {
                            info.base_bdev = std::string(spdk_bdev_get_name(lvs_bdev->bdev));
                        } else {
                            info.base_bdev = "unknown";
                        }
                        
                        // Get cluster information from blobstore (struct spdk_blob_store *blobstore)
                        if (lvs->blobstore) {
                            info.total_clusters = spdk_bs_get_cluster_count(lvs->blobstore);
                            info.free_clusters = spdk_bs_free_cluster_count(lvs->blobstore);
                            info.cluster_size = spdk_bs_get_cluster_size(lvs->blobstore);
                            info.block_size = spdk_bs_get_io_unit_size(lvs->blobstore);
                        } else {
                            info.total_clusters = 0;
                            info.free_clusters = 0;
                            info.cluster_size = 0;
                            info.block_size = 0;
                        }
                        
                        lvs_list.push_back(info);
                        count++;
                        
                        spdk_flint::logger()->debug("[SPDK] LVS #{}: name='{}', uuid='{}', base_bdev='{}', "
                                                   "clusters={}/{}, cluster_size={}, block_size={}", 
                                                   count, info.name, info.uuid, info.base_bdev,
                                                   info.free_clusters, info.total_clusters, 
                                                   info.cluster_size, info.block_size);
                    } else {
                        spdk_flint::logger()->warn("[SPDK] Found lvol_store_bdev with null lvs pointer, skipping");
                    }
                    
                    lvs_bdev = vbdev_lvol_store_next(lvs_bdev);
                }
                
                spdk_flint::logger()->info("[SPDK] Enumerated {} LVol stores successfully", lvs_list.size());
                
            } else {
                // Get single LVS by UUID or name
                spdk_flint::logger()->debug("[SPDK] Getting specific LVol store");
                
                struct spdk_lvol_store* lvs = get_lvol_store_by_uuid_or_name(ctx->uuid, ctx->lvs_name);
                if (lvs != nullptr) {
                    LvolStoreInfo info;
                    
                    // Extract LVS information (using complete struct from spdk_internal/lvolstore.h)
                    // From https://raw.githubusercontent.com/spdk/spdk/refs/heads/v25.05.x/include/spdk_internal/lvolstore.h
                    char uuid_str[SPDK_UUID_STRING_LEN];
                    spdk_uuid_fmt_lower(uuid_str, sizeof(uuid_str), &lvs->uuid);
                    info.uuid = std::string(uuid_str);
                    
                    // Get name from struct (char name[SPDK_LVS_NAME_MAX])
                    info.name = std::string(lvs->name);
                    
                    // Get base bdev name through lvol_store_bdev helper function
                    struct lvol_store_bdev* lvs_bdev = vbdev_get_lvs_bdev_by_lvs(lvs);
                    if (lvs_bdev && lvs_bdev->bdev) {
                        info.base_bdev = std::string(spdk_bdev_get_name(lvs_bdev->bdev));
                    } else {
                        info.base_bdev = "unknown";
                    }
                    
                    // Get cluster information from blobstore (struct spdk_blob_store *blobstore)
                    if (lvs->blobstore) {
                        info.total_clusters = spdk_bs_get_cluster_count(lvs->blobstore);
                        info.free_clusters = spdk_bs_free_cluster_count(lvs->blobstore);
                        info.cluster_size = spdk_bs_get_cluster_size(lvs->blobstore);
                        info.block_size = spdk_bs_get_io_unit_size(lvs->blobstore);
                    } else {
                        info.total_clusters = 0;
                        info.free_clusters = 0;
                        info.cluster_size = 0;
                        info.block_size = 0;
                    }
                    
                    lvs_list.push_back(info);
                    
                    spdk_flint::logger()->info("[SPDK] Found LVS: name='{}', uuid='{}', usage={:.1f}%", 
                                              info.name, info.uuid, 
                                              info.total_clusters > 0 ? 
                                                  100.0 * (info.total_clusters - info.free_clusters) / info.total_clusters : 0.0);
                    spdk_flint::logger()->debug("[SPDK] LVS details: base_bdev='{}', total_clusters={}, "
                                               "free_clusters={}, cluster_size={}, block_size={}", 
                                               info.base_bdev, info.total_clusters, info.free_clusters,
                                               info.cluster_size, info.block_size);
                } else {
                    error = -ENODEV;
                    spdk_flint::logger()->error("[SPDK] LVol store not found - UUID: '{}', Name: '{}'", 
                                               ctx->uuid, ctx->lvs_name);
                }
            }
        } catch (const std::exception& e) {
            spdk_flint::logger()->error("[SPDK] Exception in getLvolStoresAsync: {}", e.what());
            error = -EINVAL;
        }
        
        auto duration = std::chrono::duration_cast<std::chrono::microseconds>(
            std::chrono::steady_clock::now() - ctx->start_time);
        spdk_flint::logger()->debug("[SPDK] LVS enumeration completed in {} μs", duration.count());
        
        // Call the callback with results
        if (ctx->callback) {
            ctx->callback(lvs_list, error);
        }
        
        delete ctx;
    }, ctx);
}

void SpdkWrapper::createLvolStoreAsync(
    const std::string& bdev_name,
    const std::string& lvs_name,
    const std::string& clear_method,
    uint32_t cluster_sz,
    LvolStoreCreateCallback callback) {
    
    auto start_time = std::chrono::steady_clock::now();
    spdk_flint::logger()->info("[SPDK] Creating LVol store - Bdev: '{}', Name: '{}', Clear: '{}', Cluster: {}",
                              bdev_name, lvs_name, clear_method, cluster_sz);
    spdk_flint::logger()->debug("[SPDK] Callback provided: {}", callback ? "yes" : "no");
    
    // Validate input parameters
    if (bdev_name.empty()) {
        spdk_flint::logger()->error("[SPDK] LVS creation failed: empty bdev name");
        if (callback) callback("", -EINVAL);
        return;
    }
    if (lvs_name.empty()) {
        spdk_flint::logger()->error("[SPDK] LVS creation failed: empty LVS name");
        if (callback) callback("", -EINVAL);
        return;
    }
    
    // Context for async operation
    struct CreateLvsCtx {
        LvolStoreCreateCallback callback;
        std::string bdev_name;
        std::string lvs_name;
        std::string clear_method;
        uint32_t cluster_sz;
        std::chrono::steady_clock::time_point start_time;
    };
    
    auto* ctx = new CreateLvsCtx{callback, bdev_name, lvs_name, clear_method, cluster_sz, start_time};
    spdk_flint::logger()->debug("[SPDK] Created async context for LVS creation");
    
    spdk_thread_send_msg(spdk_get_thread(), [](void* arg) {
        auto* ctx = static_cast<CreateLvsCtx*>(arg);
        
        spdk_flint::logger()->debug("[SPDK] Executing LVS creation on reactor thread");
        
        // Validate that the bdev exists
        struct spdk_bdev* bdev = spdk_bdev_get_by_name(ctx->bdev_name.c_str());
        if (!bdev) {
            spdk_flint::logger()->error("[SPDK] LVS creation failed: bdev '{}' not found", ctx->bdev_name);
            if (ctx->callback) {
                ctx->callback("", -ENODEV);
            }
            delete ctx;
            return;
        }
        
        spdk_flint::logger()->debug("[SPDK] Target bdev '{}' found: {} blocks × {} bytes = {} MB",
                                   ctx->bdev_name, spdk_bdev_get_num_blocks(bdev), 
                                   spdk_bdev_get_block_size(bdev),
                                   (spdk_bdev_get_num_blocks(bdev) * spdk_bdev_get_block_size(bdev)) / (1024 * 1024));
        
        // Parse clear method
        enum lvs_clear_method clear_method_enum;
        if (ctx->clear_method == "none") {
            clear_method_enum = LVS_CLEAR_WITH_NONE;
            spdk_flint::logger()->debug("[SPDK] Using clear method: none (fastest)");
        } else if (ctx->clear_method == "unmap") {
            clear_method_enum = LVS_CLEAR_WITH_UNMAP;
            spdk_flint::logger()->debug("[SPDK] Using clear method: unmap (recommended)");
        } else if (ctx->clear_method == "write_zeroes") {
            clear_method_enum = LVS_CLEAR_WITH_WRITE_ZEROES;
            spdk_flint::logger()->debug("[SPDK] Using clear method: write_zeroes (secure)");
        } else {
            clear_method_enum = LVS_CLEAR_WITH_UNMAP; // Default
            spdk_flint::logger()->debug("[SPDK] Using default clear method: unmap (unknown method '{}')", ctx->clear_method);
        }
        
        // Use default cluster size if not specified
        uint32_t effective_cluster_sz = ctx->cluster_sz;
        if (effective_cluster_sz == 0) {
            effective_cluster_sz = 4194304; // 4MB default
            spdk_flint::logger()->debug("[SPDK] Using default cluster size: {} bytes (4 MB)", effective_cluster_sz);
        } else {
            spdk_flint::logger()->debug("[SPDK] Using specified cluster size: {} bytes ({} MB)", 
                                       effective_cluster_sz, effective_cluster_sz / (1024 * 1024));
        }
        
        // Call vbdev_lvs_create_ext with callback
        spdk_flint::logger()->debug("[SPDK] Calling vbdev_lvs_create_ext with async callback");
        int rc = vbdev_lvs_create_ext(
            ctx->bdev_name.c_str(),
            ctx->lvs_name.c_str(),
            effective_cluster_sz,
            clear_method_enum,
            0, // num_md_pages_per_cluster_ratio (default)
            0, // md_page_size (default)
            [](void* cb_arg, struct spdk_lvol_store* lvol_store, int lvserrno) {
                auto* ctx = static_cast<CreateLvsCtx*>(cb_arg);
                
                auto duration = std::chrono::duration_cast<std::chrono::milliseconds>(
                    std::chrono::steady_clock::now() - ctx->start_time);
                
                if (lvserrno == 0 && lvol_store != nullptr) {
                    // Get UUID of created LVS
                    char uuid_str[SPDK_UUID_STRING_LEN];
                    spdk_uuid_fmt_lower(uuid_str, sizeof(uuid_str), &lvol_store->uuid);
                    std::string uuid = std::string(uuid_str);
                    
                    // Log detailed creation results
                    uint64_t total_clusters = spdk_bs_get_cluster_count(lvol_store->blobstore);
                    uint64_t cluster_size = spdk_bs_get_cluster_size(lvol_store->blobstore);
                    uint64_t total_size = total_clusters * cluster_size;
                    
                    spdk_flint::logger()->info("[SPDK] Successfully created LVol store '{}' in {} ms", 
                                             ctx->lvs_name, duration.count());
                    spdk_flint::logger()->info("[SPDK] LVS details: UUID={}, total_size={} MB, clusters={}, cluster_size={} KB",
                                             uuid, total_size / (1024 * 1024), total_clusters, cluster_size / 1024);
                    
                    if (ctx->callback) {
                        ctx->callback(uuid, 0);
                    }
                } else {
                    spdk_flint::logger()->error("[SPDK] Failed to create LVol store '{}' after {} ms: {} ({})", 
                                               ctx->lvs_name, duration.count(), lvserrno, strerror(-lvserrno));
                    
                    if (ctx->callback) {
                        ctx->callback("", lvserrno);
                    }
                }
                
                delete ctx;
            },
            ctx
        );
        
        if (rc != 0) {
            spdk_flint::logger()->error("[SPDK] Failed to start LVol store creation: {} ({})", rc, strerror(-rc));
            if (ctx->callback) {
                ctx->callback("", rc);
            }
            delete ctx;
        } else {
            spdk_flint::logger()->debug("[SPDK] LVS creation initiated successfully, waiting for completion");
        }
    }, ctx);
}

void SpdkWrapper::deleteLvolStoreAsync(
    const std::string& uuid,
    const std::string& lvs_name,
    LvolStoreDeleteCallback callback) {
    
    auto start_time = std::chrono::steady_clock::now();
    spdk_flint::logger()->info("[SPDK] Deleting LVol store - UUID: '{}', Name: '{}'", uuid, lvs_name);
    spdk_flint::logger()->debug("[SPDK] Callback provided: {}", callback ? "yes" : "no");
    
    if (uuid.empty() && lvs_name.empty()) {
        spdk_flint::logger()->error("[SPDK] LVS deletion failed: neither UUID nor name specified");
        if (callback) {
            callback(-EINVAL);
        }
        return;
    }
    
    // Context for async operation
    struct DeleteLvsCtx {
        LvolStoreDeleteCallback callback;
        std::string uuid;
        std::string lvs_name;
        std::chrono::steady_clock::time_point start_time;
    };
    
    auto* ctx = new DeleteLvsCtx{callback, uuid, lvs_name, start_time};
    spdk_flint::logger()->debug("[SPDK] Created async context for LVS deletion");
    
    spdk_thread_send_msg(spdk_get_thread(), [](void* arg) {
        auto* ctx = static_cast<DeleteLvsCtx*>(arg);
        
        spdk_flint::logger()->debug("[SPDK] Executing LVS deletion on reactor thread");
        
        // Get LVS by UUID or name
        struct spdk_lvol_store* lvs = get_lvol_store_by_uuid_or_name(ctx->uuid, ctx->lvs_name);
        if (lvs == nullptr) {
            spdk_flint::logger()->error("[SPDK] LVS deletion failed: LVS not found - UUID: '{}', Name: '{}'", 
                                       ctx->uuid, ctx->lvs_name);
            if (ctx->callback) {
                ctx->callback(-ENODEV);
            }
            delete ctx;
            return;
        }
        
        // Log details before deletion (using complete struct from spdk_internal/lvolstore.h)
        char uuid_str[SPDK_UUID_STRING_LEN];
        spdk_uuid_fmt_lower(uuid_str, sizeof(uuid_str), &lvs->uuid);
        
        spdk_flint::logger()->debug("[SPDK] Found LVS for deletion: uuid='{}', name='{}'", 
                                   uuid_str, lvs->name);
        
        // Get usage information from blobstore (struct spdk_blob_store *blobstore)
        if (lvs->blobstore) {
            uint64_t total_clusters = spdk_bs_get_cluster_count(lvs->blobstore);
            uint64_t free_clusters = spdk_bs_free_cluster_count(lvs->blobstore);
            uint64_t used_clusters = total_clusters - free_clusters;
            
            spdk_flint::logger()->debug("[SPDK] LVS usage: {}/{} clusters used", used_clusters, total_clusters);
            
            if (used_clusters > 0) {
                spdk_flint::logger()->warn("[SPDK] Deleting LVS with {} used clusters - data will be lost", used_clusters);
            }
        } else {
            spdk_flint::logger()->warn("[SPDK] Cannot access blobstore for usage information");
        }
        
        // Call vbdev_lvs_destruct with callback
        spdk_flint::logger()->debug("[SPDK] Calling vbdev_lvs_destruct with async callback");
        vbdev_lvs_destruct(lvs, [](void* cb_arg, int lvserrno) {
            auto* ctx = static_cast<DeleteLvsCtx*>(cb_arg);
            
            auto duration = std::chrono::duration_cast<std::chrono::milliseconds>(
                std::chrono::steady_clock::now() - ctx->start_time);
            
            if (lvserrno == 0) {
                spdk_flint::logger()->info("[SPDK] Successfully deleted LVol store in {} ms", duration.count());
            } else {
                spdk_flint::logger()->error("[SPDK] Failed to delete LVol store after {} ms: {} ({})", 
                                           duration.count(), lvserrno, strerror(-lvserrno));
            }
            
            if (ctx->callback) {
                ctx->callback(lvserrno);
            }
            
            delete ctx;
        }, ctx);
    }, ctx);
}

// Block Device Creation Implementation

void SpdkWrapper::createAioBdevAsync(
    const std::string& name,
    const std::string& filename,
    uint32_t block_size,
    bool readonly,
    bool fallocate,
    const std::string& uuid,
    BdevCreateCallback callback) {
    
    auto start_time = std::chrono::steady_clock::now();
    spdk_flint::logger()->info("[SPDK] Creating AIO bdev - Name: '{}', File: '{}', BlockSize: {}, RO: {}, Fallocate: {}",
                              name, filename, block_size, readonly, fallocate);
    spdk_flint::logger()->debug("[SPDK] UUID: '{}', Callback: {}", uuid.empty() ? "auto-generated" : uuid, callback ? "provided" : "none");
    
    // Validate parameters
    if (name.empty()) {
        spdk_flint::logger()->error("[SPDK] AIO bdev creation failed: empty name");
        if (callback) callback("", -EINVAL);
        return;
    }
    if (filename.empty()) {
        spdk_flint::logger()->error("[SPDK] AIO bdev creation failed: empty filename");
        if (callback) callback("", -EINVAL);
        return;
    }
    if (block_size == 0 || (block_size & (block_size - 1)) != 0) {
        spdk_flint::logger()->error("[SPDK] AIO bdev creation failed: invalid block size {} (must be power of 2)", block_size);
        if (callback) callback("", -EINVAL);
        return;
    }
    
    // Context for async operation
    struct CreateAioCtx {
        BdevCreateCallback callback;
        std::string name;
        std::string filename;
        uint32_t block_size;
        bool readonly;
        bool fallocate;
        std::string uuid;
        std::chrono::steady_clock::time_point start_time;
    };
    
    auto* ctx = new CreateAioCtx{callback, name, filename, block_size, readonly, fallocate, uuid, start_time};
    spdk_flint::logger()->debug("[SPDK] Created async context for AIO bdev creation");
    
    spdk_thread_send_msg(spdk_get_thread(), [](void* arg) {
        auto* ctx = static_cast<CreateAioCtx*>(arg);
        
        spdk_flint::logger()->debug("[SPDK] Executing AIO bdev creation on reactor thread");
        
        // Check if file exists and get size
        struct stat file_stat;
        if (stat(ctx->filename.c_str(), &file_stat) == 0) {
            spdk_flint::logger()->debug("[SPDK] Target file '{}' exists: size={} MB, mode=0{:o}",
                                       ctx->filename, file_stat.st_size / (1024 * 1024), file_stat.st_mode & 0777);
        } else {
            spdk_flint::logger()->debug("[SPDK] Target file '{}' does not exist (will be created)", ctx->filename);
        }
        
        // Call create_aio_bdev (external function from module/bdev/aio/bdev_aio_rpc.c)
        extern int create_aio_bdev(const char* name, const char* filename, uint32_t block_size,
                                  bool readonly, bool fallocate, const char* uuid);
        
        const char* uuid_ptr = ctx->uuid.empty() ? nullptr : ctx->uuid.c_str();
        spdk_flint::logger()->debug("[SPDK] Calling create_aio_bdev with parameters: name='{}', file='{}', "
                                   "block_size={}, readonly={}, fallocate={}, uuid='{}'",
                                   ctx->name, ctx->filename, ctx->block_size, ctx->readonly, 
                                   ctx->fallocate, uuid_ptr ? uuid_ptr : "auto");
        
        int rc = create_aio_bdev(ctx->name.c_str(), ctx->filename.c_str(), ctx->block_size,
                                ctx->readonly, ctx->fallocate, uuid_ptr);
        
        auto duration = std::chrono::duration_cast<std::chrono::milliseconds>(
            std::chrono::steady_clock::now() - ctx->start_time);
        
        if (rc == 0) {
            // Verify the bdev was created
            struct spdk_bdev* bdev = spdk_bdev_get_by_name(ctx->name.c_str());
            if (bdev) {
                spdk_flint::logger()->info("[SPDK] Successfully created AIO bdev '{}' in {} ms", ctx->name, duration.count());
                spdk_flint::logger()->info("[SPDK] AIO bdev details: {} blocks × {} bytes = {} MB",
                                          spdk_bdev_get_num_blocks(bdev), spdk_bdev_get_block_size(bdev),
                                          (spdk_bdev_get_num_blocks(bdev) * spdk_bdev_get_block_size(bdev)) / (1024 * 1024));
            } else {
                spdk_flint::logger()->warn("[SPDK] AIO bdev '{}' creation reported success but bdev not found", ctx->name);
            }
            
            if (ctx->callback) {
                ctx->callback(ctx->name, 0);
            }
        } else {
            spdk_flint::logger()->error("[SPDK] Failed to create AIO bdev '{}' after {} ms: {} ({})", 
                                       ctx->name, duration.count(), rc, strerror(-rc));
            if (ctx->callback) {
                ctx->callback("", rc);
            }
        }
        
        delete ctx;
    }, ctx);
}

void SpdkWrapper::createUringBdevAsync(
    const std::string& name,
    const std::string& filename,
    uint32_t block_size,
    const std::string& uuid,
    BdevCreateCallback callback) {
    
    auto start_time = std::chrono::steady_clock::now();
    spdk_flint::logger()->info("[SPDK] Creating uring bdev - Name: '{}', File: '{}', BlockSize: {}",
                              name, filename, block_size);
    spdk_flint::logger()->debug("[SPDK] UUID: '{}', Callback: {}", uuid.empty() ? "auto-generated" : uuid, callback ? "provided" : "none");
    
    // Validate parameters
    if (name.empty()) {
        spdk_flint::logger()->error("[SPDK] uring bdev creation failed: empty name");
        if (callback) callback("", -EINVAL);
        return;
    }
    if (filename.empty()) {
        spdk_flint::logger()->error("[SPDK] uring bdev creation failed: empty filename");
        if (callback) callback("", -EINVAL);
        return;
    }
    if (block_size == 0 || (block_size & (block_size - 1)) != 0) {
        spdk_flint::logger()->error("[SPDK] uring bdev creation failed: invalid block size {} (must be power of 2)", block_size);
        if (callback) callback("", -EINVAL);
        return;
    }
    
    // Context for async operation
    struct CreateUringCtx {
        BdevCreateCallback callback;
        std::string name;
        std::string filename;
        uint32_t block_size;
        std::string uuid;
        std::chrono::steady_clock::time_point start_time;
    };
    
    auto* ctx = new CreateUringCtx{callback, name, filename, block_size, uuid, start_time};
    spdk_flint::logger()->debug("[SPDK] Created async context for uring bdev creation");
    
    spdk_thread_send_msg(spdk_get_thread(), [](void* arg) {
        auto* ctx = static_cast<CreateUringCtx*>(arg);
        
        spdk_flint::logger()->debug("[SPDK] Executing uring bdev creation on reactor thread");
        
        // Check if file exists and get size
        struct stat file_stat;
        if (stat(ctx->filename.c_str(), &file_stat) == 0) {
            spdk_flint::logger()->debug("[SPDK] Target file '{}' exists: size={} MB, mode=0{:o}",
                                       ctx->filename, file_stat.st_size / (1024 * 1024), file_stat.st_mode & 0777);
        } else {
            spdk_flint::logger()->debug("[SPDK] Target file '{}' does not exist (will be created)", ctx->filename);
        }
        
        // Call create_uring_bdev (external function from module/bdev/uring/bdev_uring_rpc.c)
        extern int create_uring_bdev(const char* name, const char* filename, uint32_t block_size, const char* uuid);
        
        const char* uuid_ptr = ctx->uuid.empty() ? nullptr : ctx->uuid.c_str();
        spdk_flint::logger()->debug("[SPDK] Calling create_uring_bdev with parameters: name='{}', file='{}', "
                                   "block_size={}, uuid='{}'",
                                   ctx->name, ctx->filename, ctx->block_size, uuid_ptr ? uuid_ptr : "auto");
        
        int rc = create_uring_bdev(ctx->name.c_str(), ctx->filename.c_str(), ctx->block_size, uuid_ptr);
        
        auto duration = std::chrono::duration_cast<std::chrono::milliseconds>(
            std::chrono::steady_clock::now() - ctx->start_time);
        
        if (rc == 0) {
            // Verify the bdev was created
            struct spdk_bdev* bdev = spdk_bdev_get_by_name(ctx->name.c_str());
            if (bdev) {
                spdk_flint::logger()->info("[SPDK] Successfully created uring bdev '{}' in {} ms", ctx->name, duration.count());
                spdk_flint::logger()->info("[SPDK] uring bdev details: {} blocks × {} bytes = {} MB",
                                          spdk_bdev_get_num_blocks(bdev), spdk_bdev_get_block_size(bdev),
                                          (spdk_bdev_get_num_blocks(bdev) * spdk_bdev_get_block_size(bdev)) / (1024 * 1024));
            } else {
                spdk_flint::logger()->warn("[SPDK] uring bdev '{}' creation reported success but bdev not found", ctx->name);
            }
            
            if (ctx->callback) {
                ctx->callback(ctx->name, 0);
            }
        } else {
            spdk_flint::logger()->error("[SPDK] Failed to create uring bdev '{}' after {} ms: {} ({})", 
                                       ctx->name, duration.count(), rc, strerror(-rc));
            if (ctx->callback) {
                ctx->callback("", rc);
            }
        }
        
        delete ctx;
    }, ctx);
}

// NVMe Operations Implementation

void SpdkWrapper::getNvmeControllersAsync(
    const std::string& name,
    std::function<void(const std::vector<NvmeControllerInfo>&, int)> callback) {
    
    auto start_time = std::chrono::steady_clock::now();
    spdk_flint::logger()->info("[SPDK] Getting NVMe controllers - Name: '{}'", name);
    spdk_flint::logger()->debug("[SPDK] Callback provided: {}", callback ? "yes" : "no");
    
    // Context for async operation
    struct GetNvmeCtrlCtx {
        std::function<void(const std::vector<NvmeControllerInfo>&, int)> callback;
        std::string name;
        std::chrono::steady_clock::time_point start_time;
    };
    
    auto* ctx = new GetNvmeCtrlCtx{callback, name, start_time};
    spdk_flint::logger()->debug("[SPDK] Created async context for NVMe controller enumeration");
    
    spdk_thread_send_msg(spdk_get_thread(), [](void* arg) {
        auto* ctx = static_cast<GetNvmeCtrlCtx*>(arg);
        std::vector<NvmeControllerInfo> controllers;
        int error = 0;
        
        spdk_flint::logger()->debug("[SPDK] Executing NVMe controller enumeration on reactor thread");
        
        try {
            // Use NVMe controller functions (declared at top of file)
            if (!ctx->name.empty()) {
                // Get specific controller by name
                spdk_flint::logger()->debug("[SPDK] Searching for specific NVMe controller: '{}'", ctx->name);
                struct nvme_bdev_ctrlr* ctrlr = nvme_bdev_ctrlr_get_by_name(ctx->name.c_str());
                if (ctrlr != nullptr) {
                    NvmeControllerInfo info;
                    info.name = ctx->name;
                    // TODO: Fill in other controller details from ctrlr structure
                    info.trtype = "unknown";
                    info.traddr = "unknown";
                    info.state = "connected";
                    controllers.push_back(info);
                    spdk_flint::logger()->info("[SPDK] Found NVMe controller: '{}'", ctx->name);
                    spdk_flint::logger()->debug("[SPDK] Controller details: type={}, addr={}, state={}", 
                                               info.trtype, info.traddr, info.state);
                } else {
                    error = -ENODEV;
                    spdk_flint::logger()->error("[SPDK] NVMe controller '{}' not found", ctx->name);
                }
            } else {
                // Get all controllers
                spdk_flint::logger()->debug("[SPDK] Enumerating all NVMe controllers");
                struct nvme_bdev_ctrlr* ctrlr = nvme_bdev_ctrlr_first();
                int count = 0;
                while (ctrlr != nullptr) {
                    NvmeControllerInfo info;
                    // TODO: Extract controller information from ctrlr structure
                    info.name = fmt::format("nvme_controller_{}", count);
                    info.trtype = "unknown";
                    info.traddr = "unknown";
                    info.state = "connected";
                    controllers.push_back(info);
                    count++;
                    
                    spdk_flint::logger()->debug("[SPDK] Controller #{}: name={}, type={}, addr={}, state={}",
                                               count, info.name, info.trtype, info.traddr, info.state);
                    
                    ctrlr = nvme_bdev_ctrlr_next(ctrlr);
                }
                spdk_flint::logger()->info("[SPDK] Enumerated {} NVMe controllers", controllers.size());
            }
        } catch (const std::exception& e) {
            spdk_flint::logger()->error("[SPDK] Exception in getNvmeControllersAsync: {}", e.what());
            error = -EINVAL;
        }
        
        auto duration = std::chrono::duration_cast<std::chrono::microseconds>(
            std::chrono::steady_clock::now() - ctx->start_time);
        spdk_flint::logger()->debug("[SPDK] NVMe controller enumeration completed in {} μs", duration.count());
        
        if (ctx->callback) {
            ctx->callback(controllers, error);
        }
        
        delete ctx;
    }, ctx);
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
    
    spdk_flint::logger()->info("[SPDK] Attaching NVMe controller - Name: '{}', Type: '{}', Addr: '{}'",
                              name, trtype, traddr);
    spdk_flint::logger()->debug("[SPDK] Parameters: adrfam='{}', trsvcid='{}', priority={}, multipath={}",
                               adrfam, trsvcid, priority, multipath);
    spdk_flint::logger()->debug("[SPDK] Advanced: subnqn='{}', hostnqn='{}', queues={}, timeouts={}/{}/{}",
                               subnqn, hostnqn, num_io_queues, ctrlr_loss_timeout_sec, 
                               reconnect_delay_sec, fast_io_fail_timeout_sec);
    
    // Validate required parameters
    if (name.empty()) {
        spdk_flint::logger()->error("[SPDK] NVMe attach failed: empty controller name");
        if (callback) callback({}, -EINVAL);
        return;
    }
    if (traddr.empty()) {
        spdk_flint::logger()->error("[SPDK] NVMe attach failed: empty transport address");
        if (callback) callback({}, -EINVAL);
        return;
    }
    
    // Context for async operation
    struct AttachNvmeCtx {
        NvmeAttachCallback callback;
        std::string name;
        std::string trtype;
        std::string traddr;
        std::string adrfam;
        std::string trsvcid;
        std::string subnqn;
        std::string hostnqn;
        std::string hostaddr;
        std::string hostsvcid;
        bool multipath;
        uint32_t num_io_queues;
        uint32_t ctrlr_loss_timeout_sec;
        uint32_t reconnect_delay_sec;
        uint32_t fast_io_fail_timeout_sec;
        std::chrono::steady_clock::time_point start_time;
    };
    
    auto* ctx = new AttachNvmeCtx{callback, name, trtype, traddr, adrfam, trsvcid, subnqn, hostnqn, 
                                 hostaddr, hostsvcid, multipath, num_io_queues, ctrlr_loss_timeout_sec,
                                 reconnect_delay_sec, fast_io_fail_timeout_sec, std::chrono::steady_clock::now()};
    spdk_flint::logger()->debug("[SPDK] Created async context for NVMe controller attach");
    
    spdk_thread_send_msg(spdk_get_thread(), [](void* arg) {
        auto* ctx = static_cast<AttachNvmeCtx*>(arg);
        
        spdk_flint::logger()->debug("[SPDK] Executing NVMe controller attach on reactor thread");
        
        // Convert parameters to C strings (some can be NULL)
        const char* priority_str = nullptr;  // Use default priority
        const char* subnqn_str = ctx->subnqn.empty() ? nullptr : ctx->subnqn.c_str();
        const char* hostnqn_str = ctx->hostnqn.empty() ? nullptr : ctx->hostnqn.c_str();
        const char* hostaddr_str = ctx->hostaddr.empty() ? nullptr : ctx->hostaddr.c_str();
        const char* hostsvcid_str = ctx->hostsvcid.empty() ? nullptr : ctx->hostsvcid.c_str();
        const char* adrfam_str = ctx->adrfam.empty() ? nullptr : ctx->adrfam.c_str();
        const char* trsvcid_str = ctx->trsvcid.empty() ? nullptr : ctx->trsvcid.c_str();
        
        spdk_flint::logger()->debug("[SPDK] Calling bdev_nvme_attach_controller with parameters:");
        spdk_flint::logger()->debug("[SPDK]   name='{}', trtype='{}', traddr='{}'", ctx->name, ctx->trtype, ctx->traddr);
        spdk_flint::logger()->debug("[SPDK]   adrfam='{}', trsvcid='{}', multipath={}", 
                                   adrfam_str ? adrfam_str : "null", trsvcid_str ? trsvcid_str : "null", ctx->multipath);
        spdk_flint::logger()->debug("[SPDK]   queues={}, timeouts={}/{}/{}", ctx->num_io_queues, 
                                   ctx->ctrlr_loss_timeout_sec, ctx->reconnect_delay_sec, ctx->fast_io_fail_timeout_sec);
        
                 // Call the real SPDK function
         int rc = bdev_nvme_attach_controller(
             ctx->name.c_str(),
             ctx->trtype.c_str(),
             ctx->traddr.c_str(),
             adrfam_str,
             trsvcid_str,
             priority_str,
             subnqn_str,
             hostnqn_str,
             hostaddr_str,
             hostsvcid_str,
             ctx->multipath,
             ctx->num_io_queues,
             ctx->ctrlr_loss_timeout_sec,
             ctx->reconnect_delay_sec,
             ctx->fast_io_fail_timeout_sec,
            [](void* cb_arg, size_t bdev_count, int nvme_status) {
                auto* ctx = static_cast<AttachNvmeCtx*>(cb_arg);
                
                auto duration = std::chrono::duration_cast<std::chrono::milliseconds>(
                    std::chrono::steady_clock::now() - ctx->start_time);
                
                if (nvme_status == 0) {
                    // Success - build list of attached bdev names
                    std::vector<std::string> bdev_names;
                    
                    // For NVMe controllers, bdevs are typically named like "Nvme0n1", "Nvme0n2", etc.
                    // where "Nvme0" is the controller name and "n1", "n2" are namespace numbers
                    for (size_t i = 1; i <= bdev_count; i++) {
                        std::string bdev_name = ctx->name + "n" + std::to_string(i);
                        bdev_names.push_back(bdev_name);
                    }
                    
                    spdk_flint::logger()->info("[SPDK] Successfully attached NVMe controller '{}' in {} ms", 
                                             ctx->name, duration.count());
                    
                    // Create comma-separated list of bdev names
                    std::string bdev_list;
                    for (size_t i = 0; i < bdev_names.size(); i++) {
                        if (i > 0) bdev_list += ", ";
                        bdev_list += bdev_names[i];
                    }
                    
                    spdk_flint::logger()->info("[SPDK] Created {} block device(s): {}", 
                                             bdev_count, bdev_list);
                    
                    if (ctx->callback) {
                        ctx->callback(bdev_names, 0);
                    }
                } else {
                    spdk_flint::logger()->error("[SPDK] Failed to attach NVMe controller '{}' after {} ms: {} ({})", 
                                               ctx->name, duration.count(), nvme_status, strerror(-nvme_status));
                    
                    if (ctx->callback) {
                        ctx->callback({}, nvme_status);
                    }
                }
                
                delete ctx;
            },
            ctx
        );
        
        if (rc != 0) {
            // Immediate error - callback won't be called
            auto duration = std::chrono::duration_cast<std::chrono::milliseconds>(
                std::chrono::steady_clock::now() - ctx->start_time);
            
            spdk_flint::logger()->error("[SPDK] Failed to initiate NVMe controller attach '{}' after {} ms: {} ({})", 
                                       ctx->name, duration.count(), rc, strerror(-rc));
            
            if (ctx->callback) {
                ctx->callback({}, rc);
            }
            
            delete ctx;
        }
    }, ctx);
}

// Block Device Enumeration Implementation

void SpdkWrapper::getBdevsAsync(
    const std::string& name,
    uint32_t timeout,
    std::function<void(const std::vector<BdevInfo>&, int)> callback) {
    
    auto start_time = std::chrono::steady_clock::now();
    spdk_flint::logger()->info("[SPDK] Getting block devices - Name: '{}', Timeout: {}", name, timeout);
    spdk_flint::logger()->debug("[SPDK] Callback provided: {}", callback ? "yes" : "no");
    
    // Context for async operation
    struct GetBdevsCtx {
        std::function<void(const std::vector<BdevInfo>&, int)> callback;
        std::string name;
        uint32_t timeout;
        std::chrono::steady_clock::time_point start_time;
    };
    
    auto* ctx = new GetBdevsCtx{callback, name, timeout, start_time};
    spdk_flint::logger()->debug("[SPDK] Created async context for bdev enumeration");
    
    spdk_thread_send_msg(spdk_get_thread(), [](void* arg) {
        auto* ctx = static_cast<GetBdevsCtx*>(arg);
        std::vector<BdevInfo> bdevs;
        int error = 0;
        
        spdk_flint::logger()->debug("[SPDK] Executing bdev enumeration on reactor thread");
        
        try {
            if (!ctx->name.empty()) {
                // Get specific bdev by name
                spdk_flint::logger()->debug("[SPDK] Searching for specific bdev: '{}'", ctx->name);
                struct spdk_bdev* bdev = spdk_bdev_get_by_name(ctx->name.c_str());
                if (bdev != nullptr) {
                    BdevInfo info;
                    info.name = std::string(spdk_bdev_get_name(bdev));
                    info.product_name = std::string(spdk_bdev_get_product_name(bdev));
                    info.block_size = spdk_bdev_get_block_size(bdev);
                    info.num_blocks = spdk_bdev_get_num_blocks(bdev);
                    info.claimed = spdk_bdev_is_claimed(bdev);
                    
                    // Get UUID if available
                    const struct spdk_uuid* uuid = spdk_bdev_get_uuid(bdev);
                    if (uuid != nullptr) {
                        char uuid_str[SPDK_UUID_STRING_LEN];
                        spdk_uuid_fmt_lower(uuid_str, sizeof(uuid_str), uuid);
                        info.uuid = std::string(uuid_str);
                    }
                    
                    // Calculate total size
                    uint64_t total_size = info.num_blocks * info.block_size;
                    
                    bdevs.push_back(info);
                    spdk_flint::logger()->info("[SPDK] Found bdev: '{}', size={} MB, claimed={}", 
                                              info.name, total_size / (1024 * 1024), info.claimed);
                    spdk_flint::logger()->debug("[SPDK] bdev details: product='{}', blocks={}, block_size={}, uuid='{}'",
                                               info.product_name, info.num_blocks, info.block_size, 
                                               info.uuid.empty() ? "none" : info.uuid);
                } else {
                    error = -ENODEV;
                    spdk_flint::logger()->error("[SPDK] Block device '{}' not found", ctx->name);
                }
            } else {
                // Get all bdevs using spdk_bdev_first() and spdk_bdev_next()
                spdk_flint::logger()->debug("[SPDK] Enumerating all block devices");
                struct spdk_bdev* bdev = spdk_bdev_first();
                int count = 0;
                uint64_t total_storage = 0;
                
                while (bdev != nullptr) {
                    BdevInfo info;
                    info.name = std::string(spdk_bdev_get_name(bdev));
                    info.product_name = std::string(spdk_bdev_get_product_name(bdev));
                    info.block_size = spdk_bdev_get_block_size(bdev);
                    info.num_blocks = spdk_bdev_get_num_blocks(bdev);
                    info.claimed = spdk_bdev_is_claimed(bdev);
                    
                    // Get UUID if available
                    const struct spdk_uuid* uuid = spdk_bdev_get_uuid(bdev);
                    if (uuid != nullptr) {
                        char uuid_str[SPDK_UUID_STRING_LEN];
                        spdk_uuid_fmt_lower(uuid_str, sizeof(uuid_str), uuid);
                        info.uuid = std::string(uuid_str);
                    }
                    
                    uint64_t bdev_size = info.num_blocks * info.block_size;
                    total_storage += bdev_size;
                    
                    bdevs.push_back(info);
                    count++;
                    
                    spdk_flint::logger()->debug("[SPDK] bdev #{}: name='{}', product='{}', size={} MB, "
                                               "claimed={}, uuid='{}'",
                                               count, info.name, info.product_name, bdev_size / (1024 * 1024),
                                               info.claimed, info.uuid.empty() ? "none" : info.uuid);
                    
                    bdev = spdk_bdev_next(bdev);
                }
                spdk_flint::logger()->info("[SPDK] Enumerated {} block devices, total storage: {} GB", 
                                          bdevs.size(), total_storage / (1024 * 1024 * 1024));
            }
        } catch (const std::exception& e) {
            spdk_flint::logger()->error("[SPDK] Exception in getBdevsAsync: {}", e.what());
            error = -EINVAL;
        }
        
        auto duration = std::chrono::duration_cast<std::chrono::microseconds>(
            std::chrono::steady_clock::now() - ctx->start_time);
        spdk_flint::logger()->debug("[SPDK] bdev enumeration completed in {} μs", duration.count());
        
        if (ctx->callback) {
            ctx->callback(bdevs, error);
        }
        
        delete ctx;
    }, ctx);
}

// Process Control Implementation

void SpdkWrapper::stopApplicationAsync(std::function<void(int)> callback) {
    spdk_flint::logger()->info("[SPDK] Stopping SPDK application gracefully");
    spdk_flint::logger()->debug("[SPDK] Callback provided: {}", callback ? "yes" : "no");
    
    // Context for async operation
    struct StopAppCtx {
        std::function<void(int)> callback;
    };
    
    auto* ctx = new StopAppCtx{callback};
    
    spdk_thread_send_msg(spdk_get_thread(), [](void* arg) {
        auto* ctx = static_cast<StopAppCtx*>(arg);
        
        spdk_flint::logger()->debug("[SPDK] Executing application stop on reactor thread");
        
        // Call spdk_app_stop()
        spdk_app_stop(0);
        
        spdk_flint::logger()->info("[SPDK] SPDK application stop initiated - shutdown in progress");
        
        if (ctx->callback) {
            ctx->callback(0);
        }
        
        delete ctx;
    }, ctx);
}

// Event Loop Management

void SpdkWrapper::processEvents() {
    if (g_spdk_initialized) {
        // Process any pending SPDK events
        spdk_thread_poll(spdk_get_thread(), 0, 0);
    }
}

void SpdkWrapper::runEventLoop() {
    event_loop_running_ = true;
    spdk_flint::logger()->info("[SPDK] Starting event loop");
    spdk_flint::logger()->debug("[SPDK] Event loop thread: {}", spdk_flint::current_thread_id());
    
    int iterations = 0;
    auto last_log_time = std::chrono::steady_clock::now();
    
    while (event_loop_running_ && !shutdown_requested_) {
        processEvents();
        iterations++;
        
        // Log periodic status
        auto now = std::chrono::steady_clock::now();
        if (now - last_log_time >= std::chrono::minutes(1)) {
            spdk_flint::logger()->debug("[SPDK] Event loop active: {} iterations processed", iterations);
            last_log_time = now;
            iterations = 0;
        }
        
        std::this_thread::sleep_for(std::chrono::milliseconds(1));
    }
    
    spdk_flint::logger()->info("[SPDK] Event loop stopped");
}

void SpdkWrapper::stopEventLoop() {
    spdk_flint::logger()->info("[SPDK] Requesting event loop stop");
    event_loop_running_ = false;
    shutdown_requested_ = true;
}

// Synchronous convenience methods

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
                spdk_flint::logger()->error("[SPDK] Synchronous getBdevs failed: {} ({})", error, strerror(-error));
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
                spdk_flint::logger()->error("[SPDK] Synchronous getLvolStores failed: {} ({})", error, strerror(-error));
            }
            promise.set_value();
        });
    
    future.wait();
    return result;
}

std::string SpdkWrapper::getVersionSync() const {
    return getVersion();
}

// Callback management helpers

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