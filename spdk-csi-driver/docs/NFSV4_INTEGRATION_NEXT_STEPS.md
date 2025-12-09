# NFSv4.2 Server Integration - Next Steps

**Date:** December 9, 2025
**Status:** 🚧 IN PROGRESS - Server integration blocked by API mismatches
**Priority:** HIGH - Complete integration to enable concurrent I/O testing

---

## Executive Summary

The NFSv4.2 protocol implementation is **100% complete** (dispatcher, operations, state management, CREATE/REMOVE). However, the integration into the `flint-nfs-server` binary is **incomplete** due to architectural differences between the server TCP layer and the NFSv4.2 module APIs.

**Current State:**
- ✅ NFSv4.2 protocol layer: COMPLETE (6,400 lines of code)
- ✅ All operations: COMPLETE (CREATE, REMOVE, READ, WRITE, LOCK, etc.)
- ✅ State management: COMPLETE (sessions, locks, leases)
- ⚠️ Server integration: BLOCKED (15 compilation errors)
- ❌ Testing: NOT STARTED (waiting for server integration)

**Goal:** Fix all compilation errors and successfully start NFSv4.2 server for testing.

---

## What We've Accomplished

### 1. Complete NFSv4.2 Implementation ✅

**Files Created/Modified (Previous Session):**
- `src/nfs/v4/dispatcher.rs` (700 lines) - COMPOUND request dispatcher
- `src/nfs/v4/operations/fileops.rs` (744 lines) - File operations including CREATE/REMOVE
- `src/nfs/v4/operations/ioops.rs` (400 lines) - I/O operations
- `src/nfs/v4/operations/session.rs` (450 lines) - Session management
- `src/nfs/v4/operations/lockops.rs` (650 lines) - Lock operations
- `src/nfs/v4/operations/perfops.rs` (550 lines) - Performance operations
- `src/nfs/v4/state/` (1,200 lines) - Lock-free state management
- `src/nfs/v4/filehandle.rs` (350 lines) - File handle management

**Operations Implemented:**
- Session: EXCHANGE_ID, CREATE_SESSION, SEQUENCE, DESTROY_SESSION
- File: PUTROOTFH, PUTFH, GETFH, SAVEFH, RESTOREFH, LOOKUP, LOOKUPP, READDIR
- Attributes: GETATTR, SETATTR, ACCESS
- I/O: OPEN, CLOSE, READ, WRITE, COMMIT
- **File Creation:** CREATE (files and directories) ✅ **NEW**
- **File Deletion:** REMOVE (files and directories) ✅ **NEW**
- Locks: LOCK, LOCKT, LOCKU, RELEASE_LOCKOWNER
- Performance: COPY, CLONE, ALLOCATE, DEALLOCATE, SEEK, READ_PLUS

### 2. Server Integration Started (This Session) ⚠️

**Files Created:**
- `src/nfs/server_v4.rs` (295 lines) - NFSv4.2 TCP server implementation

**Files Modified:**
- `src/nfs/mod.rs` - Updated exports to use server_v4 instead of server
- `src/nfs_main.rs` - Updated documentation and banners for NFSv4.2

**Architecture:**
```
┌─────────────────────────────────────────────────────────────┐
│ nfs_main.rs (Binary Entry Point)                           │
│   ↓                                                         │
│ server_v4.rs (TCP Transport) ← NEEDS FIXING                │
│   ↓                                                         │
│ RPC Layer (Call/Reply)                                     │
│   ↓                                                         │
│ NFSv4 COMPOUND Dispatcher ← API MISMATCH                   │
│   ↓                                                         │
│ Operation Handlers (Session, File, I/O, Lock, Perf)        │
│   ↓                                                         │
│ State Management (DashMap-based)                           │
│   ↓                                                         │
│ File Handle Manager                                        │
│   ↓                                                         │
│ LocalFilesystem (VFS Backend) ← NOT USED BY V4             │
└─────────────────────────────────────────────────────────────┘
```

---

## Current Blockers

### Compilation Errors (15 total)

#### Error Category 1: API Signature Mismatches (6 errors)

**Location:** `src/nfs/server_v4.rs:64-68`

**Issue:** Operation handler constructors have different signatures than assumed

**Current (INCORRECT) Code:**
```rust
let session_handler = Arc::new(SessionOperationHandler::new(state_mgr.clone()));
let file_handler = Arc::new(FileOperationHandler::new(fh_mgr.clone(), fs.clone())); // ← ERROR
let io_handler = Arc::new(IoOperationHandler::new(fh_mgr.clone(), state_mgr.clone(), fs.clone())); // ← ERROR
let lock_handler = Arc::new(LockOperationHandler::new(state_mgr.clone())); // ← ERROR
let perf_handler = Arc::new(PerfOperationHandler::new(fh_mgr.clone(), state_mgr.clone(), fs.clone())); // ← ERROR
```

**Actual Signatures:**
```rust
// src/nfs/v4/operations/session.rs:170
impl SessionOperationHandler {
    pub fn new(state_mgr: Arc<StateManager>) -> Self
}

// src/nfs/v4/operations/fileops.rs:206
impl FileOperationHandler {
    pub fn new(fh_mgr: Arc<FileHandleManager>) -> Self  // ← Only 1 arg
}

// src/nfs/v4/operations/ioops.rs:187
impl IoOperationHandler {
    pub fn new(state_mgr: Arc<StateManager>) -> Self  // ← Only 1 arg
}

// src/nfs/v4/operations/lockops.rs:348
impl LockOperationHandler {
    pub fn new(state_mgr: Arc<StateManager>, lock_mgr: Arc<LockManager>) -> Self  // ← 2 args
}

// src/nfs/v4/operations/perfops.rs (need to check exact signature)
impl PerfOperationHandler {
    pub fn new(state_mgr: Arc<StateManager>) -> Self  // ← Likely only 1 arg
}
```

**Root Cause:** NFSv4 handlers don't use `LocalFilesystem` directly. They work through file handles and state management only. The VFS interaction happens internally within the handlers, not passed as constructor parameters.

#### Error Category 2: CompoundDispatcher Misuse (3 errors)

**Location:** `src/nfs/server_v4.rs:71-80`

**Issue 1:** Creating handlers manually when dispatcher creates them internally

**Current (INCORRECT) Code:**
```rust
let dispatcher = Arc::new(CompoundDispatcher::new(
    fh_mgr,
    state_mgr,
    session_handler,  // ← ERROR: Dispatcher creates these internally
    file_handler,
    io_handler,
    lock_handler,
    perf_handler,
));
```

**Actual Signature (src/nfs/v4/dispatcher.rs:50-54):**
```rust
impl CompoundDispatcher {
    pub fn new(
        fh_mgr: Arc<FileHandleManager>,
        state_mgr: Arc<StateManager>,
        lock_mgr: Arc<LockManager>,  // ← Only 3 parameters!
    ) -> Self {
        // Creates handlers internally (lines 56-60)
        let session_handler = SessionOperationHandler::new(state_mgr.clone());
        let file_handler = FileOperationHandler::new(fh_mgr.clone());
        let io_handler = IoOperationHandler::new(state_mgr.clone());
        let perf_handler = PerfOperationHandler::new(state_mgr.clone());
        let lock_handler = LockOperationHandler::new(state_mgr.clone(), lock_mgr.clone());
        // ...
    }
}
```

**Issue 2:** Method name incorrect

**Current (INCORRECT):**
```rust
// Line 278
let compound_resp = dispatcher.handle_compound(compound_req).await;
```

**Correct:**
```rust
// src/nfs/v4/dispatcher.rs:75
pub async fn dispatch_compound(&self, request: CompoundRequest) -> CompoundResponse
```

**Issue 3:** Missing LockManager initialization

**Current Code (INCOMPLETE):**
```rust
// Line 61
let state_mgr = Arc::new(StateManager::new());
// ← Missing: let lock_mgr = Arc::new(LockManager::new());
```

#### Error Category 3: XDR Layer Type Mismatches (4 errors)

**Location:** `src/nfs/server_v4.rs:264, 285`

**Issue:** Using trait names as types instead of concrete types

**Current (INCORRECT) Code:**
```rust
// Line 264
let compound_req = match CompoundRequest::decode(&mut Nfs4XdrDecoder::new(request.clone())) {
    //                                                  ^^^^^^^^^^^^^^ ← Trait, not type
```

**Root Cause:** `Nfs4XdrDecoder` and `Nfs4XdrEncoder` are **traits**, not concrete types.

**Need to Check (src/nfs/v4/xdr.rs):**
- What are the actual implementing types?
- How should they be instantiated?
- What's the proper decode/encode pattern?

**Similar Issue in Encoder (Line 285):**
```rust
let mut encoder = Nfs4XdrEncoder::new();  // ← Same problem
```

#### Error Category 4: CompoundRequest Structure (2 errors)

**Location:** `src/nfs/server_v4.rs:273-274`

**Issue 1:** Field type mismatch
```rust
// Line 273
String::from_utf8_lossy(&compound_req.tag)
//                      ^^^^^^^^^^^^^^^^^ expected &[u8], found &String
```

**Issue 2:** Field name mismatch
```rust
// Line 274
compound_req.minorversion  // ← Field doesn't exist
```

**Need to Check (src/nfs/v4/compound.rs or protocol.rs):**
- Actual `CompoundRequest` struct definition
- Correct field names and types
- How to properly access tag and version

#### Error Category 5: ReplyBuilder API Mismatches (3 errors)

**Location:** `src/nfs/server_v4.rs:216, 224, 294`

**Issue 1:** Missing method
```rust
// Line 216
ReplyBuilder::prog_mismatch(call.xid, 4, 4)  // ← Method doesn't exist
```

**Issue 2:** Wrong number of arguments
```rust
// Line 224
ReplyBuilder::success(call.xid, &[])  // ← Takes 1 arg, not 2
// Line 294
ReplyBuilder::success(call.xid, &compound_data)  // ← Takes 1 arg, not 2
```

**Issue 3:** Wrong return type
```rust
// Expected: Bytes
// Got: ReplyBuilder (builder pattern?)
```

**Need to Check (src/nfs/rpc.rs):**
- Actual `ReplyBuilder` API
- How to create NFSv4 RPC replies
- Proper pattern for embedding COMPOUND response in RPC reply

---

## Detailed Fix Plan

### Phase 1: Understand NFSv4 Module APIs ⏳

**Task 1.1:** Read and document actual structures

**Files to Review:**
1. `src/nfs/v4/compound.rs` - CompoundRequest, CompoundResponse structures
2. `src/nfs/v4/xdr.rs` - XDR decoder/encoder concrete types
3. `src/nfs/v4/dispatcher.rs` - Full CompoundDispatcher API
4. `src/nfs/rpc.rs` - ReplyBuilder API and RPC reply structure

**Checklist:**
- [ ] Document `CompoundRequest` structure (fields, types)
- [ ] Document `CompoundResponse` structure
- [ ] Identify concrete XDR decoder/encoder types
- [ ] Document how to decode CompoundRequest from RPC call
- [ ] Document how to encode CompoundResponse into RPC reply
- [ ] Check if NFSv4 needs different RPC reply format

**Commands to Run:**
```bash
# Check CompoundRequest structure
grep -A 20 "pub struct CompoundRequest" src/nfs/v4/compound.rs

# Check XDR implementations
grep -A 10 "impl.*Nfs4Xdr" src/nfs/v4/xdr.rs

# Check ReplyBuilder
grep -A 30 "impl ReplyBuilder" src/nfs/rpc.rs
```

### Phase 2: Fix server_v4.rs API Mismatches ⏳

**Task 2.1:** Fix NfsServer::new() method

**File:** `src/nfs/server_v4.rs:56-81`

**Changes Needed:**
```rust
pub fn new(config: NfsConfig, fs: Arc<LocalFilesystem>) -> std::io::Result<Self> {
    // Initialize NFSv4.2 components
    let fh_mgr = Arc::new(FileHandleManager::new(config.export_path.clone()));
    let state_mgr = Arc::new(StateManager::new());

    // ADD THIS: Initialize lock manager
    let lock_mgr = Arc::new(LockManager::new());

    // REMOVE: Manual handler creation (dispatcher does this internally)
    // DELETE LINES 64-68

    // FIX: Create COMPOUND dispatcher with only 3 args
    let dispatcher = Arc::new(CompoundDispatcher::new(
        fh_mgr,
        state_mgr,
        lock_mgr,  // ← Add this third parameter
    ));

    Ok(Self { config, dispatcher })
}
```

**Task 2.2:** Fix dispatch_compound call

**File:** `src/nfs/server_v4.rs:278`

**Change:**
```rust
// FROM:
let compound_resp = dispatcher.handle_compound(compound_req).await;

// TO:
let compound_resp = dispatcher.dispatch_compound(compound_req).await;
```

**Task 2.3:** Fix XDR decoder/encoder usage

**File:** `src/nfs/server_v4.rs:264, 285`

**Steps:**
1. Check `src/nfs/v4/xdr.rs` for concrete types
2. Update decoder instantiation based on actual API
3. Update encoder instantiation based on actual API

**Possible Fix (needs verification):**
```rust
// If XdrDecoder is the concrete type:
use super::v4::xdr::XdrDecoder as Nfs4XdrDecoder;
use super::v4::xdr::XdrEncoder as Nfs4XdrEncoder;

// Then:
let compound_req = match CompoundRequest::decode(
    &mut Nfs4XdrDecoder::new(request.clone())
) {
    // ...
}
```

**Task 2.4:** Fix CompoundRequest field access

**File:** `src/nfs/server_v4.rs:273-274`

**Steps:**
1. Check actual CompoundRequest struct definition
2. Update field names and types
3. Fix tag access (might already be String, not &[u8])

**Possible Fix:**
```rust
debug!("COMPOUND: tag={}, minor_version={}, {} operations",
       compound_req.tag,  // ← Might already be String
       compound_req.minor_version,  // ← Fix field name
       compound_req.operations.len());
```

**Task 2.5:** Fix ReplyBuilder usage

**File:** `src/nfs/server_v4.rs:216, 224, 294`

**Steps:**
1. Check if ReplyBuilder has builder pattern methods
2. Understand how to embed COMPOUND response in RPC reply
3. Fix method calls and return types

**Possible Fix Pattern 1 (if builder pattern):**
```rust
ReplyBuilder::new(call.xid)
    .success()
    .data(&compound_data)
    .build()
```

**Possible Fix Pattern 2 (if takes data separately):**
```rust
let reply = ReplyBuilder::success(call.xid);
// Then append compound_data somehow
```

**Possible Fix Pattern 3 (if NFSv4 needs custom reply):**
```rust
// Create custom NFSv4 RPC reply structure
let reply = create_nfsv4_reply(call.xid, compound_resp);
```

### Phase 3: Add Missing Imports ⏳

**Task 3.1:** Import LockManager

**File:** `src/nfs/server_v4.rs:9-15`

**Add:**
```rust
use super::v4::lock_manager::LockManager;
```

**Task 3.2:** Verify all v4 module exports

**File:** `src/nfs/v4/mod.rs`

**Check that these are exported:**
- `pub use self::compound::{CompoundRequest, CompoundResponse};`
- `pub use self::dispatcher::CompoundDispatcher;`
- `pub use self::filehandle::FileHandleManager;`
- `pub use self::state::StateManager;`
- `pub use self::lock_manager::LockManager;`

### Phase 4: Handle RPC Layer Differences 🔍

**Issue:** NFSv4 and NFSv3 use the same RPC layer but different payload structures.

**Investigation Needed:**
1. Does NFSv4 COMPOUND response need special RPC wrapping?
2. How does the client distinguish v3 vs v4 responses?
3. Are there version-specific RPC reply formats?

**Reference Files:**
- `src/nfs/rpc.rs` - Current RPC implementation
- `src/nfs/v4/protocol.rs` - NFSv4 constants and structures

**Possible Approaches:**

**Approach A: Reuse existing ReplyBuilder**
- NFSv3 and NFSv4 use same RPC success reply format
- Only the payload (procedure-specific data) differs
- Just need to properly serialize CompoundResponse

**Approach B: Create NFSv4-specific reply builder**
- If NFSv4 has different RPC reply structure
- Create `Nfs4ReplyBuilder` wrapping the response properly

**Approach C: Manual RPC reply construction**
- Build RPC reply header manually
- Append XDR-encoded CompoundResponse
- Most control but more code

### Phase 5: Test Compilation ⏳

**Commands:**
```bash
# Full build to see all errors
cargo build --release --bin flint-nfs-server 2>&1 | tee build.log

# Count remaining errors
grep "^error\[E" build.log | wc -l

# Extract unique error types
grep "^error\[E" build.log | cut -d: -f1 | sort -u
```

**Success Criteria:**
- Zero compilation errors
- Only warnings remaining (acceptable)
- Binary successfully created at `target/release/flint-nfs-server`

### Phase 6: Runtime Testing 🧪

**Test 1: Server Startup**
```bash
# Start server
RUST_LOG=debug ./target/release/flint-nfs-server \
  --export-path /tmp/nfs-test-export \
  --volume-id test-vol-001 \
  > /tmp/nfs-v4-server.log 2>&1 &

# Check it's listening
ss -tlnp | grep :2049

# Check logs
tail -f /tmp/nfs-v4-server.log
```

**Expected Output:**
```
✅ NFSv4.2 TCP server listening on 0.0.0.0:2049
🚀 Starting NFSv4.2 server on 0.0.0.0:2049
📂 Exporting: "/tmp/nfs-test-export"
```

**Test 2: Mount with NFSv4.1**
```bash
# Create mount point
mkdir -p /mnt/nfs-v4-test

# Mount
mount -t nfs -o vers=4.1,tcp localhost:/ /mnt/nfs-v4-test

# Verify
mount | grep nfs-v4-test
df -h /mnt/nfs-v4-test
```

**Expected:** Mount succeeds without errors

**Test 3: Basic Operations**
```bash
# Test CREATE
echo "Hello NFSv4.2" > /mnt/nfs-v4-test/testfile.txt

# Test READ
cat /mnt/nfs-v4-test/testfile.txt

# Test WRITE (append)
echo "More data" >> /mnt/nfs-v4-test/testfile.txt

# Test REMOVE
rm /mnt/nfs-v4-test/testfile.txt

# Test directory CREATE
mkdir /mnt/nfs-v4-test/testdir

# Test directory REMOVE
rmdir /mnt/nfs-v4-test/testdir
```

**Expected:** All operations succeed

**Test 4: Check Server Logs**
```bash
grep -E "COMPOUND|CREATE|REMOVE|READ|WRITE" /tmp/nfs-v4-server.log
```

**Expected Log Patterns:**
```
>>> COMPOUND procedure
COMPOUND: tag=, minor_version=1, 3 operations
>>> Operation: CREATE
CREATE: type=Regular, name=testfile.txt
✅ Created file with handle
>>> Operation: REMOVE
REMOVE: target=testfile.txt
✅ Removed successfully
```

### Phase 7: Connectathon Testing 🧪

**Once basic operations work, run industry-standard tests:**

```bash
cd /tmp/cthon04/basic

# Set test parameters
export NFSTESTDIR=/mnt/nfs-v4-test/test
mkdir -p $NFSTESTDIR

# Run basic tests
./runtests

# Expected results:
# - test1 (basic file operations): PASS
# - test2 (file I/O): PASS
# - test3 (directory operations): PASS
# - test4 (negative tests): PASS
# - test5 (link tests): PARTIAL (if LINK not implemented)
```

---

## Architecture Deep Dive

### Why VFS (LocalFilesystem) Isn't Used in NFSv4 Handlers

**NFSv3 Architecture:**
```
Handler → VFS (LocalFilesystem) → tokio::fs → Kernel
         ↑
         Directly passed to handlers
```

**NFSv4 Architecture:**
```
Handler → FileHandleManager → Path Resolution → tokio::fs → Kernel
         ↑                    ↑
         Uses fh_mgr only    VFS interaction internal
```

**Key Difference:**
- NFSv3: Handlers get VFS reference directly, call methods on it
- NFSv4: Handlers only get FileHandleManager, which internally handles path resolution and filesystem access
- NFSv4: File operations work through **filehandles only**, not paths

**Why This Matters:**
When you see `IoOperationHandler::new(state_mgr)` with only 1 parameter, it's because:
1. I/O operations need state tracking (for stateids)
2. File access happens through filehandles tracked in state
3. The handler resolves filehandles to paths internally
4. No direct VFS reference needed

### NFSv4 COMPOUND Processing Flow

**Request Path:**
```
1. TCP Socket
   ↓
2. RPC Record Marker (4 bytes)
   ↓
3. RPC Call Message (XDR)
   ├─ XID
   ├─ Program: 100003 (NFS)
   ├─ Version: 4
   ├─ Procedure: 1 (COMPOUND)
   └─ Auth credentials
   ↓
4. COMPOUND Request (XDR)
   ├─ Tag (opaque string)
   ├─ Minor version (1 or 2)
   └─ Operations array
      ├─ Op 1: PUTROOTFH
      ├─ Op 2: LOOKUP "testfile.txt"
      ├─ Op 3: OPEN
      └─ Op 4: READ
   ↓
5. CompoundDispatcher.dispatch_compound()
   ↓
6. For each operation:
   │  ├─ Extract operation
   │  ├─ Call appropriate handler
   │  ├─ Update CompoundContext
   │  └─ Collect result
   ↓
7. CompoundResponse
   ├─ Status
   ├─ Tag (echoed)
   └─ Results array
   ↓
8. XDR encode CompoundResponse
   ↓
9. Wrap in RPC SUCCESS reply
   ↓
10. Send with RPC record marker
    ↓
11. TCP Socket
```

**Critical Points:**
- **CompoundContext:** Carries state between operations (current_fh, saved_fh)
- **Operations are sequential:** Each operation sees results of previous ones
- **Single RPC transaction:** All operations in one COMPOUND succeed or fail together
- **Stateful:** Unlike NFSv3, NFSv4 tracks client state (sessions, opens, locks)

### State Management Architecture

```
StateManager (DashMap-based, lock-free)
├─ Clients (by client_id)
│  └─ Client
│     ├─ client_id: u64
│     ├─ confirmed: bool
│     └─ sessions: Vec<Session>
│
├─ Sessions (by session_id)
│  └─ Session
│     ├─ session_id: [u8; 16]
│     ├─ sequence_id: u32
│     └─ channel_attrs
│
├─ Open States (by stateid)
│  └─ OpenState
│     ├─ stateid: StateId
│     ├─ fh: FileHandle
│     ├─ access: u32
│     └─ deny: u32
│
└─ Lock States (by stateid)
   └─ LockState
      ├─ stateid: StateId
      ├─ owner: LockOwner
      └─ byte_range
```

**Concurrency Model:**
- DashMap provides lock-free concurrent access
- Multiple clients can access different files simultaneously
- Session slots prevent out-of-order operation replay
- Stateids track individual file opens and locks

---

## Debugging Guide

### Common Issues and Solutions

**Issue 1: "Procedure not supported"**
- Cause: Client sent NFSv3 procedure (not COMPOUND)
- Solution: Verify client is using `vers=4.1` or `vers=4.2`
- Check: `tcpdump -i any -n port 2049 -X`

**Issue 2: "Garbage arguments"**
- Cause: XDR decoding failed
- Solution: Check CompoundRequest XDR structure matches RFC 8881
- Debug: Add logging in decode functions

**Issue 3: "SEQUENCE failed"**
- Cause: Session not established or sequence ID mismatch
- Solution: Verify EXCHANGE_ID and CREATE_SESSION succeeded first
- Check: StateManager has session entry

**Issue 4: "Stale file handle"**
- Cause: FileHandle not found in FileHandleManager cache
- Solution: Check FileHandle generation and caching logic
- Debug: Log all filehandle operations

**Issue 5: Mount fails with "Protocol not supported"**
- Cause: Server not responding to NFSv4 NULL procedure
- Solution: Verify NULL procedure handler works
- Test: `rpcinfo -p localhost | grep nfs`

### Logging Strategy

**Add comprehensive debug logging:**

```rust
// In dispatch_nfsv4
debug!(">>> RPC Call: xid={}, prog={}, ver={}, proc={}",
       call.xid, call.program, call.version, call.procedure);

// In handle_compound
debug!(">>> COMPOUND: tag={:?}, ops={}",
       compound_req.tag, compound_req.operations.len());
for (i, op) in compound_req.operations.iter().enumerate() {
    debug!("  Op {}: {:?}", i, op);
}

// After dispatch_compound
debug!("<<< COMPOUND: status={:?}, results={}",
       compound_resp.status, compound_resp.resarray.len());
```

**Log Levels:**
- `ERROR`: Unrecoverable errors, server failures
- `WARN`: Client errors, unsupported operations
- `INFO`: Client connections, major operations (COMPOUND start/end)
- `DEBUG`: Individual operations, state changes
- `TRACE`: XDR encoding/decoding, byte-level details

---

## Success Criteria

### Compilation Success ✅
- [ ] Zero errors in `cargo build --release --bin flint-nfs-server`
- [ ] Binary created: `target/release/flint-nfs-server`
- [ ] File size reasonable (~30-40 MB for release build)

### Runtime Success ✅
- [ ] Server starts without panics
- [ ] Listens on port 2049
- [ ] Logs show "NFSv4.2 TCP server listening"

### Mount Success ✅
- [ ] `mount -t nfs -o vers=4.1` succeeds
- [ ] `df -h` shows mounted filesystem
- [ ] No kernel errors in `dmesg`

### Basic Operations Success ✅
- [ ] File creation: `echo "test" > /mnt/test.txt`
- [ ] File reading: `cat /mnt/test.txt`
- [ ] File writing: `echo "more" >> /mnt/test.txt`
- [ ] File deletion: `rm /mnt/test.txt`
- [ ] Directory creation: `mkdir /mnt/testdir`
- [ ] Directory deletion: `rmdir /mnt/testdir`

### Connectathon Success ✅
- [ ] test1 (basic files): PASS
- [ ] test2 (I/O): PASS
- [ ] test3 (directories): PASS
- [ ] test4 (negative tests): PASS

### Concurrent I/O Success ✅
- [ ] Multiple clients can read simultaneously
- [ ] Multiple clients can write to different files
- [ ] Lock coordination works under contention
- [ ] No data corruption or race conditions

---

## Implementation Priority

### P0: Fix Compilation (Required for Any Testing)
1. Fix CompoundDispatcher instantiation
2. Fix XDR decoder/encoder usage
3. Fix ReplyBuilder usage
4. Fix CompoundRequest field access
5. Add missing imports

**Estimated Time:** 2-3 hours
**Blocking:** Everything else

### P1: Basic Server Functionality (Required for Mount Testing)
1. Server starts successfully
2. Accepts TCP connections
3. Handles NULL procedure
4. Handles COMPOUND procedure
5. Logs show proper operation dispatch

**Estimated Time:** 1-2 hours
**Blocking:** All testing

### P2: Core Operations (Required for File I/O Testing)
1. PUTROOTFH works
2. LOOKUP works
3. GETFH works
4. OPEN works
5. READ/WRITE work
6. CLOSE works

**Estimated Time:** 2-4 hours (if bugs found)
**Blocking:** File operations testing

### P3: File Modification (Required for Concurrent I/O Testing)
1. CREATE works (already implemented)
2. REMOVE works (already implemented)
3. Directory operations work
4. GETATTR/SETATTR work

**Estimated Time:** 1-2 hours (if bugs found)
**Blocking:** Connectathon tests

### P4: Concurrent Access (Goal Achievement)
1. Multiple clients can connect
2. Session management works
3. Lock coordination works
4. Concurrent I/O verified with stress test

**Estimated Time:** 4-8 hours (including testing)
**Blocking:** Production readiness

---

## Resource Requirements

### Knowledge Required
- Rust async/await and Arc/Mutex patterns
- NFSv4.2 protocol structure (RFC 7862, RFC 8881)
- XDR encoding/decoding
- Sun RPC message format
- TCP socket programming

### Tools Required
- Rust toolchain (already installed)
- NFS client tools: `mount.nfs`, `nfs-utils`
- Network tools: `tcpdump`, `wireshark`, `rpcinfo`
- Testing tools: Connectathon test suite (already built)
- Debugging: `gdb`, `strace` (optional)

### Documentation Required
- NFSv4.2 operation specifications
- Existing code understanding (dispatcher, handlers)
- RPC reply format documentation

---

## Estimated Timeline

### Phase 1: API Understanding (1-2 hours)
- Read module source files
- Document actual structures
- Identify concrete types

### Phase 2: Fix Compilation (2-4 hours)
- Fix all 15 compilation errors
- Add missing imports
- Verify clean build

### Phase 3: Basic Testing (2-3 hours)
- Start server
- Mount filesystem
- Test basic operations
- Debug any runtime issues

### Phase 4: Advanced Testing (3-5 hours)
- Run Connectathon tests
- Test concurrent operations
- Measure performance
- Document results

### Phase 5: Polish (1-2 hours)
- Clean up debug logging
- Update documentation
- Create deployment guide

**Total Estimated Time:** 9-16 hours

**Critical Path:** Phase 1 → Phase 2 → Phase 3 (must be sequential)
**Parallelizable:** Phase 4 testing while Phase 5 documentation

---

## Rollback Plan

If NFSv4.2 integration proves too complex or time-consuming:

### Option A: Temporary NFSv3 Testing
1. Revert module exports back to server (NFSv3)
2. Test CREATE/REMOVE work in NFSv3
3. Verify concurrent I/O capability with v3
4. Complete v4 integration separately

### Option B: Hybrid Approach
1. Keep NFSv3 server as-is
2. Create separate NFSv4.2 server binary (`flint-nfs4-server`)
3. Test both independently
4. Choose best performing version

### Option C: Staged Integration
1. Fix compilation errors first (this document)
2. If runtime issues arise, document them
3. Continue with phased testing approach
4. Don't block other work on v4 integration

---

## Next Immediate Actions

**RIGHT NOW - Start Here:**

1. **Read XDR Module** (10 minutes)
   ```bash
   grep -A 50 "pub trait Nfs4Xdr" src/nfs/v4/xdr.rs
   grep "impl.*Xdr.*for" src/nfs/v4/xdr.rs
   ```

2. **Read CompoundRequest** (10 minutes)
   ```bash
   grep -A 30 "pub struct CompoundRequest" src/nfs/v4/compound.rs
   ```

3. **Read ReplyBuilder** (10 minutes)
   ```bash
   grep -A 50 "impl ReplyBuilder" src/nfs/rpc.rs
   ```

4. **Fix NfsServer::new()** (20 minutes)
   - Remove manual handler creation
   - Add LockManager initialization
   - Fix CompoundDispatcher::new() call

5. **Fix dispatch_compound** (5 minutes)
   - Change method name from handle_compound

6. **Try Build** (5 minutes)
   ```bash
   cargo build --release --bin flint-nfs-server 2>&1 | tee build.log
   grep "^error" build.log | wc -l
   ```

**Expected:** Errors reduced from 15 to ~8-10 after these fixes.

**Continue** with remaining errors based on what's left.

---

## Contact and Questions

If you encounter issues not covered in this document:

1. Check actual source code in `src/nfs/v4/` modules
2. Read the RFC sections for the specific operation
3. Add debug logging to understand runtime behavior
4. Use `tcpdump` to capture and analyze NFS packets
5. Reference existing NFSv3 server code for patterns

**Key Source Files:**
- `src/nfs/v4/mod.rs` - Module exports
- `src/nfs/v4/protocol.rs` - Constants and enums
- `src/nfs/v4/compound.rs` - COMPOUND structures
- `src/nfs/v4/dispatcher.rs` - Main dispatcher logic
- `src/nfs/v4/xdr.rs` - XDR encoding/decoding
- `src/nfs/rpc.rs` - RPC message handling

---

**Document Status:** 📝 COMPLETE - Ready for implementation
**Last Updated:** December 9, 2025
**Next Update:** After Phase 2 (compilation fixes) completion
