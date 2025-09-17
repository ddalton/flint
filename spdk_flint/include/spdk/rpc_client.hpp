#pragma once

#include <string>
#include <functional>
#include <memory>
#include <nlohmann/json.hpp>
#include <sys/socket.h>
#include <sys/un.h>
#include <future>
#include <mutex>
#include <atomic>

namespace spdk_flint {
namespace spdk {

using json = nlohmann::json;

// RPC response callback type
using RpcCallback = std::function<void(const json& response, int error)>;

// RPC Client for communicating with SPDK via Unix socket
class RpcClient {
public:
    explicit RpcClient(const std::string& socket_path = "/var/tmp/spdk.sock");
    ~RpcClient();

    // Initialize the RPC client connection
    bool connect();

    // Disconnect from SPDK
    void disconnect();

    // Check if connected
    bool isConnected() const;

    // Send an RPC request synchronously
    json callRpc(const std::string& method, const json& params = json::object());

    // Send an RPC request asynchronously
    void callRpcAsync(const std::string& method,
                      const json& params,
                      RpcCallback callback);

    // Ublk-specific RPC methods
    json createUblkTarget();
    json startUblkDisk(int ublk_id, const std::string& bdev_name);
    json stopUblkDisk(int ublk_id);
    json getUblkDisks();

    // Bdev RPC methods (for volume operations)
    json getBdevs(const std::string& name = "");
    json createLvolBdev(const std::string& lvs_name, const std::string& lvol_name, uint64_t size);
    json deleteBdev(const std::string& bdev_name);
    json resizeLvolBdev(const std::string& name, uint64_t size);

    // Lvol store operations
    json getLvolStores(const std::string& uuid = "", const std::string& lvs_name = "");
    json createLvolStore(const std::string& bdev_name, const std::string& lvs_name, uint32_t cluster_sz = 0);
    json deleteLvolStore(const std::string& uuid = "", const std::string& lvs_name = "");

    // NVMe-oF operations
    json nvmfGetSubsystems();
    json nvmfCreateSubsystem(const std::string& nqn, const std::string& serial_number = "",
                             const std::string& model_number = "", bool allow_any_host = true);
    json nvmfDeleteSubsystem(const std::string& nqn);
    json nvmfSubsystemAddListener(const std::string& nqn, const std::string& trtype,
                                  const std::string& traddr, const std::string& trsvcid,
                                  const std::string& adrfam = "");
    json nvmfSubsystemAddNs(const std::string& nqn, const std::string& bdev_name,
                            const std::string& ns_id = "", const std::string& nguid = "",
                            const std::string& eui64 = "", const std::string& uuid = "");

private:
    // Internal helper methods
    json sendRequest(const json& request);
    std::string readResponse();
    void throwOnRpcError(const json& response, const std::string& operation);

    // Socket management
    std::string socket_path_;
    int socket_fd_;
    mutable std::mutex socket_mutex_;
    std::atomic<bool> connected_{false};

    // Request ID management
    std::atomic<uint64_t> next_request_id_{1};

    // Helper to format RPC request
    json formatRpcRequest(const std::string& method, const json& params);
};

} // namespace spdk
} // namespace spdk_flint