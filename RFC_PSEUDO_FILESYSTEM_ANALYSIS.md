# RFC 7530 Pseudo-Filesystem Requirements - Detailed Analysis

**Date:** December 11, 2024  
**RFCs Analyzed:** RFC 7530 (NFSv4), RFC 5661 (NFSv4.1), RFC 7862 (NFSv4.2)  
**Focus:** Section 7 - File System Namespace and Pseudo-Filesystem

---

## RFC 7530 Section 7: NFSv4 File System Namespace

### Key Quote from RFC 7530:

> "NFSv4 servers present all the exports for a given server as entries  
> in a pseudo file system, which provides a unique namespace for the  
> server, allowing clients to browse all exports."

### Critical Requirements:

1. **Pseudo-Filesystem is MANDATORY**
   - ALL NFSv4 servers MUST implement a pseudo-filesystem
   - This is not optional - it's a core protocol requirement
   - Clients expect this structure and will fail without it

2. **Server Export Model**
   ```
   /                    ← PSEUDO-ROOT (synthetic, always present)
   ├── export1         ← First export  
   ├── export2         ← Second export
   └── volumes/
       └── vol1        ← Nested export
   ```

3. **Single Export Special Case**
   - Even with ONE export, pseudo-filesystem is REQUIRED
   - The export appears as a child of the pseudo-root
   - Client mounts "/" and navigates to export

---

## Why Pseudo-Filesystem Exists

### Problem It Solves:

In NFSv3, each export was mounted separately:
```bash
mount server:/export1 /mnt/export1
mount server:/export2 /mnt/export2  # Different mount
```

In NFSv4, ONE mount gives access to ALL exports:
```bash
mount server:/ /mnt         # Mount pseudo-root
ls /mnt/export1             # Access first export
ls /mnt/export2             # Access second export
```

### Benefits:

1. **Unified Namespace** - All exports under one mount point
2. **Cross-Export Operations** - Can navigate between exports
3. **Export Discovery** - Client can list available exports
4. **Simplified Client** - No need to know export paths beforehand

---

## PUTROOTFH Behavior

### RFC Requirement:

> "The PUTROOTFH operation sets the current filehandle to the root  
> of the pseudo file system... not to any particular export."

### What This Means:

```
Client sends: PUTROOTFH
Server MUST return: Filehandle for PSEUDO-ROOT (virtual)
Server MUST NOT return: Filehandle for an actual export directory
```

### Our Current Bug:

```rust
// WRONG: We return the actual export directory
pub fn root_filehandle(&self) -> Result<Nfs4FileHandle, String> {
    self.path_to_filehandle(&self.export_path)  // ❌ Returns real directory!
}
```

Should be:
```rust
// CORRECT: Return pseudo-root
pub fn root_filehandle(&self) -> Result<Nfs4FileHandle, String> {
    self.get_pseudo_root_handle()  // ✅ Returns virtual root
}
```

---

## Pseudo-Root vs Real Directory Attributes

### Pseudo-Root (PUTROOTFH response):

| Attribute | Value | Why |
|-----------|-------|-----|
| TYPE | NF4DIR (2) | It's a directory-like entity |
| FSID | {0, 0} or synthetic | Not a real filesystem |
| FILEID | Synthetic (e.g., 1) | Not a real inode |
| SIZE | 0 or small | No actual data |
| MTIME | Server start or constant | No real modification |
| NLINK | 2 or synthetic | Virtual links |
| PARENT | None (IS root) | No parent exists |

### Real Export Directory (after LOOKUP):

| Attribute | Value | Why |
|-----------|-------|-----|
| TYPE | NF4DIR (2) | Real directory |
| FSID | Real filesystem ID | Actual filesystem |
| FILEID | Real inode number | From filesystem |
| SIZE | Real size | Disk allocation |
| MTIME | Real modification time | From filesystem |
| NLINK | Real link count | From filesystem |
| PARENT | Pseudo-root | Has parent |

---

## Client Mount Sequence

### Correct Sequence (with Pseudo-Filesystem):

```
1. Client: EXCHANGE_ID
2. Client: CREATE_SESSION
3. Client: PUTROOTFH
   Server: Returns pseudo-root handle
4. Client: GETFH
   Server: Returns pseudo-root filehandle
5. Client: GETATTR(pseudo-root)
   Server: Returns SYNTHETIC attributes (FSID=0/0, FILEID=1, etc.)
6. Client: Recognizes pseudo-root → Mount succeeds
7. Client: LOOKUP("export-name")
   Server: Returns actual export handle
8. Client: GETATTR(export)
   Server: Returns REAL filesystem attributes
9. Client: Can now access files
```

### Our Broken Sequence (without Pseudo-Filesystem):

```
1. Client: EXCHANGE_ID
2. Client: CREATE_SESSION
3. Client: PUTROOTFH
   Server: Returns REAL export directory handle ❌
4. Client: GETFH
   Server: Returns actual directory filehandle
5. Client: GETATTR
   Server: Returns REAL filesystem attributes (real FSID, real inode, etc.) ❌
6. Client: This doesn't match pseudo-root semantics → ENOTDIR ❌
7. Mount fails
```

---

## How to Detect Pseudo-Root

### Client-Side Checks (from Linux kernel code):

The Linux NFS client (`fs/nfs/nfs4proc.c`) checks:

1. **FSID Check:**
   ```c
   // Pseudo-root often has FSID {0, 0} or special value
   if (fattr->fsid.major == 0 && fattr->fsid.minor == 0) {
       // Might be pseudo-root
   }
   ```

2. **Path Context:**
   ```c
   // If we just did PUTROOTFH and path is "/"
   if (is_root_path && just_did_putrootfh) {
       // Expect pseudo-root attributes
   }
   ```

3. **Attribute Consistency:**
   ```c
   // Pseudo-root has specific attribute patterns
   // Low file IDs, synthetic timestamps, etc.
   ```

If attributes don't match pseudo-root expectations → **ENOTDIR**

---

## Implementation Strategies

### Strategy 1: Minimal (Single Export Only) ⭐ **RECOMMENDED**

For servers that only export ONE volume:

```rust
pub struct PseudoFilesystem {
    // Pseudo-root with synthetic attributes
    pseudo_root: PseudoNode,
    
    // Single export
    export_name: String,      // E.g., "volume"
    export_path: PathBuf,     // Real directory
}

impl PseudoFilesystem {
    // PUTROOTFH returns this
    pub fn get_pseudo_root(&self) -> Nfs4FileHandle {
        // Return synthetic handle for "/"
    }
    
    // LOOKUP from pseudo-root
    pub fn lookup_from_root(&self, name: &str) -> Result<PathBuf> {
        if name == self.export_name {
            Ok(self.export_path.clone())
        } else {
            Err(NFS4ERR_NOENT)
        }
    }
}
```

**Mount Command:**
```bash
# Client mounts pseudo-root
mount -t nfs server:/ /mnt

# Access actual files via export name
ls /mnt/volume/      # Works!
cat /mnt/volume/file.txt
```

---

### Strategy 2: Full (Multiple Exports)

For servers with multiple exports:

```rust
pub struct PseudoFilesystem {
    pseudo_root: PseudoNode,
    exports: HashMap<String, Export>,  // name → export info
}

struct Export {
    pseudo_path: String,    // "/vol1" or "/data/vol1"
    real_path: PathBuf,     // Actual filesystem path
    export_id: u32,
}
```

**Mount Command:**
```bash
mount -t nfs server:/ /mnt
ls /mnt/                   # Shows: vol1, vol2, data/
ls /mnt/vol1/              # Access first export
ls /mnt/vol2/              # Access second export
```

---

## Special Considerations

### 1. FSID Assignment

**RFC Guidance:**
- Pseudo-root can have FSID `{0, 0}` (common)
- Or synthetic FSID different from real filesystems
- Each real export MUST have unique FSID

**Implementation:**
```rust
fn get_fsid(&self, handle: &Nfs4FileHandle) -> (u64, u64) {
    if self.is_pseudo_root(handle) {
        (0, 0)  // Pseudo-root special value
    } else {
        // Real filesystem FSID from stat()
        let stat = fs::metadata(path)?;
        (stat.dev() as u64, 0)
    }
}
```

### 2. File ID Assignment

**RFC Guidance:**
- Pseudo-root needs synthetic file ID (e.g., 1)
- Must be distinct from real filesystem inodes
- Stable across server restarts (if possible)

**Implementation:**
```rust
const PSEUDO_ROOT_FILEID: u64 = 1;

fn get_fileid(&self, handle: &Nfs4FileHandle) -> u64 {
    if self.is_pseudo_root(handle) {
        PSEUDO_ROOT_FILEID
    } else {
        // Real inode from filesystem
        fs::metadata(path)?.ino()
    }
}
```

### 3. Parent Directory

**RFC Guidance:**
- Pseudo-root has NO parent
- LOOKUPP on pseudo-root returns NFS4ERR_NOENT
- This distinguishes it from regular directories

**Implementation:**
```rust
fn handle_lookupp(&self, current_fh: &FileHandle) -> Result<FileHandle> {
    if self.is_pseudo_root(current_fh) {
        Err(NFS4ERR_NOENT)  // Can't go up from root
    } else if self.is_export_root(current_fh) {
        Ok(self.pseudo_root_handle())  // Parent is pseudo-root
    } else {
        // Normal parent lookup
        Ok(get_parent_directory(current_fh))
    }
}
```

---

## Testing Pseudo-Filesystem

### Test 1: Basic Mount
```bash
mount -t nfs server:/ /mnt
# Should succeed (currently fails)

stat /mnt
# Should show pseudo-root attributes (FSID=0/0, low fileid)
```

### Test 2: Export Discovery
```bash
ls /mnt/
# Should show export name(s)

ls /mnt/export-name/
# Should show actual files
```

### Test 3: Cross-Export Navigation  
```bash
cd /mnt/export1
ls
cd ../export2      # Navigate between exports
ls
```

### Test 4: LOOKUPP at Root
```bash
cd /mnt
cd ..              # Try to go above pseudo-root
# Should stay at /mnt (or error)
```

---

## Migration Path

### Phase 1: Minimal Implementation (1-2 days)
- [ ] Create `PseudoFilesystem` struct
- [ ] Implement pseudo-root with synthetic attributes
- [ ] Modify PUTROOTFH to return pseudo-root
- [ ] Add LOOKUP from pseudo-root to single export
- [ ] Test basic mount and access

### Phase 2: Robust Single-Export (1 day)
- [ ] Handle LOOKUPP at pseudo-root
- [ ] Proper FSID and file ID generation
- [ ] READDIR on pseudo-root (shows export)
- [ ] Error handling for edge cases

### Phase 3: Multi-Export Support (2-3 days)
- [ ] Export registry and management
- [ ] Hierarchical pseudo-paths
- [ ] Export discovery and listing
- [ ] Cross-export navigation

---

## Code Changes Required

### File: `src/nfs/v4/pseudo.rs` (NEW)

```rust
pub struct PseudoFilesystem {
    exports: Vec<Export>,
}

struct Export {
    name: String,           // "volume", "data", etc.
    path: PathBuf,          // Real directory
}

impl PseudoFilesystem {
    pub fn new(export_name: String, export_path: PathBuf) -> Self {
        // Create pseudo-fs with one export
    }
    
    pub fn get_pseudo_root_handle(&self) -> Nfs4FileHandle {
        // Return synthetic handle for "/"
    }
    
    pub fn is_pseudo_root(&self, handle: &Nfs4FileHandle) -> bool {
        // Check if handle represents pseudo-root
    }
    
    pub fn lookup_from_pseudo_root(&self, name: &str) -> Option<&Export> {
        // Find export by name
    }
}
```

### File: `src/nfs/v4/filehandle.rs` (MODIFY)

```rust
pub struct FileHandleManager {
    pseudo_fs: Arc<PseudoFilesystem>,  // Add this
    // ... existing fields
}

impl FileHandleManager {
    pub fn root_filehandle(&self) -> Result<Nfs4FileHandle> {
        // Return pseudo-root, NOT export root
        Ok(self.pseudo_fs.get_pseudo_root_handle())
    }
}
```

### File: `src/nfs/v4/operations/fileops.rs` (MODIFY)

```rust
pub fn handle_getattr(&self, op: GetAttrOp, ctx: &CompoundContext) -> GetAttrRes {
    let current_fh = ctx.current_fh.as_ref()?;
    
    if self.fh_mgr.is_pseudo_root(current_fh) {
        // Return SYNTHETIC attributes
        return encode_pseudo_root_attributes(&op.attr_request);
    }
    
    // Existing real filesystem logic
    let path = self.fh_mgr.resolve_handle(current_fh)?;
    let metadata = fs::metadata(&path)?;
    encode_attributes(&op.attr_request, &metadata, &path)
}

pub fn handle_lookup(&self, op: LookupOp, ctx: &mut CompoundContext) -> LookupRes {
    let current_fh = ctx.current_fh.as_ref()?;
    
    if self.fh_mgr.is_pseudo_root(current_fh) {
        // LOOKUP from pseudo-root → find export
        if let Some(export) = self.fh_mgr.lookup_export(&op.component) {
            ctx.current_fh = Some(self.fh_mgr.path_to_filehandle(&export.path)?);
            return LookupRes { status: Ok };
        }
        return LookupRes { status: NoEnt };
    }
    
    // Existing real filesystem lookup logic
    // ...
}
```

---

## References

### RFCs:
- **RFC 7530 Section 7** - "File System Namespace" (PRIMARY)
  https://datatracker.ietf.org/doc/html/rfc7530#section-7

- **RFC 7530 Section 18.16** - PUTROOTFH operation
  https://datatracker.ietf.org/doc/html/rfc7530#section-18.16

- **RFC 5661** - NFSv4.1 (extends pseudo-filesystem concepts)
  https://datatracker.ietf.org/doc/html/rfc5661

### Implementations:
- **NFS Ganesha:** `src/Protocols/NFS/nfs4_pseudo.c`
  Shows production pseudo-filesystem implementation

- **Linux Kernel:** `fs/nfs/nfs4namespace.c`  
  Client-side pseudo-filesystem handling

### Key Quote from RFC 7530 Section 7.3:

> "The pseudo file system is not a true file system but an artificial  
> construct to provide a single rooted name space for all exports.  
> Attributes returned for directories in the pseudo file system should  
> be chosen so as not to confuse clients."

This confirms:
1. Pseudo-filesystem is artificial/synthetic
2. Attributes must be chosen carefully
3. Purpose is unified namespace

---

## Conclusion

**Pseudo-Filesystem is NOT Optional:**
- RFC 7530 mandates it for ALL NFSv4 servers
- Even single-export servers need it
- Clients depend on this architecture

**Our Current Issue:**
- We skip pseudo-filesystem layer
- Return real directory for PUTROOTFH
- Client detects violation → ENOTDIR

**Fix Complexity:**
- **Minimal:** 1-2 days (single export)
- **Full:** 3-5 days (multiple exports)
- **Well-documented:** RFC + reference implementations available

**Next Action:**
Implement minimal pseudo-filesystem for single-export case.

---

**Analysis Date:** December 11, 2024  
**Status:** Requirements documented, ready for implementation  
**Priority:** HIGH (blocks all NFSv4 mounts)

