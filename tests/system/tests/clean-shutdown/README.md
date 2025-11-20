# Clean Shutdown Test

## Purpose

Verifies that SPDK blobstore properly handles clean shutdown operations with all required patches applied. This test ensures volumes can be remounted quickly without requiring lengthy recovery scans.

## Critical Requirement

**With patches applied**: Blobstore marks itself "clean" on unmount → fast remount (< 15 seconds)  
**Without patches**: Blobstore not marked clean → 3-5 minute recovery on every remount

## Required SPDK Patches

This test validates that all four critical patches are working:

1. **lvol-flush.patch** - Adds FLUSH support to lvol bdev layer
2. **ublk-debug.patch** - Logs FLUSH capability advertisement
3. **blob-shutdown-debug.patch** - Logs blobstore unload operations
4. **blob-recovery-progress.patch** - Logs recovery vs clean mount decisions

See: `spdk-csi-driver/docker/Dockerfile.spdk` for patch application during build.

## Test Flow

```
Step 00: Create PVC
  ├─ PVC bound successfully
  └─ Volume provisioned on node

Step 01: Write Data
  ├─ Pod writes test data + 10MB blob
  ├─ sync() called to flush data
  └─ Pod completes successfully

Step 02: Delete Pod (Clean Shutdown)
  ├─ Pod deletion triggers volume unmount
  ├─ SPDK blobstore unload initiated
  └─ CRITICAL: Blobstore marks itself "clean"

Step 03: Verify Logs
  ├─ Check SPDK logs for "BLOBSTORE UNLOAD"
  └─ Confirm FLUSH support advertised

Step 04: Fast Remount
  ├─ New pod mounts volume
  ├─ CRITICAL: No recovery triggered
  ├─ Verify data integrity
  └─ Complete within 30 seconds

Step 05: Verify No Recovery
  ├─ Check SPDK logs for "Clean blobstore, no recovery needed"
  └─ FAIL if "Recovery required" found

Step 06: Rapid Cycle
  ├─ Third mount/unmount cycle
  ├─ Append additional data
  └─ Verify multiple iterations work
```

## Success Criteria

| Check | Expected Result |
|-------|----------------|
| FLUSH Support | "supports FLUSH - setting UBLK_ATTR_VOLATILE_CACHE" in logs |
| Clean Shutdown | "BLOBSTORE UNLOAD COMPLETE (status: 0)" in logs |
| Fast Remount | Pod ready within 30 seconds (not 3-5 minutes) |
| No Recovery | "Clean blobstore, no recovery needed" in logs |
| Data Integrity | All written data readable after remount |
| Rapid Cycles | Multiple mount/unmount cycles work without recovery |

## Running the Test

### Run this test only

```bash
cd tests/system
kubectl kuttl test --test clean-shutdown
```

### Run with verbose output

```bash
kubectl kuttl test --test clean-shutdown --suppress=
```

### Expected Duration

- **With patches**: ~2-3 minutes total
- **Without patches**: Would timeout (recovery takes 3-5 min per remount)

## Interpreting Results

### ✅ PASS - All patches working correctly

```
Step 03-verify-logs: ✅ Found BLOBSTORE UNLOAD
Step 04-fast-remount: Pod ready in ~10 seconds
Step 05-verify-no-recovery: ✅ Clean blobstore confirmed
Step 06-rapid-cycle: Multiple cycles complete quickly
```

### ❌ FAIL - Patches not applied or not working

**Symptom**: Step 04 times out (> 30 seconds)

```
Step 04-fast-remount: Pod stuck ContainerCreating...
Logs show: "BLOBSTORE RECOVERY STARTING"
           "This may take several minutes..."
```

**Root Cause**: One or more patches not applied during SPDK build

**Fix**: Rebuild SPDK container with patches:
```bash
cd spdk-csi-driver
docker build -f docker/Dockerfile.spdk -t flint-spdk:patched .
# Look for: ✅ FLUSH patch applied to lvol bdev
#           ✅ Blobstore shutdown debug logging patch applied
```

### ⚠️ WARNING - Inconclusive results

**Symptom**: Cannot find log messages

**Possible Causes**:
- Log rotation (logs older than checked window)
- SPDK logging level not set to NOTICE
- Wrong namespace or pod names

**Debug**:
```bash
# Check SPDK pods
kubectl get pods -n kube-system -l app=spdk-node

# Check logs directly
kubectl logs -n kube-system <spdk-pod> | grep -E "FLUSH|BLOBSTORE|recovery"

# Verify patches in startup logs
kubectl logs -n kube-system <spdk-pod> | grep "patch applied"
```

## Common Issues

### Test fails at Step 05 with "Recovery required"

**Problem**: Blobstore was not cleanly unmounted

**Check**:
1. Verify lvol-flush.patch is applied:
   ```bash
   kubectl logs -n kube-system <spdk-pod> | grep "FLUSH patch applied"
   ```

2. Verify FLUSH support is advertised:
   ```bash
   kubectl logs -n kube-system <spdk-pod> | grep "supports FLUSH"
   ```

3. Check if ublk has volatile cache attribute:
   ```bash
   kubectl exec -n kube-system <spdk-pod> -- cat /sys/class/ublk/*/ublk0/params
   # Should show: attrs: 0x1 (UBLK_ATTR_VOLATILE_CACHE)
   ```

### Pod stuck at ContainerCreating during remount

**Problem**: Waiting for recovery to complete

**Timeline**:
- 0-30s: Normal mount time (with patches)
- 3-5 min: Recovery in progress (without patches)
- 5+ min: Recovery stuck or failed

**Check recovery progress**:
```bash
kubectl logs -n kube-system <spdk-pod> -f | grep -i recovery
# Should see progress updates if recovery is running
```

## Integration with CI/CD

Add to your test pipeline:

```yaml
# .github/workflows/test.yml
- name: Run Clean Shutdown Test
  run: |
    cd tests/system
    kubectl kuttl test --test clean-shutdown --timeout 600
```

## Manual Verification

If automated test is inconclusive, verify manually:

```bash
# 1. Create and delete a pod with volume
kubectl apply -f test-pod.yaml
kubectl wait --for=condition=Ready pod/test-pod
kubectl delete pod test-pod

# 2. Check logs immediately after deletion
kubectl logs -n kube-system <spdk-pod> --tail=50 | grep "BLOBSTORE UNLOAD"

# 3. Recreate pod and check mount speed
time kubectl apply -f test-pod.yaml
time kubectl wait --for=condition=Ready pod/test-pod --timeout=30s

# 4. Check for recovery
kubectl logs -n kube-system <spdk-pod> --tail=50 | grep -E "clean flag|recovery"
```

## Related Documentation

- **Architecture**: `/FLINT_CSI_ARCHITECTURE.md` - Data Persistence section
- **Patches**: `/spdk-csi-driver/*.patch`
- **Dockerfile**: `/spdk-csi-driver/docker/Dockerfile.spdk`
- **Test Framework**: `/tests/system/README.md`

## Performance Metrics

Expected timings with patches applied:

| Operation | Target Time | Acceptable Range |
|-----------|-------------|------------------|
| Initial PVC bind | 5-10s | < 30s |
| Data write + sync | 2-5s | < 10s |
| Pod deletion | 5-10s | < 30s |
| Fast remount | 5-15s | < 30s |
| Full test suite | 2-3 min | < 5 min |

Without patches, remount alone would take 3-5 minutes, causing test to fail.

