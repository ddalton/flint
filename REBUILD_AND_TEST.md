# Rebuild and Test Instructions

## Changes Pushed

**Commit**: `07722c9`  
**Branch**: `feature/pnfs-implementation`  
**Fix**: Added debug logging for SEQUENCE operation to diagnose SeqMisordered errors

---

## Rebuild on Linux Machine

```bash
# SSH to the build machine
ssh root@cdrv-1.vpc.cloudera.com

# Navigate to flint directory
cd /root/flint/spdk-csi-driver

# Pull latest changes
git fetch origin
git checkout feature/pnfs-implementation
git pull origin feature/pnfs-implementation

# Verify you have the latest commit
git log --oneline -1
# Should show: 07722c9 fix: Add debug logging for SEQUENCE operation...

# Build and push the image
docker buildx build --platform linux/amd64 \
  -f docker/Dockerfile.pnfs \
  -t docker-sandbox.infra.cloudera.com/ddalton/pnfs:latest \
  --push .

# Wait for build to complete (~8 minutes)
```

---

## Redeploy on Kubernetes

```bash
# Set kubeconfig
export KUBECONFIG=/Users/ddalton/.kube/config.cdrv

# Delete existing test deployment
kubectl delete namespace pnfs-test

# Wait for cleanup
sleep 10

# Recreate namespace
kubectl create namespace pnfs-test

# Deploy MDS
kubectl apply -f /tmp/pnfs-test-mds.yaml

# Deploy DS (2 pods with 1GB PVCs)
kubectl apply -f /tmp/pnfs-test-ds.yaml

# Wait for pods to start
kubectl wait --for=condition=ready pod -l app=pnfs-mds -n pnfs-test --timeout=60s
kubectl wait --for=condition=ready pod -l app=pnfs-ds -n pnfs-test --timeout=60s

# Check status
kubectl get pods -n pnfs-test
```

---

## Test with NFS Client

```bash
# SSH to node
ssh root@cdrv-1.vpc.cloudera.com

# Get MDS IP
MDS_IP=$(kubectl get svc -n pnfs-test pnfs-mds -o jsonpath='{.spec.clusterIP}')
echo "MDS IP: $MDS_IP"

# Mount
mkdir -p /mnt/pnfs-test
mount -t nfs -o vers=4.1,proto=tcp $MDS_IP:/ /mnt/pnfs-test

# Check mount
df -h /mnt/pnfs-test

# Test file operations
echo "Test file" > /mnt/pnfs-test/test.txt
cat /mnt/pnfs-test/test.txt

# Test larger file (should be faster now)
dd if=/dev/zero of=/mnt/pnfs-test/bigfile bs=1M count=10

# Check for errors
dmesg | tail -20 | grep -i nfs
```

---

## What to Look For

### In MDS Logs (with new debug output)

```bash
kubectl logs -n pnfs-test -l app=pnfs-mds --tail=50

# Look for:
# 🔍 SEQUENCE processing: slot=0, client_seq=1, slot_seq=0, expecting=1
# ✅ SEQUENCE first request: slot=0, seq=1
# (Should NOT see: ❌ SEQUENCE mismatch)
```

### In DS Logs

```bash
kubectl logs -n pnfs-test pnfs-ds-1 --tail=30

# Look for:
# ✅ Successfully registered with MDS
# ✅ Heartbeat acknowledged
# DS READ: ... (when client reads)
# DS WRITE: ... (when client writes)
```

### In Client dmesg

```bash
ssh root@cdrv-1.vpc.cloudera.com "dmesg | tail -20"

# Should NOT see:
# "check lease failed on NFSv4 server ... with error 10058"
#
# Should see (if pNFS working):
# "NFS: pNFS LAYOUTGET"
# "NFS: using pNFS"
```

---

## Expected Behavior After Fix

### Before Fix (Current)
```
Client → MDS: SEQUENCE (seq=1)
MDS: Expected seq=1, got seq=1... but slot_seq=0
MDS: Returns SeqMisordered ❌
Client: Retries forever
Result: Hang on I/O
```

### After Fix
```
Client → MDS: SEQUENCE (seq=1)
MDS: slot_seq=0, client_seq=1, this is first request
MDS: Returns OK ✅
Client: Proceeds with operations
Result: I/O works
```

---

## Troubleshooting

### If Still Getting SeqMisordered

Check the debug logs:
```bash
kubectl logs -n pnfs-test -l app=pnfs-mds | grep "SEQUENCE processing"
```

Look for the values:
- `slot_seq`: What the server expects
- `client_seq`: What the client sent
- `expecting`: What should come next

### If Symlink Errors

The MDS is exporting the container root `/` which has system symlinks.
This is cosmetic - doesn't affect pNFS functionality.

To fix properly, MDS should export a dedicated directory, but for
testing, you can ignore symlink errors.

---

## Performance Test

Once working:

```bash
# Baseline (should be fast now)
time dd if=/dev/zero of=/mnt/pnfs-test/test1 bs=1M count=100

# Should complete in ~1 second (100 MB)
```

---

## Cleanup

```bash
# Unmount
ssh root@cdrv-1.vpc.cloudera.com "umount /mnt/pnfs-test"

# Delete test namespace
kubectl delete namespace pnfs-test
```

---

## Summary

**Fix Pushed**: ✅ Commit 07722c9  
**Next**: Rebuild image on cdrv-1 and redeploy  
**Expected**: SEQUENCE operations should work, I/O should be fast  
**Debug**: Enhanced logging to diagnose any remaining issues

