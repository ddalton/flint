#pragma once

#include <string>
#include <vector>
#include <memory>
#include <functional>

namespace spdk_flint {

// Forward declarations
namespace spdk {
    class SpdkWrapper;
}

namespace kube {
    class KubeClient;
}

// Disk information structure (matches Rust UnimplementedDisk)
struct DiskInfo {
    std::string pci_address;
    std::string device_name;
    std::string driver;
    uint64_t size_bytes;
    std::string model;
    std::string vendor_id;
    std::string device_id;
    bool is_system_disk;
    bool spdk_ready;
    std::vector<std::string> mounted_partitions;
};

// Disk setup request structure
struct DiskSetupRequest {
    std::vector<std::string> pci_addresses;
    bool force_unmount = false;
    bool backup_data = false;
    uint32_t huge_pages_mb = 0;
    std::string driver_override;
};

// Disk setup result structure
struct DiskSetupResult {
    bool success = true;
    std::vector<std::string> setup_disks;
    std::vector<std::pair<std::string, std::string>> failed_disks; // pci_addr, error
    std::vector<std::string> warnings;
    uint32_t huge_pages_configured = 0;
    std::string completed_at;
};

// Disk Manager class - handles PCI device discovery and setup
class DiskManager {
public:
    explicit DiskManager(std::shared_ptr<spdk::SpdkWrapper> spdk_wrapper,
                        const std::string& node_name,
                        const std::string& target_namespace);
    ~DiskManager();

    // Main disk operations (async with callbacks)
    void discoverAllDisksAsync(std::function<void(const std::vector<DiskInfo>&, int)> callback);
    void setupDisksAsync(const DiskSetupRequest& request, 
                        std::function<void(const DiskSetupResult&, int)> callback);
    
    // Individual disk operations
    void getDiskInfoAsync(const std::string& pci_addr,
                         std::function<void(const DiskInfo&, int)> callback);
    void setupSingleDiskAsync(const std::string& pci_addr, const DiskSetupRequest& request,
                             std::function<void(int)> callback);

private:
    std::shared_ptr<spdk::SpdkWrapper> spdk_wrapper_;
    std::string node_name_;
    std::string target_namespace_;

    // PCI device discovery
    void getNvmePciDevicesAsync(std::function<void(const std::vector<std::string>&, int)> callback);
    void createBasicDiskInfoFromSysfs(const std::string& pci_addr,
                                     std::function<void(const DiskInfo&, int)> callback);

    // Driver management
    void getCurrentDriverAsync(const std::string& pci_addr,
                              std::function<void(const std::string&, int)> callback);
    void bindToDriverAsync(const std::string& pci_addr, const std::string& driver,
                          std::function<void(int)> callback);
    void loadDriverModuleAsync(const std::string& driver,
                              std::function<void(int)> callback);
    void selectOptimalSpdkDriverAsync(std::function<void(const std::string&, int)> callback);

    // Validation and setup helpers
    void validateDiskForSetupAsync(const std::string& pci_addr, bool force_unmount,
                                  std::function<void(int)> callback);
    void verifySpdkSetupAsync(const std::string& pci_addr,
                             std::function<void(int)> callback);

    // Utility functions
    void readSysfsFileAsync(const std::string& path,
                           std::function<void(const std::string&, int)> callback);
    void runCommandAsync(const std::vector<std::string>& args,
                        std::function<void(const std::string&, int)> callback);
    
    // NVMe device specific functions
    void findNvmeDeviceNameAsync(const std::string& pci_addr,
                                std::function<void(const std::string&, int)> callback);
    void getNvmeDeviceDetailsAsync(const std::string& device_name,
                                  std::function<void(uint64_t, const std::string&, int)> callback);
    
    // Internal helpers
    std::string diskCrdName(const std::string& pci_addr) const;
    bool isSystemDisk(const std::string& device_name) const;
    void parseNvmeDevice(const std::string& lspci_line, std::string& pci_addr) const;
};

} // namespace spdk_flint 