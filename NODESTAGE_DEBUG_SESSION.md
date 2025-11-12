# NodeStageVolume Troubleshooting Session

**Date:** November 12, 2025  
**Branch:** `feature/minimal-state`  
**Cluster:** `KUBECONFIG=/Users/ddalton/.kube/config.ublk`

## 🎉 What's Working

### ✅ Volumes Created Successfully
```bash
kubectl exec -n flint-system flint-csi-node-mgw84 -c spdk-tgt -- python3 -c "..." 
# Shows 3 logical volumes:
# - 3a5edd12-3bd7-4263-96ba-1a01c715123f (vol_pvc-37ed92a7..., 1GB)
# - d9544543-79e6-472a-9d7c-315376397f61 (vol_pvc-34b92bac..., 5GB) 
# - 354cf4d3-f47a-4789-af7a-7892e574c65e (vol_pvc-8373291b..., 2GB)
```

### ✅ LVS Discovered on nvme3n1
- LVS Name: `lvs_ublk-2_nvme3n1`
- Base bdev: `kernel_nvme3n1`
- Free: 996GB (1013855 clusters)
- Disk shows: `initialized: true`

### ✅ ControllerPublishVolume Working
```
12:46:20 - ✅ [CONTROLLER] Volume pvc-37ed92a7-a093-4aec-b6d9-e507008a5c43 published successfully
```

### ✅ VolumeAttachment Succeeded
```yaml
status:
  attached: true
  attachmentMetadata:
    volumeType: local
    bdevName: 3a5edd12-3bd7-4263-96ba-1a01c715123f
    lvsName: lvs_ublk-2_nvme3n1
```

## ❌ What's NOT Working

### Issue: NodeStageVolume Not Being Called

**Symptoms:**
- Test pod stuck at `ContainerCreating` for 73+ minutes
- Latest error: `ublk device /dev/ublkb42643 not found`
- NO NodeStageVolume (📦) logs in flint-csi-driver container
- ONLY NodePublishVolume (📋) logs found

**Evidence:**
```bash
# Pod events:
11m - SuccessfulAttachVolume
5m  - FailedMount: "Node publish volume not implemented"
59s - FailedMount: "ublk device /dev/ublkb42643 not found"

# Node logs:
NO "📦" emoji (NodeStageVolume marker)
NO "Staging volume" messages
NO "Creating ublk device" messages

# Old ublk devices on node (from Nov 10):
/dev/ublkb502  (created Nov 10 23:31)
/dev/ublkc502  (created Nov 10 23:31)
```

### Issue: Port 9810 Panic
```
thread 'tokio-runtime-worker' panicked at warp/src/server.rs:217:27:
error binding to 0.0.0.0:9810: error creating server listener: Address already in use (os error 98)
```

## 🔍 Investigation Steps for Next Session

### 1. Fix Port 9810 Panic
The health server conflict might be preventing CSI gRPC from working properly.

**Check:**
- What's using port 9810?
- Is it the health server trying to bind twice?
- Does the panic affect the gRPC server?

**Files to check:**
- `spdk-csi-driver/src/main.rs` - health server setup
- Helm chart - health port configuration

### 2. Verify CSI gRPC Service is Functional

**Test capability query:**
```bash
POD=$(kubectl get pod -n flint-system -l app=flint-csi-node -o json | \
      jq -r '.items[] | select(.spec.nodeName=="ublk-2.vpc.cloudera.com") | .metadata.name')

kubectl exec -n flint-system $POD -c flint-csi-driver -- sh -c '
apk add --no-cache socat 2>/dev/null || true
# Use grpcurl or manual test to query NodeGetCapabilities
'
```

**Expected response:**
- Should include `STAGE_UNSTAGE_VOLUME` capability

### 3. Test NodeStage Directly

**Manually trigger NodeStageVolume:**
```bash
# From controller or test script, call NodeStageVolume RPC
# Check if it's actually implemented and reachable
```

### 4. Check Kubelet Logs

```bash
# On ublk-2 node:
journalctl -u kubelet -f | grep -i "stage\|csi"
```

Look for:
- Why kubelet is skipping NodeStageVolume
- Any capability cache issues
- CSI socket communication errors

### 5. Restart Kubelet (if needed)

```bash
# On ublk-2 node:
systemctl restart kubelet
```

This clears any cached capabilities.

## 🐛 Debugging Commands

### Check CSI Socket
```bash
kubectl exec -n flint-system $POD -c flint-csi-driver -- ls -la /csi/csi.sock
```

### Monitor Node Logs in Real-Time
```bash
kubectl logs -n flint-system $POD -c flint-csi-driver -f | grep -E "📦|📋|NODE"
```

### Check Pod Scheduling
```bash
kubectl get pod final-test-pod -o yaml | grep -A30 "volumes:"
```

## 📊 Key Discoveries Today

### 1. Missing base_bdev Field (CRITICAL!)
**Root cause:** LvsInfo struct didn't include `base_bdev`, preventing disk-to-LVS matching.
**Fix:** Added field to struct and serialization (commit 5884099)

### 2. Slow Disk Discovery (CRITICAL!)
**Root cause:** Every /api/disks request ran 20s auto-recovery
**Fix:** Added fast path without auto-recovery (commit da3e1ac)

### 3. Volume Lookup Method
**Root cause:** Using bdev_get_bdevs (returns UUIDs) instead of bdev_lvol_get_lvols (returns names)
**Fix:** Use bdev_lvol_get_lvols to find vol_{volume_id} (commit a213cfa)

### 4. Duplicate Container
**Root cause:** Both flint-csi-driver and node-agent binding to port 8081
**Fix:** Removed duplicate node-agent container (commit b179515)

### 5. RDMA Transport Fatal Error
**Root cause:** RDMA init in config was fatal on systems without RDMA hardware
**Fix:** Only TCP in config, RDMA optional via RPC (commit fe75361)

## 📁 Important Files

### Modified Files (feature/minimal-state branch)
- `spdk-csi-driver/src/main.rs` - CSI lifecycle methods
- `spdk-csi-driver/src/driver.rs` - NVMe-oF, volume mgmt
- `spdk-csi-driver/src/node_agent.rs` - HTTP API endpoints
- `spdk-csi-driver/src/minimal_disk_service.rs` - Disk discovery, LVS
- `spdk-csi-driver/src/spdk_native.rs` - LvsInfo struct
- `spdk-csi-driver/docker/Dockerfile.spdk` - Graceful shutdown
- `flint-csi-driver-chart/templates/node.yaml` - preStop hook, removed duplicate
- `flint-csi-driver-chart/templates/rbac.yaml` - volumeattachments permissions

### Test Resources
```yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: final-test-pvc
spec:
  accessModes: [ReadWriteOnce]
  storageClassName: flint-single-replica
  resources:
    requests:
      storage: 2Gi
---
apiVersion: v1
kind: Pod
metadata:
  name: final-test-pod
spec:
  containers:
  - name: test-container
    image: nginx:latest
    volumeMounts:
    - name: test-volume
      mountPath: /data
  volumes:
  - name: test-volume
    persistentVolumeClaim:
      claimName: final-test-pvc
```

## 🔧 Quick Status Check Commands

```bash
# Set kubeconfig
export KUBECONFIG=/Users/ddalton/.kube/config.ublk

# Check pod status
kubectl get pod final-test-pod
kubectl get pvc final-test-pvc
kubectl get volumeattachment | grep pvc-8373291b

# Check logs
kubectl logs -n flint-system flint-csi-node-mgw84 -c flint-csi-driver --tail=100
kubectl logs -n flint-system -l app=flint-csi-controller -c flint-csi-controller --tail=50

# Check ublk devices on node
kubectl exec -n flint-system flint-csi-node-mgw84 -c spdk-tgt -- ls -la /dev/ublk*

# Check LVS state
kubectl exec -n flint-system flint-csi-node-mgw84 -c spdk-tgt -- python3 -c "
import socket, json
s = socket.socket(socket.AF_UNIX)
s.connect('/var/tmp/spdk.sock')
rpc = json.dumps({'jsonrpc': '2.0', 'id': 1, 'method': 'bdev_lvol_get_lvstores'})
s.send((rpc + '\n').encode())
print(json.loads(s.recv(8192).decode()))
s.close()
"
```

## 🎯 Success Criteria

When everything works, you should see:
1. `final-test-pod` status: `Running` (not ContainerCreating)
2. ublk device created: `/dev/ublkb{some_id}`
3. Logs showing:
   - `📦 [NODE] Staging volume pvc-8373291b...`
   - `✅ [NODE] ublk device created: /dev/ublkb{id}`  
   - `📋 [NODE] Publishing volume pvc-8373291b...`
   - `✅ [NODE] Volume published successfully`
4. Pod running nginx with volume mounted at `/data`

## ✅ Session 2 Progress (Nov 12, 21:15 UTC)

### Fixed: Port 9810 Panic
**Root Cause:** Health server trying to bind to port 9810 which was already in use, causing container crash loops (7 restarts observed).

**Fix:** Changed `values.yaml` health port from 9810 to 9809. In three-container mode, the node-agent is integrated into flint-csi-driver, so 9809 is available.

**Result:** ✅ Container now stable with 0 restarts!

### Verified Configuration
- ✅ CSI socket exists at `/csi/csi.sock` (inside container)
- ✅ `plugin-dir` volume mount correctly configured
- ✅ node-driver-registrar successfully connected and registered driver
- ✅ PV has `volumeMode: Filesystem` (correct for staging)
- ✅ `NodeGetCapabilities` returns `StageUnstageVolume` capability

### 🔴 Remaining Issue: NodeStageVolume Still Not Called

**Symptoms:**
- Kubelet only calls `NodePublishVolume` (📋), never `NodeStageVolume` (📦)
- Error: "ublk device /dev/ublkb12733 does not exist" (because staging never created it)
- Mount error: "mount(2) system call failed: Not a directory"

**Branch Comparison Findings:**
- `main` branch has **identical** `NodeGetCapabilities` and `NodeStageVolume` code
- `main` branch has separate `node.rs` file but same implementation
- Commit 3735927 originally implemented the full CSI lifecycle we have now
- Commit 8a41753 added NodePublishVolume for bind mounting
- All branches use same CSIDriver config: `fsGroupPolicy: ReadWriteOnceWithFSType`
- All branches use same StorageClass: `volumeBindingMode: WaitForFirstConsumer`

**Investigation Needed:**
1. Add GRPC request logging to see what methods kubelet is actually calling
2. Check if there's a Kubernetes 1.33.5-specific behavior change
3. Verify kubelet can query NodeGetCapabilities successfully
4. Test with a fresh PVC to eliminate caching issues

**Environment:**
- Kubernetes: v1.33.5+rke2r1 (RKE2 distribution)
- Node: ublk-2.vpc.cloudera.com (Ubuntu 24.04 LTS)

**Commits:** a16f1d6 (health fix), cab01fa (GRPC logging)

---
**Status:** Health port fixed, but NodeStageVolume mystery remains! 🔍

## 🎉 Session 3 Discovery (Nov 12, 21:32 UTC) - BREAKTHROUGH!

### GRPC Logging Reveals The Truth!

**Critical Finding:** NodeStageVolume **IS** being called! The earlier assumption was wrong.

**Actual Flow Observed:**
```
✅ Node.NodeGetCapabilities called → returns StageUnstageVolume
✅ Node.NodeStageVolume CALLED ← THIS WAS HAPPENING ALL ALONG!
  ✅ ublk device created: /dev/ublkb41339
  ✅ Volume staged successfully
✅ Node.NodePublishVolume called
  ❌ Mount failed: "mount(2) system call failed: Not a directory"
```

### The Real Problem:

The issue is in `NodePublishVolume` trying to bind mount the ublk block device:
```rust
mount --bind /dev/ublkb41339 /var/lib/kubelet/pods/.../mount
```

This fails because:
- We're trying to bind mount a **block device** to a **directory**
- For `volumeMode: Filesystem`, we need to:
  1. **Format** the ublk device with a filesystem (in NodeStageVolume)
  2. **Mount** it to the staging path (in NodeStageVolume)
  3. **Bind mount** the staging path to target (in NodePublishVolume)

**Commits:** 
- cab01fa: GRPC logging added
- 7c97fef: Removed redundant ublk_create_target call
- bca45b6: Implemented proper filesystem volume support

### The Solution:

**NodeStageVolume** now properly handles filesystem volumes:
```rust
1. Create ublk device from bdev
2. Format device with filesystem (ext4 default, skip if already formatted)
3. Mount formatted device to staging_target_path
4. For block volumes: skip formatting/mounting
```

**NodePublishVolume** now uses correct bind mount source:
```rust
Filesystem volumes: mount --bind staging_target_path target_path
Block volumes:      mount --bind /dev/ublkbN target_path
```

**NodeUnstageVolume** implemented:
```rust
1. Unmount staging_target_path
2. Delete ublk device
3. Disconnect from NVMe-oF (if remote)
```

**NodeUnpublishVolume** simplified:
```rust
1. Unmount target_path (the bind mount)
2. Remove target directory
(Device cleanup happens in NodeUnstageVolume)
```

## 🎉 COMPLETE SUCCESS! (Nov 12, 21:49 UTC)

### Verification Results:

**Test Pod:** `debug-test-pod` - **RUNNING** ✅

**Volume Flow:**
```bash
# NodeStageVolume executed:
📦 [NODE] Staging volume pvc-1d4f851c...
📁 [NODE] Formatting device /dev/ublkb41339 with ext4
🔧 [NODE] Mounting /dev/ublkb41339 to .../globalmount
✅ [NODE] Volume staged successfully

# NodePublishVolume executed:
📋 [NODE] Publishing volume pvc-1d4f851c...
📋 [NODE] Filesystem volume - bind mounting staging path to target
✅ [NODE] Volume published successfully
```

**Device Verification:**
```bash
# ublk device exists and is formatted:
$ blkid /dev/ublkb41339
/dev/ublkb41339: UUID="5f086edd-fb60-4174-af3c-8ca9650c4e51" TYPE="ext4"

# Mounted correctly (twice - staging and target):
$ findmnt | grep ublk
.../globalmount  /dev/ublkb41339  ext4  rw,relatime,stripe=256
.../mount        /dev/ublkb41339  ext4  rw,relatime,stripe=256

# Volume is accessible from pod:
$ kubectl exec debug-test-pod -- df -h /data
Filesystem      Size  Used Avail Use%
/dev/ublkb41339 974M   24K  907M   1%

# I/O works:
$ kubectl exec debug-test-pod -- dd if=/dev/zero of=/data/bigfile bs=1M count=100
100MB written at 1.6 GB/s ✅
```

### 🏆 Final Status: **RESOLVED**

All issues fixed:
1. ✅ Health port panic (9810 → 9809)
2. ✅ NodeStageVolume being called (always was, but now logged)
3. ✅ Filesystem formatting and mounting
4. ✅ Proper bind mounting in NodePublishVolume
5. ✅ Complete lifecycle implementation
6. ✅ Pod running with working volume storage

**Final Commits:**
- a16f1d6: Health port fix
- e396bfb: Branch comparison docs
- cab01fa: GRPC logging
- 7c97fef: Removed redundant ublk init
- bca45b6: Filesystem volume support ⭐
- 52cbcd6: Final documentation

---
**Mission Accomplished!** 🚀 The CSI driver now fully supports filesystem volumes with proper Stage/Unstage/Publish/Unpublish lifecycle!

