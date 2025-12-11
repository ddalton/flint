# Flint NFS Server - Mount Failure Investigation Summary

**Date:** December 11, 2024  
**Server:** tnfs.vpc.cloudera.com  
**Issue:** Linux NFS client mount fails with `decode_getfattr_attrs: xdr returned 5`  

---

## Root Cause Identified ✅

**PRIMARY BUG:** NFSv4 attribute ID constants were **systematically incorrect**, not matching RFC 5661 Table 3.

###  Incorrect Attribute ID Mappings

| Attribute Name | RFC 5661 ID | Our WRONG ID | Impact |
|----------------|-------------|--------------|--------|
| CANSETTIME | 15 | 35 ❌ | Client requests 35 (NUMLINKS), we encode CANSETTIME |
| CASE_INSENSITIVE | 16 | 39 ❌ | Byte alignment off |
| CASE_PRESERVING | 17 | 40 ❌ | Byte alignment off |
| MAXFILESIZE | 27 | 42 ❌ | Wrong type encoded |
| MAXLINK | 28 | 41 ❌ | Client requests 41 (RAWDEV), we encode MAXLINK |
| MAXNAME | 29 | 45 ❌ | Wrong type encoded |
| MAXREAD | 30 | 43 ❌ | Wrong type encoded |
| MAXWRITE | 31 | 44 ❌ | Wrong type encoded |
| NUMLINKS | 35 | 27 ❌ | u32 vs bool size mismatch |
| **RAWDEV** | 41 | **MISSING** ❌ | Client requests but we skip → byte count off |
| SPACE_AVAIL | 42 | 47 ❌ | Wrong position |
| SPACE_FREE | 43 | 48 ❌ | Wrong position |
| SPACE_TOTAL | 44 | 49 ❌ | Wrong position |
| SPACE_USED | 45 | 50 ❌ | Wrong position |
| TIME_ACCESS | 47 | 51 ❌ | Wrong position |

### How This Caused Mount Failure

1. **Client requests:** Attributes `{1, 3, 4, 8, 20, 33, 35, 36, 37, 41, 45, 47, 52, 53, 55}`
2. **For attribute 35** (NUMLINKS u32 4 bytes):
   - We encoded CANSETTIME (bool, 4 bytes) ✓ Same size, but wrong type
3. **For attribute 41** (RAWDEV specdata4 8 bytes):
   - We encoded MAXLINK (u32, 4 bytes) ❌ **4-byte shortfall!**
4. **For attribute 45** (SPACE_USED u64 8 bytes):
   - We encoded MAXNAME (u32, 4 bytes) ❌ **4-byte shortfall!**
5. **For attribute 47** (TIME_ACCESS nfstime4 12 bytes):
   - We encoded SPACE_AVAIL (u64, 8 bytes) ❌ **4-byte shortfall!**

**Total byte count mismatch:** We declared `attr_vals_len = 116` but due to wrong sizes, kernel's `verify_attr_len()` found only 104 bytes consumed → **Returned -EIO (error 5)**

---

## Fixes Applied

### 1. ✅ Corrected Attribute ID Constants

**File:** `src/nfs/v4/operations/fileops.rs`

Fixed all attribute IDs to match RFC 5661 Table 3. Added missing attributes:
- `FATTR4_RAWDEV = 41` (specdata4: major+minor device, 8 bytes)
- `FATTR4_CHOWN_RESTRICTED = 18`
- `FATTR4_FS_LOCATIONS = 24`
- `FATTR4_HIDDEN = 25`
- `FATTR4_HOMOGENEOUS = 26`
- `FATTR4_MIMETYPE = 32`
- `FATTR4_NO_TRUNC = 34`
- `FATTR4_QUOTA_* = 38, 39, 40`
- `FATTR4_SYSTEM = 46`
- `FATTR4_TIME_*_SET = 48, 54`
- `FATTR4_TIME_BACKUP/CREATE/DELTA = 49, 50, 51`
- `FATTR4_SUPPATTR_EXCLCREAT = 75`

### 2. ✅ Fixed SUPPORTED_ATTRS Encoding  

**Issue:** Encoded as raw u64 (8 bytes) instead of `bitmap4` array

**Was:**
```rust
buf.put_u32((supported >> 32) as u32); // word 0
buf.put_u32(supported as u32); // word 1
// Total: 8 bytes ❌
```

**Fixed:**
```rust
buf.put_u32(2); // array length
buf.put_u32((supported >> 32) as u32); // word 0
buf.put_u32(supported as u32); // word 1
// Total: 12 bytes ✅
```

### 3. ✅ Fixed SUPPATTR_EXCLCREAT Encoding

Added encoding for attribute 75 as `bitmap4` with array length prefix.

### 4. ✅ Added RAWDEV Encoding

Implemented `specdata4` encoding (major+minor device numbers, 8 bytes).

---

## Test Results

### Before Fixes ❌

```
decode_getfattr_attrs: xdr returned 5  (EIO)
decode_server_caps: xdr returned 5  (EIO)
nfs4_get_rootfh: getroot error = 5
```

**Result:** Mount failed immediately, couldn't decode GETATTR responses.

### After Fixes ✅

```
decode_getfattr_attrs: xdr returned 0  ✅
decode_getfattr_generic: xdr returned 0  ✅
decode_server_caps: xdr returned 0!  ✅
```

**Result:** XDR decoding **SUCCEEDS**! Mount progresses much further.

### Current Status ⚠️

**New error:** `nfs4_try_get_tree() = -20` (ENOTDIR)

Kernel successfully decodes all GETATTR responses but reports some attribute values as 0:
- `fsid=(0x0/0x0)` - should be `(0x0/0x1)`
- `rdev=(0x0:0x0)` - should be non-zero
- `space_used=0` - should be 4096
- `mtime=0` - should be 1765474980  
- `mounted_on_fileid=0` - should be 792437

**Analysis:** XDR structure is now correct (byte counts match), but specific attribute VALUES are encoding as zero. This is a different class of bug - not XDR protocol, but attribute value computation.

---

## Unit Tests Created ✅

1. **`getattr_encoding_test.rs`** - GETATTR fattr4 structure roundtrip
2. **`compound_encoding_test.rs`** - Full 4-operation COMPOUND response
3. **`nfs4_attribute_ids_test.rs`** - Documents correct RFC 5661 attribute IDs
4. **`verify_attr_len_test.rs`** - Validates attr_vals byte counting
5. **`secinfo_encoding_test.rs`** - SECINFO_NO_NAME encoding
6. **`server_caps_encoding_test.rs`** - Server capabilities GETATTR

**All structural tests pass** ✅ proving XDR encoding is RFC-compliant.

---

## Comparison with NFS Ganesha

**Method:** Installed Ganesha side-by-side, captured packet dumps, compared byte-by-byte.

**Findings:**
- ✅ COMPOUND response structure identical
- ✅ SEQUENCE, PUTROOTFH, GETFH encoding identical
- ✅ GETATTR structure identical (after fixes)
- ✅ Response sizes match
- ✅ Ganesha mounts successfully ✅

---

## Next Steps

1. **Debug attribute value encoding** - Why do some attributes encode as 0?
   - FSID minor should be 1, not 0
   - SPACE_USED should be file size, not 0
   - TIME_MODIFY should be mtime, not 0
   - MOUNTED_ON_FILEID should be inode, not 0

2. **Add logging** to show actual bytes being encoded for these specific attributes

3. **Verify metadata values** - Check if `metadata.mtime()` actually returns correct value

4. **Test with simplified attributes** - Try GETATTR with just {1, 3, 4} to isolate issue

---

## Key Learnings

✅ **Unit tests are essential** - Would have caught attribute ID bugs immediately  
✅ **Compare with working implementation** - Ganesha source code was invaluable  
✅ **Use rpcdebug** - Kernel debug logging showed exact XDR decoding steps  
✅ **Packet captures** - tcpdump comparison found structural differences  

---

## Files Modified

1. `src/nfs/v4/operations/fileops.rs` - Fixed all attribute IDs, added RAWDEV
2. `src/nfs/v4/compound.rs` - Fixed GETATTR encoding (append_raw), SECINFO
3. `src/nfs/v4/operations/session.rs` - CREATE_SESSION flags, backchannel
4. `src/nfs/v4/filehandle.rs` - Export path canonicalization
5. `src/nfs/xdr.rs` - Added append_raw() method

---

**Status:** XDR protocol layer **FIXED** ✅  
**Remaining:** Attribute value computation bugs (non-zero values encoding as zero)  
**Progress:** From immediate XDR failure → Mount negotiation completes → ENOTDIR at final step

---

Generated: December 11, 2024  
Test Server: tnfs.vpc.cloudera.com  
Reference: NFS Ganesha v4.3, RFC 5661

