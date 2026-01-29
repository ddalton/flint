# Generation Tracking Implementation for Replica Consistency

This document describes the implementation of generation tracking for replica consistency detection in the Flint SPDK CSI driver.

## Overview

Generation tracking provides **automatic detection of out-of-sync replicas** after node failures or network partitions. Each replica stores a generation number in its blob xattr metadata, which is incremented every time a volume is attached. This enables split-brain detection without external state management.

## Architecture

### Components

1. **`generation_tracking.rs`** - Core generation tracking module
   - `GenerationMetadata` - 24-byte metadata structure stored in blob xattrs
   - `GenerationComparisonResult` - Result of comparing generations across replicas
   - Helper functions for packing/unpacking binary metadata

2. **`spdk_native.rs`** - SPDK RPC methods for xattr operations
   - `lvol_set_xattr()` - Write generation metadata to lvol
   - `lvol_get_xattr()` - Read generation metadata from lvol
   - `lvol_remove_xattr()` - Remove generation metadata

3. **`driver.rs`** - Integration into RAID creation workflow
   - `read_replica_generation()` - Read generation from a replica
   - `write_replica_generation()` - Write generation to a replica
   - `check_replica_generations()` - Compare generations across all replicas
   - `increment_replica_generations()` - Increment generation on volume attach
   - `create_raid_from_replicas()` - Updated to include generation tracking

4. **`minimal_models.rs`** - Extended ReplicaInfo model
   - Added `generation: u64` field
   - Added `generation_timestamp: u64` field

## Metadata Format

Generation metadata is stored as a 24-byte binary structure in the blob xattr `csi.generation`:

```rust
struct GenerationMetadata {
    magic: u32,        // 0x4753504B ("GSPK") for validation
    generation: u64,   // Monotonically increasing counter
    timestamp: u64,    // Unix timestamp (seconds)
    node_id: u32,      // Hash of node name
}
```

The metadata is base64-encoded for JSON-RPC transport to/from SPDK.

## Workflow

### Volume Creation (CreateVolume)

1. Create lvol replicas on selected nodes
2. Initialize `ReplicaInfo` with `generation=0` (uninitialized)
3. Store replica information in PV annotations

### Volume Attach (NodePublishVolume)

1. **Read generations** from all replicas
   ```rust
   let gen_comparison = driver.check_replica_generations(&replicas).await?;
   ```

2. **Detect stale replicas**
   - Replicas with `generation < max_generation` are stale
   - Replicas without xattr are uninitialized
   
3. **Handle inconsistency**
   - If all replicas are consistent: proceed normally
   - If stale replicas detected: log warning, proceed with current replicas only
   - Future enhancement: automatic rebuild of stale replicas

4. **Attach replicas** (local or via NVMe-oF)

5. **Create RAID 1 bdev** from available replicas

6. **Increment generation** on all replicas
   ```rust
   driver.increment_replica_generations(&replicas, &current_node).await?;
   ```

### Example Generation States

#### Scenario 1: Normal Operation (All Replicas In Sync)

```
Replica 0: generation=5  ✓ Current
Replica 1: generation=5  ✓ Current
Replica 2: generation=5  ✓ Current
→ All in sync, create RAID with all 3 replicas
→ Increment to generation 6
```

#### Scenario 2: Stale Replica Detected

```
Replica 0: generation=10  ✓ Current
Replica 1: generation=7   ⚠️ Stale (behind by 3)
Replica 2: generation=10  ✓ Current
→ Proceed DEGRADED with replicas 0 and 2
→ Log warning about stale replica 1
→ Increment to generation 11
→ Replica 1 needs manual rebuild
```

#### Scenario 3: Uninitialized Replica (New Volume)

```
Replica 0: no xattr       ⚠️ Uninitialized
Replica 1: no xattr       ⚠️ Uninitialized
Replica 2: no xattr       ⚠️ Uninitialized
→ First attach, all replicas empty
→ Create RAID with all 3 replicas
→ Initialize generation to 1
```

## SPDK RPC Methods

The implementation requires SPDK with the `lvol-xattr-rpc.patch` applied. The patch adds three new RPC methods:

### bdev_lvol_set_xattr

Set xattr on an lvol:

```bash
rpc.py bdev_lvol_set_xattr \
  --name "lvs1/volume-uuid" \
  --xattr_name "csi.generation" \
  --xattr_value "R1NQSwAAAAoAAAAAAAAAAAAAAAA="  # base64 encoded
```

### bdev_lvol_get_xattr

Get xattr from an lvol:

```bash
rpc.py bdev_lvol_get_xattr \
  --name "lvs1/volume-uuid" \
  --xattr_name "csi.generation"
```

Returns:
```json
{
  "name": "lvs1/volume-uuid",
  "xattr_name": "csi.generation",
  "xattr_value": "R1NQSwAAAAoAAAAAAAAAAAAAAAA=",
  "value_len": 24
}
```

### bdev_lvol_remove_xattr

Remove xattr from an lvol:

```bash
rpc.py bdev_lvol_remove_xattr \
  --name "lvs1/volume-uuid" \
  --xattr_name "csi.generation"
```

## Implementation Details

### Generation Increment Strategy

- **When**: Every `NodePublishVolume` call (pod attach)
- **Where**: On the node performing the attach
- **How**: 
  1. Read current max generation across all replicas
  2. Create `new_generation = max_generation + 1`
  3. Write new generation to all replicas
  4. Continue even if some replicas fail (best-effort)

### Error Handling

- **Xattr not found** (ENOENT): Treated as uninitialized replica (generation=0)
- **Invalid magic number**: Treated as corrupted metadata, re-initialize
- **RPC failure**: Log warning, continue (generation tracking is best-effort)
- **Stale replica detected**: Log warning, proceed with current replicas only

### Performance Impact

- **Zero data path overhead**: Xattrs stored in blob metadata, not data blocks
- **One-time read cost**: Generation read happens once during volume attach
- **Minimal write cost**: Generation increment is async metadata write
- **Expansion-safe**: Xattrs unaffected by volume resize

## Testing

### Unit Tests

The `generation_tracking` module includes comprehensive unit tests:

```bash
cd spdk-csi-driver
cargo test generation_tracking
```

Tests cover:
- ✅ Pack/unpack binary format
- ✅ Base64 encoding/decoding
- ✅ Invalid magic number detection
- ✅ Generation comparison (all in sync)
- ✅ Generation comparison (with stale replicas)
- ✅ Generation comparison (with uninitialized replicas)

### Integration Testing

Manual testing workflow:

1. **Create multi-replica volume**:
   ```yaml
   apiVersion: v1
   kind: PersistentVolumeClaim
   metadata:
     name: test-pvc
   spec:
     storageClassName: spdk-replicated
     accessModes: ["ReadWriteOnce"]
     resources:
       requests:
         storage: 1Gi
   ```

2. **Attach to pod** (first time):
   ```
   Check logs for:
   🔍 [GEN_TRACK] Checking generations for 3 replicas...
      Replica 0: uninitialized
      Replica 1: uninitialized
      Replica 2: uninitialized
   📈 [GEN_TRACK] New generation: 1 (from 0)
   ```

3. **Detach and re-attach**:
   ```
   Check logs for:
   🔍 [GEN_TRACK] Checking generations for 3 replicas...
      Replica 0: generation=1, timestamp=...
      Replica 1: generation=1, timestamp=...
      Replica 2: generation=1, timestamp=...
   ✅ [GEN_TRACK] All replicas are in sync
   📈 [GEN_TRACK] New generation: 2 (from 1)
   ```

4. **Simulate stale replica** (for testing):
   ```bash
   # Manually reset generation on one replica
   rpc.py bdev_lvol_remove_xattr \
     --name "replica-1-uuid" \
     --xattr_name "csi.generation"
   ```

5. **Re-attach**:
   ```
   Check logs for:
   🔍 [GEN_TRACK] Checking generations for 3 replicas...
      Replica 0: generation=2, timestamp=...
      Replica 1: uninitialized
      Replica 2: generation=2, timestamp=...
   ⚠️ [DRIVER] WARNING: Detected 1 out-of-sync replicas
   ⚠️ [DRIVER] Proceeding in DEGRADED mode with current replicas only
   ```

## Future Enhancements

### Automatic Replica Rebuild

Currently, stale replicas are detected but require manual rebuild. Future enhancement:

1. **Detect stale replica** during `NodePublishVolume`
2. **Identify current replica** (source for rebuild)
3. **Copy data** from current replica to stale replica:
   - For local replicas: use `dd` or `bdev_lvol_clone`
   - For remote replicas: setup temporary NVMe-oF connection
4. **Update generation** on rebuilt replica
5. **Add to RAID** once synchronized

### Generation Metadata Versioning

Add version field to metadata format for future extensions:

```rust
struct GenerationMetadata {
    magic: u32,        // 0x4753504B
    version: u16,      // Metadata format version
    reserved: u16,     // Reserved for future use
    generation: u64,
    timestamp: u64,
    node_id: u32,
    crc32: u32,        // Checksum for validation
}
```

### Generation History

Track generation history in separate xattr for debugging:

```rust
struct GenerationHistory {
    entries: Vec<GenerationHistoryEntry>,
}

struct GenerationHistoryEntry {
    generation: u64,
    timestamp: u64,
    node_id: u32,
    event: String,  // "increment", "rebuild", "initialize"
}
```

## Troubleshooting

### Error: "bdev_lvol_set_xattr: command not found"

**Cause**: SPDK not built with xattr patch

**Solution**: Rebuild SPDK container with `lvol-xattr-rpc.patch`:
```bash
cd spdk-csi-driver
docker build -f docker/Dockerfile.spdk -t spdk-csi:xattr .
```

Check build logs for:
```
✅ Lvol xattr RPC support applied (bdev_lvol_set/get/remove_xattr)
```

### Warning: "Detected N out-of-sync replicas"

**Cause**: Replica was offline during previous attaches, or network partition

**Solution**: 
1. Check replica health: `kubectl logs -n flint-system spdk-node-<node> -c node-agent`
2. If replica is healthy but stale: manual rebuild required
3. If replica is unhealthy: replace replica

### Error: "Failed to decode generation: InvalidMagic"

**Cause**: Corrupted metadata or version mismatch

**Solution**:
1. Remove corrupted xattr: `rpc.py bdev_lvol_remove_xattr --name <lvol> --xattr_name csi.generation`
2. Re-attach volume to re-initialize generation

## References

- **XATTR_GENERATION_TRACKING.md** - SPDK xattr RPC documentation
- **lvol-xattr-rpc.patch** - SPDK patch for xattr support
- **SPDK Blob Documentation**: https://spdk.io/doc/blob.html
- **SPDK RPC Documentation**: https://spdk.io/doc/jsonrpc.html

## Implementation Status

✅ **Completed**:
- Generation metadata structure and serialization
- SPDK xattr RPC wrappers
- ReplicaInfo model extension
- Generation tracking in RAID creation workflow
- Stale replica detection
- Generation increment on attach
- Comprehensive unit tests

🚧 **Future Work**:
- Automatic stale replica rebuild
- Generation metadata versioning
- Generation history tracking
- Admin CLI for manual generation management

## License

Copyright 2026 Flint Storage Project
