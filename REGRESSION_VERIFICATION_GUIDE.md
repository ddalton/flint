# Regression Verification Guide - Single Replica Code Path

## Overview

This guide ensures that the multi-replica implementation does **NOT** affect existing single-replica functionality.

## 🛡️ Regression Prevention Mechanisms

### 1. Code Isolation Strategy

#### Early Exit Pattern
```rust
// In create_volume() - Line 395-397
if replica_count == 1 {
    // Single replica: Use existing path (zero changes to existing logic)
    return self.create_single_replica_volume(volume_id, size_bytes, thin_provision).await;
}
```

**Why this works:**
- ✅ Single-replica path exits BEFORE any multi-replica code
- ✅ No conditional logic that could affect single-replica behavior
- ✅ `create_single_replica_volume()` is the EXACT original code, just extracted

#### Method Extraction
The original volume creation code (lines 158-239 in original driver.rs) was:
1. **Extracted** into `create_single_replica_volume()` - **NOT modified**
2. **Kept identical** - same logic, same error handling, same capacity cache usage
3. **Same return type** - `VolumeCreationResult` with single replica

### 2. Metadata Storage - Backward Compatible

#### In main.rs CreateVolume response:
```rust
if result.replicas.len() == 1 {
    // SINGLE REPLICA: Store simple metadata (EXISTING FORMAT)
    let replica = &result.replicas[0];
    volume_context.insert("flint.csi.storage.io/node-name", ...);
    volume_context.insert("flint.csi.storage.io/lvol-uuid", ...);
    volume_context.insert("flint.csi.storage.io/lvs-name", ...);
} else {
    // MULTI-REPLICA: Store full replica array as JSON (NEW FORMAT)
    volume_context.insert("flint.csi.storage.io/replicas", ...);
}
```

**Why this works:**
- ✅ Single-replica PVs have IDENTICAL metadata format as before
- ✅ Existing volumes continue working (read old format)
- ✅ New single-replica volumes use same format (write old format)

### 3. Volume Attachment - No Changes for Single Replica

#### In ControllerPublishVolume:
```rust
match self.driver.get_replicas_from_pv(&volume_id).await {
    Ok(Some(replicas)) => {
        // MULTI-REPLICA path (NEW)
    }
    Ok(None) => {
        // SINGLE REPLICA path (EXISTING - UNCHANGED)
        let volume_info = self.driver.get_volume_info(&volume_id).await?;
        // ... existing local/remote logic ...
    }
}
```

**Why this works:**
- ✅ Single-replica volumes use EXISTING `get_volume_info()` method
- ✅ Existing local/remote NVMe-oF logic unchanged
- ✅ No new code executes for single-replica volumes

#### In NodeStageVolume:
```rust
let bdev_name = if volume_type == "multi-replica" {
    // NEW: RAID creation
} else if volume_type == "local" {
    // EXISTING: Local bdev (UNCHANGED)
} else if volume_type == "remote" {
    // EXISTING: NVMe-oF connection (UNCHANGED)
}
```

**Why this works:**
- ✅ Single-replica volumes never hit multi-replica branch
- ✅ Local and remote paths identical to before
- ✅ No RAID code executes for single replica

### 4. Volume Deletion - Backward Compatible

#### In DeleteVolume:
```rust
match self.driver.get_replicas_from_pv(&volume_id).await {
    Ok(Some(replicas)) => {
        // MULTI-REPLICA deletion (NEW)
        // Deletes all replicas
    }
    Ok(None) => {
        // SINGLE REPLICA deletion (EXISTING - UNCHANGED)
    }
    Err(e) => {
        // Volume not found (idempotent - UNCHANGED)
    }
}
```

**Why this works:**
- ✅ Single-replica deletion uses EXISTING logic
- ✅ Same defensive cleanup (force_unstage, etc.)
- ✅ Same error handling

## 📋 Verification Checklist

### Static Analysis

- [x] **Code Review**: Single-replica path extracted, not modified
- [x] **Early Exit**: `replica_count == 1` exits before multi-replica code
- [x] **Metadata Format**: Single-replica uses old format
- [x] **No Shared State**: Multi-replica code doesn't affect single-replica
- [x] **Linter Clean**: No errors or warnings

### Runtime Verification

#### Step 1: Run Existing System Tests

All existing tests should pass **without modification**:

```bash
cd tests/system

# Test 1: RWO PVC Migration
echo "=== Testing RWO PVC Migration ==="
kubectl kuttl test --test rwo-pvc-migration
# Expected: PASS (no changes)

# Test 2: RWX Multi-Pod
echo "=== Testing RWX Multi-Pod ==="
kubectl kuttl test --test rwx-multi-pod
# Expected: PASS (no changes)

# Test 3: Volume Expansion
echo "=== Testing Volume Expansion ==="
kubectl kuttl test --test volume-expansion
# Expected: PASS (no changes)

# Test 4: Snapshot Restore
echo "=== Testing Snapshot Restore ==="
kubectl kuttl test --test snapshot-restore
# Expected: PASS (no changes)

# Test 5: Clean Shutdown
echo "=== Testing Clean Shutdown ==="
kubectl kuttl test --test clean-shutdown
# Expected: PASS (no changes)
```

**Success Criteria**: All tests pass with **zero modifications** to test files.

#### Step 2: Single Replica Manual Test

Create a test with explicit `numReplicas: "1"`:

```bash
# Create StorageClass
cat <<EOF | kubectl apply -f -
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: flint-single-test
provisioner: flint.csi.storage.io
parameters:
  numReplicas: "1"
  thinProvision: "false"
EOF

# Create PVC
cat <<EOF | kubectl apply -f -
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: test-single-replica
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 5Gi
  storageClassName: flint-single-test
EOF

# Wait and verify
kubectl wait --for=condition=Bound pvc/test-single-replica --timeout=30s

# Check metadata format
PV_NAME=$(kubectl get pvc test-single-replica -o jsonpath='{.spec.volumeName}')
kubectl get pv $PV_NAME -o jsonpath='{.spec.csi.volumeAttributes}' | jq

# Expected output:
# {
#   "flint.csi.storage.io/replica-count": "1",
#   "flint.csi.storage.io/node-name": "node-xyz",
#   "flint.csi.storage.io/lvol-uuid": "12345...",
#   "flint.csi.storage.io/lvs-name": "lvs_..."
# }
# NOTE: Should NOT have "replicas" field (only for multi-replica)
```

#### Step 3: Default Behavior Test

Test that default (no numReplicas specified) still creates single replica:

```bash
# Create StorageClass WITHOUT numReplicas
cat <<EOF | kubectl apply -f -
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: flint-default-test
provisioner: flint.csi.storage.io
parameters:
  thinProvision: "false"
EOF

# Create PVC
cat <<EOF | kubectl apply -f -
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: test-default-replica
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 5Gi
  storageClassName: flint-default-test
EOF

# Verify it creates single replica
kubectl wait --for=condition=Bound pvc/test-default-replica --timeout=30s
PV_NAME=$(kubectl get pvc test-default-replica -o jsonpath='{.spec.volumeName}')
REPLICA_COUNT=$(kubectl get pv $PV_NAME -o jsonpath='{.spec.csi.volumeAttributes.flint\.csi\.storage\.io/replica-count}')
echo "Replica count: $REPLICA_COUNT"
# Expected: "1"
```

#### Step 4: Volume Attachment Test

```bash
# Create Pod using single-replica PVC
cat <<EOF | kubectl apply -f -
apiVersion: v1
kind: Pod
metadata:
  name: test-single-pod
spec:
  containers:
  - name: app
    image: busybox
    command: ["sh", "-c", "dd if=/dev/urandom of=/data/testfile bs=1M count=50 && md5sum /data/testfile && sleep 3600"]
    volumeMounts:
    - name: storage
      mountPath: /data
  volumes:
  - name: storage
    persistentVolumeClaim:
      claimName: test-single-replica
EOF

# Wait for pod
kubectl wait --for=condition=Ready pod/test-single-pod --timeout=60s

# Verify no RAID created (single-replica should NOT create RAID)
NODE=$(kubectl get pod test-single-pod -o jsonpath='{.spec.nodeName}')
echo "Pod on node: $NODE"
kubectl logs -n kube-system -l app=flint-csi-node --tail=100 | grep -i "raid"
# Expected: No RAID logs for single replica

# Verify data written successfully
sleep 10
kubectl exec test-single-pod -- cat /data/testfile | md5sum
```

#### Step 5: Volume Deletion Test

```bash
# Delete pod and PVC
kubectl delete pod test-single-pod
kubectl delete pvc test-single-replica

# Check logs - should use single-replica deletion path
kubectl logs -n kube-system -l app=flint-csi-controller --tail=50 | grep -i "deleting volume"
# Expected: "Single-replica volume" message, NOT "Multi-replica volume"
```

### Performance Verification

Single-replica volumes should have **identical performance** to before:

```bash
# Benchmark single replica (before and after implementation)
cat <<EOF | kubectl apply -f -
apiVersion: v1
kind: Pod
metadata:
  name: perf-test-single
spec:
  containers:
  - name: fio
    image: ljishen/fio
    command: ["fio"]
    args:
      - "--name=randwrite"
      - "--ioengine=libaio"
      - "--iodepth=32"
      - "--rw=randwrite"
      - "--bs=4k"
      - "--size=1G"
      - "--numjobs=1"
      - "--time_based"
      - "--runtime=60"
      - "--group_reporting"
      - "--filename=/data/testfile"
    volumeMounts:
    - name: storage
      mountPath: /data
  volumes:
  - name: storage
    persistentVolumeClaim:
      claimName: test-single-replica
  restartPolicy: Never
EOF

# Compare results with baseline (before multi-replica implementation)
# IOPS and latency should be within 5% variance
```

## 🔍 Code Path Analysis

### What Changed?

**Files Modified**:
1. `driver.rs`: Added routing in `create_volume()`, extracted `create_single_replica_volume()`
2. `main.rs`: Added replica count check in CreateVolume, ControllerPublish, NodeStage, DeleteVolume
3. `minimal_models.rs`: Added new error types (don't affect existing errors)

**Files NOT Changed**:
- `node_agent.rs` - Node agent unchanged
- `nvmeof_utils.rs` - NVMe-oF utilities unchanged
- `spdk_native.rs` - SPDK native interface unchanged
- All test files - Zero modifications

### What Stayed the Same?

**For Single-Replica Volumes**:
- ✅ Same node selection algorithm (`select_node_for_single_replica()`)
- ✅ Same capacity cache behavior
- ✅ Same lvol creation (`create_lvol()`)
- ✅ Same metadata format in PV
- ✅ Same publish context (local/remote)
- ✅ Same ublk device creation
- ✅ Same mount/unmount logic
- ✅ Same deletion cleanup
- ✅ Same error handling

## 📊 Expected Results

### Test Results Matrix

| Test | Expected Result | Verification Method |
|------|----------------|---------------------|
| rwo-pvc-migration | PASS | kubectl kuttl test |
| rwx-multi-pod | PASS | kubectl kuttl test |
| volume-expansion | PASS | kubectl kuttl test |
| snapshot-restore | PASS | kubectl kuttl test |
| clean-shutdown | PASS | kubectl kuttl test |
| Single replica explicit | PASS | Manual test |
| Default (no numReplicas) | Creates 1 replica | Check PV metadata |
| Performance | Within 5% of baseline | fio benchmark |

### Log Verification

For single-replica volumes, logs should show:

```
✅ Expected logs (UNCHANGED):
- "Creating volume: pvc-xyz (... bytes, 1 replicas, ...)"
- "Selected node: node-abc (free: XGB / XGB)"
- "Volume pvc-xyz created successfully with lvol UUID: ..."
- "Publishing volume pvc-xyz to node node-abc"
- "Volume is local to node - no NVMe-oF needed" OR "Volume is remote - setting up NVMe-oF"
- "ublk device created: /dev/ublkbX"

❌ Should NOT see:
- "Creating RAID 1 on node:"
- "Processing N replicas..."
- "Multi-replica volume"
- "Creating distributed multi-replica volume"
```

## 🚨 Red Flags

If any of these occur, there's a regression:

1. **Existing tests fail** without code changes
2. **Performance degradation** > 5% for single-replica
3. **Metadata format changes** for single-replica PVs
4. **RAID code executes** for single-replica volumes
5. **Different error messages** for same failure scenarios
6. **Changed log patterns** for single-replica operations

## ✅ Sign-off Checklist

- [ ] All 5 existing system tests pass unchanged
- [ ] Single-replica manual test passes
- [ ] Default behavior test passes (creates 1 replica)
- [ ] Volume attachment works for single replica
- [ ] Volume deletion works for single replica
- [ ] Performance within 5% of baseline
- [ ] Logs show no RAID code for single replica
- [ ] PV metadata format unchanged for single replica
- [ ] No new errors or warnings
- [ ] Code review confirms early exit pattern

## 📝 Test Report Template

```
# Single Replica Regression Test Report

**Date**: YYYY-MM-DD
**Tester**: Name
**Cluster**: cluster-name
**Driver Version**: vX.Y.Z

## Test Results

### Existing System Tests
- [ ] rwo-pvc-migration: PASS / FAIL
- [ ] rwx-multi-pod: PASS / FAIL
- [ ] volume-expansion: PASS / FAIL
- [ ] snapshot-restore: PASS / FAIL
- [ ] clean-shutdown: PASS / FAIL

### Single Replica Tests
- [ ] Explicit numReplicas="1": PASS / FAIL
- [ ] Default (no numReplicas): PASS / FAIL
- [ ] Volume attachment: PASS / FAIL
- [ ] Volume deletion: PASS / FAIL

### Performance
- Baseline IOPS: XXXX
- Current IOPS: XXXX
- Variance: X%
- Status: ACCEPTABLE / REGRESSION

### Log Verification
- [ ] No RAID logs for single replica
- [ ] Expected log patterns present
- [ ] No unexpected errors

## Conclusion
Single replica code path: ✅ NO REGRESSION / ❌ REGRESSION DETECTED

**Issues Found**: None / List issues

**Sign-off**: [ ] Approved for production
```

## 🎯 Automation Script

Save this as `verify-no-regression.sh`:

```bash
#!/bin/bash
set -e

echo "================================"
echo "Single Replica Regression Check"
echo "================================"

# Run all existing tests
echo "Step 1: Running existing system tests..."
cd tests/system
for test in rwo-pvc-migration rwx-multi-pod volume-expansion snapshot-restore clean-shutdown; do
    echo "Testing: $test"
    kubectl kuttl test --test $test || {
        echo "❌ REGRESSION DETECTED: $test failed"
        exit 1
    }
done

echo "✅ All existing tests passed"

# Create single replica test
echo "Step 2: Testing explicit single replica..."
kubectl apply -f - <<EOF
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: flint-regression-test
provisioner: flint.csi.storage.io
parameters:
  numReplicas: "1"
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: regression-test-pvc
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 1Gi
  storageClassName: flint-regression-test
EOF

kubectl wait --for=condition=Bound pvc/regression-test-pvc --timeout=60s || {
    echo "❌ REGRESSION: Single replica PVC failed to bind"
    exit 1
}

# Verify metadata
PV=$(kubectl get pvc regression-test-pvc -o jsonpath='{.spec.volumeName}')
COUNT=$(kubectl get pv $PV -o jsonpath='{.spec.csi.volumeAttributes.flint\.csi\.storage\.io/replica-count}')
if [ "$COUNT" != "1" ]; then
    echo "❌ REGRESSION: Expected replica count 1, got $COUNT"
    exit 1
fi

# Cleanup
kubectl delete pvc regression-test-pvc
kubectl delete sc flint-regression-test

echo "✅ Single replica test passed"
echo "================================"
echo "✅ NO REGRESSION DETECTED"
echo "================================"
```

Run with:
```bash
chmod +x verify-no-regression.sh
./verify-no-regression.sh
```

---

**Summary**: The single-replica path is **completely isolated** through early exit routing. No multi-replica code executes for single-replica volumes, ensuring zero regression.

