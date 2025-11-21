# Multi-Replica Support - Quick Reference

## TL;DR

**Goal**: Add **distributed multi-replica support** using SPDK RAID 1 for true high availability across nodes.

**Strategy**: Conditional code path based on `replica_count`:
- `replica_count == 1` → Existing code (zero changes)
- `replica_count >= 2` → New distributed RAID 1 code path

**Key Principles**:
- ✅ Replicas MUST be on different nodes
- ✅ RAID created on Pod's node with mixed local/remote access
- ✅ Replica info stored in PV annotations (survives cluster restart)
- ✅ Degraded operation with 2+ replicas (minimum 2 required)
- ✅ Auto-rebuild when down nodes return

**Timeline**: 15 weeks (distributed RAID only, no local RAID phase)

## Key Decisions

### 1. Use SPDK RAID 1 (Not Application-Level Replication)
**Why**: 
- Native SPDK support
- Hardware-accelerated
- Transparent to applications
- Automatic failover and rebuild

### 2. Distributed RAID 1 Only (No Local RAID)
**Design**: Replicas MUST be on different nodes
- Protects against node failures (not just disk failures)
- True distributed high availability
- Industry standard approach for HA storage
- Simpler implementation (one code path)

### 3. Smart RAID Creation on Pod's Node
**Key Insight**: RAID bdev is created where the Pod runs, not where replicas live
- Local replica: Direct lvol bdev access (fast)
- Remote replicas: NVMe-oF initiator connection (network)
- Mixed access in single RAID 1 bdev
- Optimal performance for local replica

### 4. Static Replicas with PV Metadata
**Design**: Replica locations stored in PersistentVolume annotations
- Survives cluster restarts
- No external database needed
- Kubernetes-native approach
- Replica info immutable after creation

### 5. Degraded Operation Support
**Design**: RAID can operate with fewer replicas than created
- Minimum 2 replicas required to create RAID 1
- Can continue with 2+ replicas even if some nodes down
- Background monitor adds replicas back when nodes return
- Automatic rebuild when replica rejoins

### 6. Zero Regression via Conditional Logic
**Code Pattern**:
```rust
pub async fn create_volume(..., replica_count: u32, ...) {
    if replica_count == 1 {
        // Existing single-replica code - UNCHANGED
        return self.create_single_replica_volume(...).await;
    }
    
    if replica_count > 1 {
        // NEW: RAID code path
        return self.create_multi_replica_volume(...).await;
    }
}
```

### 7. Isolated Implementation
Following the snapshot module pattern:
```
src/raid/
├── mod.rs              # Module definition
├── raid_service.rs     # RAID operations
├── raid_models.rs      # Data structures
└── raid_health.rs      # Health monitoring
```

Minimal integration (< 100 lines of changes to existing files).

## SPDK RAID 1 APIs

### Create RAID 1 Bdev
```json
{
  "method": "bdev_raid_create",
  "params": {
    "name": "raid_vol_pvc-abc123",
    "raid_level": "1",
    "base_bdevs": [
      "lvol_uuid_1",
      "lvol_uuid_2"
    ]
  }
}
```

### Query RAID Status
```json
{
  "method": "bdev_raid_get_bdevs",
  "params": {
    "category": "all"
  }
}
```

Response includes:
- `raid_level`: "raid1"
- `num_base_bdevs`: 2
- `num_base_bdevs_operational`: 2 (or 1 if degraded)
- `state`: "online", "degraded", or "offline"

### Delete RAID Bdev
```json
{
  "method": "bdev_raid_delete",
  "params": {
    "name": "raid_vol_pvc-abc123"
  }
}
```

## Volume Creation Flow (Distributed RAID 1)

```
1. User creates PVC with numReplicas: "3"
   ↓
2. CSI Controller: Find 3 DIFFERENT nodes with available space
   ↓
3. Found 3 nodes? NO → Fail PVC with Event
   ↓
4. Found 3 nodes? YES → Continue
   ↓
5. Create lvol on each node:
   Node 1: Create lvol_1 (replica 0)
   Node 2: Create lvol_2 (replica 1)
   Node 3: Create lvol_3 (replica 2)
   ↓
6. Store replica info in PV annotations:
   {
     "replicas": [
       {"node": "node1", "lvol_uuid": "...", ...},
       {"node": "node2", "lvol_uuid": "...", ...},
       {"node": "node3", "lvol_uuid": "...", ...}
     ]
   }
   ↓
7. Return PV to Kubernetes (PVC becomes Bound)
```

## Volume Attachment Flow (Pod Scheduled)

```
1. Pod scheduled on Node 2
   ↓
2. CSI Node: Read replica info from PV annotations
   Replicas: [node1, node2, node3]
   ↓
3. For each replica:
   Replica on Node 2 (LOCAL):
     → Use lvol bdev directly (lvol_uuid_2)
   Replica on Node 1 (REMOTE):
     → Setup NVMe-oF target on Node 1
     → Attach from Node 2 → nvme_bdev_1
   Replica on Node 3 (REMOTE):
     → Setup NVMe-oF target on Node 3
     → Attach from Node 2 → nvme_bdev_3
   ↓
4. Create RAID 1 bdev on Node 2:
   bdev_raid_create(
     name: "raid_pvc-abc123",
     raid_level: "1",
     base_bdevs: [lvol_uuid_2, nvme_bdev_1, nvme_bdev_3]
   )
   ↓
5. Expose RAID bdev via ublk → /dev/ublkb0
   ↓
6. Mount filesystem and publish to Pod
```

## Volume Deletion Flow

```
1. CSI Controller delete_volume(volume_id)
   ↓
2. Read replica info from PV annotations
   ↓
3. If multi-replica:
     ↓
   For each replica:
     a. Delete lvol on replica's node
     b. Delete NVMe-oF target (if exists)
   ↓
4. If single replica:
     ↓
   Use existing deletion code (unchanged)

Note: RAID bdev is deleted automatically during NodeUnpublishVolume
(when Pod is deleted), not during DeleteVolume
```

## Key Features

### 1. PVC Event on Insufficient Nodes

If CSI controller cannot find enough nodes:

```
Event on PVC:
  Type: Warning
  Reason: InsufficientNodes
  Message: Cannot create volume with 3 replicas: only 2 nodes have sufficient space (10GB required per node)
```

### 2. Degraded Operation

RAID 1 continues to work with 2+ replicas:

```
Created with 3 replicas, 1 node goes down:
  Status: DEGRADED (2/3 replicas operational)
  I/O: Continues normally
  Background: Monitor waits for node to return
```

### 3. Automatic Rebuild

When down node returns:

```
1. Background monitor detects node is back
2. Setup NVMe-oF target for that replica
3. Attach to current RAID bdev: bdev_raid_add_base_bdev
4. SPDK automatically rebuilds the replica
5. Status: ONLINE (3/3 replicas operational)
```

### 4. Cluster Restart Recovery

After cluster restart:

```
1. Pods rescheduled (may be on different nodes)
2. CSI Node reads replica info from PV annotations
3. Recreates RAID bdev with available replicas
4. Continues in degraded mode if some nodes not up yet
5. Rebuilds when all nodes are back
```

## StorageClass Configuration

### Single Replica (Default - Existing Behavior)
```yaml
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: flint-csi
provisioner: flint.csi.storage.io
parameters:
  numReplicas: "1"        # Default - existing path
  thinProvision: "false"
```

### 2-Way Mirror (High Availability)
```yaml
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: flint-csi-ha
provisioner: flint.csi.storage.io
parameters:
  numReplicas: "2"        # Minimum for RAID 1
  thinProvision: "false"
# Replicas automatically placed on different nodes
```

### 3-Way Mirror (Maximum Availability)
```yaml
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: flint-csi-ha-3way
provisioner: flint.csi.storage.io
parameters:
  numReplicas: "3"        # Can survive 2 node failures (degraded with 1)
  thinProvision: "false"
# Replicas automatically placed on different nodes
```

**Note**: No `raidMode` parameter - all multi-replica volumes are distributed across nodes

## Failure Scenarios

### Local RAID 1: One Disk Fails
```
Status: DEGRADED
- RAID bdev remains accessible (reads/writes continue)
- num_base_bdevs_operational: 1 (was 2)
- Performance: Reads/writes use remaining disk
- Action: Replace failed disk, trigger rebuild (future work)
```

### Distributed RAID 1: One Node Fails
```
Status: DEGRADED
- RAID bdev remains accessible via remaining node
- NVMe-oF connection to failed node times out
- Reads/writes redirected to operational replica
- Action: Node recovery triggers reconnection
```

### Both Replicas Fail
```
Status: OFFLINE
- Volume becomes inaccessible
- Pod enters CrashLoopBackOff
- Action: Manual intervention required
```

## Testing Strategy

### 1. Regression Tests (Critical)
Run ALL existing tests to ensure zero regressions:
```bash
cd tests/system
kubectl kuttl test --test clean-shutdown
kubectl kuttl test --test volume-expansion  
kubectl kuttl test --test snapshot-restore
kubectl kuttl test --test rwo-pvc-migration
kubectl kuttl test --test rwx-multi-pod
```

**Success Criteria**: All tests pass without modifications.

### 2. RAID Functional Tests
New test suite: `tests/system/tests/multi-replica/`
- Create RAID 1 volume (numReplicas=2)
- Write data, verify integrity
- Check RAID status (online)
- Delete volume, verify cleanup

### 3. Failure Simulation
- Stop one disk, verify degraded operation
- Restart disk, verify recovery
- Performance testing (local vs distributed)

## Performance Expectations

### Local RAID 1
- **Read**: Same as single disk (read from one mirror)
- **Write**: Slightly slower (write to both mirrors, ~10-20% overhead)
- **Latency**: Single-digit microsecond overhead

### Distributed RAID 1
- **Read**: Add network RTT (~100-500µs depending on fabric)
- **Write**: Add network RTT for remote replica
- **Trade-off**: Availability vs latency (worth it for HA)

## Code Changes Summary

### New Files (~1500 lines)
- `src/raid/mod.rs` (10 lines)
- `src/raid/raid_service.rs` (400 lines)
- `src/raid/raid_models.rs` (150 lines)
- `src/raid/raid_health.rs` (200 lines)
- Test files (500 lines)
- Documentation (240 lines)

### Modified Files (< 200 lines total)
- `src/driver.rs` (+150 lines) - Add RAID creation/deletion logic
- `src/node_agent.rs` (+30 lines) - Add RAID endpoints
- `src/main.rs` (+5 lines) - Wire up RAID module
- `src/minimal_models.rs` (+10 lines) - Add RAID error types
- `src/lib.rs` (+1 line) - Export RAID module

**Total Impact**: ~1700 new lines, ~200 modified lines in existing files.

## Risk Mitigation

| Risk | Mitigation |
|------|-----------|
| Regression in single-replica | Conditional logic preserves existing code path |
| SPDK RAID 1 instability | Thorough testing, alpha/beta rollout |
| Increased complexity | Isolated module, clear documentation |
| Performance degradation | Make RAID 1 opt-in, benchmark before/after |
| Data loss during failure | RAID 1 provides redundancy, test extensively |

## Implementation Checklist

### Phase 1: Local RAID 1 (Weeks 1-6)
- [ ] Create `src/raid/` module structure
- [ ] Implement `RaidService` with SPDK RPC calls
- [ ] Add RAID models and error types
- [ ] Modify `create_volume()` with conditional logic
- [ ] Add node agent RAID endpoints
- [ ] Implement volume deletion for RAID volumes
- [ ] Write unit tests
- [ ] Write integration tests (kuttl)
- [ ] Run regression tests
- [ ] Update documentation

### Phase 2: Distributed RAID 1 (Weeks 7-12)
- [ ] Implement multi-node disk selection
- [ ] NVMe-oF target creation for replicas
- [ ] NVMe-oF connection on attach node
- [ ] Distributed RAID bdev creation
- [ ] Test failover scenarios
- [ ] Implement health monitoring
- [ ] Dashboard integration
- [ ] Update documentation

### Phase 3: Release (Weeks 13-15)
- [ ] Alpha testing in dev cluster
- [ ] Beta testing in staging cluster
- [ ] Performance benchmarking
- [ ] Production deployment guide
- [ ] User documentation
- [ ] GA release

## Quick Start (After Implementation)

### 1. Create RAID 1 Volume
```bash
cat <<EOF | kubectl apply -f -
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: my-raid-pvc
spec:
  accessModes:
    - ReadWriteOnce
  resources:
    requests:
      storage: 10Gi
  storageClassName: flint-csi-raid1-local
EOF
```

### 2. Use in Pod
```bash
cat <<EOF | kubectl apply -f -
apiVersion: v1
kind: Pod
metadata:
  name: raid-test-pod
spec:
  containers:
  - name: app
    image: nginx
    volumeMounts:
    - name: storage
      mountPath: /data
  volumes:
  - name: storage
    persistentVolumeClaim:
      claimName: my-raid-pvc
EOF
```

### 3. Check RAID Status
```bash
# Via node agent
kubectl exec -n kube-system <spdk-pod> -- curl http://localhost:8081/api/raid/status?raid_name=raid_vol_my-raid-pvc

# Via dashboard
curl http://dashboard-url/api/raid/cluster_status
```

### 4. Simulate Failure (Test Only!)
```bash
# Fail one disk (on SPDK node)
sudo nvme disconnect /dev/nvme1n1

# Check RAID status - should show DEGRADED but still accessible
```

## Key Takeaways

1. **Zero Regression**: Existing single-replica volumes completely unaffected
2. **Phased Approach**: Start simple (local), then add complexity (distributed)
3. **SPDK Native**: Leverage SPDK RAID 1 for best performance and reliability
4. **Isolated Module**: Keep RAID code separate like snapshots
5. **Opt-In**: Default remains single replica, RAID 1 is opt-in via StorageClass
6. **Comprehensive Testing**: Regression, functional, and failure scenario tests

## References

- **Full Plan**: `MULTI_REPLICA_IMPLEMENTATION_PLAN_V2.md`
- **Metadata Storage**: `VOLUME_METADATA_STORAGE.md`
- **SPDK RAID Docs**: `/Users/ddalton/github/spdk/doc/bdev.md` (lines 486-508)
- **SPDK RPC Docs**: `/Users/ddalton/github/spdk/doc/jsonrpc.md.jinja2` (lines 10287-10556)
- **Current Code**: `spdk-csi-driver/src/driver.rs`
- **Snapshot Pattern**: `spdk-csi-driver/src/snapshot/` (reference for module isolation)

---

**Status**: Ready for Implementation
**Next Step**: Review plan with team, create GitHub issues, start implementation

