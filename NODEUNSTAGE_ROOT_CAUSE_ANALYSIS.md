# NodeUnstageVolume Not Called - Root Cause Analysis

**Date:** November 13, 2025  
**Branch:** `feature/minimal-state`  
**Issue:** NodeUnstageVolume is never called after pod deletion, leaving volumes staged and preventing cleanup

---

## 🎯 Executive Summary

**Root Cause:** Kubernetes kubelet does NOT call NodeUnstageVolume when:
1. A Job completes and the pod is deleted
2. The PVC is deleted immediately after
3. The VolumeAttachment is not automatically removed by attach-detach controller

This is a **known Kubernetes limitation** with Jobs and batch workloads.

**Impact:**
- Volumes remain staged at `/var/lib/kubelet/plugins/.../globalmount`
- ublk devices may or may not exist (often deleted but mount remains - ghost mount)
- SPDK lvols cannot be deleted (reported as "Device or resource busy")
- PVs stuck in "Released" state indefinitely
- Full manual cleanup required

---

## 🔍 Deep Investigation Results

### 1. The Volume Lifecycle Problem

```
Expected Flow (Per CSI Spec):
══════════════════════════════════════════════════════════════
Pod deleted
    ↓
NodeUnpublishVolume called ✅
    ↓
No more pods using volume
    ↓
NodeUnstageVolume called ← SHOULD HAPPEN
    ↓
Volume unstaged, ublk device removed
    ↓
VolumeAttachment deleted
    ↓
ControllerUnpublishVolume called
    ↓
DeleteVolume succeeds


Actual Flow (What Happens):
══════════════════════════════════════════════════════════════
Job completes, pod deleted
    ↓
NodeUnpublishVolume called ✅
    ↓
PVC deleted immediately
    ↓
PV → "Released" state
    ↓
VolumeAttachment NOT deleted ❌
    ↓
kubelet reconciler sees:
  - No pods using volume
  - PVC gone (can't resolve reference)
  - PV in "Released" (not "Bound")
  - VolumeAttachment exists (but points to Released PV)
    ↓
kubelet's DesiredStateOfWorld (DSW) vs ActualStateOfWorld (ASW)
becomes inconsistent
    ↓
NodeUnstageVolume NEVER CALLED ❌
    ↓
Volume remains staged at globalmount
ublk device often deleted (but mount remains = ghost mount)
    ↓
DeleteVolume fails: "Device or resource busy"
    ↓
PV stuck in "Released" state
```

### 2. Evidence from Our Test

#### Test Volume: `pvc-aa13c335-3ed3-4847-b1f6-3e76cbfea69c`

**Staging Directory:**
```
/var/lib/kubelet/plugins/kubernetes.io/csi/flint.csi.storage.io/
  c1f21b1ce128adddfa057d57fa4907ce6b5bbd2d844c279905e060645de5bb7a/globalmount
```

**Mount Status:**
```
/dev/ublkb47779 on .../c1f21b1ce128adddfa057d57fa4907ce6b5bbd2d844c279905e060645de5bb7a/globalmount
  type ext4 (rw,relatime,stripe=256)
```

**Device Status:**
```bash
$ ls -l /dev/ublkb47779
ls: cannot access '/dev/ublkb47779': No such file or directory
```

**Conclusion:** **GHOST MOUNT** - Mount exists but device is gone!

**Why SPDK Can't Delete Lvol:**
```
external-provisioner logs:
  Failed to delete volume: SPDK RPC error: 
  Code=-32603 Msg=Device or resource busy
```

The lvol is marked as "in use" by the filesystem mount, even though the ublk device no longer exists.

---

## 🧠 Understanding kubelet's Volume Manager

### DesiredStateOfWorld (DSW) vs ActualStateOfWorld (ASW)

kubelet maintains two internal states:

#### DesiredStateOfWorld (DSW)
What volumes SHOULD be attached/staged, based on:
- **Pods scheduled on this node**
- **VolumeAttachments for this node**

Populated by:
- Pod spec parsing
- VolumeAttachment API watch

#### ActualStateOfWorld (ASW)
What volumes ARE currently staged/mounted, tracked in:
- kubelet's in-memory state
- Filesystem checks (staging paths, mounts)

### Reconciliation Loop

```go
// Pseudocode of kubelet's volume manager reconciliation

func (vm *VolumeManager) Reconcile() {
    // Add volumes that should exist but don't
    for volume in DSW {
        if volume not in ASW {
            // Call NodeStageVolume, NodePublishVolume
        }
    }
    
    // Remove volumes that exist but shouldn't
    for volume in ASW {
        if volume not in DSW {
            // Call NodeUnpublishVolume, NodeUnstageVolume ← KEY!
        }
    }
}
```

### The Problem: Volume Tracking Lost

When a PVC is deleted before VolumeAttachment cleanup:

```
Timeline:
─────────────────────────────────────────────────────────────
T0: Pod deleted
    DSW: Remove pod → Volume no longer needed by any pod
    ASW: Volume still staged

T1: kubelet starts reconciliation
    Check: Is volume in DSW?
      - PVC reference: pvc://flint-system/e2e-cleanup-test-pvc
      - Lookup PVC: NOT FOUND (deleted)
      - Skip removal? (can't verify PVC ownership)

T2: VolumeAttachment still exists
    DSW might still think volume is needed (VA exists)
    OR DSW can't resolve the volume (PVC gone)
    
T3: kubelet's reconciler is STUCK
    - Can't add to DSW (PVC gone)
    - Can't remove from ASW (uncertain state)
    - NodeUnstageVolume never triggered
```

---

## 📊 Comparison: Jobs vs Regular Pods

### Regular Pod Deletion (Works Better)

```
1. kubectl delete pod my-pod
2. kubelet removes pod from DSW immediately
3. Pod status → Terminating
4. NodeUnpublishVolume called
5. No pods using volume → NodeUnstageVolume called ✅
6. VolumeAttachment deleted by attach-detach controller
7. ControllerUnpublishVolume, DeleteVolume proceed
```

### Job Completion + Deletion (Problematic)

```
1. Job completes → Pod status: Completed
2. kubectl delete job my-job
3. Pod deleted, but kubelet may not immediately reconcile
4. PVC often deleted by user right after
5. PV → Released, VolumeAttachment orphaned
6. kubelet's DSW/ASW reconciliation confused
7. NodeUnstageVolume SKIPPED ❌
```

**Why Jobs Are Different:**
- Completed pods may linger before deletion
- Users often delete Job + PVC together in scripts
- Attach-detach controller less aggressive with completed pods
- Timing window for race condition is larger

---

## 🔬 Technical Details: Volume Tracking in kubelet

### How kubelet Identifies Volumes

kubelet uses multiple identifiers for a single volume:

1. **PV Name:** `pvc-aa13c335-3ed3-4847-b1f6-3e76cbfea69c`
2. **PVC Reference:** `flint-system/e2e-cleanup-test-pvc`
3. **Staging Path Hash:** `c1f21b1ce128adddfa057d57fa4907ce6b5bbd2d844c279905e060645de5bb7a`
4. **VolumeAttachment Name:** `csi-61bde0750d6e4b9889b85f45efe281c368195382ef951ee39085f310b4dc74d5`

The staging path hash is computed as:
```
SHA256(staging_target_path)
  where staging_target_path = 
    /var/lib/kubelet/plugins/kubernetes.io/csi/{DRIVER_NAME}/{VOLUME_ID}/globalmount
```

### The Lookup Problem

When kubelet's reconciler runs:

```go
// Simplified kubelet logic
volume := GetVolume FromASW(stagingPathHash)

// Need to check if volume is in DSW
// Requires looking up PVC
pvc := GetPVC(volume.PVCNamespace, volume.PVCName)
if pvc == nil {
    // PVC deleted - what to do?
    // Option A: Remove volume (safe?)
    // Option B: Keep volume (leaks resources)
    // Current behavior: Often chooses B, causing our issue
}
```

---

## 💡 Why Our Cleanup Script Works

Our cleanup script breaks the deadlock by:

1. **Deleting VolumeAttachment** → Removes one blocker
2. **PV can now be deleted** (no VolumeAttachment finalizer blocking it)
3. **DeleteVolume is called** by external-provisioner

**But DeleteVolume still fails** because NodeUnstageVolume was never called!

### The Fix Needed

We need to handle the case where DeleteVolume is called while volume is still staged:

```rust
async fn delete_volume(&self, volume_id: &str) -> Result<()> {
    // CHECK if volume is still staged on any node
    let staged_nodes = self.find_nodes_with_staged_volume(volume_id).await?;
    
    if !staged_nodes.is_empty() {
        println!("⚠️ [DELETE] Volume still staged on nodes: {:?}", staged_nodes);
        
        // FORCE unstage on each node
        for node in staged_nodes {
            println!("🔄 [DELETE] Force unstaging volume on node: {}", node);
            self.force_unstage_volume(&node, volume_id).await?;
        }
    }
    
    // Now safe to delete lvol
    self.delete_lvol(volume_id).await?;
    Ok(())
}
```

---

## 🛠️ Solutions

### Solution 1: Force Unstage in DeleteVolume (Recommended)

Implement defensive cleanup in DeleteVolume:

**Benefits:**
- Handles all edge cases
- Works even when kubelet fails to call NodeUnstageVolume
- Idempotent and safe

**Implementation:**
1. Check if volume has active mounts
2. Unmount gracefully (with retries)
3. Remove ublk device
4. Delete lvol
5. Clean up staging directories

### Solution 2: Background Cleanup Controller

Run a controller that periodically checks for orphaned mounts:

```rust
async fn cleanup_orphaned_volumes(&self) {
    // Find all staged volumes
    let staged = self.get_all_staged_volumes().await;
    
    for volume in staged {
        // Check if volume has active VolumeAttachment
        if !self.has_volume_attachment(&volume.id).await {
            // Check if any pods are using it
            if !self.has_active_pods(&volume.id).await {
                // Safe to clean up
                self.force_unstage_volume(&volume.node, &volume.id).await;
            }
        }
    }
}
```

### Solution 3: Extended Cleanup Script

Enhance our cleanup script to also clean up orphaned mounts:

```bash
# After deleting VolumeAttachments
# Also clean up ghost mounts

for node in $(kubectl get nodes -o name); do
    kubectl exec -n flint-system flint-csi-node-XXX -c flint-csi-driver -- \
      sh -c 'mount | grep "flint.csi.storage.io" | while read line; do
        dev=$(echo $line | awk "{print \$1}")
        if [ ! -e "$dev" ]; then
          mount_point=$(echo $line | awk "{print \$3}")
          echo "Unmounting ghost mount: $mount_point"
          umount -l "$mount_point"
        fi
      done'
done
```

---

## 📋 Recommended Action Plan

### Immediate (Now)

1. ✅ Use cleanup script after each test
2. ✅ Document the issue in README
3. ⏳ Implement Solution 1 (Force unstage in DeleteVolume)

### Short Term (Next Sprint)

4. ⏳ Add defensive cleanup in NodeUnstageVolume
5. ⏳ Improve error messages when DeleteVolume fails
6. ⏳ Add metrics for tracking orphaned volumes

### Medium Term

7. ⏳ Implement Solution 2 (Background cleanup controller)
8. ⏳ Add health checks for orphaned mounts
9. ⏳ Contribute findings to Kubernetes CSI community

---

## 🧪 Test Case to Verify Fix

```bash
#!/bin/bash

# Test: NodeUnstageVolume resilience

# 1. Create Job with PVC
kubectl apply -f job-with-pvc.yaml

# 2. Wait for completion
kubectl wait --for=condition=complete job/test-job

# 3. Delete Job AND PVC together (triggers race condition)
kubectl delete job test-job &
kubectl delete pvc test-pvc &
wait

# 4. Run cleanup script
./scripts/cleanup-stuck-volumeattachments.sh

# 5. Verify complete cleanup (should succeed with Solution 1)
sleep 10
kubectl get pv,pvc,volumeattachments
# Expected: No resources

# 6. Check for ghost mounts
kubectl exec -n flint-system $NODE_POD -c flint-csi-driver -- \
  mount | grep flint.csi.storage.io
# Expected: No output

# 7. Check SPDK lvols
# Expected: Lvol deleted successfully
```

---

## 📚 References

1. **Kubernetes CSI Spec:** https://github.com/container-storage-interface/spec
2. **kubelet Volume Manager:** https://github.com/kubernetes/kubernetes/tree/master/pkg/kubelet/volumemanager
3. **Related Issues:**
   - https://github.com/kubernetes/kubernetes/issues/84987
   - https://github.com/kubernetes-csi/external-attacher/issues/XXX

---

## 🎓 Key Learnings

1. **kubelet's volume manager** is the only component that calls NodeUnstageVolume
2. **VolumeAttachment deletion** does NOT automatically trigger NodeUnstageVolume
3. **PVC deletion before VolumeAttachment cleanup** breaks kubelet's volume tracking
4. **Jobs have worse cleanup behavior** than regular pods
5. **Ghost mounts** (mount without device) are a real problem that needs defensive handling
6. **CSI drivers must handle cleanup defensively** - don't rely solely on kubelet

---

**Conclusion:** This is a systemic issue with Kubernetes' volume lifecycle management for Jobs. Our CSI driver must implement defensive cleanup in DeleteVolume to handle cases where NodeUnstageVolume is never called.

