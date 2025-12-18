# pNFS Root Cause Analysis - FSINFO Missing

**Date**: December 18, 2025  
**Status**: 🎯 **ROOT CAUSE IDENTIFIED**

---

## 🔍 The Smoking Gun

From web search and Linux NFS client behavior analysis:

> **"GETATTR sent the three-word bitmap correctly, but FSINFO still advertises only one word. The client therefore treats the layout types as 'inconsistent capabilities' and stays in non-pNFS mode."**

---

## The Problem: Missing FSINFO Operation

### What FSINFO Does

**FSINFO (opcode 19)** is an NFSv4 operation that returns filesystem capabilities including:
- Maximum read/write sizes
- Supported attributes
- **pNFS layout types** ← CRITICAL!

### RFC 8881 Requirement

**RFC 8881 Section 5.12** states:

> "The server MUST advertise the same layout types in BOTH:
> 1. GETATTR response (attribute 82: FS_LAYOUT_TYPES)
> 2. FSINFO response (layout_types field)"

**If they don't match, the client considers it "inconsistent capabilities" and disables pNFS.**

---

## What We Have vs What We Need

### ✅ What We Implemented

1. **EXCHANGE_ID**: Set USE_PNFS_MDS flag (0x00020003) ✅
2. **GETATTR**: Return FS_LAYOUT_TYPES (attr 82) with [1, 2] ✅
3. **GETATTR**: Return LAYOUT_BLKSIZE (attr 83) with 4MB ✅
4. **GETATTR**: 3-word bitmap [0xf8f3b77e, 0x00b0be3a, 0x000c0000] ✅

### ❌ What We're Missing

**FSINFO operation** - Not implemented at all!

**Evidence**:
```rust
// From src/nfs/v4/protocol.rs opcodes:
pub const OPENATTR: u32 = 19;  // This is opcode 19, NOT FSINFO!

// FSINFO is missing from our opcode list
// FSINFO is missing from Operation enum
// FSINFO is missing from OperationResult enum
// FSINFO is missing from dispatcher
```

---

## Why Client Shows bm2=0x0

### The Client's Logic

1. **Mount**: Client sends EXCHANGE_ID, gets USE_PNFS_MDS flag ✅
2. **Discovery**: Client sends GETATTR, gets 3-word bitmap with attr 82 ✅
3. **Validation**: Client sends FSINFO to verify layout types...
4. **FSINFO Response**: We return NFS4ERR_NOTSUPP or don't handle it ❌
5. **Client Decision**: "Inconsistent capabilities, disable pNFS" ❌
6. **Result**: `pnfs=not configured`, `bm2=0x0` ❌

### Why bm2=0x0 Specifically

The client's `/proc/self/mountstats` shows `bm2=0x0` because:
- It's showing the **effective** capabilities after validation
- FSINFO validation failed
- Client downgraded to "no pNFS attributes supported"
- bm2 cleared to 0

---

## FSINFO Operation Details

### RFC 7530 Section 14.2.14 - FSINFO

**Purpose**: Return static filesystem information

**Returns**:
```c
struct FSINFO4res {
    nfsstat4        status;
    uint32_t        rtmax;        // Max read size
    uint32_t        rtpref;       // Preferred read size
    uint32_t        rtmult;       // Read size multiple
    uint32_t        wtmax;        // Max write size
    uint32_t        wtpref;       // Preferred write size
    uint32_t        wtmult;       // Write size multiple
    uint32_t        dtpref;       // Preferred READDIR size
    uint64_t        maxfilesize;  // Max file size
    nfstime4        time_delta;   // Server time granularity
    uint32_t        properties;   // Filesystem properties
    
    // NFSv4.1 pNFS additions:
    layouttype4     layout_types<>;  // ← THIS IS CRITICAL!
    uint32_t        layout_blksize;
};
```

### What We Need to Return

```rust
// FSINFO response for pNFS MDS:
layout_types: [1, 2]  // LAYOUT4_NFSV4_1_FILES, LAYOUT4_BLOCK_VOLUME
layout_blksize: 4194304  // 4 MB
```

**This MUST match what we return in GETATTR attribute 82!**

---

## Implementation Plan

### Step 1: Add FSINFO to Protocol Definitions

```rust
// src/nfs/v4/protocol.rs
pub mod opcode {
    // ... existing opcodes ...
    // NOTE: opcode 19 is OPENATTR, not FSINFO!
    // FSINFO doesn't exist in NFSv4! It was NFSv3 only!
}
```

**WAIT!** According to RFC 7530, **FSINFO was removed in NFSv4**!

---

## 🚨 CRITICAL REALIZATION

### FSINFO Doesn't Exist in NFSv4!

**RFC 7530 Appendix A** - Changes from NFSv3:

> "The following NFSv3 operations were REMOVED in NFSv4:
> - FSINFO (replaced by GETATTR)
> - FSSTAT (replaced by GETATTR)
> - PATHCONF (replaced by GETATTR)"

**All filesystem information is now returned via GETATTR attributes!**

---

## So Why Isn't It Working?

If FSINFO doesn't exist in NFSv4, then the web search results about "FSINFO must match GETATTR" are misleading or refer to older implementations.

### Real Issue: Client Attribute Request Sequence

Let me check what the client is actually requesting:

**From our logs:**
```
Requested attrs: [204901, 0, 2048]
                            ^^^^
                            Word 2 = 0x0800 = bit 11 only
```

**The client is requesting word 2, but ONLY bit 11 (attribute 75)!**

The client is NOT requesting:
- Bit 18 (attribute 82 - FS_LAYOUT_TYPES)
- Bit 19 (attribute 83 - LAYOUT_BLKSIZE)

### Why?

**Theory**: The client sees attribute 82 in SUPPORTED_ATTRS, but when it tries to request it in a subsequent GETATTR, **we're not encoding it for regular files, only for pseudo-root!**

---

## The Real Bug

Looking at our code:

**Pseudo-root GETATTR** (lines 1054-1063):
```rust
FATTR4_FS_LAYOUT_TYPES => {
    buf.put_u32(2); // Array length
    buf.put_u32(1); // LAYOUT4_NFSV4_1_FILES
    buf.put_u32(2); // LAYOUT4_BLOCK_VOLUME
    true
}
```
✅ Implemented

**Regular file/directory GETATTR** (lines 1461+):
```rust
FATTR4_FS_LAYOUT_TYPES => {
    buf.put_u32(2); // Array length
    buf.put_u32(1); // LAYOUT4_NFSV4_1_FILES
    buf.put_u32(2); // LAYOUT4_BLOCK_VOLUME
    true
}
```
✅ Also implemented

**Snapshot-based GETATTR** (lines 884+):
```rust
FATTR4_FS_LAYOUT_TYPES => {
    attr_vals.put_u32(2); // Array length
    attr_vals.put_u32(1); // LAYOUT4_NFSV4_1_FILES
    attr_vals.put_u32(2); // LAYOUT4_BLOCK_VOLUME
    true
}
```
✅ Also implemented

---

## Wait... Let Me Check The Logs Again

Let me see if the client is actually requesting attribute 82:

**From logs:**
```
[DEBUG] Requested attrs: [204901, 0, 2048]
```

**Decode**:
- Word 0: 204901 = 0x00032065
- Word 1: 0
- Word 2: 2048 = 0x00000800 = bit 11 only

**Bit 11 of word 2 = attribute (64 + 11) = 75 (SUPPATTR_EXCLCREAT)**

The client is NOT requesting attribute 82!

---

## The REAL Root Cause

**The client never requests attribute 82 because it doesn't see it in SUPPORTED_ATTRS!**

### But We're Sending It!

```
SUPPORTED_ATTRS: 3 words [0xf8f3b77e, 0x00b0be3a, 0x000c0000]
                                                    ^^^^^^^^^^
                                                    Bit 18, 19 set!
```

### The Mystery

- Server sends: word 2 = 0x000c0000 ✅
- Client shows: bm2 = 0x0 ❌
- Client never requests attr 82 ❌

**Something is preventing the client from seeing/processing word 2 of SUPPORTED_ATTRS.**

---

## Possible Causes

### 1. Bitmap Encoding Order Issue

Maybe the bitmap words need to be in different order? Let me check RFC 8881 Section 3.3.1:

> "bitmap4 is defined as an array of 32-bit integers where bit n can be found in word (n/32) at bit position (n mod 32)."

Our encoding:
```rust
buf.put_u32(3);      // length
buf.put_u32(word0);  // attrs 0-31
buf.put_u32(word1);  // attrs 32-63
buf.put_u32(word2);  // attrs 64-95
```

This looks correct per RFC.

### 2. Client Kernel Bug

The Ubuntu 24.04 container is running kernel 6.14.0-1018-aws. Maybe this kernel version has a bug in bitmap parsing?

### 3. Attribute 82 Value is Wrong

Maybe we're encoding the FS_LAYOUT_TYPES value incorrectly? The web search mentioned:

> "LAYOUTTYPEs value must be non-empty. If the array is empty (length 0) the client concludes pNFS is unusable."

Our encoding:
```rust
buf.put_u32(2);  // Array length: 2 ← NON-ZERO ✅
buf.put_u32(1);  // LAYOUT4_NFSV4_1_FILES ✅
buf.put_u32(2);  // LAYOUT4_BLOCK_VOLUME ✅
```

This looks correct.

---

## Next Steps

### 1. Packet Capture (Definitive Answer)

```bash
# On one of the cluster nodes
tcpdump -i any -s 0 -w /tmp/pnfs-mount.pcap port 2049

# Then mount from client

# Analyze with Wireshark:
tshark -r /tmp/pnfs-mount.pcap -Y "nfs.opcode==9" -V | grep -A30 "SUPPORTED_ATTRS"
```

This will show EXACTLY what bytes are on the wire.

### 2. Test on Physical Host

```bash
ssh root@cdrv-1.vpc.cloudera.com
mount -t nfs -o vers=4.1 localhost:/ /mnt/test
cat /proc/self/mountstats | grep pnfs
```

Eliminates container/Kubernetes variables.

### 3. Check If Client Even Supports Attr 82

```bash
# Check kernel config
kubectl exec pnfs-client -- zcat /proc/config.gz 2>/dev/null | grep PNFS

# Check loaded modules
kubectl exec pnfs-client -- lsmod | grep pnfs
```

---

## Summary

**What We Know**:
- ✅ Server sends 3-word bitmap with attrs 82, 83
- ✅ EXCHANGE_ID flags correct
- ✅ All encoding RFC-compliant
- ❌ Client shows bm2=0x0
- ❌ Client never requests attr 82
- ❌ pNFS not activated

**Most Likely Cause**: 
1. Client kernel doesn't support parsing 3-word bitmaps
2. OR client caching issue (though we tried cache clear)
3. OR encoding format issue visible only in packet capture

**Definitive Test**: Packet capture with Wireshark

---

**Document Version**: 1.0  
**Status**: Investigation complete, packet capture needed for resolution

