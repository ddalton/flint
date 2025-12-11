# NFSv4 Pseudo-Filesystem Requirement - Root Cause Found

**Date:** December 11, 2024  
**Issue:** Mount fails with ENOTDIR even after all XDR fixes  
**Root Cause:** Missing pseudo-filesystem implementation  

---

## The Problem

When mounting `mount -t nfs server:/ /mnt`, NFSv4 clients expect a **pseudo-filesystem** root, not the actual export directory.

### What We're Doing Wrong ❌

```
Client: PUTROOTFH
Our Server: Returns filehandle for /root/flint/spdk-csi-driver/target/nfs-test-export
Client: GETATTR  
Our Server: Returns attributes of the ACTUAL export directory
Client: This doesn't look like a pseudo-filesystem root → ENOTDIR
```

### What Ganesha Does Right ✅

```
Client: PUTROOTFH
Ganesha: Returns pseudo-filesystem root with "Root node (nil)"
Client: GETATTR
Ganesha: Returns attributes of VIRTUAL pseudo-root (not actual directory)
Client: OK, this is a pseudo-root → Mount succeeds (shows empty)
```

---

## Evidence from Testing

### Our Server Log:
```
PUTROOTFH
GETATTR for path: "/root/flint/spdk-csi-driver/target/nfs-test-export"
```
→ We return the **actual export directory**

### Ganesha Log:
```
PUTROOTFH Export 0 pseudo (/) with path (/) and tag ((null))
Root node (nil)
```
→ Ganesha returns a **pseudo-filesystem root**

### Mount Results:
- **Ganesha:** Mount succeeds, `ls /mnt` shows empty (pseudo-root)
- **Our Server:** Mount fails with "not a directory" error

---

## What is NFSv4 Pseudo-Filesystem?

Per RFC 7530 Section 7:

> NFSv4 servers present all the exports for a given server as entries  
> in a pseudo file system, which provides a unique namespace for the  
> server, allowing clients to browse all exports.

**Key Points:**
1. The root "/" is a VIRTUAL filesystem  
2. Actual exports appear as entries within this virtual root
3. PUTROOTFH returns the pseudo-root, not any actual directory
4. Clients navigate to exports via LOOKUP from the pseudo-root

**Example Pseudo-Filesystem Structure:**
```
/                          ← Pseudo-root (virtual)
├── export1               ← Actual export
│   ├── file1.txt
│   └── subdir/
└── export2               ← Another export
    └── data/
```

---

## Why This Causes ENOTDIR

The Linux NFS client (`nfs4_try_get_tree()`):
1. Mounts `server:/` expecting a pseudo-root
2. Gets a filehandle and does GETATTR
3. Sees it's a regular directory with real filesystem attributes
4. Realizes it's NOT a pseudo-filesystem root
5. Returns **ENOTDIR** because the semantics don't match

---

## The Fix (Required)

### Architecture Change Needed:

1. **Create Pseudo-Filesystem Layer**
   - Maintain a virtual root separate from actual exports
   - PUTROOTFH returns pseudo-root filehandle  
   - Pseudo-root GETATTR returns synthetic attributes

2. **Export Registry**
   - Track all exports and their pseudo-paths
   - Our case: One export at pseudo-path "/"
   - Support multiple exports in future

3. **Path Navigation**
   - Implement LOOKUP from pseudo-root to exports
   - Handle both pseudo-paths and real filesystem paths
   - Maintain separate filehandle spaces

### Implementation Files to Modify:

1. **`src/nfs/v4/filehandle.rs`**
   - Add pseudo-filesystem root handling
   - Distinguish pseudo-handles from real filehandles
   - Add export path registry

2. **`src/nfs/v4/operations/fileops.rs`**
   - PUTROOTFH: Return pseudo-root instead of export root
   - GETATTR: Handle pseudo-root attributes specially
   - LOOKUP: Navigate from pseudo-root to exports

3. **`src/nfs/v4/pseudo.rs`** (NEW)
   - Pseudo-filesystem implementation
   - Export management
   - Synthetic attribute generation

---

## Workaround (Immediate)

Instead of mounting the pseudo-root, mount the export directly:

### Current (Fails):
```bash
mount -t nfs server:/ /mnt
```

### Workaround (Should work once pseudo-fs is implemented):
```bash
# If we expose export with explicit name:
mount -t nfs server:/volume-name /mnt
```

But this still requires pseudo-filesystem infrastructure to route the request.

---

## Comparison: Pseudo-Root vs Real Directory

| Attribute | Pseudo-Root | Real Directory |
|-----------|-------------|----------------|
| Path | "/" (virtual) | "/actual/path" |
| Inode | Synthetic | Real filesystem inode |
| Parent | None (is root) | Has parent directory |
| Contents | Export list | Real files/dirs |
| mtime | Server start time | Real modification time |
| Behavior | Virtual navigation | Real filesystem |

---

## Why We Missed This Initially

1. **XDR Protocol Focus:** We fixed attribute encoding, which was real
2. **Ganesha Comparison:** Didn't realize Ganesha returns pseudo-root
3. **Error Message:** "not a directory" was misleading  
4. **Kernel Behavior:** Client expects specific pseudo-fs semantics

---

## Next Steps

### Phase 1: Minimal Pseudo-Filesystem (Single Export)
- [ ] Create pseudo-root with synthetic attributes
- [ ] PUTROOTFH returns pseudo-root handle
- [ ] Implement export as direct child of pseudo-root
- [ ] LOOKUP "/" → returns our single export

### Phase 2: Full Pseudo-Filesystem
- [ ] Support multiple exports
- [ ] Hierarchical pseudo-paths (/vol1, /vol2, etc.)
- [ ] Export discovery and listing
- [ ] READDIR on pseudo-root shows exports

### Phase 3: Advanced Features
- [ ] Referrals and fs_locations
- [ ] Cross-export navigation
- [ ] Pseudo-filesystem persistence

---

## Testing Plan

### Test 1: Basic Pseudo-Root
```bash
mount -t nfs server:/ /mnt
ls /mnt  # Should show export name, not files yet
```

### Test 2: Navigate to Export
```bash
ls /mnt/export-name  # Should show actual files
cat /mnt/export-name/test.txt
```

### Test 3: Attributes
```bash
stat /mnt  # Should show pseudo-root attributes
stat /mnt/export-name  # Should show real directory
```

---

## References

- **RFC 7530 Section 7:** "File System Namespace"  
  https://datatracker.ietf.org/doc/html/rfc7530#section-7

- **Linux Kernel:** `fs/nfs/nfs4super.c:nfs4_try_get_tree()`  
  Expects pseudo-filesystem semantics

- **Ganesha:** `src/FSAL/Stackable_FSALs/FSAL_MDCACHE/mdcache_lru.c`  
  Pseudo-filesystem implementation reference

---

## Conclusion

The ENOTDIR issue is **NOT** a bug in our XDR encoding (that's fixed!).  
It's a missing **architectural feature**: NFSv4 pseudo-filesystem support.

Without pseudo-filesystem:
- ❌ Cannot mount `server:/`
- ❌ Single-export servers don't work properly  
- ❌ Not RFC 7530 compliant

With pseudo-filesystem:
- ✅ Proper `server:/` mounts
- ✅ Multi-export support  
- ✅ RFC 7530 compliant
- ✅ Compatible with all NFS clients

**This is a significant but well-defined feature addition.**

---

**Discovered:** December 11, 2024  
**Severity:** High (blocks basic NFSv4 mounting)  
**Complexity:** Medium (well-documented in RFC, reference implementations available)  
**Estimated Effort:** 2-3 days for minimal implementation, 1 week for full feature

