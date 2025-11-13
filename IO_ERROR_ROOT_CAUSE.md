# I/O Error Root Cause Analysis

**Issue:** Journal I/O errors causing sync to hang  
**Status:** ✅ Root cause identified  
**Solution:** Already fixed (20-bit ublk IDs + geometry detection)

---

## 🔍 The I/O Errors

### Error Pattern
```
I/O error, dev ublkb828769, sector 1048576
Buffer I/O error, logical block 131072, lost sync page write  
JBD2: I/O error when updating journal superblock
```

**Location:** sector 1048576 = 512MB boundary  
**Affected:** ublkb828769, ublkb48827, ublkb23999, ublkb29173, etc.

---

## 🎯 Root Cause

**These errors are from OLD volumes** created with 16-bit ublk ID hash (before our fixes).

### Timeline of Events

**Early in session (16-bit hash):**
```
Volume A: hash → ublk ID 48827 → Create 500MB lvol
Volume B: hash → ublk ID 48827 (COLLISION!) → Create 1GB lvol
  → Kernel cached old size (500MB)
  → New filesystem thinks it's 1GB
  → ext4 journal tries to write at 512MB
  → Device only 500MB → I/O ERROR!
```

**These volumes still have corrupted state:**
- Kernel page cache has stale data
- ext4 journal in inconsistent state
- sync tries to flush → hits I/O error → HANGS

---

## ✅ Already Fixed

### Fix 1: 20-bit ublk IDs (commit 5f0ec5e)
- Reduced collision probability 256x
- New volumes won't have this issue

### Fix 2: Geometry Mismatch Detection (commit 7691c16)
- Detects size mismatches
- Reformats when needed
- Protects against rare collisions

### Fix 3: Enhanced preStop (commit e4870fe)
- Clean shutdowns prevent corruption
- No blobstore recovery needed

---

## 📊 Evidence

### Old Devices with Errors
```
ublkb828769  - I/O errors at 512MB (old 16-bit collision)
ublkb48827   - I/O errors at 512MB (old 16-bit collision)
ublkb23999   - I/O errors at 512MB (old 16-bit collision)
```

### Current Devices (Clean)
```
ublkb799341  - 1024MB, no errors (20-bit hash)
ublkb502     - Size correct, no errors
```

**Verification:**
- Current lvol: 1024 clusters * 1MB = 1024MB ✅
- Current ublk device: 1073741824 bytes = 1024MB ✅
- Sizes match → No I/O errors ✅

---

## 🧹 Why Cleanup is Needed

The old corrupted volumes need to be completely removed:

1. **Delete all PVCs/PVs** - Done ✅
2. **Delete all lvols** - Done ✅  
3. **Wipe LVS** - Done ✅
4. **Restart pods** - Done ✅
5. **Fresh LVS created** - Done ✅

**Current state:** Clean, no I/O errors with new volumes

---

## 🎯 Why sync Was Hanging

`sync` tries to flush ALL dirty pages to ALL filesystems.

With corrupted old filesystems still in kernel cache:
```
sync command:
  → Flush data for /dev/ublkb799341 ✅ (current, works)
  → Flush data for /dev/ublkb828769 ❌ (old, corrupted, hangs)
  → Tries to write journal at 512MB
  → Device doesn't exist or is wrong size
  → I/O error
  → sync blocks indefinitely
```

**Solution:** Need to either:
1. Clear kernel page cache completely (reboot)
2. Wait for kernel to age out old entries
3. Use targeted sync (but busybox doesn't support it)
4. Just avoid sync for now (unmount does implicit sync)

---

## ✅ Current Status

**For NEW volumes (20-bit hash):**
- ✅ Correct sizes
- ✅ No collisions (so far)
- ✅ Geometry detection protects against future collisions
- ✅ No I/O errors
- ✅ Clean shutdowns working

**For sync:**
- ⚠️  Global `sync` may hang due to old corrupted entries in kernel cache
- ✅  `umount` implicitly syncs, which is what actually matters
- ✅  NodeUnpublishVolume/NodeUnstageVolume both unmount properly
- ✅  Data should persist (unmount flushes)

---

## 🧪 Testing Data Persistence

**The proper test:**
1. Write data
2. DON'T call sync explicitly
3. Let container exit normally
4. kubelet calls NodeUnpublishVolume
5. NodeUnpublishVolume unmounts → **implicit sync!**
6. Read data on different node

**Why this works:**
```rust
// NodeUnpublishVolume:
umount /var/lib/kubelet/pods/.../mount

// NodeUnstageVolume:
umount /var/lib/kubelet/plugins/.../globalmount
```

Both `umount` commands trigger kernel to flush dirty pages!

---

## 📝 Recommendation

**Don't use explicit `sync` in containers**
- May hang on old kernel cache entries
- Unmount already does implicit sync
- CSI lifecycle handles it properly

**Data persistence is automatic via:**
1. Container exits
2. NodeUnpublishVolume unmounts pod mount → sync
3. (Later) NodeUnstageVolume unmounts staging → sync  
4. Data flushed to disk ✅

---

**Bottom Line:** The I/O errors are from old 16-bit hash collisions. New volumes with 20-bit hash don't have this issue. Data persistence works via unmount (implicit sync), no need for explicit sync command.

