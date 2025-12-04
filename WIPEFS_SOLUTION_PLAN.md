# Wipefs Solution - Unified Filesystem-Initialized Attribute

**Date:** December 3, 2025  
**Status:** Design Complete, Implementation In Progress

## The Problem

**Ublk ID Reuse + Kernel Cache:**
- ublk IDs are hash-based (same volume ID → same ublk ID)
- When ublk devices are deleted/recreated, kernel caches filesystem signatures
- blkid can report stale ext4 from a DIFFERENT volume that used the same ublk ID
- Mounting fails: "bad superblock" (filesystem is from wrong volume)

**Wipefs Purpose:**
- Clear filesystem signatures from device
- Forces kernel to re-scan fresh
- **BUT: wipefs WRITES to the device** (modifies lvol data!)
- Running wipefs on volume with data → **CORRUPTS filesystem** ❌

## Current Issues

**The num_allocated_clusters approach doesn't work:**
```
Non-thin volumes (thin_provision: false):
  - SPDK pre-allocates ALL clusters immediately  
  - num_allocated_clusters = 1024 even when brand new
  - Can't distinguish "brand new" from "has data"
  
Result:
  - Our code skips wipefs (thinks it has data)
  - blkid reports stale cache
  - Mount fails: bad superblock
```

## The Unified Solution

**Single PV Attribute: `filesystem-initialized`**

### Controller Side (DONE)

**For regular volumes:**
```rust
CreateVolume:
  // Don't set filesystem-initialized
  // Node will format and set it
```

**For snapshot clones:**
```rust
CreateVolumeFromSnapshot:
  volume_context["filesystem-initialized"] = "true"
  volume_context["source-snapshot"] = snapshot_id
  // Filesystem exists from snapshot
```

**For PVC clones:**
```rust
CreateVolumeFromVolume:
  volume_context["filesystem-initialized"] = "true"  
  volume_context["source-volume"] = source_pvc_id
  // Filesystem exists from source PVC
```

### Node Side (TODO)

**Wipefs Decision (Simple!):**
```rust
NodeStageVolume:
  let fs_initialized = req.volume_context.get("filesystem-initialized") == Some("true")
  
  if fs_initialized {
      // Filesystem exists - NEVER run wipefs
      eprintln!("✅ [WIPEFS] SKIPPED - filesystem already initialized")
      skip wipefs
  } else {
      // Brand new volume - safe to wipefs
      eprintln!("🧹 [WIPEFS] EXECUTING - brand new volume")
      run wipefs  // Clear any stale kernel cache
  }
  
  // Check/format/mount
  check blkid
  if no filesystem:
      format device
      // TODO: Update PV to set filesystem-initialized = true
  mount
```

### Benefits

✅ **Unified logic** - one attribute for all volume types
✅ **Works for thin and non-thin** - doesn't depend on allocation semantics
✅ **Works for clones** - controller sets attribute
✅ **Works for regular volumes** - node sets attribute after first format
✅ **Safe** - wipefs only on brand new volumes
✅ **Simple** - clear boolean decision

## Implementation Steps

### Step 1: Controller (DONE ✅)
- [x] Update CreateVolumeFromSnapshot to set filesystem-initialized
- [x] Update CreateVolumeFromVolume to set filesystem-initialized
- [x] Remove is-clone attribute
- [x] Committed changes

### Step 2: Node Wipefs Logic (TODO)
- [ ] Replace clone detection with filesystem-initialized check
- [ ] Simplify wipefs decision to single if/else
- [ ] Remove complex SPDK metadata queries
- [ ] Keep wipefs logging prominent

### Step 3: Node PV Update (TODO - Optional)
- [ ] After formatting new volume, update PV attribute
- [ ] Use Kubernetes API from node
- [ ] Handle failures gracefully

**Note:** Step 3 is optional because:
- Without it: wipefs runs on every restaging (but safe due to blkid check)
- With it: wipefs only on truly first staging (optimal)

## Testing Plan

1. Regular volume (new) - should format and work
2. Regular volume (restage) - should preserve data
3. Snapshot clone - should preserve snapshot data
4. PVC clone - should preserve source data
5. Volume expansion - should preserve data during resize

All tests should pass with this unified approach!

