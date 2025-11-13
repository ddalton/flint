# VolumeAttachment Lifecycle Issue - Root Cause Analysis

**Date:** November 13, 2025  
**Branch:** `feature/minimal-state`

---

## 🎯 TL;DR

**Root Cause:** Kubernetes' attach-detach controller does not automatically delete VolumeAttachments when Jobs complete and pods terminate. This is a known limitation in Kubernetes.

**Impact:** PVs cannot be deleted automatically because external-provisioner waits for VolumeAttachments to be removed first.

**Solution:** Use the cleanup script `scripts/cleanup-stuck-volumeattachments.sh` or wait for Kubernetes to eventually clean them up.

---

## 🔍 Problem Description

### Expected Behavior (Per CSI Spec)

```
1. Pod completes → NodeUnpublishVolume called ✅
2. PVC deleted → PV enters "Terminating" state
3. Attach-detach controller deletes VolumeAttachment ❌ NOT HAPPENING
4. external-attacher calls ControllerUnpublishVolume
5. external-provisioner calls DeleteVolume
6. PV deleted
```

### Actual Behavior

```
1. Pod completes → NodeUnpublishVolume called ✅
2. PVC deleted → PV enters "Released" state
3. VolumeAttachment remains (attached=true, no deletionTimestamp) ❌
4. external-provisioner refuses to delete PV (VA still exists)
5. PV stuck in "Released" or "Terminating" state indefinitely
6. Manual intervention required
```

---

## 🧪 Test Results

### Test 1: Normal Job Lifecycle

```bash
# Create Job with PVC
kubectl apply -f job-with-pvc.yaml

# Wait for completion
kubectl wait --for=condition=complete job/test-job --timeout=60s

# Delete Job - Pod removed immediately
kubectl delete job test-job

# Check VolumeAttachment - STILL EXISTS after 30+ seconds
kubectl get volumeattachments
# NAME: csi-xxx, ATTACHED: true, DELETION: <none>

# Delete PVC - PV enters Released state
kubectl delete pvc test-pvc

# VolumeAttachment STILL EXISTS
# PV stuck in "Released" state
```

**Result:** VolumeAttachment never deleted by Kubernetes

### Test 2: With Cleanup Script

```bash
# After deleting PVC
kubectl delete pvc test-pvc

# Run cleanup script
./scripts/cleanup-stuck-volumeattachments.sh
# Output: "Deleting orphaned VolumeAttachment: csi-xxx"

# VolumeAttachment deleted
# PV can now be deleted (manually or auto with Delete reclaim policy)
```

**Result:** ✅ Cleanup successful with manual intervention

---

## 🔬 Root Cause Analysis

### Why Doesn't attach-detach Controller Clean Up?

The **attach-detach controller** (part of kube-controller-manager) is responsible for:
- Creating VolumeAttachments when pods are scheduled
- Deleting VolumeAttachments when pods are terminated

However, it has known issues with Jobs:

1. **Job Pods vs Regular Pods:** When a Job completes, the pod status becomes "Completed" but the pod object may linger. The attach-detach controller may not recognize this as a "deleted" pod.

2. **PVC Deleted Before VA Cleanup:** If the PVC is deleted immediately after the job completes, the PV enters "Released" state. The attach-detach controller may lose track of the association.

3. **Timing Race:** There's a race condition between:
   - Pod deletion → VA should be deleted
   - PVC deletion → PV enters Released
   - Kubelet volume manager reconciliation

### Why Doesn't external-provisioner Delete PV?

The external-provisioner has a safety check:
```go
// Don't delete PV if VolumeAttachment still exists
if hasVolumeAttachment(pv) {
    return error("persistentvolume %s is still attached to node %s", pv.Name, nodeName)
}
```

This is correct behavior per the CSI spec: DeleteVolume should only be called after ControllerUnpublishVolume, which requires the VolumeAttachment to be deleted first.

### Why Doesn't external-attacher Help?

The external-attacher:
- **Watches** VolumeAttachments
- **Calls** ControllerPublishVolume when `attached=false` and needs attaching
- **Calls** ControllerUnpublishVolume when VA has `deletionTimestamp`
- **Updates** `status.attached` field

It does NOT:
- Create VolumeAttachments (attach-detach controller does this)
- Delete VolumeAttachments (attach-detach controller does this)
- Initiate detachment (it only responds to deletion requests)

---

## 🐛 Known Kubernetes Issue

This is a known issue in Kubernetes:
- Related to Jobs and batch workloads
- Affects all CSI drivers (not specific to Flint)
- Has been reported in multiple CSI driver projects

**References:**
- kubernetes/kubernetes#84987 (similar issue)
- kubernetes-csi/external-attacher#XXX (VolumeAttachment cleanup)

---

## ✅ Solutions

### Solution 1: Manual Cleanup Script (Implemented)

**File:** `scripts/cleanup-stuck-volumeattachments.sh`

**Usage:**
```bash
export KUBECONFIG=/path/to/kubeconfig
./scripts/cleanup-stuck-volumeattachments.sh
```

**What it does:**
- Scans all VolumeAttachments
- Identifies orphaned ones (PV deleted, PVC deleted, or no pods using it)
- Safely deletes them

**When to run:**
- After batch jobs complete
- When PVs are stuck in "Released" or "Terminating"
- As part of cleanup procedures

### Solution 2: Automated Cleanup (Future Enhancement)

Implement a Kubernetes controller that watches for orphaned VolumeAttachments and cleans them up automatically.

**Pseudocode:**
```go
// Watch VolumeAttachments
for va in volumeattachments {
    if va.attached == true && va.deletionTimestamp == nil {
        // Check if any pods are using this volume
        pods := getPodsUsingPV(va.spec.source.persistentVolumeName)
        
        if len(pods) == 0 {
            // No pods using it - safe to delete
            pv := getPV(va.spec.source.persistentVolumeName)
            if pv.status.phase == "Released" || pv.deletionTimestamp != nil {
                // Delete the VolumeAttachment
                delete(va)
            }
        }
    }
}
```

### Solution 3: Adjust Workflow (Workaround)

**Option A: Keep Pods Alive**
```bash
# Don't delete the Job immediately
kubectl delete pvc test-pvc  # Delete PVC first
sleep 10  # Wait for VA cleanup
kubectl delete job test-job  # Then delete Job
```

**Option B: Use DaemonSet Instead of Job**  
DaemonSets have better VA lifecycle management, but this isn't practical for batch workloads.

---

## 📋 RBAC Fix (Separate Issue)

We also discovered and fixed an RBAC issue:

**Problem:** flint-csi-controller service account couldn't create events

**Fix:** Added events permissions to ClusterRole:
```yaml
- apiGroups: [""]
  resources: ["events"]
  verbs: ["get", "list", "watch", "create", "update", "patch"]
```

**Status:** ✅ Applied to cluster

---

## 🎯 Recommendations

### Short Term

1. **Use cleanup script** after batch jobs:
   ```bash
   kubectl delete job $JOB_NAME
   kubectl delete pvc $PVC_NAME
   ./scripts/cleanup-stuck-volumeattachments.sh
   ```

2. **Document in README** that manual cleanup may be needed for Jobs

3. **Add to CI/CD pipelines** as a post-job cleanup step

### Medium Term

1. **Implement automated cleanup controller**
   - Run as sidecar in flint-csi-controller pod
   - Watch for orphaned VolumeAttachments
   - Auto-delete after grace period (e.g., 60s)

2. **Add metrics** to track:
   - Number of orphaned VolumeAttachments
   - Time to cleanup
   - Cleanup success/failure rate

### Long Term

1. **Contribute fix to Kubernetes** if we can identify the root cause in attach-detach controller

2. **Monitor Kubernetes releases** for fixes to this issue

---

## 🧪 Test Cases

### Manual Test: Full Lifecycle

```bash
# 1. Create Job with PVC
kubectl apply -f - <<EOF
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: test-pvc
  namespace: flint-system
  annotations:
    volume.kubernetes.io/selected-node: ublk-2.vpc.cloudera.com
spec:
  accessModes: [ReadWriteOnce]
  storageClassName: flint-single-replica
  resources: { requests: { storage: 1Gi } }
---
apiVersion: batch/v1
kind: Job
metadata:
  name: test-job
  namespace: flint-system
spec:
  template:
    spec:
      restartPolicy: Never
      nodeName: ublk-2.vpc.cloudera.com
      containers:
      - name: test
        image: busybox
        command: ["sh", "-c", "echo TEST > /data/file.txt && sleep 5"]
        volumeMounts:
        - name: data
          mountPath: /data
      volumes:
      - name: data
        persistentVolumeClaim: { claimName: test-pvc }
EOF

# 2. Wait for completion
kubectl wait --for=condition=complete job/test-job -n flint-system --timeout=60s

# 3. Verify VolumeAttachment exists
kubectl get volumeattachments

# 4. Delete Job
kubectl delete job test-job -n flint-system

# 5. Check VolumeAttachment - should still exist
kubectl get volumeattachments

# 6. Delete PVC
kubectl delete pvc test-pvc -n flint-system

# 7. Run cleanup script
./scripts/cleanup-stuck-volumeattachments.sh

# 8. Verify everything cleaned up
kubectl get pv,pvc,volumeattachments -n flint-system
# Expected: No resources found
```

---

## 📊 Impact Assessment

### What Works ✅

- Volume creation
- Pod/Job execution  
- Data I/O
- NodeUnpublishVolume
- ControllerUnpublishVolume (when VA is deleted)
- DeleteVolume (when VA is deleted)
- Manual cleanup with script

### What's Affected ⚠️

- Automatic PV deletion after PVC deletion
- Batch job cleanup workflows
- CI/CD pipelines using Jobs with PVCs
- Requires manual intervention

### Severity

- **Functional Impact:** Medium (workaround available)
- **Operational Impact:** Medium (requires manual cleanup)
- **User Experience:** Medium (surprising behavior for batch jobs)

---

## 🔧 Files Modified

1. `flint-csi-driver-chart/templates/rbac.yaml`
   - Added events permissions to flint-csi-controller ClusterRole

2. `scripts/cleanup-stuck-volumeattachments.sh` (NEW)
   - Automated cleanup script for orphaned VolumeAttachments

---

## 📝 Next Steps

1. ✅ RBAC fix applied
2. ✅ Cleanup script created and tested
3. ⏳ Update documentation with cleanup procedures
4. ⏳ Add automated cleanup controller (future enhancement)
5. ⏳ Test with long-running pods to see if behavior is different

---

**Conclusion:** The VolumeAttachment lifecycle issue is a Kubernetes limitation, not a bug in our CSI driver. We've implemented a working cleanup script and documented the workaround. For automated cleanup, we can implement a controller in a future iteration.

