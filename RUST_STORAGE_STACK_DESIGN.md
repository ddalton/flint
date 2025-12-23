# Rust Storage Stack Design Document

**Project Name**: Flint Rust Storage Stack
**Version**: 1.0
**Date**: 2025-12-22
**Status**: Design Phase

---

## Executive Summary

This document outlines the design for a high-performance, memory-safe storage stack written in Rust to replace SPDK-based components in the Flint CSI driver. The stack will provide NVMe, NVMe-oF (NVMe over Fabrics), ublk, logical volume management, and RDMA capabilities while addressing kernel compatibility issues present in SPDK.

### Key Objectives
- **Memory Safety**: Leverage Rust's type system to eliminate entire classes of bugs
- **Performance**: Match or exceed SPDK performance (within 1-5%)
- **Kernel Compatibility**: Solve ublk kernel 6.17+ compatibility issues
- **Maintainability**: Reduce complexity with ~60% less code than SPDK
- **Production Ready**: Battle-tested components with comprehensive testing

### Success Metrics
- I/O latency within 5% of SPDK baseline
- IOPS within 1% of SPDK on NVMe devices
- Zero memory safety violations in production
- Support for Linux kernel 6.6+
- Operational stability for 99.9% uptime

---

## Table of Contents

1. [Goals and Non-Goals](#goals-and-non-goals)
2. [Architecture Overview](#architecture-overview)
3. [Component Design](#component-design)
4. [Data Flow and Interfaces](#data-flow-and-interfaces)
5. [API Design](#api-design)
6. [Performance Considerations](#performance-considerations)
7. [Security and Safety](#security-and-safety)
8. [Testing Strategy](#testing-strategy)
9. [Implementation Roadmap](#implementation-roadmap)
10. [Risks and Mitigations](#risks-and-mitigations)
11. [Dependencies and Third-Party Libraries](#dependencies-and-third-party-libraries)
12. [Alternative Approaches Considered](#alternative-approaches-considered)

---

## Goals and Non-Goals

### Goals

**Primary Goals:**
1. **NVMe Local Access**: Direct userspace access to NVMe devices via PCIe/VFIO
2. **NVMe-oF Support**: Full NVMe over Fabrics initiator and target support
   - RDMA transport (RoCE v2)
   - TCP transport
3. **ublk Integration**: Expose storage as Linux block devices via kernel ublk interface
4. **Logical Volume Management**: Thin provisioning, snapshots, and volume management
5. **RDMA Networking**: High-performance RDMA for NVMe-oF and data replication
6. **Kernel Compatibility**: Support Linux 6.6+ including problematic 6.17+

**Secondary Goals:**
- Async/await throughout for efficient resource utilization
- Comprehensive metrics and observability
- Clean separation of concerns for testability
- Well-documented APIs for future extensions

### Non-Goals

**Explicitly Out of Scope:**
1. iSCSI support (use existing kernel implementation if needed)
2. Fibre Channel transport
3. Full SPDK feature parity (blobstore, vhost, etc.)
4. NVMe device emulation
5. Windows/macOS support (Linux-only for now)
6. Kernel driver implementation (userspace only)

**Future Considerations:**
- NVMe Zoned Namespaces (ZNS) support
- Computational storage offload
- GPU Direct Storage integration
- NVMe-MI (Management Interface)

---

## Architecture Overview

### High-Level Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                    Flint CSI Driver (Go)                        │
│                   gRPC API / Control Plane                       │
└────────────────────────────┬────────────────────────────────────┘
                             │ JSON-RPC / gRPC
                             ▼
┌─────────────────────────────────────────────────────────────────┐
│                 Rust Storage Stack (This Project)               │
│                                                                 │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │              Management & Control Layer                   │  │
│  │  • RPC Server (JSON-RPC)                                 │  │
│  │  • Volume Manager                                        │  │
│  │  • Target Manager (NVMe-oF)                              │  │
│  │  • Metrics & Telemetry                                   │  │
│  └──────────────────────────────────────────────────────────┘  │
│                             │                                   │
│  ┌──────────────┬───────────┼──────────────┬─────────────────┐ │
│  ▼              ▼           ▼              ▼                 ▼ │
│ ┌──────────┐ ┌──────┐  ┌─────────┐  ┌──────────┐  ┌─────────┐│
│ │  NVMe    │ │ NVMe │  │  ublk   │  │  LVM     │  │  RDMA   ││
│ │  Driver  │ │ -oF  │  │  Block  │  │ (Device  │  │ Verbs   ││
│ │ (vroom)  │ │Target│  │ Device  │  │ Mapper)  │  │(ibverbs)││
│ │          │ │/Init │  │ (rublk) │  │          │  │         ││
│ └──────────┘ └──────┘  └─────────┘  └──────────┘  └─────────┘│
│      │          │           │             │             │      │
└──────┼──────────┼───────────┼─────────────┼─────────────┼──────┘
       │          │           │             │             │
       ▼          ▼           ▼             ▼             ▼
┌─────────────────────────────────────────────────────────────────┐
│                    I/O Foundation Layer                         │
│                                                                 │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐         │
│  │  io_uring    │  │   Tokio      │  │   Memory     │         │
│  │   (async)    │  │   Runtime    │  │  Management  │         │
│  └──────────────┘  └──────────────┘  └──────────────┘         │
└─────────────────────────────────────────────────────────────────┘
       │                     │                    │
       ▼                     ▼                    ▼
┌─────────────────────────────────────────────────────────────────┐
│                        Linux Kernel                             │
│  • io_uring subsystem    • RDMA verbs (rdma-core)               │
│  • ublk driver           • Device mapper                        │
│  • NVMe driver           • Network stack                        │
└─────────────────────────────────────────────────────────────────┘
```

### Component Layering

The architecture follows a clean layered approach:

1. **I/O Foundation Layer**: Low-level async I/O and runtime
   - io_uring for kernel interaction
   - Tokio async runtime for task scheduling
   - Custom memory allocators for zero-copy paths

2. **Storage Primitives Layer**: Core storage components
   - NVMe driver for local device access
   - NVMe-oF for fabric access (initiator + target)
   - ublk for Linux block device exposure
   - Device mapper for volume management
   - RDMA for high-performance networking

3. **Management Layer**: High-level orchestration
   - Volume lifecycle management
   - NVMe-oF target configuration
   - Metrics collection and export
   - RPC interface for control plane

### Threading Model

The system uses Tokio's work-stealing scheduler with the following thread pools:

- **Control Plane Threads** (2-4): RPC handling, configuration
- **I/O Worker Threads** (# of CPU cores): Async I/O processing
- **Completion Poller Threads** (# of NVMe queues): Dedicated polling
- **RDMA Threads** (configurable): RDMA completion queue polling

**Thread Affinity**: Critical I/O paths pin threads to specific CPU cores to minimize context switches and cache misses.

---

## Component Design

### 1. NVMe Userspace Driver

**Technology**: [vroom](https://github.com/bootreer/vroom) - proven SPDK-level performance

**Responsibilities:**
- Direct PCIe device access via VFIO
- NVMe admin and I/O command submission
- Completion queue polling
- Namespace management
- Controller initialization and reset

**Key Design Decisions:**

1. **VFIO-based Access**: Use VFIO instead of UIO for better IOMMU support and security
2. **Polling vs. Interrupts**: Primarily polling for lowest latency; optional interrupt mode for idle periods
3. **Queue Pair Management**: One submission/completion queue pair per CPU core
4. **Memory Registration**: Pre-register large DMA buffers; use io_uring for efficient buffer management

**Interface:**
```rust
pub trait NvmeDevice {
    async fn read(&self, lba: u64, buffer: &mut [u8]) -> Result<usize>;
    async fn write(&self, lba: u64, buffer: &[u8]) -> Result<usize>;
    async fn flush(&self) -> Result<()>;
    async fn deallocate(&self, lba: u64, blocks: u32) -> Result<()>;
    fn get_namespace_info(&self, nsid: u32) -> NamespaceInfo;
}
```

**Extensions Needed:**
- Integrate latest vroom release
- Add admin command passthrough
- Implement namespace attachment/detachment
- Add telemetry and health monitoring

---

### 2. NVMe-oF Target and Initiator

**Status**: **Custom Implementation Required** (no mature Rust library exists)

This is the most complex new component. We'll implement both target and initiator with RDMA and TCP transports.

#### NVMe-oF Target Design

**Architecture:**
```
┌─────────────────────────────────────────────┐
│         NVMe-oF Target Subsystem            │
│                                             │
│  ┌────────────┐  ┌────────────┐            │
│  │ Discovery  │  │ Subsystem  │            │
│  │ Controller │  │ Controller │            │
│  └────────────┘  └────────────┘            │
│         │              │                    │
│  ┌──────┴──────────────┴────────┐          │
│  │     Transport Layer           │          │
│  │  ┌──────────┐  ┌───────────┐ │          │
│  │  │   RDMA   │  │    TCP    │ │          │
│  │  │ (ibverbs)│  │ (io_uring)│ │          │
│  │  └──────────┘  └───────────┘ │          │
│  └────────────────────────────────          │
│         │              │                    │
│  ┌──────┴──────────────┴────────┐          │
│  │   Backend Storage             │          │
│  │  (NVMe, LVM, ublk)            │          │
│  └───────────────────────────────┘          │
└─────────────────────────────────────────────┘
```

**Key Components:**

1. **Discovery Service**
   - Advertise available subsystems
   - Handle discovery log page requests
   - Support both in-band and out-of-band discovery

2. **Subsystem Controller**
   - Manage NVMe-oF subsystems and namespaces
   - Handle admin commands (identify, get log page, etc.)
   - Namespace sharing and access control

3. **I/O Command Handler**
   - Process read/write/flush commands
   - Queue management (16-bit queue depth support)
   - Scatter-Gather List (SGL) handling
   - Inline data transfer optimization

4. **Transport Abstraction**
```rust
#[async_trait]
pub trait NvmfTransport: Send + Sync {
    async fn accept_connection(&mut self) -> Result<Box<dyn NvmfConnection>>;
    async fn create_listener(&self, addr: SocketAddr) -> Result<()>;
    fn transport_type(&self) -> NvmfTransportType;
}

#[async_trait]
pub trait NvmfConnection: Send + Sync {
    async fn receive_capsule(&mut self) -> Result<NvmeCapsule>;
    async fn send_capsule(&mut self, capsule: NvmeCapsule) -> Result<()>;
    async fn send_data(&mut self, data: &[u8]) -> Result<()>;
    async fn receive_data(&mut self, buffer: &mut [u8]) -> Result<usize>;
}
```

#### NVMe-oF Initiator Design

**Responsibilities:**
- Connect to remote NVMe-oF targets
- Expose remote namespaces as local block devices
- Handle connection failures and reconnection
- Multipath support for HA

**Interface:**
```rust
pub struct NvmfInitiator {
    discovery_service: DiscoveryClient,
    subsystems: HashMap<String, NvmfSubsystem>,
}

impl NvmfInitiator {
    pub async fn discover(&mut self, addr: SocketAddr) -> Result<Vec<DiscoveryLogEntry>>;
    pub async fn connect(&mut self, subsystem_nqn: &str, transport: TransportType) -> Result<()>;
    pub async fn disconnect(&mut self, subsystem_nqn: &str) -> Result<()>;
    pub fn get_namespaces(&self, subsystem_nqn: &str) -> Vec<NamespaceInfo>;
}
```

**Transport Implementation:**

1. **RDMA Transport** (using ibverbs)
   - Connection-oriented reliable connection (RC) QPs
   - Inline data for small commands (<= 4KB)
   - RDMA READ for large reads
   - RDMA WRITE for large writes
   - Shared Receive Queue (SRQ) for scalability

2. **TCP Transport** (using io_uring)
   - Fixed-size header + variable payload
   - PDU (Protocol Data Unit) assembly/disassembly
   - Zero-copy where possible
   - Connection pooling

**NVMe-oF Protocol Handling:**

```rust
// Capsule = NVMe command + optional inline data
pub struct NvmeCapsule {
    pub command: NvmeCommand,
    pub inline_data: Option<Vec<u8>>,
}

pub struct NvmeCommand {
    pub opcode: u8,
    pub nsid: u32,
    pub cdw10: u32,
    pub cdw11: u32,
    // ... other fields
}
```

**Implementation Priority:**
- Phase 1: TCP transport (simpler, easier to debug)
- Phase 2: RDMA transport (higher performance)

---

### 3. ublk Block Device

**Technology**: [rublk](https://github.com/ublk-org/rublk) / [libublk](https://lib.rs/crates/libublk)

**Responsibilities:**
- Expose storage volumes as Linux block devices (/dev/ublkbN)
- Handle kernel I/O requests via io_uring
- Support for flush, discard, and write-zeroes operations
- Per-queue I/O handling for multi-queue block layer

**Key Features:**
- **Solves kernel 6.17+ compatibility**: rublk already handles UBLK_F_PER_IO_DAEMON correctly
- **Multiple backend support**: Can back ublk with NVMe, NVMe-oF, or LVM
- **High performance**: Zero-copy path for aligned I/O

**Integration:**
```rust
use libublk::UblkDev;

pub struct FlintUblkTarget {
    backend: Arc<dyn BlockBackend>,
    dev: UblkDev,
}

impl FlintUblkTarget {
    pub async fn create(
        id: u32,
        backend: Arc<dyn BlockBackend>,
        num_queues: u32,
        queue_depth: u32,
    ) -> Result<Self> {
        let dev = UblkDev::new(id, num_queues, queue_depth)?;
        dev.set_params(backend.get_capacity(), backend.get_block_size())?;
        dev.start().await?;
        Ok(Self { backend, dev })
    }
}
```

**Backend Abstraction:**
```rust
#[async_trait]
pub trait BlockBackend: Send + Sync {
    async fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<usize>;
    async fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<usize>;
    async fn flush(&self) -> Result<()>;
    async fn discard(&self, offset: u64, length: u64) -> Result<()>;
    fn get_capacity(&self) -> u64;
    fn get_block_size(&self) -> u32;
}
```

---

### 4. Logical Volume Management

**Technology**: [devicemapper](https://docs.rs/devicemapper) crate

**Responsibilities:**
- Thin provisioning with copy-on-write
- Volume snapshots
- Volume cloning
- Dynamic volume resizing
- Space management and monitoring

**Architecture:**
```
┌────────────────────────────────────────┐
│      Volume Manager                    │
│                                        │
│  ┌──────────────────────────────────┐ │
│  │   Thin Pool                      │ │
│  │  ┌───────────┐  ┌───────────┐   │ │
│  │  │ Metadata  │  │   Data    │   │ │
│  │  │  Device   │  │  Device   │   │ │
│  │  └───────────┘  └───────────┘   │ │
│  │         │             │          │ │
│  │  ┌──────┴─────┬───────┴──────┐  │ │
│  │  │            │              │  │ │
│  │  ▼            ▼              ▼  │ │
│  │ ┌──────┐  ┌──────┐      ┌──────┐│ │
│  │ │Vol 1 │  │Vol 2 │ ...  │Vol N ││ │
│  │ └──────┘  └──────┘      └──────┘│ │
│  └──────────────────────────────────┘ │
└────────────────────────────────────────┘
```

**Core Operations:**
```rust
pub struct VolumeManager {
    dm: DeviceMapper,
    pools: HashMap<String, ThinPool>,
}

impl VolumeManager {
    pub async fn create_pool(
        &mut self,
        name: &str,
        metadata_dev: &Path,
        data_dev: &Path,
        block_size: u32,
    ) -> Result<ThinPool>;

    pub async fn create_volume(
        &mut self,
        pool: &str,
        name: &str,
        size: u64,
    ) -> Result<ThinVolume>;

    pub async fn create_snapshot(
        &mut self,
        volume: &str,
        snapshot_name: &str,
    ) -> Result<ThinVolume>;

    pub async fn resize_volume(
        &mut self,
        volume: &str,
        new_size: u64,
    ) -> Result<()>;
}
```

**Metadata Management:**
- Use device-mapper's built-in metadata management
- Periodic metadata backups
- Thin pool monitoring for low space conditions
- Automatic snapshot merging when possible

---

### 5. RDMA Layer

**Technology**: [ibverbs](https://docs.rs/ibverbs) crate

**Why ibverbs over sideway?**
- **More mature**: 48K+ downloads vs 3.6K
- **Longer track record**: More battle-tested in production
- **Stable API**: Established interfaces that won't change
- **Better documentation**: More examples and community knowledge

(Note: We can evaluate sideway in the future for performance optimization)

**Responsibilities:**
- RDMA connection management (QP, CQ, PD, MR)
- Memory registration and management
- RDMA READ/WRITE/SEND operations
- Completion queue polling
- Connection establishment and teardown

**Design:**
```rust
pub struct RdmaContext {
    context: ibverbs::Context,
    pd: ibverbs::ProtectionDomain,
    completion_channel: CompletionChannel,
}

pub struct RdmaConnection {
    qp: QueuePair,
    cq_send: CompletionQueue,
    cq_recv: CompletionQueue,
    registered_buffers: Vec<MemoryRegion>,
}

impl RdmaConnection {
    pub async fn rdma_write(&self, local: &[u8], remote_addr: u64, rkey: u32) -> Result<()>;
    pub async fn rdma_read(&self, local: &mut [u8], remote_addr: u64, rkey: u32) -> Result<()>;
    pub async fn send(&self, data: &[u8]) -> Result<()>;
    pub async fn recv(&self, buffer: &mut [u8]) -> Result<usize>;
}
```

**Memory Management:**
- Pre-allocate large buffer pools
- Register buffers once, reuse multiple times
- Support for different memory region types (local, remote)
- Integration with io_uring for efficient memory operations

**NVMe-oF RDMA Integration:**
```rust
pub struct NvmfRdmaTransport {
    listen_id: RdmaCmId,
    connections: Vec<RdmaConnection>,
}

impl NvmfTransport for NvmfRdmaTransport {
    async fn accept_connection(&mut self) -> Result<Box<dyn NvmfConnection>> {
        // Accept RDMA CM connection request
        // Setup QP, register buffers
        // Return NvmfRdmaConnection
    }
}
```

---

## Data Flow and Interfaces

### Typical I/O Paths

#### Path 1: Local NVMe Read via ublk

```
User Application
    │ read(/dev/ublkb0, ...)
    ▼
Linux Kernel (block layer)
    │ bio request
    ▼
ublk Driver
    │ io_uring FETCH_REQ
    ▼
rublk (our code)
    │ async read request
    ▼
Backend (NVMe)
    │ NVMe read command
    ▼
vroom Driver
    │ submission to NVMe SQ
    ▼
NVMe Device
    │ DMA to memory
    ▼
vroom Driver
    │ poll CQ, complete future
    ▼
rublk
    │ io_uring COMMIT_AND_FETCH
    ▼
ublk Driver
    │ complete bio
    ▼
Linux Kernel
    │ return to user
    ▼
User Application
```

**Latency Budget**: ~10-20μs (NVMe) + ~5-10μs (software overhead) = **15-30μs total**

#### Path 2: NVMe-oF RDMA Write

```
Initiator Application
    │ write request
    ▼
NVMe-oF Initiator
    │ create NVMe write command
    ▼
RDMA Transport
    │ RDMA WRITE with inline command
    ▼
Network (RoCE)
    │
    ▼
Target RDMA Transport
    │ CQ event
    ▼
NVMe-oF Target
    │ parse write command
    ▼
Backend (LVM thin volume)
    │ write to device-mapper
    ▼
Device Mapper
    │ allocate block if needed
    ▼
Underlying NVMe
    │ write command
    ▼
NVMe Device
    │ complete
    ▼
NVMe-oF Target
    │ create completion
    ▼
RDMA Transport
    │ SEND completion
    ▼
Network
    │
    ▼
Initiator RDMA Transport
    │ CQ event
    ▼
NVMe-oF Initiator
    │ complete request
    ▼
Initiator Application
```

**Latency Budget**: Network RTT (~5-50μs) + Target processing (~10-20μs) + Backend I/O (~10-30μs) = **25-100μs total**

---

## API Design

### Public RPC API (JSON-RPC over Unix socket or TCP)

The control plane exposes a JSON-RPC API for CSI driver integration:

```rust
// Volume Management
pub trait VolumeApi {
    async fn volume_create(name: String, size_bytes: u64) -> Result<VolumeInfo>;
    async fn volume_delete(name: String) -> Result<()>;
    async fn volume_resize(name: String, new_size_bytes: u64) -> Result<()>;
    async fn volume_snapshot(name: String, snapshot_name: String) -> Result<SnapshotInfo>;
    async fn volume_clone(source: String, target: String) -> Result<VolumeInfo>;
    async fn volume_list() -> Result<Vec<VolumeInfo>>;
    async fn volume_stats(name: String) -> Result<VolumeStats>;
}

// Block Device Exposure
pub trait BlockDeviceApi {
    async fn block_device_create(volume: String, device_id: u32) -> Result<String>; // returns /dev/ublkbN
    async fn block_device_delete(device_id: u32) -> Result<()>;
    async fn block_device_list() -> Result<Vec<BlockDeviceInfo>>;
}

// NVMe-oF Target Management
pub trait NvmfTargetApi {
    async fn subsystem_create(nqn: String) -> Result<()>;
    async fn subsystem_delete(nqn: String) -> Result<()>;
    async fn subsystem_add_namespace(nqn: String, nsid: u32, volume: String) -> Result<()>;
    async fn subsystem_remove_namespace(nqn: String, nsid: u32) -> Result<()>;
    async fn listener_add(nqn: String, transport: TransportType, addr: SocketAddr) -> Result<()>;
    async fn listener_remove(nqn: String, transport: TransportType, addr: SocketAddr) -> Result<()>;
}

// NVMe-oF Initiator Management
pub trait NvmfInitiatorApi {
    async fn discover(addr: SocketAddr, transport: TransportType) -> Result<Vec<DiscoveryLogEntry>>;
    async fn connect(subsystem_nqn: String, transport: TransportType, addr: SocketAddr) -> Result<()>;
    async fn disconnect(subsystem_nqn: String) -> Result<()>;
    async fn list_subsystems() -> Result<Vec<SubsystemInfo>>;
}
```

### Internal Component APIs

Components communicate via async trait interfaces for testability:

```rust
// Storage backend abstraction
#[async_trait]
pub trait StorageBackend: Send + Sync {
    async fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>>;
    async fn write(&self, offset: u64, data: &[u8]) -> Result<()>;
    async fn flush(&self) -> Result<()>;
    async fn deallocate(&self, offset: u64, len: u64) -> Result<()>;
    fn capacity(&self) -> u64;
    fn block_size(&self) -> u32;
}

// Implementations: NvmeBackend, NvmfBackend, LvmBackend, etc.
```

---

## Performance Considerations

### Optimization Strategies

1. **Zero-Copy I/O Paths**
   - Use io_uring's buffer registration
   - RDMA memory regions mapped directly to NVMe buffers
   - Avoid intermediate copies where possible

2. **CPU Affinity and NUMA Awareness**
   - Pin polling threads to specific cores
   - Allocate memory on local NUMA node
   - Queue pair per CPU core

3. **Batching**
   - Batch NVMe commands where possible
   - Coalesce RDMA operations
   - Aggregate completion processing

4. **Polling vs. Interrupts**
   - Hybrid mode: poll under load, interrupt when idle
   - Adaptive polling based on queue depth
   - Event-driven mode for low-throughput scenarios

5. **Memory Pooling**
   - Pre-allocate command structures
   - Buffer pools for different size classes
   - Lock-free ring buffers for inter-thread communication

### Performance Targets

| Metric | Target | Measurement Method |
|--------|--------|-------------------|
| **Single Queue Latency** | < 20μs (p99) | fio with iodepth=1 |
| **IOPS (4K Random Read)** | > 1M IOPS | fio with iodepth=32 |
| **Throughput (128K Sequential)** | > 6 GB/s | fio sequential |
| **NVMe-oF RDMA Latency** | < 50μs (p99) | Custom benchmark |
| **NVMe-oF TCP Latency** | < 200μs (p99) | Custom benchmark |
| **Memory Overhead** | < 500MB base | /proc/meminfo |
| **CPU Utilization** | < 5% idle, < 80% saturated | perf stat |

### Benchmarking Plan

- **Baseline**: Compare against SPDK on identical hardware
- **Tools**: fio, perf, bpftrace, custom NVMe-oF benchmarks
- **Scenarios**:
  - Local NVMe (direct, via ublk)
  - NVMe-oF RDMA (various block sizes)
  - NVMe-oF TCP (various block sizes)
  - Thin volume overhead
  - Snapshot impact

---

## Security and Safety

### Memory Safety

Rust's ownership system eliminates:
- Use-after-free bugs
- Double-free errors
- Buffer overflows
- Data races

**Unsafe Code Audit**:
- Minimize unsafe blocks (< 5% of codebase)
- Document all unsafe usage with safety invariants
- Use `cargo-geiger` to track unsafe usage
- Mandatory review for all unsafe code

### Privilege Separation

- **VFIO requires root initially**, then drop privileges
- Use Linux capabilities instead of full root where possible
- Separate process for privileged operations
- Seccomp filters to restrict syscalls

### Input Validation

- Validate all NVMe-oF commands from network
- Bounds checking on all user inputs
- Sanitize subsystem NQNs and namespace IDs
- Rate limiting on control plane APIs

### Resource Limits

- Configurable limits on:
  - Maximum number of volumes
  - Maximum volume size
  - Maximum number of NVMe-oF connections
  - Memory allocation caps
  - Queue depths

---

## Testing Strategy

### Unit Tests

- Every public function has unit tests
- Mock implementations of all traits
- Property-based testing with `proptest` for invariants
- Target: > 80% code coverage

### Integration Tests

- Test component interactions
- NVMe-oF round-trip tests (loopback)
- ublk with actual kernel driver
- Device-mapper thin provisioning scenarios

### Performance Tests

- Automated benchmarking in CI
- Regression detection (fail if > 5% slower than baseline)
- Latency distribution analysis (p50, p95, p99, p999)

### Stress Tests

- Long-running tests (24+ hours)
- Resource exhaustion scenarios
- Connection failure and recovery
- Concurrent operations

### Compatibility Tests

- Matrix of kernel versions (6.6, 6.12, 6.17, 6.18)
- Different NVMe devices (Intel, Samsung, Micron)
- Various RDMA NICs (Mellanox, Broadcom)

### Safety Tests

- Miri for undefined behavior detection
- AddressSanitizer builds
- ThreadSanitizer for race conditions
- Fuzzing with cargo-fuzz

---

## Implementation Roadmap

### Phase 0: Foundation (Weeks 1-2)

**Goals**: Set up project structure and CI/CD

- [ ] Create Cargo workspace
- [ ] Set up GitHub Actions CI
- [ ] Configure rustfmt, clippy, deny.toml
- [ ] Create integration test framework
- [ ] Design logging and tracing strategy

**Deliverables**: Buildable project skeleton

---

### Phase 1: Core Components (Weeks 3-8)

**Goals**: Integrate existing libraries, basic functionality

#### Week 3-4: NVMe Driver Integration
- [ ] Integrate vroom for local NVMe access
- [ ] Create NVMe backend trait implementation
- [ ] Basic read/write/flush operations
- [ ] Unit tests and benchmarks

#### Week 5-6: ublk Integration
- [ ] Integrate libublk/rublk
- [ ] Implement BlockBackend trait for NVMe
- [ ] Create and destroy ublk devices
- [ ] Verify kernel 6.17+ compatibility
- [ ] Integration tests with actual kernel

#### Week 7-8: Volume Management
- [ ] Integrate devicemapper crate
- [ ] Thin pool creation and management
- [ ] Volume create/delete/resize
- [ ] Snapshot support
- [ ] Integration with ublk

**Deliverables**:
- Working ublk device backed by local NVMe
- Thin-provisioned volumes exposed as block devices
- Basic RPC API for volume management

---

### Phase 2: NVMe-oF Implementation (Weeks 9-16)

**Goals**: Custom NVMe-oF target and initiator

#### Week 9-10: Protocol Foundations
- [ ] NVMe capsule parsing/serialization
- [ ] NVMe command structures
- [ ] Admin command handling
- [ ] Discovery service protocol

#### Week 11-12: TCP Transport
- [ ] io_uring-based TCP transport
- [ ] PDU assembly/disassembly
- [ ] Connection management
- [ ] Basic I/O command handling
- [ ] Target implementation
- [ ] Initiator implementation

#### Week 13-14: RDMA Transport (ibverbs)
- [ ] RDMA connection setup (QP, CQ, MR)
- [ ] Inline data handling
- [ ] RDMA READ/WRITE for large transfers
- [ ] Completion queue polling
- [ ] Integration with NVMe-oF

#### Week 15-16: Integration and Testing
- [ ] End-to-end NVMe-oF tests (loopback)
- [ ] Multi-queue support
- [ ] Namespace management
- [ ] Benchmarking vs SPDK
- [ ] Bug fixes and optimization

**Deliverables**:
- Working NVMe-oF target (TCP + RDMA)
- Working NVMe-oF initiator
- Volumes accessible over network
- Performance within 10% of SPDK

---

### Phase 3: Production Hardening (Weeks 17-20)

**Goals**: Stability, observability, documentation

#### Week 17: Observability
- [ ] Prometheus metrics export
- [ ] Structured logging (tracing)
- [ ] Performance counters
- [ ] Health check endpoints

#### Week 18: Error Handling
- [ ] Graceful degradation
- [ ] Connection retry logic
- [ ] I/O timeout handling
- [ ] Resource cleanup on errors

#### Week 19: Testing
- [ ] Stress tests
- [ ] Chaos engineering scenarios
- [ ] Kernel compatibility matrix
- [ ] Memory leak detection

#### Week 20: Documentation
- [ ] API documentation
- [ ] Architecture guide
- [ ] Deployment guide
- [ ] Troubleshooting runbook

**Deliverables**:
- Production-ready system
- Complete documentation
- Comprehensive test suite

---

### Phase 4: CSI Driver Integration (Weeks 21-24)

**Goals**: Replace SPDK in Flint CSI driver

#### Week 21-22: API Alignment
- [ ] Match existing SPDK RPC API
- [ ] Create compatibility layer if needed
- [ ] Integration with existing CSI code

#### Week 23: Deployment
- [ ] Docker image creation
- [ ] Helm chart updates
- [ ] Deployment testing

#### Week 24: Validation
- [ ] Kubernetes integration tests
- [ ] Real workload testing
- [ ] Performance comparison
- [ ] Bug fixes

**Deliverables**:
- Flint CSI driver using Rust stack
- Kubernetes test results
- Migration guide

---

## Risks and Mitigations

### Technical Risks

| Risk | Impact | Probability | Mitigation |
|------|--------|-------------|------------|
| **NVMe-oF implementation complexity** | High | Medium | Start with TCP (simpler), phased approach, extensive testing |
| **Performance not meeting targets** | High | Low | Early benchmarking, profiling, optimization sprints |
| **Kernel compatibility issues** | Medium | Medium | Test matrix, close tracking of kernel changes |
| **RDMA hardware variability** | Medium | Medium | Test on multiple NIC vendors, fallback to TCP |
| **Memory safety violations in unsafe code** | High | Low | Minimal unsafe, thorough review, sanitizers |

### Schedule Risks

| Risk | Impact | Probability | Mitigation |
|------|--------|-------------|------------|
| **NVMe-oF taking longer than estimated** | High | Medium | Allocate buffer time, MVP scope reduction option |
| **Dependency library bugs** | Medium | Low | Contribute fixes upstream, fork if necessary |
| **Scope creep** | Medium | Medium | Strict adherence to non-goals, change control process |

### Operational Risks

| Risk | Impact | Probability | Mitigation |
|------|--------|-------------|------------|
| **Production issues in CSI driver** | High | Low | Phased rollout, canary deployments, rollback plan |
| **Breaking API changes** | Medium | Low | Semantic versioning, deprecation policy |
| **Maintenance burden** | Low | Medium | Good documentation, clean architecture |

---

## Dependencies and Third-Party Libraries

### Core Dependencies

| Crate | Version | Purpose | License | Notes |
|-------|---------|---------|---------|-------|
| **tokio** | 1.x | Async runtime | MIT | Industry standard |
| **tokio-rs/io-uring** | Latest | io_uring interface | MIT/Apache-2.0 | Official Tokio project |
| **vroom** | Latest | NVMe driver | Custom | Fork and vendor if needed |
| **libublk** | Latest | ublk library | MIT/Apache-2.0 | Actively maintained |
| **rublk** | Latest | ublk reference | MIT/Apache-2.0 | Reference implementation |
| **devicemapper** | Latest | Device mapper | Apache-2.0 | Stratis project |
| **ibverbs** | 0.9+ | RDMA verbs | MIT/Apache-2.0 | jonhoo/rust-ibverbs |

### Additional Dependencies

- **serde/serde_json**: RPC serialization
- **tracing/tracing-subscriber**: Logging and diagnostics
- **prometheus**: Metrics export
- **clap**: CLI argument parsing
- **anyhow/thiserror**: Error handling
- **bytes**: Efficient byte buffers
- **crossbeam**: Concurrent data structures

### System Requirements

- **Linux Kernel**: 6.6+
- **rdma-core**: For RDMA support
- **libibverbs**: RDMA verbs library
- **liburing**: Optional, for testing

---

## Alternative Approaches Considered

### 1. Use SPDK with Rust Bindings

**Pros**: Proven performance, full feature set
**Cons**:
- Existing kernel compatibility issues
- C codebase complexity
- Difficult to maintain bindings
- Doesn't solve ublk 6.17+ problem

**Decision**: Rejected - doesn't meet goals

### 2. Pure Kernel-Based Solution

Use nvmet (kernel NVMe-oF target) and dm-thin

**Pros**: No userspace complexity, kernel maintained
**Cons**:
- Lower performance than userspace
- Less flexibility
- Cannot use VFIO/direct NVMe access

**Decision**: Rejected - performance requirements

### 3. Zig Implementation

**Pros**: Simple language, great C interop
**Cons**:
- Immature ecosystem
- Fewer libraries available
- Smaller community

**Decision**: Rejected - Rust has better libraries

### 4. Mix of SPDK + Custom Components

Keep SPDK for NVMe, rewrite only ublk/NVMe-oF in Rust

**Pros**: Lower initial effort
**Cons**:
- FFI complexity
- Still have SPDK dependency
- Mixed memory safety guarantees

**Decision**: Rejected - prefer clean slate

---

## Appendices

### A. Glossary

- **CSI**: Container Storage Interface
- **DMA**: Direct Memory Access
- **IOPS**: Input/Output Operations Per Second
- **LVM**: Logical Volume Manager
- **NQN**: NVMe Qualified Name
- **NVMe**: Non-Volatile Memory Express
- **NVMe-oF**: NVMe over Fabrics
- **RDMA**: Remote Direct Memory Access
- **RoCE**: RDMA over Converged Ethernet
- **SGL**: Scatter-Gather List
- **VFIO**: Virtual Function I/O
- **ublk**: Userspace block device

### B. References

- [NVMe 2.2 Specification](http://nvmexpress.org/)
- [NVMe-oF 1.1 Specification](http://nvmexpress.org/)
- [Linux ublk Documentation](https://docs.kernel.org/block/ublk.html)
- [SPDK Documentation](https://spdk.io/doc/)
- [Rust Async Book](https://rust-lang.github.io/async-book/)
- [io_uring Documentation](https://kernel.dk/io_uring.pdf)

### C. Performance Baseline (Current SPDK Setup)

To be measured and documented:
- Local NVMe IOPS and latency
- NVMe-oF RDMA performance
- Thin volume overhead
- Current issues and bottlenecks

---

## Document Revision History

| Version | Date | Author | Changes |
|---------|------|--------|---------|
| 1.0 | 2025-12-22 | Claude | Initial design document |

---

## Approval

This design document should be reviewed and approved by:

- [ ] Technical Lead
- [ ] Architecture Review Board
- [ ] Security Team
- [ ] DevOps/SRE Team

**Next Steps After Approval**:
1. Create GitHub repository
2. Set up project structure
3. Begin Phase 0 implementation
4. Weekly progress reviews
