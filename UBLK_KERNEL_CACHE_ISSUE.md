# ublk Kernel Cache Issue - Critical Fix

**Date:** December 4, 2025  
**Severity:** CRITICAL - Can cause data corruption or mount failures  
**Status:** ✅ FIXED

## The Problem

### Hash-Based ublk IDs + Kernel Caching = Stale Filesystem Detection

ublk device IDs are deterministic (hash-based from volume ID). This means:
- Volume with ID `pvc-abc123` → always maps to `/dev/ublkb5` (example)
- When device is deleted and recreated → **same device path**
- Linux kernel caches block device metadata by device path
- Cache includes filesystem type, superblock location, etc.

### Attack Scenario

```
Timeline:
1. Volume A (pvc-abc123) created
   → ublk device /dev/ublkb5 created
   → formatted with ext4
   → mounted, used, unmounted
   → Volume deleted (ublk device destroyed)
   → Kernel STILL has cached: "ublkb5 = ext4 filesystem"

2. Volume B (pvc-def456) created from snapshot
   → Hash collision! Also maps to ublk ID 5
   → ublk device /dev/ublkb5 recreated  
   → Contains XFS filesystem from snapshot
   → BUT kernel cache says "ublkb5 = ext4" ❌

3. NodeStageVolume runs:
   → blkid /dev/ublkb5
   → Kernel returns CACHED ext4 metadata (WRONG!)
   → Real device has XFS, but we don't know it
   → Try to mount as ext4 → FAILS or CORRUPTS DATA ❌
```

## Why This Is Critical

**Without cache clearing for volumes with existing filesystems:**

1. **Wrong filesystem type detection:**
   ```bash
   # Real device has XFS
   $ blkid /dev/ublkb5
   /dev/ublkb5: TYPE="ext4"  # WRONG! Kernel cache is stale
   ```

2. **Mount failures:**
   ```
   mount: /var/lib/kubelet/...: wrong fs type, bad option, 
          bad superblock on /dev/ublkb5
   ```

3. **Potential data corruption:**
   - If mount succeeds with wrong fs type
   - Writes go to wrong structures
   - Filesystem corruption

## The Solution

### Two-Pronged Approach

**For brand new volumes (no existing filesystem):**
```bash
wipefs --all --force /dev/ublkb5
```
- Clears filesystem signatures
- Clears kernel cache
- Safe because volume is empty

**For volumes with existing filesystems (clones/snapshots):**
```bash
blockdev --flushbufs /dev/ublkb5
```
- Flushes kernel's block device cache
- Does NOT modify device data (non-destructive)
- Forces kernel to re-read device on next access
- **Critical:** Must run BEFORE blkid check

### Implementation

```rust
let fs_initialized = volume_context.get("filesystem-initialized") == Some("true");

if !fs_initialized {
    // Brand new volume - clear signatures + cache
    Command::new("wipefs")
        .args(&["--all", "--force", device_path])
        .output()?;
} else {
    // CRITICAL: Volume has existing filesystem
    // Must clear cache without destroying data
    Command::new("blockdev")
        .args(&["--flushbufs", device_path])
        .output()?;
}

// NOW blkid will see the REAL filesystem
let blkid_output = Command::new("blkid").arg(device_path).output()?;
```

## Why `blockdev --flushbufs` Is Safe

- **Read-only operation** - doesn't write to device
- **Cache-only** - only affects kernel's in-memory cache
- **Forced re-read** - kernel queries device fresh on next access
- **No data loss** - actual device data untouched
- **Fast** - completes in milliseconds

## Alternative Approaches Considered

### ❌ Don't use hash-based ublk IDs
- Would require major architecture change
- Breaks deterministic device naming
- Not worth the complexity

### ❌ Only run wipefs (current approach)
- Works for new volumes
- **FAILS for cloned volumes** (destroys their filesystem)

### ❌ Skip cache clearing for clones
- **FAILS** - blkid sees stale filesystem (this bug!)

### ✅ Use blockdev --flushbufs for clones (our solution)
- Safe (non-destructive)
- Fast (milliseconds)
- Effective (clears cache)
- Compatible with all filesystem types

## Testing Scenarios

### Test 1: Clone with Different Filesystem Type
```bash
# Setup
1. Create Volume A with ext4, use it, delete it (ublkb5)
2. Create Volume B from snapshot with XFS (also ublkb5)

# Expected WITHOUT fix:
- blkid reports ext4 (WRONG - stale cache)
- Mount fails

# Expected WITH fix:
- blockdev --flushbufs clears cache
- blkid reports XFS (CORRECT)
- Mount succeeds
```

### Test 2: ublk ID Reuse
```bash
# Setup
1. Create/delete volumes until ublk ID wraps around
2. Verify kernel cache is properly cleared each time

# Expected:
- Each volume sees correct filesystem
- No stale cache issues
```

### Test 3: Rapid Create/Delete
```bash
# Setup
1. Rapidly create and delete volumes (same ID reuse)
2. Mix of ext4, xfs, new volumes, clones

# Expected:
- All volumes mount correctly
- No cache-related failures
```

## Code Locations

**Implementation:**
- `spdk-csi-driver/src/main.rs` lines 1344-1400
- NodeStageVolume function

**Controller metadata:**
- CreateVolumeFromSnapshot - sets `filesystem-initialized: true`
- CreateVolumeFromVolume - sets `filesystem-initialized: true`

## Monitoring

Watch for these log messages:

**Good (cache cleared for clone):**
```
🧹 [CACHE_CLEAR] BLOCKDEV FLUSH for volume with existing filesystem
   Method: blockdev --flushbufs (safe, preserves data)
   Reason: Clear stale kernel cache without destroying filesystem
✅ [BLOCKDEV] Kernel cache flushed successfully
```

**Bad (flush failed):**
```
⚠️ [BLOCKDEV] Flush command failed (continuing): ...
   This may cause blkid to see stale filesystem - mounting may fail
```

## Impact

**Before fix:**
- ❌ Cloned volumes could fail to mount (stale cache)
- ❌ Wrong filesystem type detected
- ❌ Potential data corruption

**After fix:**
- ✅ All volumes see correct filesystem
- ✅ Clones mount successfully
- ✅ Data integrity preserved
- ✅ Works for all filesystem types

## References

- Linux kernel block device caching: `/Documentation/block/`
- blockdev(8) man page
- wipefs(8) man page
- WIPEFS_SOLUTION_PLAN.md
- WIPEFS_IMPLEMENTATION_SUMMARY.md

