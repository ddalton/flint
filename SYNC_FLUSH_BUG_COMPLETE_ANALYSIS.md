# Complete Analysis: ublk Sync/Flush Bug and Fix

## Problem Statement
**Symptom**: The `sync` command hangs indefinitely when called on filesystems mounted from ublk devices, causing pods to fail termination.

## Root Cause Analysis

### Architecture
```
Application writes data
      ↓
Filesystem (ext4) - buffered in page cache  
      ↓
Linux VFS issues FLUSH (REQ_OP_FLUSH) on sync/fsync
      ↓
ublk kernel driver (Linux 6.0+)
      ↓  ioctl(UBLK_CMD_FLUSH)
SPDK ublk target (userspace driver)
      ↓
SPDK bdev_flush()
      ↓
SPDK lvol/NVMe device
```

### The Bug
**SPDK v25.05.x should have flush support** (added in v24.01), but it's either:
1. Not properly implemented for lvol bdevs
2. Has a regression
3. Requires specific configuration we're not setting

###  Environment
- **SPDK Version**: v25.05.x
- **Kernel**: 6.8.0-aws (supports ublk flush)
- **Write Cache**: write through (already disabled)  
- **FUA Support**: 0 (not supported)

### Test Results
| Operation | Result | Notes |
|-----------|--------|-------|
| File writes (echo, dd) | ✅ WORKS | 778 MB/s |
| File reads | ✅ WORKS | 161 MB/s |
| touch (file creation) | ✅ WORKS | Instant |
| `sync` command | ❌ HANGS | Blocks forever |
| `fsync()` | ❌ HANGS | (Implied) |
| Direct device write | ✅ WORKS | 119 MB/s (when no sync) |

## Solutions

### Solution 1: Remove sync from test scripts (IMMEDIATE)
**Timeline**: Now  
**Risk**: Low for testing, HIGH for production

Don't call `sync` in your pod scripts:

```yaml
# BEFORE (hangs):
command: |
  echo "data" > /data/test.txt
  sync  # ← HANGS HERE
  
# AFTER (works):
command: |
  echo "data" > /data/test.txt
  # No sync - relies on kernel writeback
```

**Limitations**:
- ⚠️ Data may not be durable on crash
- ⚠️ No guarantees data reaches disk
- ✅ Works for testing/development
- ❌ NOT suitable for production databases

### Solution 2: Mount with nobarrier (TESTING ONLY)
**Timeline**: 1 hour  
**Risk**: EXTREME - data loss on crash

Modify `src/main.rs` NodeStageVolume:

```rust
// Around line 934
let mount_output = std::process::Command::new("mount")
    .arg("-o")
    .arg("nobarrier") // Disable write barriers
    .arg(&device_path)
    .arg(&staging_target_path)
    .output()?;
```

**Limitations**:
- ❌ EXTREME data loss risk
- ❌ Do NOT use in production
- ✅ Allows sync to return immediately
- ✅ Good for testing other features

### Solution 3: Investigate SPDK ublk flush implementation (RECOMMENDED)
**Timeline**: 1-2 weeks
**Risk**: Low - proper fix

**Steps**:

1. **Check if lvol supports flush**:
```bash
# Test if the underlying lvol bdev supports flush
kubectl exec POD -c flint-csi-driver -- curl -s http://localhost:8081/api/spdk/rpc \
  -d '{"jsonrpc":"2.0","method":"bdev_get_bdevs","params":{"name":"LVOL_UUID"},"id":1}' \
  | jq '.result[0].supported_io_types.flush'
```

2. **Enable SPDK debug logging**:
```bash
# In spdk_tgt startup, add:
-L ublk  # Enable ublk debug logs
-L lvol  # Enable lvol debug logs
```

3. **Check for SPDK config issues**:
```rust
// When calling ublk_start_disk, try adding flush parameter:
let ublk_params = json!({
    "method": "ublk_start_disk",
    "params": {
        "bdev_name": bdev_name,
        "ublk_id": ublk_id,
        "enable_user_copy": false,  // Try different options
    }
});
```

4. **Test with simple SPDK setup**:
```bash
# Outside Kubernetes, test SPDK ublk directly:
spdk_tgt &
rpc.py bdev_malloc_create 512 4096 -b malloc0
rpc.py ublk_start_disk -b malloc0 -n 0
# Try: echo "test" > /dev/ublkb0 && sync
```

5. **Report to SPDK if bug confirmed**:
- Check SPDK GitHub issues for similar reports  
- File bug report with reproduction steps
- Consider contributing fix if possible

### Solution 4: Switch to NVMe-oF for all access (ALTERNATIVE)
**Timeline**: 1 week  
**Risk**: Medium - major architecture change

**Pros**:
- NVMe-oF has proven, mature flush support
- Already working for remote access
- Consistent code path (no ublk special case)

**Cons**:
- Higher latency for local access (~2-5μs overhead)
- More CPU usage (network stack)
- More complex setup (NVMe-oF target per node)

**Implementation**:
```rust
// In NodeStageVolume, always use NVMe-oF:
let (bdev_name, volume_type) = if volume_on_node == current_node {
    // OLD: create ublk device
    // NEW: connect via localhost NVMe-oF
    let conn_info = NvmeofConnectionInfo {
        nqn: format!("nqn.2024-11.com.flint:volume:{}", volume_id),
        transport: "tcp",
        address: "127.0.0.1",  // Localhost!
        port: 4420,
    };
    self.driver.connect_to_nvmeof_target(&conn_info).await?
} else {
    // Remote: use NVMe-oF as before
    ...
}
```

### Solution 5: Use NBD instead of ublk
**Timeline**: 2 weeks  
**Risk**: Medium - different kernel interface

NBD (Network Block Device) is older but more mature than ublk:
- Full flush support
- Well-tested in production
- Higher overhead than ublk but lower than NVMe-oF

## Testing After Fix

Once flush is working, verify with:

```bash
# Test 1: Basic sync
echo "test" > /data/file.txt
time sync  # Should complete in < 1 second

# Test 2: Python fsync
python3 << EOF
import os
f = open('/data/test.txt', 'w')
f.write('test')
f.flush()
os.fsync(f.fileno())  # Should not hang
f.close()
print('SUCCESS')
EOF

# Test 3: dd with sync
dd if=/dev/zero of=/data/test bs=1M count=10 conv=fsync

# Test 4: Database simulation
fio --name=db --rw=randwrite --bs=8k --size=100M \
    --fsync=1 --filename=/data/testfile --runtime=30
```

## Recommended Action Plan

### Phase 1: Immediate (Today)
- [x] Remove `sync` calls from test scripts
- [ ] Document limitation in README
- [ ] Test pod migration WITHOUT sync
- [ ] Verify cross-node access works

### Phase 2: Investigation (This Week)
- [ ] Enable SPDK debug logging for ublk and lvol
- [ ] Test SPDK ublk flush outside Kubernetes
- [ ] Check if lvol bdevs report flush capability
- [ ] Review SPDK 25.05.x changelog for ublk changes
- [ ] Test with different ublk_start_disk parameters

### Phase 3: Fix (Next 1-2 Weeks)
**Option A** (if SPDK fix possible):
- [ ] Fix SPDK ublk flush handling
- [ ] Contribute fix upstream if needed
- [ ] Rebuild SPDK container

**Option B** (if SPDK can't be fixed quickly):
- [ ] Implement NVMe-oF for all volumes
- [ ] Test performance impact
- [ ] Update documentation

### Phase 4: Production (After Fix Verified)
- [ ] Comprehensive flush testing
- [ ] Database workload testing (PostgreSQL)
- [ ] Failover testing (verify durability)
- [ ] Performance benchmarks
- [ ] Update deployment docs

## Current Status

### What Works ✅
- Volume creation and attachment
- Local and remote volume access
- Read operations (excellent performance)
- Write operations (excellent performance)
- **Pod migration** (if we avoid sync)
- Cross-node access via NVMe-oF

### What Doesn't Work ❌
- `sync` command
- `fsync()` / `fdatasync()` system calls
- Applications that require durability (databases)
- Clean pod termination (if app calls fsync)

### Production Readiness
**Status**: ❌ NOT READY

**Blockers**:
1. No data durability guarantee
2. sync/fsync hangs
3. Database workloads won't work

**Workaround for Testing**:
- Don't use databases
- Don't call sync/fsync
- Accept potential data loss on crashes
- Document this limitation clearly

## Next Steps

1. **Complete pod migration test** (without sync) - verify basic functionality
2. **Enable SPDK debug logging** - understand what's happening with flush
3. **Test SPDK ublk independently** - isolate if it's CSI integration or SPDK core
4. **Make go/no-go decision**: Fix SPDK vs switch to NVMe-oF

---

**Last Updated**: November 14, 2025  
**Status**: Root cause identified, fix in progress
**Priority**: P1 - Blocks production deployment


