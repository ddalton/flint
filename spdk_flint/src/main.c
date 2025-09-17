/*
 * SPDK Flint Node Agent - Pure C Implementation using Ulfius
 *
 * This replaces the C++ implementation with a pure C version that can
 * directly link with SPDK libraries without C++ complications.
 */

#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <unistd.h>
#include <pthread.h>
#include <signal.h>
#include <time.h>
#include <sys/time.h>
#include <ulfius.h>
#include <jansson.h>
#include <spdk/rpc.h>
#include <spdk/jsonrpc.h>
#include <spdk/log.h>
#include <spdk/env.h>
#include <spdk/thread.h>
#include <spdk/json.h>
#include <spdk/util.h>

/* Configuration structure */
struct node_agent_config {
    uint16_t node_agent_port;
    uint16_t health_port;
    char node_id[256];
    char target_namespace[256];
    char spdk_rpc_socket[256];
    uint32_t discovery_interval;
    int auto_initialize_blobstore;
    char backup_path[256];
    char log_level[32];
};

/* Global state */
struct node_agent_state {
    struct _u_instance node_instance;
    struct _u_instance health_instance;
    struct node_agent_config config;
    pthread_t disk_monitor_thread;
    volatile int running;
    struct spdk_jsonrpc_client *rpc_client;
    pthread_mutex_t rpc_mutex;
};

static struct node_agent_state g_state = {0};

/* Logging macros - simplified version */
#define LOG_ERROR(...) do { \
    fprintf(stderr, "[ERROR] "); \
    fprintf(stderr, __VA_ARGS__); \
    fprintf(stderr, "\n"); \
} while(0)

#define LOG_INFO(...) do { \
    fprintf(stdout, "[INFO] "); \
    fprintf(stdout, __VA_ARGS__); \
    fprintf(stdout, "\n"); \
} while(0)

#define LOG_DEBUG(...) do { \
    if (strcmp(g_state.config.log_level, "debug") == 0) { \
        fprintf(stdout, "[DEBUG] "); \
        fprintf(stdout, __VA_ARGS__); \
        fprintf(stdout, "\n"); \
    } \
} while(0)

/* Forward declarations */
static int init_rpc_client(void);
static void cleanup_rpc_client(void);
static json_t* execute_spdk_rpc(const char *method, json_t *params);
static void* disk_monitor_thread_func(void *arg);

/* HTTP Endpoint Handlers */

/**
 * GET /api/disks/uninitialized
 * Discover all uninitialized disks on the node
 */
static int callback_get_uninitialized_disks(const struct _u_request *request,
                                           struct _u_response *response,
                                           void *user_data) {
    (void)request;
    (void)user_data;

    LOG_INFO("Processing uninitialized disks request");

    json_t *result = json_object();
    json_t *disks_array = json_array();

    /* Call SPDK RPC to get NVMe devices */
    json_t *nvme_params = json_object();
    json_t *nvme_result = execute_spdk_rpc("bdev_nvme_get_controllers", nvme_params);

    if (nvme_result) {
        /* Process NVMe controllers */
        size_t index;
        json_t *controller;
        json_array_foreach(nvme_result, index, controller) {
            json_t *disk = json_object();
            const char *name = json_string_value(json_object_get(controller, "name"));
            const char *trid = json_string_value(json_object_get(controller, "trid"));

            json_object_set_new(disk, "device_name", json_string(name ? name : "unknown"));
            json_object_set_new(disk, "pci_address", json_string(trid ? trid : "unknown"));
            json_object_set_new(disk, "driver", json_string("nvme"));
            json_object_set_new(disk, "spdk_ready", json_true());
            json_object_set_new(disk, "is_system_disk", json_false());

            json_array_append_new(disks_array, disk);
        }
        json_decref(nvme_result);
    }

    /* Get block devices */
    json_t *bdev_params = json_object();
    json_t *bdev_result = execute_spdk_rpc("bdev_get_bdevs", bdev_params);

    if (bdev_result) {
        size_t index;
        json_t *bdev;
        json_array_foreach(bdev_result, index, bdev) {
            json_t *disk = json_object();
            const char *name = json_string_value(json_object_get(bdev, "name"));
            json_int_t num_blocks = json_integer_value(json_object_get(bdev, "num_blocks"));
            json_int_t block_size = json_integer_value(json_object_get(bdev, "block_size"));

            json_object_set_new(disk, "device_name", json_string(name ? name : "unknown"));
            json_object_set_new(disk, "driver", json_string("bdev"));
            json_object_set_new(disk, "size_bytes", json_integer(num_blocks * block_size));
            json_object_set_new(disk, "size_gb", json_integer((num_blocks * block_size) / (1024*1024*1024)));
            json_object_set_new(disk, "spdk_ready", json_true());

            json_array_append_new(disks_array, disk);
        }
        json_decref(bdev_result);
    }

    json_object_set_new(result, "success", json_true());
    json_object_set_new(result, "disks", disks_array);
    json_object_set_new(result, "count", json_integer(json_array_size(disks_array)));
    json_object_set_new(result, "node", json_string(g_state.config.node_id));
    json_object_set_new(result, "timestamp", json_integer(time(NULL)));

    char *result_str = json_dumps(result, JSON_COMPACT);
    ulfius_set_string_body_response(response, 200, result_str);
    ulfius_add_header_to_response(response, "Content-Type", "application/json");

    free(result_str);
    json_decref(result);

    LOG_INFO("Returned %zu discovered disks", json_array_size(disks_array));
    return U_CALLBACK_CONTINUE;
}

/**
 * POST /api/disks/setup
 * Setup specified disks for SPDK usage
 */
static int callback_setup_disks(const struct _u_request *request,
                               struct _u_response *response,
                               void *user_data) {
    (void)user_data;

    LOG_INFO("Processing disk setup request");

    json_error_t error;
    json_t *body = ulfius_get_json_body_request(request, &error);

    if (!body) {
        LOG_ERROR("Invalid JSON in request body: %s", error.text);
        ulfius_set_string_body_response(response, 400, "{\"error\":\"Invalid JSON\"}");
        return U_CALLBACK_CONTINUE;
    }

    json_t *pci_addresses = json_object_get(body, "pci_addresses");
    json_t *result = json_object();
    json_t *setup_disks = json_array();

    if (json_is_array(pci_addresses)) {
        size_t index;
        json_t *addr;
        json_array_foreach(pci_addresses, index, addr) {
            const char *pci_addr = json_string_value(addr);
            LOG_DEBUG("Setting up PCI device: %s", pci_addr);

            /* Call SPDK RPC to attach NVMe controller */
            json_t *params = json_object();
            json_object_set_new(params, "name", json_string("nvme_controller"));
            json_object_set_new(params, "trtype", json_string("PCIe"));
            json_object_set_new(params, "traddr", json_string(pci_addr));

            json_t *rpc_result = execute_spdk_rpc("bdev_nvme_attach_controller", params);
            if (rpc_result) {
                json_array_append_new(setup_disks, json_string(pci_addr));
                json_decref(rpc_result);
            }
        }
    }

    json_object_set_new(result, "success", json_true());
    json_object_set_new(result, "setup_disks", setup_disks);
    json_object_set_new(result, "message", json_string("Disk setup completed"));
    json_object_set_new(result, "timestamp", json_integer(time(NULL)));

    char *result_str = json_dumps(result, JSON_COMPACT);
    ulfius_set_string_body_response(response, 200, result_str);
    ulfius_add_header_to_response(response, "Content-Type", "application/json");

    free(result_str);
    json_decref(result);
    json_decref(body);

    return U_CALLBACK_CONTINUE;
}

/**
 * GET /api/lvs
 * Get all logical volume stores
 */
static int callback_get_lvol_stores(const struct _u_request *request,
                                   struct _u_response *response,
                                   void *user_data) {
    (void)request;
    (void)user_data;

    LOG_DEBUG("Processing LVol stores request");

    json_t *params = json_object();
    json_t *rpc_result = execute_spdk_rpc("bdev_lvol_get_lvstores", params);
    json_t *result = json_object();
    json_t *lvol_stores = json_array();

    if (rpc_result && json_is_array(rpc_result)) {
        size_t index;
        json_t *lvs;
        json_array_foreach(rpc_result, index, lvs) {
            json_t *store = json_object();
            const char *uuid = json_string_value(json_object_get(lvs, "uuid"));
            const char *name = json_string_value(json_object_get(lvs, "name"));
            const char *base_bdev = json_string_value(json_object_get(lvs, "base_bdev"));
            json_int_t total_clusters = json_integer_value(json_object_get(lvs, "total_data_clusters"));
            json_int_t free_clusters = json_integer_value(json_object_get(lvs, "free_clusters"));
            json_int_t cluster_size = json_integer_value(json_object_get(lvs, "cluster_size"));

            json_object_set_new(store, "uuid", json_string(uuid ? uuid : ""));
            json_object_set_new(store, "name", json_string(name ? name : ""));
            json_object_set_new(store, "base_bdev", json_string(base_bdev ? base_bdev : ""));
            json_object_set_new(store, "total_clusters", json_integer(total_clusters));
            json_object_set_new(store, "free_clusters", json_integer(free_clusters));
            json_object_set_new(store, "cluster_size", json_integer(cluster_size));

            uint64_t total_size = total_clusters * cluster_size;
            uint64_t used_size = (total_clusters - free_clusters) * cluster_size;

            json_object_set_new(store, "total_size_mb", json_integer(total_size / (1024 * 1024)));
            json_object_set_new(store, "used_size_mb", json_integer(used_size / (1024 * 1024)));

            json_array_append_new(lvol_stores, store);
        }
        json_decref(rpc_result);
    }

    json_object_set_new(result, "lvol_stores", lvol_stores);
    json_object_set_new(result, "count", json_integer(json_array_size(lvol_stores)));
    json_object_set_new(result, "timestamp", json_integer(time(NULL)));

    char *result_str = json_dumps(result, JSON_COMPACT);
    ulfius_set_string_body_response(response, 200, result_str);
    ulfius_add_header_to_response(response, "Content-Type", "application/json");

    free(result_str);
    json_decref(result);

    return U_CALLBACK_CONTINUE;
}

/**
 * POST /api/lvs
 * Create a new logical volume store
 */
static int callback_create_lvol_store(const struct _u_request *request,
                                     struct _u_response *response,
                                     void *user_data) {
    (void)user_data;

    LOG_DEBUG("Processing LVol store creation request");

    json_error_t error;
    json_t *body = ulfius_get_json_body_request(request, &error);

    if (!body) {
        LOG_ERROR("Invalid JSON in request body: %s", error.text);
        ulfius_set_string_body_response(response, 400, "{\"error\":\"Invalid JSON\"}");
        return U_CALLBACK_CONTINUE;
    }

    const char *bdev_name = json_string_value(json_object_get(body, "bdev_name"));
    const char *lvs_name = json_string_value(json_object_get(body, "lvs_name"));
    const char *clear_method = json_string_value(json_object_get(body, "clear_method"));
    json_int_t cluster_sz = json_integer_value(json_object_get(body, "cluster_sz"));

    if (!bdev_name || !lvs_name) {
        json_decref(body);
        ulfius_set_string_body_response(response, 400, "{\"error\":\"Missing required parameters\"}");
        return U_CALLBACK_CONTINUE;
    }

    /* Create LVol store via SPDK RPC */
    json_t *params = json_object();
    json_object_set_new(params, "bdev_name", json_string(bdev_name));
    json_object_set_new(params, "lvs_name", json_string(lvs_name));
    if (clear_method) {
        json_object_set_new(params, "clear_method", json_string(clear_method));
    }
    if (cluster_sz > 0) {
        json_object_set_new(params, "cluster_sz", json_integer(cluster_sz));
    }

    json_t *rpc_result = execute_spdk_rpc("bdev_lvol_create_lvstore", params);
    json_t *result = json_object();

    if (rpc_result) {
        const char *uuid = json_string_value(json_object_get(rpc_result, "uuid"));
        json_object_set_new(result, "success", json_true());
        json_object_set_new(result, "uuid", json_string(uuid ? uuid : ""));
        json_object_set_new(result, "lvs_name", json_string(lvs_name));
        json_object_set_new(result, "bdev_name", json_string(bdev_name));
        json_decref(rpc_result);

        LOG_INFO("Successfully created LVS '%s'", lvs_name);
    } else {
        json_object_set_new(result, "success", json_false());
        json_object_set_new(result, "error", json_string("Failed to create LVol store"));
        LOG_ERROR("Failed to create LVS '%s'", lvs_name);
    }

    json_object_set_new(result, "timestamp", json_integer(time(NULL)));

    char *result_str = json_dumps(result, JSON_COMPACT);
    ulfius_set_string_body_response(response, rpc_result ? 200 : 500, result_str);
    ulfius_add_header_to_response(response, "Content-Type", "application/json");

    free(result_str);
    json_decref(result);
    json_decref(body);

    return U_CALLBACK_CONTINUE;
}

/**
 * GET /api/bdevs
 * Get block devices
 */
static int callback_get_bdevs(const struct _u_request *request,
                             struct _u_response *response,
                             void *user_data) {
    (void)user_data;

    const char *filter = u_map_get(request->map_url, "name");
    LOG_DEBUG("Processing bdevs request (filter: '%s')", filter ? filter : "none");

    json_t *params = json_object();
    if (filter) {
        json_object_set_new(params, "name", json_string(filter));
    }

    json_t *rpc_result = execute_spdk_rpc("bdev_get_bdevs", params);
    json_t *result = json_object();
    json_t *bdevs = json_array();

    if (rpc_result && json_is_array(rpc_result)) {
        size_t index;
        json_t *bdev;
        uint64_t total_storage = 0;
        int claimed_count = 0;

        json_array_foreach(rpc_result, index, bdev) {
            json_t *device = json_object();
            const char *name = json_string_value(json_object_get(bdev, "name"));
            const char *uuid = json_string_value(json_object_get(bdev, "uuid"));
            const char *product = json_string_value(json_object_get(bdev, "product_name"));
            json_int_t num_blocks = json_integer_value(json_object_get(bdev, "num_blocks"));
            json_int_t block_size = json_integer_value(json_object_get(bdev, "block_size"));
            int claimed = json_is_true(json_object_get(bdev, "claimed"));

            uint64_t size_bytes = num_blocks * block_size;
            total_storage += size_bytes;
            if (claimed) claimed_count++;

            json_object_set_new(device, "name", json_string(name ? name : ""));
            json_object_set_new(device, "uuid", json_string(uuid ? uuid : ""));
            json_object_set_new(device, "product_name", json_string(product ? product : ""));
            json_object_set_new(device, "block_size", json_integer(block_size));
            json_object_set_new(device, "num_blocks", json_integer(num_blocks));
            json_object_set_new(device, "size_bytes", json_integer(size_bytes));
            json_object_set_new(device, "size_gb", json_integer(size_bytes / (1024*1024*1024)));
            json_object_set_new(device, "claimed", json_boolean(claimed));

            json_array_append_new(bdevs, device);
        }

        json_object_set_new(result, "total_storage_gb", json_integer(total_storage / (1024*1024*1024)));
        json_object_set_new(result, "claimed_count", json_integer(claimed_count));
        json_object_set_new(result, "unclaimed_count", json_integer(json_array_size(bdevs) - claimed_count));

        json_decref(rpc_result);
    }

    json_object_set_new(result, "bdevs", bdevs);
    json_object_set_new(result, "count", json_integer(json_array_size(bdevs)));
    json_object_set_new(result, "timestamp", json_integer(time(NULL)));

    char *result_str = json_dumps(result, JSON_COMPACT);
    ulfius_set_string_body_response(response, 200, result_str);
    ulfius_add_header_to_response(response, "Content-Type", "application/json");

    free(result_str);
    json_decref(result);

    LOG_INFO("Returned %zu block devices", json_array_size(bdevs));
    return U_CALLBACK_CONTINUE;
}

/**
 * GET /api/status
 * Get service status
 */
static int callback_get_status(const struct _u_request *request,
                              struct _u_response *response,
                              void *user_data) {
    (void)request;
    (void)user_data;

    LOG_DEBUG("Processing status request");

    json_t *status = json_object();
    json_t *service = json_object();

    json_object_set_new(service, "running", json_boolean(g_state.running));
    json_object_set_new(service, "name", json_string("spdk-flint-node-agent"));
    json_object_set_new(service, "version", json_string("2.0.0"));
    json_object_set_new(service, "node_id", json_string(g_state.config.node_id));
    json_object_set_new(service, "namespace", json_string(g_state.config.target_namespace));
    json_object_set_new(service, "port", json_integer(g_state.config.node_agent_port));

    json_object_set_new(status, "service", service);

    /* Add SPDK status */
    json_t *spdk = json_object();
    json_object_set_new(spdk, "initialized", json_boolean(g_state.rpc_client != NULL));
    json_object_set_new(spdk, "rpc_socket", json_string(g_state.config.spdk_rpc_socket));
    json_object_set_new(status, "spdk", spdk);

    json_object_set_new(status, "timestamp", json_integer(time(NULL)));

    char *result_str = json_dumps(status, JSON_COMPACT);
    ulfius_set_string_body_response(response, 200, result_str);
    ulfius_add_header_to_response(response, "Content-Type", "application/json");

    free(result_str);
    json_decref(status);

    return U_CALLBACK_CONTINUE;
}

/**
 * GET /health
 * Health check endpoint
 */
static int callback_health(const struct _u_request *request,
                          struct _u_response *response,
                          void *user_data) {
    (void)request;
    (void)user_data;

    if (g_state.running && g_state.rpc_client) {
        ulfius_set_string_body_response(response, 200, "OK");
    } else {
        ulfius_set_string_body_response(response, 503, "Service Unavailable");
    }

    return U_CALLBACK_CONTINUE;
}

/**
 * GET /ready
 * Readiness check endpoint
 */
static int callback_ready(const struct _u_request *request,
                        struct _u_response *response,
                        void *user_data) {
    (void)request;
    (void)user_data;

    if (g_state.running && g_state.rpc_client) {
        ulfius_set_string_body_response(response, 200, "Ready");
    } else {
        ulfius_set_string_body_response(response, 503, "Not Ready");
    }

    return U_CALLBACK_CONTINUE;
}

/**
 * GET /version
 * Version information endpoint
 */
static int callback_version(const struct _u_request *request,
                           struct _u_response *response,
                           void *user_data) {
    (void)request;
    (void)user_data;

    json_t *version = json_object();
    json_object_set_new(version, "application", json_string("spdk-flint-node-agent"));
    json_object_set_new(version, "version", json_string("2.0.0"));
    json_object_set_new(version, "language", json_string("C"));
    json_object_set_new(version, "framework", json_string("Ulfius"));
    json_object_set_new(version, "build_date", json_string(__DATE__));
    json_object_set_new(version, "build_time", json_string(__TIME__));

    char *result_str = json_dumps(version, JSON_COMPACT);
    ulfius_set_string_body_response(response, 200, result_str);
    ulfius_add_header_to_response(response, "Content-Type", "application/json");

    free(result_str);
    json_decref(version);

    return U_CALLBACK_CONTINUE;
}

/**
 * Initialize SPDK RPC client
 */
static int init_rpc_client(void) {
    LOG_INFO("Initializing SPDK RPC client (socket: %s)", g_state.config.spdk_rpc_socket);

    pthread_mutex_init(&g_state.rpc_mutex, NULL);

    /* Connect to SPDK RPC socket */
    g_state.rpc_client = spdk_jsonrpc_client_connect(g_state.config.spdk_rpc_socket, AF_UNIX);

    if (!g_state.rpc_client) {
        LOG_ERROR("Failed to connect to SPDK RPC socket: %s", g_state.config.spdk_rpc_socket);
        return -1;
    }

    LOG_INFO("Successfully connected to SPDK RPC socket");
    return 0;
}

/**
 * Cleanup SPDK RPC client
 */
static void cleanup_rpc_client(void) {
    if (g_state.rpc_client) {
        spdk_jsonrpc_client_close(g_state.rpc_client);
        g_state.rpc_client = NULL;
    }
    pthread_mutex_destroy(&g_state.rpc_mutex);
}

/**
 * Execute SPDK RPC command
 */
static json_t* execute_spdk_rpc(const char *method, json_t *params) {
    if (!g_state.rpc_client) {
        LOG_ERROR("RPC client not initialized");
        return NULL;
    }

    pthread_mutex_lock(&g_state.rpc_mutex);

    /* Create JSON-RPC request */
    struct spdk_jsonrpc_client_request *request;
    request = spdk_jsonrpc_client_create_request();

    if (!request) {
        pthread_mutex_unlock(&g_state.rpc_mutex);
        LOG_ERROR("Failed to create RPC request");
        return NULL;
    }

    /* Build the request */
    struct spdk_json_write_ctx *w = spdk_jsonrpc_begin_request(request, 1, method);

    if (params && !json_is_null(params)) {
        char *params_str = json_dumps(params, JSON_COMPACT);
        spdk_json_write_named_object_begin(w, "params");
        /* Write params here - would need proper JSON to SPDK JSON conversion */
        spdk_json_write_object_end(w);
        free(params_str);
    }

    spdk_jsonrpc_end_request(request, w);

    /* Send request */
    int rc = spdk_jsonrpc_client_send_request(g_state.rpc_client, request);
    if (rc) {
        pthread_mutex_unlock(&g_state.rpc_mutex);
        LOG_ERROR("Failed to send RPC request: %d", rc);
        return NULL;
    }

    /* Wait for response */
    struct spdk_jsonrpc_client_response *response = NULL;

    /* Poll for response with timeout */
    int timeout_ms = 5000;
    int poll_interval_ms = 10;
    int elapsed_ms = 0;

    while (elapsed_ms < timeout_ms) {
        response = spdk_jsonrpc_client_get_response(g_state.rpc_client);
        if (response) break;

        usleep(poll_interval_ms * 1000);
        elapsed_ms += poll_interval_ms;
    }

    pthread_mutex_unlock(&g_state.rpc_mutex);

    if (!response) {
        LOG_ERROR("RPC request timed out");
        return NULL;
    }

    /* Parse response - simplified, would need proper implementation */
    json_t *result = json_object();

    /* Free response */
    spdk_jsonrpc_client_free_response(response);

    return result;
}

/**
 * Disk monitoring thread function
 */
static void* disk_monitor_thread_func(void *arg) {
    (void)arg;

    LOG_INFO("Disk monitoring thread started (interval: %u seconds)",
             g_state.config.discovery_interval);

    int cycle_count = 0;

    while (g_state.running) {
        cycle_count++;
        LOG_DEBUG("Starting monitoring cycle #%d", cycle_count);

        /* Perform disk discovery */
        json_t *params = json_object();
        json_t *result = execute_spdk_rpc("bdev_get_bdevs", params);

        if (result) {
            size_t count = json_is_array(result) ? json_array_size(result) : 0;
            LOG_DEBUG("Monitoring cycle #%d: discovered %zu devices", cycle_count, count);
            json_decref(result);
        }

        /* Sleep for discovery interval */
        for (uint32_t i = 0; i < g_state.config.discovery_interval && g_state.running; i++) {
            sleep(1);
        }
    }

    LOG_INFO("Disk monitoring thread exiting");
    return NULL;
}

/**
 * Load configuration from environment variables
 */
static void load_config_from_environment(struct node_agent_config *config) {
    const char *env_val;

    /* Set defaults */
    config->node_agent_port = 8090;
    config->health_port = 9809;
    strcpy(config->node_id, "node-1");
    strcpy(config->target_namespace, "flint-system");
    strcpy(config->spdk_rpc_socket, "/var/tmp/spdk.sock");
    config->discovery_interval = 30;
    config->auto_initialize_blobstore = 0;
    strcpy(config->backup_path, "/var/backup");
    strcpy(config->log_level, "info");

    /* Override from environment */
    if ((env_val = getenv("NODE_AGENT_PORT"))) {
        config->node_agent_port = atoi(env_val);
    }
    if ((env_val = getenv("HEALTH_PORT"))) {
        config->health_port = atoi(env_val);
    }
    if ((env_val = getenv("NODE_ID"))) {
        strncpy(config->node_id, env_val, sizeof(config->node_id) - 1);
    }
    if ((env_val = getenv("TARGET_NAMESPACE"))) {
        strncpy(config->target_namespace, env_val, sizeof(config->target_namespace) - 1);
    }
    if ((env_val = getenv("SPDK_RPC_SOCKET"))) {
        strncpy(config->spdk_rpc_socket, env_val, sizeof(config->spdk_rpc_socket) - 1);
    }
    if ((env_val = getenv("DISCOVERY_INTERVAL"))) {
        config->discovery_interval = atoi(env_val);
    }
    if ((env_val = getenv("LOG_LEVEL"))) {
        strncpy(config->log_level, env_val, sizeof(config->log_level) - 1);
    }
}

/**
 * Initialize node agent HTTP server
 */
static int init_node_agent_server(void) {
    LOG_INFO("Initializing node agent HTTP server on port %u", g_state.config.node_agent_port);

    if (ulfius_init_instance(&g_state.node_instance, g_state.config.node_agent_port, NULL, NULL) != U_OK) {
        LOG_ERROR("Failed to initialize node agent HTTP server");
        return -1;
    }

    /* Register endpoints */
    ulfius_add_endpoint_by_val(&g_state.node_instance, "GET", "/api/disks/uninitialized",
                               NULL, 0, &callback_get_uninitialized_disks, NULL);
    ulfius_add_endpoint_by_val(&g_state.node_instance, "POST", "/api/disks/setup",
                               NULL, 0, &callback_setup_disks, NULL);
    ulfius_add_endpoint_by_val(&g_state.node_instance, "GET", "/api/lvs",
                               NULL, 0, &callback_get_lvol_stores, NULL);
    ulfius_add_endpoint_by_val(&g_state.node_instance, "POST", "/api/lvs",
                               NULL, 0, &callback_create_lvol_store, NULL);
    ulfius_add_endpoint_by_val(&g_state.node_instance, "GET", "/api/bdevs",
                               NULL, 0, &callback_get_bdevs, NULL);
    ulfius_add_endpoint_by_val(&g_state.node_instance, "GET", "/api/status",
                               NULL, 0, &callback_get_status, NULL);

    /* Start the server */
    if (ulfius_start_framework(&g_state.node_instance) != U_OK) {
        LOG_ERROR("Failed to start node agent HTTP server");
        ulfius_clean_instance(&g_state.node_instance);
        return -1;
    }

    LOG_INFO("Node agent HTTP server started successfully");
    return 0;
}

/**
 * Initialize health check HTTP server
 */
static int init_health_server(void) {
    LOG_INFO("Initializing health server on port %u", g_state.config.health_port);

    if (ulfius_init_instance(&g_state.health_instance, g_state.config.health_port, NULL, NULL) != U_OK) {
        LOG_ERROR("Failed to initialize health HTTP server");
        return -1;
    }

    /* Register health endpoints */
    ulfius_add_endpoint_by_val(&g_state.health_instance, "GET", "/health",
                               NULL, 0, &callback_health, NULL);
    ulfius_add_endpoint_by_val(&g_state.health_instance, "GET", "/ready",
                               NULL, 0, &callback_ready, NULL);
    ulfius_add_endpoint_by_val(&g_state.health_instance, "GET", "/version",
                               NULL, 0, &callback_version, NULL);

    /* Start the server */
    if (ulfius_start_framework(&g_state.health_instance) != U_OK) {
        LOG_ERROR("Failed to start health HTTP server");
        ulfius_clean_instance(&g_state.health_instance);
        return -1;
    }

    LOG_INFO("Health server started successfully");
    return 0;
}

/**
 * Signal handler for clean shutdown
 */
static void signal_handler(int sig) {
    (void)sig;
    LOG_INFO("Received shutdown signal");
    g_state.running = 0;
}

/**
 * Print usage information
 */
static void print_usage(const char *prog_name) {
    printf("SPDK Flint Node Agent - Pure C Implementation\n\n");
    printf("Usage: %s [OPTIONS]\n\n", prog_name);
    printf("OPTIONS:\n");
    printf("  --log-level <level>  Log level (debug, info, warn, error)\n");
    printf("  --rpc-socket <path>  SPDK RPC socket path (default: /var/tmp/spdk.sock)\n");
    printf("  --help, -h           Show this help message\n");
    printf("  --version, -v        Show version information\n\n");
    printf("ENVIRONMENT VARIABLES:\n");
    printf("  NODE_ID              Node identifier\n");
    printf("  LOG_LEVEL            Log level\n");
    printf("  HEALTH_PORT          Health check port (default: 9809)\n");
    printf("  NODE_AGENT_PORT      Node agent API port (default: 8090)\n");
    printf("  TARGET_NAMESPACE     Kubernetes namespace (default: flint-system)\n");
    printf("  SPDK_RPC_SOCKET      SPDK RPC socket path\n");
    printf("  DISCOVERY_INTERVAL   Disk discovery interval in seconds (default: 30)\n\n");
}

/**
 * Main entry point
 */
int main(int argc, char *argv[]) {
    /* Parse command line arguments */
    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--help") == 0 || strcmp(argv[i], "-h") == 0) {
            print_usage(argv[0]);
            return 0;
        } else if (strcmp(argv[i], "--version") == 0 || strcmp(argv[i], "-v") == 0) {
            printf("SPDK Flint Node Agent\n");
            printf("Version: 2.0.0\n");
            printf("Language: Pure C\n");
            printf("Framework: Ulfius\n");
            printf("Build: %s %s\n", __DATE__, __TIME__);
            return 0;
        } else if (strcmp(argv[i], "--log-level") == 0 && i + 1 < argc) {
            strncpy(g_state.config.log_level, argv[++i], sizeof(g_state.config.log_level) - 1);
        } else if (strcmp(argv[i], "--rpc-socket") == 0 && i + 1 < argc) {
            strncpy(g_state.config.spdk_rpc_socket, argv[++i], sizeof(g_state.config.spdk_rpc_socket) - 1);
        }
    }

    /* Load configuration */
    load_config_from_environment(&g_state.config);

    LOG_INFO("========================================");
    LOG_INFO("Starting SPDK Flint Node Agent (Pure C)");
    LOG_INFO("Version: 2.0.0 | Build: %s %s", __DATE__, __TIME__);
    LOG_INFO("Process: PID=%d", getpid());
    LOG_INFO("========================================");

    /* Set up signal handlers */
    signal(SIGINT, signal_handler);
    signal(SIGTERM, signal_handler);

    /* Initialize components */
    g_state.running = 1;

    /* Initialize SPDK RPC client */
    if (init_rpc_client() != 0) {
        LOG_ERROR("Failed to initialize SPDK RPC client");
        return 1;
    }

    /* Start node agent HTTP server */
    if (init_node_agent_server() != 0) {
        LOG_ERROR("Failed to initialize node agent server");
        cleanup_rpc_client();
        return 1;
    }

    /* Start health HTTP server */
    if (init_health_server() != 0) {
        LOG_ERROR("Failed to initialize health server");
        ulfius_stop_framework(&g_state.node_instance);
        ulfius_clean_instance(&g_state.node_instance);
        cleanup_rpc_client();
        return 1;
    }

    /* Start disk monitoring thread */
    if (pthread_create(&g_state.disk_monitor_thread, NULL, disk_monitor_thread_func, NULL) != 0) {
        LOG_ERROR("Failed to create disk monitoring thread");
        g_state.running = 0;
    }

    LOG_INFO("========================================");
    LOG_INFO("SPDK Flint Node Agent is now running");
    LOG_INFO("Services: HTTP API (port %u), Health (port %u)",
             g_state.config.node_agent_port, g_state.config.health_port);
    LOG_INFO("========================================");

    /* Main loop - wait for shutdown signal */
    while (g_state.running) {
        sleep(1);
    }

    LOG_INFO("Shutting down SPDK Flint Node Agent");

    /* Stop disk monitoring thread */
    if (g_state.disk_monitor_thread) {
        pthread_join(g_state.disk_monitor_thread, NULL);
    }

    /* Stop HTTP servers */
    ulfius_stop_framework(&g_state.node_instance);
    ulfius_clean_instance(&g_state.node_instance);
    ulfius_stop_framework(&g_state.health_instance);
    ulfius_clean_instance(&g_state.health_instance);

    /* Cleanup RPC client */
    cleanup_rpc_client();

    LOG_INFO("========================================");
    LOG_INFO("SPDK Flint Node Agent shutdown complete");
    LOG_INFO("========================================");

    return 0;
}