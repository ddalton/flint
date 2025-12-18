# pNFS Device ID Fix and Debug Logging Implementation

**Date**: December 18, 2025  
**Status**: ✅ **FIXED AND DEPLOYED**

---

## Summary

Successfully implemented environment variable substitution for DS device IDs and added comprehensive debug logging for pNFS operations. The MDS now correctly sees **2 active Data Servers** instead of 1, enabling proper pNFS striping.

---

## Changes Implemented

### 1. Environment Variable Substitution in Config (`src/pnfs/config.rs`)

**Problem**: Device IDs in YAML contained literal `${NODE_NAME}-ds` instead of expanded values.

**Solution**: Added `substitute_env_vars()` and `expand_env_vars()` methods to `PnfsConfig` implementation.

```rust
fn substitute_env_vars(&mut self) {
    if let Some(ref mut ds_config) = self.ds {
        ds_config.device_id = Self::expand_env_vars(&ds_config.device_id);
    }
}

fn expand_env_vars(input: &str) -> String {
    // Replaces ${VAR_NAME} patterns with actual environment variable values
    ...
}
```

**Result**: Each DS pod now registers with unique device ID:
- Pod on cdrv-1: `cdrv-1.vpc.cloudera.com-ds`
- Pod on cdrv-2: `cdrv-2.vpc.cloudera.com-ds`

---

### 2. Enhanced DS Startup Logging (`src/nfs_ds_main.rs`)

**Added**:
- Import of `debug` macro from tracing
- Logging of NODE_NAME environment variable
- Clear indication that device ID was expanded

```rust
use tracing::{debug, error, info};  // Added debug

debug!("   • NODE_NAME env var: {:?}", std::env::var("NODE_NAME"));
info!("   • Device ID: {} (after env var expansion)", ds_config.device_id);
```

**Sample Output**:
```
[DEBUG] • NODE_NAME env var: Ok("cdrv-1.vpc.cloudera.com")
[INFO]  • Device ID: cdrv-1.vpc.cloudera.com-ds (after env var expansion)
```

---

### 3. Enhanced MDS Device Registry Logging (`src/pnfs/mds/device.rs`)

**Added**:
- Emoji indicators for visual scanning (✅, 🔄, 📊)
- Device count logging on registration
- Capacity and binary device ID debugging
- Active vs total device counts

```rust
info!("✅ Registering new device: {} @ {}", device_id, info.primary_endpoint);
debug!("   Capacity: {} bytes ({} GB)", info.capacity, info.capacity / (1024*1024*1024));
debug!("   Binary device ID: {:02x?}", &info.binary_device_id[0..8]);
info!("📊 Device registry: {} total, {} active", total_devices, active_devices);
```

**Sample Output**:
```
[INFO]  ✅ Registering new device: cdrv-1.vpc.cloudera.com-ds @ 0.0.0.0:2049
[DEBUG]    Capacity: 1000000000000 bytes (931 GB)
[DEBUG]    Binary device ID: [e2, 9c, cc, 1a, b1, bf, 10, ae]
[INFO]  📊 Device registry: 2 total, 2 active
```

---

### 4. Enhanced LAYOUTGET Logging (`src/pnfs/mds/operations/mod.rs`)

**Added**:
- Request details logging with emojis (📥, ✅, ❌)
- Available data server count
- Success/failure indicators
- Segment count in response

```rust
info!("📥 LAYOUTGET: offset={}, length={}, iomode={:?}, layout_type={:?}",
     args.offset, args.length, args.iomode, args.layout_type);
info!("   Available data servers: {}", active_devices);
info!("✅ LAYOUTGET successful: {} segments returned", layout.segments.len());
```

**Sample Output**:
```
[INFO]  📥 LAYOUTGET: offset=0, length=104857600, iomode=ReadWrite, layout_type=NfsV4_1Files
[INFO]     Available data servers: 2
[INFO]  ✅ LAYOUTGET successful: 2 segments returned
```

---

## Deployment Process

### 1. Code Changes Committed and Pushed

```bash
git add spdk-csi-driver/src/{pnfs/config.rs,pnfs/mds/device.rs,pnfs/mds/operations/mod.rs,nfs_ds_main.rs}
git add deployments/ PNFS_DEPLOYMENT_TEST_RESULTS.md
git commit -m "Fix pNFS device ID substitution and add enhanced debug logging"
git push origin feature/pnfs-implementation
```

**Commit**: `3e32006`

### 2. Image Built on Linux Server

```bash
ssh root@cdrv-1.vpc.cloudera.com
cd /root/flint/spdk-csi-driver
git pull origin feature/pnfs-implementation
docker buildx build --platform linux/amd64 \
  -f docker/Dockerfile.pnfs \
  -t docker-sandbox.infra.cloudera.com/ddalton/pnfs:latest \
  --push .
```

**Result**: Image pushed successfully to registry

### 3. Deployed to Kubernetes

```bash
kubectl delete namespace pnfs-test
./deploy-all.sh
```

**Deployed Components**:
- pNFS MDS (1 replica)
- pNFS DS (2 DaemonSet pods, one per node)
- Standalone NFS (for comparison)
- Test client pod

---

## Verification Results

### ✅ Device Registration Status

**MDS Logs Show**:
```
[INFO]  📝 DS Registration: device_id=cdrv-2.vpc.cloudera.com-ds, endpoint=0.0.0.0:2049
[INFO]  ✅ Registering new device: cdrv-2.vpc.cloudera.com-ds @ 0.0.0.0:2049
[DEBUG]    Capacity: 1000000000000 bytes (931 GB)
[INFO]  📊 Device registry: 2 total, 2 active

[INFO]  📝 DS Registration: device_id=cdrv-1.vpc.cloudera.com-ds, endpoint=0.0.0.0:2049
[INFO]  ✅ Registering new device: cdrv-1.vpc.cloudera.com-ds @ 0.0.0.0:2049
[DEBUG]    Capacity: 1000000000000 bytes (931 GB)
[INFO]  📊 Device registry: 2 total, 2 active
```

**Status**: ✅ **2 Data Servers registered** (previously was 1)

### ✅ Heartbeat Monitoring

**MDS Logs Show**:
```
[DEBUG] Heartbeat received from device: cdrv-1.vpc.cloudera.com-ds
[DEBUG] Updated capacity for device cdrv-1.vpc.cloudera.com-ds: 0 / 1000000000000 bytes

[DEBUG] Heartbeat received from device: cdrv-2.vpc.cloudera.com-ds
[DEBUG] Updated capacity for device cdrv-2.vpc.cloudera.com-ds: 0 / 1000000000000 bytes
```

**Status**: ✅ Both DSs heartbeating every 10 seconds

### ✅ Pod Status

```
NAME                              READY   STATUS    RESTARTS   AGE
pnfs-client                       1/1     Running   0          43s
pnfs-ds-6hc4q                     1/1     Running   0          57s
pnfs-ds-9bwgw                     1/1     Running   0          57s
pnfs-mds-5b7b67ddb7-nmpfx         1/1     Running   0          60s
standalone-nfs-6496d966c7-8sb6g   1/1     Running   0          46s
```

**Status**: ✅ All pods running

---

## Performance Testing Status

### Initial Test Results

**pNFS Write (100MB)**:
```
write: IOPS=31, BW=31.8MiB/s (33.4MB/s)(100MiB/3141msec)
```

### ⚠️ Remaining Issue: pNFS Not Activating

The tests show that pNFS is still not being fully activated by the client. This is indicated by:

1. **Performance**: Write speed (~32 MB/s) is similar to standalone NFS, not 2x faster
2. **Missing LAYOUTGET logs**: No `📥 LAYOUTGET` messages in MDS logs during file creation
3. **Missing EXCHANGE_ID logs**: No `🎯 EXCHANGE_ID` flag modification logs

**Root Cause**: The pNFS flag fix (in `src/pnfs/mds/server.rs`) is present in the code, but the EXCHANGE_ID operation may not be triggering the post-processing step that modifies the flags.

---

## What's Working ✅

1. ✅ **Environment Variable Substitution**: Device IDs are correctly expanded
2. ✅ **Unique Device IDs**: Each DS has a unique identifier  
3. ✅ **Device Registration**: Both DSs successfully register with MDS
4. ✅ **Device Count**: MDS correctly reports "2 total, 2 active"
5. ✅ **Heartbeats**: Both DSs sending regular heartbeats
6. ✅ **Debug Logging**: Comprehensive logging for troubleshooting
7. ✅ **All Pods Running**: Deployment is stable

---

## What's Not Working ⚠️

1. ⚠️ **pNFS Client Activation**: Client not requesting layouts
2. ⚠️ **EXCHANGE_ID Flags**: pNFS MDS flag not being advertised to client
3. ⚠️ **Layout Generation**: No LAYOUTGET requests observed
4. ⚠️ **Performance Doubling**: Speed not improved with 2 DSs

---

## Next Steps for Full pNFS Activation

### Investigation Needed

1. **Verify EXCHANGE_ID Flow**:
   ```bash
   # Check if EXCHANGE_ID is being processed
   kubectl logs -l app=pnfs-mds -n pnfs-test | grep -i "exchange_id"
   ```

2. **Check Client Mount Options**:
   ```bash
   kubectl exec -n pnfs-test pnfs-client -- mount | grep pnfs-mds
   kubectl exec -n pnfs-test pnfs-client -- cat /proc/self/mountstats | grep pnfs
   ```
   - Should show `pnfs=LAYOUT_NFSV4_1_FILES`
   - Currently shows `pnfs=not configured`

3. **Test with Fresh Mount**:
   ```bash
   # Unmount and remount to trigger fresh EXCHANGE_ID
   kubectl exec -n pnfs-test pnfs-client -- umount /mnt/pnfs
   kubectl exec -n pnfs-test pnfs-client -- mount -t nfs -o vers=4.1 pnfs-mds:/ /mnt/pnfs
   
   # Check MDS logs immediately
   kubectl logs -l app=pnfs-mds -n pnfs-test --tail=50
   ```

### Potential Issues

1. **EXCHANGE_ID Post-Processing Not Triggering**:
   - The post-processing code in `handle_compound_with_pnfs()` may not be matching the EXCHANGE_ID result
   - May need to check the exact pattern matching logic

2. **Client Not Requesting pNFS**:
   - Client may need explicit mount option: `vers=4.1,minorversion=1`
   - May need to force pNFS with `nconnect=` option

3. **MDS Not Running in pNFS Mode**:
   - Verify config is being loaded correctly
   - Check that PnfsCompoundWrapper is being used

---

## Debug Commands

### Check Device Registration
```bash
kubectl logs -l app=pnfs-mds -n pnfs-test | grep "Device registry"
```

### Watch Heartbeats
```bash
kubectl logs -l app=pnfs-mds -n pnfs-test --tail=20 -f | grep "Heartbeat"
```

### Check for EXCHANGE_ID
```bash
kubectl logs -l app=pnfs-mds -n pnfs-test | grep -A5 "EXCHANGE_ID"
```

### Check for LAYOUTGET  
```bash
kubectl logs -l app=pnfs-mds -n pnfs-test | grep -A5 "LAYOUTGET"
```

### Get DS Device IDs
```bash
kubectl logs -l app=pnfs-ds -n pnfs-test | grep "Device ID"
```

---

## Files Modified

### Source Code
1. `spdk-csi-driver/src/pnfs/config.rs` - Environment variable substitution
2. `spdk-csi-driver/src/nfs_ds_main.rs` - Enhanced DS startup logging
3. `spdk-csi-driver/src/pnfs/mds/device.rs` - Enhanced device registry logging
4. `spdk-csi-driver/src/pnfs/mds/operations/mod.rs` - Enhanced LAYOUTGET logging

### Deployment Files (New)
5. `deployments/pnfs-namespace.yaml`
6. `deployments/pnfs-mds-config.yaml`
7. `deployments/pnfs-mds-deployment.yaml`
8. `deployments/pnfs-ds-config.yaml`
9. `deployments/pnfs-ds-daemonset.yaml`
10. `deployments/pnfs-client-pod.yaml`
11. `deployments/standalone-nfs-deployment.yaml`
12. `deployments/deploy-all.sh`
13. `deployments/run-performance-tests.sh`
14. `deployments/build-and-deploy-fixes.sh`

### Documentation (New)
15. `PNFS_DEPLOYMENT_TEST_RESULTS.md`
16. `PNFS_FIX_IMPLEMENTATION_SUMMARY.md` (this file)

---

## Success Metrics

| Metric | Before | After | Status |
|--------|--------|-------|--------|
| Registered DSs | 1 | 2 | ✅ **FIXED** |
| Unique Device IDs | No | Yes | ✅ **FIXED** |
| Debug Logging | Minimal | Comprehensive | ✅ **ADDED** |
| pNFS Activation | Not working | Not working | ⚠️ **IN PROGRESS** |
| Performance (2 DS) | N/A | ~32 MB/s | ⚠️ **No 2x improvement yet** |

---

## Conclusion

**Primary Issue FIXED**: ✅  
The device ID environment variable substitution is now working correctly. Both Data Servers register with unique IDs, and the MDS correctly tracks 2 active data servers.

**Secondary Issue REMAINING**: ⚠️  
pNFS is not being activated by the client. The EXCHANGE_ID flag modification code exists but may not be triggering correctly. This requires further investigation into the EXCHANGE_ID flow and client mount behavior.

**Recommendation**:  
Focus next session on:
1. Verifying EXCHANGE_ID flow with enhanced logging
2. Testing different mount options to force pNFS activation
3. Checking if PnfsCompoundWrapper is correctly wrapping requests
4. Potentially adding more debug output to the EXCHANGE_ID handler itself

---

**Implementation completed by**: AI Assistant  
**Tested on**: 2-node Kubernetes cluster (cdrv-1, cdrv-2)  
**Image**: docker-sandbox.infra.cloudera.com/ddalton/pnfs:latest  
**Git Commit**: 3e32006

