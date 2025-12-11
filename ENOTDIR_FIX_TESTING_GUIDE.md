# Testing the ENOTDIR Fix

**Date:** December 11, 2024  
**Issue Fixed:** LOOKUP operations not validating filesystem paths  
**Files Changed:** 
- `src/nfs/v4/operations/fileops.rs`
- `src/nfs/v4/filehandle.rs`

---

## Quick Start

### 1. Rebuild the Server

```bash
cd /Users/ddalton/projects/rust/flint/spdk-csi-driver
cargo build --release
```

Expected output: `Finished 'release' profile [optimized] target(s)`

### 2. Prepare Test Directory

```bash
# Create export directory with test content
mkdir -p target/nfs-test-export
echo "Hello from Flint NFS!" > target/nfs-test-export/test.txt
mkdir target/nfs-test-export/subdir
echo "File in subdir" > target/nfs-test-export/subdir/nested.txt
```

### 3. Start the NFS Server

```bash
# Stop any existing server
sudo pkill -f nfs-server

# Start fresh server
sudo ./target/release/nfs-server \
    --export-path target/nfs-test-export \
    --bind-addr 127.0.0.1 \
    --bind-port 2049
```

Expected output:
```
🚀 Starting NFSv4.2 server on 127.0.0.1:2049
📂 Exporting: /full/path/to/target/nfs-test-export
```

### 4. Mount from Client

```bash
# Create mount point
sudo mkdir -p /mnt/nfs-test

# Clear any stale mounts
sudo umount -f /mnt/nfs-test 2>/dev/null

# Mount with NFSv4.2
sudo mount -t nfs -o vers=4.2,tcp 127.0.0.1:/ /mnt/nfs-test
```

**Expected:** Mount succeeds without errors!

### 5. Verify Basic Operations

```bash
# List files
ls -la /mnt/nfs-test
# Expected: test.txt, subdir/

# Read file
cat /mnt/nfs-test/test.txt
# Expected: "Hello from Flint NFS!"

# Navigate subdirectory
ls /mnt/nfs-test/subdir/
# Expected: nested.txt

# Read nested file
cat /mnt/nfs-test/subdir/nested.txt
# Expected: "File in subdir"

# Write new file
echo "New content" | sudo tee /mnt/nfs-test/newfile.txt

# Read back
cat /mnt/nfs-test/newfile.txt
# Expected: "New content"
```

---

## What Changed?

### Before the Fix:

```
Client: LOOKUP "nonexistent"
Server: OK (without checking) ❌
Client: GETATTR on invalid filehandle
Server: Returns zeros/errors
Client: ENOTDIR!
```

### After the Fix:

```
Client: LOOKUP "nonexistent"
Server: Checking filesystem... doesn't exist
Server: NFS4ERR_NOENT ✅
Client: OK, handle error appropriately

Client: LOOKUP "subdir"
Server: Checking filesystem... exists and is directory
Server: OK + valid filehandle ✅
Client: Proceed with operations
```

---

## Testing Scenarios

### Scenario 1: Valid Paths (Should Work)

```bash
# These should all succeed
ls /mnt/nfs-test
ls /mnt/nfs-test/subdir
cat /mnt/nfs-test/test.txt
```

### Scenario 2: Invalid Paths (Should Fail Gracefully)

```bash
# These should return "No such file or directory"
ls /mnt/nfs-test/nonexistent
cat /mnt/nfs-test/missing.txt

# NOT "Input/output error" or ENOTDIR
```

### Scenario 3: Directory Operations

```bash
# Create directory
sudo mkdir /mnt/nfs-test/newdir

# Navigate into it
cd /mnt/nfs-test/newdir

# Create file in new directory
echo "test" | sudo tee ./file.txt

# List
ls -la
```

### Scenario 4: Export Boundary (Security)

```bash
# Try to escape export (should be prevented)
# This will fail at the client level, but server should handle gracefully
ls /mnt/nfs-test/../../etc
# Expected: Permission denied or "No such file"
```

---

## Debugging Mount Failures

If mount still fails, check:

### 1. Server Logs

Look for:
```
PUTROOTFH
GETATTR: Requested attributes: [...]
LOOKUP: component=...
```

Should NOT see:
```
LOOKUP: Path ... does not exist
PUTROOTFH failed
```

### 2. Client Kernel Logs

```bash
# Enable debug logging
sudo rpcdebug -m nfs -s all
sudo rpcdebug -m rpc -s all

# Check for errors
sudo dmesg | tail -50
```

Should see:
```
decode_getfattr_attrs: xdr returned 0  ✅
NFS: nfs_fhget(...)  ✅
```

Should NOT see:
```
decode_getfattr_attrs: xdr returned 5  ❌
nfs4_try_get_tree() = -20  ❌
```

### 3. Verify Export Path

```bash
# Check export exists and is accessible
ls -la /full/path/to/target/nfs-test-export

# Check permissions
stat /full/path/to/target/nfs-test-export
```

---

## Expected Results

### ✅ Success Indicators:

1. Mount command completes without error
2. `ls /mnt/nfs-test` shows files
3. `cat /mnt/nfs-test/test.txt` shows content
4. Write operations succeed
5. Directory navigation works
6. No ENOTDIR errors

### ❌ Failure Indicators:

1. Mount hangs
2. "mount.nfs: mount system call failed"
3. ENOTDIR errors in dmesg
4. "Input/output error" for valid files

---

## Performance Test

Once basic operations work:

```bash
# Write test
dd if=/dev/zero of=/mnt/nfs-test/testfile bs=1M count=100

# Read test
dd if=/mnt/nfs-test/testfile of=/dev/null bs=1M

# Many small files
mkdir /mnt/nfs-test/perftest
for i in {1..100}; do
    echo "File $i" > /mnt/nfs-test/perftest/file$i.txt
done

# List performance
time ls /mnt/nfs-test/perftest
```

---

## Comparison with Working Server

If you have NFS Ganesha installed:

```bash
# Start Ganesha
sudo systemctl start nfs-ganesha

# Mount
sudo mount -t nfs -o vers=4.2,tcp 127.0.0.1:/export /mnt/ganesha

# Compare behavior
ls /mnt/ganesha
ls /mnt/nfs-test

# Both should behave identically
```

---

## Rollback (If Needed)

If the fix causes issues:

```bash
# Revert changes
cd /Users/ddalton/projects/rust/flint
git checkout src/nfs/v4/operations/fileops.rs
git checkout src/nfs/v4/filehandle.rs

# Rebuild
cd spdk-csi-driver
cargo build --release
```

---

## Success Criteria

✅ **Mount succeeds**  
✅ **All file operations work**  
✅ **No ENOTDIR errors**  
✅ **Invalid paths return NOENT (not EIO)**  
✅ **Matches Ganesha behavior**

---

**Fix Ready:** December 11, 2024  
**Build Status:** ✅ PASS  
**Testing:** Ready

**Go test and celebrate! 🎉**

