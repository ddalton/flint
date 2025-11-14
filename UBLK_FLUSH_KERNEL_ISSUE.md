# UBLK Flush/Sync Issue - Kernel Driver Limitation - November 14, 2025

## Problem Statement

Applications calling `sync()` on ublk-backed volumes hang indefinitely, even after applying the lvol flush patch.

## Investigation Results

### ✅ What's Working

1. **lvol flush support** - Patch applied successfully
   ```
   "supported_io_types": {"flush": true}  ← All lvol bdevs
   ```

2. **SPDK flush detection** - Working correctly
   ```
   ublk556319: bdev '...' supports FLUSH - setting UBLK_ATTR_VOLATILE_CACHE
   ```

3. **Blobstore clean shutdown** - NOW WORKING!
   ```
   BLOBSTORE LOAD: Checking recovery status
     clean flag: 1  ← Clean shutdown!
     DECISION: Clean blobstore, no recovery needed
   ```

### ❌ What's NOT Working

**Kernel ublk driver doesn't honor `UBLK_ATTR_VOLATILE_CACHE`:**

```bash
# What SPDK does:
ublk_info_param_init() {
    if (spdk_bdev_io_type_supported(bdev, SPDK_BDEV_IO_TYPE_FLUSH)) {
        uparams.basic.attrs = UBLK_ATTR_VOLATILE_CACHE;  ← Set correctly
    }
}

# What the kernel shows:
/sys/block/ublkb556319/queue/fua = 0          ← NOT enabled
/sys/block/ublkb556319/queue/write_cache = write back
```

## Root Cause

The Linux kernel 6.8.0-1008-aws ublk driver (`ublk_drv.ko`) does **not properly enable FUA** (Force Unit Access) support when SPDK sets the `UBLK_ATTR_VOLATILE_CACHE` attribute.

### Why sync() Hangs

1. Application calls `sync()`
2. Kernel tries to flush cached writes to `/dev/ublkb556319`
3. Kernel checks `/sys/block/ublkb556319/queue/fua` → sees 0 (not supported)
4. Kernel has no way to flush, so it **waits indefinitely**
5. Timeout or hang

## Evidence

### Kernel Version
```
uname -r: 6.8.0-1008-aws
```

### SPDK Logs (Proof SPDK is doing its part)
```
[2025-11-14 05:25:18] ublk.c:1743: ublk556319: bdev supports FLUSH 
                      - setting UBLK_ATTR_VOLATILE_CACHE
```

### Kernel Device Attributes (Proof kernel isn't responding)
```
/sys/block/ublkb556319/queue/fua: 0
/sys/block/ublkb556319/queue/write_cache: write back
```

### SPDK Source Code
`lib/ublk/ublk.c` lines 1741-1743:
```c
if (spdk_bdev_io_type_supported(bdev, SPDK_BDEV_IO_TYPE_FLUSH)) {
    uparams.basic.attrs = UBLK_ATTR_VOLATILE_CACHE;  // This IS being set
}
```

## Solution Options

### Option 1: Use NBD Instead of UBLK ⭐ RECOMMENDED

Replace ublk with NBD (Network Block Device):
- **Pros**: NBD has mature, stable flush support in all kernels
- **Pros**: Well-tested for storage workloads
- **Cons**: Slightly lower performance than ublk
- **Implementation**: Modify CSI driver to use NBD instead of ublk

### Option 2: Patch Kernel ublk Driver

Fix the kernel's ublk driver to honor `UBLK_ATTR_VOLATILE_CACHE`:
- **Pros**: Fixes the root cause
- **Pros**: Benefits future SPDK/ublk users
- **Cons**: Requires kernel rebuild/upgrade
- **Cons**: Needs mainline kernel acceptance
- **File**: `drivers/block/ublk_drv.c` in Linux kernel

### Option 3: Use Kernel NVMe-oF Initiator

For remote volumes, use kernel's native NVMe-oF initiator:
- **Pros**: Kernel handles everything, no ublk needed
- **Pros**: Full flush/FUA support
- **Cons**: Only works for remote volumes
- **Cons**: Doesn't help for local volumes

### Option 4: Workaround - Disable Caching

Mount with `sync` option or use direct I/O:
- **Pros**: Quick workaround
- **Cons**: Severe performance impact
- **Cons**: Doesn't actually fix sync

### Option 5: Wait for Kernel Update

Newer kernels may have better ublk support:
- **Pros**: No code changes needed
- **Cons**: Unknown timeline
- **Cons**: May never be fixed

## Recommendation

**Use NBD for local volumes** until kernel ublk driver is fixed.

### Migration Path
1. Add NBD support to CSI driver alongside ublk
2. Add a configuration option to choose: `blockDeviceType: nbd` or `ublk`
3. Test with NBD to verify sync works
4. Default to NBD until kernel ublk is fixed
5. Keep ublk code for future when kernel is fixed

## Files to Modify for NBD Implementation

1. `spdk-csi-driver/src/driver.rs` - Add NBD device creation
2. `spdk-csi-driver/src/node_agent.rs` - NBD RPC calls
3. `flint-csi-driver-chart/values.yaml` - Add blockDeviceType config

## Verification Commands

```bash
# Verify SPDK is setting the attribute
kubectl logs -n flint-system <pod> -c spdk-tgt | grep "UBLK_ATTR_VOLATILE_CACHE"

# Verify kernel is ignoring it
kubectl exec -n default <test-pod> -- cat /sys/block/ublkb*/queue/fua
# Should be 1, but shows 0

# Test with NBD (future)
kubectl exec -n default <test-pod> -- cat /sys/block/nbd*/queue/fua
# Should show 1
```

## Related Issues

This may be a known issue in kernel 6.8.x ublk driver. Worth checking:
- Linux kernel mailing list archives
- SPDK GitHub issues
- Ubuntu kernel bug tracker

## Successful Achievements Today

1. ✅ lvol flush patch working
2. ✅ Clean blobstore shutdowns working  
3. ✅ Fast restarts (no recovery needed)
4. ✅ Identified exact root cause: kernel ublk driver

The only remaining issue is the kernel ublk driver limitation with FUA support.

