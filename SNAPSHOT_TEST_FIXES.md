# Snapshot Test Fixes - Build Checklist

## Required Commits (All on `uring` branch)

The following commits MUST all be included in the image build:

### 1. Snapshot Restore Metadata Fix
**Commit**: `35017e9`  
**File**: `spdk-csi-driver/src/main.rs`  
**Fix**: Populate volume_context in CreateVolumeResponse when creating from snapshot  
**Lines**: 391-428  

### 2. ublk Device Cache Clearing
**Commit**: `0bbd379`  
**File**: `spdk-csi-driver/src/main.rs`  
**Fix**: Always run wipefs before blkid to clear stale ublk device signatures  
**Lines**: 1087-1127  
**Prevents**: "Bad message" I/O errors, "wrong superblock" mount failures  

### 3. LVS Name Extraction Fix
**Commit**: `34821a5`  
**File**: `spdk-csi-driver/src/snapshot/snapshot_service.rs`  
**Fix**: Extract lvs_name from lvol alias instead of non-existent field  
**Lines**: 134-147, 309-328  
**Adds**: Comprehensive logging for debugging  

### 4. Helm Template Fixes
**Commits**: `65ef3ed`, `d3f976e`  
**Files**: `flint-csi-driver-chart/templates/volumesnapshotclass.yaml`  
**Fix**: Correct template helper names  

## Build Command

```bash
cd /Users/ddalton/projects/rust/flint/spdk-csi-driver
git pull origin uring  # Ensure you have all commits
cargo build --release --bin csi-driver
# Build and push Docker image
```

## Verify Image Contains Fixes

After building, you can verify the fixes are present:

```bash
# Check the binary was built after all commits
ls -l target/release/csi-driver

# Verify git commit in build
git log -1 --oneline
# Should show: 34821a5 Fix snapshot service lvs_name extraction and add logging
```

## Deployment

After building and pushing the image:

```bash
# Restart CSI pods
kubectl delete pods -n flint-system -l app=flint-csi-controller
kubectl delete pods -n flint-system -l app=flint-csi-node

# Wait for ready
kubectl wait --for=condition=ready pod -l app=flint-csi-node -n flint-system --timeout=120s
kubectl wait --for=condition=ready pod -l app=flint-csi-controller -n flint-system --timeout=120s

# Clean up test namespaces
kubectl get ns | grep kuttl-test | awk '{print $1}' | xargs kubectl delete ns

# Run snapshot test
cd tests/system
KUBECONFIG=/path/to/kubeconfig kubectl kuttl test --test snapshot-restore tests-standard --timeout 600
```

## Expected Behavior After Fixes

### Source Volume (initial-data-writer)
- ✅ wipefs clears ublk cache
- ✅ blkid finds no filesystem (new volume)
- ✅ mkfs.ext4 formats device
- ✅ mount succeeds
- ✅ Data written successfully (no "Bad message" errors)

### Snapshot Creation
- ✅ Snapshot created from source lvol
- ✅ Filesystem and data preserved in snapshot

### Restored Volume (from snapshot)
- ✅ Clone created with lvs_name populated
- ✅ Volume metadata stored in PV
- ✅ AttachVolume succeeds (metadata present)
- ✅ wipefs clears ublk cache
- ✅ blkid detects ext4 filesystem FROM CLONE
- ✅ Filesystem preserved (no reformat!)
- ✅ Mount succeeds
- ✅ Data from snapshot is present

## Logs to Verify

### Controller Logs
```
✅ [CONTROLLER] Volume pvc-xxx created from snapshot (clone UUID: xxx, lvs: lvs_ublk-1...)
📝 [CONTROLLER] Storing snapshot-restored volume metadata in PV: node=xxx, lvol=xxx
```

### Node Agent Logs  
```
🧹 [NODE] Clearing ublk device cache (prevents stale signature detection)
✅ [NODE] ublk device cache clean
🔍 [NODE] Now checking REAL filesystem state from lvol (cache cleared)
📁 [NODE] Device /dev/ublkbXXX already has filesystem: TYPE="ext4"
✅ [NODE] Preserving existing filesystem (data persistence)
```

### Snapshot Service Logs
```
🔍 [SNAPSHOT_SERVICE] Extracting LVS name from lvol JSON
🔍 [SNAPSHOT_SERVICE] Found alias: lvs_ublk-1.vpc.cloudera.com_0000-00-1d-0/vol_pvc-xxx
✅ [SNAPSHOT_SERVICE] Extracted LVS name from alias: lvs_ublk-1.vpc.cloudera.com_0000-00-1d-0
📋 [SNAPSHOT_SERVICE] Clone details: lvs_name=Some("lvs_ublk-1..."), size=1073741824 bytes
```

## Troubleshooting

If snapshot test still fails after applying all fixes:

1. **Verify image version**: `kubectl get pods -n flint-system -o jsonpath='{.items[0].spec.containers[0].image}'`
2. **Check pod restart times**: Pods should be newer than commit push time
3. **Verify wipefs present**: `kubectl exec -n flint-system <pod> -c flint-csi-driver -- wipefs --version`
4. **Check logs**: Look for the expected log messages above
5. **Clean everything**: Delete all test namespaces and orphaned snapshots before retrying

