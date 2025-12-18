# pNFS (Parallel NFS) Support for Flint

## Overview

This directory contains the exploration, design, and initial implementation of pNFS (Parallel NFS) support for the Flint CSI driver's NFS server.

**pNFS** is an NFSv4.1+ extension that enables true parallel I/O by separating metadata operations from data operations, providing 2-3x performance improvements for workloads with multiple data servers.

## Status

- **Phase**: Exploration & Design Complete ✅
- **Implementation**: Stub modules created 🚧
- **Production Ready**: No ❌

## Documentation

### Quick Start
- **[PNFS_QUICKSTART.md](PNFS_QUICKSTART.md)** - Start here! Quick overview and getting started guide

### Comprehensive Documentation
- **[PNFS_RFC_GUIDE.md](PNFS_RFC_GUIDE.md)** ⭐ RFC implementation guide
  - [RFC 8881 (NFSv4.1)](https://datatracker.ietf.org/doc/html/rfc8881) - Primary pNFS specification
  - [RFC 7862 (NFSv4.2)](https://datatracker.ietf.org/doc/html/rfc7862) - Performance operations
  - Key sections and XDR definitions
  - Protocol flow examples
  - Implementation checklist by RFC section

- **[PNFS_EXPLORATION.md](PNFS_EXPLORATION.md)** - Full architecture document (17 sections, ~1000 lines)
  - Current architecture analysis
  - pNFS protocol requirements
  - Module structure and implementation details
  - Configuration system design
  - Development phases (27 weeks estimated)
  - Testing strategy
  - Performance expectations

### Visual Guides
- **[PNFS_ARCHITECTURE_DIAGRAM.md](PNFS_ARCHITECTURE_DIAGRAM.md)** - ASCII architecture diagrams
  - High-level architecture
  - Protocol flow diagrams
  - Module structure
  - MDS and DS internals
  - Kubernetes deployment
  - State persistence options

### Summary
- **[PNFS_SUMMARY.md](PNFS_SUMMARY.md)** - Executive summary of findings and deliverables

### Configuration
- **[config/pnfs.example.yaml](config/pnfs.example.yaml)** - Fully documented configuration example

## What is pNFS?

```
Traditional NFS:                pNFS:
                               
Client                         Client
  │                              │
  │ All Operations               │ Metadata    Data I/O
  ▼                              ▼             ▼
Server                         MDS           DS-1, DS-2, DS-3
  │                              │             │
  ▼                              ▼             ▼
Storage                        State         SPDK Bdevs

Result: Single bottleneck      Result: Parallel I/O (3x faster)
```

**Key Benefits:**
- 2-3x throughput with multiple data servers
- Better scaling with concurrent clients
- Reduced metadata server bottleneck
- Standard NFSv4.1 protocol (no client changes)

## Quick Reference

### Build
```bash
cd spdk-csi-driver
cargo build --release --bin flint-pnfs-mds
cargo build --release --bin flint-pnfs-ds
```

### Run (Stubs)
```bash
# Metadata Server
./target/release/flint-pnfs-mds --config ../config/pnfs.example.yaml

# Data Server
./target/release/flint-pnfs-ds --config ../config/pnfs.example.yaml
```

**Note**: These are stub implementations. See implementation roadmap below.

### Configuration Example
```yaml
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
```

See [config/pnfs.example.yaml](config/pnfs.example.yaml) for complete configuration.

## Implementation Roadmap

### Phase 0: Design & Setup ✅ COMPLETE (2 weeks)
- [x] Architecture exploration
- [x] Configuration system design
- [x] Module structure
- [x] Stub implementations
- [x] Documentation

### Phase 1: Basic MDS (4 weeks)
- [ ] Device registry implementation
- [ ] LAYOUTGET operation
- [ ] GETDEVICEINFO operation
- [ ] Round-robin layout policy
- [ ] In-memory state

### Phase 2: Basic DS (3 weeks)
- [ ] Minimal NFS server (READ/WRITE/COMMIT)
- [ ] SPDK bdev integration
- [ ] MDS registration
- [ ] Heartbeat mechanism

### Phase 3: Integration (2 weeks)
- [ ] MDS + DS communication
- [ ] Linux kernel client testing
- [ ] Basic benchmarking

### Phase 4-8: Advanced Features (16 weeks)
- [ ] Multi-DS striping
- [ ] State persistence (ConfigMap, etcd)
- [ ] Failure handling and recovery
- [ ] HA with MDS replication
- [ ] Production hardening

**Total Estimated Time**: 27 weeks (~6-7 months)

## Module Structure

```
spdk-csi-driver/src/
├── nfs/                    ← EXISTING (unchanged)
│   └── ...                   Standalone NFSv4.2 server
│
├── pnfs/                   ← NEW (additive only)
│   ├── mod.rs                Module structure
│   ├── config.rs             Configuration parsing ✅
│   ├── mds/                  Metadata Server
│   │   ├── server.rs         MDS main loop
│   │   ├── device.rs         Device registry
│   │   ├── layout.rs         Layout generation
│   │   └── operations/       pNFS operations
│   └── ds/                   Data Server
│       ├── server.rs         DS main loop
│       ├── io.rs             I/O operations
│       └── registration.rs   MDS registration
│
├── nfs_main.rs             ← EXISTING
├── nfs_mds_main.rs         ← NEW: MDS binary
└── nfs_ds_main.rs          ← NEW: DS binary
```

## Key Design Decisions

### Zero Impact on Existing Code ✅
- Current NFSv4.2 server completely unchanged
- pNFS is entirely additive
- Separate binaries for MDS and DS
- Can coexist with standalone mode

### Configuration-Driven ✅
- YAML file (primary)
- Environment variables (fallback)
- Kubernetes CRD (future)

### Modular Architecture ✅
- Clear separation: MDS vs DS
- Pluggable state backends (memory, ConfigMap, etcd)
- Pluggable layout policies (stripe, round-robin, locality)

### Production-Ready Path 🚧
- HA support (MDS replication)
- Failure handling (layout recall)
- Monitoring (Prometheus)
- Security (Kerberos, TLS)

## Performance Expectations

With 3 Data Servers:
- **Sequential I/O**: 3x throughput (3 GB/s vs 1 GB/s)
- **Random I/O**: 2-2.5x IOPS (100-125K vs 50K)
- **Concurrent Clients**: 5-10x better scaling

See [PNFS_EXPLORATION.md#appendix-b-performance-expectations](PNFS_EXPLORATION.md#appendix-b-performance-expectations) for details.

## Files Created

### Documentation (6 files)
- `PNFS_README.md` - This file
- `PNFS_QUICKSTART.md` - Quick start guide
- `PNFS_RFC_GUIDE.md` - RFC implementation guide ⭐ NEW
- `PNFS_EXPLORATION.md` - Full architecture (17 sections)
- `PNFS_SUMMARY.md` - Executive summary
- `PNFS_ARCHITECTURE_DIAGRAM.md` - Visual diagrams
- `config/pnfs.example.yaml` - Configuration example

### Source Code (13 files)
- `src/pnfs/mod.rs` - Module structure
- `src/pnfs/config.rs` - Configuration parsing (WORKING)
- `src/pnfs/mds/*.rs` - MDS stubs (5 files)
- `src/pnfs/ds/*.rs` - DS stubs (4 files)
- `src/nfs_mds_main.rs` - MDS binary entry point
- `src/nfs_ds_main.rs` - DS binary entry point

### Build System (2 files modified)
- `Cargo.toml` - Added pNFS binaries
- `src/lib.rs` - Added pNFS module export

**Total**: 19 new files, 2 modified files, ~2500 lines (mostly documentation)

## Next Steps

1. **Review Documentation**
   - Read [PNFS_QUICKSTART.md](PNFS_QUICKSTART.md) for overview
   - Read [PNFS_EXPLORATION.md](PNFS_EXPLORATION.md) for details

2. **Make Decisions**
   - MDS-DS protocol: gRPC vs NFS RPC?
   - State persistence: ConfigMap vs etcd?
   - Timeline: Full implementation vs MVP?

3. **Start Implementation** (when ready)
   - Phase 1: Basic MDS (4 weeks)
   - Phase 2: Basic DS (3 weeks)
   - Phase 3: Integration (2 weeks)

## Open Questions

1. **MDS-DS Communication**
   - gRPC (recommended) vs NFS RPC (simpler)?

2. **State Persistence**
   - Start with ConfigMap or go straight to etcd?

3. **Timeline**
   - Full 27-week implementation or phased MVP?

4. **Deployment**
   - Kubernetes-only or support bare metal?

5. **Feature Scope**
   - FILE layout only or also BLOCK/OBJECT?

## References

### RFCs
- **[RFC 8881](https://datatracker.ietf.org/doc/html/rfc8881)** ⭐ NFSv4.1 (Primary - includes pNFS spec)
- **[RFC 7862](https://datatracker.ietf.org/doc/html/rfc7862)** ⭐ NFSv4.2 (Performance operations)
- [RFC 5661](https://datatracker.ietf.org/doc/html/rfc5661) - NFSv4.1 (Historical, obsoleted by RFC 8881)
- [RFC 8434](https://datatracker.ietf.org/doc/html/rfc8434) - pNFS Layout Types Requirements
- [RFC 5663](https://datatracker.ietf.org/doc/html/rfc5663) - pNFS Block/Volume Layout
- [RFC 5664](https://datatracker.ietf.org/doc/html/rfc5664) - pNFS Object-Based Layout

See **[PNFS_RFC_GUIDE.md](PNFS_RFC_GUIDE.md)** for detailed section-by-section implementation guide.

### Implementation Examples
- Linux kernel: `fs/nfs/pnfs*`
- Ganesha NFS: `src/FSAL/*/pnfs.c`

### Testing Tools
- `nfstest_pnfs` - pNFS conformance testing
- `fio` - Performance benchmarking

## Contributing

This is currently in the exploration phase. Implementation will begin after:
1. Review of architecture documents
2. Approval of design decisions
3. Resource allocation

## License

Same as Flint CSI driver.

## Contact

For questions or discussions about pNFS support, please refer to the main Flint project.

---

**Status**: Phase 0 Complete ✅  
**Next**: Await decision on Phase 1 kickoff  
**Estimated MVP**: 12-16 weeks (Phases 1-5)  
**Estimated Full**: 27 weeks (All phases)

🚀 Ready for implementation when you are!

