# NFS Server Optimization: Eliminate Unnecessary Data Copying

## Executive Summary

This document outlines how to eliminate unnecessary copying of file contents in the NFS server's write path and UDP receive path. The primary optimization (write path) can eliminate copying of up to 1MB per write operation, resulting in estimated 5-15% CPU savings on write-heavy workloads.

## Problem Analysis

### Issue #1: Write Path Data Copy (HIGH PRIORITY)

**Location:** `vfs.rs:135`

**Current Code:**
```rust
pub async fn write(&self, fh: &FileHandle, offset: u64, data: &[u8]) -> io::Result<u32> {
    let path = self.resolve(fh)?;
    let data_owned = data.to_vec(); // ❌ EXPENSIVE: Copies entire write buffer
    let len = data_owned.len() as u32;

    tokio::task::spawn_blocking(move || {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)?;
        file.write_at(&data_owned, offset)?;
        Ok::<_, io::Error>(len)
    })
    .await
    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))??;

    Ok(len)
}
```

**Problem:**
- Copies up to 1MB of data per write (typical TCP rsize/wsize=1048576)
- Happens on every write operation
- Data is already in a `Bytes` buffer (Arc-backed) from XDR decoding
- The `.to_vec()` creates an entirely new allocation and memcpy

**Impact:**
- Estimated 100+ MB/sec of unnecessary copying on typical workloads
- 5-15% CPU overhead on write-heavy operations
- Memory pressure from duplicate allocations

---

### Issue #2: UDP Request Copy (LOWER PRIORITY)

**Location:** `server.rs:206`

**Current Code:**
```rust
async fn serve_udp(addr: &str, fs: Arc<LocalFilesystem>) -> std::io::Result<()> {
    let socket = Arc::new(UdpSocket::bind(addr).await?);
    let mut buf = vec![0u8; 65536];

    loop {
        let (len, peer) = socket.recv_from(&mut buf).await?;
        let request = Bytes::copy_from_slice(&buf[..len]); // ❌ Copies packet data
        // ...
    }
}
```

**Problem:**
- Copies every UDP packet (up to 64KB)
- Affects all UDP operations, not just writes
- UDP is less commonly used in production (TCP is standard)

**Impact:**
- Lower priority since UDP is rarely used
- Saves a few MB/sec if UDP transport is used

---

## Solution: Zero-Copy using Bytes

### Fix #1: Write Path (CRITICAL)

The key insight is that the data is already in a `Bytes` object from XDR decoding. `Bytes` is Arc-backed, so cloning it is just a pointer copy + ref count increment (cheap), not a data copy.

#### Step 1: Change VFS write signature

**File:** `vfs.rs` (around line 133)

**Change from:**
```rust
pub async fn write(&self, fh: &FileHandle, offset: u64, data: &[u8]) -> io::Result<u32>
```

**Change to:**
```rust
pub async fn write(&self, fh: &FileHandle, offset: u64, data: Bytes) -> io::Result<u32>
```

#### Step 2: Use cheap Bytes clone instead of to_vec()

**File:** `vfs.rs` (around line 135-136)

**Change from:**
```rust
let data_owned = data.to_vec(); // ❌ Expensive copy
let len = data_owned.len() as u32;
```

**Change to:**
```rust
let data_clone = data.clone(); // ✅ Cheap: just increments Arc ref count
let len = data_clone.len() as u32;
```

#### Step 3: Update spawn_blocking to use cloned Bytes

**File:** `vfs.rs` (around line 140-146)

**Change from:**
```rust
tokio::task::spawn_blocking(move || {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)?;
    file.write_at(&data_owned, offset)?;
    // ...
```

**Change to:**
```rust
tokio::task::spawn_blocking(move || {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)?;
    file.write_at(&data_clone, offset)?;
    // ...
```

Note: `write_at` accepts `&[u8]`, and `Bytes` derefs to `&[u8]`, so this works seamlessly.

#### Step 4: Update the caller in handlers.rs

**File:** `handlers.rs` (around line 280-289)

**Change from:**
```rust
// Decode data
let data = match dec.decode_opaque() {
    Ok(d) => d,
    Err(_) => return ReplyBuilder::garbage_args(call.xid),
};

// Write to filesystem
match fs.write(&file_handle, offset, &data).await {
    //                                  ^^^^^ passing &[u8]
```

**Change to:**
```rust
// Decode data
let data = match dec.decode_opaque() {
    Ok(d) => d,
    Err(_) => return ReplyBuilder::garbage_args(call.xid),
};

// Write to filesystem
match fs.write(&file_handle, offset, data).await {
    //                                 ^^^^ pass Bytes directly (no &)
```

#### Step 5: Add import for Bytes in vfs.rs (if not already present)

Check that `vfs.rs` imports Bytes:
```rust
use bytes::Bytes;
```

This should already be present at line 15, so no change needed.

---

### Fix #2: UDP Path (OPTIONAL)

This is a lower priority optimization but follows the same zero-copy pattern used in the TCP path.

#### Change UDP receive buffer handling

**File:** `server.rs` (around line 196-206)

**Change from:**
```rust
async fn serve_udp(addr: &str, fs: Arc<LocalFilesystem>) -> std::io::Result<()> {
    let socket = Arc::new(UdpSocket::bind(addr).await?);
    info!("NFS UDP server listening on {}", addr);

    let mut buf = vec![0u8; 65536];

    loop {
        let (len, peer) = socket.recv_from(&mut buf).await?;
        debug!("UDP request from {}, {} bytes", peer, len);

        let request = Bytes::copy_from_slice(&buf[..len]); // ❌ Copy
        let fs = fs.clone();
        let socket = socket.clone();

        // Handle request in separate task
        tokio::spawn(async move {
            let reply = dispatch(request, fs).await;
            // ...
        });
    }
}
```

**Change to:**
```rust
async fn serve_udp(addr: &str, fs: Arc<LocalFilesystem>) -> std::io::Result<()> {
    let socket = Arc::new(UdpSocket::bind(addr).await?);
    info!("NFS UDP server listening on {}", addr);

    // Use BytesMut for zero-copy split (same pattern as TCP path)
    let mut buf = BytesMut::with_capacity(65536);

    loop {
        // Prepare buffer for receive
        buf.clear();
        buf.reserve(65536);
        unsafe { buf.set_len(65536); }

        let (len, peer) = socket.recv_from(&mut buf[..]).await?;
        debug!("UDP request from {}, {} bytes", peer, len);

        // Zero-copy split (same as TCP at line 174)
        buf.truncate(len);
        let request = buf.split_to(len).freeze(); // ✅ Zero-copy

        let fs = fs.clone();
        let socket = socket.clone();

        // Handle request in separate task
        tokio::spawn(async move {
            let reply = dispatch(request, fs).await;
            // ...
        });
    }
}
```

**Add import:**
```rust
use bytes::BytesMut; // Add if not already present
```

---

## Testing Plan

### Unit Tests

1. **Verify existing tests still pass:**
   ```bash
   cargo test --package nfs
   ```

2. **Test concurrent writes:**
   The existing test in `tests.rs` around line 207 tests concurrent writes:
   ```bash
   cargo test test_concurrent_writes
   ```

### Integration Tests

1. **Mount NFS and test writes:**
   ```bash
   # Mount the NFS share
   sudo mount -t nfs -o vers=3,tcp localhost:/export /mnt/test

   # Write test
   dd if=/dev/zero of=/mnt/test/testfile bs=1M count=100

   # Verify
   md5sum /mnt/test/testfile
   ```

2. **Concurrent performance test:**
   ```bash
   # Run the concurrent test script
   ./concurrent_test.sh
   ```

### Performance Validation

**Before optimization:**
```bash
# Profile CPU usage during writes
perf record -g dd if=/dev/zero of=/mnt/test/bigfile bs=1M count=1000
perf report
# Look for time spent in to_vec() / memcpy
```

**After optimization:**
```bash
# Verify memcpy eliminated
perf record -g dd if=/dev/zero of=/mnt/test/bigfile bs=1M count=1000
perf report
# Should see reduced CPU in write path
```

### Regression Testing

Ensure these operations still work correctly:
- Small writes (< 4KB)
- Large writes (1MB)
- Partial writes
- Concurrent writes from multiple clients
- Write + immediate read verification
- COMMIT operation after UNSTABLE writes

---

## Expected Performance Improvements

### Write Path (Fix #1)

**Workload:** 100 writes/sec of 1MB each

**Before:**
- 100 MB/sec copied unnecessarily
- Extra CPU cycles for allocation + memcpy
- Memory pressure from duplicate buffers

**After:**
- 0 bytes copied (just Arc ref count increment)
- Estimated 5-15% CPU reduction on write-heavy workloads
- Reduced memory allocator pressure

### UDP Path (Fix #2)

**Workload:** 100 UDP requests/sec of 32KB each (typical)

**Before:**
- 3.2 MB/sec copied

**After:**
- 0 bytes copied

**Note:** Low impact since UDP is rarely used in production NFS deployments.

---

## Implementation Checklist

- [ ] **vfs.rs**: Change `write()` signature to accept `Bytes` instead of `&[u8]`
- [ ] **vfs.rs**: Replace `data.to_vec()` with `data.clone()` (cheap Arc increment)
- [ ] **vfs.rs**: Update variable names (`data_owned` → `data_clone`)
- [ ] **handlers.rs**: Update `handle_write()` to pass `data` instead of `&data`
- [ ] **Verify imports**: Ensure `use bytes::Bytes;` is present in vfs.rs
- [ ] **Run unit tests**: `cargo test --package nfs`
- [ ] **Run integration tests**: Mount and test actual NFS operations
- [ ] **Optional - server.rs**: Implement zero-copy UDP receive
- [ ] **Optional - server.rs**: Add `use bytes::BytesMut;` if implementing UDP fix
- [ ] **Performance validation**: Profile before/after to confirm improvement

---

## Rollback Plan

If issues are discovered:

1. **Immediate rollback:** Revert the 4 file changes (vfs.rs signature, vfs.rs impl, handlers.rs caller)
2. **Verify:** Run test suite to ensure rollback is clean
3. **Root cause:** Investigate what failed (likely lifetime or Send issues with Bytes)

The changes are minimal and isolated to the write path, so rollback should be straightforward.

---

## References

- **Bytes crate documentation**: https://docs.rs/bytes/latest/bytes/
- **Arc-based sharing**: `Bytes::clone()` is O(1) because it's Arc-backed
- **Current TCP zero-copy pattern**: See `server.rs:174` for similar usage

---

## Notes

- The read path (`vfs.rs:107-126`) is already efficient - no changes needed
- TCP receive path (`server.rs:174`) already uses zero-copy `split().freeze()` pattern
- XDR decoding (`xdr.rs:161`) already uses efficient `copy_to_bytes()` slicing
- Only the write path and UDP receive have unnecessary copying

---

## Questions / Concerns

If you encounter issues during implementation:

1. **"Bytes doesn't implement Send"** - It does, this should work fine
2. **"Lifetime issues with spawn_blocking"** - `Bytes` is 'static when cloned
3. **"write_at doesn't accept Bytes"** - It accepts `&[u8]`, and `Bytes` derefs to `&[u8]`

All these scenarios should work correctly with the proposed changes.
