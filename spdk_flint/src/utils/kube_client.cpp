#include "utils/kube_client.hpp"
#include <fstream>
#include <sstream>
#include <curl/curl.h>
#include <thread>
#include <mutex>

namespace spdk_flint {
namespace kube {

// Utility functions for service account authentication
std::string get_service_account_token() {
    std::ifstream token_file("/var/run/secrets/kubernetes.io/serviceaccount/token");
    if (!token_file.is_open()) {
        return "";
    }
    
    std::string token;
    std::getline(token_file, token);
    return token;
}

std::string get_service_account_namespace() {
    std::ifstream namespace_file("/var/run/secrets/kubernetes.io/serviceaccount/namespace");
    if (!namespace_file.is_open()) {
        return "default";
    }
    
    std::string namespace_;
    std::getline(namespace_file, namespace_);
    return namespace_.empty() ? "default" : namespace_;
}

std::string get_ca_cert_path() {
    return "/var/run/secrets/kubernetes.io/serviceaccount/ca.crt";
}

// HTTP Client implementation
class HttpClient::Impl {
public:
    Impl() {
        curl_global_init(CURL_GLOBAL_DEFAULT);
    }
    
    ~Impl() {
        curl_global_cleanup();
    }
    
    static size_t WriteCallback(void* contents, size_t size, size_t nmemb, std::string* response) {
        size_t total_size = size * nmemb;
        response->append(static_cast<char*>(contents), total_size);
        return total_size;
    }
    
    Response make_request(const std::string& method, const std::string& url, 
                         const std::string& body = "",
                         const std::map<std::string, std::string>& headers = {}) {
        CURL* curl = curl_easy_init();
        if (!curl) {
            return {0, "", {}, false};
        }
        
        Response response;
        
        // Set basic options
        curl_easy_setopt(curl, CURLOPT_URL, url.c_str());
        curl_easy_setopt(curl, CURLOPT_WRITEFUNCTION, WriteCallback);
        curl_easy_setopt(curl, CURLOPT_WRITEDATA, &response.body);
        curl_easy_setopt(curl, CURLOPT_TIMEOUT, 30L);
        curl_easy_setopt(curl, CURLOPT_FOLLOWLOCATION, 1L);
        
        // Set method
        if (method == "POST") {
            curl_easy_setopt(curl, CURLOPT_POST, 1L);
            if (!body.empty()) {
                curl_easy_setopt(curl, CURLOPT_POSTFIELDS, body.c_str());
            }
        } else if (method == "PUT") {
            curl_easy_setopt(curl, CURLOPT_CUSTOMREQUEST, "PUT");
            if (!body.empty()) {
                curl_easy_setopt(curl, CURLOPT_POSTFIELDS, body.c_str());
            }
        } else if (method == "DELETE") {
            curl_easy_setopt(curl, CURLOPT_CUSTOMREQUEST, "DELETE");
        }
        
        // Set headers
        struct curl_slist* header_list = nullptr;
        for (const auto& header : headers) {
            std::string header_str = header.first + ": " + header.second;
            header_list = curl_slist_append(header_list, header_str.c_str());
        }
        if (header_list) {
            curl_easy_setopt(curl, CURLOPT_HTTPHEADER, header_list);
        }
        
        // Perform request
        CURLcode res = curl_easy_perform(curl);
        if (res == CURLE_OK) {
            long status_code;
            curl_easy_getinfo(curl, CURLINFO_RESPONSE_CODE, &status_code);
            response.status_code = static_cast<int>(status_code);
        }
        
        // Cleanup
        if (header_list) {
            curl_slist_free_all(header_list);
        }
        curl_easy_cleanup(curl);
        
        return response;
    }
};

HttpClient::HttpClient() : impl_(std::make_unique<Impl>()) {}
HttpClient::~HttpClient() = default;

std::future<HttpClient::Response> HttpClient::get(const std::string& url, 
                                                 const std::map<std::string, std::string>& headers) {
    return std::async(std::launch::async, [this, url, headers]() {
        return impl_->make_request("GET", url, "", headers);
    });
}

std::future<HttpClient::Response> HttpClient::post(const std::string& url, 
                                                  const std::string& body,
                                                  const std::map<std::string, std::string>& headers) {
    return std::async(std::launch::async, [this, url, body, headers]() {
        return impl_->make_request("POST", url, body, headers);
    });
}

std::future<HttpClient::Response> HttpClient::put(const std::string& url, 
                                                 const std::string& body,
                                                 const std::map<std::string, std::string>& headers) {
    return std::async(std::launch::async, [this, url, body, headers]() {
        return impl_->make_request("PUT", url, body, headers);
    });
}

std::future<HttpClient::Response> HttpClient::delete_(const std::string& url,
                                                     const std::map<std::string, std::string>& headers) {
    return std::async(std::launch::async, [this, url, headers]() {
        return impl_->make_request("DELETE", url, "", headers);
    });
}

// Custom Resource serialization implementations
void VolumeSpec::to_json(json& j) const {
    j = json{
        {"volume_id", volume_id},
        {"size_bytes", size_bytes},
        {"num_replicas", num_replicas},
        {"access_modes", access_modes},
        {"storage_class", storage_class},
        {"parameters", parameters}
    };
}

void VolumeSpec::from_json(const json& j) {
    j.at("volume_id").get_to(volume_id);
    j.at("size_bytes").get_to(size_bytes);
    j.at("num_replicas").get_to(num_replicas);
    j.at("access_modes").get_to(access_modes);
    j.at("storage_class").get_to(storage_class);
    j.at("parameters").get_to(parameters);
}

void ReplicaSpec::to_json(json& j) const {
    j = json{
        {"node", node},
        {"bdev_name", bdev_name},
        {"state", state},
        {"lvol_uuid", lvol_uuid}
    };
    if (nvmf_nqn.has_value()) {
        j["nvmf_nqn"] = nvmf_nqn.value();
    }
}

void ReplicaSpec::from_json(const json& j) {
    j.at("node").get_to(node);
    j.at("bdev_name").get_to(bdev_name);
    j.at("state").get_to(state);
    j.at("lvol_uuid").get_to(lvol_uuid);
    if (j.contains("nvmf_nqn")) {
        nvmf_nqn = j.at("nvmf_nqn").get<std::string>();
    }
}

void SpdkVolumeSpec::to_json(json& j) const {
    j = json{
        {"volume_id", volume_id},
        {"size_bytes", size_bytes},
        {"num_replicas", num_replicas},
        {"raid_level", raid_level},
        {"state", state}
    };
    
    json replicas_json = json::array();
    for (const auto& replica : replicas) {
        json replica_json;
        replica.to_json(replica_json);
        replicas_json.push_back(replica_json);
    }
    j["replicas"] = replicas_json;
}

void SpdkVolumeSpec::from_json(const json& j) {
    j.at("volume_id").get_to(volume_id);
    j.at("size_bytes").get_to(size_bytes);
    j.at("num_replicas").get_to(num_replicas);
    j.at("raid_level").get_to(raid_level);
    j.at("state").get_to(state);
    
    if (j.contains("replicas")) {
        replicas.clear();
        for (const auto& replica_json : j.at("replicas")) {
            ReplicaSpec replica;
            replica.from_json(replica_json);
            replicas.push_back(replica);
        }
    }
}

void SpdkVolumeStatus::to_json(json& j) const {
    j = json{
        {"phase", phase},
        {"message", message},
        {"conditions", conditions}
    };
}

void SpdkVolumeStatus::from_json(const json& j) {
    j.at("phase").get_to(phase);
    j.at("message").get_to(message);
    j.at("conditions").get_to(conditions);
}

void SpdkVolume::to_json(json& j) const {
    j = json{
        {"apiVersion", api_version},
        {"kind", kind},
        {"metadata", metadata}
    };
    
    json spec_json;
    spec.to_json(spec_json);
    j["spec"] = spec_json;
    
    json status_json;
    status.to_json(status_json);
    j["status"] = status_json;
}

void SpdkVolume::from_json(const json& j) {
    j.at("apiVersion").get_to(api_version);
    j.at("kind").get_to(kind);
    j.at("metadata").get_to(metadata);
    
    if (j.contains("spec")) {
        spec.from_json(j.at("spec"));
    }
    if (j.contains("status")) {
        status.from_json(j.at("status"));
    }
}

std::string SpdkVolume::name() const {
    if (metadata.contains("name")) {
        return metadata.at("name").get<std::string>();
    }
    return "";
}

std::string SpdkVolume::namespace_() const {
    if (metadata.contains("namespace")) {
        return metadata.at("namespace").get<std::string>();
    }
    return "default";
}

std::string SpdkVolume::uid() const {
    if (metadata.contains("uid")) {
        return metadata.at("uid").get<std::string>();
    }
    return "";
}

void SpdkNode::to_json(json& j) const {
    j = json{
        {"apiVersion", api_version},
        {"kind", kind},
        {"metadata", metadata},
        {"spec", spec},
        {"status", status}
    };
}

void SpdkNode::from_json(const json& j) {
    j.at("apiVersion").get_to(api_version);
    j.at("kind").get_to(kind);
    j.at("metadata").get_to(metadata);
    if (j.contains("spec")) {
        j.at("spec").get_to(spec);
    }
    if (j.contains("status")) {
        j.at("status").get_to(status);
    }
}

std::string SpdkNode::name() const {
    if (metadata.contains("name")) {
        return metadata.at("name").get<std::string>();
    }
    return "";
}

std::string SpdkNode::namespace_() const {
    if (metadata.contains("namespace")) {
        return metadata.at("namespace").get<std::string>();
    }
    return "default";
}

// KubeClient implementation
class KubeClient::Impl {
public:
    std::string api_server;
    std::string token;
    std::string ca_cert_path;
    bool verify_ssl;
    HttpClient http_client;
    
    Impl(const std::string& api_server, const std::string& token, 
         const std::string& ca_cert_path, bool verify_ssl)
        : api_server(api_server), token(token), ca_cert_path(ca_cert_path), verify_ssl(verify_ssl) {}
};

KubeClient::KubeClient(const std::string& api_server, const std::string& token, 
                      const std::string& ca_cert_path, bool verify_ssl)
    : impl_(std::make_unique<Impl>(api_server, token, ca_cert_path, verify_ssl)) {
}

KubeClient::~KubeClient() = default;

std::shared_ptr<KubeClient> KubeClient::create_incluster() {
    try {
        std::string token = get_service_account_token();
        if (token.empty()) {
            LOG_ERROR("Failed to read service account token");
            return nullptr;
        }
        
        std::string api_server = "https://kubernetes.default.svc";
        std::string ca_cert_path = get_ca_cert_path();
        
        return std::shared_ptr<KubeClient>(new KubeClient(api_server, token, ca_cert_path, true));
    } catch (const std::exception& e) {
        LOG_ERROR("Failed to create in-cluster client: {}", e.what());
        return nullptr;
    }
}

std::shared_ptr<KubeClient> KubeClient::create_from_kubeconfig(const std::string& path) {
    // TODO: Implement kubeconfig parsing
    LOG_WARN("kubeconfig client not implemented yet");
    return nullptr;
}

std::shared_ptr<KubeClient> KubeClient::create() {
    auto client = create_incluster();
    if (!client) {
        client = create_from_kubeconfig();
    }
    return client;
}

std::string KubeClient::build_url(const std::string& api_group,
                                 const std::string& version,
                                 const std::string& namespace_,
                                 const std::string& resource_type,
                                 const std::string& name) const {
    std::ostringstream url;
    url << impl_->api_server;
    
    if (api_group.empty()) {
        url << "/api/" << version;
    } else {
        url << "/apis/" << api_group << "/" << version;
    }
    
    if (!namespace_.empty()) {
        url << "/namespaces/" << namespace_;
    }
    
    url << "/" << resource_type;
    
    if (!name.empty()) {
        url << "/" << name;
    }
    
    return url.str();
}

std::map<std::string, std::string> KubeClient::get_auth_headers() const {
    return {
        {"Authorization", "Bearer " + impl_->token},
        {"Content-Type", "application/json"},
        {"Accept", "application/json"}
    };
}

std::string KubeClient::get_current_namespace() const {
    return get_service_account_namespace();
}

// Specialized SPDK volume operations
std::future<std::vector<SpdkVolume>> KubeClient::list_spdk_volumes(const std::string& namespace_) const {
    return std::async(std::launch::async, [this, namespace_]() -> std::vector<SpdkVolume> {
        try {
            std::string url = build_url("storage.spdk.io", "v1", namespace_, "spdkvolumes");
            auto response_future = impl_->http_client.get(url, get_auth_headers());
            auto response = response_future.get();
            
            if (!response.success()) {
                LOG_ERROR("Failed to list SPDK volumes: HTTP {}", response.status_code);
                return {};
            }
            
            json response_json = json::parse(response.body);
            std::vector<SpdkVolume> volumes;
            
            if (response_json.contains("items")) {
                for (const auto& item : response_json.at("items")) {
                    SpdkVolume volume;
                    volume.from_json(item);
                    volumes.push_back(volume);
                }
            }
            
            return volumes;
        } catch (const std::exception& e) {
            LOG_ERROR("Exception listing SPDK volumes: {}", e.what());
            return {};
        }
    });
}

std::future<std::optional<SpdkVolume>> KubeClient::get_spdk_volume(const std::string& name, 
                                                                  const std::string& namespace_) const {
    return std::async(std::launch::async, [this, name, namespace_]() -> std::optional<SpdkVolume> {
        try {
            std::string url = build_url("storage.spdk.io", "v1", namespace_, "spdkvolumes", name);
            auto response_future = impl_->http_client.get(url, get_auth_headers());
            auto response = response_future.get();
            
            if (!response.success()) {
                if (response.status_code == 404) {
                    return std::nullopt;
                }
                LOG_ERROR("Failed to get SPDK volume {}: HTTP {}", name, response.status_code);
                return std::nullopt;
            }
            
            json response_json = json::parse(response.body);
            SpdkVolume volume;
            volume.from_json(response_json);
            return volume;
        } catch (const std::exception& e) {
            LOG_ERROR("Exception getting SPDK volume {}: {}", name, e.what());
            return std::nullopt;
        }
    });
}

std::future<SpdkVolume> KubeClient::create_spdk_volume(const SpdkVolume& volume, 
                                                      const std::string& namespace_) const {
    return std::async(std::launch::async, [this, volume, namespace_]() -> SpdkVolume {
        try {
            std::string url = build_url("storage.spdk.io", "v1", namespace_, "spdkvolumes");
            
            json volume_json;
            volume.to_json(volume_json);
            
            auto response_future = impl_->http_client.post(url, volume_json.dump(), get_auth_headers());
            auto response = response_future.get();
            
            if (!response.success()) {
                LOG_ERROR("Failed to create SPDK volume: HTTP {}", response.status_code);
                throw std::runtime_error("Failed to create SPDK volume");
            }
            
            json response_json = json::parse(response.body);
            SpdkVolume created_volume;
            created_volume.from_json(response_json);
            return created_volume;
        } catch (const std::exception& e) {
            LOG_ERROR("Exception creating SPDK volume: {}", e.what());
            throw;
        }
    });
}

std::future<SpdkVolume> KubeClient::update_spdk_volume(const SpdkVolume& volume, 
                                                      const std::string& namespace_) const {
    return std::async(std::launch::async, [this, volume, namespace_]() -> SpdkVolume {
        try {
            std::string url = build_url("storage.spdk.io", "v1", namespace_, "spdkvolumes", volume.name());
            
            json volume_json;
            volume.to_json(volume_json);
            
            auto response_future = impl_->http_client.put(url, volume_json.dump(), get_auth_headers());
            auto response = response_future.get();
            
            if (!response.success()) {
                LOG_ERROR("Failed to update SPDK volume {}: HTTP {}", volume.name(), response.status_code);
                throw std::runtime_error("Failed to update SPDK volume");
            }
            
            json response_json = json::parse(response.body);
            SpdkVolume updated_volume;
            updated_volume.from_json(response_json);
            return updated_volume;
        } catch (const std::exception& e) {
            LOG_ERROR("Exception updating SPDK volume {}: {}", volume.name(), e.what());
            throw;
        }
    });
}

std::future<bool> KubeClient::delete_spdk_volume(const std::string& name, 
                                                const std::string& namespace_) const {
    return std::async(std::launch::async, [this, name, namespace_]() -> bool {
        try {
            std::string url = build_url("storage.spdk.io", "v1", namespace_, "spdkvolumes", name);
            auto response_future = impl_->http_client.delete_(url, get_auth_headers());
            auto response = response_future.get();
            
            return response.success() || response.status_code == 404;
        } catch (const std::exception& e) {
            LOG_ERROR("Exception deleting SPDK volume {}: {}", name, e.what());
            return false;
        }
    });
}

// Node operations - similar implementations
std::future<std::vector<SpdkNode>> KubeClient::list_spdk_nodes(const std::string& namespace_) const {
    return std::async(std::launch::async, [this, namespace_]() -> std::vector<SpdkNode> {
        try {
            std::string url = build_url("storage.spdk.io", "v1", namespace_, "spdknodes");
            auto response_future = impl_->http_client.get(url, get_auth_headers());
            auto response = response_future.get();
            
            if (!response.success()) {
                LOG_ERROR("Failed to list SPDK nodes: HTTP {}", response.status_code);
                return {};
            }
            
            json response_json = json::parse(response.body);
            std::vector<SpdkNode> nodes;
            
            if (response_json.contains("items")) {
                for (const auto& item : response_json.at("items")) {
                    SpdkNode node;
                    node.from_json(item);
                    nodes.push_back(node);
                }
            }
            
            LOG_INFO("Successfully listed {} SPDK nodes", nodes.size());
            return nodes;
        } catch (const std::exception& e) {
            LOG_ERROR("Exception listing SPDK nodes: {}", e.what());
            return {};
        }
    });
}

std::future<std::optional<SpdkNode>> KubeClient::get_spdk_node(const std::string& name, 
                                                              const std::string& namespace_) const {
    return std::async(std::launch::async, [this, name, namespace_]() -> std::optional<SpdkNode> {
        try {
            std::string url = build_url("storage.spdk.io", "v1", namespace_, "spdknodes", name);
            auto response_future = impl_->http_client.get(url, get_auth_headers());
            auto response = response_future.get();
            
            if (!response.success()) {
                if (response.status_code == 404) {
                    LOG_DEBUG("SPDK node {} not found", name);
                    return std::nullopt;
                }
                LOG_ERROR("Failed to get SPDK node {}: HTTP {}", name, response.status_code);
                return std::nullopt;
            }
            
            json response_json = json::parse(response.body);
            SpdkNode node;
            node.from_json(response_json);
            
            LOG_DEBUG("Successfully retrieved SPDK node {}", name);
            return node;
        } catch (const std::exception& e) {
            LOG_ERROR("Exception getting SPDK node {}: {}", name, e.what());
            return std::nullopt;
        }
    });
}

std::future<SpdkNode> KubeClient::create_spdk_node(const SpdkNode& node, 
                                                   const std::string& namespace_) const {
    return std::async(std::launch::async, [this, node, namespace_]() -> SpdkNode {
        try {
            std::string url = build_url("storage.spdk.io", "v1", namespace_, "spdknodes");
            json body;
            node.to_json(body);
            
            auto response_future = impl_->http_client.post(url, body.dump(), get_auth_headers());
            auto response = response_future.get();
            
            if (!response.success()) {
                LOG_ERROR("Failed to create SPDK node: HTTP {}", response.status_code);
                throw std::runtime_error("Failed to create SPDK node");
            }
            
            json response_json = json::parse(response.body);
            SpdkNode created_node;
            created_node.from_json(response_json);
            
            LOG_INFO("Successfully created SPDK node {}", created_node.get_name());
            return created_node;
        } catch (const std::exception& e) {
            LOG_ERROR("Exception creating SPDK node: {}", e.what());
            throw;
        }
    });
}

std::future<SpdkNode> KubeClient::update_spdk_node(const SpdkNode& node, 
                                                   const std::string& namespace_) const {
    return std::async(std::launch::async, [this, node, namespace_]() -> SpdkNode {
        try {
            std::string url = build_url("storage.spdk.io", "v1", namespace_, "spdknodes", node.get_name());
            json body;
            node.to_json(body);
            
            auto response_future = impl_->http_client.put(url, body.dump(), get_auth_headers());
            auto response = response_future.get();
            
            if (!response.success()) {
                LOG_ERROR("Failed to update SPDK node {}: HTTP {}", node.get_name(), response.status_code);
                throw std::runtime_error("Failed to update SPDK node");
            }
            
            json response_json = json::parse(response.body);
            SpdkNode updated_node;
            updated_node.from_json(response_json);
            
            LOG_INFO("Successfully updated SPDK node {}", updated_node.get_name());
            return updated_node;
        } catch (const std::exception& e) {
            LOG_ERROR("Exception updating SPDK node {}: {}", node.get_name(), e.what());
            throw;
        }
    });
}

std::future<std::map<std::string, std::string>> KubeClient::discover_spdk_nodes() const {
    return std::async(std::launch::async, [this]() -> std::map<std::string, std::string> {
        try {
            // First get regular Kubernetes nodes
            std::string url = build_url("", "v1", "", "nodes");
            auto response_future = impl_->http_client.get(url, get_auth_headers());
            auto response = response_future.get();
            
            if (!response.success()) {
                LOG_ERROR("Failed to list Kubernetes nodes: HTTP {}", response.status_code);
                return {};
            }
            
            json response_json = json::parse(response.body);
            std::map<std::string, std::string> discovered_nodes;
            
            if (response_json.contains("items")) {
                for (const auto& item : response_json.at("items")) {
                    if (item.contains("metadata") && item.at("metadata").contains("name")) {
                        std::string node_name = item.at("metadata").at("name");
                        
                        // Check if node has storage-related labels or annotations
                        bool is_storage_node = false;
                        std::string node_role = "worker";
                        
                        // Check labels for storage indicators
                        if (item.at("metadata").contains("labels")) {
                            const auto& labels = item.at("metadata").at("labels");
                            
                            // Look for storage-related labels
                            if (labels.contains("node-type") && labels.at("node-type") == "storage") {
                                is_storage_node = true;
                                node_role = "storage";
                            } else if (labels.contains("storage.spdk.io/enabled") && 
                                     labels.at("storage.spdk.io/enabled") == "true") {
                                is_storage_node = true;
                                node_role = "storage";
                            } else if (labels.contains("kubernetes.io/arch")) {
                                // Assume nodes are potential storage nodes
                                is_storage_node = true;
                                node_role = "potential-storage";
                            }
                        }
                        
                        // Get node address
                        std::string node_address = node_name;
                        if (item.contains("status") && item.at("status").contains("addresses")) {
                            for (const auto& addr : item.at("status").at("addresses")) {
                                if (addr.contains("type") && addr.at("type") == "InternalIP" &&
                                    addr.contains("address")) {
                                    node_address = addr.at("address");
                                    break;
                                }
                            }
                        }
                        
                        if (is_storage_node || node_role == "potential-storage") {
                            discovered_nodes[node_name] = node_address;
                            LOG_DEBUG("Discovered SPDK node candidate: {} ({})", node_name, node_address);
                        }
                    }
                }
            }
            
            LOG_INFO("Discovered {} potential SPDK storage nodes", discovered_nodes.size());
            return discovered_nodes;
            
        } catch (const std::exception& e) {
            LOG_ERROR("Exception discovering SPDK nodes: {}", e.what());
            return {};
        }
    });
}

} // namespace kube
} // namespace spdk_flint 