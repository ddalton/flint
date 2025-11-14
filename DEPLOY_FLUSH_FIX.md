# Deploy FLUSH Fix for ublk Sync Hang

## What Was Fixed

Added FLUSH support to SPDK lvol bdev module to fix the sync hang issue on ublk devices.

**Changes Made:**
- Modified `Dockerfile.spdk` to patch `module/bdev/lvol/vbdev_lvol.c` during build
- Adds `SPDK_BDEV_IO_TYPE_FLUSH` to supported I/O types
- Implements `lvol_flush()` handler function
- Routes FLUSH requests through the bdev layer

## Rebuild and Deploy

### Step 1: Rebuild SPDK Container

```bash
cd /Users/ddalton/projects/rust/flint/spdk-csi-driver

# Build the SPDK container with the flush fix
docker build -f docker/Dockerfile.spdk \
  -t docker-sandbox.infra.cloudera.com/ddalton/spdk-tgt:flush-fix \
  .

# Tag as latest
docker tag docker-sandbox.infra.cloudera.com/ddalton/spdk-tgt:flush-fix \
  docker-sandbox.infra.cloudera.com/ddalton/spdk-tgt:latest

# Push to registry
docker push docker-sandbox.infra.cloudera.com/ddalton/spdk-tgt:flush-fix
docker push docker-sandbox.infra.cloudera.com/ddalton/spdk-tgt:latest
```

**Note**: This build takes ~10-15 minutes as it compiles SPDK from source.

### Step 2: Restart CSI Node Pods

```bash
export KUBECONFIG=/Users/ddalton/.kube/config.ublk

# Restart CSI node pods to pull new SPDK image
kubectl delete pod -n flint-system -l app=flint-csi-node

# Wait for pods to be ready
kubectl wait --for=condition=Ready pod -l app=flint-csi-node -n flint-system --timeout=120s

# Verify new pods are running
kubectl get pods -n flint-system -l app=flint-csi-node
```

### Step 3: Wait for LVS Recovery

After restarting, SPDK needs to import existing LVS:

```bash
# Check SPDK logs for LVS import
kubectl logs -n flint-system -l app=flint-csi-node -c spdk-tgt --tail=50 | grep -i lvol

# Verify LVS is loaded
kubectl exec -n flint-system $(kubectl get pod -n flint-system -l app=flint-csi-node -o name | head -1 | cut -d/ -f2) \
  -c flint-csi-driver -- \
  curl -s http://localhost:8081/api/disks | jq '.disks[] | select(.blobstore_initialized==true)'
```

## Testing the Fix

### Test 1: Basic Sync Test

```bash
export KUBECONFIG=/Users/ddalton/.kube/config.ublk

# Create test PVC and pod
cat <<EOF | kubectl apply -f -
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: sync-test-pvc
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
        echo "test data" > /data/test.txt
        echo "Calling sync..."
        time sync
        echo "✓ Sync completed successfully!"
        echo "Data persisted:"
        cat /data/test.txt
        sleep 300
    volumeMounts:
    - name: data
      mountPath: /data
  volumes:
  - name: data
    persistentVolumeClaim:
      claimName: sync-test-pvc
  restartPolicy: Never
EOF

# Wait for pod to start
sleep 15

# Check logs
kubectl logs sync-test -n default
```

**Expected Output:**
```
Writing data...
Calling sync...
real    0m 0.01s    ← Should complete in milliseconds
user    0m 0.00s
sys     0m 0.00s
✓ Sync completed successfully!
Data persisted:
test data
```

**If sync hangs:** The patch didn't apply correctly, rebuild SPDK container.

### Test 2: Pod Deletion Test

```bash
# Delete the pod cleanly
kubectl delete pod sync-test -n default

# Should complete in < 10 seconds (no force needed!)
```

**Expected:** Pod terminates gracefully without force delete.

### Test 3: Cross-Node Migration with Sync

```bash
# Test full migration with sync commands
cat <<EOF | kubectl apply -f -
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: migration-pvc
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
  name: writer-node2
  namespace: default
spec:
  nodeSelector:
    kubernetes.io/hostname: ublk-2.vpc.cloudera.com
  containers:
  - name: writer
    image: busybox:latest
    command: ["/bin/sh", "-c"]
    args:
      - |
        echo "Writing on node-2..."
        echo "Data from node-2" > /data/test.txt
        sync  # ← Should NOT hang
        echo "✓ Data synced"
        cat /data/test.txt
        sleep 10
    volumeMounts:
    - name: data
      mountPath: /data
  volumes:
  - name: data
    persistentVolumeClaim:
      claimName: migration-pvc
  restartPolicy: Never
EOF

# Wait for completion
kubectl wait --for=condition=Ready=False pod/writer-node2 -n default --timeout=30s

# Delete pod
kubectl delete pod writer-node2 -n default

# Create reader on node-1
cat <<EOF | kubectl apply -f -
apiVersion: v1
kind: Pod
metadata:
  name: reader-node1
  namespace: default
spec:
  nodeSelector:
    kubernetes.io/hostname: ublk-1.vpc.cloudera.com
  containers:
  - name: reader
    image: busybox:latest
    command: ["/bin/sh", "-c"]
    args:
      - |
        echo "Reading on node-1..."
        cat /data/test.txt
        sync  # ← Should work on NVMe-oF too
        echo "✓ Cross-node migration successful!"
        sleep 300
    volumeMounts:
    - name: data
      mountPath: /data
  volumes:
  - name: data
    persistentVolumeClaim:
      claimName: migration-pvc
  restartPolicy: Never
EOF

# Check logs
sleep 20
kubectl logs reader-node1 -n default
```

**Expected:** Both pods complete without hanging, data persists across migration.

### Test 4: Database Workload (PostgreSQL)

```bash
# Test with real database that requires fsync
helm install postgres bitnami/postgresql \
  --set primary.persistence.storageClass=flint \
  --set primary.persistence.size=2Gi

# Wait for PostgreSQL to start
kubectl wait --for=condition=Ready pod -l app.kubernetes.io/name=postgresql --timeout=120s

# Check if PostgreSQL is running without hangs
kubectl logs -l app.kubernetes.io/name=postgresql -f
# Should see normal PostgreSQL startup, no hung processes
```

## Verification Checklist

After deploying the fix, verify:

- [ ] SPDK container image tag shows new build timestamp
- [ ] CSI node pods restart successfully
- [ ] LVS imported and volumes visible
- [ ] `sync` command completes in < 1 second
- [ ] Pods delete cleanly without force
- [ ] Cross-node migration works
- [ ] No "directory not empty" errors in kubelet
- [ ] Database workloads start successfully

## Rollback Plan

If the patch causes issues:

```bash
# Revert to previous image (without patch)
kubectl set image daemonset/flint-csi-node -n flint-system \
  spdk-tgt=docker-sandbox.infra.cloudera.com/ddalton/spdk-tgt:pre-flush-fix

# Or rebuild without patch:
cd /Users/ddalton/projects/rust/flint/spdk-csi-driver
git checkout docker/Dockerfile.spdk  # Revert changes
docker build -f docker/Dockerfile.spdk -t ...spdk-tgt:rollback .
```

## Success Criteria

✅ **FIXED** when:
- sync completes instantly
- Pods terminate gracefully
- No force deletes needed
- Database workloads function properly
- Cross-node migration works end-to-end

## Timeline

- Docker build: ~15 minutes
- Deploy + restart: ~5 minutes
- LVS recovery: ~2 minutes
- Testing: ~10 minutes
- **Total: ~30-35 minutes**

Much faster than architectural changes like switching to NVMe-oF for all volumes!


