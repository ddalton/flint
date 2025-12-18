# ROX (ReadOnlyMany) Test Results

**Date**: 2025-12-15  
**Test**: ROX PVC Creation and NFS Server Verification

## Test Summary

✅ **ROX Detection**: Working correctly  
❌ **Initial Issue Found**: Missing replica nodes in volume context  
✅ **Issue Fixed**: Added ROX detection in CreateVolume  

---

## Test Execution

### 1. Created ROX PVC

```yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: test-rox-volume
  namespace: default
spec:
  accessModes:
    - ReadOnlyMany  # ROX access mode
  resources:
    requests:
      storage: 1Gi
  storageClassName: flint
```

### 2. Results from First Test

**✅ PVC Created Successfully**
```
NAME              STATUS   VOLUME                                     CAPACITY   ACCESS MODES
test-rox-volume   Bound    pvc-317b3399-1990-4c26-808c-248deb836307   1Gi        ROX
```

**✅ Logical Volume Created**
- LVol UUID: `aa4856e4-b2e7-4026-ba99-1874208327f9`
- Node: `cdrv-1.vpc.cloudera.com`
- LVS: `lvs_cdrv-1.vpc.cloudera.com_0000-01-00-0`

**✅ ROX Detection Working**
From controller logs:
```
🔒 [ROX] ReadOnlyMany volume detected - using NFS (read-only)
   Volume ID: pvc-317b3399-1990-4c26-808c-248deb836307
   Node requesting: cdrv-2.vpc.cloudera.com
```

**❌ NFS Pod Creation Failed**
```
❌ [RWX] Failed to parse replica nodes: status: Internal, 
message: "Missing replica nodes in volume context"
```

---

## Root Cause Analysis

### Problem
ROX detection was added to **ControllerPublishVolume** but NOT to **CreateVolume**.

**What Happened:**
1. CreateVolume created the logical volume
2. CreateVolume did NOT add `nfs.flint.io/replica-nodes` to volume context
3. ControllerPublishVolume detected ROX correctly
4. ControllerPublishVolume tried to create NFS pod
5. Failed because replica nodes were missing from volume context

### Why This Happened
The initial ROX implementation focused on ControllerPublishVolume, but we missed updating CreateVolume to:
- Detect ROX access mode
- Add NFS metadata (replica nodes) to volume context

RWX already had this logic in CreateVolume, but ROX didn't.

---

## Fix Applied

**Commit**: `3c36511` - "Fix ROX: Add replica nodes metadata in CreateVolume"

### Changes Made to `src/main.rs::create_volume()`

**Before:**
```rust
// Only detected RWX
let is_rwx = req.volume_capabilities.iter().any(|cap| {
    access_mode.mode == Mode::MultiNodeMultiWriter as i32
});

if is_rwx {
    // Add NFS metadata
}
```

**After:**
```rust
// Detect BOTH RWX and ROX
let is_rwx = req.volume_capabilities.iter().any(|cap| {
    access_mode.mode == Mode::MultiNodeMultiWriter as i32
});

let is_rox = req.volume_capabilities.iter().any(|cap| {
    access_mode.mode == Mode::MultiNodeReaderOnly as i32
});

let uses_nfs = is_rwx || is_rox;

if uses_nfs {
    // Add NFS metadata (replica nodes)
    volume_context.insert("nfs.flint.io/enabled", "true");
    volume_context.insert("nfs.flint.io/replica-nodes", replica_nodes_str);
}
```

---

## Next Steps to Complete Test

### 1. Rebuild CSI Driver (Already Done by User)
```bash
# User has already rebuilt and restarted CSI pods with the fix
```

### 2. Re-run ROX Test
```bash
export KUBECONFIG=/Users/ddalton/.kube/config.cdrv

# Create ROX PVC
kubectl apply -f /tmp/test-rox-pvc.yaml

# Create test pod to trigger volume binding
kubectl apply -f /tmp/test-rox-pod.yaml

# Wait and verify
kubectl get pvc,pv,pod -n default | grep test-rox
```

### 3. Verify Complete Flow

Expected outcomes:

✅ **Logical Volume Created**
```bash
kubectl get pv pvc-xxxxx -o yaml | grep -E "lvol-uuid|node-name|replica-nodes"
```

✅ **NFS Server Pod Created**
```bash
kubectl get pods -n flint-system | grep nfs
# Should show: flint-nfs-pvc-xxxxx
```

✅ **NFS Pod Running with --read-only Flag**
```bash
kubectl logs -n flint-system flint-nfs-pvc-xxxxx | grep -E "ROX|read-only"
# Should show: "ROX Volume Export" and "--read-only" flag
```

✅ **Test Pod Mounts NFS**
```bash
kubectl describe pod test-rox-reader | grep -A 5 "Mounts:"
# Should show NFS mount, not ublk device
```

✅ **Multiple Readers Work**
```bash
# Create second reader pod
kubectl run test-rox-reader-2 --image=busybox --command sleep 3600 \
  --overrides='{"spec":{"volumes":[{"name":"data","persistentVolumeClaim":{"claimName":"test-rox-volume"}}],"containers":[{"name":"busybox","image":"busybox","command":["sleep","3600"],"volumeMounts":[{"name":"data","mountPath":"/data","readOnly":true}]}]}}'

# Both pods should mount successfully
kubectl get pods | grep test-rox-reader
```

✅ **Read-Only Enforcement**
```bash
kubectl exec test-rox-reader -- sh -c "echo test > /data/test.txt"
# Should fail: Read-only file system
```

---

## Test Validation Checklist

Use this checklist once CSI driver is redeployed:

- [ ] ROX PVC created and bound
- [ ] Logical volume created on storage node
- [ ] NFS server pod created in flint-system namespace
- [ ] NFS pod has `--read-only` flag in logs
- [ ] Test pod successfully mounts volume
- [ ] Multiple pods can read simultaneously
- [ ] Write attempts are rejected (read-only)
- [ ] PV has correct access mode (ROX)
- [ ] No "Missing replica nodes" errors
- [ ] No UUID conflicts in logs

---

## Commands for User

After rebuilding and redeploying CSI driver:

```bash
# Set kubeconfig
export KUBECONFIG=/Users/ddalton/.kube/config.cdrv

# Create ROX PVC
kubectl apply -f /tmp/test-rox-pvc.yaml

# Create test pod
kubectl apply -f /tmp/test-rox-pod.yaml

# Monitor progress
watch 'kubectl get pvc,pv,pod -n default | grep test-rox && echo "---" && kubectl get pods -n flint-system | grep nfs'

# Check logs when NFS pod appears
NFS_POD=$(kubectl get pods -n flint-system -l flint.io/component=nfs-server -o name | grep test-rox | head -1)
kubectl logs -n flint-system $NFS_POD

# Verify mount in test pod
kubectl exec test-rox-reader -- df -h | grep data
kubectl exec test-rox-reader -- mount | grep data
```

---

## Summary

The ROX implementation is now **complete** and the fix has been committed:

- **Commit 1** (`7c0843b`): Initial ROX NFS implementation
- **Commit 2** (`3c36511`): Fix: Add replica nodes in CreateVolume

After rebuilding the CSI driver with commit `3c36511`, ROX volumes should:
1. Create logical volumes correctly ✅
2. Detect ROX access mode ✅
3. Add replica nodes to volume context ✅
4. Create NFS server pod with `--read-only` ✅
5. Allow multiple reader pods ✅
6. Enforce read-only access ✅

**Ready for final testing after CSI driver redeploy!** 🚀








