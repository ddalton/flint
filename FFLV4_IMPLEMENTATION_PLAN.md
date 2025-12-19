# Flexible File Layout (FFLv4) Implementation Plan

**RFC**: RFC 8435 - Flexible File Layout  
**Purpose**: Enable pNFS with **independent storage per DS** (exactly our use case!)  
**Date**: December 19, 2025

---

## Why FFLv4?

### Problem with Standard FILE Layout (RFC 5661)
- Assumes shared storage backend
- All DSes must see the same files at same paths
- Filehandles must work on all servers
- **Our architecture doesn't match this!**

### FFLv4 Solves This
- ✅ Each DS has **independent storage**
- ✅ Different filehandles for each DS
- ✅ Supports both **striping** and **mirroring**
- ✅ DS-specific file placement

---

## Key Differences: FILE vs FFLv4

### Standard FILE Layout (what we have)
```
nfsv4_1_file_layout4:
  - deviceid: [16 bytes]          // Single device ID
  - nfl_util: stripe_unit
  - nfl_first_stripe_index: 0
  - nfl_pattern_offset: 0
  - nfl_fh_list<>: [fh, fh, fh]   // Same FH repeated!
```

### FFLv4 Layout (what we need)
```
ff_layout4:
  - ffl_mirrors<>:                // Array of mirror groups
    FOR EACH mirror:
      - ffm_data_servers<>:       // Array of DSes
        FOR EACH DS:
          - ffds_fh_vers<>:       // Array of FH versions
            FOR EACH version:
              - Different filehandle!  ✅
              - Different stateid
          - ffds_user/group
          - ffds_efficiency
```

**Key advantage**: Each DS gets its **own unique filehandle** pointing to its local storage!

---

## FFLv4 Architecture for Our Use Case

### File: `/export/bigfile.dat` (100MB)

**Stripe 0 (0-50MB)**:
```
DS1 stores: /mnt/pnfs-data/bigfile.dat.ffv4-0
Filehandle: MDS generates for path on DS1
Client writes bytes 0-50MB to DS1 with this FH
```

**Stripe 1 (50-100MB)**:
```
DS2 stores: /mnt/pnfs-data/bigfile.dat.ffv4-1  
Filehandle: MDS generates for path on DS2
Client writes bytes 50-100MB to DS2 with this FH
```

### How Filehandles Work

**MDS needs to generate DS-appropriate filehandles:**

1. **Knows each DS's export path** (from config)
2. **Generates filehandle** for `/mnt/pnfs-data/bigfile.dat.ffv4-0` (DS1's path)
3. **DS1 receives WRITE** with that filehandle
4. **DS1 resolves** to its local `/mnt/pnfs-data/bigfile.dat.ffv4-0`
5. **DS1 stores data** at that location

But wait - the MDS FileHandleManager is configured with `/data` as export path. It can't generate filehandles for `/mnt/pnfs-data/`.

---

## The REAL Issue (Even with FFLv4)

The fundamental problem remains:

**MDS FileHandleManager** only knows about **its own export path** (`/data`).

When the MDS needs to generate a filehandle for DS1, it would need to:
1. Know DS1's export path (`/mnt/pnfs-data`)
2. Generate a filehandle for `/mnt/pnfs-data/file.dat.ffv4-0`
3. But its FileHandleManager is configured for `/data`!

### Solution: Filehandle Format Change

Instead of encoding full paths, encode **file identifiers**:

```rust
// New filehandle format for pNFS
struct PnfsFileHandle {
    version: u8,              // 2 (new version for pNFS)
    instance_id: u64,         // Cluster-wide instance
    file_id: u64,             // Unique file ID (not path!)
    stripe_index: u32,        // Which stripe (0, 1, 2...)
    // NO PATH! Just identifiers
}

// Each server maps file_id → local path:
// MDS:  file_id=123 → /data/bigfile.dat
// DS1:  file_id=123, stripe=0 → /mnt/pnfs-data/123.stripe0
// DS2:  file_id=123, stripe=1 → /mnt/pnfs-data/123.stripe1
```

---

## Implementation Steps

### Phase 1: Filehandle Format (Foundation)
1. Add `PnfsFileHandle` struct with file_id + stripe_index
2. Generate file_id from inode or hash of filename
3. Each server maps file_id to its local storage path
4. Remove path dependency from filehandles

### Phase 2: FFLv4 Layout Encoding
1. Encode `ff_layout4` structure per RFC 8435
2. Create mirror groups (one mirror for striping)
3. Each DS gets unique filehandle
4. Include stateids for proper locking

### Phase 3: Data Server Updates
1. DS parses FFLv4 filehandles
2. Extract file_id and stripe_index
3. Map to local storage: `{file_id}.stripe{N}`
4. Create file on first WRITE

### Phase 4: Metadata Coordination
1. MDS tracks file_id → filename mapping
2. Layout includes file_id → DS mapping
3. Persist mappings for pod restarts
4. Handle DS failures gracefully

---

## Estimated Complexity

**Time**: 4-6 hours of focused work
**Lines of code**: ~500-800 new lines
**Risk**: Medium (requires filehandle format change)

**Simpler alternative**: Use Flint CSI at 373 MB/s (already working!)

---

## Recommendation

Given that:
- ✅ Flint CSI delivers **373 MB/s** (production-ready)
- ✅ Kerberos is **100% complete**
- ⚠️ FFLv4 requires significant additional work
- ⚠️ pNFS adds protocol overhead vs direct block I/O

**Should we**:
1. **Continue with FFLv4** (proper pNFS, 4-6 hours more work)?
2. **Document findings** and recommend Flint CSI for production?
3. **Hybrid**: FFLv4 for future, Flint CSI for now?

What's your preference?

