# Code Path Diagram - Single vs Multi-Replica

## Volume Creation Flow

```
┌─────────────────────────────────────────────────────────────────┐
│                   CSI CreateVolume Request                      │
│                (numReplicas from StorageClass)                  │
└─────────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────────┐
│              driver.create_volume(replica_count)                │
└─────────────────────────────────────────────────────────────────┘
                              ↓
                    ┌─────────┴─────────┐
                    │ replica_count == 1? │
                    └─────────┬─────────┘
                              │
            ┌─────────────────┴─────────────────┐
            │ YES (Single)                      │ NO (Multi)
            ↓                                   ↓
┌─────────────────────────────┐   ┌─────────────────────────────┐
│ create_single_replica_volume│   │create_distributed_multi_    │
│         (EXISTING CODE)     │   │    replica_volume           │
│                             │   │      (NEW CODE)             │
└──────────────┬──────────────┘   └──────────────┬──────────────┘
               │                                  │
               │                                  │
        ┌──────┴──────┐                   ┌──────┴──────┐
        ↓             ↓                   ↓             ↓
  Select 1 node   Create lvol      Select N nodes  Create N lvols
  (existing)      (existing)       (new)           (new)
        │             │                   │             │
        └──────┬──────┘                   └──────┬──────┘
               ↓                                  ↓
    ┌──────────────────┐           ┌──────────────────────┐
    │ VolumeCreationResult          │ VolumeCreationResult │
    │ replicas: [1]    │           │ replicas: [N]        │
    └──────────────────┘           └──────────────────────┘
               │                                  │
               └──────────────┬───────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────────┐
│                  CreateVolumeResponse                           │
│                                                                 │
│  if replicas.len() == 1:                                       │
│    volumeAttributes:                                           │
│      node-name: "node1"        ← EXISTING FORMAT               │
│      lvol-uuid: "12345..."                                     │
│      lvs-name: "lvs_..."                                       │
│  else:                                                         │
│    volumeAttributes:                                           │
│      replicas: "[{...},{...}]" ← NEW FORMAT                    │
└─────────────────────────────────────────────────────────────────┘
```

## Volume Attachment Flow

```
┌─────────────────────────────────────────────────────────────────┐
│            CSI ControllerPublishVolume Request                  │
│                  (volume_id, node_id)                           │
└─────────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────────┐
│          driver.get_replicas_from_pv(volume_id)                 │
└─────────────────────────────────────────────────────────────────┘
                              ↓
                    ┌─────────┴─────────┐
                    │ replicas found?   │
                    └─────────┬─────────┘
                              │
            ┌─────────────────┴─────────────────┐
            │ None (Single)                     │ Some(replicas) (Multi)
            ↓                                   ↓
┌─────────────────────────────┐   ┌─────────────────────────────┐
│ get_volume_info()           │   │  Pass replicas JSON         │
│   (EXISTING)                │   │  in publish_context         │
│                             │   │  (NEW)                      │
└──────────────┬──────────────┘   └──────────────┬──────────────┘
               │                                  │
        ┌──────┴──────┐                          │
        ↓             ↓                          │
  Local node?   Remote node?                     │
  (existing)    (existing)                       │
        │             │                          │
        ↓             ↓                          │
  volumeType:   volumeType:                      │
    "local"       "remote"                       │
  bdevName      nqn, targetIp                    │
                                                 ↓
                                           volumeType:
                                             "multi-replica"
                                           replicas JSON
               │                                  │
               └──────────────┬───────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────────┐
│              ControllerPublishVolumeResponse                    │
│                    (publish_context)                            │
└─────────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────────┐
│               CSI NodeStageVolume Request                       │
│                  (publish_context)                              │
└─────────────────────────────────────────────────────────────────┘
                              ↓
                    ┌─────────┴─────────┐
                    │ volumeType ?      │
                    └─────────┬─────────┘
                              │
        ┌─────────────────────┼─────────────────────┐
        ↓                     ↓                     ↓
   "local"             "remote"           "multi-replica"
   (EXISTING)          (EXISTING)              (NEW)
        │                     │                     │
        ↓                     ↓                     ↓
  Use lvol bdev    Connect NVMe-oF    create_raid_from_replicas()
  directly         get nvme bdev                    │
  (existing)       (existing)              ┌────────┴────────┐
        │                     │             │                 │
        │                     │         For each replica:     │
        │                     │           Local: Use lvol     │
        │                     │           Remote: NVMe-oF     │
        │                     │             │                 │
        │                     │         Create RAID 1 bdev    │
        │                     │                 │             │
        └──────────┬──────────┴─────────────────┘             │
                   ↓                                          │
        ┌──────────────────┐                                  │
        │ create_ublk_device│  ← SAME for all types          │
        └──────────┬─────────┘                                │
                   ↓                                          │
        ┌──────────────────┐                                  │
        │ Format & Mount   │  ← SAME for all types           │
        └──────────────────┘                                  │
```

## Volume Deletion Flow

```
┌─────────────────────────────────────────────────────────────────┐
│                CSI DeleteVolume Request                         │
│                     (volume_id)                                 │
└─────────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────────┐
│          driver.get_replicas_from_pv(volume_id)                 │
└─────────────────────────────────────────────────────────────────┘
                              ↓
                    ┌─────────┴─────────┐
                    │ replicas found?   │
                    └─────────┬─────────┘
                              │
            ┌─────────────────┴─────────────────┐
            │ None (Single)                     │ Some(replicas) (Multi)
            ↓                                   ↓
┌─────────────────────────────┐   ┌─────────────────────────────┐
│ get_volume_info()           │   │  For each replica:          │
│   (EXISTING)                │   │    delete_lvol(node, uuid)  │
│                             │   │    cleanup NVMe-oF          │
│         ↓                   │   │  (NEW)                      │
│ force_unstage_if_needed     │   │                             │
│ delete_lvol(node, uuid)     │   │                             │
│ cleanup NVMe-oF             │   │                             │
│   (EXISTING)                │   │                             │
└─────────────────────────────┘   └─────────────────────────────┘
               │                                  │
               └──────────────┬───────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────────┐
│              DeleteVolumeResponse (success)                     │
└─────────────────────────────────────────────────────────────────┘
```

## Code Execution Matrix

| Operation | Single Replica | Multi-Replica | Shared Code |
|-----------|----------------|---------------|-------------|
| **Node Selection** | `select_node_for_single_replica()` | `select_nodes_for_replicas()` | Capacity cache |
| **Lvol Creation** | 1× `create_lvol()` | N× `create_lvol()` | Same method |
| **Metadata Storage** | Simple format | JSON array | Storage logic |
| **Controller Publish** | Local/Remote check | Pass replicas | K8s API |
| **Bdev Attachment** | Direct lvol or NVMe-oF | RAID from mixed sources | `create_ublk_device()` |
| **ublk Device** | `create_ublk_device()` | `create_ublk_device()` | **SAME** |
| **Mount** | Mount ublk | Mount ublk | **SAME** |
| **Deletion** | 1× `delete_lvol()` | N× `delete_lvol()` | Same method |

## Key Isolation Points

### 1. **Entry Point Isolation**
```rust
// driver.rs:390-397
pub async fn create_volume(..., replica_count: u32, ...) {
    if replica_count == 1 {
        return self.create_single_replica_volume(...).await;
        //     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
        //     EARLY EXIT - No multi-replica code executes!
    }
    // Multi-replica code here (never reached if replica_count == 1)
}
```

### 2. **Metadata Format Isolation**
```rust
// main.rs:479-506
if result.replicas.len() == 1 {
    // SINGLE: Use existing simple format
    volume_context.insert("node-name", ...);
} else {
    // MULTI: Use new JSON format
    volume_context.insert("replicas", json);
}
```

### 3. **Attachment Logic Isolation**
```rust
// main.rs:ControllerPublishVolume
match get_replicas_from_pv() {
    Ok(None) => {
        // SINGLE: Existing local/remote logic
        let volume_info = get_volume_info().await?;
        // ... existing code ...
    }
    Ok(Some(replicas)) => {
        // MULTI: New RAID logic
    }
}
```

### 4. **Node Stage Isolation**
```rust
// main.rs:NodeStageVolume
if volume_type == "local" {
    // SINGLE LOCAL: Existing
} else if volume_type == "remote" {
    // SINGLE REMOTE: Existing
} else if volume_type == "multi-replica" {
    // MULTI: New RAID path
}
```

## Why This Guarantees No Regression

1. **Early Exit Pattern**: Single-replica returns BEFORE any multi-replica code
2. **Code Extraction**: Single-replica method is EXACT copy of original
3. **Separate Branches**: Single/multi paths never intersect
4. **Shared Utilities Only**: Only use same low-level functions (create_lvol, etc.)
5. **Metadata Compatibility**: Single-replica format unchanged
6. **Type Safety**: Rust's type system prevents accidental mixing

## Testing Strategy

```
┌─────────────────────────────────────────────────────────────────┐
│                     Test Coverage                               │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  ┌───────────────┐                      ┌───────────────┐      │
│  │ Single Replica│                      │ Multi-Replica │      │
│  │ Tests         │                      │ Tests         │      │
│  │ (EXISTING)    │                      │ (NEW)         │      │
│  └───────┬───────┘                      └───────┬───────┘      │
│          │                                      │              │
│          ↓                                      ↓              │
│  ✓ rwo-pvc-migration                   ✓ multi-replica test   │
│  ✓ rwx-multi-pod                       ✓ 2-way mirror         │
│  ✓ volume-expansion                    ✓ 3-way mirror         │
│  ✓ snapshot-restore                    ✓ insufficient nodes   │
│  ✓ clean-shutdown                      ✓ degraded mode        │
│                                                                 │
│  Both paths test:                                              │
│  ┌──────────────────────────────────────────────────────┐     │
│  │ • create_lvol()                                      │     │
│  │ • delete_lvol()                                      │     │
│  │ • create_ublk_device()                               │     │
│  │ • Mount/Unmount                                      │     │
│  │ • Capacity cache                                     │     │
│  └──────────────────────────────────────────────────────┘     │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

---

**Summary**: Complete isolation through early exit routing ensures single-replica code path is **completely unaffected** by multi-replica implementation.

