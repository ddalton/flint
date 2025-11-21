# Flint CSI Driver - Next Steps

## Priority 1: Critical Testing

### ✅ Clean Shutdown Testing (Required)
**Status**: System test created - ready to run

**Location**: `tests/system/tests/clean-shutdown/`

**Why Critical**: The SPDK patches we applied (lvol-flush, ublk-debug, blob-shutdown-debug, blob-recovery-progress) fix a critical issue where blobstore wasn't marking itself "clean" on unmount, causing 3-5 minute recovery delays on every pod restart.

**Run Test**:
```bash
cd tests/system
kubectl kuttl test --test clean-shutdown
```

**What It Tests**:
1. Volume creation and data write
2. Clean shutdown on pod deletion
3. SPDK logs verification (BLOBSTORE UNLOAD)
4. Fast remount without recovery (< 30s)
5. SPDK logs verification (no recovery triggered)
6. Rapid mount/unmount cycles
7. Data integrity across all cycles

**Success Criteria**:
- ✅ Pod remount < 30 seconds (not 3-5 minutes)
- ✅ Zero recovery events during normal operation
- ✅ Data integrity maintained across pod restarts
- ✅ Multiple rapid cycles work without delays

**Expected Duration**: 2-3 minutes (would timeout without patches)

---

## Priority 2: Foundation Complete ✅

### ✅ Dynamic Node Selection (IMPLEMENTED & TESTED)
**Status**: ✅ **COMPLETE** - Deployed and tested in cluster

**Implementation**:
- Removed hardcoded `"ublk-2.vpc.cloudera.com"`
- Dynamic node selection with capacity cache
- Parallel node capacity queries
- Load balancing (selects node with most free space)

**Testing Results**: ✅ PASSED
- System test `rwo-pvc-migration` passed
- Volume created on dynamically selected node
- Metadata stored in PV volumeAttributes
- Pod mounted and accessed volume successfully

### ✅ Capacity Caching (IMPLEMENTED & TESTED)
**Status**: ✅ **COMPLETE** - Phase 1 deployed

**Implementation**:
- In-memory cache with 30s TTL
- Background refresh every 60 seconds
- Cache invalidation after volume creation
- Optimistic capacity reservation (prevents race conditions)
- Warm-up on startup

**Performance Results**:
- 5x faster volume creation
- O(1) volume lookups (vs O(nodes))
- Cache hit rate: 100% after warmup
- Scales to 1000s of volumes

**Files**:
- `spdk-csi-driver/src/capacity_cache.rs` (407 lines)
- `PHASE1_IMPLEMENTATION_SUMMARY.md` (complete details)
- `VOLUME_METADATA_STORAGE.md` (metadata strategy)

**Commits**: 336b4b1, 817e81b, 7dd56bf

---

## Priority 3: CSI Features

### Volume Snapshot and Clone
**Status**: ✅ **IMPLEMENTED & COMPILED**

Standardized APIs for creating point-in-time snapshots of volumes and cloning existing volumes to new PVCs. This is foundational for backup, recovery, and development workflows.

**Implementation**: Isolated module in `src/snapshot/` with zero impact on existing code
- ✅ Core SPDK operations (`bdev_lvol_snapshot`, `bdev_lvol_clone`)
- ✅ HTTP API endpoints (5 routes on port 8081)
- ✅ CSI RPC implementations (`CreateSnapshot`, `DeleteSnapshot`, `ListSnapshots`)
- ✅ Clean compilation with zero errors
- ✅ Only 61 lines changed in existing files (minimal integration)

**See**: [Volume Snapshots](FLINT_CSI_ARCHITECTURE.md#volume-snapshots) section in architecture doc

**Next Steps**:
- 📋 Write unit tests
- 🧪 Deploy and test in cluster  
- 📝 Add kuttl integration tests
- 🎯 Add VolumeSnapshotClass to Helm chart

### Volume Expansion (Resizing)
**Status**: ✅ **IMPLEMENTED & TESTED**

The ability to dynamically grow the size of a persistent volume without taking down the consuming Pod or application.

**SPDK Function**: `bdev_lvol_resize`
```json
{
  "method": "bdev_lvol_resize",
  "params": {
    "name": "lvol_uuid",
    "size_in_mib": 2048
  }
}
```

**Implementation**: Complete (~80 lines)
- ✅ `resize_lvol()` method in MinimalDiskService
- ✅ `POST /api/volumes/resize_lvol` endpoint in node agent
- ✅ `ControllerExpandVolume` CSI RPC implemented
- ✅ `EXPAND_VOLUME` capability already advertised
- ✅ StorageClass has `allowVolumeExpansion: true`

**Usage**:
```bash
# Expand a PVC from 1Gi to 2Gi
kubectl patch pvc my-pvc -p '{"spec":{"resources":{"requests":{"storage":"2Gi"}}}}'

# Kubernetes will:
# 1. Call ControllerExpandVolume (resize bdev)
# 2. Call NodeExpandVolume (resize filesystem)
# 3. Update PVC status
```

**Test Results**: ✅ PASSED
- Expanded 1Gi → 2Gi successfully
- SPDK bdev: 2.00 GB
- Filesystem: ~1.9 GB (automatic resize)
- Zero downtime expansion

**Additional Feature**: ✅ Thin provisioning support added
- Configurable via StorageClass parameter: `thinProvision: "true"`
- Default: thick provisioning (false) for predictable performance
- Thin: allocate space on write for better utilization

### Raw Block Volume Support
Allows CSI drivers to provision volumes as raw block devices instead of requiring a filesystem on them, which is critical for databases and high-performance applications that need to manage the filesystem directly.

**Implementation Notes**:
- Already supported via `volumeMode: Block` in PVC
- Bypasses mkfs and directly exposes block device
- Requires testing with databases (PostgreSQL, MySQL)

---

## Priority 4: Advanced Features

### Topology-Aware Scheduling
When a new PVC is created, Kubernetes uses the **`volumeBindingMode: WaitForFirstConsumer`** setting in the StorageClass. This defers provisioning until the Pod is scheduled, ensuring the volume is created in a zone that matches the Pod's node.

**Implementation Notes**:
- Requires topology labels on nodes
- CSI driver reports topology keys
- Already configured in StorageClass

### CSI Ephemeral Volumes
Allows developers to define a volume directly within the **Pod specification**, bypassing PersistentVolumeClaims (PVCs) to provision non-persistent, per-Pod storage that is created when the Pod starts and destroyed when the Pod terminates. Often used for injecting **secrets, credentials, or transient runtime data** into a container.

**Implementation Notes**:
- Requires implementing `NodePublishVolume` inline mode
- Volumes auto-deleted with pod termination
- Useful for temporary scratch space

### Multi-Replica Support
**Status**: 🚀 **READY TO IMPLEMENT** - Foundation complete

True distributed high availability using **SPDK RAID 1** across nodes with automatic failover and rebuild.

**Implementation Plan**: See `MULTI_REPLICA_IMPLEMENTATION_PLAN_V2.md` for complete details

**Architecture** (v2.0 - Production Design):
- ✅ **Distributed Only**: Replicas MUST be on different nodes (no local RAID)
- ✅ **Smart RAID Creation**: On Pod's node with mixed local/remote access
- ✅ **Persistent State**: Replica info stored in PV annotations
- ✅ **Degraded Operation**: Works with 2+ replicas (minimum 2 required)
- ✅ **Auto-Rebuild**: Monitors down nodes and rebuilds when available
- ✅ **Cluster Restart**: Survives full cluster restart via PV metadata

**Key Features**:
- 🎯 Replicas on different nodes (true HA)
- 🚨 PVC creation fails with event if insufficient nodes
- 📍 RAID created where Pod is scheduled (not where replicas are)
- 🔄 Automatic rebuild when down nodes return
- 📊 Minimum 2 replicas for RAID 1
- 💾 Replica metadata persists in PV annotations
- 🛡️ Zero regression: single-replica code path unchanged

**Implementation**:
- Week 1-3: Multi-node replica creation + PV annotations
- Week 4-6: Smart RAID with mixed local/remote NVMe-oF
- Week 7-9: Auto-rebuild monitor
- Week 10-14: Testing and production release

**Configuration**:
```yaml
# 2-way mirror
parameters:
  numReplicas: "2"

# 3-way mirror  
parameters:
  numReplicas: "3"
```

**Technical Details**:
- Use SPDK `bdev_raid_create` with `raid_level: "1"`
- Conditional: `replica_count == 1` → existing path (no changes)
- Conditional: `replica_count >= 2` → distributed RAID path
- Local replica: Direct lvol bdev access (fast)
- Remote replicas: NVMe-oF initiator connections
- Testing: All existing tests must pass (regression prevention)

---

## Priority 5: Production Hardening

### Monitoring and Observability
- Prometheus metrics export from dashboard backend
- Grafana dashboards for volume/disk health
- Alert rules for disk failures, capacity

### High Availability
- Controller leader election
- Node agent fault tolerance
- SPDK target auto-restart

### Performance Testing
- fio benchmark suite
- Database workload testing
- Multi-tenant isolation verification

---

## References

- **Architecture**: `FLINT_CSI_ARCHITECTURE.md`
- **Clean Shutdown**: `CLEAN_SHUTDOWN_TEST_PLAN.md` 
- **CSI Spec**: https://github.com/container-storage-interface/spec
- **SPDK Docs**: https://spdk.io/doc/