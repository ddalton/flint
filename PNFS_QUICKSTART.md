# pNFS Support - Quick Start Guide

This guide provides a quick overview of the pNFS (Parallel NFS) support exploration for Flint.

## What is pNFS?

pNFS (Parallel NFS) is an NFSv4.1+ extension that separates metadata operations from data operations:

- **Metadata Server (MDS)**: Handles control plane (OPEN, GETATTR, layouts)
- **Data Servers (DS)**: Handle data plane (READ, WRITE) for parallel I/O

```
        Client
          |
    ┌─────┴──────┐
    |            |
   MDS          DS-1, DS-2, DS-3
(metadata)     (parallel data I/O)
```

## Current Status

**Phase**: Exploration and Design ✅  
**Implementation**: Stub modules created 🚧  
**Production Ready**: No ❌

## What's Been Done

### 1. Documentation
- [`PNFS_EXPLORATION.md`](PNFS_EXPLORATION.md) - Comprehensive architecture document
- [`config/pnfs.example.yaml`](config/pnfs.example.yaml) - Example configuration file
- This quick start guide

### 2. Module Structure
Created stub modules with clear separation:

```
spdk-csi-driver/src/pnfs/
├── mod.rs              ✅ Module structure
├── config.rs           ✅ Configuration parsing
├── mds/                🚧 Metadata Server
│   ├── server.rs       🚧 Stub implementation
│   ├── device.rs       🚧 Device registry stub
│   ├── layout.rs       🚧 Layout management stub
│   └── operations/     🚧 pNFS operations
└── ds/                 🚧 Data Server
    ├── server.rs       🚧 Stub implementation
    ├── io.rs           🚧 I/O operations stub
    └── registration.rs 🚧 Registration stub
```

### 3. Binary Entry Points
- `flint-pnfs-mds` - Metadata Server binary
- `flint-pnfs-ds` - Data Server binary

### 4. Configuration System
- YAML-based configuration
- Environment variable support
- Three modes: `standalone`, `mds`, `ds`

## Testing the Stubs

### Build
```bash
cd spdk-csi-driver
cargo build --release --bin flint-pnfs-mds
cargo build --release --bin flint-pnfs-ds
```

### Run MDS (stub)
```bash
# With config file
./target/release/flint-pnfs-mds --config ../config/pnfs.example.yaml

# With environment variables
PNFS_MODE=mds ./target/release/flint-pnfs-mds
```

### Run DS (stub)
```bash
# With config file
./target/release/flint-pnfs-ds --config ../config/pnfs.example.yaml

# With environment variables
PNFS_MODE=ds ./target/release/flint-pnfs-ds
```

**Note**: These are stub implementations and will print a message indicating they're not yet functional.

## Configuration Example

```yaml
# Minimal pNFS configuration
apiVersion: flint.io/v1alpha1
kind: PnfsConfig

mode: mds  # or 'ds' or 'standalone'

mds:
  bind:
    address: "0.0.0.0"
    port: 2049
  
  layout:
    type: file
    stripeSize: 8388608  # 8 MiB
    policy: stripe
  
  dataServers:
    - deviceId: ds-01
      endpoint: "10.244.1.10:2049"
      bdevs: [nvme0n1]

  state:
    backend: memory  # or kubernetes, etcd
```

See [`config/pnfs.example.yaml`](config/pnfs.example.yaml) for full configuration options.

## Next Steps

### Immediate (Week 1-2)
1. **Review Architecture** - Read [`PNFS_EXPLORATION.md`](PNFS_EXPLORATION.md)
2. **Decide on Approach**:
   - Configuration format (YAML vs K8s CRD)
   - MDS-DS protocol (gRPC vs NFS RPC)
   - State persistence (memory vs ConfigMap vs etcd)

### Phase 1: Basic MDS (Week 3-6)
- [ ] Implement device registry
- [ ] Implement `LAYOUTGET` operation
- [ ] Implement `GETDEVICEINFO` operation
- [ ] Static layout generation (round-robin)
- [ ] In-memory state

### Phase 2: Basic DS (Week 7-9)
- [ ] Minimal NFS server (READ/WRITE/COMMIT)
- [ ] SPDK bdev integration
- [ ] MDS registration
- [ ] Heartbeat mechanism

### Phase 3: Integration (Week 10-11)
- [ ] MDS + DS communication
- [ ] Layout distribution
- [ ] Client testing with Linux kernel
- [ ] Basic benchmarking

## Key Design Decisions Needed

1. **Configuration Loading**
   - ✅ YAML file support (implemented)
   - ⏳ Environment variables (partial)
   - ❓ Kubernetes CRD (future)

2. **MDS-DS Communication**
   - ❓ gRPC (recommended - better tooling)
   - ❓ NFS RPC (simpler, same protocol)

3. **State Persistence**
   - ✅ In-memory (implemented)
   - ⏳ Kubernetes ConfigMap (planned)
   - ⏳ etcd (HA future)

4. **Layout Type**
   - ✅ FILE layout (Phase 1)
   - ❓ BLOCK layout (future)
   - ❓ OBJECT layout (future)

## Architecture Highlights

### Zero Impact on Existing Code ✅
- Current NFSv4.2 server unchanged
- pNFS is entirely additive
- Separate binaries for MDS and DS
- Can coexist with standalone mode

### Modular Design ✅
- Clear separation of concerns
- Independent MDS and DS modules
- Pluggable configuration
- Pluggable state backends

### Production-Ready Path 🚧
- HA support planned (MDS replication)
- Failure handling (layout recall)
- Monitoring (Prometheus metrics)
- Security (Kerberos, TLS)

## Performance Expectations

With 3 Data Servers:
- **Sequential I/O**: 3x throughput vs standalone
- **Random I/O**: 2-2.5x IOPS vs standalone
- **Concurrent Clients**: 5-10x better scaling

See [PNFS_EXPLORATION.md#appendix-b-performance-expectations](PNFS_EXPLORATION.md#appendix-b-performance-expectations) for details.

## Files Created

### Documentation
- `PNFS_EXPLORATION.md` - Full architecture (17 sections)
- `PNFS_QUICKSTART.md` - This file
- `config/pnfs.example.yaml` - Configuration example

### Source Code
- `src/pnfs/mod.rs` - Module structure
- `src/pnfs/config.rs` - Configuration parsing (working)
- `src/pnfs/mds/` - MDS stubs (6 files)
- `src/pnfs/ds/` - DS stubs (4 files)
- `src/nfs_mds_main.rs` - MDS binary entry point
- `src/nfs_ds_main.rs` - DS binary entry point

### Build System
- Updated `Cargo.toml` - Added pNFS binaries
- Updated `src/lib.rs` - Added pNFS module export

**Total new files**: 15  
**Modified files**: 2 (Cargo.toml, lib.rs)  
**Lines of code**: ~1,500 (mostly documentation and stubs)

## References

### RFCs
- [RFC 5661](https://datatracker.ietf.org/doc/html/rfc5661) - NFSv4.1 with pNFS
- [RFC 8881](https://datatracker.ietf.org/doc/html/rfc8881) - NFSv4.1 (updated)

### Implementation Examples
- Linux kernel: `fs/nfs/pnfs*`
- Ganesha NFS: `src/FSAL/*/pnfs.c`

### Testing Tools
- `nfstest_pnfs` - pNFS conformance
- `fio` - Performance benchmarking

## Questions?

Read the full architecture document: [`PNFS_EXPLORATION.md`](PNFS_EXPLORATION.md)

Key sections:
- Section 2: pNFS Architecture
- Section 3: New Modules Required
- Section 4: Configuration File Format
- Section 5: Protocol Implementation
- Section 9: Development Phases

## Summary

**What exists today:**
- ✅ Comprehensive architecture document
- ✅ Configuration system design
- ✅ Module structure with stubs
- ✅ Binary entry points
- ✅ Example configuration

**What needs implementation:**
- 🚧 Device registry
- 🚧 Layout generation
- 🚧 pNFS operations (LAYOUTGET, etc.)
- 🚧 MDS server logic
- 🚧 DS I/O operations
- 🚧 MDS-DS communication
- 🚧 State persistence
- 🚧 Failure handling

**Estimated time to MVP**: 27 weeks (~6-7 months)

The foundation is laid out. The path forward is clear. Ready to start implementation when you are! 🚀





