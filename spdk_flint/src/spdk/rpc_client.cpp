#include "spdk/rpc_client.hpp"
#include "logging.hpp"
#include <unistd.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <chrono>
#include <thread>
#include <sstream>
#include <iomanip>
#include <cstring>
#include <cerrno>

namespace spdk_flint {
namespace spdk {

RpcClient::RpcClient(const std::string& socket_path)
    : socket_path_(socket_path), socket_fd_(-1) {
    logger()->debug("[RPC_CLIENT] Creating RPC client for socket: {}", socket_path);
}

RpcClient::~RpcClient() {
    if (isConnected()) {
        disconnect();
    }
}

bool RpcClient::connect() {
    std::lock_guard<std::mutex> lock(socket_mutex_);

    if (connected_.load()) {
        logger()->debug("[RPC_CLIENT] Already connected to SPDK");
        return true;
    }

    logger()->info("[RPC_CLIENT] Connecting to SPDK RPC socket: {}", socket_path_);

    // Create Unix domain socket
    socket_fd_ = socket(AF_UNIX, SOCK_STREAM, 0);
    if (socket_fd_ < 0) {
        logger()->error("[RPC_CLIENT] Failed to create socket: {}", strerror(errno));
        return false;
    }

    // Set socket address
    struct sockaddr_un addr;
    memset(&addr, 0, sizeof(addr));
    addr.sun_family = AF_UNIX;
    strncpy(addr.sun_path, socket_path_.c_str(), sizeof(addr.sun_path) - 1);

    // Try to connect with retries
    int max_retries = 30;
    int retry_delay_ms = 1000;

    for (int i = 0; i < max_retries; i++) {
        if (::connect(socket_fd_, (struct sockaddr*)&addr, sizeof(addr)) == 0) {
            connected_.store(true);
            logger()->info("[RPC_CLIENT] Successfully connected to SPDK RPC socket");
            return true;
        }

        if (i < max_retries - 1) {
            logger()->debug("[RPC_CLIENT] Connection attempt {} failed: {}. Retrying in {} ms...",
                          i + 1, strerror(errno), retry_delay_ms);
            std::this_thread::sleep_for(std::chrono::milliseconds(retry_delay_ms));
        }
    }

    logger()->error("[RPC_CLIENT] Failed to connect to SPDK after {} attempts: {}",
                   max_retries, strerror(errno));
    close(socket_fd_);
    socket_fd_ = -1;
    return false;
}

void RpcClient::disconnect() {
    std::lock_guard<std::mutex> lock(socket_mutex_);

    if (socket_fd_ >= 0) {
        logger()->info("[RPC_CLIENT] Disconnecting from SPDK RPC socket");
        close(socket_fd_);
        socket_fd_ = -1;
    }
    connected_.store(false);
}

bool RpcClient::isConnected() const {
    return connected_.load();
}

json RpcClient::formatRpcRequest(const std::string& method, const json& params) {
    json request;
    request["jsonrpc"] = "2.0";
    request["id"] = next_request_id_.fetch_add(1);
    request["method"] = method;

    if (!params.empty()) {
        request["params"] = params;
    } else {
        request["params"] = json::object();
    }

    return request;
}

json RpcClient::sendRequest(const json& request) {
    if (!isConnected()) {
        throw std::runtime_error("Not connected to SPDK RPC socket");
    }

    // Convert JSON to string and add newline
    std::string request_str = request.dump() + "\n";

    logger()->debug("[RPC_CLIENT] Sending request: {}", request_str);

    // Send request
    ssize_t sent = send(socket_fd_, request_str.c_str(), request_str.length(), 0);
    if (sent < 0) {
        throw std::runtime_error(std::string("Failed to send RPC request: ") + strerror(errno));
    }

    // Read response
    std::string response_str = readResponse();

    logger()->debug("[RPC_CLIENT] Received response: {}", response_str);

    // Parse JSON response
    try {
        return json::parse(response_str);
    } catch (const json::parse_error& e) {
        logger()->error("[RPC_CLIENT] Failed to parse response JSON: {}", e.what());
        throw std::runtime_error("Invalid JSON response from SPDK");
    }
}

std::string RpcClient::readResponse() {
    const size_t buffer_size = 65536;
    char buffer[buffer_size];
    std::string response;

    // Read until we get a complete JSON response (ends with newline)
    while (true) {
        ssize_t received = recv(socket_fd_, buffer, buffer_size - 1, 0);
        if (received < 0) {
            throw std::runtime_error(std::string("Failed to receive RPC response: ") + strerror(errno));
        }
        if (received == 0) {
            throw std::runtime_error("SPDK RPC socket closed unexpectedly");
        }

        buffer[received] = '\0';
        response += buffer;

        // Check if we have a complete response (ends with newline)
        if (!response.empty() && response.back() == '\n') {
            // Remove trailing newline
            response.pop_back();
            break;
        }
    }

    return response;
}

void RpcClient::throwOnRpcError(const json& response, const std::string& operation) {
    if (response.contains("error")) {
        auto error = response["error"];
        int code = error.value("code", 0);
        std::string message = error.value("message", "Unknown error");

        std::ostringstream error_msg;
        error_msg << operation << " failed: " << message << " (code: " << code << ")";

        logger()->error("[RPC_CLIENT] {}", error_msg.str());
        throw std::runtime_error(error_msg.str());
    }
}

json RpcClient::callRpc(const std::string& method, const json& params) {
    std::lock_guard<std::mutex> lock(socket_mutex_);

    auto request = formatRpcRequest(method, params);
    auto response = sendRequest(request);

    throwOnRpcError(response, method);

    if (response.contains("result")) {
        return response["result"];
    }

    return json::object();
}

void RpcClient::callRpcAsync(const std::string& method,
                              const json& params,
                              RpcCallback callback) {
    // For now, implement as synchronous call in a separate thread
    std::thread([this, method, params, callback]() {
        try {
            auto result = callRpc(method, params);
            if (callback) {
                callback(result, 0);
            }
        } catch (const std::exception& e) {
            logger()->error("[RPC_CLIENT] Async RPC failed: {}", e.what());
            if (callback) {
                json error_response;
                error_response["error"] = e.what();
                callback(error_response, -1);
            }
        }
    }).detach();
}

// Ublk-specific RPC methods

json RpcClient::createUblkTarget() {
    logger()->info("[RPC_CLIENT] Creating ublk target");
    return callRpc("ublk_create_target", json::object());
}

json RpcClient::startUblkDisk(int ublk_id, const std::string& bdev_name) {
    logger()->info("[RPC_CLIENT] Starting ublk disk: id={}, bdev={}", ublk_id, bdev_name);

    json params;
    params["ublk_id"] = ublk_id;
    params["bdev_name"] = bdev_name;

    return callRpc("ublk_start_disk", params);
}

json RpcClient::stopUblkDisk(int ublk_id) {
    logger()->info("[RPC_CLIENT] Stopping ublk disk: id={}", ublk_id);

    json params;
    params["ublk_id"] = ublk_id;

    return callRpc("ublk_stop_disk", params);
}

json RpcClient::getUblkDisks() {
    logger()->debug("[RPC_CLIENT] Getting ublk disks");
    return callRpc("ublk_get_disks", json::object());
}

// Bdev RPC methods

json RpcClient::getBdevs(const std::string& name) {
    logger()->debug("[RPC_CLIENT] Getting bdevs: name={}", name);

    json params;
    if (!name.empty()) {
        params["name"] = name;
    }

    return callRpc("bdev_get_bdevs", params);
}

json RpcClient::createLvolBdev(const std::string& lvs_name, const std::string& lvol_name, uint64_t size) {
    logger()->info("[RPC_CLIENT] Creating lvol bdev: lvs={}, name={}, size={}", lvs_name, lvol_name, size);

    json params;
    params["lvs_name"] = lvs_name;
    params["lvol_name"] = lvol_name;
    params["size"] = size;

    return callRpc("bdev_lvol_create", params);
}

json RpcClient::deleteBdev(const std::string& bdev_name) {
    logger()->info("[RPC_CLIENT] Deleting bdev: {}", bdev_name);

    json params;
    params["name"] = bdev_name;

    return callRpc("bdev_lvol_delete", params);
}

json RpcClient::resizeLvolBdev(const std::string& name, uint64_t size) {
    logger()->info("[RPC_CLIENT] Resizing lvol bdev: name={}, new_size={}", name, size);

    json params;
    params["name"] = name;
    params["size"] = size;

    return callRpc("bdev_lvol_resize", params);
}

// Lvol store operations

json RpcClient::getLvolStores(const std::string& uuid, const std::string& lvs_name) {
    logger()->debug("[RPC_CLIENT] Getting lvol stores: uuid={}, name={}", uuid, lvs_name);

    json params;
    if (!uuid.empty()) {
        params["uuid"] = uuid;
    }
    if (!lvs_name.empty()) {
        params["lvs_name"] = lvs_name;
    }

    return callRpc("bdev_lvol_get_lvstores", params);
}

json RpcClient::createLvolStore(const std::string& bdev_name, const std::string& lvs_name, uint32_t cluster_sz) {
    logger()->info("[RPC_CLIENT] Creating lvol store: bdev={}, name={}, cluster_sz={}",
                  bdev_name, lvs_name, cluster_sz);

    json params;
    params["bdev_name"] = bdev_name;
    params["lvs_name"] = lvs_name;
    if (cluster_sz > 0) {
        params["cluster_sz"] = cluster_sz;
    }

    return callRpc("bdev_lvol_create_lvstore", params);
}

json RpcClient::deleteLvolStore(const std::string& uuid, const std::string& lvs_name) {
    logger()->info("[RPC_CLIENT] Deleting lvol store: uuid={}, name={}", uuid, lvs_name);

    json params;
    if (!uuid.empty()) {
        params["uuid"] = uuid;
    } else if (!lvs_name.empty()) {
        params["lvs_name"] = lvs_name;
    } else {
        throw std::invalid_argument("Either uuid or lvs_name must be specified");
    }

    return callRpc("bdev_lvol_delete_lvstore", params);
}

// NVMe-oF operations

json RpcClient::nvmfGetSubsystems() {
    logger()->debug("[RPC_CLIENT] Getting NVMe-oF subsystems");
    return callRpc("nvmf_get_subsystems", json::object());
}

json RpcClient::nvmfCreateSubsystem(const std::string& nqn, const std::string& serial_number,
                                    const std::string& model_number, bool allow_any_host) {
    logger()->info("[RPC_CLIENT] Creating NVMe-oF subsystem: nqn={}", nqn);

    json params;
    params["nqn"] = nqn;
    params["allow_any_host"] = allow_any_host;

    if (!serial_number.empty()) {
        params["serial_number"] = serial_number;
    }
    if (!model_number.empty()) {
        params["model_number"] = model_number;
    }

    return callRpc("nvmf_create_subsystem", params);
}

json RpcClient::nvmfDeleteSubsystem(const std::string& nqn) {
    logger()->info("[RPC_CLIENT] Deleting NVMe-oF subsystem: {}", nqn);

    json params;
    params["nqn"] = nqn;

    return callRpc("nvmf_delete_subsystem", params);
}

json RpcClient::nvmfSubsystemAddListener(const std::string& nqn, const std::string& trtype,
                                         const std::string& traddr, const std::string& trsvcid,
                                         const std::string& adrfam) {
    logger()->info("[RPC_CLIENT] Adding listener to subsystem {}: {}:{}:{}", nqn, trtype, traddr, trsvcid);

    json params;
    params["nqn"] = nqn;

    json listen_address;
    listen_address["trtype"] = trtype;
    listen_address["traddr"] = traddr;
    listen_address["trsvcid"] = trsvcid;
    if (!adrfam.empty()) {
        listen_address["adrfam"] = adrfam;
    }

    params["listen_address"] = listen_address;

    return callRpc("nvmf_subsystem_add_listener", params);
}

json RpcClient::nvmfSubsystemAddNs(const std::string& nqn, const std::string& bdev_name,
                                   const std::string& ns_id, const std::string& nguid,
                                   const std::string& eui64, const std::string& uuid) {
    logger()->info("[RPC_CLIENT] Adding namespace to subsystem {}: bdev={}", nqn, bdev_name);

    json params;
    params["nqn"] = nqn;

    json ns;
    ns["bdev_name"] = bdev_name;
    if (!ns_id.empty()) {
        ns["ns_id"] = std::stoi(ns_id);
    }
    if (!nguid.empty()) {
        ns["nguid"] = nguid;
    }
    if (!eui64.empty()) {
        ns["eui64"] = eui64;
    }
    if (!uuid.empty()) {
        ns["uuid"] = uuid;
    }

    params["namespace"] = ns;

    return callRpc("nvmf_subsystem_add_ns", params);
}

} // namespace spdk
} // namespace spdk_flint