#pragma once

#include <string>
#include <vector>
#include <memory>
#include <functional>
#include <optional>
#include <map>

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
}

namespace spdk_flint {
namespace spdk {

// Forward declarations
struct VolumeInfo;
struct RaidInfo;
struct BdevInfo;
struct NvmfSubsystemInfo;

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

// SPDK wrapper class - replaces HTTP RPC calls with direct C function calls
class SpdkWrapper {
public:
    explicit SpdkWrapper(const std::string& config_file = "");
    ~SpdkWrapper();

    // Initialization and cleanup
    bool initialize();
    void shutdown();
    bool isInitialized() const { return initialized_; }

    // Version information (replaces spdk_get_version RPC)
    std::string getVersion() const;

    // Block device operations (replaces bdev_* RPCs)
    std::vector<BdevInfo> getBdevs(const std::string& name = "") const;
    bool createAioBdev(const std::string& name, const std::string& filename, 
                       uint32_t block_size = 512);
    bool createUringBdev(const std::string& name, const std::string& filename,
                         uint32_t block_size = 512);
    bool deleteBdev(const std::string& name);
    
    // NVMe operations (replaces bdev_nvme_* RPCs)
    bool attachNvmeController(const std::string& name, const std::string& traddr,
                             const std::string& trtype = "PCIe");
    bool detachNvmeController(const std::string& name);
    std::vector<std::string> getNvmeControllers() const;

    // Logical Volume Store operations (replaces bdev_lvol_*_lvstore RPCs)
    std::string createLvstore(const std::string& bdev_name, const std::string& lvs_name,
                             uint32_t cluster_size = 4194304,  // 4MB default
                             const std::string& clear_method = "write_zeroes");
    bool deleteLvstore(const std::string& lvs_name);
    std::vector<std::string> getLvstores() const;

    // Logical Volume operations (replaces bdev_lvol_* RPCs)
    std::string createLvol(const std::string& lvs_name, const std::string& lvol_name,
                          uint64_t size_mib, bool thin_provision = false,
                          const std::string& clear_method = "write_zeroes");
    bool deleteLvol(const std::string& lvol_name);
    bool resizeLvol(const std::string& lvol_name, uint64_t new_size_mib);
    
    // Snapshot operations (replaces bdev_lvol_snapshot RPC)
    std::string createSnapshot(const std::string& lvol_name, const std::string& snapshot_name);
    
    // RAID operations (replaces bdev_raid_* RPCs)
    std::string createRaid(const std::string& name, const std::string& raid_level,
                          const std::vector<std::string>& base_bdevs,
                          uint32_t strip_size_kb = 64);
    bool deleteRaid(const std::string& name);
    std::vector<RaidInfo> getRaidBdevs(const std::string& category = "all") const;
    bool addRaidBaseBdev(const std::string& raid_bdev, const std::string& base_bdev);
    bool removeRaidBaseBdev(const std::string& base_bdev_name);

    // NVMe-oF Target operations (replaces nvmf_* RPCs)
    bool createNvmfSubsystem(const std::string& nqn, bool allow_any_host = true);
    bool deleteNvmfSubsystem(const std::string& nqn);
    bool addNvmfNamespace(const std::string& nqn, const std::string& bdev_name,
                         uint32_t nsid = 0);
    bool addNvmfListener(const std::string& nqn, const std::string& trtype,
                        const std::string& traddr, const std::string& trsvcid);
    std::vector<NvmfSubsystemInfo> getNvmfSubsystems() const;

    // UBLK operations (replaces ublk_* RPCs)  
    bool createUblkTarget(const std::string& cpumask = "");
    bool destroyUblkTarget();
    int startUblkDisk(const std::string& bdev_name, uint32_t ublk_id,
                      uint32_t queue_depth = 128, uint32_t num_queues = 1);
    bool stopUblkDisk(uint32_t ublk_id);

    // Statistics and monitoring
    std::map<std::string, uint64_t> getBdevIoStats(const std::string& bdev_name = "") const;

    // Event handling and threading
    void processEvents();
    void runEventLoop();
    void stopEventLoop();

private:
    bool initialized_ = false;
    std::string config_file_;
    
    // SPDK application context
    struct spdk_app_opts* opts_ = nullptr;
    
    // Internal helper methods
    static void appStarted(void* arg);
    static void appStopped(void* arg, int rc);
    static int parseArgs(int argc, char** argv);
    
    // Thread-safe execution helpers
    template<typename Func>
    auto executeOnSpdkThread(Func&& func) -> decltype(func());
    
    // Error conversion helpers
    static SpdkError convertErrno(int err);
    static void throwOnError(int rc, const std::string& operation);
};

// Data structures for information retrieval
struct BdevInfo {
    std::string name;
    std::string product_name;
    uint64_t num_blocks;
    uint32_t block_size;
    std::string uuid;
    bool claimed;
    std::vector<std::string> aliases;
};

struct RaidInfo {
    std::string name;
    std::string uuid;
    std::string state;
    std::string raid_level;
    uint32_t strip_size_kb;
    uint32_t num_base_bdevs;
    uint32_t num_base_bdevs_discovered;
    uint32_t num_base_bdevs_operational;
    std::vector<std::string> base_bdevs_list;
};

struct NvmfSubsystemInfo {
    std::string nqn;
    std::string subtype;
    bool allow_any_host;
    std::vector<std::string> hosts;
    std::vector<std::map<std::string, std::string>> namespaces;
    std::vector<std::map<std::string, std::string>> listeners;
};

} // namespace spdk
} // namespace spdk_flint 