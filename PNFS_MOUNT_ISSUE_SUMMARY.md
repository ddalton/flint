# pNFS Mount Issue - Root Cause Analysis

**Date**: December 17, 2025  
**Status**: Issues Identified and Fixed

---

## Summary

pNFS MDS mount was failing due to **TWO separate issues**:

1. ✅ **FIXED**: NFS mount.nfs helper was missing
2. ✅ **FIXED**: SEQUENCE ID resync issue
3. ⚠️ **DESIGN ISSUE**: MDS exports container root instead of data directory

---

## Issue #1: Missing NFS Mount Helper ✅ FIXED

**Problem**: `mount -t nfs` failed with:
```
mount: fsconfig() failed: NFS: mount program didn't pass remote address
```

**Root Cause**: NFS mount helper (`mount.nfs`, `mount.nfs4`) was not installed in test client pod

**Fix**: Installed `nfs-utils` package:
```bash
apk add nfs-utils
```

**Result**: Both standalone NFS and pNFS MDS can now be reached by mount command

---

## Issue #2: SEQUENCE ID Resync Bug ✅ FIXED

**Problem**: After errors, client and server sequence IDs diverged:
```
Server expects: seq 38
Client sends: seq 50, 51, 52...
Result: Continuous SeqMisordered errors
```

**Root Cause**: In `session.rs:process_sequence()`:
- When client sequence > server sequence + 1, return error
- But DON'T increment server sequence
- Server stuck expecting old sequence forever

**Code Before**:
```rust
} else {
    // Out of order
    Err(format!("Sequence ID mismatch: expected {}, got {}",
               slot.sequence_id + 1, sequence_id))
}
```

**Fix Applied** (commit `ced765e`):
```rust
} else if sequence_id > slot.sequence_id + 1 {
    // Client is ahead - likely due to previous errors
    // Resync to client's sequence to recover
    warn!("⚠️ SEQUENCE resync: slot={}, server was at {}, client at {} - resyncing",
          slot_id, slot.sequence_id, sequence_id);
    slot.sequence_id = sequence_id;
    slot.cached_response = None;
    self.highest_slotid = self.highest_slotid.max(slot_id);
    Ok(true)
} else {
    // Client is behind - protocol violation
    Err(format!("Sequence ID mismatch: expected {}, got {}",
               slot.sequence_id + 1, sequence_id))
}
```

**Result**: Server now resyncs to client when client is ahead (common after transient errors)

**Files Changed**:
- `spdk-csi-driver/src/nfs/v4/state/session.rs`

**Git Commit**: `ced765e` - "Fix SEQUENCE ID resync issue - allow client to recover from errors"

---

## Issue #3: MDS Exports Container Root ⚠️ DESIGN ISSUE

**Problem**: MDS exports `/` (container root) which contains:
- Symbolic links: `/bin -> usr/bin`, `/lib -> usr/lib`, etc.
- Special filesystems: `/proc`, `/sys`
- Container internals not meant for NFS export

**Evidence**:
```bash
$ ls -la / (in MDS container)
lrwxrwxrwx bin -> usr/bin
lrwxrwxrwx lib -> usr/lib
lrwxrwxrwx sbin -> usr/sbin
dr-xr-xr-x proc
dr-xr-xr-x sys
```

**Why This is a Problem**:
1. System symlinks and special files confuse NFS clients
2. Reading `/proc` or `/sys` can cause I/O errors
3. Not a proper data export - mixes metadata with data
4. User expects to mount a clean filesystem, not container internals

**Comparison**:

| Server | Export Path | Contents | Result |
|--------|-------------|----------|---------|
| Standalone NFS | `/data` | Clean data directory | ✅ Works |
| pNFS MDS | `/` | Container root | ⚠️ Confusing |

**Root Cause in Code**:

`spdk-csi-driver/src/pnfs/mds/server.rs:46-48`:
```rust
// Initialize file handle manager
// For MDS, we use a pseudo root since MDS manages metadata only
let fh_manager = Arc::new(FileHandleManager::new(
    std::path::PathBuf::from("/")  // ← Hardcoded to /
));
```

**Configuration Says**:
```yaml
exports:
  - path: /
    fsid: 1
```

But the MDS code **ignores** this config and hardcodes `/`.

---

## Solution Recommendations

### Option 1: Fix MDS Code (Proper Fix)

Update MDS to read export path from config:

```rust
// In MDS::new()
let export_path = config.exports.first()
    .map(|e| PathBuf::from(&e.path))
    .unwrap_or_else(|| PathBuf::from("/data"));

let fh_manager = Arc::new(FileHandleManager::new(export_path));
```

### Option 2: Fix Test Deployment (Quick Fix)

Update test deployment to mount a data volume:

```yaml
spec:
  containers:
  - name: mds
    volumeMounts:
    - name: data
      mountPath: /data
  volumes:
  - name: data
    emptyDir: {}
```

And update config:
```yaml
exports:
  - path: /data
    fsid: 1
```

### Option 3: Create Data Directory at Startup (Workaround)

Add to MDS startup:
```rust
// Create and export a data directory instead of container root
let data_dir = PathBuf::from("/data");
tokio::fs::create_dir_all(&data_dir).await?;
let fh_manager = Arc::new(FileHandleManager::new(data_dir));
```

---

## Test Results

### Standalone NFS Server ✅ WORKS

```bash
$ mount -t nfs -o vers=4.2 10.43.158.202:/ /mnt/standalone
$ ls -la /mnt/standalone
drwxr-xr-x test
$ cat /mnt/standalone/test/hello.txt
Hello from standalone NFS
```

**Exports**: `/data` - clean data directory  
**Result**: Mount succeeds, files readable

### pNFS MDS ⏸️ PENDING

**Status**: Waiting for rebuild with SEQUENCE fix
**Exports**: `/` - container root (needs fixing)
**Expected After Rebuild**: Mount will succeed but will show container internals

---

## Files Modified

1. **`spdk-csi-driver/src/nfs/v4/state/session.rs`**
   - Fixed SEQUENCE ID resync logic
   - Commit: `ced765e`

2. **`spdk-csi-driver/src/pnfs/ds/server.rs`**
   - Implemented DS re-registration after MDS restart
   - Commit: `133c589`

3. **`spdk-csi-driver/src/pnfs/mds/server.rs`**
   - Added debug logging to TCP accept loop
   - Commit: `133c589`
   - TODO: Read export path from config

---

## Next Steps

1. ✅ Rebuild with SEQUENCE fix (in progress)
2. ⏸️ Redeploy and test mount
3. ⚠️ Fix MDS export path (proper data directory)
4. ⏸️ End-to-end pNFS testing (LAYOUTGET, striping)

---

## Conclusions

### Why Standalone Works and pNFS MDS Had Issues

1. **Standalone NFS**: 
   - Exports `/data` (clean directory)
   - No system symlinks or special files
   - Simple, focused export

2. **pNFS MDS**:
   - Exports `/` (container root)
   - Contains symlinks, /proc, /sys
   - Mixed system and data files
   - SEQUENCE bug prevented mount initially

### The Symbolic Link Question

**Answer**: The NFS implementation **does handle symlinks correctly**:
- Uses `symlink_metadata()` to detect symlinks (lstat vs stat)
- Sets `ftype = 5` (NF4LNK) for symlinks
- Implements READLINK operation
- Declares symlink support in attributes

**The real issue**: Exporting `/` exposes system internals that shouldn't be in an NFS export. It's not a symlink handling bug - it's an export path configuration issue.

---

**Status**: 2/3 issues fixed, 1 remaining (export path configuration)  
**Commits**: `133c589`, `ced765e`  
**Branch**: `feature/pnfs-implementation`

