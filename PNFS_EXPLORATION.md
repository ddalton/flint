# pNFS (Parallel NFS) Support Exploration

## Executive Summary

This document explores adding pNFS (parallel NFS) support to the Flint NFS server while keeping the existing standalone NFSv4.2 server implementation unchanged. pNFS separates metadata operations from data operations, enabling true parallel I/O across multiple storage servers.

**Status**: Exploration phase  
**Goal**: Modular pNFS implementation with configuration-driven deployment  
**Strategy**: New modules for MDS and DS without modifying existing NFS server

---

## 1. Current Architecture Overview

### Existing Implementation

The current NFS server (`spdk-csi-driver/src/nfs/`) is a fully functional NFSv4.2 server:

**Components:**
- **Transport Layer**: TCP-based async I/O (Tokio)
- **RPC Layer**: Sun RPC message encoding/decoding
- **Protocol Layer**: NFSv4.2 COMPOUND operations
- **State Management**: Sessions, locks, leases (DashMap-based)
- **Filesystem Layer**: Direct filesystem access via FileHandleManager

**Deployment Model:**
- Single NFS server pod per volume
- Exports SPDK-backed volumes over NFS
- Provides ReadWriteMany (RWX) capability
- Kubernetes Service provides stable endpoint

**Key Files:**
```
src/nfs/
├── mod.rs              # Module structure
├── server_v4.rs        # TCP server & COMPOUND dispatcher
├── rpc.rs              # RPC message handling
├── xdr.rs              # XDR encoding/decoding
├── v4/
│   ├── compound.rs     # COMPOUND request/response
│   ├── dispatcher.rs   # Operation dispatcher
│   ├── filehandle.rs   # File handle management
│   ├── protocol.rs     # NFSv4 constants & types
│   ├── state/          # State management
│   └── operations/     # NFS operations (READ, WRITE, etc.)
└── rwx_nfs.rs          # Kubernetes integration
```

---

## 2. pNFS Architecture (NFSv4.1+)

### Overview

pNFS (RFC 5661, part of NFSv4.1) separates **control plane** (metadata) from **data plane** (I/O):

```
                    ┌─────────────────────┐
                    │   NFS Clients       │
                    │   (Linux kernel)    │
                    └──────────┬──────────┘
                               │
                    ┌──────────┴──────────┐
                    │  Metadata Operations │  Data Operations
                    │  (OPEN, GETATTR...)  │  (READ, WRITE...)
                    └──────────┬──────────┘
                               │
           ┌───────────────────┴───────────────────┐
           │                                       │
           ▼                                       ▼
    ┌──────────────┐                    ┌──────────────┐
    │     MDS      │                    │   DS-1...n   │
    │   Metadata   │◄───control────────►│  Data I/O    │
    │    Server    │                    │   Servers    │
    └──────────────┘                    └──────────────┘
           │                                    │
           │ Layouts & Device Info              │ Direct I/O
           │                                    │
           ▼                                    ▼
    ┌──────────────┐                    ┌──────────────┐
    │     State    │                    │  SPDK Bdevs  │
    │   Database   │                    │   (NVMe)     │
    │  (etcd/K8s)  │                    └──────────────┘
    └──────────────┘
```

### Two Server Roles

**1. Metadata Server (MDS)**
- Handles all metadata operations:
  - `OPEN`, `CLOSE`, `CREATE`, `REMOVE`
  - `GETATTR`, `SETATTR`
  - `LOOKUP`, `READDIR`
  - `LOCK`, `LOCKU`, `LOCKT`
- Serves layout information:
  - `LAYOUTGET` - tells client which DS to use for byte ranges
  - `LAYOUTRETURN` - client returns layout
  - `LAYOUTCOMMIT` - client commits writes
  - `GETDEVICEINFO` - describes DS endpoints
- Manages state:
  - Client IDs and sessions
  - State IDs (open/lock)
  - Leases
  - Layout recalls

**2. Data Server (DS)**
- Handles only data operations:
  - `READ` - direct reads from storage
  - `WRITE` - direct writes to storage
  - `COMMIT` - fsync/flush
- Optimized for:
  - High throughput
  - Low latency
  - Parallel I/O
- Can be thin wrapper around SPDK

### Protocol Flow

**Initial Mount:**
1. Client contacts MDS (port 2049)
2. Client performs `EXCHANGE_ID`, `CREATE_SESSION`
3. Client does `PUTROOTFH`, `GETFH` to get root filehandle

**File Access:**
1. Client sends `OPEN` to MDS
2. MDS returns stateid
3. Client sends `LAYOUTGET` to MDS
4. MDS returns layout:
   - Device ID
   - Byte ranges
   - DS endpoint(s)
5. If needed, client sends `GETDEVICEINFO` to MDS
6. MDS returns DS network addresses

**Data I/O:**
1. Client connects directly to DS
2. Client sends `READ`/`WRITE` with:
   - Filehandle (from MDS)
   - Stateid (from MDS)
   - Offset/length (within layout range)
3. DS performs I/O directly on SPDK bdev

**Close/Commit:**
1. Client sends `LAYOUTCOMMIT` to MDS (optional)
2. Client sends `LAYOUTRETURN` to MDS
3. Client sends `CLOSE` to MDS

---

## 3. New Modules Required

### 3.1 Metadata Server Module

**Location**: `src/pnfs/mds/`

```
src/pnfs/mds/
├── mod.rs              # Module exports
├── server.rs           # MDS server main loop
├── layout.rs           # Layout management
├── device.rs           # Device ID registry
├── stripe.rs           # Striping policies
├── state.rs            # MDS-specific state
├── operations/
│   ├── mod.rs
│   ├── layoutget.rs    # LAYOUTGET implementation
│   ├── layoutreturn.rs # LAYOUTRETURN
│   ├── layoutcommit.rs # LAYOUTCOMMIT
│   ├── getdeviceinfo.rs # GETDEVICEINFO
│   └── getdevicelist.rs # GETDEVICELIST
└── config.rs           # MDS configuration
```

**Key Responsibilities:**
- Parse pNFS configuration file
- Register and track data servers
- Serve metadata operations (reuse existing v4 operations)
- Generate layouts based on policy (round-robin, stripe, etc.)
- Handle layout recalls on DS failure
- Persist state (etcd, K8s ConfigMaps, or local DB)

**New Operations to Implement:**
- `LAYOUTGET` (opcode 50)
- `LAYOUTRETURN` (opcode 51)
- `LAYOUTCOMMIT` (opcode 52)
- `GETDEVICEINFO` (opcode 47)
- `GETDEVICELIST` (opcode 48)

### 3.2 Data Server Module

**Location**: `src/pnfs/ds/`

```
src/pnfs/ds/
├── mod.rs              # Module exports
├── server.rs           # DS server main loop
├── io.rs               # I/O operations
├── registration.rs     # Register with MDS
└── config.rs           # DS configuration
```

**Key Responsibilities:**
- Lightweight NFS server handling only `READ`, `WRITE`, `COMMIT`
- Register with MDS at startup (device ID, endpoint, capacity)
- Heartbeat to MDS
- Direct I/O to SPDK bdevs
- Minimal state (no OPEN/CLOSE tracking)

**Supported Operations:**
- `READ` (opcode 25)
- `WRITE` (opcode 38)
- `COMMIT` (opcode 5)
- `NULL` (opcode 0)
- `COMPOUND` (opcode 1) - wrapper only

### 3.3 Configuration Module

**Location**: `src/pnfs/config.rs`

**Structure:**
```rust
pub struct PnfsConfig {
    pub mode: PnfsMode,
    pub mds: Option<MdsConfig>,
    pub ds: Option<DsConfig>,
}

pub enum PnfsMode {
    Disabled,           // Use standalone NFS (current)
    MetadataServer,     // Run as MDS
    DataServer,         // Run as DS
}

pub struct MdsConfig {
    pub bind_addr: String,
    pub bind_port: u16,
    pub layout_type: LayoutType,
    pub stripe_size: u64,
    pub data_servers: Vec<DataServerInfo>,
    pub state_backend: StateBackend,
}

pub struct DsConfig {
    pub bind_addr: String,
    pub bind_port: u16,
    pub device_id: String,
    pub mds_endpoint: String,
    pub bdevs: Vec<String>,
    pub heartbeat_interval: u64,
}

pub enum LayoutType {
    File,    // NFSv4.1 FILE layout (RFC 5661)
    Block,   // NFSv4.1 BLOCK layout (RFC 5663) - future
    Object,  // NFSv4.1 OBJECT layout (RFC 5664) - future
}

pub enum StateBackend {
    InMemory,      // For dev/testing
    Etcd(String),  // For production HA
    K8sConfigMap,  // For simple K8s deployments
}

pub struct DataServerInfo {
    pub device_id: String,
    pub endpoint: String,      // IP:port or DNS name
    pub multipath: Vec<String>, // Additional endpoints for HA
}
```

---

## 4. Configuration File Format

### 4.1 YAML Configuration

**File**: `/etc/flint/pnfs.yaml` or ConfigMap in Kubernetes

```yaml
# pNFS Configuration for Flint NFS Server
apiVersion: flint.io/v1alpha1
kind: PnfsConfig

# Mode: standalone, mds, or ds
mode: standalone  # Default - current behavior

# Metadata Server Configuration (when mode: mds)
mds:
  bind:
    address: "0.0.0.0"
    port: 2049
  
  layout:
    type: file           # file, block, object
    stripeSize: 8388608  # 8 MiB stripe size
    policy: roundrobin   # roundrobin, stripe, locality
  
  # Data server registry
  dataServers:
    - deviceId: ds-01
      endpoint: "10.244.1.10:2049"
      multipath:
        - "10.244.2.10:2049"  # Optional HA paths
      bdevs:
        - nvme0n1
    
    - deviceId: ds-02
      endpoint: "10.244.1.11:2049"
      bdevs:
        - nvme0n1
        - nvme1n1
    
    - deviceId: ds-03
      endpoint: "10.244.1.12:2049"
      bdevs:
        - nvme0n1
  
  # State persistence
  state:
    backend: kubernetes  # memory, etcd, kubernetes
    config:
      namespace: flint-system
      configmap: flint-pnfs-state
  
  # High availability
  ha:
    enabled: true
    replicas: 3
    leaderElection: true

# Data Server Configuration (when mode: ds)
ds:
  bind:
    address: "0.0.0.0"
    port: 2049
  
  deviceId: ds-01  # Unique identifier
  
  # MDS to register with
  mds:
    endpoint: "flint-pnfs-mds.flint-system.svc.cluster.local:2049"
    heartbeatInterval: 10  # seconds
    registrationRetry: 5   # seconds
  
  # SPDK block devices to serve
  bdevs:
    - name: nvme0n1
      path: /dev/nvme0n1
    - name: nvme1n1
      path: /dev/nvme1n1
  
  # Resource limits
  resources:
    maxConnections: 1000
    ioQueueDepth: 128

# Exports (shared between modes)
exports:
  - path: /
    fsid: 1
    options:
      - rw
      - sync
      - no_subtree_check
    access:
      - network: 0.0.0.0/0
        permissions: rw

# Logging and monitoring
logging:
  level: info  # debug, info, warn, error
  format: json
  
monitoring:
  prometheus:
    enabled: true
    port: 9090
```

### 4.2 Environment Variables (Kubernetes)

For simpler deployments, support environment variables:

```bash
# Mode selection
PNFS_MODE=standalone|mds|ds

# MDS-specific
PNFS_MDS_BIND_ADDR=0.0.0.0
PNFS_MDS_BIND_PORT=2049
PNFS_MDS_LAYOUT_TYPE=file
PNFS_MDS_STRIPE_SIZE=8388608
PNFS_MDS_DATA_SERVERS=ds-01:10.244.1.10:2049,ds-02:10.244.1.11:2049

# DS-specific
PNFS_DS_DEVICE_ID=ds-01
PNFS_DS_BIND_ADDR=0.0.0.0
PNFS_DS_BIND_PORT=2049
PNFS_DS_MDS_ENDPOINT=flint-pnfs-mds:2049
PNFS_DS_BDEVS=nvme0n1,nvme1n1
```

### 4.3 Kubernetes CRD (Advanced)

**Custom Resource Definition**: `PnfsExport`

```yaml
apiVersion: flint.io/v1alpha1
kind: PnfsExport
metadata:
  name: my-pnfs-volume
  namespace: default
spec:
  volumeId: vol-12345
  capacity: 10Gi
  
  # pNFS configuration
  pnfs:
    enabled: true
    layoutType: file
    stripeSize: 8Mi
  
  # MDS configuration
  mds:
    replicas: 3
    image: flint/pnfs-mds:latest
    resources:
      requests:
        memory: 256Mi
        cpu: 500m
      limits:
        memory: 512Mi
        cpu: 1000m
  
  # Data servers
  dataServers:
    - nodeSelector:
        kubernetes.io/hostname: node-1
      bdevs:
        - nvme0n1
    - nodeSelector:
        kubernetes.io/hostname: node-2
      bdevs:
        - nvme0n1
    - nodeSelector:
        kubernetes.io/hostname: node-3
      bdevs:
        - nvme0n1
  
  # Access control
  access:
    mode: ReadWriteMany
    allowedClients:
      - 10.244.0.0/16  # Pod CIDR

status:
  phase: Ready
  mdsEndpoint: flint-pnfs-mds-my-pnfs-volume:2049
  dataServers:
    - deviceId: ds-node-1-nvme0n1
      endpoint: 10.244.1.10:2049
      status: Ready
    - deviceId: ds-node-2-nvme0n1
      endpoint: 10.244.1.11:2049
      status: Ready
    - deviceId: ds-node-3-nvme0n1
      endpoint: 10.244.1.12:2049
      status: Ready
```

---

## 5. Protocol Implementation Requirements

### 5.1 NFSv4.1 pNFS Operations

The existing NFSv4.2 implementation needs to be extended with:

#### LAYOUTGET (opcode 50)

**Request:**
```rust
pub struct LayoutGet {
    pub signal_layout_avail: bool,
    pub layout_type: LayoutType,
    pub iomode: IoMode,
    pub offset: u64,
    pub length: u64,
    pub minlength: u64,
    pub stateid: StateId,
    pub maxcount: u32,
}

pub enum IoMode {
    Read = 1,
    RW = 2,
    Any = 3,
}
```

**Response:**
```rust
pub struct LayoutGetResult {
    pub return_on_close: bool,
    pub stateid: StateId,
    pub layouts: Vec<Layout>,
}

pub struct Layout {
    pub offset: u64,
    pub length: u64,
    pub iomode: IoMode,
    pub layout_content: LayoutContent,
}

pub enum LayoutContent {
    File(FileLayout),
    Block(BlockLayout),
    Object(ObjectLayout),
}

pub struct FileLayout {
    pub device_id: Vec<u8>,
    pub pattern_offset: u64,
    pub first_stripe_index: u32,
    pub stripe_unit: u64,
    pub commit_through_mds: bool,
    pub stripe_indices: Vec<u32>,
    pub filehandles: Vec<Nfs4FileHandle>,
}
```

**Logic:**
1. Validate stateid
2. Check file is open for appropriate mode
3. Select data server(s) based on policy
4. Generate layout covering requested range
5. Return layout with device IDs and filehandles

#### GETDEVICEINFO (opcode 47)

**Request:**
```rust
pub struct GetDeviceInfo {
    pub device_id: Vec<u8>,
    pub layout_type: LayoutType,
    pub maxcount: u32,
    pub notify_types: Vec<u32>,
}
```

**Response:**
```rust
pub struct DeviceInfo {
    pub device_id: Vec<u8>,
    pub layout_type: LayoutType,
    pub layout_device: LayoutDevice,
    pub notification: Vec<NotifyDeviceType>,
}

pub struct LayoutDevice {
    pub addresses: Vec<DeviceAddr>,
}

pub struct DeviceAddr {
    pub netid: String,    // "tcp", "rdma"
    pub addr: String,     // "10.244.1.10.8.1" (XDR format)
}
```

**Logic:**
1. Lookup device ID in registry
2. Return network addresses for DS
3. Optionally include notification preferences

#### LAYOUTRETURN (opcode 51)

**Request:**
```rust
pub struct LayoutReturn {
    pub reclaim: bool,
    pub layout_type: LayoutType,
    pub iomode: IoMode,
    pub return_type: LayoutReturnType,
}

pub enum LayoutReturnType {
    File {
        offset: u64,
        length: u64,
        stateid: StateId,
        layout_body: Vec<u8>,
    },
    Fsid(Fsid),
    All,
}
```

**Response:**
```rust
pub struct LayoutReturnResult {
    pub new_stateid: Option<StateId>,
}
```

**Logic:**
1. Validate stateid
2. Update layout state (mark returned)
3. If all layouts returned, update stateid
4. Cleanup any recalled layouts

#### LAYOUTCOMMIT (opcode 52)

**Request:**
```rust
pub struct LayoutCommit {
    pub offset: u64,
    pub length: u64,
    pub reclaim: bool,
    pub stateid: StateId,
    pub new_offset: u64,
    pub new_time: Option<NfsTime>,
    pub layout_body: Vec<u8>,
}
```

**Response:**
```rust
pub struct LayoutCommitResult {
    pub new_size: Option<u64>,
    pub new_time: Option<NfsTime>,
}
```

**Logic:**
1. Validate stateid
2. Update file metadata (size, mtime)
3. Mark layout committed
4. Return new file attributes

### 5.2 Layout Types

Start with **FILE** layout (RFC 5661 Section 13):

```rust
pub enum LayoutType {
    File = 1,    // LAYOUT4_NFSV4_1_FILES
    Block = 2,   // LAYOUT4_BLOCK_VOLUME (future)
    Object = 3,  // LAYOUT4_OSD2_OBJECTS (future)
}
```

**File Layout** maps byte ranges to data servers:
- Simple stripe: split file into fixed-size chunks across DSs
- Round-robin: first chunk to DS1, second to DS2, etc.
- Dense stripe: interleave small (4K-64K) units for parallel I/O

### 5.3 Device ID Format

**Simple format** (for initial implementation):
```
device_id = "ds-" + node_name + "-" + bdev_name
Example: "ds-node-1-nvme0n1"
```

**Binary format** (XDR encoded):
```rust
pub struct DeviceId {
    pub node_id: u32,   // Unique node identifier
    pub bdev_id: u32,   // Bdev index on that node
    pub generation: u32, // Increments on restart
}
```

---

## 6. MDS State Management

### 6.1 State Components

**1. Device Registry**
```rust
pub struct DeviceRegistry {
    devices: DashMap<DeviceId, DeviceInfo>,
}

pub struct DeviceInfo {
    pub device_id: DeviceId,
    pub addresses: Vec<DeviceAddr>,
    pub capacity: u64,
    pub used: u64,
    pub status: DeviceStatus,
    pub last_heartbeat: Instant,
}

pub enum DeviceStatus {
    Active,
    Degraded,
    Offline,
}
```

**2. Layout State**
```rust
pub struct LayoutManager {
    layouts: DashMap<StateId, Vec<LayoutSegment>>,
    file_layouts: DashMap<Nfs4FileHandle, Vec<LayoutSegment>>,
}

pub struct LayoutSegment {
    pub offset: u64,
    pub length: u64,
    pub iomode: IoMode,
    pub device_id: DeviceId,
    pub stripe_index: u32,
    pub generation: u32,
    pub recalled: bool,
}
```

**3. Layout Policies**
```rust
pub trait LayoutPolicy {
    fn generate_layout(
        &self,
        filehandle: &Nfs4FileHandle,
        offset: u64,
        length: u64,
        iomode: IoMode,
        devices: &[DeviceInfo],
    ) -> Result<Vec<LayoutSegment>>;
    
    fn recall_on_failure(&self, failed_device: &DeviceId);
}

pub struct RoundRobinPolicy {
    stripe_size: u64,
}

pub struct StripePolicy {
    stripe_unit: u64,
    stripe_count: u32,
}

pub struct LocalityPolicy {
    // Prefer local DS based on client location
}
```

### 6.2 Persistence

**Options:**

1. **In-Memory** (dev/testing)
   - Simple DashMap storage
   - Lost on restart
   - Fast

2. **Kubernetes ConfigMaps** (simple production)
   - Store state in ConfigMap
   - Periodic snapshots
   - K8s-native

3. **etcd** (HA production)
   - Distributed consensus
   - Multiple MDS replicas
   - Leader election
   - Strong consistency

**State to Persist:**
- Device registry
- Active layouts
- Client sessions
- Stateids

---

## 7. Data Server Implementation

### 7.1 Minimal Operations

DS only needs to implement:

```rust
pub struct DataServer {
    config: DsConfig,
    bdevs: Arc<BdevManager>,
    mds_client: Arc<MdsClient>,
}

impl DataServer {
    // Register with MDS at startup
    pub async fn register(&self) -> Result<()>;
    
    // Send periodic heartbeats
    pub async fn heartbeat_loop(&self);
    
    // Handle NFS operations
    pub async fn handle_read(&self, req: ReadRequest) -> Result<ReadResult>;
    pub async fn handle_write(&self, req: WriteRequest) -> Result<WriteResult>;
    pub async fn handle_commit(&self, req: CommitRequest) -> Result<CommitResult>;
}
```

### 7.2 Registration Protocol

**MDS-DS Communication** (out-of-band control channel):

Option 1: **gRPC**
```protobuf
service MdsControl {
  rpc RegisterDataServer(RegisterRequest) returns (RegisterResponse);
  rpc Heartbeat(HeartbeatRequest) returns (HeartbeatResponse);
  rpc UpdateCapacity(CapacityUpdate) returns (Ack);
}

message RegisterRequest {
  string device_id = 1;
  repeated string endpoints = 2;
  uint64 capacity = 3;
  repeated string bdevs = 4;
}
```

Option 2: **NFS RPC** (simpler)
- Use private NFSv4 operations (high opcodes)
- `OP_REGISTER_DS` (opcode 10001)
- `OP_HEARTBEAT` (opcode 10002)

### 7.3 Direct I/O Path

```rust
pub async fn handle_read(
    &self,
    filehandle: &Nfs4FileHandle,
    stateid: &StateId,
    offset: u64,
    count: u32,
) -> Result<ReadResult> {
    // 1. Validate filehandle and stateid (minimal checks)
    
    // 2. Map filehandle to bdev
    let bdev = self.bdevs.lookup_by_fh(filehandle)?;
    
    // 3. Direct SPDK I/O
    let data = bdev.read(offset, count as usize).await?;
    
    // 4. Return data
    Ok(ReadResult {
        eof: offset + (count as u64) >= bdev.size(),
        data,
    })
}
```

---

## 8. Integration with Existing System

### 8.1 Minimal Changes to Current Code

**No modifications needed** to:
- `src/nfs/server_v4.rs` - standalone mode continues to work
- `src/nfs/v4/operations/` - all existing ops reused
- `src/rwx_nfs.rs` - K8s integration

**New files only** (additive):
- `src/pnfs/` - all new modules
- `src/nfs_mds_main.rs` - MDS binary entry point
- `src/nfs_ds_main.rs` - DS binary entry point

### 8.2 Binary Targets

Add to `Cargo.toml`:

```toml
[[bin]]
name = "flint-nfs-server"
path = "src/nfs_main.rs"
# Existing standalone server

[[bin]]
name = "flint-pnfs-mds"
path = "src/nfs_mds_main.rs"
# NEW: pNFS metadata server

[[bin]]
name = "flint-pnfs-ds"
path = "src/nfs_ds_main.rs"
# NEW: pNFS data server
```

### 8.3 Docker Images

**Option 1**: Single image with multiple binaries
```dockerfile
FROM rust:1.75 as builder
# Build all three binaries
COPY . /build
RUN cargo build --release --bins

FROM ubuntu:22.04
COPY --from=builder /build/target/release/flint-nfs-server /usr/local/bin/
COPY --from=builder /build/target/release/flint-pnfs-mds /usr/local/bin/
COPY --from=builder /build/target/release/flint-pnfs-ds /usr/local/bin/

# Select at runtime via command
ENTRYPOINT ["/usr/local/bin/flint-nfs-server"]
```

**Option 2**: Separate images
```
flint/nfs-server:latest     # Standalone
flint/pnfs-mds:latest       # MDS only
flint/pnfs-ds:latest        # DS only
```

### 8.4 Kubernetes Deployment

**Standalone Mode** (current):
```yaml
apiVersion: v1
kind: Pod
metadata:
  name: flint-nfs-vol-123
spec:
  containers:
  - name: nfs-server
    image: flint/nfs-server:latest
    command: ["/usr/local/bin/flint-nfs-server"]
    args:
      - --export-path=/mnt/volume
      - --volume-id=vol-123
```

**pNFS Mode** (new):
```yaml
# MDS Deployment (1-3 replicas)
apiVersion: apps/v1
kind: Deployment
metadata:
  name: flint-pnfs-mds-vol-123
spec:
  replicas: 3
  template:
    spec:
      containers:
      - name: mds
        image: flint/pnfs-mds:latest
        command: ["/usr/local/bin/flint-pnfs-mds"]
        args:
          - --config=/etc/pnfs/mds.yaml
        volumeMounts:
        - name: config
          mountPath: /etc/pnfs
      volumes:
      - name: config
        configMap:
          name: pnfs-mds-config-vol-123

---
# Data Server DaemonSet (one per node)
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: flint-pnfs-ds-vol-123
spec:
  selector:
    matchLabels:
      app: flint-pnfs-ds
      volume: vol-123
  template:
    spec:
      containers:
      - name: ds
        image: flint/pnfs-ds:latest
        command: ["/usr/local/bin/flint-pnfs-ds"]
        args:
          - --config=/etc/pnfs/ds.yaml
        volumeMounts:
        - name: config
          mountPath: /etc/pnfs
        - name: spdk-socket
          mountPath: /var/tmp/spdk
      volumes:
      - name: config
        configMap:
          name: pnfs-ds-config-vol-123
      - name: spdk-socket
        hostPath:
          path: /var/tmp/spdk
```

---

## 9. Development Phases

### Phase 0: Design & Setup (2 weeks)
- [x] Explore architecture and requirements
- [ ] Finalize configuration format
- [ ] Design module structure
- [ ] Create stub implementations

### Phase 1: Basic MDS (4 weeks)
- [ ] Implement `LAYOUTGET` operation
- [ ] Implement `GETDEVICEINFO` operation
- [ ] Static device registry (hardcoded for testing)
- [ ] Simple round-robin layout policy
- [ ] In-memory state only

### Phase 2: Basic DS (3 weeks)
- [ ] Implement minimal NFS server (READ/WRITE/COMMIT only)
- [ ] Direct I/O to SPDK bdevs
- [ ] Registration with MDS (gRPC or NFS RPC)
- [ ] Heartbeat mechanism

### Phase 3: Integration Testing (2 weeks)
- [ ] MDS + single DS test
- [ ] Linux kernel pNFS client
- [ ] Basic read/write tests
- [ ] Layout generation verification

### Phase 4: Multi-DS & Striping (3 weeks)
- [ ] Multiple DS support
- [ ] Stripe layout policy
- [ ] Parallel I/O testing
- [ ] Performance benchmarking

### Phase 5: Configuration & Deployment (2 weeks)
- [ ] YAML configuration parsing
- [ ] Environment variable support
- [ ] Kubernetes manifests
- [ ] Helm chart updates

### Phase 6: State Management (4 weeks)
- [ ] `LAYOUTRETURN` operation
- [ ] `LAYOUTCOMMIT` operation
- [ ] State persistence (ConfigMap)
- [ ] State persistence (etcd)

### Phase 7: Failure Handling (3 weeks)
- [ ] DS failure detection
- [ ] Layout recall (CB_LAYOUTRECALL)
- [ ] Client recovery
- [ ] Failover testing

### Phase 8: HA & Production (4 weeks)
- [ ] MDS replication
- [ ] Leader election
- [ ] Stateful failover
- [ ] Comprehensive testing

**Total Estimated Time**: 27 weeks (~6-7 months)

---

## 10. Testing Strategy

### 10.1 Unit Tests
- Layout generation algorithms
- Device registry operations
- Stateid management
- XDR encoding/decoding

### 10.2 Integration Tests
```bash
# Test with Linux kernel NFS client
mount -t nfs -o vers=4.1,minorversion=1 mds-server:/ /mnt

# Verify pNFS negotiation
cat /proc/self/mountstats | grep pnfs

# Run I/O tests
fio --name=pnfs-test --rw=randwrite --bs=4k --size=1G --filename=/mnt/testfile
```

### 10.3 Conformance Tests
- NFSv4.1 test suite: `nfstest_pnfs`
- Connectathon NFS tests
- pNFS-specific tests from `git://git.linux-nfs.org/projects/jlayton/nfstest.git`

### 10.4 Performance Tests
```bash
# Baseline (standalone NFS)
fio --name=baseline --rw=randwrite --bs=4k --size=10G --numjobs=1 --filename=/mnt/test

# pNFS with 1 DS (should match baseline)
fio --name=pnfs-1ds --rw=randwrite --bs=4k --size=10G --numjobs=1 --filename=/mnt/test

# pNFS with 3 DS (should show parallel I/O)
fio --name=pnfs-3ds --rw=randwrite --bs=4k --size=10G --numjobs=4 --filename=/mnt/test
```

**Expected improvements** with pNFS:
- 2-3x throughput with 3 data servers
- Lower latency under high concurrency
- Better scaling with client count

---

## 11. Configuration Examples

### 11.1 Development Setup

**Single-node test** (MDS + DS on localhost):

`pnfs-dev.yaml`:
```yaml
mode: mds

mds:
  bind:
    address: "127.0.0.1"
    port: 2049
  
  layout:
    type: file
    stripeSize: 4194304  # 4 MiB
    policy: roundrobin
  
  dataServers:
    - deviceId: ds-local-1
      endpoint: "127.0.0.1:2050"
      bdevs: [test-bdev-1]
  
  state:
    backend: memory

exports:
  - path: /
    fsid: 1
```

`ds-dev.yaml`:
```yaml
mode: ds

ds:
  bind:
    address: "127.0.0.1"
    port: 2050
  
  deviceId: ds-local-1
  
  mds:
    endpoint: "127.0.0.1:2049"
    heartbeatInterval: 10
  
  bdevs:
    - name: test-bdev-1
      path: /tmp/test.img
```

### 11.2 Production Setup

**3-node cluster** with MDS HA:

`pnfs-prod.yaml`:
```yaml
mode: mds

mds:
  bind:
    address: "0.0.0.0"
    port: 2049
  
  layout:
    type: file
    stripeSize: 8388608  # 8 MiB
    policy: stripe
  
  dataServers:
    - deviceId: ds-node1-nvme0
      endpoint: "node1.cluster.local:2049"
      multipath:
        - "node1-rdma.cluster.local:20049"
      bdevs: [nvme0n1]
    
    - deviceId: ds-node2-nvme0
      endpoint: "node2.cluster.local:2049"
      bdevs: [nvme0n1, nvme1n1]
    
    - deviceId: ds-node3-nvme0
      endpoint: "node3.cluster.local:2049"
      bdevs: [nvme0n1]
  
  state:
    backend: etcd
    config:
      endpoints:
        - etcd-0.etcd:2379
        - etcd-1.etcd:2379
        - etcd-2.etcd:2379
  
  ha:
    enabled: true
    replicas: 3
    leaderElection: true

exports:
  - path: /
    fsid: 1
    options: [rw, sync]
    access:
      - network: 10.0.0.0/8
        permissions: rw
```

---

## 12. Open Questions & Decisions

### 12.1 Configuration Loading
**Question**: Support YAML file, env vars, or K8s CRD?
**Recommendation**: All three (in order of preference):
1. CRD for production K8s deployments
2. YAML file for flexibility
3. Env vars for simple cases

### 12.2 MDS-DS Communication
**Question**: gRPC control plane or NFS RPC?
**Recommendation**: Start with gRPC
- Better tooling
- Type safety
- Easy testing
- Clear separation from NFS protocol

### 12.3 State Persistence
**Question**: etcd, K8s ConfigMaps, or embedded DB (sled)?
**Recommendation**: Tiered approach:
- Phase 1: In-memory (dev)
- Phase 2: K8s ConfigMaps (simple prod)
- Phase 3: etcd (HA prod)

### 12.4 Layout Type
**Question**: FILE, BLOCK, or OBJECT layout?
**Recommendation**: FILE layout first
- Simplest to implement
- Best Linux kernel support
- Natural fit for filesystem workloads

### 12.5 Failure Handling
**Question**: How to handle DS failures?
**Options**:
1. Recall all layouts immediately (safe, but disruptive)
2. Recall only affected layouts (complex)
3. Let clients discover failure organically (slow)

**Recommendation**: Hybrid:
- Fast failure detection via heartbeat
- Recall layouts for failed device only
- Remap to surviving DSs

### 12.6 SPDK Integration
**Question**: How does DS access SPDK bdevs?
**Options**:
1. Direct SPDK library calls (requires same process)
2. SPDK JSON-RPC (out-of-process)
3. ublk/bdev_ublk (Linux block device)

**Recommendation**: ublk for Phase 1 (simpler), SPDK library for Phase 2 (faster)

---

## 13. Benefits of pNFS for Flint

### 13.1 Performance
- **Parallel I/O**: Multiple DSs serve different byte ranges simultaneously
- **Reduced MDS load**: Data path bypasses metadata server
- **Scalability**: Add DSs independently of MDS
- **Locality**: Place DS on same node as SPDK bdev

### 13.2 High Availability
- **MDS replication**: Multiple MDS replicas with leader election
- **DS failover**: Layout recall and remap on DS failure
- **No single point of failure**: Client can reach any MDS replica

### 13.3 Resource Efficiency
- **Dedicated resources**: MDS and DS can scale independently
- **Optimized DS**: Thin I/O-only server can be very lightweight
- **Cache efficiency**: MDS caches metadata, DS caches data

### 13.4 Flexibility
- **Mixed workloads**: Small files use MDS, large files stripe across DSs
- **Policy-based**: Layout policies adapt to workload (sequential vs random)
- **Dynamic scaling**: Add/remove DSs without downtime

---

## 14. Migration Path

### 14.1 Coexistence
- Existing volumes continue using standalone NFS
- New volumes can opt into pNFS via annotation
- No flag day required

### 14.2 Upgrade Process
```yaml
# Existing PVC (standalone NFS)
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: my-volume
spec:
  accessModes: [ReadWriteMany]
  resources:
    requests:
      storage: 10Gi
  storageClassName: flint

# New PVC (pNFS-enabled)
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: my-pnfs-volume
  annotations:
    flint.io/pnfs: "true"
    flint.io/pnfs-layout-type: "file"
    flint.io/pnfs-stripe-size: "8Mi"
spec:
  accessModes: [ReadWriteMany]
  resources:
    requests:
      storage: 10Gi
  storageClassName: flint
```

### 14.3 Feature Flag
```bash
# Disable pNFS globally (default: disabled)
helm install flint-csi-driver flint/flint-csi-driver \
  --set pnfs.enabled=false

# Enable pNFS globally
helm install flint-csi-driver flint/flint-csi-driver \
  --set pnfs.enabled=true \
  --set pnfs.mds.replicas=3 \
  --set pnfs.ds.nodeselector="storage-node=true"
```

---

## 15. Next Steps

### Immediate Actions
1. **Review this document** with team
2. **Decide on configuration approach** (YAML vs CRD vs env vars)
3. **Choose MDS-DS protocol** (gRPC vs NFS RPC)
4. **Set up development environment**

### Phase 0 Deliverables
1. **Module structure**:
   ```
   src/pnfs/
   ├── mod.rs
   ├── config.rs       # Config parsing (stub)
   ├── mds/
   │   ├── mod.rs
   │   └── server.rs   # MDS skeleton
   └── ds/
       ├── mod.rs
       └── server.rs   # DS skeleton
   ```

2. **Configuration schema** (finalized YAML spec)

3. **Protocol definitions** (Rust structs for pNFS operations)

4. **Build system** (Cargo.toml with new binaries)

5. **Test plan** (unit tests, integration tests, perf tests)

### Questions to Answer
- [ ] Configuration format preference?
- [ ] MDS-DS communication protocol?
- [ ] State persistence strategy?
- [ ] Timeline expectations?
- [ ] Resource allocation (developers, infrastructure)?

---

## 16. References

### RFCs
- **RFC 5661**: NFSv4.1 (includes pNFS spec)
- **RFC 7862**: NFSv4.2
- **RFC 8881**: NFSv4.1 (updated)
- **RFC 8434**: Requirements for Parallel NFS (pNFS) Layout Types

### Layout-Specific RFCs
- **RFC 5661 Chapter 13**: FILE Layout
- **RFC 5663**: BLOCK Volume Layout
- **RFC 5664**: OBJECT Layout

### Implementation Guides
- Linux kernel NFS: `fs/nfs/pnfs*`
- Ganesha NFS: `src/FSAL/*/pnfs.c`
- NetApp ONTAP pNFS architecture
- Lustre pNFS implementation

### Testing Tools
- `nfstest_pnfs`: pNFS conformance testing
- `fio`: Performance benchmarking
- `nfs4_getdeviceinfo`: Client-side debugging
- Wireshark: Protocol analysis

---

## 17. Glossary

- **pNFS**: Parallel NFS - NFSv4.1+ extension for parallel data access
- **MDS**: Metadata Server - handles control plane operations
- **DS**: Data Server - handles data plane (READ/WRITE) operations
- **Layout**: Mapping of byte ranges to data servers
- **Device ID**: Identifier for a data server
- **Layout Type**: FILE, BLOCK, or OBJECT
- **Stateid**: NFSv4 state identifier (open/lock/layout)
- **COMPOUND**: NFSv4 operation batching mechanism

---

## Appendix A: File Structure

```
spdk-csi-driver/src/
├── nfs/                    # EXISTING - Standalone NFSv4.2 server
│   ├── mod.rs
│   ├── server_v4.rs
│   ├── rpc.rs
│   ├── xdr.rs
│   └── v4/
│       ├── compound.rs
│       ├── dispatcher.rs
│       ├── filehandle.rs
│       ├── protocol.rs
│       ├── state/
│       └── operations/
├── nfs_main.rs             # EXISTING - Standalone NFS binary
├── rwx_nfs.rs              # EXISTING - K8s integration
│
├── pnfs/                   # NEW - pNFS implementation
│   ├── mod.rs
│   ├── config.rs           # Configuration parsing
│   │
│   ├── mds/                # Metadata Server
│   │   ├── mod.rs
│   │   ├── server.rs       # MDS main loop
│   │   ├── layout.rs       # Layout management
│   │   ├── device.rs       # Device registry
│   │   ├── stripe.rs       # Striping policies
│   │   ├── state.rs        # MDS-specific state
│   │   ├── persistence/    # State persistence
│   │   │   ├── mod.rs
│   │   │   ├── memory.rs   # In-memory (dev)
│   │   │   ├── configmap.rs # K8s ConfigMap
│   │   │   └── etcd.rs     # etcd backend
│   │   └── operations/     # pNFS operations
│   │       ├── mod.rs
│   │       ├── layoutget.rs
│   │       ├── layoutreturn.rs
│   │       ├── layoutcommit.rs
│   │       ├── getdeviceinfo.rs
│   │       └── getdevicelist.rs
│   │
│   ├── ds/                 # Data Server
│   │   ├── mod.rs
│   │   ├── server.rs       # DS main loop
│   │   ├── io.rs           # I/O operations
│   │   ├── registration.rs # MDS registration
│   │   └── heartbeat.rs    # Health monitoring
│   │
│   └── proto/              # Protocol definitions (optional gRPC)
│       ├── mds_control.proto
│       └── ds_registration.proto
│
├── nfs_mds_main.rs         # NEW - MDS binary entry point
└── nfs_ds_main.rs          # NEW - DS binary entry point
```

**Total new files**: ~25  
**Modified files**: 0 (truly additive!)  
**New binaries**: 2 (`flint-pnfs-mds`, `flint-pnfs-ds`)

---

## Appendix B: Performance Expectations

### Baseline (Standalone NFS)
- Single NFS server pod
- All I/O through one network path
- Bottleneck: Single server CPU/network

**Typical Performance:**
- ~1 GB/s sequential read/write
- ~50K IOPS random 4K
- ~50-100 concurrent clients

### pNFS with 3 Data Servers

**Sequential Workload:**
- 3x throughput: ~3 GB/s
- Striping across 3 DSs
- Near-linear scaling

**Random Workload:**
- 2-2.5x IOPS: ~100-125K IOPS
- Depends on locality
- Better with many clients

**Many Clients:**
- 5-10x better scaling
- MDS not in data path
- DS failures don't block MDS

### Bottlenecks

**MDS**:
- Layout generation (CPU)
- State management (memory)
- Recall operations (network)

**DS**:
- SPDK bdev performance
- Network bandwidth
- CPU for XDR encoding

**Network**:
- Client-to-DS bandwidth
- MDS-DS control traffic (minimal)

---

## Appendix C: Security Considerations

### 1. Authentication
- Reuse NFSv4.1 auth (Kerberos, AUTH_SYS)
- MDS authenticates clients
- MDS signs layouts
- DS validates layout signatures

### 2. Authorization
- MDS checks ACLs/permissions
- DS trusts MDS decisions
- Stateids bind client to layout

### 3. Network Security
- TLS for MDS-DS control channel
- IPsec for data plane (optional)
- Network policies in K8s

### 4. Multi-Tenancy
- Namespace isolation
- Per-tenant device pools
- Resource quotas

---

**End of Document**

This exploration provides a comprehensive roadmap for adding pNFS support to Flint's NFS server. The modular design ensures zero impact on the existing standalone NFS implementation while enabling high-performance parallel I/O for demanding workloads.


