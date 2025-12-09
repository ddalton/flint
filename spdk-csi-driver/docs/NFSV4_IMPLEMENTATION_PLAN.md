# NFSv4.2 Implementation Plan for SPDK CSI Driver

## Executive Summary

Converting from NFSv3 (+ MOUNT + NLM + NSM) to NFSv4.2 for cleaner architecture, better Kubernetes integration, and **dramatic performance improvements**.

**Goal:** Production-ready NFSv4.2 server leveraging RFC 7862 performance features

**Key Performance Features:**
- Server-side COPY (no network transfer for clones)
- CLONE operation (atomic copy-on-write)
- ALLOCATE/DEALLOCATE (efficient space management with SPDK)
- SEEK (sparse file optimization)
- READ_PLUS (avoid transmitting holes)
- IO_ADVISE (I/O pattern hints)

## Current State vs Target

### Current Implementation (NFSv3)
- ✅ NFSv3 core operations: ~1,580 lines
- ✅ MOUNT protocol: ~230 lines
- ✅ NLM protocol: ~754 lines (localhost only)
- ❌ NSM protocol: Not implemented (~1,000-1,600 lines needed)
- ❌ Remote locking: Broken (requires NSM)
- **Total: ~5,500 lines across 5 protocols**

### Target Implementation (NFSv4.2)
- Single unified protocol (no MOUNT/NLM/NSM)
- Single port (2049)
- Integrated locking (no separate NLM)
- Built-in state management (no NSM)
- **Performance operations** (COPY, CLONE, ALLOCATE, DEALLOCATE, SEEK, READ_PLUS)
- **Estimated: ~5,000-7,000 lines in clean architecture**
  - NFSv4.1 foundation: ~4,000 lines
  - NFSv4.2 performance ops: ~1,000 lines

## Implementation Phases

### Phase 1: NFSv4.1 Minimal (RWX Support)
**Goal:** Basic file operations for ReadWriteMany volumes

**Operations to Implement:**

#### Core Infrastructure
- [ ] NFSv4 RPC layer (different from NFSv3)
- [ ] COMPOUND operation framework
- [ ] XDR encoding/decoding for NFSv4
- [ ] File handle format (NFSv4 uses different format)
- [ ] State ID management (seqid + 12-byte opaque)

#### Required Operations (RFC 7530)
- [ ] **NULL** - Null procedure
- [ ] **COMPOUND** - Compound operations framework
- [ ] **ACCESS** - Check access rights
- [ ] **CLOSE** - Close file
- [ ] **COMMIT** - Commit cached data
- [ ] **CREATE** - Create non-regular file
- [ ] **GETATTR** - Get attributes
- [ ] **GETFH** - Get current filehandle
- [ ] **LINK** - Create link
- [ ] **LOOKUP** - Lookup filename
- [ ] **NVERIFY** - Verify difference in attributes
- [ ] **OPEN** - Open file
- [ ] **OPENATTR** - Open named attribute directory
- [ ] **PUTFH** - Set current filehandle
- [ ] **PUTPUBFH** - Set public filehandle
- [ ] **PUTROOTFH** - Set root filehandle
- [ ] **READ** - Read from file
- [ ] **READDIR** - Read directory
- [ ] **READLINK** - Read symbolic link
- [ ] **REMOVE** - Remove filesystem object
- [ ] **RENAME** - Rename directory entry
- [ ] **RENEW** - Renew lease (deprecated in v4.1, but may be needed)
- [ ] **RESTOREFH** - Restore saved filehandle
- [ ] **SAVEFH** - Save current filehandle
- [ ] **SECINFO** - Obtain security info
- [ ] **SETATTR** - Set attributes
- [ ] **SETCLIENTID** - Negotiate client ID (v4.0 only)
- [ ] **SETCLIENTID_CONFIRM** - Confirm client ID (v4.0 only)
- [ ] **VERIFY** - Verify same attributes
- [ ] **WRITE** - Write to file

#### NFSv4.1 Specific Operations
- [ ] **EXCHANGE_ID** - Establish client ID (replaces SETCLIENTID)
- [ ] **CREATE_SESSION** - Create session
- [ ] **DESTROY_SESSION** - Destroy session
- [ ] **SEQUENCE** - Maintain session (slot management)
- [ ] **RECLAIM_COMPLETE** - Indicate reclaim done
- [ ] **DESTROY_CLIENTID** - Destroy unused client ID

**Estimated Lines:** ~3,500 lines

**Deliverable:** Mount NFSv4.1 volume, read/write files, basic directory operations

**Test Criteria:**
- Mount from Linux client: `mount -t nfs -o vers=4.1 server:/export /mnt`
- Create, read, write, delete files
- Directory operations (mkdir, rmdir, ls)
- Connectathon basic tests pass

---

### Phase 2: NFSv4.2 Performance Operations (RFC 7862)
**Goal:** Leverage SPDK backend for maximum performance with modern NFS features

**High-Priority Operations:**

#### Server-Side Copy & Cloning
- [ ] **COPY** - Server-side file copy (eliminates network transfer)
  - Synchronous and asynchronous modes
  - Intra-server and inter-server support
  - CB_OFFLOAD callback for async status
  - **Use Case:** Container image cloning, database snapshots

- [ ] **CLONE** - Atomic copy-on-write cloning
  - Synchronous operation (simpler than COPY)
  - Same-server only
  - Atomic guarantee
  - **Use Case:** Instant volume clones, dev/test environments
  - **SPDK Benefit:** Can leverage SPDK snapshot/clone primitives

#### Space Management
- [ ] **ALLOCATE** - Pre-allocate space without zeroing
  - Guarantees future writes won't fail with ENOSPC
  - **Use Case:** VM disk images, database files
  - **SPDK Benefit:** Maps directly to SPDK unmap/write_zeroes

- [ ] **DEALLOCATE** - Punch holes/trim blocks
  - Release unused space (sparse files)
  - **Use Case:** VM thin provisioning, space reclamation
  - **SPDK Benefit:** SPDK TRIM/UNMAP for actual block deallocation

#### Sparse File Optimization
- [ ] **SEEK** - Find next data or hole offset
  - DATA: find next non-hole region
  - HOLE: find next hole region
  - **Use Case:** Efficient sparse file scanning
  - **SPDK Benefit:** Can query SPDK extent map

- [ ] **READ_PLUS** - Enhanced READ with hole detection
  - Returns discriminated union of data or hole metadata
  - Avoids transmitting zeros over network
  - **Use Case:** VM disk image reads, sparse file transfers
  - **SPDK Benefit:** Significant network bandwidth savings

#### I/O Optimization Hints
- [ ] **IO_ADVISE** - Application I/O pattern hints
  - WILLNEED, DONTNEED, WILLNEED_OPPORTUNISTIC, DONTNEED_OPPORTUNISTIC
  - NORMAL, SEQUENTIAL, RANDOM, NOREUSE
  - **Use Case:** Optimize caching and prefetch strategies
  - **SPDK Benefit:** Can tune SPDK read-ahead policies

**Optional Operations:**
- [ ] **WRITE_SAME** - Application Data Block (ADB) initialization
  - Repeated pattern writes for block device formatting
  - **Use Case:** Virtual disk initialization

**Estimated Lines:** ~1,200 lines
- COPY/CLONE: ~400 lines
- ALLOCATE/DEALLOCATE: ~250 lines
- SEEK: ~150 lines
- READ_PLUS: ~300 lines
- IO_ADVISE: ~100 lines

**Deliverable:** High-performance NFS with SPDK-optimized operations

**Test Criteria:**
- COPY operation works (measure speedup vs network copy)
- CLONE creates instant clones (test with SPDK snapshots)
- ALLOCATE/DEALLOCATE manage space correctly
- SEEK finds holes efficiently
- READ_PLUS avoids transmitting zeros
- Performance benchmarks show significant improvements

**SPDK Integration Points:**
```rust
// Example: DEALLOCATE maps to SPDK unmap
async fn handle_deallocate(&self, file: &File, offset: u64, length: u64) -> Result<()> {
    // Call SPDK unmap to actually free blocks
    self.spdk.unmap(file.volume_id, offset, length).await?;
    // Update file metadata
    Ok(())
}

// Example: CLONE maps to SPDK snapshot
async fn handle_clone(&self, src: &File, dst: &File) -> Result<()> {
    // Use SPDK snapshot/clone instead of copying data
    self.spdk.create_clone(src.volume_id, dst.volume_id).await?;
    Ok(())
}
```

---

### Phase 3: NFSv4.1 Locking & Advanced Features
**Goal:** Full POSIX locking and production-ready robustness

**Locking Operations:**
- [ ] **LOCK** - Create lock
- [ ] **LOCKT** - Test for lock
- [ ] **LOCKU** - Unlock file
- [ ] **RELEASE_LOCKOWNER** - Release lock-owner state

**State Management:**
- [ ] Lock state tracking (stateid management)
- [ ] Lock owner identification
- [ ] Byte-range lock tracking
- [ ] Share reservations (OPEN with deny modes)
- [ ] Lease management (default 90 seconds)
- [ ] Grace period implementation

**Recovery Mechanisms:**
- [ ] Client restart detection
- [ ] Server restart detection
- [ ] Grace period for lock reclaim
- [ ] RECLAIM_COMPLETE handling

**Advanced Features (Optional):**
- [ ] **DELEGRETURN** - Return delegation
- [ ] Read delegations (client-side caching)
- [ ] Write delegations (exclusive access)
- [ ] Delegation recall mechanism
- [ ] Callback channel (server -> client)
- [ ] **TEST_STATEID** - Test stateid validity
- [ ] **FREE_STATEID** - Free stateid

**Estimated Lines:** ~1,800 lines
- Locking: ~1,200 lines
- Delegations: ~600 lines

**Deliverable:** Production-ready NFSv4.2 server with full locking

**Test Criteria:**
- `fcntl()` locks work across clients
- `flock()` locks work across clients
- Lock conflict detection working
- Lock recovery after server restart
- All Connectathon tests pass (including lock tests)
- Stress tests pass (multiple clients, many files)
- Failover/recovery tests pass

---

## Connectathon Test Suite Requirements

The Connectathon Test Suite includes:

### Basic Tests
- File and directory operations
- Large file I/O
- Permissions and attributes
- Symbolic links
- Special files

### General Tests
- Rename operations
- Concurrent access
- Large directory operations

### Lock Tests (Phase 2)
- Exclusive locks
- Shared locks
- Lock conflict detection
- Lock upgrade/downgrade
- Mandatory locking

### Special Tests (Phase 3)
- Large file support (>2GB)
- Negative tests (error handling)
- Edge cases

## Technical Architecture

### NFSv4 vs NFSv3 Key Differences

| Aspect | NFSv3 | NFSv4.1 |
|--------|-------|---------|
| **Operations** | Simple, stateless | Compound, stateful |
| **File handles** | Opaque bytes | Persistent, hierarchical |
| **State** | External (NLM/NSM) | Built-in (stateids) |
| **Locking** | Separate protocol | Integrated operations |
| **Sessions** | None | Session-based |
| **Recovery** | NSM notifications | Grace period + reclaim |
| **Mount** | Separate protocol | PUTROOTFH operation |
| **Ports** | Multiple (5+) | Single (2049) |

### State Management

NFSv4 uses **stateids** (128-bit identifiers):

```
stateid {
    uint32_t seqid;        // Sequence number (updated on each change)
    opaque   other[12];    // Unique identifier
}
```

**State Types:**
- **Open state:** Tracks opened files
- **Lock state:** Tracks byte-range locks
- **Delegation state:** Tracks delegated files (Phase 3)
- **Layout state:** Tracks pNFS layouts (optional)

**Lease Management:**
- Default lease time: 90 seconds
- Clients must renew via SEQUENCE operation (v4.1) or RENEW (v4.0)
- Server expires stale state after lease timeout

### Grace Period

After server restart:
1. Server enters grace period (90 seconds)
2. Only RECLAIM operations allowed
3. Clients reclaim their previous state
4. After grace, normal operations resume
5. Any un-reclaimed state is lost

## Code Structure

Proposed file organization:

```
src/nfs/
├── v4/
│   ├── mod.rs              # NFSv4 module entry
│   ├── compound.rs         # COMPOUND operation framework
│   ├── operations/
│   │   ├── mod.rs
│   │   ├── filehandle.rs   # GETFH, PUTFH, SAVEFH, RESTOREFH
│   │   ├── access.rs       # ACCESS, OPEN, CLOSE
│   │   ├── io.rs           # READ, WRITE, COMMIT
│   │   ├── lookup.rs       # LOOKUP, READDIR
│   │   ├── attrs.rs        # GETATTR, SETATTR
│   │   ├── modify.rs       # CREATE, REMOVE, RENAME, LINK
│   │   ├── lock.rs         # LOCK, LOCKT, LOCKU (Phase 2)
│   │   └── session.rs      # EXCHANGE_ID, CREATE_SESSION, SEQUENCE
│   ├── state/
│   │   ├── mod.rs
│   │   ├── client.rs       # Client ID management
│   │   ├── session.rs      # Session management
│   │   ├── stateid.rs      # Stateid generation and tracking
│   │   ├── open.rs         # Open file state
│   │   ├── lock.rs         # Lock state (Phase 2)
│   │   └── lease.rs        # Lease renewal and expiration
│   ├── protocol.rs         # NFSv4 protocol constants and types
│   ├── xdr.rs              # XDR encoding/decoding for NFSv4
│   └── filehandle.rs       # NFSv4 file handle format
├── server.rs               # Updated to support both v3 and v4
├── vfs.rs                  # Shared VFS backend (reusable)
└── rpc.rs                  # Shared RPC infrastructure (reusable)
```

## Reusable Components

From existing NFSv3 implementation:

- ✅ **VFS backend** (vfs.rs) - File operations on SPDK volumes
- ✅ **RPC infrastructure** (rpc.rs) - Basic RPC handling
- ✅ **File handle generation** (concept, different format for v4)
- ✅ **Lock manager logic** (nlm.rs, lock_manager.rs) - Concepts apply to v4
- ✅ **Testing infrastructure** (tests.rs)

## Migration Strategy

### Option A: Clean Slate (Recommended)
- Create new `src/nfs/v4/` directory
- Implement NFSv4 from scratch
- Reuse VFS and RPC infrastructure
- Keep NFSv3 code for reference
- Switch to v4 when Phase 1 complete

**Pros:**
- Clean separation
- No risk to existing code
- Can compare implementations
- Easy rollback

**Cons:**
- Some code duplication initially

### Option B: Incremental Migration
- Modify existing server to support both v3 and v4
- Add protocol version detection
- Share maximum code

**Pros:**
- Gradual transition
- Support both protocols simultaneously

**Cons:**
- More complex server logic
- Risk of breaking existing v3 code
- Harder to maintain

**Recommendation: Option A** - Clean implementation, switch when ready

## Timeline Estimate

Based on ~4,000-6,000 lines to implement:

- **Phase 1 (Minimal):** ~3,500 lines, ~2-3 weeks full-time
- **Phase 2 (Locking):** ~1,500 lines, ~1-2 weeks full-time
- **Phase 3 (Performance):** ~1,000 lines, ~1 week full-time
- **Testing & Debugging:** ~1-2 weeks
- **Total:** 5-8 weeks full-time development

**Critical Path:**
1. COMPOUND operation framework (foundation)
2. File handle and stateid management (core infrastructure)
3. Basic operations (OPEN, READ, WRITE, CLOSE)
4. Session management (EXCHANGE_ID, CREATE_SESSION, SEQUENCE)
5. Testing and iteration

## References

### Official Specifications
- **RFC 7530:** NFSv4 Protocol (base specification)
- **RFC 8881:** NFSv4.1 Protocol (sessions and pNFS)
- **RFC 7862:** NFSv4.2 Protocol (**PRIMARY TARGET** - performance operations)

### Implementation Guides
- Linux NFS source code: `fs/nfsd/` in kernel
- NFS Ganesha: User-space reference implementation
- FreeBSD NFS server: Alternative reference

### Test Suites
- Connectathon NFS Test Suite (basic functionality)
- pynfs (NFSv4-specific tests, Python-based)
- Linux Test Project NFS tests

## Success Criteria

### Phase 1 Complete (NFSv4.1 Foundation)
- [ ] Mount NFSv4.1/4.2 volume from Linux client
- [ ] Create, read, write, delete files
- [ ] Directory operations work
- [ ] Connectathon basic tests pass
- [ ] Performance comparable to NFSv3 for basic I/O

### Phase 2 Complete (NFSv4.2 Performance)
- [ ] COPY operation works - measure speedup vs traditional copy
- [ ] CLONE creates instant clones using SPDK snapshots
- [ ] ALLOCATE pre-allocates space efficiently
- [ ] DEALLOCATE/TRIM reclaims blocks in SPDK backend
- [ ] SEEK finds holes without reading entire files
- [ ] READ_PLUS skips transmitting zero regions
- [ ] Performance benchmarks show 2-10x improvements for relevant workloads

### Phase 3 Complete (Locking & Production-Ready)
- [ ] POSIX locks work across multiple clients
- [ ] Lock conflicts detected correctly
- [ ] Grace period and recovery work
- [ ] All Connectathon tests pass (including lock tests)
- [ ] Multi-client stress tests pass
- [ ] Failover/recovery tests pass
- [ ] Production-ready for CSI driver deployment

## Why NFSv4.2 + SPDK is a Powerful Combination

### Performance Synergies

**Traditional NFS Problem:** Operations like file copy require:
1. Client READs data from source → Network transfer
2. Client WRITEs data to dest → Network transfer
**Result:** 2x network bandwidth, high latency

**NFSv4.2 COPY Solution:**
1. Client sends COPY command (tiny request)
2. Server copies internally (SPDK-to-SPDK, no network)
**Result:** Near-instant copy, zero network data transfer

### SPDK-Specific Optimizations

| NFSv4.2 Operation | SPDK Backend Integration | Performance Gain |
|-------------------|-------------------------|------------------|
| **CLONE** | `spdk_bdev_create_snapshot()` + clone | Instant volume clones (ms vs minutes) |
| **COPY** | SPDK-to-SPDK block copy | 10-100x faster than network copy |
| **ALLOCATE** | `spdk_bdev_write_zeroes()` with unmap flag | Space reservation without I/O |
| **DEALLOCATE** | `spdk_bdev_unmap()` (TRIM) | Actual block deallocation, thin provisioning |
| **SEEK** | Query SPDK extent map | Instant hole detection (no reads) |
| **READ_PLUS** | Return hole metadata instead of zeros | 10-100x bandwidth savings for sparse files |

### Use Case Examples

**Container Image Distribution:**
```
Traditional: Pull 1GB image → 1GB network transfer per pod
NFSv4.2 CLONE: Pull once → CLONE operation for each pod (instant, ~1ms)
Benefit: 100x faster pod startup for image-heavy workloads
```

**Database Snapshots:**
```
Traditional: pg_dump → hours, full data copy
NFSv4.2 CLONE: Instant snapshot via SPDK clone
Benefit: Backup windows from hours → seconds
```

**VM Thin Provisioning:**
```
Traditional: Allocate 100GB → writes 100GB of zeros
NFSv4.2 ALLOCATE: Reserve 100GB, DEALLOCATE unused → sparse file
READ_PLUS: Skip reading/transmitting holes
Benefit: 10x storage efficiency, 10x faster VM creation
```

**Development Environments:**
```
Traditional: Each dev clones entire dataset (slow, space-hungry)
NFSv4.2 CLONE: Instant copy-on-write clones
Benefit: 100 devs sharing base dataset, isolated changes
```

### CSI Driver Advantages

For Kubernetes CSI driver specifically:

1. **Volume Cloning** - PVC clones are instant (CLONE operation)
2. **Snapshots** - Volume snapshots leverage SPDK snapshots (COPY/CLONE)
3. **Thin Provisioning** - Efficient space usage (ALLOCATE/DEALLOCATE)
4. **RWX Performance** - Multiple pods access with optimizations (READ_PLUS, IO_ADVISE)
5. **Single Port** - Simple Kubernetes Service configuration (no portmapper)

### Expected Performance Improvements

Based on RFC 7862 benchmarks and SPDK capabilities:

- **File cloning:** 100-1000x faster (network copy → instant SPDK snapshot)
- **Sparse file I/O:** 10-100x bandwidth reduction (READ_PLUS skips holes)
- **Space reclamation:** Actual block deallocation vs file-level (DEALLOCATE → SPDK unmap)
- **Large file allocation:** 10-100x faster (no zero writes with ALLOCATE)

## Notes

- NFSv4.2 builds on NFSv4.1 foundation - must implement NFSv4.1 first
- Session management is critical (every operation needs SEQUENCE)
- Stateid management is the heart of NFSv4 - get this right first
- Grace period is mandatory for proper recovery
- Test frequently with real Linux clients, not just unit tests
- Connectathon will reveal edge cases - expect iterations
- **NFSv4.2 performance operations are optional** - can deploy Phase 1, add Phase 2 later
- SPDK integration requires SPDK API for snapshots, unmap, write_zeroes

---

**Next Step:** Begin Phase 1 implementation with COMPOUND operation framework
