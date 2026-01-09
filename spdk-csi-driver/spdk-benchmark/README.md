# SPDK Native Benchmark

A Rust-based benchmark tool that uses SPDK APIs directly to measure true NVMe performance without kernel overhead.

## Features

- **Direct SPDK API access**: Bypasses kernel and ublk layer for true userspace performance
- **Polling mode**: Continuously polls for I/O completions (no interrupts)
- **Zero-copy DMA**: Direct memory access without kernel buffers
- **Lock-free I/O**: SPDK's lock-free submission path
- **Comprehensive tests**: Sequential read/write and random read tests

## Architecture

```
Traditional Stack (ublk):        SPDK Native (this tool):
┌──────────────┐                ┌──────────────┐
│ Application  │                │ Application  │
└──────┬───────┘                └──────┬───────┘
       │                               │
┌──────▼───────┐                ┌──────▼───────┐
│  Filesystem  │                │  SPDK APIs   │
└──────┬───────┘                └──────┬───────┘
       │                               │
┌──────▼───────┐                ┌──────▼───────┐
│ ublk driver  │                │     DMA      │
└──────┬───────┘                └──────┬───────┘
       │                               │
┌──────▼───────┐                ┌──────▼───────┐
│     SPDK     │                │   NVMe PCIe  │
└──────┬───────┘                └──────────────┘
       │
┌──────▼───────┐
│   NVMe PCIe  │
└──────────────┘
```

## Performance Expectations

Based on your TenaFe TC2201 NVMe disk specifications:

| Metric | ublk + SPDK | Kernel Driver | SPDK Native (Expected) |
|--------|-------------|---------------|------------------------|
| Sequential Read | 3.6 GB/s | 4.0 GB/s | **4.0-4.5 GB/s** |
| Sequential Write | 3.8 GB/s | 4.0 GB/s | **4.0-4.5 GB/s** |
| Random Read IOPS | 189k | 426k | **600k-800k** |
| Latency | ~170μs | ~75μs | **<10μs** |

SPDK Native advantages:
- **No kernel context switches**: Polling eliminates interrupt overhead
- **No system calls**: Direct function calls to SPDK libraries
- **No ublk translation**: Direct NVMe command submission
- **Lower latency**: Sub-10μs latency possible with polling

## Building

### Using Docker (Recommended)

```bash
cd /Users/ddalton/github/flint/spdk-csi-driver
docker build -f docker/Dockerfile.spdk-benchmark -t dilipdalton/spdk-benchmark:latest .
docker push dilipdalton/spdk-benchmark:latest
```

### Local Build (requires SPDK installed)

```bash
cd spdk-benchmark
cargo build --release
```

## Running

### Prerequisites

1. **Hugepages configured** (required for SPDK):
   ```bash
   echo 1024 > /proc/sys/vm/nr_hugepages
   mkdir -p /mnt/huge
   mount -t hugetlbfs nodev /mnt/huge
   ```

2. **NVMe device bound to vfio-pci** (required for userspace access):
   ```bash
   # Unbind from kernel driver
   echo "0000:02:00.0" > /sys/bus/pci/drivers/nvme/unbind

   # Bind to vfio-pci
   echo "1987 5016" > /sys/bus/pci/drivers/vfio-pci/new_id
   echo "0000:02:00.0" > /sys/bus/pci/drivers/vfio-pci/bind
   ```

3. **IOMMU configured** (for DMA):
   ```bash
   # Check IOMMU is enabled
   dmesg | grep -i iommu
   ```

### Run Benchmark

```bash
# Run directly
./target/release/spdk-benchmark

# Or via Docker
docker run --rm --privileged \
  -v /dev:/dev \
  -v /sys:/sys \
  -v /mnt/huge:/mnt/huge \
  dilipdalton/spdk-benchmark:latest
```

### Kubernetes Deployment

Create a pod manifest:

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: spdk-benchmark
  namespace: default
spec:
  hostNetwork: true
  hostPID: true
  containers:
  - name: benchmark
    image: dilipdalton/spdk-benchmark:latest
    securityContext:
      privileged: true
    volumeMounts:
    - name: dev
      mountPath: /dev
    - name: sys
      mountPath: /sys
    - name: hugepages
      mountPath: /mnt/huge
  volumes:
  - name: dev
    hostPath:
      path: /dev
  - name: sys
    hostPath:
      path: /sys
  - name: hugepages
    hostPath:
      path: /mnt/huge
  restartPolicy: Never
  nodeSelector:
    kubernetes.io/hostname: ubuntu  # Node with NVMe device
```

## Test Configuration

Current defaults (modify in `src/main.rs`):

- **Block size**: 4KB
- **Queue depth**: 128
- **Test size**: 1GB (262,144 blocks)
- **Tests**: Sequential read, sequential write, random read

## Understanding the Results

### Sequential Read/Write
- Measures maximum throughput for large transfers
- Should approach disk specifications (4-4.5 GB/s for your disk)
- Tests sustained performance (no SLC cache burst)

### Random Read (4K)
- Measures IOPS for small random operations
- Critical for database and VM workloads
- SPDK should show 2-3x improvement vs ublk
- Sub-10μs latency expected

### Comparison with Previous Tests

Your previous results:
```
ublk+SPDK:     189k IOPS (4K random)
Kernel driver: 426k IOPS (4K random)
```

Expected with SPDK Native:
```
SPDK Native:   600-800k IOPS (4K random)
```

The improvement comes from:
1. **No ublk overhead**: Eliminates block layer translation
2. **Polling mode**: No interrupt latency (~5-10μs saved)
3. **Zero-copy**: No buffer copies between kernel/userspace
4. **Lock-free**: SPDK's optimized submission path

## Troubleshooting

### Error: "Failed to initialize SPDK environment"
- Check hugepages: `cat /proc/meminfo | grep HugePages`
- Verify mount: `mount | grep huge`

### Error: "No NVMe controllers found"
- Verify device bound to vfio-pci: `lspci -k -s 02:00.0`
- Check IOMMU: `dmesg | grep -i iommu`

### Low performance
- Verify polling mode is active (no interrupts)
- Check CPU frequency scaling: `cat /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor`
- Disable power saving: `echo performance | tee /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor`

## Technical Details

### SPDK Libraries Used

- `libspdk_nvme`: NVMe driver implementation
- `libspdk_env_dpdk`: Environment abstraction (DPDK-based)
- `libspdk_log`: Logging utilities
- `libspdk_util`: Utility functions

### I/O Flow

1. **Initialization**: `spdk_env_init()` sets up hugepages and DPDK
2. **Discovery**: `spdk_nvme_probe()` finds NVMe controllers
3. **Attach**: Allocates I/O queue pairs
4. **Submit**: `spdk_nvme_ns_cmd_read/write()` submits commands
5. **Poll**: `spdk_nvme_qpair_process_completions()` polls for results
6. **Callback**: Completion callback updates IoContext
7. **Repeat**: Maintains queue depth with new submissions

### Memory Management

- **DMA buffers**: Allocated with `spdk_zmalloc()` (hugepage-backed)
- **Alignment**: 4KB alignment for optimal PCIe transfer
- **Cleanup**: `spdk_free()` releases buffers

## Next Steps

To further optimize:

1. **Tune queue depth**: Experiment with 256, 512 depths
2. **Multi-queue**: Use multiple queue pairs for parallel I/O
3. **CPU pinning**: Pin to specific cores for cache locality
4. **Batch submission**: Submit multiple I/Os before polling
5. **Adaptive polling**: Reduce CPU usage with hybrid polling
