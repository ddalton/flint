#pragma once

#include <string>
#include <vector>
#include <memory>
#include <optional>
#include <map>
#include <future>
#include <nlohmann/json.hpp>
#include "logging.hpp"

namespace spdk_flint {
namespace kube {

using json = nlohmann::json;

// Forward declarations
class KubeClient;
class Resource;

// Custom Resource definitions based on the Rust models
struct VolumeSpec {
    std::string volume_id;
    uint64_t size_bytes;
    int num_replicas;
    std::vector<std::string> access_modes;
    std::string storage_class;
    std::map<std::string, std::string> parameters;
    
    // Serialization
    void to_json(json& j) const;
    void from_json(const json& j);
};

struct ReplicaSpec {
    std::string node;
    std::string bdev_name;
    std::string state; // "Online", "Degraded", "Failed"
    std::string lvol_uuid;
    std::optional<std::string> nvmf_nqn;
    
    void to_json(json& j) const;
    void from_json(const json& j);
};

struct SpdkVolumeSpec {
    std::string volume_id;
    uint64_t size_bytes;
    int num_replicas;
    std::vector<ReplicaSpec> replicas;
    std::string raid_level; // "0", "1", "5", etc.
    std::string state; // "Creating", "Ready", "Degraded", "Failed"
    
    void to_json(json& j) const;
    void from_json(const json& j);
};

struct SpdkVolumeStatus {
    std::string phase; // "Pending", "Available", "Bound", "Released", "Failed"
    std::string message;
    std::vector<std::string> conditions;
    
    void to_json(json& j) const;
    void from_json(const json& j);
};

struct SpdkVolume {
    std::string api_version = "storage.spdk.io/v1";
    std::string kind = "SpdkVolume";
    json metadata;
    SpdkVolumeSpec spec;
    SpdkVolumeStatus status;
    
    void to_json(json& j) const;
    void from_json(const json& j);
    
    std::string name() const;
    std::string namespace_() const;
    std::string uid() const;
};

// Similar structures for other custom resources
struct SpdkNode {
    std::string api_version = "storage.spdk.io/v1";
    std::string kind = "SpdkNode";
    json metadata;
    json spec;
    json status;
    
    void to_json(json& j) const;
    void from_json(const json& j);
    
    std::string name() const;
    std::string namespace_() const;
};

// HTTP client for Kubernetes API
class HttpClient {
public:
    HttpClient();
    ~HttpClient();
    
    struct Response {
        int status_code;
        std::string body;
        std::map<std::string, std::string> headers;
        bool success() const { return status_code >= 200 && status_code < 300; }
    };
    
    std::future<Response> get(const std::string& url, 
                             const std::map<std::string, std::string>& headers = {});
    std::future<Response> post(const std::string& url, 
                              const std::string& body,
                              const std::map<std::string, std::string>& headers = {});
    std::future<Response> put(const std::string& url, 
                             const std::string& body,
                             const std::map<std::string, std::string>& headers = {});
    std::future<Response> delete_(const std::string& url,
                                 const std::map<std::string, std::string>& headers = {});

private:
    class Impl;
    std::unique_ptr<Impl> impl_;
};

// Main Kubernetes client - equivalent to Rust kube::Client
class KubeClient {
public:
    static std::shared_ptr<KubeClient> create();
    static std::shared_ptr<KubeClient> create_incluster();
    static std::shared_ptr<KubeClient> create_from_kubeconfig(const std::string& path = "");
    
    ~KubeClient();
    
    // Generic resource operations
    template<typename T>
    std::future<std::vector<T>> list(const std::string& namespace_ = "") const;
    
    template<typename T>
    std::future<std::optional<T>> get(const std::string& name, const std::string& namespace_) const;
    
    template<typename T>
    std::future<T> create(const T& resource, const std::string& namespace_) const;
    
    template<typename T>
    std::future<T> update(const T& resource, const std::string& namespace_) const;
    
    template<typename T>
    std::future<bool> delete_(const std::string& name, const std::string& namespace_) const;
    
    // Specialized methods for our custom resources
    std::future<std::vector<SpdkVolume>> list_spdk_volumes(const std::string& namespace_ = "") const;
    std::future<std::optional<SpdkVolume>> get_spdk_volume(const std::string& name, 
                                                          const std::string& namespace_) const;
    std::future<SpdkVolume> create_spdk_volume(const SpdkVolume& volume, 
                                              const std::string& namespace_) const;
    std::future<SpdkVolume> update_spdk_volume(const SpdkVolume& volume, 
                                              const std::string& namespace_) const;
    std::future<bool> delete_spdk_volume(const std::string& name, 
                                        const std::string& namespace_) const;
    
    // Similar methods for SpdkNode
    std::future<std::vector<SpdkNode>> list_spdk_nodes(const std::string& namespace_ = "") const;
    std::future<std::optional<SpdkNode>> get_spdk_node(const std::string& name, 
                                                       const std::string& namespace_) const;
    std::future<SpdkNode> create_spdk_node(const SpdkNode& node, 
                                          const std::string& namespace_) const;
    std::future<SpdkNode> update_spdk_node(const SpdkNode& node, 
                                          const std::string& namespace_) const;
    
    // Namespace detection
    std::string get_current_namespace() const;
    
    // Node discovery for dashboard
    std::future<std::map<std::string, std::string>> discover_spdk_nodes() const;

private:
    explicit KubeClient(const std::string& api_server, 
                       const std::string& token, 
                       const std::string& ca_cert_path = "",
                       bool verify_ssl = true);
    
    std::string build_url(const std::string& api_group,
                         const std::string& version,
                         const std::string& namespace_,
                         const std::string& resource_type,
                         const std::string& name = "") const;
    
    std::map<std::string, std::string> get_auth_headers() const;
    
    class Impl;
    std::unique_ptr<Impl> impl_;
};

// Utility functions
std::string get_service_account_token();
std::string get_service_account_namespace();
std::string get_ca_cert_path();

// Resource watching (for future enhancement)
template<typename T>
class ResourceWatcher {
public:
    explicit ResourceWatcher(std::shared_ptr<KubeClient> client, 
                           const std::string& namespace_ = "");
    
    void start();
    void stop();
    
    // Callbacks for resource events
    void on_added(std::function<void(const T&)> callback);
    void on_modified(std::function<void(const T&, const T&)> callback);  // old, new
    void on_deleted(std::function<void(const T&)> callback);

private:
    std::shared_ptr<KubeClient> client_;
    std::string namespace_;
    std::atomic<bool> running_{false};
    std::vector<std::function<void(const T&)>> added_callbacks_;
    std::vector<std::function<void(const T&, const T&)>> modified_callbacks_;
    std::vector<std::function<void(const T&)>> deleted_callbacks_;
};

} // namespace kube
} // namespace spdk_flint 