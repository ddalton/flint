# Memory Disk Cleanup Procedure

## ⚠️ CRITICAL: Memory Disks are Ephemeral

**Memory disks (malloc bdevs) are NOT suitable for persistent volumes!**

- They are stored in RAM
- All data is **lost when SPDK restarts**
- Restarting CSI pods destroys all memory disks
- Existing PVCs will fail (backing storage gone)

## Safe Cleanup Before Restarting CSI Pods

### Step 1: Delete All Pods Using Memory Disk Volumes

```bash
export KUBECONFIG=/Users/ddalton/.kube/config.flnt

# List pods using PVCs on memory disks
kubectl get pods -n default

# Delete the pods
kubectl delete pod test-memory-pod -n default
```

### Step 2: Delete PVCs on Memory Disks

```bash
# Delete the PVC (will trigger volume deletion)
kubectl delete pvc test-memory-512m -n default
```

### Step 3: Wait for PVC/PV Cleanup

```bash
# Monitor deletion (should complete cleanly since SPDK is still running)
kubectl get pvc,pv -A | grep memory

# Wait until both PVC and PV are gone
```

### Step 4: Now Safe to Restart CSI Pods

```bash
# Restart CSI node pods (memory disk will be recreated empty)
kubectl delete pod -n flint-system -l app=flint-csi-node

# Wait for pods to restart
kubectl get pods -n flint-system -l app=flint-csi-node -w
```

### Step 5: Restart Dashboard (if needed)

```bash
# Restart dashboard to pick up new CSI driver code
kubectl delete pod -n flint-system -l app=spdk-dashboard
```

## Alternative: Test with Fresh Memory Disk

After cleanup and restart:

1. Memory disk will be recreated (empty, 4GB)
2. You can create new test volumes
3. But remember: **All data lost on next restart!**

## Production Recommendation

**DO NOT use memory disks for production workloads!**

Memory disks are only suitable for:
- ✅ Temporary testing
- ✅ Benchmarking (performance testing)
- ✅ CI/CD ephemeral environments
- ❌ Any data that needs to persist

For production:
- Use physical NVMe/SSD disks
- Use networked storage (if needed)
- Never use malloc bdevs for persistent data

## What About Automatic Failure Detection?

Currently, orphaned PVs (whose backing storage disappeared) are not automatically detected.

Potential improvements:
1. Health check endpoint that validates lvol existence
2. Controller reconciliation loop to mark PVs as "Failed" when lvol is gone
3. Admission webhook to prevent PVC creation on memory disks (opt-in)
4. Warnings in UI when using ephemeral storage

This is a known limitation of memory-backed storage.
