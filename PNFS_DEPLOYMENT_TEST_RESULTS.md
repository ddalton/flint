# pNFS Deployment and Performance Test Results

**Date**: December 18, 2025  
**Cluster**: cdrv (2-node Kubernetes cluster)  
**Test Objective**: Deploy pNFS with 2 Data Servers and compare performance with standalone NFS

---

## ✅ What Was Successfully Deployed

### 1. Infrastructure
- **Namespace**: `pnfs-test`
- **Kubernetes Cluster**: 2 nodes (cdrv-1, cdrv-2)
- **All Pods Running**: MDS, 2x DS, Standalone NFS, Test Client

### 2. Components Deployed

| Component | Image | Status | Node |
|-----------|-------|--------|------|
| pNFS MDS | docker-sandbox.infra.cloudera.com/ddalton/pnfs:latest | ✅ Running | cdrv-2 |
| pNFS DS #1 | docker-sandbox.infra.cloudera.com/ddalton/pnfs:latest | ✅ Running | cdrv-1 |
| pNFS DS #2 | docker-sandbox.infra.cloudera.com/ddalton/pnfs:latest | ✅ Running | cdrv-2 |
| Standalone NFS | docker-sandbox.infra.cloudera.com/ddalton/flint-driver:latest | ✅ Running | cdrv-1 |
| Test Client | ubuntu:24.04 | ✅ Running | cdrv-2 |

### 3. Services
- `pnfs-mds`: ClusterIP service on ports 2049 (NFS) and 50051 (gRPC)
- `standalone-nfs`: ClusterIP service on port 2049

---

## ⚠️ Issues Identified

### Issue #1: Device ID Environment Variable Not Substituted ❌

**Problem**: Both DS pods are registering with the MDS using the literal string `${NODE_NAME}-ds` instead of their actual node names.

**Evidence**:
```
MDS Status Report:
  Data Servers: 1 active / 1 total  ← Should be 2!
  Capacity: 1000000000000 bytes total

MDS logs show:
  Heartbeat received from device: ${NODE_NAME}-ds
  Heartbeat received from device: ${NODE_NAME}-ds
```

**Impact**: MDS thinks there's only 1 DS, so it cannot stripe data across 2 DSs. Performance will NOT double.

**Root Cause**: The config file `/etc/flint/pnfs.yaml` contains:
```yaml
deviceId: "${NODE_NAME}-ds"
```

But YAML doesn't perform environment variable substitution. The DS binary receives the literal string.

**Fix**: The DS binary needs to substitute the environment variable at runtime, OR we need to use `envsubst` to process the config file before starting the DS.

---

### Issue #2: pNFS Not Being Activated by Client ❌

**Problem**: When the NFS client mounts the MDS, pNFS is not being activated.

**Evidence**:
```
/proc/self/mountstats shows:
  pnfs=not configured
```

Should show:
```
  pnfs=LAYOUT_NFSV4_1_FILES
```

**Impact**: Client is using regular NFSv4.1 (not pNFS), so all I/O goes through the MDS instead of directly to DSs.

**Possible Causes**:
1. MDS may not be advertising pNFS capability during EXCHANGE_ID
2. Client may need specific mount options
3. MDS configuration may have pNFS disabled

**Investigation Needed**: Check MDS logs for EXCHANGE_ID flags and ensure the pNFS flag fix from NEXT_STEPS.md is applied.

---

## 📊 Performance Test Results (Preliminary)

### Test 1: pNFS MDS Write (100MB)

**Configuration**: NFS 4.1, direct I/O, 1M block size

**Results**:
- **Bandwidth**: 29.8 MiB/s (31.3 MB/s)
- **IOPS**: 29
- **Latency**: Average 33.5ms per operation
- **Duration**: 3.35 seconds

### Test 2-4: Not Completed

The test script failed after Test 1 due to mount issues (both `/mnt/pnfs` and `/mnt/standalone` were mounting to `pnfs-mds` instead of the standalone NFS service).

---

## 🔍 Analysis

### Why Performance Didn't Double

**Expected**: With 2 DSs and pNFS striping, we should see ~2x improvement over standalone NFS.

**Actual**: Only 1 DS is registered with MDS, and pNFS is not active on the client.

**Breakdown**:
1. ❌ Only 1 DS registered (both using same device ID)
2. ❌ pNFS not active on client (`pnfs=not configured`)
3. ✅ MDS is running and accepting connections
4. ✅ DS pods are heartbeating to MDS

**Result**: The deployment is functioning as a **regular NFS server through MDS proxy**, NOT as pNFS with parallel I/O.

---

## 🔧 Next Steps to Fix

### Step 1: Fix DS Device ID Substitution

**Option A: Runtime substitution in DS binary** (Preferred)
```rust
// In DS startup code:
let device_id = config.device_id.replace("${NODE_NAME}", &env::var("NODE_NAME")?);
```

**Option B: Use envsubst in container**
```dockerfile
CMD envsubst < /etc/flint/pnfs.yaml > /tmp/pnfs.yaml && \
    /usr/local/bin/flint-pnfs-ds --config /tmp/pnfs.yaml
```

**Option C: Use Kubernetes downward API to create per-node config**
```yaml
volumeMounts:
- name: config
  mountPath: /etc/flint/pnfs.yaml
  subPath: pnfs.yaml
```
With per-pod config generation.

---

### Step 2: Verify pNFS Flag Fix

**From NEXT_STEPS.md**: Ensure the MDS is setting the correct EXCHANGE_ID flags.

**Expected log message**:
```
🎯 EXCHANGE_ID: Modified flags for pNFS MDS
   Before: 0x00000001 (USE_NON_PNFS)
   After:  0x00000002 (USE_PNFS_MDS)
   ✅ Client will now request layouts and use pNFS!
```

**Check**:
```bash
kubectl logs -n pnfs-test -l app=pnfs-mds | grep "EXCHANGE_ID"
```

If not present, need to rebuild image with the pNFS flag fix from the feature branch.

---

### Step 3: Verify pNFS Layout Generation

Once pNFS is active, check for layout generation logs:

```bash
kubectl logs -n pnfs-test -l app=pnfs-mds | grep "Generated"
```

**Expected**:
```
🎯 Generated pNFS layout with 2 segments
   📊 Layout details:
      Segment 0: device=cdrv-1.vpc.cloudera.com-ds, offset=0, length=4194304
      Segment 1: device=cdrv-2.vpc.cloudera.com-ds, offset=4194304, length=...
```

---

### Step 4: Re-run Performance Tests

Once both issues are fixed:

```bash
# Rebuild with fixes
ssh root@cdrv-1.vpc.cloudera.com
cd /root/flint/spdk-csi-driver

# Apply fixes to code
# ... (device ID substitution + pNFS flags)

# Rebuild images
docker buildx build --platform linux/amd64 \
  -f docker/Dockerfile.pnfs \
  -t docker-sandbox.infra.cloudera.com/ddalton/pnfs:v2 \
  --push .

# Redeploy
kubectl delete namespace pnfs-test
kubectl apply -f /path/to/deployments/

# Wait for pods
kubectl wait --for=condition=ready pod -l app=pnfs-mds -n pnfs-test --timeout=120s

# Verify 2 DSs registered
kubectl logs -n pnfs-test -l app=pnfs-mds | grep "Status Report" -A4
# Should show: "Data Servers: 2 active / 2 total"

# Verify pNFS active
kubectl exec -n pnfs-test pnfs-client -- bash -c "
  mount -t nfs -o vers=4.1 pnfs-mds:/ /mnt/pnfs
  cat /proc/self/mountstats | grep 'pnfs='
"
# Should show: "pnfs=LAYOUT_NFSV4_1_FILES"

# Run performance tests
cd /Users/ddalton/projects/rust/flint/deployments
KUBECONFIG=/Users/ddalton/.kube/config.cdrv ./run-performance-tests.sh
```

---

## 📈 Expected Results (After Fixes)

### Performance Comparison

| Test | Standalone NFS | pNFS (1 DS) | pNFS (2 DS) | Improvement |
|------|----------------|-------------|-------------|-------------|
| Write (100MB) | ~94 MB/s | ~94 MB/s | **~180 MB/s** | **2×** |
| Read (100MB) | ~481 MB/s | ~481 MB/s | **~900 MB/s** | **2×** |
| Random Read | ~2535 IOPS | ~2535 IOPS | **~5000 IOPS** | **2×** |

---

## 🎯 Success Criteria

### ✅ Deployment Successful When:

1. **MDS Status Report shows**:
   ```
   Data Servers: 2 active / 2 total
   ```

2. **Client mountstats shows**:
   ```
   pnfs=LAYOUT_NFSV4_1_FILES
   ```

3. **MDS logs show**:
   ```
   🎯 Generated pNFS layout with 2 segments
   ```

4. **DS logs show I/O activity on both DSs**:
   ```
   DS-1: READ/WRITE operations
   DS-2: READ/WRITE operations
   ```

5. **Performance doubles** (or close to it):
   ```
   pNFS (2 DS) / Standalone ≈ 2.0
   ```

---

## 📁 Deployment Files Created

All files located in `/Users/ddalton/projects/rust/flint/deployments/`:

1. `pnfs-namespace.yaml` - Namespace definition
2. `pnfs-mds-config.yaml` - MDS configuration ConfigMap
3. `pnfs-ds-config.yaml` - DS configuration ConfigMap (needs device ID fix)
4. `pnfs-mds-deployment.yaml` - MDS Deployment and Service
5. `pnfs-ds-daemonset.yaml` - DS DaemonSet (runs on all nodes)
6. `pnfs-client-pod.yaml` - Test client pod
7. `standalone-nfs-deployment.yaml` - Standalone NFS for comparison
8. `deploy-all.sh` - Automated deployment script
9. `run-performance-tests.sh` - Performance comparison script (needs mount fix)

---

## 🚀 Quick Commands

### Check Status
```bash
export KUBECONFIG=/Users/ddalton/.kube/config.cdrv

# All pods
kubectl get pods -n pnfs-test -o wide

# MDS status
kubectl logs -n pnfs-test -l app=pnfs-mds | grep "Status Report" -A4

# DS heartbeats
kubectl logs -n pnfs-test -l app=pnfs-ds | grep -i heartbeat
```

### Get Logs
```bash
# MDS logs
kubectl logs -n pnfs-test -l app=pnfs-mds --tail=100

# DS logs
kubectl logs -n pnfs-test -l app=pnfs-ds --tail=50

# Standalone NFS logs
kubectl logs -n pnfs-test -l app=standalone-nfs --tail=50
```

### Cleanup
```bash
kubectl delete namespace pnfs-test
```

---

## 📊 Current Status Summary

| Component | Status | Notes |
|-----------|--------|-------|
| Kubernetes Deployment | ✅ Complete | All pods running |
| MDS Running | ✅ Working | Accepting connections |
| DS Running | ✅ Working | Both heartbeating |
| DS Registration | ⚠️ Partial | Only 1 of 2 registered |
| pNFS Activation | ❌ Not Working | Client shows "pnfs=not configured" |
| Performance Testing | ⚠️ Incomplete | Only 1 of 4 tests completed |
| 2x Performance | ❌ Not Achieved | Awaiting fixes |

---

## 🎓 Lessons Learned

1. **Environment variable substitution in YAML doesn't work**: Need runtime substitution or preprocessing.

2. **pNFS activation is a multi-step process**:
   - MDS must advertise pNFS capability (EXCHANGE_ID flags)
   - Client must request layouts (LAYOUTGET)
   - MDS must generate layouts
   - Client must use layouts (direct I/O to DSs)

3. **Monitoring is critical**:
   - Watch MDS status reports for DS count
   - Check client mountstats for pNFS activation
   - Verify layout generation in MDS logs
   - Confirm I/O on DS logs

4. **Testing infrastructure setup takes time**:
   - Package installation in Ubuntu containers is slow
   - NFS client tools and fio need to be pre-installed or cached

---

## 📝 Recommendations

### Immediate (Fix and Retest)
1. ✅ Fix DS device ID substitution (runtime or envsubst)
2. ✅ Verify/apply pNFS flag fix from feature branch
3. ✅ Rebuild images with fixes
4. ✅ Redeploy and verify 2 DSs registered
5. ✅ Verify pNFS activation on client
6. ✅ Re-run performance tests

### Short-term (Improve Testing)
1. Create pre-built test client image with tools installed
2. Add automated verification scripts
3. Add more detailed logging for debugging
4. Create Prometheus metrics dashboard

### Long-term (Production)
1. Add persistent storage for DS data
2. Implement SPDK backend for DS
3. Add RDMA support for client-DS communication
4. Implement MDS high availability (etcd backend)

---

**Status**: Deployment completed, issues identified, awaiting fixes and retesting.

**Next Action**: Fix device ID substitution and pNFS flags, then redeploy and retest.

