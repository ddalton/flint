# Permission Denied Root Cause Analysis

**Date:** December 11, 2024  
**Status:** Mount succeeds, READDIR works, but `ls -la` shows ???????

---

## Current Behavior

```bash
$ mount -t nfs -o vers=4.2,tcp 127.0.0.1:/ /mnt/nfs-test
✅ Mount: SUCCESS

$ ls /mnt/nfs-test/
volume  ← Entry appears! ✅

$ ls -la /mnt/nfs-test/
total 0
d????????? ? ? ? ?            ? .
d????????? ? ? ? ?            ? ..
d????????? ? ? ? ?            ? volume
ls: cannot access '/mnt/nfs-test/.': Permission denied
ls: cannot access '/mnt/nfs-test/..': Permission denied
ls: cannot access '/mnt/nfs-test/volume': Permission denied ❌

$ cd /mnt/nfs-test/volume
Permission denied ❌
```

---

## What We Know

### ✅ Working Operations

1. **Mount** - Client can mount pseudo-root
2. **READDIR** - Returns "volume" with 13 attributes (112 bytes)
3. **GETATTR on pseudo-root** - Returns synthetic attributes
4. **ACCESS on pseudo-root** - Grants READ | LOOKUP | EXECUTE

### ❌ Failing Operations

1. **ls -la** - Cannot get attributes for entries (shows ???????)
2. **cd into volume** - Permission denied
3. **LOOKUP** - **NOT BEING CALLED!** ← Smoking gun

---

## The Core Problem

When client does `ls -la`, it tries to:

```
For each entry (., .., volume):
    1. GETATTR using entry's filehandle
       ↓
    ❌ ERROR: Entry doesn't have a filehandle!
       ↓
    2. Shows ??????? (can't display attributes)
```

**The issue:** READDIR returns entries with attributes, but NOT filehandles!

Per RFC 7530, READDIR returns:
- ✅ Entry name
- ✅ Entry attributes  
- ❌ NOT the filehandle!

Client must do **LOOKUP** to get filehandle before doing GETATTR.

---

## Why Client Doesn't LOOKUP

The client makes a decision based on READDIR attributes alone:

**Hypothesis 1: FSID Mismatch**
- We return FSID = (1, file_id_hash)
- Client expects FSID to match actual filesystem
- Client rejects entry as "different mount point"

**Hypothesis 2: Missing/Wrong Attribute**
- Client requires specific attribute value
- We're not providing it correctly
- Client treats entry as inaccessible

**Hypothesis 3: Owner/Group Format Issue**
- Even with username lookup, format might be wrong
- Client expects "user@domain" format?
- We're returning just "root"

---

## What Ganesha Does Differently

From packet capture:
```
Ganesha READDIR on pseudo-root:
    Value Follows: No
    EOF: Yes

(No entries returned!)
```

**Ganesha's approach:**
- Pseudo-root READDIR returns **EMPTY**
- Clients must know export path beforehand
- Mount directly: `mount server:/volume /mnt`
- No discovery via READDIR

**Why this works:**
- Client never sees synthetic directory entries
- Direct LOOKUP to "/volume" 
- Server translates to real filesystem path

---

## Possible Solutions

### Solution 1: Return Filehandles in READDIR (NOT RFC COMPLIANT)

Some servers return an optional filehandle with each entry:

```rust
// entry4 can optionally include filehandle
struct entry4 {
    cookie: u64,
    name: string,
    attrs: fattr4,
    // Optional in some implementations (NOT standard!)
    filehandle?: nfs_fh4,
}
```

**Problem:** This is NOT in RFC 7530 READDIR specification!

### Solution 2: Fix FSID for Export Entries

Return FSID that indicates this IS a mount point:

```rust
// Current: Different FSID to show different filesystem
fsid_major: 1,
fsid_minor: file_id_hash,

// Try: Same as pseudo-root to show same filesystem?
fsid_major: 0,
fsid_minor: 0,

// Or: Match actual export directory's filesystem
let export_metadata = fs::metadata(export.path)?;
fsid_major: export_metadata.dev(),
fsid_minor: 0,
```

### Solution 3: Change Pseudo-Filesystem Model (Like Ganesha)

Don't return exports in READDIR:

```rust
if self.fh_mgr.is_pseudo_root(current_fh) {
    // Return EMPTY like Ganesha
    return ReadDirRes {
        status: Nfs4Status::Ok,
        cookieverf: 1,
        entries: vec![],  // Empty!
        eof: true,
    };
}
```

**But:** This defeats the purpose of our export-listing model!

### Solution 4: Fix Special Entries (., ..)

The permission denied on `.` and `..` is suspicious:

```
ls: cannot access '/mnt/nfs-test/.': Permission denied
ls: cannot access '/mnt/nfs-test/..': Permission denied
```

These should ALWAYS work! We might need to handle LOOKUP for "." and ".." specially.

---

## Recommended Next Step

**Test Solution 2: Fix FSID**

The FSID mismatch is the most likely culprit. The client sees:
- Pseudo-root: FSID = (0, 0)
- Export entry: FSID = (1, hash)

Client thinks: "This is a DIFFERENT filesystem, I need to check if it's a mount point"

But we haven't implemented the mount point check properly!

**Fix:**
```rust
// In encode_export_entry_attributes
// Return SAME FSID as pseudo-root to indicate same filesystem
snapshot.fsid_major = 0;  // Same as pseudo-root
snapshot.fsid_minor = 0;  // Same as pseudo-root
```

This tells the client "this entry is part of the same filesystem", not a mount point boundary.

---

## Code to Test

```rust
fn encode_export_entry_attributes(name: &str, requested_attrs: &[u32]) -> (Vec<u8>, Vec<u32>) {
    // ... generate file_id ...
    
    let snapshot = AttributeSnapshot {
        ftype: 2, // NF4DIR
        size: 4096,
        space_used: 4096,
        fileid: file_id,
        fsid_major: 0,  // ← CHANGE: Same as pseudo-root!
        fsid_minor: 0,  // ← CHANGE: Same as pseudo-root!
        // ... rest ...
    };
}
```

---

## Next Actions

1. Change FSID to (0, 0) in export entries
2. Test mount + ls -la
3. If still failing, add detailed logging for GETATTR on entries
4. Check kernel logs: `dmesg | grep NFS`


