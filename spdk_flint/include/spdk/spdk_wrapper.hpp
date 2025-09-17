#pragma once

#include <string>
#include <vector>
#include <memory>
#include <functional>
#include <optional>
#include <map>
#include <future>
#include <mutex>
#include <thread>
#include <atomic>
#include <chrono>

// Suppress pedantic warnings from SPDK headers (they use C99/GNU extensions)
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wpedantic"

// SPDK headers
extern "C" {
#include <spdk/stdinc.h>
#include <spdk/env.h>
#include <spdk/event.h>
#include <spdk/bdev.h>
#include <spdk/blob_bdev.h>
#include <spdk/blobfs.h>
#include <spdk/lvol.h>
#include <spdk/nvme.h>
#include <spdk/nvmf.h>
#include <spdk/rpc.h>
#include <spdk/thread.h>
#include <spdk/log.h>
#include <spdk/uuid.h>
}

// Restore previous diagnostic settings
#pragma GCC diagnostic pop

namespace spdk_flint {
namespace spdk {

// Forward declarations
struct VolumeInfo;
struct RaidInfo;
struct BdevInfo;
struct NvmfSubsystemInfo;
struct LvolStoreInfo;
struct NvmeControllerInfo;

// Error handling
enum class SpdkError {
    SUCCESS = 0,
    INVALID_PARAM,
    NO_MEMORY,
    NOT_FOUND,
    ALREADY_EXISTS,
    IO_ERROR,
    TIMEOUT,
    BUSY,
    UNKNOWN
};

class SpdkException : public std::exception {
public:
    explicit SpdkException(SpdkError error, const std::string& message)
        : error_(error), message_(message) {}
    
    const char* what() const noexcept override { return message_.c_str(); }
    SpdkError error() const { return error_; }

private:
    SpdkError error_;
    std::string message_;
};

// Callback types for async operations
using LvolStoreCreateCallback = std::function<void(const std::string& uuid, int error)>;
using LvolStoreDeleteCallback = std::function<void(int error)>;
using BdevCreateCallback = std::function<void(const std::string& bdev_name, int error)>;
using NvmeAttachCallback = std::function<void(const std::vector<std::string>& bdev_names, int error)>;
using UblkCallback = std::function<void(const std::string& device_path, int error)>;

// Data structures for return values
struct LvolStoreInfo {
    std::string uuid;
    std::string name;
    std::string base_bdev;
    uint64_t total_clusters;
    uint64_t free_clusters;
    uint64_t cluster_size;
    uint64_t block_size;
};

struct BdevInfo {
    std::string name;
    std::string uuid;
    std::string product_name;
    uint64_t block_size;
    uint64_t num_blocks;
    std::vector<std::string> aliases;
    bool claimed;
    std::string driver_specific;
};

struct NvmeControllerInfo {
    std::string name;
    std::string trtype;
    std::string traddr;
    std::string state;
    std::vector<std::string> bdevs;
};

struct UblkDiskInfo {
    int ublk_id;
    std::string bdev_name;
    std::string device_path;
    bool active;
};

// SPDK wrapper class - Node Agent functionality only
class SpdkWrapper {
public:
    explicit SpdkWrapper(const std::string& config_file = "");
    ~SpdkWrapper();

    // Initialization and cleanup
    bool initialize();
    void shutdown();
    bool isInitialized() const;

    // Version information
    std::string getVersion() const;

    // ===== NODE AGENT DIRECT SPDK C API METHODS =====
    
    // LVol Store Operations (replaces bdev_lvol_* RPCs)
    void getLvolStoresAsync(
        const std::string& uuid = "",
        const std::string& lvs_name = "",
        std::function<void(const std::vector<LvolStoreInfo>&, int)> callback = nullptr
    );
    
    void createLvolStoreAsync(
        const std::string& bdev_name,
        const std::string& lvs_name, 
        const std::string& clear_method = "unmap",
        uint32_t cluster_sz = 0,
        LvolStoreCreateCallback callback = nullptr
    );
    
    void deleteLvolStoreAsync(
        const std::string& uuid = "",
        const std::string& lvs_name = "",
        LvolStoreDeleteCallback callback = nullptr
    );

    // Block Device Creation (replaces bdev_aio_create, bdev_uring_create RPCs)
    void createAioBdevAsync(
        const std::string& name,
        const std::string& filename,
        uint32_t block_size = 512,
        bool readonly = false,
        bool fallocate = false,
        const std::string& uuid = "",
        BdevCreateCallback callback = nullptr
    );
    
    void createUringBdevAsync(
        const std::string& name,
        const std::string& filename,
        uint32_t block_size = 512,
        const std::string& uuid = "",
        BdevCreateCallback callback = nullptr
    );

    // NVMe Operations (replaces bdev_nvme_* RPCs)
    void getNvmeControllersAsync(
        const std::string& name = "",
        std::function<void(const std::vector<NvmeControllerInfo>&, int)> callback = nullptr
    );
    
    void attachNvmeControllerAsync(
        const std::string& name,
        const std::string& trtype,
        const std::string& traddr,
        const std::string& adrfam = "",
        const std::string& trsvcid = "",
        uint32_t priority = 0,
        const std::string& subnqn = "",
        const std::string& hostnqn = "",
        const std::string& hostaddr = "",
        const std::string& hostsvcid = "",
        bool multipath = false,
        uint32_t num_io_queues = 0,
        uint32_t ctrlr_loss_timeout_sec = 0,
        uint32_t reconnect_delay_sec = 0,
        uint32_t fast_io_fail_timeout_sec = 0,
        NvmeAttachCallback callback = nullptr
    );

    // Block Device Enumeration (replaces bdev_get_bdevs RPC)
    void getBdevsAsync(
        const std::string& name = "",
        uint32_t timeout = 0,
        std::function<void(const std::vector<BdevInfo>&, int)> callback = nullptr
    );

    // Process control
    void stopApplicationAsync(std::function<void(int)> callback = nullptr);

    // ===== UBLK RPC OPERATIONS (via RPC client) =====

    // Initialize ublk target (must be called once before using ublk devices)
    bool ensureUblkTarget();

    // Create a ublk device for a bdev
    std::string createUblkDevice(int ublk_id, const std::string& bdev_name);

    // Delete a ublk device
    bool deleteUblkDevice(int ublk_id);

    // Get list of ublk devices
    std::vector<UblkDiskInfo> getUblkDevices();

    // Async ublk operations
    void createUblkDeviceAsync(int ublk_id, const std::string& bdev_name, UblkCallback callback = nullptr);
    void deleteUblkDeviceAsync(int ublk_id, std::function<void(int)> callback = nullptr);

    // ===== VOLUME OPERATIONS (via RPC for CSI) =====

    // Create logical volume (for CSI CreateVolume)
    std::string createVolume(const std::string& lvs_name, const std::string& volume_name, uint64_t size_bytes);

    // Delete logical volume (for CSI DeleteVolume)
    bool deleteVolume(const std::string& volume_name);

    // Resize logical volume (for CSI ExpandVolume)
    bool resizeVolume(const std::string& volume_name, uint64_t new_size_bytes);

    // Get volume info
    std::optional<BdevInfo> getVolumeInfo(const std::string& volume_name);

    // Event handling and threading
    void processEvents();
    void runEventLoop();
    void stopEventLoop();

    // Synchronous convenience methods (for backwards compatibility)
    std::vector<BdevInfo> getBdevs(const std::string& name = "") const;
    std::vector<LvolStoreInfo> getLvolStores(const std::string& uuid = "", const std::string& lvs_name = "") const;
    std::string getVersionSync() const;

private:
    // Internal helper methods
    static SpdkError convertErrno(int err);
    static void throwOnError(int rc, const std::string& operation);

    // Internal data members
    std::string config_file_;
    struct spdk_app_opts* opts_;
    std::atomic<bool> event_loop_running_{false};
    std::atomic<bool> shutdown_requested_{false};

    // RPC client for ublk and volume operations
    class RpcClient;
    std::unique_ptr<RpcClient> rpc_client_;
    std::atomic<bool> ublk_target_initialized_{false};

    // Callback management for async operations
    struct CallbackContext {
        uint64_t id;
        std::function<void()> cleanup;
    };

    std::mutex callback_mutex_;
    std::map<uint64_t, CallbackContext> pending_callbacks_;
    std::atomic<uint64_t> next_callback_id_{1};

    uint64_t registerCallback(std::function<void()> cleanup);
    void unregisterCallback(uint64_t callback_id);
};

} // namespace spdk
} // namespace spdk_flint 