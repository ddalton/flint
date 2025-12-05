# Flint NFSv3 Server Implementation Roadmap

## Executive Summary

This document outlines the plan to add ReadWriteMany (RWX) support to Flint CSI driver by implementing a custom NFSv3 server in Rust. The NFS server will serve SPDK-backed volumes over the network, enabling multiple pods on different nodes to mount and write to the same volume simultaneously.

**Timeline:** 3-4 weeks for production-ready implementation
**Effort:** Medium complexity (★★★☆☆)
**Impact:** Enables RWX volumes for Kubernetes workloads

---

## Current Status

### ✅ Completed (Week 0)

1. **Architecture designed** - NFSv3 server serving local filesystem over SPDK volumes
2. **Protocol references identified** - RFC 1813 (NFSv3), RFC 4506 (XDR), RFC 5531 (RPC)
3. **Core modules implemented:**
   - `src/nfs/mod.rs` - Module organization
   - `src/nfs/xdr.rs` - XDR encoding/decoding (RFC 4506 compliant)
   - `src/nfs/rpc.rs` - Sun RPC message handling (RFC 5531 compliant)
   - `src/nfs/protocol.rs` - NFSv3 protocol types (RFC 1813 based)

**Lines of code so far:** ~800 lines
**Remaining:** ~1,200 lines estimated

---

## Implementation Plan

### Week 1: Core NFS Operations (Mount + Basic I/O)

**Goal:** Mountable NFS server with read/write capability

#### Day 1-2: VFS Abstraction & File Handle Management
**Files:**
- `src/nfs/vfs.rs` - Virtual filesystem trait
- `src/nfs/filehandle.rs` - File handle ↔ path mapping

**Key Components:**
```rust
// VFS trait - backend abstraction
pub trait VFS {
    async fn lookup(&self, dir_fh: FileHandle, name: &str) -> Result<FileHandle>;
    async fn getattr(&self, fh: FileHandle) -> Result<FileAttr>;
    async fn read(&self, fh: FileHandle, offset: u64, count: u32) -> Result<Vec<u8>>;
    async fn write(&self, fh: FileHandle, offset: u64, data: &[u8]) -> Result<u32>;
}

// LocalFilesystem - implementation for Flint
pub struct LocalFilesystem {
    export_path: PathBuf,  // e.g., /var/lib/flint/exports/vol-123
    handle_cache: HandleCache,
}
```

**Estimated time:** 8-10 hours

---

#### Day 3-4: Tier 1 NFSv3 Handlers
**File:** `src/nfs/handlers.rs`

**Procedures to implement:**
1. **NULL** (Procedure 0) - Ping/health check
2. **GETATTR** (Procedure 1) - Get file attributes
3. **LOOKUP** (Procedure 3) - Find file by name
4. **FSSTAT** (Procedure 18) - Filesystem statistics
5. **FSINFO** (Procedure 19) - Filesystem capabilities

**Example:**
```rust
pub async fn handle_getattr(
    vfs: &dyn VFS,
    call: &CallMessage,
    dec: &mut XdrDecoder,
) -> Bytes {
    // Decode file handle from request
    let file_handle = FileHandle::decode(dec)?;

    // Get attributes from VFS
    let attrs = vfs.getattr(file_handle).await?;

    // Build success reply
    let mut reply = ReplyBuilder::success(call.xid);
    let enc = reply.encoder();

    // Encode status + attributes (RFC 1813 Section 3.3.1)
    enc.encode_u32(NFS3Status::Ok as u32);
    enc.encode_bool(true);  // attributes_follow
    attrs.encode(enc);

    reply.finish()
}
```

**Estimated time:** 12-14 hours

---

#### Day 5: I/O Operations
**Procedures:**
6. **READ** (Procedure 6) - Read file data
7. **WRITE** (Procedure 7) - Write file data

**Key implementation:**
```rust
pub async fn handle_read(
    vfs: &dyn VFS,
    call: &CallMessage,
    dec: &mut XdrDecoder,
) -> Bytes {
    let file_handle = FileHandle::decode(dec)?;
    let offset = dec.decode_u64()?;
    let count = dec.decode_u32()?;

    // Read from VFS
    let data = vfs.read(file_handle.clone(), offset, count).await?;
    let eof = data.len() < count as usize;

    // Get updated attributes
    let attrs = vfs.getattr(file_handle).await?;

    // Build reply (RFC 1813 Section 3.3.6)
    let mut reply = ReplyBuilder::success(call.xid);
    let enc = reply.encoder();

    enc.encode_u32(NFS3Status::Ok as u32);
    enc.encode_bool(true);
    attrs.encode(enc);
    enc.encode_u32(data.len() as u32);
    enc.encode_bool(eof);
    enc.encode_opaque(&data);

    reply.finish()
}
```

**Estimated time:** 8-10 hours

---

### Week 1 Deliverable: Mountable NFS Server

**Test scenario:**
```bash
# Start NFS server
./flint-nfs-server --export-path /var/lib/flint/exports/vol-123 --volume-id vol-123

# From client
mount -t nfs -o vers=3,tcp localhost:/exports /mnt/test
echo "hello" > /mnt/test/file.txt
cat /mnt/test/file.txt  # Should print "hello"
```

**Success criteria:**
- ✅ Can mount with `mount.nfs`
- ✅ Can read existing files
- ✅ Can write to files
- ✅ Can see file metadata with `ls -la`

---

## Week 2: File Management Operations

### Day 1-2: File Creation & Deletion
**Files:** Continue in `src/nfs/handlers.rs`

**Procedures:**
8. **CREATE** (Procedure 8) - Create new file
9. **REMOVE** (Procedure 12) - Delete file

**Implementation:**
```rust
pub async fn handle_create(
    vfs: &dyn VFS,
    call: &CallMessage,
    dec: &mut XdrDecoder,
) -> Bytes {
    let dir_handle = FileHandle::decode(dec)?;
    let filename = dec.decode_string()?;

    // Create file via VFS
    let file_handle = vfs.create(dir_handle, &filename).await?;
    let attrs = vfs.getattr(file_handle.clone()).await?;

    // Build reply (RFC 1813 Section 3.3.8)
    let mut reply = ReplyBuilder::success(call.xid);
    let enc = reply.encoder();

    enc.encode_u32(NFS3Status::Ok as u32);
    enc.encode_bool(true);  // handle_follows
    file_handle.encode(enc);
    enc.encode_bool(true);  // attributes_follow
    attrs.encode(enc);

    reply.finish()
}
```

**Estimated time:** 8-10 hours

---

### Day 3-4: Directory Operations
**Procedures:**
10. **MKDIR** (Procedure 9) - Create directory
11. **RMDIR** (Procedure 13) - Delete directory

**Estimated time:** 8-10 hours

---

### Day 5: Directory Listing
**Procedure:**
12. **READDIR** (Procedure 16) - List directory contents

**Implementation:**
```rust
pub async fn handle_readdir(
    vfs: &dyn VFS,
    call: &CallMessage,
    dec: &mut XdrDecoder,
) -> Bytes {
    let dir_handle = FileHandle::decode(dec)?;
    let cookie = dec.decode_u64()?;  // Resume point
    let count = dec.decode_u32()?;   // Max bytes to return

    // List directory
    let entries = vfs.readdir(dir_handle, cookie, count).await?;

    // Build reply (RFC 1813 Section 3.3.16)
    let mut reply = ReplyBuilder::success(call.xid);
    let enc = reply.encoder();

    enc.encode_u32(NFS3Status::Ok as u32);
    // ... encode entries ...

    reply.finish()
}
```

**Estimated time:** 8-10 hours

---

### Week 2 Deliverable: Full File Operations

**Test scenario:**
```bash
mount -t nfs -o vers=3,tcp localhost:/exports /mnt/test

# File operations
touch /mnt/test/newfile.txt
echo "data" > /mnt/test/newfile.txt
cat /mnt/test/newfile.txt
rm /mnt/test/newfile.txt

# Directory operations
mkdir /mnt/test/mydir
ls -la /mnt/test/mydir
rmdir /mnt/test/mydir

# All should work!
```

**Success criteria:**
- ✅ Can create files
- ✅ Can delete files
- ✅ Can create directories
- ✅ Can delete directories
- ✅ Can list directory contents with `ls`

---

## Week 3: TCP Server & Integration

### Day 1-2: TCP/UDP Server
**File:** `src/nfs/server.rs`

**Key components:**
```rust
pub struct NfsServer {
    config: NfsConfig,
    vfs: Arc<dyn VFS>,
}

impl NfsServer {
    pub async fn serve(&self) -> Result<()> {
        // Bind UDP socket (NFSv3 can use UDP or TCP)
        let udp_socket = UdpSocket::bind(
            format!("{}:{}", self.config.bind_addr, self.config.bind_port)
        ).await?;

        let mut buf = vec![0u8; 65536];

        loop {
            let (len, addr) = udp_socket.recv_from(&mut buf).await?;

            // Parse RPC call
            let call = CallMessage::decode(Bytes::copy_from_slice(&buf[..len]))?;

            // Dispatch to handler
            let reply = self.dispatch(call, &buf[..len]).await;

            // Send reply
            udp_socket.send_to(&reply, addr).await?;
        }
    }

    async fn dispatch(&self, call: CallMessage, buf: &[u8]) -> Bytes {
        let mut dec = XdrDecoder::new(Bytes::copy_from_slice(buf));

        // Skip RPC header (already parsed)
        dec.decode_u32()?; // xid
        dec.decode_u32()?; // msg_type
        // ... skip to procedure args

        match Procedure::from_u32(call.procedure) {
            Some(Procedure::Null) => handlers::handle_null(&call),
            Some(Procedure::GetAttr) => handlers::handle_getattr(&*self.vfs, &call, &mut dec).await,
            Some(Procedure::Lookup) => handlers::handle_lookup(&*self.vfs, &call, &mut dec).await,
            Some(Procedure::Read) => handlers::handle_read(&*self.vfs, &call, &mut dec).await,
            Some(Procedure::Write) => handlers::handle_write(&*self.vfs, &call, &mut dec).await,
            // ... other procedures
            None => ReplyBuilder::proc_unavail(call.xid),
        }
    }
}
```

**Estimated time:** 12-14 hours

---

### Day 3: Binary Entry Point
**File:** `src/nfs_main.rs`

```rust
#[derive(Parser)]
struct Args {
    #[arg(short, long)]
    export_path: PathBuf,

    #[arg(short, long)]
    volume_id: String,

    #[arg(short, long, default_value = "0.0.0.0")]
    bind_addr: String,

    #[arg(short, long, default_value_t = 2049)]
    port: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Create VFS backend
    let vfs = LocalFilesystem::new(args.export_path)?;

    // Create NFS server
    let config = NfsConfig {
        bind_addr: args.bind_addr,
        bind_port: args.port,
        volume_id: args.volume_id,
        ..Default::default()
    };

    let server = NfsServer::new(config, Arc::new(vfs))?;

    println!("🚀 Flint NFS Server starting on {}:{}", config.bind_addr, config.bind_port);

    server.serve().await
}
```

**Update Cargo.toml:**
```toml
[[bin]]
name = "flint-nfs-server"
path = "src/nfs_main.rs"

[dependencies]
# Add:
clap = { version = "4.0", features = ["derive"] }
```

**Estimated time:** 4-6 hours

---

### Day 4-5: CSI Driver Integration
**Files to modify:**
- `src/driver.rs` (CSI Controller)
- `src/node.rs` (CSI Node)

**Controller changes:**
```rust
// In create_volume()
async fn create_volume(&mut self, request: CreateVolumeRequest) -> Result<CreateVolumeResponse> {
    let is_rwx = request.volume_capabilities
        .iter()
        .any(|cap| cap.access_mode == AccessMode::MultiNodeMultiWriter);

    if is_rwx {
        // Mark volume for NFS export
        volume_context.insert("nfs_export", "true");
        volume_context.insert("nfs_server_node", selected_node_id);
    }

    // ... rest of existing create logic
}
```

**Node changes:**
```rust
// In node_stage_volume()
async fn node_stage_volume(&self, request: NodeStageVolumeRequest) -> Result<NodeStageVolumeResponse> {
    // ... existing staging logic

    // Check if this volume needs NFS export
    if request.volume_context.get("nfs_export") == Some(&"true".to_string()) {
        let my_node = std::env::var("NODE_ID")?;
        let nfs_node = request.volume_context.get("nfs_server_node");

        if nfs_node == Some(&my_node) {
            // This node hosts the NFS server
            start_nfs_server(volume_id, staging_path).await?;
        }
    }

    Ok(NodeStageVolumeResponse {})
}

async fn start_nfs_server(volume_id: &str, mount_path: &str) -> Result<()> {
    // Spawn NFS server as background process
    tokio::process::Command::new("/usr/local/bin/flint-nfs-server")
        .arg("--export-path").arg(mount_path)
        .arg("--volume-id").arg(volume_id)
        .arg("--port").arg("2049")
        .spawn()?;

    Ok(())
}
```

**Estimated time:** 8-10 hours

---

### Week 3 Deliverable: End-to-End RWX

**Test scenario:**
```yaml
# 1. Create RWX storage class
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: flint-rwx
provisioner: csi.flint.io
parameters:
  thin: "true"
allowVolumeExpansion: true

---
# 2. Create RWX PVC
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: shared-data
spec:
  storageClassName: flint-rwx
  accessModes:
    - ReadWriteMany
  resources:
    requests:
      storage: 10Gi

---
# 3. Deploy multi-pod app
apiVersion: apps/v1
kind: Deployment
metadata:
  name: rwx-test
spec:
  replicas: 3
  template:
    spec:
      containers:
      - name: writer
        image: busybox
        command:
        - sh
        - -c
        - |
          while true; do
            echo "$(date) from $(hostname)" >> /data/shared.log
            sleep 5
          done
        volumeMounts:
        - name: data
          mountPath: /data
      volumes:
      - name: data
        persistentVolumeClaim:
          claimName: shared-data
```

**Success criteria:**
- ✅ PVC provisions successfully
- ✅ NFS server starts automatically on selected node
- ✅ All 3 pods can mount the volume
- ✅ All 3 pods can write concurrently
- ✅ Data is consistent across all pods

---

## Week 4: Polish & Production Readiness

### Day 1-2: Error Handling & Edge Cases
**Tasks:**
- Proper error handling for all operations
- Handle disk full scenarios
- Handle permission errors
- Handle stale file handles
- Timeout handling

**Estimated time:** 10-12 hours

---

### Day 3: Performance Optimization
**Tasks:**
- Buffer pooling to reduce allocations
- Metadata caching
- Async I/O optimization
- Benchmark against target workloads

**Target performance:**
- Read throughput: 1-2 GB/s (over NFS)
- Write throughput: 800 MB/s - 1.5 GB/s
- Latency: < 500μs for metadata operations

**Estimated time:** 8-10 hours

---

### Day 4-5: Testing & Documentation
**Tasks:**

1. **Unit tests** - Each handler, XDR encoding/decoding
2. **Integration tests** - Full mount/read/write scenarios
3. **Kubernetes tests** - Add to `tests/kuttl/`:
   ```
   tests/kuttl/rwx-basic/
   ├── 00-storageclass.yaml
   ├── 01-pvc.yaml
   ├── 02-multi-pod.yaml
   ├── 03-assert.yaml
   └── 04-cleanup.yaml
   ```

4. **Documentation:**
   - README section on RWX support
   - Architecture diagram
   - Troubleshooting guide
   - Performance tuning guide

**Estimated time:** 12-16 hours

---

## Future Enhancements (Post-Launch)

### Phase 2: High Availability (Optional)
**Estimated:** 1-2 weeks

Add multi-replica support with automatic failover:
```
┌─────────────────┐     ┌─────────────────┐
│ Node A          │     │ Node B          │
│ - SPDK Replica 1│◄───►│ SPDK Replica 2  │
│ - NFS Primary   │     │ - NFS Standby   │
│ - Active        │     │ - Ready         │
└─────────────────┘     └─────────────────┘

On failure: Node B takes over VIP in ~15 seconds
```

**Components needed:**
- VIP management (keepalived or manual)
- Health monitoring
- Failover logic
- Lock synchronization

---

### Phase 3: Advanced Features (Optional)
**Estimated:** 2-3 weeks total

1. **NFSv4 support** (if needed)
   - More complex, but better caching
   - Stateful sessions
   - File delegations

2. **pNFS support** (parallel NFS)
   - Multiple data servers
   - Higher throughput
   - Complex implementation

3. **Zero-copy optimization**
   - Use `io_uring` for direct I/O
   - Reduce memory allocations
   - Target: 3-4 GB/s throughput

---

## Dependencies & Requirements

### Rust Crates to Add
```toml
[dependencies]
# Already have: tokio, bytes, anyhow, tracing

# Need to add:
clap = { version = "4.0", features = ["derive"] }  # CLI parsing
thiserror = "1.0"                                   # Error types
```

### System Requirements (Runtime)
- Linux kernel with NFS client support (standard)
- No special kernel modules needed (userspace implementation)
- Network access on port 2049 (NFS)

### Build Requirements
- Rust 1.70+ (already have)
- Standard build tools (already have)

---

## Risk Assessment

### Low Risk ✅
- **XDR/RPC implementation** - Simple, well-tested protocols
- **File I/O operations** - Using standard Rust APIs
- **Integration with Flint** - Minimal changes to existing code

### Medium Risk ⚠️
- **NFSv3 protocol compliance** - Need thorough testing with real clients
- **Performance** - Need to verify meets targets
- **Edge cases** - File locking, concurrent access patterns

### Mitigation Strategies
1. **Test with real NFS clients** - Linux mount.nfs, macOS NFS client
2. **Benchmark early** - Week 1 deliverable includes perf testing
3. **Reference implementation** - Use `nfsserve` patterns as guide
4. **Phased rollout** - Mark as beta initially, promote after validation

---

## Success Metrics

### Week 1
- [ ] Can mount NFS server
- [ ] Can read existing files
- [ ] Can write new data
- [ ] Basic `ls` works

### Week 2
- [ ] Can create/delete files
- [ ] Can create/delete directories
- [ ] Full `ls -la` with metadata

### Week 3
- [ ] End-to-end Kubernetes integration
- [ ] Multiple pods writing simultaneously
- [ ] Data consistency verified

### Week 4
- [ ] Performance targets met
- [ ] All tests passing
- [ ] Documentation complete

---

## Next Immediate Steps

1. **Review this roadmap** - Confirm approach and timeline
2. **Continue implementation:**
   - Next: `src/nfs/vfs.rs` (VFS trait)
   - Then: `src/nfs/filehandle.rs` (File handle management)
   - Then: `src/nfs/handlers.rs` (Start with Tier 1 handlers)

3. **Set up development environment:**
   ```bash
   # Build NFS server binary
   cargo build --bin flint-nfs-server

   # Test locally
   mkdir -p /tmp/test-export
   ./target/debug/flint-nfs-server --export-path /tmp/test-export --volume-id test-vol
   ```

4. **Create tracking issues** (optional)
   - GitHub issues for each week's deliverables
   - Use for progress tracking

---

## Questions to Resolve

1. **Port allocation:** Should NFS server use fixed port 2049 or dynamic?
   - **Recommendation:** Fixed 2049 (standard NFS port)

2. **HA strategy:** Implement in Phase 1 or defer to Phase 2?
   - **Recommendation:** Defer to Phase 2 (get basic RWX working first)

3. **UDP vs TCP:** Support both or TCP-only?
   - **Recommendation:** TCP-only initially (simpler, modern clients prefer TCP)

4. **Authentication:** Support AUTH_UNIX or just AUTH_NULL?
   - **Recommendation:** AUTH_NULL initially (simpler, K8s provides pod isolation)

---

## Contact & Resources

**RFCs:**
- [RFC 1813 - NFSv3](https://datatracker.ietf.org/doc/html/rfc1813)
- [RFC 4506 - XDR](https://datatracker.ietf.org/doc/html/rfc4506)
- [RFC 5531 - RPC](https://datatracker.ietf.org/doc/html/rfc5531)

**Reference Implementation:**
- [nfsserve](https://github.com/xetdata/nfsserve) - Rust NFSv3 server

**Current Progress:**
- Code location: `/Users/ddalton/projects/rust/flint/spdk-csi-driver/src/nfs/`
- Files created: `mod.rs`, `xdr.rs`, `rpc.rs`, `protocol.rs`
- Lines of code: ~800 / ~2,000 total estimated

---

## Approval & Sign-off

**Ready to proceed?**
- [ ] Architecture approved
- [ ] Timeline acceptable (3-4 weeks)
- [ ] Resource allocation confirmed
- [ ] Begin implementation

**Next command:**
```bash
# Continue with VFS trait implementation
vim /Users/ddalton/projects/rust/flint/spdk-csi-driver/src/nfs/vfs.rs
```
