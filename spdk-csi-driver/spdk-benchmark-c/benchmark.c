#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <time.h>

#include "spdk/stdinc.h"
#include "spdk/nvme.h"
#include "spdk/env.h"
#include "spdk/log.h"
#include "spdk/string.h"

#define BLOCK_SIZE 4096
#define NUM_BLOCKS 262144  // 1GB
#define QUEUE_DEPTH 128

struct nvme_controller {
    struct spdk_nvme_ctrlr *ctrlr;
    struct spdk_nvme_ns *ns;
    struct spdk_nvme_qpair *qpair;
};

// Task structure (per I/O)
struct io_task {
    void *buffer;
    struct test_context *ctx;
    int is_read;  // 1 for read, 0 for write
};

// Test context (shared state)
struct test_context {
    struct nvme_controller *nvme;
    uint64_t offset_in_blocks;    // Current LBA offset
    uint64_t io_submitted;         // Total I/Os submitted
    uint64_t io_completed;         // Total I/Os completed
    uint32_t current_queue_depth;  // Current in-flight I/Os
    int is_draining;               // Stop submitting new I/Os
    int status;                    // Error status
    int is_read;                   // Test type
};

static void submit_io(struct io_task *task);

static void io_complete(void *arg, const struct spdk_nvme_cpl *cpl)
{
    struct io_task *task = arg;
    struct test_context *ctx = task->ctx;

    // Check for errors
    if (!spdk_nvme_cpl_is_success(cpl)) {
        printf("I/O error: %s\n", spdk_nvme_cpl_get_status_string(&cpl->status));
        ctx->status = 1;
    }

    // Update counters
    ctx->current_queue_depth--;
    ctx->io_completed++;

    // If draining, just free the task structure (not the buffer - it's part of a larger allocation)
    if (ctx->is_draining) {
        free(task);
    } else {
        // Resubmit the task with next LBA
        submit_io(task);
    }
}

static void submit_io(struct io_task *task)
{
    struct test_context *ctx = task->ctx;
    struct nvme_controller *nvme = ctx->nvme;
    uint64_t lba;
    uint32_t blocks_per_io;
    int rc;

    // Don't submit if draining
    if (ctx->is_draining) {
        return;
    }

    // Calculate LBA
    uint32_t sector_size = spdk_nvme_ns_get_sector_size(nvme->ns);
    blocks_per_io = BLOCK_SIZE / sector_size;  // 4096 / 512 = 8 sectors
    lba = ctx->offset_in_blocks * blocks_per_io;

    // Increment offset for next I/O
    ctx->offset_in_blocks++;
    if (ctx->offset_in_blocks >= NUM_BLOCKS) {
        ctx->offset_in_blocks = 0;  // Wrap around
    }

    // Submit the I/O
    if (ctx->is_read) {
        rc = spdk_nvme_ns_cmd_read(nvme->ns, nvme->qpair, task->buffer,
                                   lba, blocks_per_io,
                                   io_complete, task, 0);
    } else {
        rc = spdk_nvme_ns_cmd_write(nvme->ns, nvme->qpair, task->buffer,
                                    lba, blocks_per_io,
                                    io_complete, task, 0);
    }

    if (rc != 0) {
        printf("Failed to submit %s command: %d\n", ctx->is_read ? "read" : "write", rc);
        ctx->status = 1;
        free(task);
        return;
    }

    // Update counters AFTER successful submission
    ctx->current_queue_depth++;
    ctx->io_submitted++;

    // Check if we've submitted enough I/Os
    if (ctx->io_submitted >= NUM_BLOCKS) {
        ctx->is_draining = 1;
    }
}

static bool probe_cb(void *cb_ctx, const struct spdk_nvme_transport_id *trid,
                     struct spdk_nvme_ctrlr_opts *opts)
{
    printf("Found NVMe controller: %s\n", trid->traddr);
    return true;
}

static void attach_cb(void *cb_ctx, const struct spdk_nvme_transport_id *trid,
                      struct spdk_nvme_ctrlr *ctrlr,
                      const struct spdk_nvme_ctrlr_opts *opts)
{
    struct nvme_controller **nvme_ctx = cb_ctx;
    struct spdk_nvme_ns *ns;
    uint32_t ns_id;

    // Get first namespace
    ns_id = spdk_nvme_ctrlr_get_first_active_ns(ctrlr);
    if (ns_id == 0) {
        printf("No active namespaces found\n");
        return;
    }

    ns = spdk_nvme_ctrlr_get_ns(ctrlr, ns_id);
    if (ns == NULL) {
        printf("Failed to get namespace\n");
        return;
    }

    // Create and populate controller structure
    *nvme_ctx = malloc(sizeof(struct nvme_controller));
    if (*nvme_ctx == NULL) {
        printf("Failed to allocate nvme_controller\n");
        return;
    }

    (*nvme_ctx)->ctrlr = ctrlr;
    (*nvme_ctx)->ns = ns;
    (*nvme_ctx)->qpair = NULL;
}

static double run_test(struct nvme_controller *nvme, int is_read)
{
    struct test_context ctx = {0};
    struct io_task *task;
    struct timespec start, end;
    double elapsed, throughput, iops;
    void *all_buffer;
    int i;

    // Allocate one large buffer for all I/Os
    all_buffer = spdk_zmalloc(QUEUE_DEPTH * BLOCK_SIZE, BLOCK_SIZE, NULL,
                              SPDK_ENV_SOCKET_ID_ANY, SPDK_MALLOC_DMA);
    if (all_buffer == NULL) {
        printf("Failed to allocate I/O buffer\n");
        return -1;
    }

    // Initialize test context
    ctx.nvme = nvme;
    ctx.offset_in_blocks = 0;
    ctx.io_submitted = 0;
    ctx.io_completed = 0;
    ctx.current_queue_depth = 0;
    ctx.is_draining = 0;
    ctx.status = 0;
    ctx.is_read = is_read;

    printf("Starting %s test (1 GB)...\n", is_read ? "sequential read" : "sequential write");
    fflush(stdout);
    clock_gettime(CLOCK_MONOTONIC, &start);

    // Submit initial queue depth of I/Os
    for (i = 0; i < QUEUE_DEPTH; i++) {
        task = malloc(sizeof(struct io_task));
        if (task == NULL) {
            printf("Failed to allocate task\n");
            spdk_free(all_buffer);
            return -1;
        }

        task->buffer = (char *)all_buffer + (i * BLOCK_SIZE);
        task->ctx = &ctx;
        task->is_read = is_read;

        submit_io(task);

        // Check for immediate submission failure
        if (ctx.status != 0) {
            printf("Initial submission failed\n");
            spdk_free(all_buffer);
            return -1;
        }
    }

    // Poll for completions until all I/Os are done
    uint64_t last_progress_completed = 0;
    while (ctx.io_completed < NUM_BLOCKS && ctx.status == 0) {
        // Poll for completions
        spdk_nvme_qpair_process_completions(nvme->qpair, 0);

        // Print progress every 10%
        if (ctx.io_completed - last_progress_completed >= NUM_BLOCKS / 10) {
            printf("  Progress: %lu%% (%lu/%u blocks) - submitted=%lu, in_flight=%u\n",
                   (ctx.io_completed * 100) / NUM_BLOCKS, ctx.io_completed, (uint32_t)NUM_BLOCKS,
                   ctx.io_submitted, ctx.current_queue_depth);
            fflush(stdout);
            last_progress_completed = ctx.io_completed;
        }
    }

    clock_gettime(CLOCK_MONOTONIC, &end);

    // Check final status
    if (ctx.status != 0) {
        printf("Test failed with error\n");
        spdk_free(all_buffer);
        return -1;
    }

    elapsed = (end.tv_sec - start.tv_sec) + (end.tv_nsec - start.tv_nsec) / 1e9;
    throughput = ((double)NUM_BLOCKS * BLOCK_SIZE / elapsed) / (1024.0 * 1024.0 * 1024.0);
    iops = NUM_BLOCKS / elapsed;

    printf("✓ Completed: %lu blocks in %.2fs\n", ctx.io_completed, elapsed);
    printf("  Throughput: %.2f GB/s\n", throughput);
    printf("  IOPS: %.0f\n", iops);
    fflush(stdout);

    spdk_free(all_buffer);
    return throughput;
}

int main(void)
{
    struct nvme_controller *nvme = NULL;
    struct spdk_env_opts opts;
    int rc;
    double read_throughput, write_throughput;

    printf("\n");
    printf("═══════════════════════════════════════════════════════\n");
    printf("SPDK Native Benchmark (Polling Mode, No Kernel)\n");
    printf("═══════════════════════════════════════════════════════\n");
    printf("\n");

    // Initialize SPDK environment
    spdk_env_opts_init(&opts);
    opts.name = "spdk_benchmark";
    opts.shm_id = 0;
    opts.core_mask = "0x1";
    opts.no_pci = false;

    printf("Initializing SPDK environment...\n");
    printf("  Core mask: %s\n", opts.core_mask);
    printf("  Name: %s\n", opts.name);
    printf("  Calling spdk_env_init()...\n");
    fflush(stdout);

    rc = spdk_env_init(&opts);
    if (rc < 0) {
        printf("Failed to initialize SPDK environment: %d\n", rc);
        return 1;
    }
    printf("✓ SPDK environment initialized successfully\n");
    printf("\n");
    fflush(stdout);

    // Probe for NVMe controllers
    printf("Probing for NVMe controllers...\n");
    printf("  Calling spdk_nvme_probe()...\n");
    fflush(stdout);

    rc = spdk_nvme_probe(NULL, &nvme, probe_cb, attach_cb, NULL);
    printf("  spdk_nvme_probe() returned: %d\n", rc);
    fflush(stdout);

    if (rc != 0 || nvme == NULL) {
        printf("Failed to probe NVMe controllers\n");
        spdk_env_fini();
        return 1;
    }

    // Print device info
    printf("✓ Successfully found and attached to NVMe controller\n");
    printf("  Namespace ID: %u\n", spdk_nvme_ns_get_id(nvme->ns));
    printf("  Capacity: %lu GB\n", spdk_nvme_ns_get_size(nvme->ns) / (1024 * 1024 * 1024));
    printf("  Sector size: %u bytes\n", spdk_nvme_ns_get_sector_size(nvme->ns));
    printf("  Queue depth: %d\n", QUEUE_DEPTH);
    printf("\n");
    fflush(stdout);

    // Allocate I/O queue pair
    nvme->qpair = spdk_nvme_ctrlr_alloc_io_qpair(nvme->ctrlr, NULL, 0);
    if (nvme->qpair == NULL) {
        printf("Failed to allocate I/O queue pair\n");
        free(nvme);
        spdk_env_fini();
        return 1;
    }

    // Run tests
    printf("═══════════════════════════════════════════════════════\n");
    printf("Starting benchmark tests...\n");
    printf("═══════════════════════════════════════════════════════\n");
    printf("\n");
    fflush(stdout);

    // Sequential write test (run first to evaluate write performance)
    printf("═══════════════════════════════════════════════════════\n");
    printf("SEQUENTIAL WRITE TEST (SPDK Native Polling Mode)\n");
    printf("═══════════════════════════════════════════════════════\n");
    fflush(stdout);
    write_throughput = run_test(nvme, 0);
    printf("\n");
    fflush(stdout);

    // Sequential read test
    printf("═══════════════════════════════════════════════════════\n");
    printf("SEQUENTIAL READ TEST (SPDK Native Polling Mode)\n");
    printf("═══════════════════════════════════════════════════════\n");
    fflush(stdout);
    read_throughput = run_test(nvme, 1);
    printf("\n");
    fflush(stdout);

    // Summary
    printf("═══════════════════════════════════════════════════════\n");
    printf("BENCHMARK SUMMARY\n");
    printf("═══════════════════════════════════════════════════════\n");
    printf("Sequential Write: %.2f GB/s\n", write_throughput);
    printf("Sequential Read:  %.2f GB/s\n", read_throughput);
    printf("═══════════════════════════════════════════════════════\n");
    printf("\n");
    fflush(stdout);

    // Cleanup
    spdk_nvme_ctrlr_free_io_qpair(nvme->qpair);
    spdk_nvme_detach(nvme->ctrlr);
    free(nvme);
    spdk_env_fini();

    return 0;
}
