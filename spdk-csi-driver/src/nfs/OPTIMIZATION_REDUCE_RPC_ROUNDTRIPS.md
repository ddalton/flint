# NFS Server Optimization: Reduce RPC Round Trips

## Executive Summary

This document outlines opportunities to leverage NFSv3 protocol features to reduce round-trip RPC calls between client and server. NFSv3 was specifically designed with `post_op_attr` and `wcc_data` (weak cache consistency) fields to eliminate the need for separate GETATTR calls that plagued NFSv2.

**Key Insight:** By properly populating optional attribute fields in RPC responses, we can reduce total RPC round trips by **30-50%** for typical workloads.

## Problem Analysis

Currently, the server skips many optional attribute fields in NFSv3 responses, forcing clients to issue additional GETATTR RPCs for cache coherency. This doubles or triples the number of network round trips for common operations.

---

## Optimization #1: LOOKUP Missing Directory Attributes (HIGH PRIORITY)

### Location
`handlers.rs:162`

### Current Code
```rust
// Lookup in filesystem
match fs.lookup(&dir_handle, &name).await {
    Ok((file_handle, attrs)) => {
        let mut reply = ReplyBuilder::success(call.xid);
        let enc = reply.encoder();

        // Status: NFS3_OK
        enc.encode_u32(NFS3Status::Ok as u32);

        // File handle
        file_handle.encode(enc);

        // Object attributes (optional but we always provide)
        enc.encode_bool(true); // obj_attributes_follow
        attrs.encode(enc);

        // Directory attributes (optional, we skip for simplicity)
        enc.encode_bool(false); // dir_attributes_follow  // ❌ MISSING

        reply.finish()
    }
```

### Problem
Every LOOKUP operation skips the parent directory attributes. Clients need these for:
- Cache coherency checks (detecting if directory changed)
- Avoiding a separate GETATTR on the parent directory
- Properly managing their attribute cache lifetimes

### Impact
- **Operations affected:** Every file open, stat, or access
- **Extra RPCs:** 1 additional GETATTR per LOOKUP in many cases
- **Workload impact:** 20-40% RPC reduction for file-access-heavy workloads
- **Latency impact:** Saves 1 network round trip (typically 0.1-1ms on local networks, 10-50ms on WAN)

### Solution

**File:** `handlers.rs` (around line 145-164)

```rust
// Lookup in filesystem
match fs.lookup(&dir_handle, &name).await {
    Ok((file_handle, attrs)) => {
        // Get parent directory attributes
        let dir_attrs = fs.getattr(&dir_handle).await.ok();

        let mut reply = ReplyBuilder::success(call.xid);
        let enc = reply.encoder();

        // Status: NFS3_OK
        enc.encode_u32(NFS3Status::Ok as u32);

        // File handle
        file_handle.encode(enc);

        // Object attributes (optional but we always provide)
        enc.encode_bool(true); // obj_attributes_follow
        attrs.encode(enc);

        // Directory attributes (post_op_attr)
        if let Some(attr) = dir_attrs {
            enc.encode_bool(true); // dir_attributes_follow
            attr.encode(enc);
        } else {
            enc.encode_bool(false);
        }

        reply.finish()
    }
    Err(e) => {
        warn!("LOOKUP failed: {}", e);

        // Even on error, try to return directory attributes
        let mut reply = ReplyBuilder::success(call.xid);
        let enc = reply.encoder();
        enc.encode_u32(NFS3Status::from_io_error(&e) as u32);

        // Try to provide directory attributes for cache coherency
        let dir_attrs = fs.getattr(&dir_handle).await.ok();
        if let Some(attr) = dir_attrs {
            enc.encode_bool(true);
            attr.encode(enc);
        } else {
            enc.encode_bool(false);
        }

        reply.finish()
    }
}
```

### Performance Considerations
- The extra `getattr()` on the parent is worth it because:
  1. Directory attributes are often cached in OS page cache
  2. Saves an entire RPC round trip to the client
  3. Network latency >> syscall latency (1ms vs 10μs)
- The directory handle is already resolved, so this is just one additional stat() syscall

---

## Optimization #2: READDIR Missing Directory Attributes (MEDIUM PRIORITY)

### Location
`handlers.rs:724-725`

### Current Code
```rust
match fs.readdir(&dir_handle, cookie, count).await {
    Ok(entries) => {
        let mut reply = ReplyBuilder::success(call.xid);
        let enc = reply.encoder();

        // Status: NFS3_OK
        enc.encode_u32(NFS3Status::Ok as u32);

        // Directory attributes (optional, we skip)
        enc.encode_bool(false);  // ❌ MISSING

        // Cookie verifier
        enc.encode_u64(0);

        // ... entries ...
    }
}
```

### Problem
Clients use directory attributes to detect if the directory changed during or after READDIR. Without these attributes, they must issue a separate GETATTR.

### Impact
- **Operations affected:** Every `ls`, `readdir()`, directory scan
- **Extra RPCs:** 1 GETATTR per READDIR operation
- **Workload impact:** 10-15% RPC reduction for metadata-heavy workloads
- **Latency impact:** Saves 1 network round trip per directory listing

### Solution

**File:** `handlers.rs` (around line 716-726)

```rust
match fs.readdir(&dir_handle, cookie, count).await {
    Ok(entries) => {
        // Get directory attributes for cache coherency
        let dir_attrs = fs.getattr(&dir_handle).await.ok();

        let mut reply = ReplyBuilder::success(call.xid);
        let enc = reply.encoder();

        // Status: NFS3_OK
        enc.encode_u32(NFS3Status::Ok as u32);

        // Directory attributes (post_op_attr)
        if let Some(attr) = dir_attrs {
            enc.encode_bool(true);
            attr.encode(enc);
        } else {
            enc.encode_bool(false);
        }

        // Cookie verifier
        enc.encode_u64(0);

        // ... rest of entries encoding ...
    }
    Err(e) => {
        warn!("READDIR failed: {}", e);

        let mut reply = ReplyBuilder::success(call.xid);
        let enc = reply.encoder();
        enc.encode_u32(NFS3Status::from_io_error(&e) as u32);

        // Provide directory attributes even on error
        let dir_attrs = fs.getattr(&dir_handle).await.ok();
        if let Some(attr) = dir_attrs {
            enc.encode_bool(true);
            attr.encode(enc);
        } else {
            enc.encode_bool(false);
        }

        reply.finish()
    }
}
```

---

## Optimization #3: WRITE Returns Attributes Directly (MEDIUM PRIORITY)

### Location
`handlers.rs:292`, `vfs.rs:133`

### Current Code

**handlers.rs:**
```rust
// Write to filesystem
match fs.write(&file_handle, offset, &data).await {
    Ok(written) => {
        // Get updated attributes
        let attrs = fs.getattr(&file_handle).await.ok();  // ❌ EXTRA SYSCALL

        let mut reply = ReplyBuilder::success(call.xid);
        // ... encode reply with attrs ...
    }
}
```

**vfs.rs:**
```rust
pub async fn write(&self, fh: &FileHandle, offset: u64, data: &[u8]) -> io::Result<u32> {
    let path = self.resolve(fh)?;
    let data_owned = data.to_vec();
    let len = data_owned.len() as u32;

    tokio::task::spawn_blocking(move || {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)?;
        file.write_at(&data_owned, offset)?;
        Ok::<_, io::Error>(len)  // ❌ Only returns length
    })
    .await
    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))??;

    Ok(len)
}
```

### Problem
After every WRITE, we do a separate async `getattr()` call to get file attributes. This requires:
1. Another async operation
2. Another syscall (stat/fstat)
3. Another path resolution (in some cases)

### Impact
- **Operations affected:** Every WRITE operation
- **Extra syscalls:** 1 stat() per write
- **CPU impact:** 5-10% reduction on write-heavy workloads
- **Latency:** Saves ~10-50μs per write operation

### Solution

**Step 1:** Modify VFS write signature to return attributes

**File:** `vfs.rs` (around line 133)

```rust
/// Write data to a file
///
/// Returns (bytes_written, post_write_attributes)
pub async fn write(&self, fh: &FileHandle, offset: u64, data: &[u8])
    -> io::Result<(u32, FileAttr)> {
    let path = self.resolve(fh)?;
    let data_owned = data.to_vec();
    let len = data_owned.len() as u32;

    tokio::task::spawn_blocking(move || {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)?;

        // Write data
        file.write_at(&data_owned, offset)?;

        // Get metadata immediately (file descriptor already open)
        // This is cheaper than a separate stat() call
        let metadata = file.metadata()?;

        use std::os::unix::fs::MetadataExt;
        let fileid = metadata.ino();
        let attr = FileAttr::from_metadata(&metadata.into(), fileid);

        Ok::<_, io::Error>((len, attr))
    })
    .await
    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))??
}
```

**Step 2:** Update WRITE handler to use returned attributes

**File:** `handlers.rs` (around line 289)

```rust
// Write to filesystem
match fs.write(&file_handle, offset, &data).await {
    Ok((written, attrs)) => {  // ✅ Attributes returned directly
        let mut reply = ReplyBuilder::success(call.xid);
        let enc = reply.encoder();

        // Status: NFS3_OK
        enc.encode_u32(NFS3Status::Ok as u32);

        // File attributes before operation (we skip)
        enc.encode_bool(false);

        // File attributes after operation (always available now)
        enc.encode_bool(true);
        attrs.encode(enc);

        // Count of bytes written
        enc.encode_u32(written);

        // Commit level: UNSTABLE
        enc.encode_u32(0);

        // Write verifier
        enc.encode_u64(0);

        reply.finish()
    }
    Err(e) => {
        warn!("WRITE failed: {}", e);
        error_reply(call.xid, NFS3Status::from_io_error(&e))
    }
}
```

**Step 3:** Update other callers of `write()`

Check for any test code or other handlers that call `fs.write()` and update them to handle the new return type.

---

## Optimization #4: Error Replies Should Include Attributes (MEDIUM PRIORITY)

### Location
`handlers.rs:1452-1457` (error_reply helper)

### Current Code
```rust
/// Helper to create an error reply
fn error_reply(xid: u32, status: NFS3Status) -> Bytes {
    let mut reply = ReplyBuilder::success(xid);
    let enc = reply.encoder();
    enc.encode_u32(status as u32);  // ❌ No attributes
    reply.finish()
}
```

### Problem
Many NFS operations return `post_op_attr` or `wcc_data` even on error (per RFC 1813). This allows clients to update their attribute cache even when operations fail, avoiding subsequent GETATTR calls.

**Example:** Failed WRITE should still return file attributes so client knows current file state.

### Impact
- **Operations affected:** All error paths
- **Frequency:** Low (errors are uncommon), but important for correctness
- **Latency:** Saves 1 RPC on error recovery

### Solution

This is more complex because `error_reply()` doesn't have access to file handles or the filesystem. We need to handle attributes in each error path individually.

**Example for WRITE error path:**

**File:** `handlers.rs` (around line 328-332)

```rust
Err(e) => {
    warn!("WRITE failed: {}", e);

    let mut reply = ReplyBuilder::success(call.xid);
    let enc = reply.encoder();

    // Status
    enc.encode_u32(NFS3Status::from_io_error(&e) as u32);

    // wcc_data structure:
    // - pre_op_attr (we skip)
    enc.encode_bool(false);

    // - post_op_attr (try to get even on error)
    if let Ok(attrs) = fs.getattr(&file_handle).await {
        enc.encode_bool(true);
        attrs.encode(enc);
    } else {
        enc.encode_bool(false);
    }

    reply.finish()
}
```

**Apply similar pattern to:**
- WRITE error (line 328)
- REMOVE error (line 591)
- RMDIR error (line 661)
- CREATE error (line 425)
- MKDIR error (line 515)
- Other mutating operations

---

## Optimization #5: Implement pre_op_attr for WCC Data (LOWER PRIORITY)

### Location
Throughout `handlers.rs` - all operations that return `wcc_data`

### Current Pattern
```rust
// dir_wcc (wcc_data for parent directory):
enc.encode_bool(false); // pre_op_attr (skip)  // ❌ ALWAYS SKIPPED
if let Some(attr) = dir_attrs {
    enc.encode_bool(true); // post_op_attr
    attr.encode(enc);
} else {
    enc.encode_bool(false);
}
```

### Problem
WCC (Weak Cache Consistency) data includes both `pre_op_attr` (before operation) and `post_op_attr` (after operation). We always skip the `pre` attributes.

Having both allows clients to:
- Detect concurrent modifications by other clients
- Maintain more accurate attribute caches
- Implement better cache coherency

### Impact
- **Cache coherency:** Better detection of concurrent modifications
- **Complexity:** Requires capturing state before operations
- **Performance:** Minimal - just one extra stat() before the operation

### Solution (Example for WRITE)

**File:** `handlers.rs` (around line 288)

```rust
// Write to filesystem

// Get attributes BEFORE operation (for wcc_data)
let pre_attrs = fs.getattr(&file_handle).await.ok();

match fs.write(&file_handle, offset, &data).await {
    Ok((written, post_attrs)) => {
        let mut reply = ReplyBuilder::success(call.xid);
        let enc = reply.encoder();

        // Status: NFS3_OK
        enc.encode_u32(NFS3Status::Ok as u32);

        // wcc_data:
        // - pre_op_attr (before write)
        if let Some(attr) = pre_attrs {
            enc.encode_bool(true);
            // RFC 1813: pre_op_attr only includes size, mtime, ctime
            enc.encode_u64(attr.size);
            enc.encode_u32(attr.mtime_sec);
            enc.encode_u32(attr.mtime_nsec);
            enc.encode_u32(attr.ctime_sec);
            enc.encode_u32(attr.ctime_nsec);
        } else {
            enc.encode_bool(false);
        }

        // - post_op_attr (after write)
        enc.encode_bool(true);
        post_attrs.encode(enc);

        // ... rest of reply ...
    }
}
```

**Note:** This is lower priority because:
1. Most clients work fine without pre_op_attr
2. Requires refactoring to capture state before every operation
3. The performance gain is minimal compared to other optimizations

---

## Implementation Priority

### Phase 1: High Impact, Low Complexity
1. **LOOKUP directory attributes** - Biggest RPC reduction
2. **READDIR directory attributes** - Common operation improvement

### Phase 2: Medium Impact, Medium Complexity
3. **WRITE returns attributes** - Eliminates syscall, requires VFS signature change
4. **Error replies include attributes** - Better correctness, multiple small changes

### Phase 3: Lower Priority
5. **Implement pre_op_attr** - Marginal improvement, higher complexity

---

## Testing Plan

### Unit Tests

**File:** `tests.rs`

```rust
#[tokio::test]
async fn test_lookup_includes_directory_attributes() {
    // Setup filesystem and create a file
    let tmpdir = tempfile::tempdir().unwrap();
    let vfs = LocalFilesystem::new(tmpdir.path().to_path_buf()).unwrap();
    let root_fh = vfs.root_handle().unwrap();
    vfs.create(&root_fh, "test.txt", 0o644).await.unwrap();

    // Perform LOOKUP via the handler
    let mut enc = XdrEncoder::new();
    root_fh.encode(&mut enc);
    enc.encode_string("test.txt");
    let request = enc.finish();

    let call = CallMessage { /* ... */ };
    let reply = handle_lookup(Arc::new(vfs), &call, &mut XdrDecoder::new(request)).await;

    // Decode reply and verify directory attributes are present
    let mut dec = XdrDecoder::new(reply);
    let _xid = dec.decode_u32().unwrap();
    let _msg_type = dec.decode_u32().unwrap();
    let _reply_status = dec.decode_u32().unwrap();
    // ... skip auth ...
    let _accept_status = dec.decode_u32().unwrap();
    let _nfs_status = dec.decode_u32().unwrap();

    // Skip file handle and object attributes
    // ...

    // Check directory attributes are present
    let dir_attrs_present = dec.decode_bool().unwrap();
    assert!(dir_attrs_present, "Directory attributes should be included in LOOKUP reply");
}

#[tokio::test]
async fn test_write_returns_attributes_directly() {
    let tmpdir = tempfile::tempdir().unwrap();
    let vfs = LocalFilesystem::new(tmpdir.path().to_path_buf()).unwrap();
    let root_fh = vfs.root_handle().unwrap();

    let (file_fh, _) = vfs.create(&root_fh, "test.txt", 0o644).await.unwrap();

    // Write and verify attributes are returned
    let data = b"Hello, World!";
    let result = vfs.write(&file_fh, 0, data).await;

    assert!(result.is_ok());
    let (written, attrs) = result.unwrap();
    assert_eq!(written, data.len() as u32);
    assert_eq!(attrs.size, data.len() as u64);
}
```

### Integration Tests

**Test with actual NFS client:**

```bash
#!/bin/bash
# Test LOOKUP optimization

# Mount NFS share
sudo mount -t nfs -o vers=3,tcp,port=2049 localhost:/export /mnt/test

# Create test file
echo "test" > /mnt/test/testfile

# Monitor RPC calls with nfsstat
nfsstat -c -3 > before.txt

# Access file multiple times (should trigger LOOKUP)
for i in {1..100}; do
    cat /mnt/test/testfile > /dev/null
done

nfsstat -c -3 > after.txt

# Compare GETATTR counts
# Before optimization: ~200 GETATTRs (2 per access: file + directory)
# After optimization: ~100 GETATTRs (1 per access: just file)

# Calculate reduction
GETATTR_BEFORE=$(grep GETATTR before.txt | awk '{print $2}')
GETATTR_AFTER=$(grep GETATTR after.txt | awk '{print $2}')
REDUCTION=$((GETATTR_AFTER - GETATTR_BEFORE))

echo "GETATTR calls for 100 file accesses: $REDUCTION"
echo "Expected: ~100 (optimized) vs ~200 (unoptimized)"
```

### Performance Validation

**Before optimization:**
```bash
# Benchmark file access workload
time for i in {1..1000}; do cat /mnt/test/file$i > /dev/null; done
# Record: total time, RPC count, GETATTR count
```

**After optimization:**
```bash
# Same benchmark
time for i in {1..1000}; do cat /mnt/test/file$i > /dev/null; done
# Expect: 20-40% faster, 30-50% fewer RPCs
```

---

## Expected Performance Improvements

### Workload: File Access Heavy (e.g., compile, grep, find)
- **Before:** LOOKUP + GETATTR (parent) + GETATTR (file) = 3 RPCs per file access
- **After:** LOOKUP (with attrs) + GETATTR (file) = 2 RPCs per file access
- **Improvement:** 33% RPC reduction

### Workload: Directory Listing (e.g., ls -la)
- **Before:** READDIR + GETATTR (directory) = 2 RPCs
- **After:** READDIR (with attrs) = 1 RPC
- **Improvement:** 50% RPC reduction for directory operations

### Workload: Write Heavy
- **Before:** WRITE + GETATTR = 1 RPC + 1 syscall
- **After:** WRITE (returns attrs) = 1 RPC + 0 extra syscalls
- **Improvement:** 5-10% CPU reduction, eliminated syscall

### Overall Mixed Workload
- **Expected:** 30-50% reduction in total RPC round trips
- **Latency:** 20-40% faster for metadata-heavy operations
- **Throughput:** 10-20% higher throughput due to reduced network overhead

---

## References

- **RFC 1813 - NFS Version 3 Protocol**: https://datatracker.ietf.org/doc/html/rfc1813
  - Section 3.3.3 (LOOKUP): post_op_attr for directory
  - Section 3.3.7 (WRITE): wcc_data structure
  - Section 3.3.16 (READDIR): post_op_attr
- **NFSv3 Design Rationale**: WCC data was added specifically to reduce GETATTR traffic
- **Current implementation**: See `handlers.rs` for all operation handlers

---

## Notes

- These optimizations are **protocol-level** changes that use existing NFS v3 features
- They **do not** involve caching, which has separate consistency concerns
- They **reduce network round trips**, which is the primary bottleneck for NFS performance
- The optimizations are **backward compatible** - all fields being populated are already part of the NFSv3 spec

---

## Related Optimizations

See also:
- `OPTIMIZATION_ELIMINATE_COPYING.md` - Eliminate data copying in write path
- Future: Selective TCP flushing
- Future: RPC header parsing optimization
