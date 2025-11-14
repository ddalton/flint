# Kernel Version Analysis for UBLK Flush Support - November 14, 2025

## Current Situation

**Cluster Kernels**: `6.8.0-1008-aws` (both ublk-1 and ublk-2)

**Problem**: SPDK sets `UBLK_ATTR_VOLATILE_CACHE` correctly, but kernel doesn't enable FUA on the block device.

## Relevant Kernel Commits

### 1. Cache Control Refactoring (v6.11+)
- **Commit**: `1122c0c1cc71f740fa4d5f14f239194e06a1d5e7`
- **Date**: June 19, 2024
- **Author**: Christoph Hellwig
- **Title**: "block: move cache control settings out of queue->flags"
- **Changes**:
  ```
  Move the cache control settings into the queue_limits so that the flags
  can be set atomically with the device queue frozen.
  
  The FLUSH and FUA flags are now inherited by blk_stack_limits, which
  simplified the code in dm a lot...
  ```
- **First appeared in**: **Linux 6.11** (released ~September 2024)

### 2. Earlier UBLK Flush Work
- **Commit**: `23ef8220f287abe5bf741ddfc278e7359742d3b1`
- **Date**: April 2, 2023
- **Title**: "block: ublk_drv: don't consider flush request in map/unmap io"
- **First appeared in**: **Linux 6.3**

### 3. PREFLUSH Cleanup
- **Commit**: `5f8bcc837a9640ba4bf5e7b1d7f9b254ea029f47`
- **Date**: July 21, 2022
- **Title**: "ublk: remove UBLK_IO_F_PREFLUSH"
- **First appeared in**: **Linux 6.0**

## Timeline

| Kernel | Release Date | UBLK Flush Status |
|--------|--------------|-------------------|
| 6.0 | Oct 2022 | Initial ublk driver, basic flush |
| 6.3 | Apr 2023 | Flush handling improvements |
| **6.8** | **Mar 2024** | **← WE ARE HERE** |
| **6.11** | **Sep 2024** | **Cache control refactoring** ⭐ |
| 6.12 | Nov 2024 | Current mainline |

## Gap Analysis

**Our kernel (6.8.0-1008-aws)** is missing:
- The queue_limits cache control refactoring (6.11+)
- Potentially other ublk improvements in 6.9, 6.10

**AWS Ubuntu Kernel**:
- 6.8.0-1008-aws appears to be from AWS's 6.8 kernel series
- Likely based on upstream 6.8.x LTS
- May be missing critical ublk fixes from 6.11+

## Recommendations

### Option 1: Upgrade to Kernel 6.11+ ⭐ BEST LONG-TERM
Check if AWS has newer kernel packages:
```bash
apt-cache search linux-image-aws | grep 6.11
apt-cache search linux-image-aws | grep 6.12
```

If available:
```bash
# On each node
sudo apt update
sudo apt install linux-image-6.11-aws linux-headers-6.11-aws
sudo reboot
```

### Option 2: Use HWE Kernel (Hardware Enablement)
Ubuntu often provides newer kernels via HWE:
```bash
apt-cache search linux-image-.*-hwe | sort
# Install if 6.11+ is available
```

### Option 3: Use NBD Instead ⭐ IMMEDIATE WORKAROUND
While waiting for kernel upgrade:
- Implement NBD support in CSI driver
- NBD has mature flush/FUA support in all kernels
- Can migrate back to ublk after kernel upgrade

### Option 4: Backport the Fix
Manually apply the 6.11 cache control changes to 6.8:
- **Pros**: Exact fix for the problem
- **Cons**: Requires kernel compilation, module signing
- **Complexity**: HIGH

## Verification After Kernel Upgrade

Once on kernel 6.11+:

```bash
# Create test volume
kubectl apply -f sync-test.yaml

# Check FUA support
kubectl exec -n default sync-test -- \
  cat /sys/block/ublkb*/queue/fua
# Should show: 1 (enabled)

# Test sync
kubectl exec -n default sync-test -- sync
# Should complete in milliseconds
```

## Current Workaround Status

Until kernel upgrade:
1. ✅ lvol flush patch applied (SPDK side working)
2. ✅ Clean blobstore shutdowns (no recovery needed)
3. ❌ Sync still hangs (kernel limitation)
4. **Solution**: Use NBD for local volumes

## AWS Kernel Update Path

Check AWS documentation for:
- Ubuntu 24.04 LTS with kernel 6.11+
- Custom AMI builds
- EKS optimized AMIs with newer kernels

Alternatively, consider:
- Ubuntu mainline kernel PPA (not recommended for production)
- Canonical Livepatch (may include ublk fixes)

## References

- Linux kernel git: https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git
- UBLK driver: `drivers/block/ublk_drv.c`
- Cache control commit: `1122c0c1cc71f740fa4d5f14f239194e06a1d5e7`

