# Multi-Replica Support for Flint CSI Driver

## Overview

This directory contains the comprehensive planning and design documentation for adding **distributed multi-replica support** to the Flint CSI driver using SPDK RAID 1 functionality.

> **✅ Phase 1 Complete**: Foundation is ready!
> - Dynamic node selection implemented and tested
> - Capacity caching operational (5x performance improvement)
> - Metadata storage in PV volumeAttributes working
> - System tests passing
> 
> **🚀 Ready for Multi-Replica**: All prerequisites satisfied

## Documentation Structure

### 📘 [MULTI_REPLICA_IMPLEMENTATION_PLAN_V2.md](./MULTI_REPLICA_IMPLEMENTATION_PLAN_V2.md)
**The Complete Implementation Plan** (12,000+ words)

Comprehensive document covering:
- Executive summary and architecture design
- Distributed RAID 1 (replicas on different nodes only)
- Smart RAID creation on Pod's node with mixed local/remote access
- Detailed implementation plan (6 phases over 15 weeks)
- Code examples with NodePublishVolume, auto-rebuild, volumeAttributes
- Testing strategy and regression prevention
- Risk mitigation and success criteria

**When to read**: For detailed understanding, implementation guidance, or architecture decisions.

### 📋 [MULTI_REPLICA_QUICK_REFERENCE.md](./MULTI_REPLICA_QUICK_REFERENCE.md)
**Quick Reference Guide** (3,500+ words)

Condensed reference covering:
- TL;DR and key design decisions
- Distributed-only approach (replicas on different nodes)
- Smart RAID creation on Pod's node
- volumeAttributes-based persistence
- Degraded operation and auto-rebuild
- Volume creation/attachment/deletion flows
- Configuration examples
- Testing checklist

**When to read**: For quick lookups during implementation, code reviews, or troubleshooting.

### 🗄️ [VOLUME_METADATA_STORAGE.md](./VOLUME_METADATA_STORAGE.md)
**Volume Metadata Storage Strategy**

Explains how volume metadata is stored in PV volumeAttributes:
- Current single-replica behavior (no metadata stored)
- Updated approach using `spec.csi.volumeAttributes`
- Single-replica metadata format
- Multi-replica metadata format
- Implementation code examples
- Backward compatibility strategy

**When to read**: Understanding how replica information persists across cluster restarts.

### ⚡ [SCALABILITY_ANALYSIS.md](./SCALABILITY_ANALYSIS.md)
**Scalability Analysis and Optimization**

Addresses production-scale performance requirements:
- Problem: 1000 PVCs in minutes (current: 10 minutes, need: <2 minutes)
- Bottleneck analysis (node queries, caching, race conditions)
- Capacity caching with TTL
- Parallel volume creation
- Background cache refresh
- Performance comparison (60x improvement)
- Load testing plan

**When to read**: Planning for production deployment at scale.

### 🔧 [FIX_HARDCODED_NODE_PLAN.md](./FIX_HARDCODED_NODE_PLAN.md)
**Critical Bug Fix: Hardcoded Node Name**

Plan to fix hardcoded `"ublk-2.vpc.cloudera.com"` in volume creation:
- Problem analysis
- Dynamic node selection implementation
- Store node metadata in PV volumeAttributes
- Backward compatibility
- Testing plan

**When to read**: Before implementing single-replica improvements.

### 📝 [Next steps.md](./Next%20steps.md)
**Project Roadmap**

Updated roadmap showing multi-replica support as a planned feature alongside other CSI driver improvements.

## Core Design Principles

### Key Architecture Decisions
- ✅ **Distributed Only**: Replicas MUST be on different nodes (no local RAID)
- ✅ **Smart RAID Location**: Created on Pod's scheduled node (not replica nodes)
- ✅ **Mixed Access**: Local replica = direct bdev, remote replicas = NVMe-oF
- ✅ **Persistent Metadata**: Replica info stored in PV volumeAttributes
- ✅ **Minimum 2 Replicas**: RAID 1 requires at least 2 replicas
- ✅ **Degraded Operation**: Works with 2+ replicas even if some nodes down
- ✅ **Auto-Rebuild**: Monitor and add back replicas when nodes return
- ✅ **Insufficient Nodes**: PVC creation fails with event if not enough nodes

### 1. Zero Regression Strategy
```rust
// Existing single-replica code path remains UNCHANGED
if replica_count == 1 {
    return self.create_single_replica_volume(...).await;  // Existing code
}

// New multi-replica code path is completely isolated
if replica_count > 1 {
    return self.create_multi_replica_volume(...).await;   // New RAID code
}
```

**Why**: Ensures existing workloads are completely unaffected by the new feature.

### 2. Single Implementation Phase

**No phased approach**: Implement distributed RAID 1 directly

**Weeks 1-3**: Multi-node replica creation
- Select N nodes with available space (different nodes required)
- Fail PVC with event if insufficient nodes
- Store replica info in PV annotations

**Weeks 4-6**: Smart RAID creation on Pod's node
- Read replica info from PV
- Local replica: direct lvol bdev access
- Remote replicas: NVMe-oF setup and attachment
- Create RAID 1 with mixed base bdevs

**Weeks 7-9**: Auto-rebuild for down nodes
- Background monitor detects node availability
- Add replica back to RAID: `bdev_raid_add_base_bdev`
- SPDK automatic rebuild

**Weeks 10-14**: Testing and release
- Unit, integration, regression tests
- Failure scenarios (node down/up)
- Cluster restart recovery
- Production deployment

### 3. Isolated Module Design

Following the snapshot module pattern - minimal integration with existing code.

**Impact**: 
- New code: ~1,700 lines (isolated module)
- Modified code: < 200 lines (integration points)
- Zero impact on single-replica volumes

## Quick Architecture Comparison

### Current: Single Replica
```
User requests 1Gi PVC (numReplicas: "1")
    ↓
Select 1 disk with space
    ↓
Create 1 lvol (1Gi)
    ↓
Pod scheduled → Expose via ublk
    ↓
Done
```

### New: Distributed RAID 1

**Step 1: Volume Creation** (CSI Controller)
```
User requests 1Gi PVC with numReplicas: "3"
    ↓
Find 3 DIFFERENT nodes with available space
    ↓
Found < 3? → Fail PVC with Event
Found 3? → Continue
    ↓
Node 1: Create lvol_1 (replica 0)
Node 2: Create lvol_2 (replica 1)  
Node 3: Create lvol_3 (replica 2)
    ↓
Store replica info in PV annotations
    ↓
Return PV (PVC becomes Bound)
```

**Step 2: Volume Attachment** (CSI Node, when Pod scheduled)
```
Pod scheduled on Node 2
    ↓
Read replica info from PV annotations
    ↓
Attach replicas:
  Node 2 (local): Use lvol_2 directly
  Node 1 (remote): Setup NVMe-oF → nvme_bdev_1
  Node 3 (remote): Setup NVMe-oF → nvme_bdev_3
    ↓
Create RAID 1 on Node 2:
  base_bdevs: [lvol_2, nvme_bdev_1, nvme_bdev_3]
    ↓
Expose RAID via ublk → /dev/ublkb0
    ↓
Mount and publish to Pod
    ↓
Done (survives 2 node failures with 3 replicas)
```

## SPDK RAID 1 in a Nutshell

### Create RAID 1
```json
{
  "method": "bdev_raid_create",
  "params": {
    "name": "raid_vol_pvc-123",
    "raid_level": "1",
    "base_bdevs": ["lvol_uuid_1", "lvol_uuid_2"]
  }
}
```

### Query Status
```json
{
  "method": "bdev_raid_get_bdevs",
  "params": { "category": "all" }
}
```

Returns:
- `state`: "online", "degraded", "offline"
- `num_base_bdevs_operational`: 2 (healthy) or 1 (degraded)

### Delete RAID 1
```json
{
  "method": "bdev_raid_delete",
  "params": { "name": "raid_vol_pvc-123" }
}
```

## Configuration Examples

### StorageClass: Single Replica (Default)
```yaml
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: flint-csi
provisioner: flint.csi.storage.io
parameters:
  numReplicas: "1"        # Existing behavior
```

### StorageClass: Local RAID 1
```yaml
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: flint-csi-raid1
provisioner: flint.csi.storage.io
parameters:
  numReplicas: "2"
  raidMode: "local"
```

### StorageClass: Distributed RAID 1
```yaml
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: flint-csi-ha
provisioner: flint.csi.storage.io
parameters:
  numReplicas: "2"
  raidMode: "distributed"
```

## Testing Strategy

### 1. Regression Tests (Critical)
**Must Pass**: All existing tests without modification
```bash
kubectl kuttl test --test clean-shutdown
kubectl kuttl test --test volume-expansion
kubectl kuttl test --test snapshot-restore
```

### 2. RAID Functional Tests
**New Test Suite**: `tests/system/tests/multi-replica/`
- Create RAID 1 volume
- Write data, verify integrity
- Check RAID status
- Delete and verify cleanup

### 3. Failure Scenarios
- Simulate disk failure → verify degraded operation
- Simulate node failure → verify continued access
- Performance comparison (single vs RAID)

## Success Criteria

### Must Have (MVP)
- ✅ Zero regressions in single-replica volumes
- ✅ Local RAID 1 works (2 replicas, same node)
- ✅ Volume survives single disk failure
- ✅ All existing tests pass
- ✅ New RAID tests pass

### Should Have
- ✅ Distributed RAID 1 (across nodes)
- ✅ Dashboard shows RAID status
- ✅ Health monitoring
- ✅ Documentation

### Nice to Have (Future)
- ⏭️ Automatic rebuild after disk replacement
- ⏭️ RAID 0 (striping) support
- ⏭️ RAID 5 support

## Timeline

| Phase | Duration | Deliverable |
|-------|----------|-------------|
| **Foundation** | Weeks 1-2 | RAID module structure |
| **Local RAID 1** | Weeks 3-4 | Volume creation with local RAID |
| **Cleanup** | Week 5 | Volume deletion |
| **Testing** | Week 6 | Unit, integration, regression tests |
| **Distributed** | Weeks 7-10 | NVMe-oF-based RAID |
| **Health** | Weeks 11-12 | Monitoring, dashboard |
| **Release** | Weeks 13-15 | Alpha/beta testing, production |

**Total**: 15 weeks

## Getting Started (for Implementers)

### 1. Read the Plan
Start with [MULTI_REPLICA_QUICK_REFERENCE.md](./MULTI_REPLICA_QUICK_REFERENCE.md) for overview, then dive into [MULTI_REPLICA_IMPLEMENTATION_PLAN.md](./MULTI_REPLICA_IMPLEMENTATION_PLAN.md) for details.

### 2. Set Up Branch
```bash
git checkout -b feature/multi-replica
```

### 3. Create Module Structure
```bash
mkdir -p spdk-csi-driver/src/raid
touch spdk-csi-driver/src/raid/{mod.rs,raid_service.rs,raid_models.rs,raid_health.rs}
```

### 4. Follow TDD
Write tests first, then implement:
```bash
# Create test file
touch spdk-csi-driver/src/raid/raid_service_test.rs

# Write failing test
# Implement feature
# Make test pass
```

### 5. Run Regression Tests Frequently
```bash
cd tests/system
kubectl kuttl test --test clean-shutdown
# Ensure existing tests still pass
```

## Risk Mitigation

| Risk | Mitigation |
|------|-----------|
| **Regression** | Conditional logic preserves existing code path unchanged |
| **SPDK stability** | Thorough testing, staged rollout |
| **Complexity** | Isolated module, clear documentation |
| **Performance** | Opt-in feature, benchmark before/after |

## Key Benefits

### For Users
- 🛡️ **High Availability**: Volumes survive disk/node failures
- 🔄 **Transparent Failover**: Automatic, no manual intervention
- 📈 **No Code Changes**: Works with existing applications
- ⚙️ **Configurable**: Choose single replica, local RAID, or distributed RAID

### For Operations
- 📊 **Monitoring**: Dashboard shows RAID health
- 🚨 **Alerting**: Degraded state notifications
- 🔧 **Maintenance**: Replace failed disks without downtime
- 📝 **Simple Config**: StorageClass parameter only

### For Development
- 🧩 **Modular**: Isolated RAID module, easy to test
- 🔒 **Safe**: Zero regression risk via conditional logic
- 📚 **Documented**: Comprehensive guides
- 🧪 **Tested**: Unit, integration, and regression tests

## All Documentation Files

### Multi-Replica Documentation
- 📘 `MULTI_REPLICA_IMPLEMENTATION_PLAN_V2.md` - Complete multi-replica implementation plan
- 📋 `MULTI_REPLICA_QUICK_REFERENCE.md` - Quick reference guide
- 📖 `MULTI_REPLICA_README.md` - This file (documentation index)

### Foundation (Phase 1 - COMPLETE ✅)
- ✅ `PHASE1_IMPLEMENTATION_SUMMARY.md` - Complete summary of implemented features
- 🗄️ `VOLUME_METADATA_STORAGE.md` - Metadata storage in volumeAttributes
- 📝 `Next steps.md` - Complete project roadmap


## External References

### SPDK Documentation
- **RAID Bdev**: `/Users/ddalton/github/spdk/doc/bdev.md` (lines 486-508)
- **RPC Methods**: `/Users/ddalton/github/spdk/doc/jsonrpc.md.jinja2` (lines 10287-10556)

### Flint Documentation
- **Architecture**: [FLINT_CSI_ARCHITECTURE.md](./FLINT_CSI_ARCHITECTURE.md)
- **Current Code**: [spdk-csi-driver/src/driver.rs](./spdk-csi-driver/src/driver.rs)
- **Snapshot Pattern**: [spdk-csi-driver/src/snapshot/](./spdk-csi-driver/src/snapshot/)

## Questions?

### Where do I start?
Read [MULTI_REPLICA_QUICK_REFERENCE.md](./MULTI_REPLICA_QUICK_REFERENCE.md), then review the code examples in [MULTI_REPLICA_IMPLEMENTATION_PLAN_V2.md](./MULTI_REPLICA_IMPLEMENTATION_PLAN_V2.md).

### What about regressions?
The implementation plan includes a comprehensive regression testing strategy. All existing tests must pass without modification.

### How is RAID different from manual replicas?
RAID provides automatic failover, rebuild, and transparent operation. Manual replicas require application-level coordination.

### What about performance?
Local RAID 1 has minimal overhead (~10-20% write penalty). Distributed RAID adds network latency but provides node-level HA.

### Can I help?
Yes! See the implementation checklist in [MULTI_REPLICA_IMPLEMENTATION_PLAN.md](./MULTI_REPLICA_IMPLEMENTATION_PLAN.md) or [MULTI_REPLICA_QUICK_REFERENCE.md](./MULTI_REPLICA_QUICK_REFERENCE.md).

---

**Status**: 📋 Planning Complete - Ready for Implementation
**Created**: November 21, 2025
**Next Step**: Review with team, create GitHub issues, begin Phase 1

## Document History

| Date | Version | Changes |
|------|---------|---------|
| 2025-11-21 | 1.0 | Initial planning documents created |
| 2025-11-21 | 2.0 | Updated to distributed-only design, removed local RAID |
| 2025-11-21 | 2.1 | Updated to use volumeAttributes (not annotations), removed obsolete v1.0 docs |

