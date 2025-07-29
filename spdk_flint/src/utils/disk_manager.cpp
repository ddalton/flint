#include "utils/disk_manager.hpp"
#include "spdk/spdk_wrapper.hpp"
#include "logging.hpp"
#include <thread>
#include <chrono>
#include <fstream>
#include <sstream>
#include <regex>
#include <filesystem>
#include <algorithm>
#include <cstdlib>
#include <sys/wait.h>
#include <unistd.h>

namespace spdk_flint {

DiskManager::DiskManager(std::shared_ptr<spdk::SpdkWrapper> spdk_wrapper,
                        const std::string& node_name,
                        const std::string& target_namespace)
    : spdk_wrapper_(spdk_wrapper)
    , node_name_(node_name)
    , target_namespace_(target_namespace) {
    logger()->info("[DISK_MANAGER] Initialized for node '{}' in namespace '{}'", node_name_, target_namespace_);
}

DiskManager::~DiskManager() = default;

// Main disk discovery function (ports Rust discover_all_disks)
void DiskManager::discoverAllDisksAsync(std::function<void(const std::vector<DiskInfo>&, int)> callback) {
    logger()->info("[DISK_MANAGER] Starting disk discovery for node: {}", node_name_);
    
    // Run on background thread to avoid blocking
    std::thread([this, callback]() {
        std::vector<DiskInfo> all_disks;
        
        // Get all NVMe PCI devices using lspci
        getNvmePciDevicesAsync([this, callback, &all_disks](const std::vector<std::string>& pci_devices, int error) {
            if (error != 0) {
                logger()->error("[DISK_MANAGER] Failed to get NVMe PCI devices: {}", strerror(-error));
                if (callback) callback({}, error);
                return;
            }
            
            logger()->info("[DISK_MANAGER] Processing {} PCI devices...", pci_devices.size());
            
            // Process each PCI device
            std::atomic<int> remaining_devices(pci_devices.size());
            std::atomic<bool> has_error(false);
            
            if (pci_devices.empty()) {
                logger()->info("[DISK_MANAGER] No NVMe PCI devices found");
                if (callback) callback({}, 0);
                return;
            }
            
            for (const auto& pci_addr : pci_devices) {
                logger()->debug("[DISK_MANAGER] Processing PCI device: {}", pci_addr);
                
                getDiskInfoAsync(pci_addr, [this, &all_disks, &remaining_devices, &has_error, callback, pci_addr]
                                (const DiskInfo& disk_info, int error) {
                    if (error == 0) {
                        logger()->debug("[DISK_MANAGER] Successfully got disk info for {}: name='{}', driver='{}', spdk_ready={}", 
                                       pci_addr, disk_info.device_name, disk_info.driver, disk_info.spdk_ready);
                        all_disks.push_back(disk_info);
                    } else {
                        logger()->warn("[DISK_MANAGER] Failed to get disk info for {}: {}", pci_addr, strerror(-error));
                        
                        // Try fallback discovery using sysfs only
                        createBasicDiskInfoFromSysfs(pci_addr, [this, &all_disks, pci_addr]
                                                    (const DiskInfo& fallback_disk, int fallback_error) {
                            if (fallback_error == 0) {
                                logger()->info("[DISK_MANAGER] Fallback discovery successful for {}: name='{}', driver='{}'", 
                                              pci_addr, fallback_disk.device_name, fallback_disk.driver);
                                all_disks.push_back(fallback_disk);
                            } else {
                                logger()->error("[DISK_MANAGER] Both primary and fallback discovery failed for {}", pci_addr);
                            }
                        });
                    }
                    
                    // Check if all devices processed
                    if (--remaining_devices == 0 && !has_error.exchange(true)) {
                        logger()->info("[DISK_MANAGER] Discovery completed: {} total disks found", all_disks.size());
                        for (size_t i = 0; i < all_disks.size(); i++) {
                            const auto& disk = all_disks[i];
                            logger()->debug("[DISK_MANAGER]   Disk {}: {} (PCI: {}, Driver: {}, System: {}, SPDK Ready: {})", 
                                           i+1, disk.device_name, disk.pci_address, disk.driver, 
                                           disk.is_system_disk, disk.spdk_ready);
                        }
                        
                        if (callback) callback(all_disks, 0);
                    }
                });
            }
        });
    }).detach();
}

// Get NVMe PCI devices using lspci (ports Rust get_nvme_pci_devices)
void DiskManager::getNvmePciDevicesAsync(std::function<void(const std::vector<std::string>&, int)> callback) {
    logger()->debug("[DISK_MANAGER] Scanning for NVMe PCI devices using lspci...");
    
    // Run lspci command: lspci -D -d ::0108 (NVMe class code)
    runCommandAsync({"lspci", "-D", "-d", "::0108"}, [this, callback](const std::string& output, int error) {
        if (error != 0) {
            logger()->error("[DISK_MANAGER] lspci command failed: {}", strerror(-error));
            if (callback) callback({}, error);
            return;
        }
        
        std::vector<std::string> devices;
        std::istringstream stream(output);
        std::string line;
        
        logger()->debug("[DISK_MANAGER] lspci output:");
        while (std::getline(stream, line)) {
            logger()->debug("[DISK_MANAGER]   {}", line);
            
            // Parse PCI address from first column
            std::istringstream line_stream(line);
            std::string pci_addr;
            if (line_stream >> pci_addr) {
                devices.push_back(pci_addr);
                logger()->debug("[DISK_MANAGER] Found PCI device: {}", pci_addr);
            }
        }
        
        logger()->info("[DISK_MANAGER] Total NVMe PCI devices found: {}", devices.size());
        if (callback) callback(devices, 0);
    });
}

// Get comprehensive disk information (ports Rust get_disk_info)
void DiskManager::getDiskInfoAsync(const std::string& pci_addr,
                                  std::function<void(const DiskInfo&, int)> callback) {
    logger()->debug("[DISK_MANAGER] Getting disk info for PCI: {}", pci_addr);
    
    // First get current driver
    getCurrentDriverAsync(pci_addr, [this, pci_addr, callback](const std::string& driver, int error) {
        if (error != 0) {
            logger()->error("[DISK_MANAGER] Failed to get driver for {}: {}", pci_addr, strerror(-error));
            if (callback) callback({}, error);
            return;
        }
        
        logger()->debug("[DISK_MANAGER] Driver for {}: '{}'", pci_addr, driver);
        
        // Create basic disk info and fill in details
        DiskInfo disk_info;
        disk_info.pci_address = pci_addr;
        disk_info.driver = driver;
        
        // Read vendor and device IDs from sysfs
        const std::string sysfs_path = "/sys/bus/pci/devices/" + pci_addr;
        
        readSysfsFileAsync(sysfs_path + "/vendor", [this, pci_addr, sysfs_path, disk_info, callback]
                          (const std::string& vendor_id, int vendor_error) mutable {
            if (vendor_error == 0) {
                disk_info.vendor_id = vendor_id;
            }
            
            readSysfsFileAsync(sysfs_path + "/device", [this, pci_addr, disk_info, callback]
                              (const std::string& device_id, int device_error) mutable {
                if (device_error == 0) {
                    disk_info.device_id = device_id;
                }
                
                // Determine device name and other properties based on driver
                if (disk_info.driver == "nvme") {
                    // For nvme driver, find the actual nvme device name
                    findNvmeDeviceNameAsync(pci_addr, [this, disk_info, callback]
                                           (const std::string& device_name, int name_error) mutable {
                        if (name_error == 0) {
                            disk_info.device_name = device_name;
                            disk_info.spdk_ready = false; // Bound to kernel driver
                            
                            // Get size and model from nvme device
                            getNvmeDeviceDetailsAsync(device_name, [disk_info, callback]
                                                     (uint64_t size_bytes, const std::string& model, int details_error) mutable {
                                if (details_error == 0) {
                                    disk_info.size_bytes = size_bytes;
                                    disk_info.model = model;
                                }
                                disk_info.is_system_disk = isSystemDisk(disk_info.device_name);
                                
                                if (callback) callback(disk_info, 0);
                            });
                        } else {
                            // Fallback for device name
                            disk_info.device_name = "unknown_nvme_" + pci_addr;
                            disk_info.spdk_ready = false;
                            disk_info.size_bytes = 0;
                            disk_info.model = "Unknown NVMe";
                            disk_info.is_system_disk = false;
                            
                            if (callback) callback(disk_info, 0);
                        }
                    });
                } else if (disk_info.driver == "vfio-pci" || disk_info.driver == "uio_pci_generic") {
                    // SPDK-ready devices
                    disk_info.device_name = "spdk_" + pci_addr;
                    disk_info.spdk_ready = true;
                    disk_info.size_bytes = 0; // Would need SPDK API to get size
                    disk_info.model = "SPDK Device";
                    disk_info.is_system_disk = false;
                    
                    if (callback) callback(disk_info, 0);
                } else {
                    // Other drivers or unbound
                    disk_info.device_name = disk_info.driver + "_" + pci_addr;
                    disk_info.spdk_ready = false;
                    disk_info.size_bytes = 0;
                    disk_info.model = "Unknown Device";
                    disk_info.is_system_disk = false;
                    
                    if (callback) callback(disk_info, 0);
                }
            });
        });
    });
}

// Get current driver from sysfs (ports Rust get_current_driver)
void DiskManager::getCurrentDriverAsync(const std::string& pci_addr,
                                       std::function<void(const std::string&, int)> callback) {
    const std::string driver_path = "/sys/bus/pci/devices/" + pci_addr + "/driver";
    
    std::thread([this, pci_addr, driver_path, callback]() {
        try {
            if (std::filesystem::exists(driver_path)) {
                // Read symlink to get driver name
                std::error_code ec;
                auto target = std::filesystem::read_symlink(driver_path, ec);
                if (!ec) {
                    std::string driver_name = target.filename().string();
                    logger()->debug("[DISK_MANAGER] Driver for {}: {}", pci_addr, driver_name);
                    if (callback) callback(driver_name, 0);
                } else {
                    logger()->debug("[DISK_MANAGER] Failed to read driver symlink for {}: {}", pci_addr, ec.message());
                    if (callback) callback("unbound", 0);
                }
            } else {
                logger()->debug("[DISK_MANAGER] No driver bound to {}", pci_addr);
                if (callback) callback("unbound", 0);
            }
        } catch (const std::exception& e) {
            logger()->error("[DISK_MANAGER] Exception getting driver for {}: {}", pci_addr, e.what());
            if (callback) callback("", -EINVAL);
        }
    }).detach();
}

// Helper function to find nvme device name from PCI address
void DiskManager::findNvmeDeviceNameAsync(const std::string& pci_addr,
                                         std::function<void(const std::string&, int)> callback) {
    std::thread([this, pci_addr, callback]() {
        try {
            const std::string nvme_path = "/sys/bus/pci/devices/" + pci_addr + "/nvme";
            if (std::filesystem::exists(nvme_path)) {
                for (const auto& entry : std::filesystem::directory_iterator(nvme_path)) {
                    std::string nvme_name = entry.path().filename().string();
                    if (nvme_name.find("nvme") == 0) {
                        logger()->debug("[DISK_MANAGER] Found nvme device: {}", nvme_name);
                        if (callback) callback(nvme_name, 0);
                        return;
                    }
                }
            }
            
            // Fallback: scan /dev/nvme* and match by PCI address
            for (const auto& entry : std::filesystem::directory_iterator("/dev")) {
                std::string dev_name = entry.path().filename().string();
                if (dev_name.find("nvme") == 0 && dev_name.find("n1") != std::string::npos) {
                    // Check if this device corresponds to our PCI address
                    std::string sysfs_check = "/sys/block/" + dev_name.substr(0, dev_name.find('n')) + "/device";
                    if (std::filesystem::exists(sysfs_check)) {
                        auto target = std::filesystem::read_symlink(sysfs_check);
                        if (target.string().find(pci_addr) != std::string::npos) {
                            logger()->debug("[DISK_MANAGER] Found nvme device via fallback: {}", dev_name);
                            if (callback) callback(dev_name, 0);
                            return;
                        }
                    }
                }
            }
            
            logger()->warn("[DISK_MANAGER] Could not find nvme device for PCI {}", pci_addr);
            if (callback) callback("", -ENODEV);
            
        } catch (const std::exception& e) {
            logger()->error("[DISK_MANAGER] Exception finding nvme device for {}: {}", pci_addr, e.what());
            if (callback) callback("", -EINVAL);
        }
    }).detach();
}

// Get NVMe device details (size, model) from sysfs/ioctl
void DiskManager::getNvmeDeviceDetailsAsync(const std::string& device_name,
                                           std::function<void(uint64_t, const std::string&, int)> callback) {
    std::thread([this, device_name, callback]() {
        try {
            uint64_t size_bytes = 0;
            std::string model = "Unknown NVMe";
            
            // Get size from /sys/block/nvmeXnY/size (in 512-byte sectors)
            std::string size_path = "/sys/block/" + device_name + "/size";
            std::ifstream size_file(size_path);
            if (size_file.is_open()) {
                std::string size_str;
                std::getline(size_file, size_str);
                if (!size_str.empty()) {
                    uint64_t sectors = std::stoull(size_str);
                    size_bytes = sectors * 512; // Convert sectors to bytes
                }
                size_file.close();
            }
            
            // Get model from /sys/block/nvmeXnY/device/model (for nvme controller)
            std::string base_device = device_name.substr(0, device_name.find('n')); // nvme0n1 -> nvme0
            std::string model_path = "/sys/block/" + base_device + "/device/model";
            std::ifstream model_file(model_path);
            if (model_file.is_open()) {
                std::getline(model_file, model);
                // Trim whitespace
                model.erase(0, model.find_first_not_of(" \t\n\r"));
                model.erase(model.find_last_not_of(" \t\n\r") + 1);
                model_file.close();
            }
            
            logger()->debug("[DISK_MANAGER] Device {} details: size={} MB, model='{}'", 
                           device_name, size_bytes / (1024*1024), model);
            
            if (callback) callback(size_bytes, model, 0);
            
        } catch (const std::exception& e) {
            logger()->error("[DISK_MANAGER] Exception getting details for {}: {}", device_name, e.what());
            if (callback) callback(0, "", -EINVAL);
        }
    }).detach();
}

// Create basic disk info using only sysfs (ports Rust create_basic_disk_info_from_sysfs)
void DiskManager::createBasicDiskInfoFromSysfs(const std::string& pci_addr,
                                              std::function<void(const DiskInfo&, int)> callback) {
    logger()->debug("[DISK_MANAGER] Creating basic disk info for PCI: {}", pci_addr);
    
    std::thread([this, pci_addr, callback]() {
        try {
            const std::string sysfs_path = "/sys/bus/pci/devices/" + pci_addr;
            
            // Verify PCI device exists
            if (!std::filesystem::exists(sysfs_path)) {
                logger()->error("[DISK_MANAGER] PCI device {} does not exist", pci_addr);
                if (callback) callback({}, -ENODEV);
                return;
            }
            
            DiskInfo disk_info;
            disk_info.pci_address = pci_addr;
            
            // Read basic PCI information
            disk_info.vendor_id = "0x0000";
            disk_info.device_id = "0x0000";
            
            std::ifstream vendor_file(sysfs_path + "/vendor");
            if (vendor_file.is_open()) {
                std::getline(vendor_file, disk_info.vendor_id);
                vendor_file.close();
            }
            
            std::ifstream device_file(sysfs_path + "/device");
            if (device_file.is_open()) {
                std::getline(device_file, disk_info.device_id);
                device_file.close();
            }
            
            // Get current driver
            getCurrentDriverAsync(pci_addr, [this, disk_info, callback](const std::string& driver, int error) mutable {
                if (error != 0) {
                    disk_info.driver = "unknown";
                } else {
                    disk_info.driver = driver;
                }
                
                // Generate device info based on driver state
                if (disk_info.driver == "unbound") {
                    disk_info.device_name = "unbound_" + disk_info.pci_address;
                    disk_info.size_bytes = 0;
                    disk_info.model = "Unbound NVMe Device";
                    disk_info.is_system_disk = false;
                    disk_info.spdk_ready = false;
                } else if (disk_info.driver == "vfio-pci" || disk_info.driver == "uio_pci_generic") {
                    disk_info.device_name = "spdk_" + disk_info.pci_address;
                    disk_info.size_bytes = 0;
                    disk_info.model = "SPDK-Ready Device";
                    disk_info.is_system_disk = false;
                    disk_info.spdk_ready = true;
                } else {
                    disk_info.device_name = disk_info.driver + "_" + disk_info.pci_address;
                    disk_info.size_bytes = 0;
                    disk_info.model = "Generic NVMe Device";
                    disk_info.is_system_disk = false;
                    disk_info.spdk_ready = false;
                }
                
                logger()->debug("[DISK_MANAGER] Basic disk info created for {}: name='{}', driver='{}', spdk_ready={}", 
                               pci_addr, disk_info.device_name, disk_info.driver, disk_info.spdk_ready);
                
                if (callback) callback(disk_info, 0);
            });
            
        } catch (const std::exception& e) {
            logger()->error("[DISK_MANAGER] Exception creating basic disk info for {}: {}", pci_addr, e.what());
            if (callback) callback({}, -EINVAL);
        }
    }).detach();
}

// Utility function to read sysfs files
void DiskManager::readSysfsFileAsync(const std::string& path,
                                    std::function<void(const std::string&, int)> callback) {
    std::thread([path, callback]() {
        try {
            std::ifstream file(path);
            if (file.is_open()) {
                std::string content;
                std::getline(file, content);
                
                // Trim whitespace
                content.erase(0, content.find_first_not_of(" \t\n\r"));
                content.erase(content.find_last_not_of(" \t\n\r") + 1);
                
                file.close();
                if (callback) callback(content, 0);
            } else {
                if (callback) callback("", -ENOENT);
            }
        } catch (const std::exception& e) {
            if (callback) callback("", -EINVAL);
        }
    }).detach();
}

// Utility function to run shell commands
void DiskManager::runCommandAsync(const std::vector<std::string>& args,
                                 std::function<void(const std::string&, int)> callback) {
    std::thread([args, callback]() {
        try {
            if (args.empty()) {
                if (callback) callback("", -EINVAL);
                return;
            }
            
            // Build command string
            std::string cmd = args[0];
            for (size_t i = 1; i < args.size(); i++) {
                cmd += " " + args[i];
            }
            
            // Execute command and capture output
            FILE* pipe = popen(cmd.c_str(), "r");
            if (!pipe) {
                if (callback) callback("", -errno);
                return;
            }
            
            std::ostringstream output;
            char buffer[256];
            while (fgets(buffer, sizeof(buffer), pipe) != nullptr) {
                output << buffer;
            }
            
            int exit_code = pclose(pipe);
            if (exit_code == 0) {
                if (callback) callback(output.str(), 0);
            } else {
                if (callback) callback("", -exit_code);
            }
            
        } catch (const std::exception& e) {
            if (callback) callback("", -EINVAL);
        }
    }).detach();
}

// Helper to check if device is system disk
bool DiskManager::isSystemDisk(const std::string& device_name) const {
    // Simple heuristic: check if device has mounted partitions in /
    try {
        std::ifstream mounts("/proc/mounts");
        std::string line;
        while (std::getline(mounts, line)) {
            if (line.find(device_name) != std::string::npos && 
                (line.find(" / ") != std::string::npos || line.find(" /boot") != std::string::npos)) {
                return true;
            }
        }
    } catch (const std::exception& e) {
        logger()->warn("[DISK_MANAGER] Error checking system disk status: {}", e.what());
    }
    return false;
}

// Generate stable CRD name based on PCI address (ports Rust disk_crd_name)
std::string DiskManager::diskCrdName(const std::string& pci_addr) const {
    // Convert PCI address to valid Kubernetes name: 0000:00:1f.0 → 0000-00-1f-0
    std::string pci_safe = pci_addr;
    std::replace(pci_safe.begin(), pci_safe.end(), ':', '-');
    std::replace(pci_safe.begin(), pci_safe.end(), '.', '-');
    return node_name_ + "-pci-" + pci_safe;
}

// TODO: Implement remaining functions (setupDisksAsync, setupSingleDiskAsync, etc.)
// These would include the complete disk setup workflow with driver binding,
// SPDK controller attachment, and LVS initialization.

void DiskManager::setupDisksAsync(const DiskSetupRequest& request, 
                                 std::function<void(const DiskSetupResult&, int)> callback) {
    logger()->info("[DISK_MANAGER] Starting disk setup for {} devices", request.pci_addresses.size());
    
    // TODO: Implement full setup workflow
    DiskSetupResult result;
    result.success = false;
    result.warnings.push_back("Disk setup not yet fully implemented");
    
    if (callback) callback(result, -ENOSYS);
}

void DiskManager::setupSingleDiskAsync(const std::string& pci_addr, const DiskSetupRequest& request,
                                       std::function<void(int)> callback) {
    logger()->info("[DISK_MANAGER] Setting up single disk: {}", pci_addr);
    
    // TODO: Implement complete single disk setup workflow:
    // 1. Validate disk
    // 2. Bind to SPDK driver (vfio-pci/uio_pci_generic)
    // 3. Call spdk_wrapper_->attachNvmeControllerAsync()
    // 4. Call spdk_wrapper_->createLvolStoreAsync()
    // 5. Update Kubernetes CRDs
    
    if (callback) callback(-ENOSYS);
}

} // namespace spdk_flint 