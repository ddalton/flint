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

struct io_context {
    void *buffer;
    int completed;
    int success;
};

static void io_complete(void *arg, const struct spdk_nvme_cpl *cpl)
{
    struct io_context *ctx = arg;
    ctx->completed = 1;
    ctx->success = spdk_nvme_cpl_is_success(cpl);
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
    uint64_t num_sectors, size_gb;
    uint32_t sector_size;

    ns_id = spdk_nvme_ctrlr_get_first_active_ns(ctrlr);
    if (ns_id == 0) {
        printf("No active namespaces found\n");
        return;
    }

    ns = spdk_nvme_ctrlr_get_ns(ctrlr, ns_id);
    if (!ns) {
        printf("Failed to get namespace\n");
        return;
    }

    struct spdk_nvme_qpair *qpair = spdk_nvme_ctrlr_alloc_io_qpair(ctrlr, NULL, 0);
    if (!qpair) {
        printf("Failed to allocate I/O queue pair\n");
        return;
    }

    struct nvme_controller *nvme = malloc(sizeof(*nvme));
    nvme->ctrlr = ctrlr;
    nvme->ns = ns;
    nvme->qpair = qpair;
    *nvme_ctx = nvme;

    num_sectors = spdk_nvme_ns_get_num_sectors(ns);
    sector_size = spdk_nvme_ns_get_sector_size(ns);
    size_gb = (num_sectors * sector_size) / (1024 * 1024 * 1024);

    printf("✓ Attached NVMe controller\n");
    printf("  Namespace ID: %u\n", ns_id);
    printf("  Capacity: %lu GB\n", size_gb);
    printf("  Sector size: %u bytes\n", sector_size);
    printf("  Queue depth: %d\n", QUEUE_DEPTH);
}

static double run_sequential_read(struct nvme_controller *nvme)
{
    struct io_context contexts[QUEUE_DEPTH];
    void *buffer;
    uint64_t lba = 0;
    uint64_t submitted = 0, completed = 0;
    uint32_t in_flight = 0;
    uint32_t sector_size = spdk_nvme_ns_get_sector_size(nvme->ns);
    uint32_t blocks_per_io = BLOCK_SIZE / sector_size;
    struct timespec start, end;
    double elapsed, throughput, iops;
    int rc;

    printf("\n═══════════════════════════════════════════════════════\n");
    printf("SEQUENTIAL READ TEST (SPDK Native Polling Mode)\n");
    printf("═══════════════════════════════════════════════════════\n");

    buffer = spdk_zmalloc(BLOCK_SIZE * QUEUE_DEPTH, 0x1000, NULL, SPDK_ENV_SOCKET_ID_ANY, SPDK_MALLOC_DMA);
    if (!buffer) {
        printf("Failed to allocate DMA buffer\n");
        return -1;
    }

    memset(contexts, 0, sizeof(contexts));
    for (int i = 0; i < QUEUE_DEPTH; i++) {
        contexts[i].buffer = (char *)buffer + (i * BLOCK_SIZE);
    }

    clock_gettime(CLOCK_MONOTONIC, &start);

    while (completed < NUM_BLOCKS) {
        // Submit I/Os up to queue depth
        while (in_flight < QUEUE_DEPTH && submitted < NUM_BLOCKS) {
            uint32_t ctx_idx = submitted % QUEUE_DEPTH;
            contexts[ctx_idx].completed = 0;
            contexts[ctx_idx].success = 0;

            rc = spdk_nvme_ns_cmd_read(nvme->ns, nvme->qpair,
                                       contexts[ctx_idx].buffer,
                                       lba, blocks_per_io,
                                       io_complete, &contexts[ctx_idx], 0);
            if (rc != 0) {
                printf("Failed to submit read command: %d\n", rc);
                spdk_free(buffer);
                return -1;
            }

            lba += blocks_per_io;
            submitted++;
            in_flight++;
        }

        // Poll for completions
        spdk_nvme_qpair_process_completions(nvme->qpair, 0);

        // Check for completed I/Os
        for (int i = 0; i < QUEUE_DEPTH; i++) {
            if (contexts[i].completed) {
                if (!contexts[i].success) {
                    printf("I/O failed\n");
                    spdk_free(buffer);
                    return -1;
                }
                contexts[i].completed = 0;
                completed++;
                in_flight--;
            }
        }
    }

    clock_gettime(CLOCK_MONOTONIC, &end);
    elapsed = (end.tv_sec - start.tv_sec) + (end.tv_nsec - start.tv_nsec) / 1e9;
    throughput = ((double)NUM_BLOCKS * BLOCK_SIZE / elapsed) / (1024.0 * 1024.0 * 1024.0);
    iops = NUM_BLOCKS / elapsed;

    printf("Completed: %lu blocks in %.2fs\n", completed, elapsed);
    printf("Throughput: %.2f GB/s\n", throughput);
    printf("IOPS: %.0f\n", iops);

    spdk_free(buffer);
    return throughput;
}

static double run_sequential_write(struct nvme_controller *nvme)
{
    struct io_context contexts[QUEUE_DEPTH];
    void *buffer;
    uint64_t lba = 0;
    uint64_t submitted = 0, completed = 0;
    uint32_t in_flight = 0;
    uint32_t sector_size = spdk_nvme_ns_get_sector_size(nvme->ns);
    uint32_t blocks_per_io = BLOCK_SIZE / sector_size;
    struct timespec start, end;
    double elapsed, throughput, iops;
    int rc;

    printf("\n═══════════════════════════════════════════════════════\n");
    printf("SEQUENTIAL WRITE TEST (SPDK Native Polling Mode)\n");
    printf("═══════════════════════════════════════════════════════\n");

    buffer = spdk_zmalloc(BLOCK_SIZE * QUEUE_DEPTH, 0x1000, NULL, SPDK_ENV_SOCKET_ID_ANY, SPDK_MALLOC_DMA);
    if (!buffer) {
        printf("Failed to allocate DMA buffer\n");
        return -1;
    }

    // Fill buffer with test data
    memset(buffer, 0xAA, BLOCK_SIZE * QUEUE_DEPTH);

    memset(contexts, 0, sizeof(contexts));
    for (int i = 0; i < QUEUE_DEPTH; i++) {
        contexts[i].buffer = (char *)buffer + (i * BLOCK_SIZE);
    }

    clock_gettime(CLOCK_MONOTONIC, &start);

    while (completed < NUM_BLOCKS) {
        // Submit I/Os up to queue depth
        while (in_flight < QUEUE_DEPTH && submitted < NUM_BLOCKS) {
            uint32_t ctx_idx = submitted % QUEUE_DEPTH;
            contexts[ctx_idx].completed = 0;
            contexts[ctx_idx].success = 0;

            rc = spdk_nvme_ns_cmd_write(nvme->ns, nvme->qpair,
                                        contexts[ctx_idx].buffer,
                                        lba, blocks_per_io,
                                        io_complete, &contexts[ctx_idx], 0);
            if (rc != 0) {
                printf("Failed to submit write command: %d\n", rc);
                spdk_free(buffer);
                return -1;
            }

            lba += blocks_per_io;
            submitted++;
            in_flight++;
        }

        // Poll for completions
        spdk_nvme_qpair_process_completions(nvme->qpair, 0);

        // Check for completed I/Os
        for (int i = 0; i < QUEUE_DEPTH; i++) {
            if (contexts[i].completed) {
                if (!contexts[i].success) {
                    printf("I/O failed\n");
                    spdk_free(buffer);
                    return -1;
                }
                contexts[i].completed = 0;
                completed++;
                in_flight--;
            }
        }
    }

    clock_gettime(CLOCK_MONOTONIC, &end);
    elapsed = (end.tv_sec - start.tv_sec) + (end.tv_nsec - start.tv_nsec) / 1e9;
    throughput = ((double)NUM_BLOCKS * BLOCK_SIZE / elapsed) / (1024.0 * 1024.0 * 1024.0);
    iops = NUM_BLOCKS / elapsed;

    printf("Completed: %lu blocks in %.2fs\n", completed, elapsed);
    printf("Throughput: %.2f GB/s\n", throughput);
    printf("IOPS: %.0f\n", iops);

    spdk_free(buffer);
    return throughput;
}

static double run_random_read(struct nvme_controller *nvme)
{
    struct io_context contexts[QUEUE_DEPTH];
    void *buffer;
    uint64_t submitted = 0, completed = 0;
    uint32_t in_flight = 0;
    uint32_t sector_size = spdk_nvme_ns_get_sector_size(nvme->ns);
    uint32_t blocks_per_io = BLOCK_SIZE / sector_size;
    uint64_t max_lba = spdk_nvme_ns_get_num_sectors(nvme->ns) - blocks_per_io;
    struct timespec start, end;
    double elapsed, throughput, iops;
    uint64_t rand_state = 0x12345678;
    int rc;

    printf("\n═══════════════════════════════════════════════════════\n");
    printf("RANDOM READ TEST (4K blocks, SPDK Native Polling)\n");
    printf("═══════════════════════════════════════════════════════\n");

    buffer = spdk_zmalloc(BLOCK_SIZE * QUEUE_DEPTH, 0x1000, NULL, SPDK_ENV_SOCKET_ID_ANY, SPDK_MALLOC_DMA);
    if (!buffer) {
        printf("Failed to allocate DMA buffer\n");
        return -1;
    }

    memset(contexts, 0, sizeof(contexts));
    for (int i = 0; i < QUEUE_DEPTH; i++) {
        contexts[i].buffer = (char *)buffer + (i * BLOCK_SIZE);
    }

    clock_gettime(CLOCK_MONOTONIC, &start);

    while (completed < NUM_BLOCKS) {
        // Submit I/Os up to queue depth
        while (in_flight < QUEUE_DEPTH && submitted < NUM_BLOCKS) {
            uint32_t ctx_idx = submitted % QUEUE_DEPTH;
            contexts[ctx_idx].completed = 0;
            contexts[ctx_idx].success = 0;

            // Generate random LBA
            rand_state = rand_state * 1103515245 + 12345;
            uint64_t lba = (rand_state % max_lba) & ~((uint64_t)(blocks_per_io - 1));

            rc = spdk_nvme_ns_cmd_read(nvme->ns, nvme->qpair,
                                       contexts[ctx_idx].buffer,
                                       lba, blocks_per_io,
                                       io_complete, &contexts[ctx_idx], 0);
            if (rc != 0) {
                printf("Failed to submit read command: %d\n", rc);
                spdk_free(buffer);
                return -1;
            }

            submitted++;
            in_flight++;
        }

        // Poll for completions
        spdk_nvme_qpair_process_completions(nvme->qpair, 0);

        // Check for completed I/Os
        for (int i = 0; i < QUEUE_DEPTH; i++) {
            if (contexts[i].completed) {
                if (!contexts[i].success) {
                    printf("I/O failed\n");
                    spdk_free(buffer);
                    return -1;
                }
                contexts[i].completed = 0;
                completed++;
                in_flight--;
            }
        }
    }

    clock_gettime(CLOCK_MONOTONIC, &end);
    elapsed = (end.tv_sec - start.tv_sec) + (end.tv_nsec - start.tv_nsec) / 1e9;
    throughput = ((double)NUM_BLOCKS * BLOCK_SIZE / elapsed) / (1024.0 * 1024.0 * 1024.0);
    iops = NUM_BLOCKS / elapsed;

    printf("Completed: %lu blocks in %.2fs\n", completed, elapsed);
    printf("Throughput: %.2f GB/s\n", throughput);
    printf("IOPS: %.0f (4K random reads)\n", iops);

    spdk_free(buffer);
    return iops;
}

int main(int argc, char **argv)
{
    struct spdk_env_opts opts;
    struct nvme_controller *nvme = NULL;
    int rc;

    printf("═══════════════════════════════════════════════════════\n");
    printf("SPDK Native Benchmark (Polling Mode, No Kernel)\n");
    printf("═══════════════════════════════════════════════════════\n\n");

    spdk_env_opts_init(&opts);
    opts.name = "spdk_benchmark";
    opts.core_mask = "0x1";

    printf("Initializing SPDK environment...\n");
    printf("  Core mask: %s\n", opts.core_mask);
    printf("  Name: %s\n", opts.name);
    printf("  Calling spdk_env_init()...\n");
    fflush(stdout);

    if (spdk_env_init(&opts) < 0) {
        fprintf(stderr, "Failed to initialize SPDK environment\n");
        return 1;
    }
    printf("✓ SPDK environment initialized successfully\n");
    fflush(stdout);

    printf("\nProbing for NVMe controllers...\n");
    printf("  Calling spdk_nvme_probe()...\n");
    fflush(stdout);

    rc = spdk_nvme_probe(NULL, &nvme, probe_cb, attach_cb, NULL);
    printf("  spdk_nvme_probe() returned: %d\n", rc);
    fflush(stdout);

    if (rc != 0) {
        fprintf(stderr, "Failed to probe NVMe controllers (rc=%d)\n", rc);
        return 1;
    }

    if (!nvme) {
        fprintf(stderr, "No NVMe controllers found\n");
        return 1;
    }
    printf("✓ Successfully found and attached to NVMe controller\n");
    fflush(stdout);

    printf("\n═══════════════════════════════════════════════════════\n");
    printf("Starting benchmark tests...\n");
    printf("═══════════════════════════════════════════════════════\n");

    double seq_read = run_sequential_read(nvme);
    double seq_write = run_sequential_write(nvme);
    double rand_read = run_random_read(nvme);

    printf("\n═══════════════════════════════════════════════════════\n");
    printf("BENCHMARK SUMMARY\n");
    printf("═══════════════════════════════════════════════════════\n");
    printf("Sequential Read:  %.2f GB/s\n", seq_read);
    printf("Sequential Write: %.2f GB/s\n", seq_write);
    printf("Random Read (4K): %.0f IOPS\n", rand_read);

    printf("\n═══════════════════════════════════════════════════════\n");
    printf("SPDK Native Performance Characteristics:\n");
    printf("• Polling mode (no interrupts)\n");
    printf("• Zero-copy DMA transfers\n");
    printf("• Direct PCIe access (no kernel)\n");
    printf("• Lock-free I/O submission\n");
    printf("═══════════════════════════════════════════════════════\n");

    // Cleanup
    spdk_nvme_ctrlr_free_io_qpair(nvme->qpair);
    spdk_nvme_detach(nvme->ctrlr);
    free(nvme);

    return 0;
}
