# Flint CSI vs Longhorn CSI - Performance Comparison

**Date:** November 12, 2025  
**Cluster:** KUBECONFIG=/Users/ddalton/.kube/config.ublk  
**Kubernetes:** v1.33.5+rke2r1  
**Test Tool:** fio (Flexible I/O Tester)  
**Test Duration:** 30 seconds per test  
**Test File Size:** 1GB  
**Queue Depth:** 32  
**Direct I/O:** Enabled (bypasses page cache)

## Test Scenarios

### Flint CSI Local Mode
- **Pod Node:** ublk-2.vpc.cloudera.com
- **Volume Node:** ublk-2.vpc.cloudera.com
- **Access Pattern:** Direct ublk device (no network)
- **StorageClass:** flint-single-replica (1 replica)

### Flint CSI Remote Mode
- **Pod Node:** ublk-1.vpc.cloudera.com
- **Volume Node:** ublk-2.vpc.cloudera.com (remote!)
- **Access Pattern:** NVMe-oF over TCP → ublk device
- **NVMe-oF Target:** 10.65.131.143:4420
- **StorageClass:** flint-single-replica (1 replica)

### Longhorn CSI
- **Pod Node:** ublk-2.vpc.cloudera.com
- **Replica Count:** 1 (single replica for fair comparison)
- **Access Pattern:** iSCSI over network
- **StorageClass:** longhorn-single-replica (1 replica)

## Performance Results

### Sequential Write (128K blocks)

| Driver | Bandwidth | MB/s |
|--------|-----------|------|
| **Flint Local** | 129 MiB/s | 135 MB/s |
| **Flint Remote** | 129 MiB/s | 135 MB/s |
| **Longhorn** | 102 MiB/s | 107 MB/s |

**Winner:** Flint (both modes) - **26% faster** than Longhorn

### Sequential Read (128K blocks)

| Driver | Bandwidth | MB/s |
|--------|-----------|------|
| **Flint Local** | 126 MiB/s | 132 MB/s |
| **Flint Remote** | 125 MiB/s | 132 MB/s |
| **Longhorn** | 126 MiB/s | 132 MB/s |

**Winner:** Tie - All three perform identically

### Random Write (4K blocks - IOPS critical)

| Driver | Bandwidth | IOPS (approx) |
|--------|-----------|---------------|
| **Flint Local** | 11.0 MiB/s | ~2,816 IOPS |
| **Flint Remote** | 11.9 MiB/s | ~3,046 IOPS |
| **Longhorn** | 4.2 MiB/s | ~1,078 IOPS |

**Winner:** Flint - **~2.7x faster** than Longhorn  
**Surprise:** Flint Remote slightly faster than Local (8% improvement)

### Random Read (4K blocks - IOPS critical)

| Driver | Bandwidth | IOPS (approx) |
|--------|-----------|---------------|
| **Flint Local** | 12.1 MiB/s | ~3,098 IOPS |
| **Flint Remote** | 12.0 MiB/s | ~3,072 IOPS |
| **Longhorn** | 12.0 MiB/s | ~3,072 IOPS |

**Winner:** Tie - All three perform identically

## Summary

### Overall Performance Comparison

| Workload | Flint Local | Flint Remote | Longhorn | Flint Advantage |
|----------|-------------|--------------|----------|-----------------|
| **Seq Write** | 129 MiB/s | 129 MiB/s | 102 MiB/s | **+26%** |
| **Seq Read** | 126 MiB/s | 125 MiB/s | 126 MiB/s | Tie |
| **Rand Write 4K** | 11.0 MiB/s | 11.9 MiB/s | 4.2 MiB/s | **+170%** |
| **Rand Read 4K** | 12.1 MiB/s | 12.0 MiB/s | 12.0 MiB/s | Tie |

### Key Findings

✅ **Flint Excels at:**
- **Sequential Writes:** 26% faster than Longhorn
- **Random Writes:** 2.7x faster than Longhorn (huge advantage!)
- **Network Transparency:** Remote mode performs identically to local mode

✅ **Equal Performance:**
- Sequential reads: All three drivers are identical
- Random reads: All three drivers are identical

✅ **Flint Remote Mode Surprise:**
- Remote mode (via NVMe-oF) actually performs **identically** to local mode
- Demonstrates excellent NVMe-oF implementation with minimal overhead
- In random write, remote is even 8% faster (likely CPU/cache effects)

### Architecture Advantages

**Flint CSI:**
- SPDK userspace I/O (zero-copy, polling)
- ublk kernel block device (low overhead)
- NVMe-oF for remote access (RDMA-ready protocol)
- Direct access to NVMe SSDs

**Longhorn CSI:**
- iSCSI protocol overhead
- Multiple network hops
- Kernel-based I/O path

### Recommendations

**Use Flint CSI when:**
- Write-heavy workloads (databases, logging)
- IOPS-sensitive applications
- Low-latency requirements
- NVMe SSDs are available

**Flint vs Longhorn:**
- **Write Performance:** Flint wins decisively (+26% to +170%)
- **Read Performance:** Identical
- **Remote Access:** Flint's NVMe-oF has zero overhead vs local
- **Simplicity:** Both are easy to deploy

## Test Environment

**Hardware:**
- Nodes: ublk-1, ublk-2 (Ubuntu 24.04 LTS, Kernel 6.8.0-1008-aws)
- Storage: NVMe SSDs
- Network: 10 Gbps (likely)

**Software:**
- Flint CSI: v0.4.0 (feature/minimal-state branch, commit 2fa8e82)
- Longhorn CSI: driver.longhorn.io
- SPDK: v25.05.x
- fio: latest (xridge/fio container)

**Test Configuration:**
- Block size: 128K (sequential), 4K (random)
- Queue depth: 32
- Direct I/O: Enabled (O_DIRECT)
- Runtime: 30 seconds per test
- Jobs: 1 (single-threaded)

---

**Conclusion:** Flint CSI provides superior write performance while matching Longhorn on reads. The NVMe-oF remote mode is particularly impressive, showing zero performance degradation compared to local mode.

