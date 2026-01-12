#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <time.h>

#include "spdk/stdinc.h"
#include "spdk/bdev.h"
#include "spdk/env.h"
#include "spdk/event.h"
#include "spdk/log.h"
#include "spdk/string.h"
#include "spdk/thread.h"

#define BLOCK_SIZE 131072  // 128KB for maximum throughput
#define NUM_BLOCKS 8192    // 1GB (8192 * 128KB = 1GB)
#define QUEUE_DEPTH 128
#define MALLOC_BDEV_SIZE_MB 1024  // 1GB malloc bdev

struct test_context {
    struct spdk_bdev *bdev;
    struct spdk_bdev_desc *bdev_desc;
    struct spdk_io_channel *io_channel;
    uint64_t offset_in_blocks;
    uint64_t io_submitted;
    uint64_t io_completed;
    uint32_t current_queue_depth;
    int is_draining;
    int status;
    int is_read;
    void *buffer;
};

struct io_task {
    struct test_context *ctx;
    void *buffer;
};

static void submit_io(struct io_task *task);

static void io_complete(struct spdk_bdev_io *bdev_io, bool success, void *cb_arg)
{
    struct io_task *task = cb_arg;
    struct test_context *ctx = task->ctx;

    if (!success) {
        printf("I/O error\n");
        ctx->status = 1;
    }

    ctx->current_queue_depth--;
    ctx->io_completed++;

    spdk_bdev_free_io(bdev_io);

    if (ctx->is_draining) {
        free(task);
    } else {
        submit_io(task);
    }
}

static void submit_io(struct io_task *task)
{
    struct test_context *ctx = task->ctx;
    uint64_t offset_bytes;
    int rc;

    if (ctx->is_draining) {
        return;
    }

    offset_bytes = ctx->offset_in_blocks * BLOCK_SIZE;
    
    ctx->offset_in_blocks++;
    if (ctx->offset_in_blocks >= NUM_BLOCKS) {
        ctx->offset_in_blocks = 0;
    }

    if (ctx->is_read) {
        rc = spdk_bdev_read(ctx->bdev_desc, ctx->io_channel, task->buffer,
                           offset_bytes, BLOCK_SIZE, io_complete, task);
    } else {
        rc = spdk_bdev_write(ctx->bdev_desc, ctx->io_channel, task->buffer,
                            offset_bytes, BLOCK_SIZE, io_complete, task);
    }

    if (rc != 0) {
        printf("Failed to submit %s: %d\n", ctx->is_read ? "read" : "write", rc);
        ctx->status = 1;
        free(task);
        return;
    }

    ctx->current_queue_depth++;
    ctx->io_submitted++;

    if (ctx->io_submitted >= NUM_BLOCKS) {
        ctx->is_draining = 1;
    }
}

static double run_test(struct test_context *ctx)
{
    struct io_task *task;
    struct timespec start, end;
    double elapsed, throughput, iops;
    int i;

    ctx->offset_in_blocks = 0;
    ctx->io_submitted = 0;
    ctx->io_completed = 0;
    ctx->current_queue_depth = 0;
    ctx->is_draining = 0;
    ctx->status = 0;

    printf("Starting %s test (1 GB, %dKB blocks)...\n",
           ctx->is_read ? "sequential read" : "sequential write", BLOCK_SIZE / 1024);
    fflush(stdout);
    
    clock_gettime(CLOCK_MONOTONIC, &start);

    // Submit initial queue depth
    for (i = 0; i < QUEUE_DEPTH; i++) {
        task = malloc(sizeof(struct io_task));
        if (task == NULL) {
            printf("Failed to allocate task\n");
            return -1;
        }

        task->buffer = (char *)ctx->buffer + (i * BLOCK_SIZE);
        task->ctx = ctx;

        submit_io(task);

        if (ctx->status != 0) {
            printf("Initial submission failed\n");
            return -1;
        }
    }

    // Poll for completions
    uint64_t last_progress = 0;
    while (ctx->io_completed < NUM_BLOCKS && ctx->status == 0) {
        spdk_thread_poll(spdk_get_thread(), 0, 0);

        if (ctx->io_completed - last_progress >= NUM_BLOCKS / 5) {
            printf("  Progress: %lu%% (%lu/%u blocks)\n",
                   (ctx->io_completed * 100) / NUM_BLOCKS, ctx->io_completed, (uint32_t)NUM_BLOCKS);
            fflush(stdout);
            last_progress = ctx->io_completed;
        }
    }

    clock_gettime(CLOCK_MONOTONIC, &end);

    if (ctx->status != 0) {
        printf("Test failed with error\n");
        return -1;
    }

    elapsed = (end.tv_sec - start.tv_sec) + (end.tv_nsec - start.tv_nsec) / 1e9;
    throughput = ((double)NUM_BLOCKS * BLOCK_SIZE / elapsed) / (1024.0 * 1024.0 * 1024.0);
    iops = NUM_BLOCKS / elapsed;

    printf("✓ Completed: %lu blocks in %.2fs\n", ctx->io_completed, elapsed);
    printf("  Throughput: %.2f GB/s\n", throughput);
    printf("  IOPS: %.0f\n", iops);
    fflush(stdout);

    return throughput;
}

static void test_main(void *arg1)
{
    struct test_context ctx = {0};
    double read_throughput, write_throughput;
    int rc;

    printf("\n");
    printf("═══════════════════════════════════════════════════════\n");
    printf("SPDK Malloc Bdev Benchmark (Memory Disk)\n");
    printf("═══════════════════════════════════════════════════════\n");
    printf("\n");

    // Get the malloc bdev
    ctx.bdev = spdk_bdev_get_by_name("Malloc0");
    if (ctx.bdev == NULL) {
        printf("Failed to find malloc bdev 'Malloc0'\n");
        spdk_app_stop(-1);
        return;
    }

    printf("✓ Found malloc bdev: Malloc0\n");
    printf("  Capacity: %lu MB\n", spdk_bdev_get_num_blocks(ctx.bdev) * spdk_bdev_get_block_size(ctx.bdev) / (1024 * 1024));
    printf("  Block size: %u bytes\n", spdk_bdev_get_block_size(ctx.bdev));
    printf("  Queue depth: %d\n", QUEUE_DEPTH);
    printf("\n");
    fflush(stdout);

    // Open the bdev
    rc = spdk_bdev_open_ext(spdk_bdev_get_name(ctx.bdev), true, NULL, NULL, &ctx.bdev_desc);
    if (rc != 0) {
        printf("Failed to open bdev: %d\n", rc);
        spdk_app_stop(-1);
        return;
    }

    // Get I/O channel
    ctx.io_channel = spdk_bdev_get_io_channel(ctx.bdev_desc);
    if (ctx.io_channel == NULL) {
        printf("Failed to get I/O channel\n");
        spdk_bdev_close(ctx.bdev_desc);
        spdk_app_stop(-1);
        return;
    }

    // Allocate buffer
    ctx.buffer = spdk_zmalloc(QUEUE_DEPTH * BLOCK_SIZE, BLOCK_SIZE, NULL,
                              SPDK_ENV_SOCKET_ID_ANY, SPDK_MALLOC_DMA);
    if (ctx.buffer == NULL) {
        printf("Failed to allocate I/O buffer\n");
        spdk_put_io_channel(ctx.io_channel);
        spdk_bdev_close(ctx.bdev_desc);
        spdk_app_stop(-1);
        return;
    }

    // Run tests
    printf("═══════════════════════════════════════════════════════\n");
    printf("SEQUENTIAL WRITE TEST (Direct Memory Access)\n");
    printf("═══════════════════════════════════════════════════════\n");
    fflush(stdout);
    ctx.is_read = 0;
    write_throughput = run_test(&ctx);
    printf("\n");
    fflush(stdout);

    printf("═══════════════════════════════════════════════════════\n");
    printf("SEQUENTIAL READ TEST (Direct Memory Access)\n");
    printf("═══════════════════════════════════════════════════════\n");
    fflush(stdout);
    ctx.is_read = 1;
    read_throughput = run_test(&ctx);
    printf("\n");
    fflush(stdout);

    // Summary
    printf("═══════════════════════════════════════════════════════\n");
    printf("BENCHMARK SUMMARY (Pure Memory Performance)\n");
    printf("═══════════════════════════════════════════════════════\n");
    printf("Sequential Write: %.2f GB/s\n", write_throughput);
    printf("Sequential Read:  %.2f GB/s\n", read_throughput);
    printf("═══════════════════════════════════════════════════════\n");
    printf("\n");
    fflush(stdout);

    // Cleanup
    spdk_free(ctx.buffer);
    spdk_put_io_channel(ctx.io_channel);
    spdk_bdev_close(ctx.bdev_desc);

    spdk_app_stop(0);
}

int main(int argc, char **argv)
{
    struct spdk_app_opts opts = {};
    int rc;

    spdk_app_opts_init(&opts, sizeof(opts));
    opts.name = "malloc_benchmark";
    opts.reactor_mask = "0x1";
    
    // Configure malloc bdev
    opts.json_config_file = NULL;
    
    // Add malloc bdev configuration
    char config[512];
    snprintf(config, sizeof(config),
             "{"
             "  \"subsystems\": ["
             "    {"
             "      \"subsystem\": \"bdev\","
             "      \"config\": ["
             "        {"
             "          \"method\": \"bdev_malloc_create\","
             "          \"params\": {"
             "            \"name\": \"Malloc0\","
             "            \"num_blocks\": %lu,"
             "            \"block_size\": 512"
             "          }"
             "        }"
             "      ]"
             "    }"
             "  ]"
             "}",
             (uint64_t)MALLOC_BDEV_SIZE_MB * 1024 * 1024 / 512);  // Convert MB to blocks

    printf("Creating %d MB malloc bdev...\n", MALLOC_BDEV_SIZE_MB);
    
    rc = spdk_app_start(&opts, test_main, NULL);
    
    spdk_app_fini();
    return rc;
}
