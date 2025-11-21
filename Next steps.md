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

## Priority 2: CSI Features

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

## Priority 3: Advanced Features

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
Distributed replication across multiple nodes for high availability and fault tolerance.

**Implementation Notes**:
- Currently: Multiple independent replicas created manually
- Goal: Transparent replication with automatic failover
- Consider: SPDK bdev_raid for local RAID, or application-level replication

---

## Priority 4: Production Hardening

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