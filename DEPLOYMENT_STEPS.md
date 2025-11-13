# Deployment Steps for Ghost Mount Fix

## Code Changes Summary

✅ **Files Modified:**
- `spdk-csi-driver/src/main.rs` - Added ghost mount cleanup and improved NodeUnstageVolume

**Changes:**
1. **NodeUnstageVolume** (lines 745-856):
   - Verify mount with `mountpoint -q` before unmount
   - Retry unmount 3 times with 100ms delays
   - Fallback to lazy unmount (`-l`)
   - **Critical verification** before deleting ublk device
   - Returns error if unmount fails (prevents ghost mounts)

2. **Startup Ghost Mount Cleanup** (lines 69-137):
   - Scans for ublk mounts at startup
   - Checks if devices exist
   - Cleans up any ghost mounts
   - Runs before CSI server starts

## Build & Deploy

### 1. Build the Image

```bash
cd /Users/ddalton/projects/rust/flint/spdk-csi-driver

# Build the Docker image
docker build -f docker/Dockerfile.csi \
  -t docker-sandbox.infra.cloudera.com/ddalton/spdk-csi/csi-driver:v0.4.0 .

# Push to registry
docker push docker-sandbox.infra.cloudera.com/ddalton/spdk-csi/csi-driver:v0.4.0
```

### 2. Deploy to Cluster

```bash
export KUBECONFIG=/Users/ddalton/.kube/config.ublk

# Option A: Restart the DaemonSet (forces pull of new image)
kubectl rollout restart daemonset/flint-csi-node -n flint-system

# Option B: Delete pods to force recreation
kubectl delete pods -n flint-system -l app=flint-csi-node

# Wait for pods to be ready
kubectl wait --for=condition=ready pod -n flint-system -l app=flint-csi-node --timeout=120s
```

### 3. Verify Startup Cleanup

Check the logs to see if ghost mounts were found and cleaned:

```bash
# Check ublk-1 logs
kubectl logs -n flint-system -l app=flint-csi-node -l node=ublk-1.vpc.cloudera.com -c flint-csi-driver | grep -A5 "STARTUP"

# Check ublk-2 logs
kubectl logs -n flint-system -l app=flint-csi-node -l node=ublk-2.vpc.cloudera.com -c flint-csi-driver | grep -A5 "STARTUP"
```

Expected output:
```
🧹 [STARTUP] Scanning for ghost mounts...
✅ [STARTUP] No ghost mounts found
```

Or if ghost mounts were found:
```
🧹 [STARTUP] Scanning for ghost mounts...
👻 [STARTUP] Found ghost mount: /dev/ublkb49642 -> /var/lib/kubelet/... (device doesn't exist)
✅ [STARTUP] Cleaned ghost mount: /var/lib/kubelet/...
📊 [STARTUP] Ghost mount cleanup: found 1, cleaned 1
```

## Testing

### Quick Test

Run the automated migration test:

```bash
cd /Users/ddalton/projects/rust/flint
./test-migration.sh
```

This script tests:
- ✅ Write data on ublk-2 (local)
- ✅ Fast pod deletion (<10s)
- ✅ No ghost mounts
- ✅ Read data on ublk-1 (remote via NVMe-oF)

### Manual Verification Commands

```bash
export KUBECONFIG=/Users/ddalton/.kube/config.ublk

# Check for ghost mounts on ublk-2
kubectl exec -n flint-system \
  $(kubectl get pod -n flint-system -l app=flint-csi-node \
    -o jsonpath='{.items[?(@.spec.nodeName=="ublk-2.vpc.cloudera.com")].metadata.name}') \
  -c flint-csi-driver -- sh -c \
  'mount | grep ublkb | while read line; do dev=$(echo $line | awk "{print \$1}"); [ -e "$dev" ] && echo "✅ $dev exists" || echo "❌ GHOST: $dev"; done'

# Check for hung sync processes
kubectl get pod -n flint-system -o wide | grep Running | while read name rest; do
  echo "Checking $name..."
  kubectl exec -n flint-system $name -c flint-csi-driver -- ps aux 2>/dev/null | grep sync || true
done
```

## Expected Results

### Before Fix (Broken)
- Pod deletion: 30s (SIGKILL timeout)
- Ghost mounts: Present
- Data persistence: ❌ Lost
- Sync: Hangs on non-existent device

### After Fix (Working)
- Pod deletion: 1-5s ✅
- Ghost mounts: None ✅
- Data persistence: ✅ Works
- Sync: No hanging ✅

## Troubleshooting

### If test fails:

1. **Check CSI driver logs:**
   ```bash
   kubectl logs -n flint-system -l app=flint-csi-node -c flint-csi-driver --tail=100
   ```

2. **Look for unmount failures:**
   ```bash
   kubectl logs -n flint-system -l app=flint-csi-node -c flint-csi-driver | grep -i "unmount"
   ```

3. **Check for mount verification:**
   ```bash
   kubectl logs -n flint-system -l app=flint-csi-node -c flint-csi-driver | grep "mountpoint"
   ```

4. **Verify ublk devices:**
   ```bash
   kubectl exec -n flint-system <csi-pod> -c flint-csi-driver -- ls -la /dev/ublkb*
   ```

## Success Criteria

✅ Pod deletion completes in < 5 seconds
✅ No ghost mounts in mount table
✅ Data persists across pod migrations
✅ No sync processes hanging
✅ Clean CSI driver restarts

## Next Steps After Successful Test

Once testing passes, you're ready for production use! The ghost mount bug is fixed and the CSI driver should handle:
- Fast pod creation/deletion
- Reliable data persistence
- Clean pod migrations between nodes
- Graceful driver restarts

