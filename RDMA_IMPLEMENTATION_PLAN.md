# RDMA Support Implementation Plan

**Status**: 📋 **PLANNING**  
**Date**: December 2024  
**Estimated Effort**: 4-6 weeks  
**Performance Target**: **5× throughput, 10× lower latency**

---

## Executive Summary

SPDK already has RDMA support for NVMe-oF. We can leverage this infrastructure to add RDMA transport for NFS, providing:
- **Zero-copy I/O** from NVMe to network
- **Kernel bypass** networking
- **5-10 μs latency** (vs 50-100 μs for TCP)
- **100 Gbps+** bandwidth utilization

---

## Current State

### What We Have ✅

1. **SPDK RDMA Libraries** (from Dockerfile):
   ```dockerfile
   libibverbs1      # RDMA verbs API
   librdmacm1       # RDMA connection manager
   rdma-core        # RDMA core utilities
   ```

2. **SPDK NVMe-oF RDMA Transport**:
   - Already supports RDMA for NVMe-oF
   - Has memory registration cache
   - Has poll groups for RDMA completion queues
   - Zero-copy DMA from NVMe to RDMA NIC

3. **Existing TCP Transport**:
   ```rust
   // src/pnfs/mds/server.rs
   async fn serve_tcp(&self, addr: &str) -> Result<()> {
       let listener = TcpListener::bind(addr).await?;
       // Handle connections...
   }
   ```

### What We Need 🔨

1. **RDMA Transport Layer** for NFS RPC
2. **RFC 8267 RPC-over-RDMA** protocol
3. **Integration with SPDK's RDMA infrastructure**
4. **Memory registration for NFS buffers**

---

## Architecture Design

### Option 1: Pure Rust RDMA (Simpler) ⭐ RECOMMENDED

**Use Rust RDMA crates**:
```toml
[dependencies]
rdma = "0.4"           # High-level RDMA API
rdma-sys = "0.6"       # Low-level bindings
async-rdma = "0.4"     # Async RDMA support
```

**Pros**:
- ✅ Pure Rust - type safe
- ✅ Async/await support
- ✅ Easier to integrate with Tokio
- ✅ No FFI complexity

**Cons**:
- ⚠️ Separate from SPDK's RDMA stack
- ⚠️ Can't share memory pools with SPDK
- ⚠️ May require additional memory copies

### Option 2: SPDK RDMA Integration (More Complex)

**Use SPDK's RDMA via FFI**:
```rust
// FFI bindings to SPDK RDMA
extern "C" {
    fn spdk_rdma_qpair_create(...) -> *mut spdk_rdma_qpair;
    fn spdk_rdma_poll_group_create(...) -> *mut spdk_rdma_poll_group;
    fn spdk_rdma_mr_map(...) -> *mut spdk_rdma_mr;
}
```

**Pros**:
- ✅ Shares SPDK's memory pools
- ✅ True zero-copy from NVMe to RDMA
- ✅ Shares poll groups with NVMe-oF
- ✅ Better performance

**Cons**:
- ❌ Complex FFI
- ❌ Unsafe Rust required
- ❌ Harder to maintain
- ❌ SPDK API changes break code

### Recommendation: **Start with Option 1, Migrate to Option 2 if Needed**

**Rationale**:
1. Get RDMA working quickly with pure Rust
2. Measure performance
3. If memory copies are bottleneck, migrate to SPDK integration
4. Most workloads won't notice the difference

---

## Implementation Plan

### Phase 1: RDMA Foundation (Week 1-2)

#### 1.1 Add RDMA Dependencies

```toml
# Cargo.toml
[dependencies]
rdma = "0.4"
rdma-sys = "0.6"
async-rdma = "0.4"

[features]
rdma_transport = ["rdma", "rdma-sys", "async-rdma"]
```

#### 1.2 Create RDMA Transport Module

```rust
// src/nfs/transport/mod.rs (new module)
pub mod tcp;
pub mod rdma;  // ← NEW

pub trait Transport: Send + Sync {
    async fn accept(&self) -> Result<Box<dyn Connection>>;
}

pub trait Connection: Send + Sync {
    async fn recv(&mut self) -> Result<Bytes>;
    async fn send(&mut self, data: &[u8]) -> Result<()>;
}
```

#### 1.3 Implement Basic RDMA Transport

```rust
// src/nfs/transport/rdma.rs
use rdma::{RdmaBuilder, Rdma};
use async_rdma::{RdmaListener, RdmaStream};

pub struct RdmaTransport {
    listener: RdmaListener,
    mr_cache: MemoryRegionCache,
}

impl RdmaTransport {
    pub async fn new(addr: &str) -> Result<Self> {
        let listener = RdmaListener::bind(addr).await?;
        let mr_cache = MemoryRegionCache::new();
        
        Ok(Self { listener, mr_cache })
    }
}

impl Transport for RdmaTransport {
    async fn accept(&self) -> Result<Box<dyn Connection>> {
        let stream = self.listener.accept().await?;
        Ok(Box::new(RdmaConnection::new(stream)))
    }
}
```

#### 1.4 Memory Registration Cache

```rust
// src/nfs/transport/rdma/mr_cache.rs
use rdma::MemoryRegion;
use dashmap::DashMap;

pub struct MemoryRegionCache {
    cache: DashMap<usize, Arc<MemoryRegion>>,
}

impl MemoryRegionCache {
    pub fn register(&self, buffer: &[u8]) -> Arc<MemoryRegion> {
        let key = buffer.as_ptr() as usize;
        
        self.cache.entry(key).or_insert_with(|| {
            Arc::new(MemoryRegion::new(buffer).unwrap())
        }).clone()
    }
}
```

### Phase 2: RPC-over-RDMA (Week 3-4)

#### 2.1 RFC 8267 Protocol Structures

```rust
// src/nfs/transport/rdma/rpc_rdma.rs

/// RPC-RDMA message types (RFC 8267 Section 4)
#[repr(u32)]
pub enum RpcRdmaProc {
    Msg = 0,        // Regular RPC message
    Nomsg = 1,      // RPC with RDMA chunks
    Msgp = 2,       // RPC with padding
    Done = 3,       // Completion message
    Error = 4,      // Error message
}

/// RDMA chunk (RFC 8267 Section 4.3)
pub struct RdmaSegment {
    pub handle: u32,    // Memory region handle
    pub length: u32,    // Segment length
    pub offset: u64,    // Offset in memory region
}

/// Read chunk list (for READ operations)
pub struct RdmaReadChunk {
    pub position: u32,          // Position in XDR stream
    pub target: Vec<RdmaSegment>,
}

/// Write chunk list (for WRITE operations)
pub struct RdmaWriteChunk {
    pub target: Vec<RdmaSegment>,
}
```

#### 2.2 RPC-RDMA Message Encoding

```rust
// src/nfs/transport/rdma/encoding.rs

pub struct RpcRdmaMessage {
    pub xid: u32,
    pub vers: u32,
    pub credit: u32,
    pub body_type: RpcRdmaProc,
    pub read_chunks: Vec<RdmaReadChunk>,
    pub write_chunks: Vec<RdmaWriteChunk>,
    pub reply_chunk: Option<RdmaWriteChunk>,
}

impl RpcRdmaMessage {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        
        // XDR encode header
        buf.extend_from_slice(&self.xid.to_be_bytes());
        buf.extend_from_slice(&self.vers.to_be_bytes());
        buf.extend_from_slice(&self.credit.to_be_bytes());
        buf.extend_from_slice(&(self.body_type as u32).to_be_bytes());
        
        // Encode chunk lists
        self.encode_read_chunks(&mut buf);
        self.encode_write_chunks(&mut buf);
        self.encode_reply_chunk(&mut buf);
        
        buf
    }
}
```

#### 2.3 Large Transfer Optimization

```rust
// For large READ/WRITE operations, use RDMA READ/WRITE
pub async fn handle_large_read(
    &self,
    file: &File,
    offset: u64,
    count: u32,
) -> Result<Bytes> {
    // Allocate buffer and register with RDMA
    let buffer = vec![0u8; count as usize];
    let mr = self.mr_cache.register(&buffer);
    
    // Read from file into registered memory
    file.read_at(&mut buffer, offset).await?;
    
    // Return RDMA chunk descriptor (client will RDMA READ)
    Ok(RdmaChunk {
        handle: mr.rkey(),
        length: count,
        offset: 0,
    }.encode())
}
```

### Phase 3: NFS Integration (Week 5-6)

#### 3.1 Dual Transport Support

```rust
// src/pnfs/mds/server.rs

pub async fn serve(&self) -> Result<()> {
    // Spawn TCP listener
    let tcp_task = tokio::spawn(self.serve_tcp("0.0.0.0:2049"));
    
    // Spawn RDMA listener (if enabled)
    let rdma_task = if self.config.rdma_enabled {
        Some(tokio::spawn(self.serve_rdma("0.0.0.0:20049")))
    } else {
        None
    };
    
    // Wait for both
    tokio::try_join!(tcp_task, rdma_task.unwrap_or(...))?;
    
    Ok(())
}

async fn serve_rdma(&self, addr: &str) -> Result<()> {
    let transport = RdmaTransport::new(addr).await?;
    
    loop {
        let conn = transport.accept().await?;
        let dispatcher = self.dispatcher.clone();
        
        tokio::spawn(async move {
            handle_rdma_connection(conn, dispatcher).await
        });
    }
}
```

#### 3.2 Configuration

```yaml
# config/pnfs.example.yaml
mds:
  bind:
    tcp:
      address: "0.0.0.0"
      port: 2049
    rdma:
      enabled: true
      address: "0.0.0.0"
      port: 20049
      device: "mlx5_0"  # RDMA device name
      max_inline: 1024  # Inline data threshold
      
performance:
  rdma:
    qp_depth: 128      # Queue pair depth
    cq_depth: 256      # Completion queue depth
    mr_cache_size: 1024 # Memory region cache entries
```

#### 3.3 Feature Detection

```rust
// Detect RDMA hardware at startup
pub fn detect_rdma_devices() -> Vec<RdmaDevice> {
    let output = Command::new("rdma")
        .arg("link")
        .arg("show")
        .output()
        .ok()?;
    
    // Parse output to find RDMA devices
    parse_rdma_devices(&output.stdout)
}

// Advertise RDMA in NFS responses
pub fn get_fs_locations(&self) -> FsLocations {
    FsLocations {
        servers: vec![
            FsServer {
                address: self.tcp_addr,
                protocol: "tcp",
            },
            FsServer {
                address: self.rdma_addr,
                protocol: "rdma",  // ← Client can choose
            },
        ],
    }
}
```

---

## Testing Strategy

### Unit Tests

```rust
#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn test_rdma_connection() {
        let transport = RdmaTransport::new("127.0.0.1:20049").await.unwrap();
        // Test basic send/recv
    }
    
    #[tokio::test]
    async fn test_rpc_rdma_encoding() {
        let msg = RpcRdmaMessage { ... };
        let encoded = msg.encode();
        let decoded = RpcRdmaMessage::decode(&encoded).unwrap();
        assert_eq!(msg, decoded);
    }
    
    #[tokio::test]
    async fn test_memory_registration() {
        let cache = MemoryRegionCache::new();
        let buffer = vec![0u8; 4096];
        let mr = cache.register(&buffer);
        assert!(mr.rkey() != 0);
    }
}
```

### Integration Tests

```bash
# 1. Check RDMA hardware
rdma link show

# 2. Start NFS server with RDMA
./nfs-server --rdma 0.0.0.0:20049

# 3. Mount with RDMA
mount -t nfs -o vers=4.2,proto=rdma server:/ /mnt/test

# 4. Verify RDMA is being used
rdma resource show qp  # Should show active queue pairs

# 5. Run I/O tests
fio --name=rdma-test --filename=/mnt/test/file \
    --direct=1 --rw=read --bs=1M --size=10G
```

### Performance Benchmarks

```bash
# Baseline (TCP)
mount -t nfs -o vers=4.2,proto=tcp server:/ /mnt/tcp
fio --name=tcp --filename=/mnt/tcp/file --direct=1 \
    --rw=read --bs=1M --size=10G --numjobs=4

# RDMA
mount -t nfs -o vers=4.2,proto=rdma server:/ /mnt/rdma
fio --name=rdma --filename=/mnt/rdma/file --direct=1 \
    --rw=read --bs=1M --size=10G --numjobs=4

# Compare:
# - Throughput (MB/s)
# - Latency (μs)
# - CPU usage (%)
```

---

## Hardware Requirements

### Minimum

- **RDMA-capable NIC**: Mellanox ConnectX-4 or newer
- **Network**: RoCE v2 capable switch
- **Driver**: MLNX_OFED or inbox drivers
- **Bandwidth**: 25 Gbps+ to see benefits

### Optimal

- **NIC**: Mellanox ConnectX-6 or newer (100 Gbps)
- **Network**: RoCE v2 with PFC (Priority Flow Control)
- **NVMe**: 7 GB/s+ per disk
- **Bandwidth**: 100 Gbps+

### Verification Commands

```bash
# Check RDMA devices
rdma link show

# Check device capabilities
ibv_devinfo

# Check RoCE version
rdma link show mlx5_0/1 | grep link_layer

# Test RDMA bandwidth
ib_send_bw -d mlx5_0 -i 1 server_ip
ib_read_bw -d mlx5_0 -i 1 server_ip
```

---

## Performance Expectations

### Throughput

| Workload | TCP | RDMA | Improvement |
|----------|-----|------|-------------|
| Sequential READ (1M) | 20 GB/s | 95 GB/s | **4.75×** |
| Sequential WRITE (1M) | 18 GB/s | 90 GB/s | **5×** |
| Random READ (4K) | 500K IOPS | 800K IOPS | **1.6×** |
| Random WRITE (4K) | 450K IOPS | 750K IOPS | **1.67×** |

### Latency

| Operation | TCP | RDMA | Improvement |
|-----------|-----|------|-------------|
| READ (4K) | 50 μs | 5 μs | **10×** |
| WRITE (4K) | 80 μs | 8 μs | **10×** |
| GETATTR | 100 μs | 15 μs | **6.7×** |

### CPU Usage

| Workload | TCP | RDMA | Improvement |
|----------|-----|------|-------------|
| 100 Gbps throughput | 40% | 8% | **5× lower** |
| 1M IOPS | 60% | 15% | **4× lower** |

---

## Risks & Mitigation

### Risk 1: Hardware Availability

**Risk**: Not all environments have RDMA hardware  
**Mitigation**: Make RDMA optional, fall back to TCP

### Risk 2: Complexity

**Risk**: RDMA is complex, bugs are hard to debug  
**Mitigation**: Start with pure Rust, extensive testing

### Risk 3: Performance May Not Justify Effort

**Risk**: 4-6 weeks of work for marginal gains  
**Mitigation**: Measure TCP bottleneck first, only implement if needed

### Risk 4: Kernel Bypass Issues

**Risk**: RDMA bypasses kernel, may have security implications  
**Mitigation**: Use standard RDMA security (Pkeys, isolation)

---

## Rollout Plan

### Phase 0: Validation (Week 0)

- ✅ Verify RDMA hardware exists
- ✅ Measure TCP baseline performance
- ✅ Confirm TCP is bottleneck
- ✅ Get approval for 4-6 week project

### Phase 1: Foundation (Week 1-2)

- 🔨 Add RDMA dependencies
- 🔨 Create transport abstraction
- 🔨 Implement basic RDMA transport
- 🔨 Test basic send/recv

### Phase 2: Protocol (Week 3-4)

- 🔨 Implement RFC 8267 encoding
- 🔨 Add chunk list support
- 🔨 Implement large transfer optimization
- 🔨 Test with NFS client

### Phase 3: Integration (Week 5-6)

- 🔨 Integrate with NFS server
- 🔨 Add configuration support
- 🔨 Performance tuning
- 🔨 Documentation

### Phase 4: Production (Week 7+)

- 🔨 Beta testing
- 🔨 Performance validation
- 🔨 Bug fixes
- 🔨 Production deployment

---

## Success Criteria

### Functional

- ✅ NFS client can mount over RDMA
- ✅ All NFS operations work correctly
- ✅ Failover to TCP if RDMA fails
- ✅ No data corruption

### Performance

- ✅ **5× throughput** improvement over TCP
- ✅ **10× latency** reduction
- ✅ **4× lower CPU** usage
- ✅ Saturate 100 Gbps network

### Operational

- ✅ Easy to configure
- ✅ Clear error messages
- ✅ Monitoring/metrics
- ✅ Documentation

---

## Next Steps

1. ✅ **Read Delegations** - COMPLETE
2. 🔄 **Hardware Assessment** - Check if RDMA available
3. 🔄 **TCP Baseline** - Measure current performance
4. 🔄 **Go/No-Go Decision** - Is RDMA worth it?
5. ⏳ **RDMA Implementation** - If approved, start Phase 1

---

**Document Version**: 1.0  
**Last Updated**: December 2024  
**Status**: Awaiting hardware assessment and go/no-go decision

