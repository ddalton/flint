## NFSv4.2 Implementation Status

**Last Updated:** December 8, 2025
**Current Phase:** Phase 1 - NFSv4.1 Foundation (IN PROGRESS)
**Lines Implemented:** ~600 / ~7,000 total (~8%)

---

## Phase 1: NFSv4.1 Foundation (~4,000 lines target)

### ✅ Completed (600 lines)

#### Protocol Definitions (`src/nfs/v4/protocol.rs`) - 580 lines
- [x] NFSv4 program and procedure numbers
- [x] NFSv4.0 operation codes (40 operations)
- [x] NFSv4.1 operation codes (19 operations)
- [x] NFSv4.2 operation codes (13 operations) - includes COPY, CLONE, ALLOCATE, DEALLOCATE, SEEK, READ_PLUS
- [x] Complete status code enum (90+ error codes)
- [x] Core types: StateId, Nfs4FileHandle, SessionId, ClientId
- [x] File attribute bitmap constants
- [x] File type enum
- [x] Access and open flags
- [x] Protocol constants

#### Module Structure (`src/nfs/v4/mod.rs`) - 20 lines
- [x] Module organization
- [x] Public exports

### 🔄 In Progress

#### NFSv4 XDR Encoding/Decoding (`src/nfs/v4/xdr.rs`) - Target: ~400 lines
**Status:** NOT STARTED
**Priority:** HIGH (required for all operations)
**Tasks:**
- [ ] XDR encoder for NFSv4 types
- [ ] XDR decoder for NFSv4 types
- [ ] StateId encode/decode
- [ ] FileHandle encode/decode
- [ ] Attribute bitmap encode/decode
- [ ] Variable-length array helpers
- [ ] String encode/decode (UTF-8)
- [ ] Discriminated union helpers (for READ_PLUS, etc.)

#### COMPOUND Operation Framework (`src/nfs/v4/compound.rs`) - Target: ~300 lines
**Status:** NOT STARTED
**Priority:** CRITICAL (foundation for everything)
**Tasks:**
- [ ] COMPOUND request parser
- [ ] COMPOUND response builder
- [ ] Operation dispatcher
- [ ] Current/saved filehandle management
- [ ] Error handling per-operation
- [ ] Status aggregation

#### File Handle Management (`src/nfs/v4/filehandle.rs`) - Target: ~200 lines
**Status:** NOT STARTED
**Priority:** HIGH
**Tasks:**
- [ ] File handle generation (unique, persistent)
- [ ] File handle validation
- [ ] File handle to path mapping
- [ ] Handle versioning (detect stale handles)
- [ ] Integration with existing VFS layer

---

### 📋 Not Started (Critical Path)

#### State Management (`src/nfs/v4/state/`) - Target: ~800 lines
**Priority:** CRITICAL
**Components:**

1. **Client Management** (`state/client.rs`) - ~200 lines
   - [ ] Client ID generation (EXCHANGE_ID)
   - [ ] Client registration
   - [ ] Client lease tracking
   - [ ] Client expiration

2. **Session Management** (`state/session.rs`) - ~250 lines
   - [ ] Session creation (CREATE_SESSION)
   - [ ] Session ID generation
   - [ ] Slot management (for exactly-once semantics)
   - [ ] Session destruction

3. **StateId Management** (`state/stateid.rs`) - ~200 lines
   - [ ] StateId generation
   - [ ] StateId validation
   - [ ] StateId sequence number management
   - [ ] StateId-to-state mapping

4. **Lease Management** (`state/lease.rs`) - ~150 lines
   - [ ] Lease renewal (via SEQUENCE)
   - [ ] Lease expiration detection
   - [ ] Grace period management
   - [ ] State cleanup on expiration

#### Basic Operations (`src/nfs/v4/operations/`) - Target: ~1,800 lines
**Priority:** HIGH
**Components:**

1. **Session Operations** (`operations/session.rs`) - ~300 lines
   - [ ] EXCHANGE_ID - Establish client ID
   - [ ] CREATE_SESSION - Create session
   - [ ] DESTROY_SESSION - Destroy session
   - [ ] SEQUENCE - Maintain session (required in every COMPOUND)
   - [ ] DESTROY_CLIENTID - Clean up client

2. **File Handle Operations** (`operations/filehandle.rs`) - ~150 lines
   - [ ] PUTROOTFH - Set current FH to root
   - [ ] PUTFH - Set current FH
   - [ ] GETFH - Get current FH
   - [ ] SAVEFH - Save current FH
   - [ ] RESTOREFH - Restore saved FH

3. **Lookup Operations** (`operations/lookup.rs`) - ~200 lines
   - [ ] LOOKUP - Lookup filename
   - [ ] LOOKUPP - Lookup parent directory
   - [ ] READDIR - Read directory

4. **I/O Operations** (`operations/io.rs`) - ~350 lines
   - [ ] OPEN - Open file (with stateids)
   - [ ] CLOSE - Close file
   - [ ] READ - Read from file
   - [ ] WRITE - Write to file
   - [ ] COMMIT - Commit cached data

5. **Attribute Operations** (`operations/attrs.rs`) - ~250 lines
   - [ ] GETATTR - Get file attributes
   - [ ] SETATTR - Set file attributes
   - [ ] ACCESS - Check access permissions

6. **Modify Operations** (`operations/modify.rs`) - ~300 lines
   - [ ] CREATE - Create file
   - [ ] REMOVE - Remove file
   - [ ] RENAME - Rename file
   - [ ] LINK - Create hard link
   - [ ] READLINK - Read symbolic link

7. **Utility Operations** (`operations/mod.rs`) - ~100 lines
   - [ ] NULL - Null operation
   - [ ] Operation trait definition
   - [ ] Common operation helpers

8. **Recovery Operations** (`operations/recovery.rs`) - ~150 lines
   - [ ] RECLAIM_COMPLETE - Signal reclaim done
   - [ ] TEST_STATEID - Test stateid validity
   - [ ] FREE_STATEID - Free stateid

#### Server Integration (`src/nfs/v4/server.rs`) - Target: ~500 lines
**Priority:** HIGH
**Tasks:**
- [ ] NFSv4 RPC handler
- [ ] Minor version negotiation
- [ ] COMPOUND operation dispatcher
- [ ] Integration with existing VFS backend
- [ ] Error mapping (VFS → NFSv4 status codes)
- [ ] Logging and metrics

**Total Phase 1:** ~600 / ~4,000 lines (15% complete)

---

## Phase 2: NFSv4.2 Performance Operations (~1,200 lines target)

**Status:** NOT STARTED
**Prerequisites:** Phase 1 complete, basic mount working

### Server-Side Copy & Cloning (~400 lines)
- [ ] COPY operation (sync and async modes)
- [ ] CLONE operation (atomic COW)
- [ ] CB_OFFLOAD callback (for async COPY)
- [ ] OFFLOAD_STATUS (check copy progress)
- [ ] OFFLOAD_CANCEL (cancel copy)
- [ ] COPY_NOTIFY (inter-server coordination)
- [ ] SPDK integration (snapshot/clone primitives)

### Space Management (~250 lines)
- [ ] ALLOCATE operation (pre-allocate space)
- [ ] DEALLOCATE operation (punch holes)
- [ ] SPDK integration (unmap/write_zeroes)
- [ ] Sparse file metadata tracking

### Sparse File Optimization (~450 lines)
- [ ] SEEK operation (find data/holes)
- [ ] READ_PLUS operation (enhanced read with holes)
- [ ] Hole detection logic
- [ ] Extent map integration with SPDK
- [ ] Discriminated union encoding (data vs hole)

### I/O Hints (~100 lines)
- [ ] IO_ADVISE operation
- [ ] Advice type parsing
- [ ] Integration with VFS/SPDK caching

**Total Phase 2:** 0 / ~1,200 lines (0% complete)

---

## Phase 3: Locking & Advanced Features (~1,800 lines target)

**Status:** NOT STARTED
**Prerequisites:** Phase 1 and 2 complete

### Locking (~1,200 lines)
- [ ] LOCK operation
- [ ] LOCKT operation (test lock)
- [ ] LOCKU operation (unlock)
- [ ] RELEASE_LOCKOWNER
- [ ] Lock state management
- [ ] Byte-range lock tracking
- [ ] Lock conflict detection
- [ ] Grace period for lock reclaim
- [ ] Reuse existing lock_manager.rs concepts

### Delegations (~600 lines - OPTIONAL)
- [ ] DELEGRETURN operation
- [ ] Read delegation management
- [ ] Write delegation management
- [ ] Callback channel (server → client)
- [ ] Delegation recall
- [ ] WANT_DELEGATION
- [ ] GET_DIR_DELEGATION

**Total Phase 3:** 0 / ~1,800 lines (0% complete)

---

## Overall Progress

| Phase | Target Lines | Completed | Status | % Complete |
|-------|-------------|-----------|--------|------------|
| Phase 1 (Foundation) | 4,000 | 600 | 🔄 IN PROGRESS | 15% |
| Phase 2 (Performance) | 1,200 | 0 | ⏸️ PENDING | 0% |
| Phase 3 (Locking) | 1,800 | 0 | ⏸️ PENDING | 0% |
| **TOTAL** | **7,000** | **600** | **🔄 IN PROGRESS** | **8%** |

---

## Critical Path (Next Steps)

1. **Implement NFSv4 XDR layer** (~400 lines)
   - Required for all operations
   - Can reuse some concepts from existing `src/nfs/xdr.rs`

2. **Implement COMPOUND framework** (~300 lines)
   - Foundation for all NFSv4 operations
   - Operation dispatcher
   - File handle context management

3. **Implement file handle management** (~200 lines)
   - Generate persistent handles
   - Map handles to VFS paths

4. **Implement session management** (~450 lines)
   - EXCHANGE_ID, CREATE_SESSION, SEQUENCE
   - Client and session tracking
   - Slot management

5. **Implement basic operations** (~1,800 lines)
   - Start with PUTROOTFH, GETFH, GETATTR
   - Then add LOOKUP, READDIR
   - Then OPEN, READ, WRITE, CLOSE
   - Build incrementally, test each operation

6. **Integration and testing**
   - Mount test from Linux client
   - Basic file I/O
   - Connectathon basic tests

---

## Testing Strategy

### Unit Tests
- [ ] XDR encoding/decoding tests
- [ ] StateId generation tests
- [ ] File handle tests
- [ ] Session management tests

### Integration Tests
- [ ] Mount NFSv4.1 from Linux client
- [ ] Create, read, write, delete files
- [ ] Directory operations
- [ ] Multiple sessions
- [ ] State recovery

### Connectathon Tests
- [ ] Basic tests (Phase 1 target)
- [ ] Lock tests (Phase 3 target)
- [ ] All tests pass (final goal)

---

## Implementation Notes

### Reusable Components from NFSv3

Can reuse from existing implementation:
- ✅ VFS backend (`src/nfs/vfs.rs`) - ~720 lines
- ✅ RPC infrastructure (`src/nfs/rpc.rs`) - ~268 lines
- ⚠️  XDR concepts (different format for v4)
- ⚠️  Lock manager concepts (different state model)

### New Concepts for NFSv4

- **Stateids**: 128-bit identifiers for all state
- **Sessions**: Connection-based state management
- **Slots**: Exactly-once semantics within session
- **Lease Management**: Active state renewal (90s default)
- **Grace Period**: 90s after restart for reclaim
- **COMPOUND**: All operations wrapped in compound
- **Current/Saved FH**: Implicit file handle context

### Performance Considerations

- Session state must be fast (in-memory HashMap)
- StateId lookups must be O(1)
- File handle generation must be deterministic
- Lease renewal is frequent (every ~60s per client)
- COMPOUND operations should be atomic where possible

---

## Estimated Timeline

Based on ~7,000 lines to implement:

- **Phase 1** (Foundation): ~2-3 weeks (4,000 lines)
  - Week 1: XDR, COMPOUND, file handles, state management
  - Week 2: Session operations, basic file operations
  - Week 3: Testing, debugging, refinement

- **Phase 2** (Performance): ~1 week (1,200 lines)
  - COPY/CLONE operations
  - ALLOCATE/DEALLOCATE
  - SEEK/READ_PLUS
  - SPDK integration

- **Phase 3** (Locking): ~1-2 weeks (1,800 lines)
  - Lock operations
  - State recovery
  - Connectathon lock tests

- **Testing & Debugging**: ~1-2 weeks
  - Connectathon test suite
  - Multi-client stress tests
  - Performance benchmarks

**Total:** 5-8 weeks of focused development

---

## Questions to Resolve

1. **File handle format**: Use existing filehandle.rs or new format?
2. **State storage**: In-memory only or persist to disk?
3. **SPDK integration**: What APIs available for clone/snapshot/unmap?
4. **Grace period**: 90s default or configurable?
5. **Callback channel**: Implement for delegations or skip?

---

**Next Immediate Task:** Implement NFSv4 XDR encoding/decoding (~400 lines)
