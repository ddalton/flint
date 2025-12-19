# Data Server Parallel I/O Implementation Plan

**Date**: December 18, 2025  
**Goal**: Enable DS to handle parallel I/O from NFSv4.1 clients  
**Status**: LAYOUTGET/GETDEVICEINFO working ✅ - DS needs session support

---

## 🎯 Current State

### ✅ What's Working

**MDS Side:**
- ✅ LAYOUTGET returns correct FILE layouts
- ✅ Device IDs properly encoded
- ✅ File handles included in layouts
- ✅ GETDEVICEINFO returns DS addresses (e.g., `10.65.161.80:2050`)

**Client Side:**
- ✅ pNFS activated (`pnfs=LAYOUT_NFSV4_1_FILES`)
- ✅ Sends LAYOUTGET requests
- ✅ Receives layouts with device IDs
- ✅ Sends GETDEVICEINFO
- ✅ Gets DS network addresses
- ✅ **Contacts DS successfully!** (opcode 53 seen in DS logs)

**DS Side:**
- ✅ Listens on port 2050
- ✅ Registers with MDS using correct pod IP
- ✅ Receives client connections
- ✅ Supports: PUTFH, READ, WRITE, COMMIT
- ❌ **Does NOT support: SEQUENCE and other session operations**

### ❌ What's Not Working

**Problem:**
```
DS COMPOUND: minor_version=2, 1 operations
WARN: DS received unsupported operation: 53 (SEQUENCE)
```

**Result:**
- Client tries to use NFSv4.1 sessions with DS
- DS returns NFS4ERR_NOTSUPP
- Client falls back to MDS for all I/O
- No parallel striping occurs

---

## 📋 Implementation Plan

### Option 1: Add Minimal Session Support to DS (Recommended)

**Estimated Time:** 2-3 hours

#### Step 1: Add Session State to DS (30 minutes)

**Create:** `src/pnfs/ds/session.rs`

```rust
//! Minimal NFSv4.1 Session Support for Data Server
//! 
//! Unlike the MDS which needs full session management, the DS only needs
//! minimal session support to satisfy NFSv4.1 clients.

use dashmap::DashMap;
use std::sync::Arc;

/// Minimal session manager for DS
/// Only tracks enough state to handle SEQUENCE operations
pub struct DsSessionManager {
    /// Active sessions: sessionid -> DsSession
    sessions: Arc<DashMap<[u8; 16], DsSession>>,
}

struct DsSession {
    sessionid: [u8; 16],
    /// Per-slot sequence tracking (DS typically only needs slot 0)
    slot_sequences: Vec<u32>,
}

impl DsSessionManager {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
        }
    }
    
    /// Handle SEQUENCE operation (minimal - just validate and echo back)
    pub fn handle_sequence(
        &self,
        sessionid: [u8; 16],
        sequenceid: u32,
        slotid: u32,
    ) -> Result<SequenceResult, u32> {
        // Get or create session (DS auto-creates on first SEQUENCE)
        let mut session = self.sessions.entry(sessionid)
            .or_insert_with(|| DsSession {
                sessionid,
                slot_sequences: vec![0; 128],  // 128 slots
            });
        
        // Validate slot
        if slotid >= 128 {
            return Err(NFS4ERR_BADSLOT);
        }
        
        // Update sequence number (simplified - no replay cache)
        session.slot_sequences[slotid as usize] = sequenceid;
        
        Ok(SequenceResult {
            sessionid,
            sequenceid,
            slotid,
            highest_slotid: 0,
            target_highest_slotid: 127,
            status_flags: 0,
        })
    }
}

pub struct SequenceResult {
    pub sessionid: [u8; 16],
    pub sequenceid: u32,
    pub slotid: u32,
    pub highest_slotid: u32,
    pub target_highest_slotid: u32,
    pub status_flags: u32,
}
```

#### Step 2: Update DS COMPOUND Handler (1 hour)

**Modify:** `src/pnfs/ds/server.rs` line 252-414

**Add SEQUENCE support:**

```rust
async fn handle_minimal_compound(
    call: CallMessage,
    args: Bytes,
    io_handler: Arc<IoOperationHandler>,
    session_mgr: Arc<DsSessionManager>,  // ADD THIS
) -> Bytes {
    // ... existing decode logic ...
    
    for _ in 0..op_count {
        let opcode = match decoder.decode_u32() { ... };
        
        let (status, result_data) = match opcode {
            // ADD SEQUENCE SUPPORT
            opcode::SEQUENCE => {
                let sessionid = decoder.decode_fixed_opaque(16)?;
                let sequenceid = decoder.decode_u32()?;
                let slotid = decoder.decode_u32()?;
                let highest_slotid = decoder.decode_u32()?;
                let cache_this = decoder.decode_bool()?;
                
                match session_mgr.handle_sequence(sessionid, sequenceid, slotid) {
                    Ok(result) => {
                        let mut encoder = XdrEncoder::new();
                        encoder.encode_fixed_opaque(&result.sessionid);
                        encoder.encode_u32(result.sequenceid);
                        encoder.encode_u32(result.slotid);
                        encoder.encode_u32(result.highest_slotid);
                        encoder.encode_u32(result.target_highest_slotid);
                        encoder.encode_u32(result.status_flags);
                        (Nfs4Status::Ok, encoder.finish())
                    }
                    Err(err) => (Nfs4Status::from(err), Bytes::new()),
                }
            }
            
            opcode::PUTFH => { /* existing */ }
            opcode::READ => { /* existing */ }
            opcode::WRITE => { /* existing */ }
            opcode::COMMIT => { /* existing */ }
            
            _ => {
                warn!("DS received unsupported operation: {}", opcode);
                (Nfs4Status::NotSupp, Bytes::new())
            }
        };
        
        results.push((status, result_data));
        
        // Stop on first error (NFSv4 COMPOUND semantics)
        if status != Nfs4Status::Ok {
            break;
        }
    }
    
    // ... existing encode logic ...
}
```

#### Step 3: Test Session Support (30 minutes)

**Add test:** `src/pnfs/ds/session.rs`

```rust
#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_sequence_handling() {
        let mgr = DsSessionManager::new();
        let sessionid = [1u8; 16];
        
        let result = mgr.handle_sequence(sessionid, 1, 0).unwrap();
        assert_eq!(result.sequenceid, 1);
        assert_eq!(result.slotid, 0);
        
        // Subsequent sequence should increment
        let result2 = mgr.handle_sequence(sessionid, 2, 0).unwrap();
        assert_eq!(result2.sequenceid, 2);
    }
}
```

---

### Option 2: NFSv4.0 Compatibility Mode (Alternative)

**Estimated Time:** 1-2 hours

Make DS advertise only NFSv4.0 support so clients don't use sessions.

**Pros:**
- Simpler implementation
- No session state needed

**Cons:**
- NFSv4.0 doesn't have pNFS!
- Won't work with our current approach

**Verdict:** Not viable - stick with Option 1

---

## 🔧 Additional Requirements

### 1. File Handle Mapping (Already Working?)

**Question:** Do DS file handles match MDS file handles?

**Current Approach:**
- MDS sends filehandle in LAYOUTGET response
- DS receives same filehandle in PUTFH
- DS maps filehandle to local file path

**Verification Needed:**
```bash
# In DS logs, check:
grep "PUTFH" /tmp/ds1-final.log
# Should show the same 65-byte filehandle from MDS
```

### 2. Stripe Offset Calculation (MDS Already Does This)

**MDS generates layouts with:**
- Segment 0: offset=0, length=stripe_size → DS1
- Segment 1: offset=stripe_size, length=stripe_size → DS2
- etc.

**DS just needs to:**
- Accept the filehandle from layout
- Handle READ/WRITE at the offsets the client requests
- Client does the striping math, not the DS!

### 3. COMMIT Handling

**Current DS code supports COMMIT** (line 361-377) ✅

---

## 📊 Testing Plan

### Phase 1: Verify Session Support (15 minutes)

1. Add SEQUENCE handling to DS
2. Rebuild and deploy
3. Test client write
4. Check DS logs for successful SEQUENCE operations

### Phase 2: Verify I/O Operations (15 minutes)

1. Write 100MB file from client
2. Check DS logs for READ/WRITE operations
3. Verify data is actually written to DS storage
4. Check both DSs receive operations (striping)

### Phase 3: Performance Testing (30 minutes)

**Test 1: Standalone NFS Baseline**
```bash
# Deploy standalone-nfs
mount -t nfs -o vers=4.1 standalone-nfs:/ /mnt/standalone
dd if=/dev/zero of=/mnt/standalone/test100mb bs=1M count=100
# Expected: ~90 MB/s
```

**Test 2: pNFS with 1 DS**
```bash
# Only DS1 active
dd if=/dev/zero of=/mnt/pnfs/test100mb bs=1M count=100
# Expected: ~90 MB/s (similar to standalone)
```

**Test 3: pNFS with 2 DSs**
```bash
# Both DS1 and DS2 active
dd if=/dev/zero of=/mnt/pnfs/test100mb bs=1M count=100
# Expected: ~180 MB/s (2x improvement!)
```

---

## 📝 Code Changes Needed

### File 1: `src/pnfs/ds/session.rs` (NEW)
- ~150 lines
- Minimal session manager
- SEQUENCE operation handler

### File 2: `src/pnfs/ds/server.rs`
- Add `session_mgr` field to `DataServer` struct
- Pass `session_mgr` to `handle_minimal_compound()`
- Add `opcode::SEQUENCE` case in match statement (~30 lines)

### File 3: `src/pnfs/ds/mod.rs`
- Export session module

### File 4: Tests
- Add session tests
- Add integration test for DS I/O

**Total:** ~200-250 lines of code

---

## 🎓 Design Considerations

### Sessions in DS vs MDS

**MDS Sessions (Complex):**
- Full session lifecycle (CREATE_SESSION, DESTROY_SESSION)
- Client state management
- Lease management
- Replay cache

**DS Sessions (Simple):**
- No CREATE_SESSION (inherit from MDS)
- Just track sequence numbers per slot
- No client management (stateless)
- Minimal replay protection

### Why This is Simple

The client already has a session with the MDS. When contacting the DS:
1. Client uses the **same sessionid** from MDS
2. DS just needs to validate SEQUENCE numbers
3. DS doesn't need to create/destroy sessions
4. DS is essentially stateless (file I/O only)

---

## 🔍 Debugging Hooks Needed

### Add to DS:

```rust
info!("🔥 DS received SEQUENCE: sessionid={:?}, seq={}, slot={}", 
      &sessionid[0..8], sequenceid, slotid);
      
info!("📖 DS READ: fh len={}, offset={}, count={}", 
      fh.len(), offset, count);
      
info!("✍️  DS WRITE: fh len={}, offset={}, count={}", 
      fh.len(), offset, data.len());
      
info!("💾 DS COMMIT: offset={}, count={}", offset, count);
```

This will immediately show if I/O is being striped!

---

## ✅ MDS Changes Needed

### None!

The MDS is complete and working correctly:
- ✅ Returns layouts with device IDs
- ✅ GETDEVICEINFO returns DS addresses  
- ✅ File handles are correct
- ✅ Stripe policy generates proper segments

---

## 🚀 Expected Outcome

After adding DS session support:

**Client perspective:**
```
mount -t nfs -o vers=4.1 mds:/ /mnt/pnfs
dd if=/dev/zero of=/mnt/pnfs/test100mb bs=1M count=100
# ~180 MB/s (2x improvement!)
```

**DS1 logs:**
```
🔥 DS received SEQUENCE: sessionid=[...], seq=1, slot=0
✍️  DS WRITE: offset=0, count=8388608 (8MB)
✍️  DS WRITE: offset=16777216, count=8388608 (8MB)
✍️  DS WRITE: offset=33554432, count=8388608 (8MB)
💾 DS COMMIT: offset=0, count=52428800
```

**DS2 logs:**
```
🔥 DS received SEQUENCE: sessionid=[...], seq=1, slot=0
✍️  DS WRITE: offset=8388608, count=8388608 (8MB)
✍️  DS WRITE: offset=25165824, count=8388608 (8MB)  
✍️  DS WRITE: offset=41943040, count=8388608 (8MB)
💾 DS COMMIT: offset=0, count=52428800
```

**Notice:** Each DS gets alternating 8MB stripes!

---

## 📐 Implementation Steps

### Step 1: Create Session Module (45 min)
```bash
touch src/pnfs/ds/session.rs
# Implement DsSessionManager
# Add tests
```

### Step 2: Update DS Server (1 hour)
```bash
# Edit src/pnfs/ds/server.rs
# Add session_mgr field
# Add SEQUENCE case
# Update handle_minimal_compound signature
```

### Step 3: Update Module Exports (5 min)
```bash
# Edit src/pnfs/ds/mod.rs
pub mod session;
```

### Step 4: Test with Single DS (15 min)
```bash
# Start MDS + 1 DS
# Write file, verify DS receives I/O
# Check DS logs for WRITE operations
```

### Step 5: Test with Two DSs (15 min)
```bash
# Start MDS + 2 DSs
# Write 100MB file
# Verify BOTH DSs receive writes
# Check striping pattern (alternating 8MB chunks)
```

### Step 6: Performance Test (30 min)
```bash
# Baseline: Standalone NFS
# pNFS with 1 DS
# pNFS with 2 DSs
# Compare throughput
```

---

## 🎯 Success Criteria

### Functional Requirements
- ✅ Client can write files successfully
- ✅ No I/O errors
- ✅ SEQUENCE operations succeed
- ✅ Both DSs receive I/O requests
- ✅ Data is correctly striped

### Performance Requirements
- ✅ 2 DSs achieve ~2x throughput vs 1 DS
- ✅ No performance regression vs standalone NFS
- ✅ Linear scaling with DS count

### Observability
- ✅ DS logs show I/O operations
- ✅ Can verify striping pattern
- ✅ Can measure per-DS throughput

---

## 🔒 Session Security Considerations

### Simplified for DS

**What DS DOESN'T need:**
- ❌ Session creation (client has session with MDS)
- ❌ Session destruction  
- ❌ Lease management (MDS handles)
- ❌ Client authentication (MDS did it)
- ❌ State revocation
- ❌ Replay cache (optional for DS)

**What DS DOES need:**
- ✅ Validate SEQUENCE (sessionid, sequenceid, slotid)
- ✅ Track last sequence number per slot
- ✅ Return target_highest_slotid (tell client how many slots we support)
- ✅ Return status_flags (usually 0)

**Security Note:** The DS trusts that the MDS already authenticated the client. The sessionid proves the client talked to the MDS.

---

## 📊 Architecture Diagram

```
Client (NFSv4.1)
    |
    |--- (Metadata ops) ---> MDS
    |                         |
    |                         +-- LAYOUTGET → returns [DS1, DS2]
    |                         |
    |                         +-- GETDEVICEINFO → returns addresses
    |
    |--- (Data I/O) --------> DS1 (10.65.161.80:2050)
    |                         |
    |                         +-- SEQUENCE ✅ (NEW)
    |                         +-- PUTFH ✅
    |                         +-- WRITE ✅ (stripes 0, 2, 4, ...)
    |
    +--- (Data I/O) --------> DS2 (10.65.140.37:2050)
                              |
                              +-- SEQUENCE ✅ (NEW)
                              +-- PUTFH ✅
                              +-- WRITE ✅ (stripes 1, 3, 5, ...)
```

---

## 🧪 Validation Tests

### Test 1: Session Handling
```bash
# Mount pNFS
# Write small file
# Check DS logs for:
grep "SEQUENCE" /tmp/ds1.log
# Should see: sessionid, sequenceid, slotid
```

### Test 2: Striping Pattern
```bash
# Write 32MB file (4 x 8MB stripes)
# DS1 should get: chunks at offset 0, 16MB
# DS2 should get: chunks at offset 8MB, 24MB
```

### Test 3: Concurrent Clients
```bash
# Mount from 2 different clients
# Both write simultaneously
# Verify DSs handle concurrent sessions
```

---

## 📈 Expected Performance

| Configuration | Throughput | Utilization |
|--------------|------------|-------------|
| Standalone NFS | 90 MB/s | 1 server @ 100% |
| pNFS + 1 DS | 90 MB/s | 1 DS @ 100% |
| pNFS + 2 DSs | 180 MB/s | 2 DSs @ 50% each |
| pNFS + 4 DSs | 360 MB/s | 4 DSs @ 25% each |

**Linear scaling with DS count!**

---

## 🐛 Potential Issues

### Issue 1: Session ID Mismatch
**Symptom:** DS rejects SEQUENCE with BAD_SESSION  
**Fix:** DS should accept any sessionid (trust MDS authenticated it)

### Issue 2: Sequence Replay
**Symptom:** Client retries cause BAD_SEQID  
**Fix:** Implement simple replay cache or accept seq >= last_seq

### Issue 3: File Handle Not Found
**Symptom:** DS returns STALE filehandle  
**Fix:** Verify DS file handle matches MDS encoding

---

## 📚 References

### RFC 5661 Sections
- Section 12.5.2: Data Server Session Requirements
- Section 13.6: Data Server READ/WRITE  
- Section 18.35: SEQUENCE operation
- Section 18.36: CREATE_SESSION (DS doesn't need this)

### Linux Kernel
- `fs/nfs/pnfs.c` - pNFS client implementation
- `fs/nfs/filelayout/filelayout.c` - FILE layout client
- Shows how client uses sessions with DS

---

## ⏱️ Time Estimate

| Task | Time | Difficulty |
|------|------|-----------|
| Create session.rs | 45 min | Easy |
| Update server.rs | 1 hour | Medium |
| Add tests | 30 min | Easy |
| Integration testing | 30 min | Easy |
| Performance testing | 30 min | Easy |
| **Total** | **3 hours** | **Easy-Medium** |

---

## 🎯 Deliverables

1. ✅ `src/pnfs/ds/session.rs` - Minimal session manager
2. ✅ Updated `src/pnfs/ds/server.rs` - SEQUENCE support
3. ✅ Tests for session handling
4. ✅ Performance benchmarks showing 2x improvement
5. ✅ Documentation of striping behavior

---

## 🚀 Quick Start (Once Implemented)

```bash
# Terminal 1: Start MDS
cd /root/flint/spdk-csi-driver
RUST_LOG=info ./target/release/flint-pnfs-mds --config mds.yaml

# Terminal 2: Start DS1
RUST_LOG=info POD_IP=10.65.161.80 ./target/release/flint-pnfs-ds --config ds1.yaml

# Terminal 3: Start DS2  
RUST_LOG=info POD_IP=10.65.140.37 ./target/release/flint-pnfs-ds --config ds2.yaml

# Terminal 4: Test
mount -t nfs -o vers=4.1 10.65.161.80:/ /mnt/pnfs
dd if=/dev/zero of=/mnt/pnfs/test100mb bs=1M count=100
# Should see writes in BOTH DS logs!
```

---

**Status**: Plan complete - ready to implement  
**Blocker**: None - all prerequisites met  
**Risk**: Low - well-understood problem with clear solution

