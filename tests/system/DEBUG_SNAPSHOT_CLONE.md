# Debug: SPDK Snapshot Clone Data Preservation

## The Mystery

Snapshot clone is created successfully but the restored volume has no data.

## What We Know

1. ✅ Initial data write succeeds (no I/O errors with wipefs fix)
2. ✅ Snapshot created successfully (readyToUse: true)
3. ✅ Clone created from snapshot (54 allocated clusters)
4. ✅ Cloned volume mounts successfully
5. ❌ **Cloned volume is EMPTY** (no files)

## SPDK Thin Clone Behavior

SPDK `bdev_lvol_clone` creates a thin clone using copy-on-write:

```
Time 1: Create snapshot
  Snapshot lvol: [Data blocks...]
  
Time 2: Clone snapshot  
  Clone lvol: → References snapshot's blocks (COW)
  
Time 3: Read from clone
  Should return data from snapshot's blocks
```

## Hypothesis: Clone References Wrong Snapshot

Looking at SPDK metadata:
```json
Clone: {
  "base_snapshot": "snap_pvc-XXX_timestamp",
  "num_allocated_clusters": 0,  // Thin - no data copied
  "clone": true
}
```

**The clone is thin and references the snapshot.**

**Possible issues:**
1. Snapshot taken before data flushed to SPDK
2. Clone created from empty snapshot
3. Filesystem exists but blocks are all zeros
4. ublk not reading clone correctly

## Manual Test Steps

To isolate the issue:

```bash
# 1. Create PVC and write data
kubectl apply -f - <<EOF
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: test-source
  namespace: default
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 1Gi
  storageClassName: flint
EOF

# 2. Write data
kubectl run writer --image=busybox --restart=Never -- sh -c "
  echo 'TEST DATA' > /data/test.txt
  sync
  sync /data/test.txt
  sleep 3
  cat /data/test.txt
" --overrides='{"spec":{"containers":[{"name":"writer","volumeMounts":[{"name":"v","mountPath":"/data"}]}],"volumes":[{"name":"v","persistentVolumeClaim":{"claimName":"test-source"}}]}}'

# 3. Wait for completion
kubectl wait --for=condition=complete pod/writer --timeout=60s

# 4. Verify data exists
kubectl logs writer  # Should show "TEST DATA"

# 5. Take snapshot
kubectl apply -f - <<EOF
apiVersion: snapshot.storage.k8s.io/v1
kind: VolumeSnapshot
metadata:
  name: test-snap
  namespace: default
spec:
  volumeSnapshotClassName: csi-snapclass
  source:
    persistentVolumeClaimName: test-source
EOF

# 6. Wait for snapshot ready
kubectl wait --for=jsonpath='{.status.readyToUse}'=true volumesnapshot/test-snap --timeout=60s

# 7. Create clone PVC
kubectl apply -f - <<EOF
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: test-restored
  namespace: default
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 1Gi
  storageClassName: flint
  dataSource:
    name: test-snap
    kind: VolumeSnapshot
    apiGroup: snapshot.storage.k8s.io
EOF

# 8. Read from clone
kubectl run reader --image=busybox --restart=Never -- sh -c "
  ls -la /data/
  if [ -f /data/test.txt ]; then
    cat /data/test.txt
    echo 'SUCCESS: Data found in clone!'
  else
    echo 'ERROR: No data in clone!'
    exit 1
  fi
" --overrides='{"spec":{"containers":[{"name":"reader","volumeMounts":[{"name":"v","mountPath":"/data"}]}],"volumes":[{"name":"v","persistentVolumeClaim":{"claimName":"test-restored"}}]}}'

# 9. Check result
kubectl logs reader
```

## Questions to Answer

1. Does the source lvol actually contain the file after sync?
2. Does the snapshot lvol contain the data?
3. Does the clone lvol show the data when read directly via SPDK?
4. Does the ublk device for the clone show the data when read with dd?

## Next Steps

Run manual test above to isolate where data is lost.

