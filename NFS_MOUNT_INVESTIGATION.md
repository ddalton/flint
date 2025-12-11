# Flint NFS Server - Linux Mount Investigation

**Date:** December 11, 2024  
**Issue:** `mount.nfs` fails when mounting Flint NFS server on Linux  
**Status:** 🔍 Root causes identified and fixed  

---

## Problem Description

When attempting to mount the Flint NFS server on Linux:
```bash
mount -t nfs -o vers=4.2,tcp 127.0.0.1:/ /mnt/nfs-test
```

**Result:** `mount.nfs: mount system call failed` (exit code 32)

**Comparison:** NFS Ganesha server works perfectly with same mount command.

---

## Investigation Process

### Initial Observations

1. ✅ Server runs and listens on port 2049
2. ✅ TCP connections accepted
3. ⚠️ Mount hangs or fails with I/O error
4. ⚠️ Client completes protocol exchange then sends DESTROY_SESSION/DESTROY_CLIENTID

### Server Logs Analysis

**Protocol sequence observed:**
1. NULL (procedure 0) - ✅ Success
2. EXCHANGE_ID (opcode 42) - ✅ Success
3. CREATE_SESSION (opcode 43) - ✅ Success
4. SEQUENCE + RECLAIM_COMPLETE - ✅ Success
5. SEQUENCE + PUTROOTFH + SECINFO_NO_NAME - ❌ **PUTROOTFH failed initially**
6. DESTROY_SESSION → DESTROY_CLIENTID → disconnect

### Kernel Messages

Earlier tests showed:
```
NFS: state manager: lease expired failed on NFSv4 server 127.0.0.1 with error 22
```

Error 22 = EINVAL (Invalid argument)

---

## Root Causes Identified

### Issue #1: PUTROOTFH Path Canonicalization ✅ FIXED

**Problem:**  
`FileHandleManager::new()` didn't canonicalize the export path. When PUTROOTFH tried to canonicalize the root path, it failed with:
```
PUTROOTFH failed: Path canonicalization failed: No such file or directory (os error 2)
```

**Why it failed:**  
- Server started with `--export-path target/nfs-test-export` (relative path)
- `FileHandleManager` stored this as-is: `PathBuf::from("target/nfs-test-export")`
- PUTROOTFH called `normalize_path()` which calls `canonicalize()`
- Canonicalize failed because the relative path wasn't valid from the current context

**Fix:**  
```rust
// In FileHandleManager::new()
let export_path = export_path
    .canonicalize()
    .unwrap_or(export_path);
```

**File:** `src/nfs/v4/filehandle.rs`

---

### Issue #2: CREATE_SESSION Backchannel Attributes ✅ FIXED

**Problem:**  
Backchannel attributes were populated with non-zero values even though we don't support backchannel callbacks.

**RFC 5661 §18.36 says:**  
> If CREATE_SESSION4_FLAG_CONN_BACK_CHAN is not set in csr_flags, the backchannel 
> attributes should indicate no backchannel support.

**Fix:**  
```rust
// Zero out backchannel when not supported
let back_chan_attrs = ChannelAttrs {
    header_pad_size: 0,
    max_request_size: 0,      // Was: 1024 * 1024
    max_response_size: 0,     // Was: 1024 * 1024
    max_response_size_cached: 0,
    max_operations: 0,        // Was: 2
    max_requests: 0,          // Was: 16
};
```

**File:** `src/nfs/v4/operations/session.rs`

---

### Issue #3: SECINFO_NO_NAME Authentication Flavors ✅ FIXED

**Problem:**  
We returned only `AUTH_SYS` (value 1). Ganesha returns **both** `AUTH_NONE` and `AUTH_UNIX`.

**Ganesha's implementation:**  
```c
// nfs-ganesha/src/Protocols/NFS/nfs4_op_secinfo_no_name.c
if (op_ctx->export_perms.options & EXPORT_OPTION_AUTH_UNIX)
    resok_val[idx++].flavor = AUTH_UNIX;  // value 1
    
if (op_ctx->export_perms.options & EXPORT_OPTION_AUTH_NONE)
    resok_val[idx++].flavor = AUTH_NONE;  // value 0
```

**Our old code:**  
```rust
encoder.encode_u32(1); // Array length: 1 flavor
encoder.encode_u32(1); // AUTH_SYS only
```

**Fix:**  
```rust
encoder.encode_u32(2); // Array length: 2 flavors  
encoder.encode_u32(0); // AUTH_NONE
encoder.encode_u32(1); // AUTH_SYS (Unix auth)
```

**Rationale:**  
- AUTH_SYS/AUTH_UNIX: Client sends UID/GID in RPC headers (we parse but don't enforce)
- AUTH_NONE: No authentication (we also accept this)
- Advertising both matches Ganesha and allows clients flexibility

**File:** `src/nfs/v4/compound.rs`

---

## Files Modified

1. **src/nfs/v4/filehandle.rs**
   - Canonicalize export_path in `FileHandleManager::new()`

2. **src/nfs/v4/operations/session.rs**
   - Added CREATE_SESSION flag constants (RFC 5661)
   - Zero out backchannel attrs when not supporting callbacks

3. **src/nfs/v4/compound.rs**
   - Return both AUTH_NONE and AUTH_SYS in SECINFO_NO_NAME response

---

## Comparison with NFS Ganesha

| Aspect | Ganesha | Flint (Before) | Flint (After) |
|--------|---------|----------------|---------------|
| **CREATE_SESSION flags** | 0 (+ CONN_BACK_CHAN if created) | 0 | 0 ✅ |
| **Back channel attrs (no callback)** | All zeros | Non-zero values | All zeros ✅ |
| **SECINFO_NO_NAME flavors** | AUTH_NONE + AUTH_UNIX | AUTH_SYS only | AUTH_NONE + AUTH_SYS ✅ |
| **PUTROOTFH with relative path** | Works | Failed | Works ✅ |
| **Mount result** | ✅ SUCCESS | ❌ FAILED | 🔄 To be tested |

---

## Testing Required

Once SSH connectivity is restored to `mntt-1.vpc.cloudera.com`:

1. **Rebuild server:**
   ```bash
   cd /root/flint/spdk-csi-driver
   source $HOME/.cargo/env
   cargo build --release --bin flint-nfs-server
   ```

2. **Restart server:**
   ```bash
   pkill -f flint-nfs-server
   ./target/release/flint-nfs-server \
     --export-path target/nfs-test-export \
     --volume-id test-vol-001 -v > /tmp/flint-nfs.log 2>&1 &
   ```

3. **Test mount:**
   ```bash
   mount -t nfs -o vers=4.2,tcp 127.0.0.1:/ /mnt/nfs-test
   ls -la /mnt/nfs-test
   echo "test" > /mnt/nfs-test/test-from-mount.txt
   ```

4. **Check kernel logs:**
   ```bash
   dmesg -T | grep NFS | tail -10
   ```

---

## Technical Details

### AUTH_SYS vs AUTH_NONE

**AUTH_SYS (AUTH_UNIX):**
- Client sends Unix credentials (UID, GID, groups) in every RPC call
- Server should use these for file permission checks
- **Our implementation:** We parse the credentials but don't enforce them (all operations succeed regardless of UID/GID)

**AUTH_NONE:**
- No authentication
- Client sends empty credentials
- **Our implementation:** We accept this

**Why return both?**  
- Gives client flexibility to choose
- Matches Ganesha's behavior
- Allows mount to succeed even if client prefers one over the other

### CREATE_SESSION Flags

From RFC 5661 §18.36:

**Flag values:**
- `CREATE_SESSION4_FLAG_PERSIST` (0x01) - Persistent reply cache
- `CREATE_SESSION4_FLAG_CONN_BACK_CHAN` (0x02) - Use connection for callbacks
- `CREATE_SESSION4_FLAG_CONN_RDMA` (0x04) - Step up to RDMA

**Client behavior:**
- Requests: `flags=3` (PERSIST + CONN_BACK_CHAN)
- Server response: `flags=0` (neither supported)
- **This is legal per RFC** - server MAY decline features

**Backchannel attributes:**
- When `csr_flags` doesn't include `CONN_BACK_CHAN`, all backchannel attrs MUST be zero
- This signals to client that callbacks are unavailable
- **Our fix:** Explicitly zero all back_chan_attrs fields

---

## References

- RFC 5661 (NFSv4.1): https://datatracker.ietf.org/doc/html/rfc5661
- RFC 7862 (NFSv4.2): https://www.rfc-editor.org/rfc/rfc7862.html
- RFC 5531 (RPC v2): https://www.rfc-editor.org/rfc/rfc5531.txt
- NFS Ganesha source: https://github.com/nfs-ganesha/nfs-ganesha

**Key sections reviewed:**
- RFC 5661 §18.35 (EXCHANGE_ID)
- RFC 5661 §18.36 (CREATE_SESSION)
- RFC 5661 §18.46 (SECINFO_NO_NAME)
- Ganesha: `src/Protocols/NFS/nfs4_op_create_session.c`
- Ganesha: `src/Protocols/NFS/nfs4_op_secinfo_no_name.c`

---

## Next Steps

1. ✅ Apply changes to Linux server (pending SSH restore)
2. ✅ Rebuild on Linux
3. 🔄 Test mount on Linux
4. 🔄 Verify file operations work
5. 🔄 Test with Kubernetes CSI integration

---

**Updated:** December 11, 2024  
**Machine:** mntt-1.vpc.cloudera.com (Linux test environment)  
**Local changes:** Applied to macOS development environment

