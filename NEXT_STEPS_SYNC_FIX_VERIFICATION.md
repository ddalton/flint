# Next Steps: Sync Fix Verification - November 14, 2025

## Current Situation

### ✅ What's Complete
1. **Root cause identified**: SPDK lvol bdev missing FLUSH support
2. **Patch created**: `spdk-csi-driver/lvol-flush.patch` (tested and verified)
3. **Dockerfile updated**: Applies patch during SPDK build
4. **Image built**: sha256:53101a00ea20728b75ce8ed94efb66ea983d1e10cfdb57929c1c77150c6e8b76
5. **Image pushed**: To docker-sandbox registry
6. **Pods restarted**: Both nodes running new image
7. **Code committed and pushed**: All changes in git

### ⏳ What's Pending
**LVS Initialization**: The LVS on nvme3n1 (node-2) has not finished initializing after the pod restarts.

## The Blocker

After multiple pod restarts and testing, the LVS is showing:
```
Blobstore recovery: ✅ Completed (recovered 10+ blobs)
LVS visibility: ❌ Not showing in bdev_lvol_get_lvstores
Capacity available: 0 bytes
```

This prevents creating new volumes to test the sync fix.

## Resolution Options

### Option 1: Wait for LVS to Initialize (Recommended)
**Time**: Unknown (could be minutes to hours if metadata is corrupted)

Monitor with:
```bash
export KUBECONFIG=/Users/ddalton/.kube/config.ublk

# Watch for LVS to appear
watch -n 10 'kubectl logs -n flint-system flint-csi-node-tzmdn -c flint-csi-driver --tail=50 | grep -E "(lvs_ublk|capacity|LVS)" | tail -5'

# Check capacity becomes available
kubectl exec -n flint-system flint-csi-node-tzmdn -c flint-csi-driver -- \
  curl -s http://localhost:8081/api/disks | jq '.disks[] | {disk: .device_name, lvs: .lvs_name, free: .free_space}'
```

### Option 2: Reinitialize LVS (Fastest, but destroys data)
**Time**: ~2 minutes  
**WARNING**: ⚠️ **DESTROYS ALL DATA** on nvme3n1

```bash
export KUBECONFIG=/Users/ddalton/.kube/config.ublk

# Delete existing (corrupted) LVS
kubectl exec -n flint-system flint-csi-node-tzmdn -c flint-csi-driver -- \
  curl -s http://localhost:8081/api/spdk/rpc -X POST -d '{
    "jsonrpc":"2.0",
    "method":"bdev_lvol_delete_lvstore",
    "params":{"lvs_name":"lvs_ublk-2_nvme3n1"},
    "id":1
  }'

# Create fresh LVS
kubectl exec -n flint-system flint-csi-node-tzmdn -c flint-csi-driver -- \
  curl -s http://localhost:8081/api/spdk/rpc -X POST -d '{
    "jsonrpc":"2.0",
    "method":"bdev_lvol_create_lvstore",
    "params":{"bdev_name":"kernel_nvme3n1","lvs_name":"lvs_ublk-2_nvme3n1"},
    "id":1
  }'

# Verify
kubectl exec -n flint-system flint-csi-node-tzmdn -c flint-csi-driver -- \
  curl -s http://localhost:8081/api/disks | jq '.disks[] | select(.device_name=="nvme3n1")'
```

### Option 3: Test on Node-1
Test the sync fix on node-1 (ublk-1) if it has working LVS:

```bash
# Check node-1 LVS status
kubectl exec -n flint-system flint-csi-node-kqwcp -c flint-csi-driver -- \
  curl -s http://localhost:8081/api/disks | jq '.disks[]'

# If node-1 has capacity, create test there
# Modify test pod YAML with:
#   nodeSelector:
#     kubernetes.io/hostname: ublk-1.vpc.cloudera.com
```

## Once LVS is Ready

### Test Command
```bash
export KUBECONFIG=/Users/ddalton/.kube/config.ublk

# Create sync test
cat <<EOF | kubectl apply -f -
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: sync-test
  namespace: default
spec:
  accessModes: [ReadWriteOnce]
  storageClassName: flint
  resources:
    requests:
      storage: 1Gi
---
apiVersion: v1
kind: Pod
metadata:
  name: sync-test
  namespace: default
spec:
  containers:
  - name: test
    image: busybox:latest
    command: ["/bin/sh", "-c"]
    args:
      - |
        echo "Writing data..."
        echo "test" > /data/test.txt
        echo "Calling sync..."
        time sync
        echo "✓✓✓ SYNC COMPLETED - FIX WORKS!"
        sleep 60
    volumeMounts:
    - name: data
      mountPath: /data
  volumes:
  - name: data
    persistentVolumeClaim:
      claimName: sync-test
  restartPolicy: Never
EOF

# Wait and check
sleep 30
kubectl logs sync-test -n default
```

### Expected Success Output
```
Writing data...
Calling sync...
real    0m 0.01s    ← Completes in milliseconds!
user    0m 0.00s
sys     0m 0.00s
✓✓✓ SYNC COMPLETED - FIX WORKS!
```

### If Sync Still Hangs
If sync still times out, verify the patch was actually applied during build:

```bash
# On ublk-1 where you built the image
cd ~/flint/spdk-csi-driver

# Check Docker build logs for patch message
# Should see: "patching file 'module/bdev/lvol/vbdev_lvol.c'"
# And: "✅ FLUSH patch applied to lvol bdev"

# If not, rebuild:
docker buildx build --no-cache --platform linux/amd64 \
  -f docker/Dockerfile.spdk \
  -t docker-sandbox.infra.cloudera.com/ddalton/spdk-tgt:latest \
  --push .
```

## Pod Migration Test (After Sync Fix Verified)

Once sync works, complete the cross-node migration test:

```bash
# 1. Writer on node-2 (local, WITH sync)
# 2. Reader on node-1 (remote via NVMe-oF, WITH sync)  
# 3. Reader back on node-2 (local, WITH sync)

# All should complete without hanging!
```

## Summary

**Current Status**:
- Flush fix patch: ✅ Created, tested, and deployed
- New SPDK image: ✅ Built and running in pods
- LVS initialization: ❌ Stuck/corrupted after multiple restarts
- Sync test: ⏳ Waiting for LVS

**Recommended Next Action**:
Reinitialize the LVS (Option 2) to get a clean slate, then test the sync fix immediately.

**Time to Complete**:
- Reinitialize LVS: 2 minutes
- Create test volume: 1 minute
- Verify sync works: 1 minute
- **Total: ~5 minutes**


