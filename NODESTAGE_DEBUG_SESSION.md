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

## 💡 Likely Fix

The port 9810 panic is probably preventing the CSI gRPC service from functioning correctly. Fix this and NodeStageVolume should start being called properly.

---
**Good luck with the next session!** You're 95% there! 🚀

