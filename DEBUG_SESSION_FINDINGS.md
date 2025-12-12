# Debug Session Findings - Permission Denied Root Cause

**Date:** December 12, 2024  
**Status:** Deep investigation with comprehensive logging

---

## Current Status

### ✅ What's Working

1. **Mount** - Succeeds ✅
2. **Ownership** - `Uid: (0/ root) Gid: (0/ root)` ✅ (NOT nobody!)  
3. **READDIR** - Returns "volume" with perfect attributes ✅
4. **ACCESS** - Grants READ | LOOKUP | EXECUTE on pseudo-root ✅
5. **Simple `ls`** - Shows "volume" ✅

### ❌ What's Failing

1. **`ls -la`** - Shows `???????` for all entries
2. **`cd /mnt/nfs-test/volume`** - Permission denied
3. **LOOKUP** - **NEVER CALLED** by client! ← Core issue

---

## Detailed Analysis

### READDIR Attributes - PERFECT!

**Client requested:**
```
Bitmap: [0x0010011a, 0x0030a03a]
Attributes: [1, 3, 4, 8, 20, 33, 35, 36, 37, 45, 47, 52, 53]
```

**We returned:**
```
Bitmap: [0x0010011a, 0x0030a03a] 
Attributes: [1, 3, 4, 8, 20, 33, 35, 36, 37, 45, 47, 52, 53]
```

**✅ EXACT MATCH - 100% RFC compliant!**

**Attributes returned:**
- 1=TYPE (NF4DIR)
- 3=CHANGE (timestamp)
- 4=SIZE (4096)
- 8=FSID (0, 0) - same as pseudo-root ✅
- 20=FILEID (unique hash)
- 33=MODE (0755)
- 35=NUMLINKS (2)
- 36=OWNER ("0")
- 37=OWNER_GROUP ("0")
- 45=SPACE_USED (4096)
- 47=TIME_ACCESS (timestamp)
- 52=TIME_METADATA (timestamp)
- 53=TIME_MODIFY (timestamp)

### ACCESS Operations - WORKING!

```
ACCESS called: mask=0x1f
   Requested: READ=true, LOOKUP=true, MODIFY=true, EXTEND=true, DELETE=true
   ✅ Granted: READ | LOOKUP | EXECUTE (mask=0x23)
```

### Kernel Error Messages

```
NFS: permission(0:46/1), mask=0x81, res=-13
```

- **Inode:** 0:46/1 (device 0:46, inode 1 = pseudo-root)
- **Mask:** 0x81 (Linux VFS mask, NOT NFS ACCESS bits!)
- **Result:** -13 = EACCES (Permission denied)

**Key insight:** This is a **Linux VFS permission check**, not an NFS ACCESS operation!

---

## The Real Problem

The client is making a **local permission decision** based on the READDIR attributes, WITHOUT calling LOOKUP!

**Why no LOOKUP?**

When `ls -la` tries to stat entries, it:

1. Checks if entry has cached filehandle → NO (READDIR doesn't return FH)
2. Checks if it needs LOOKUP → Makes VFS permission check first
3. **VFS permission check fails** → Stops, never does LOOKUP!

**VFS check (mask 0x81):**
- 0x80 = MAY_NOT_BLOCK (kernel internal flag)
- 0x01 = MAY_EXEC

The kernel is checking "can I traverse into this?" before issuing LOOKUP.

**Why does it fail?**

Something in the READDIR attributes makes the kernel think it can't access the entry!

---

## Hypothesis: FSID Issue

We're returning **FSID=(0, 0)** for BOTH:
- Pseudo-root: FSID=(0, 0) ✅
- Volume entry: FSID=(0, 0) ✅

**But** the Linux NFS client might interpret this as:
- "Volume has same FSID as parent → it's a regular directory"
- But it's in the pseudo-filesystem (device 0:46)
- AND it doesn't have a real inode in that filesystem
- Client gets confused → denies permission!

**Alternative hypothesis:** We should return **different FSID** to indicate this IS a mount point boundary!

---

## What Ganesha Does

From our packet capture, Ganesha's pseudo-root READDIR returns **EMPTY** (no entries).

Why this works:
- Client NEVER sees export entries in READDIR
- Client must do direct mount: `mount server:/volume /mnt`
- OR client must LOOKUP "/volume" directly
- No ambiguity about filesystem boundaries

---

## Possible Solutions

### Solution 1: Return Different FSID (Cross-Mount Indication)

```rust
// In encode_export_entry_attributes:
fsid_major: 1,  // Different from pseudo-root's 0
fsid_minor: file_id,  // Unique per export

// This tells client: "This is a different filesystem - cross-mount point"
```

**But:** We tried this before and it didn't work!

### Solution 2: Add MOUNTED_ON_FILEID Attribute

Per RFC 7530, MOUNTED_ON_FILEID (55) indicates a mount point:

```rust
FATTR4_MOUNTED_ON_FILEID => {
    // Different from FILEID to indicate mount point
    attr_vals.put_u64(1);  // Pseudo-root's fileid
    true
}
```

### Solution 3: Change to Ganesha Model (Empty Pseudo-Root)

Return empty READDIR on pseudo-root, require direct export mount:

```bash
# Client must know export name
mount -t nfs server:/volume /mnt
```

**But:** Defeats our export-discovery feature!

### Solution 4: Return Export as Symlink

Make exports appear as symlinks to the actual path:

```rust
ftype: 5,  // NF4LNK (symlink) instead of NF4DIR
```

**Problem:** Breaks expectations, might not be RFC compliant

---

## Recommended Action

**Try Solution 2: Add MOUNTED_ON_FILEID**

This is the RFC-specified way to indicate mount points. The client should recognize this and handle the boundary properly.

```rust
// In snapshot encoder, add:
FATTR4_MOUNTED_ON_FILEID => {
    // For export entries, return pseudo-root's fileid
    // This signals "you're crossing into a mounted filesystem"
    attr_vals.put_u64(1);  // Pseudo-root fileid
    true
}
```

---

## Test Next

1. Add MOUNTED_ON_FILEID to snapshot encoder
2. Ensure it's returned for export entries when requested
3. Test if client now recognizes the mount boundary
4. Check if LOOKUP is triggered

---

## Alternative: Check Linux Kernel Expectations

Research exactly what the Linux NFS client expects to see for pseudo-filesystem mount points by examining:
- Linux kernel source: `fs/nfs/dir.c`
- How it handles pseudo-root directories
- What triggers the permission check failure


