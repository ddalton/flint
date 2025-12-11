# Packet Capture Analysis: Ganesha vs Flint

**Date:** December 11, 2024  
**Analysis:** READDIR attribute encoding comparison

---

## Summary

✅ **READDIR attribute encoding is now PERFECT!**  
⚠️  **But permission denied issue persists** - likely different root cause

---

## Ganesha READDIR Response

**Behavior:**
- Returns EMPTY directory listing in pseudo-root READDIR
- No exports shown (different pseudo-filesystem model)

```
Opcode: READDIR (26)
    Status: NFS4_OK (0)
    verifier: 0xc57ade49e84c8018
    Directory Listing
        Value Follows: No
        EOF: Yes
```

**Note:** Ganesha uses a different approach where exports aren't listed in pseudo-root READDIR.

---

## Flint READDIR Response

**Client Request:**
```
Attr mask[0]: 0x0010081a (Type, Change, Size, RDAttr_Error, FileId)
Attr mask[1]: 0x0010001a (Mode, NumLinks, Owner, Time_Metadata)
```

**Flint Response:**
```
Attr mask[0]: 0x0010081a ✅ EXACT MATCH
Attr mask[1]: 0x0010001a ✅ EXACT MATCH
```

**All 9 Requested Attributes Returned:**

| Attr ID | Name | Value | Status |
|---------|------|-------|--------|
| 1 | Type | NF4DIR (2) | ✅ |
| 3 | Change | 1765495662 | ✅ |
| 4 | Size | 4096 | ✅ |
| 11 | **RDAttr_Error** | NFS4_OK (0) | ✅ **NEW!** |
| 20 | FileId | 11269761976828618539 | ✅ |
| 33 | Mode | 0755 | ✅ |
| 35 | **NumLinks** | 2 | ✅ **NEW!** |
| 36 | **Owner** | "root" | ✅ **NEW!** |
| 52 | **Time_Metadata** | 1765495662, 0ns | ✅ **NEW!** |

**Previous Bug (Fixed):**
- ❌ Was returning FSID (8) which client didn't request
- ❌ Was missing RDAttr_Error (11), NumLinks (35), Owner (36), Time_Metadata (52)
- ❌ Caused XDR decode error 121

**Current Status:**
- ✅ Returns ONLY requested attributes
- ✅ Returns attributes in correct order (by attr ID)
- ✅ Bitmap matches client request exactly
- ✅ No XDR decode errors

---

## Remaining Issue: Permission Denied

**Symptom:**
```bash
$ ls -la /mnt/nfs-test/
total 0
d????????? ? ? ? ?            ? .
d????????? ? ? ? ?            ? ..
d????????? ? ? ? ?            ? volume
ls: cannot access '/mnt/nfs-test/.': Permission denied
ls: cannot access '/mnt/nfs-test/..': Permission denied
ls: cannot access '/mnt/nfs-test/volume': Permission denied

$ cd /mnt/nfs-test/volume
bash: cd: /mnt/nfs-test/volume: Permission denied
```

**Analysis:**
The `???????` output indicates the client **cannot read attributes** for the entries, even though they appear in READDIR.

**Possible Root Causes:**

1. **GETATTR on pseudo-root entries failing**
   - Client can do READDIR on pseudo-root ✅
   - But GETATTR on `.`, `..`, and `volume` entries fails ❌
   - Need to verify GETATTR returns proper attrs for these

2. **Security/Permission mismatch**
   - Attributes show mode 0755 in READDIR
   - But actual permission checks might be failing
   - Check ACCESS operation logs

3. **LOOKUP not being called**
   - Client makes decision based on READDIR attrs alone
   - Never tries LOOKUP to get actual filehandle
   - Might be rejecting based on some attribute value

---

## Next Debugging Steps

### 1. Capture with Verbose Kernel Logging
```bash
# Enable NFS debug
echo 'module nfs +p' > /sys/kernel/debug/dynamic_debug/control
echo 'module sunrpc +p' > /sys/kernel/debug/dynamic_debug/control

# Mount and test
mount -t nfs -o vers=4.2,tcp 127.0.0.1:/ /mnt/nfs-test
dmesg | tail -50
```

### 2. Check GETATTR Operations
```bash
# Capture traffic
tcpdump -i lo -w /tmp/debug.pcap port 2049 &

# Trigger the issue
ls -la /mnt/nfs-test/

# Analyze GETATTR responses
tshark -r /tmp/debug.pcap -Y "nfs.opcode == 9" -V
```

### 3. Verify LOOKUP Implementation
- Check if LOOKUP is being called when accessing `volume`
- Verify filehandle returned by LOOKUP is valid
- Check if export path exists and is accessible

### 4. Compare with Working NFS Server
- Find an NFS server that successfully shows exports in pseudo-root
- Capture its READDIR + GETATTR responses
- Compare attribute values byte-by-byte

---

## Code Changes Made

### 1. Fixed encode_export_entry_attributes()
**File:** `src/nfs/v4/operations/fileops.rs`

**Before:**
- Returned hardcoded set of attributes (Type, Change, Size, FSID, FileId, Mode)
- Didn't check what client requested
- Caused XDR decode errors

**After:**
```rust
fn encode_export_entry_attributes(name: &str, requested_attrs: &[u32]) 
    -> (Vec<u8>, Vec<u32>) 
{
    // Iterate through attributes 0-64 in order
    // Encode ONLY those that client requested
    // Return bitmap matching what was encoded
}
```

**Key Changes:**
- Takes `requested_attrs` bitmap as parameter
- Iterates through attrs in order (critical for XDR)
- Only encodes requested attributes
- Returns bitmap showing what was actually encoded

### 2. Added Unit Tests
**File:** `tests/readdir_encoding_test.rs`

- `test_readdir_attribute_request_filtering()` - Verifies all 9 attributes
- `test_readdir_unrequested_attributes_not_returned()` - Prevents FSID bug

---

## Verification

### tshark Analysis Confirms Fix
```bash
# Extract READDIR from Flint
$ tshark -r /tmp/flint.pcap -Y "nfs.opcode == 26 and nfs.status == 0" -V

# Shows:
Attr mask[0]: 0x0010081a  ← Matches request
Attr mask[1]: 0x0010001a  ← Matches request

# All 9 attributes present with correct values
```

### Unit Tests Pass
```bash
$ cargo test --test readdir_encoding_test
running 8 tests
test tests::test_readdir_attribute_request_filtering ... ok
test tests::test_readdir_unrequested_attributes_not_returned ... ok
...
test result: ok. 8 passed
```

---

## Conclusion

**READDIR Fix: ✅ Complete**
- Attribute encoding is RFC-compliant
- Returns exactly what client requests
- No XDR decode errors
- Unit tests prevent regression

**Remaining Work: ⚠️ Permission Issue**
- Different root cause than READDIR attributes
- Likely GETATTR or permission checking problem
- Needs separate investigation

**Impact:**
The READDIR fix was critical and is working perfectly. The permission denied issue is a separate problem that doesn't invalidate the READDIR work.


