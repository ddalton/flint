# Multi-Replica Implementation - Session Handoff Document

## Context for New Session

### Current State (As of November 21, 2025)

**Phase 1**: ✅ **COMPLETE & TESTED**
- Branch: `feature/ublk-backend`
- Commits: 336b4b1, 817e81b, 7dd56bf, c975a2b
- Deployed and tested in cluster
- System test `rwo-pvc-migration` PASSED

**Next**: 🚀 **Ready to implement multi-replica support**

### What Has Been Implemented

1. **Dynamic Node Selection** ✅
   - File: `spdk-csi-driver/src/driver.rs`
   - Method: `select_node_for_single_replica()`
   - Selects node with most free space
   - Uses capacity cache for fast lookups

2. **Capacity Caching** ✅
   - File: `spdk-csi-driver/src/capacity_cache.rs` (407 lines)
   - 30s TTL, 60s background refresh
   - Optimistic capacity reservation
   - Scales to 1000s of volumes

3. **Metadata Storage in PV** ✅
   - Stores in `spec.csi.volumeAttributes`:
     - `flint.csi.storage.io/node-name`
     - `flint.csi.storage.io/lvol-uuid`
     - `flint.csi.storage.io/lvs-name`
     - `flint.csi.storage.io/replica-count`
   - O(1) volume lookups
   - No expensive node queries

4. **Helm Chart Updated** ✅
   - Default `numReplicas: "1"`
   - File: `flint-csi-driver-chart/values.yaml`

### Key Code Locations

**Single-Replica Volume Creation**:
- `spdk-csi-driver/src/driver.rs` line ~130: `create_volume()`
- Returns: `VolumeCreationResult` with `replicas: Vec<ReplicaInfo>`

**Node Selection**:
- `spdk-csi-driver/src/driver.rs` line ~85: `select_node_for_single_replica()`
- Uses: `capacity_cache.get_node_capacity()`

**Metadata Storage**:
- `spdk-csi-driver/src/main.rs` line ~464: `create_volume()` response
- Stores metadata in `volume_context`

**Capacity Cache**:
- `spdk-csi-driver/src/capacity_cache.rs`
- Used by driver for all node selection

### Multi-Replica Implementation Plan

**Complete documentation ready**:
- 📘 `MULTI_REPLICA_IMPLEMENTATION_PLAN_V2.md` - Step-by-step implementation
- 📋 `MULTI_REPLICA_QUICK_REFERENCE.md` - Quick reference
- 🗄️ `VOLUME_METADATA_STORAGE.md` - How to extend for multi-replica

### Key Design Decisions (Already Documented)

1. **Distributed RAID 1 Only** - No local RAID
2. **Replicas on Different Nodes** - Each replica MUST be on different node
3. **RAID Created on Pod's Node** - Not on replica nodes
4. **Mixed Access**:
   - Local replica: Direct lvol bdev access
   - Remote replicas: NVMe-oF initiator connections
5. **Minimum 2 Replicas** - RAID 1 requires at least 2
6. **Degraded Operation** - Works with 2+ replicas even if some nodes down
7. **Auto-Rebuild** - Monitor and add replicas back when nodes return

### SPDK RAID 1 APIs (Ready to Use)

**Create RAID**:
```json
{
  "method": "bdev_raid_create",
  "params": {
    "name": "raid_vol_pvc-abc123",
    "raid_level": "1",
    "base_bdevs": ["lvol_uuid_1", "lvol_uuid_2", "lvol_uuid_3"]
  }
}
```

**Query RAID Status**:
```json
{
  "method": "bdev_raid_get_bdevs",
  "params": { "category": "all" }
}
```

**Delete RAID**:
```json
{
  "method": "bdev_raid_delete",
  "params": { "name": "raid_vol_pvc-abc123" }
}
```

**Add Base Bdev** (for rebuild):
```json
{
  "method": "bdev_raid_add_base_bdev",
  "params": {
    "raid_bdev": "raid_vol_pvc-abc123",
    "base_bdev": "lvol_uuid_new"
  }
}
```

**SPDK Docs**: `/Users/ddalton/github/spdk/doc/jsonrpc.md.jinja2` (lines 10287-10556)

### Implementation Approach (From Planning Docs)

**Step 1**: Extend node selection for N replicas
```rust
// Reuse existing logic
async fn select_nodes_for_replicas(
    &self,
    replica_count: u32,
    size_bytes: u64,
) -> Result<Vec<NodeSelection>, MinimalStateError> {
    // Similar to select_node_for_single_replica()
    // But select N different nodes
    // Use capacity cache (already implemented)
}
```

**Step 2**: Create replicas on N nodes
```rust
async fn create_multi_replica_volume(...) -> Result<VolumeCreationResult> {
    let selected_nodes = self.select_nodes_for_replicas(replica_count, size_bytes).await?;
    
    // Create lvol on each node
    for (i, node_info) in selected_nodes.iter().enumerate() {
        let lvol_uuid = self.create_lvol(...).await?;
        replicas.push(ReplicaInfo { ... });
    }
    
    // Return VolumeCreationResult with all replicas
    Ok(VolumeCreationResult {
        volume_id,
        size_bytes,
        replicas, // Array of all replicas
    })
}
```

**Step 3**: Store replicas in volumeAttributes
```rust
// In main.rs create_volume()
if result.replicas.len() > 1 {
    // Store full replica array as JSON
    let replicas_json = serde_json::to_string(&result.replicas)?;
    volume_context.insert(
        "flint.csi.storage.io/replicas".to_string(),
        replicas_json,
    );
}
```

**Step 4**: Create RAID on Pod's node (NodePublishVolume)
```rust
async fn node_publish_volume(...) {
    // Read replicas from volume_context
    let replicas: Vec<ReplicaInfo> = serde_json::from_str(
        req.volume_context.get("flint.csi.storage.io/replicas")?
    )?;
    
    // Attach each replica (local or NVMe-oF)
    let mut base_bdevs = Vec::new();
    for replica in replicas {
        if replica.node_name == current_node {
            // Local - use direct lvol bdev
            base_bdevs.push(replica.lvol_uuid);
        } else {
            // Remote - setup NVMe-oF and attach
            let nvme_bdev = setup_and_attach_remote_replica(...).await?;
            base_bdevs.push(nvme_bdev);
        }
    }
    
    // Create RAID 1 bdev
    let raid_bdev = create_raid1_bdev(base_bdevs).await?;
    
    // Create ublk device from RAID bdev
    let device = create_ublk_device(&raid_bdev, ublk_id).await?;
}
```

### What You'll Need to Implement

**New files** (following snapshot module pattern):
```
spdk-csi-driver/src/raid/
├── mod.rs              # Module exports
├── raid_service.rs     # RAID creation/deletion
├── raid_models.rs      # Data structures
└── raid_health.rs      # Health monitoring (future)
```

**Modified files**:
- `driver.rs` - Add `select_nodes_for_replicas()`, `create_multi_replica_volume()`
- `main.rs` - Update `node_publish_volume()` to handle multi-replica
- `lib.rs` - Add `pub mod raid;`

### Testing Strategy

**Start with**: `tests/system/tests/multi-replica/` (create this)
- Create volume with `numReplicas: "2"`
- Verify 2 lvols on different nodes
- Mount on Pod
- Verify RAID created
- Read/write data
- Delete and cleanup

### Reference Implementation

**Snapshot module** is the perfect reference:
- Location: `spdk-csi-driver/src/snapshot/`
- Pattern: Isolated module with minimal integration
- Same approach for RAID module

### Quick Start Commands (For New Session)

```bash
# 1. Check out the branch
git checkout feature/ublk-backend
git pull

# 2. Review Phase 1 implementation
cat PHASE1_IMPLEMENTATION_SUMMARY.md

# 3. Read multi-replica plan
cat MULTI_REPLICA_IMPLEMENTATION_PLAN_V2.md

# 4. Start implementation
mkdir -p spdk-csi-driver/src/raid
# Follow the plan in MULTI_REPLICA_IMPLEMENTATION_PLAN_V2.md
```

### Key Files to Reference

**Planning**:
- `MULTI_REPLICA_IMPLEMENTATION_PLAN_V2.md` - Complete plan
- `MULTI_REPLICA_QUICK_REFERENCE.md` - Quick reference
- `VOLUME_METADATA_STORAGE.md` - Metadata strategy

**Implemented Foundation**:
- `spdk-csi-driver/src/capacity_cache.rs` - Reuse for multi-node selection
- `spdk-csi-driver/src/driver.rs` - Extend for multi-replica
- `spdk-csi-driver/src/main.rs` - Update CreateVolume response
- `spdk-csi-driver/src/minimal_models.rs` - VolumeCreationResult, ReplicaInfo

**Snapshot Reference**:
- `spdk-csi-driver/src/snapshot/mod.rs` - Module pattern to follow

### Questions to Answer in New Session

1. Should we extend `select_node_for_single_replica()` or create a new method?
2. Where should RAID RPC calls go? (New raid_service.rs or in driver.rs?)
3. How to handle insufficient nodes (PVC event)?
4. Test strategy - manual testing first or kuttl tests?

---

**Summary**: Yes, you have **comprehensive documentation** to start fresh! All design decisions documented, foundation complete, clear implementation path defined.

**Estimated time for multi-replica**: 2-3 weeks
- Week 1: Multi-node replica creation
- Week 2: RAID creation in NodePublishVolume
- Week 3: Testing and refinement

