# Testing: Disable UBLK_F_PER_IO_DAEMON Workaround

**Goal**: Test if kernel 6.17+ still supports legacy per-queue daemon mode by NOT setting the `UBLK_F_PER_IO_DAEMON` flag.

**Status**: Experimental - needs testing on kernel 6.17+

---

## The Workaround

Instead of enabling the new per-IO daemon mode, we explicitly disable it to force SPDK to use the old per-queue daemon model that it was designed for.

### What Changed

**Before (with ublk-per-io-daemon.patch):**
```c
if (g_ublk_tgt.per_io_daemon) {
    SPDK_NOTICELOG("Kernel supports UBLK_F_PER_IO_DAEMON - enabling flag\n");
    uinfo.flags |= UBLK_F_PER_IO_DAEMON;  // ✅ Set the flag
}
```

**After (with disable-per-io-daemon.patch):**
```c
if (g_ublk_tgt.per_io_daemon) {
    SPDK_NOTICELOG("Kernel supports UBLK_F_PER_IO_DAEMON - but DISABLED for compatibility\n");
    SPDK_NOTICELOG("Using legacy per-queue daemon mode\n");
    // ❌ Do NOT set the flag - use legacy mode
}
```

---

## Why This Might Work

According to kernel documentation and the per-IO daemon patches, the flag is **optional**:

> "If this feature is not supported by the driver, daemons must be per-queue instead"

This suggests the kernel should fall back to per-queue mode if the flag is not set, even on kernel 6.17+.

### Two Possible Outcomes

#### Outcome 1: ✅ Success (Kernel Supports Legacy Mode)
```
ublk_start_dev() succeeds
→ SPDK uses per-queue daemon model
→ Everything works on kernel 6.17+
→ No code changes needed to SPDK threading!
```

#### Outcome 2: ❌ Failure (Kernel Mandates Per-IO Mode)
```
ublk_start_dev() fails with EINVAL
→ Kernel 6.17+ requires per-IO daemon
→ Must use Rust approach or fix SPDK threading
```

---

## Testing Procedure

### Step 1: Build SPDK with the Workaround Patch

```bash
cd /path/to/spdk

# Remove the old patch (if applied)
git checkout lib/ublk/ublk.c

# Apply the new disable patch
patch -p1 < /Users/ddalton/github/flint/spdk-csi-driver/disable-per-io-daemon.patch

# Verify the patch applied
grep -A 10 "WORKAROUND: Disable UBLK_F_PER_IO_DAEMON" lib/ublk/ublk.c

# Rebuild SPDK
./configure --with-ublk --with-rdma
make clean
make -j$(nproc)
```

### Step 2: Rebuild Docker Image

```bash
cd /Users/ddalton/github/flint/spdk-csi-driver

# Update Dockerfile to use the new patch
# Edit docker/Dockerfile.spdk_simple

# Build new image
docker build -f docker/Dockerfile.spdk_simple -t flint/spdk-test:disable-per-io .
```

### Step 3: Test on Kubernetes Cluster

```bash
# Check kernel version on nodes
KUBECONFIG=/Users/ddalton/.kube/flint.yaml kubectl get nodes -o wide

# Deploy the test image
KUBECONFIG=/Users/ddalton/.kube/flint.yaml kubectl set image \
  daemonset/flint-node -n flint-system \
  spdk-target=flint/spdk-test:disable-per-io

# Watch pod restart
KUBECONFIG=/Users/ddalton/.kube/flint.yaml kubectl get pods -n flint-system -w

# Check logs for the specific message
KUBECONFIG=/Users/ddalton/.kube/flint.yaml kubectl logs -n flint-system \
  <pod-name> -c spdk-target | grep -A 5 "per-queue daemon"
```

### Step 4: Create a Test Volume

```bash
# Create a test PVC
cat <<EOF | KUBECONFIG=/Users/ddalton/.kube/flint.yaml kubectl apply -f -
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: test-ublk-workaround
  namespace: default
spec:
  accessModes:
    - ReadWriteOnce
  resources:
    requests:
      storage: 1Gi
  storageClassName: flint
EOF

# Watch for errors
KUBECONFIG=/Users/ddalton/.kube/flint.yaml kubectl describe pvc test-ublk-workaround

# If PVC is Bound, create a test pod
cat <<EOF | KUBECONFIG=/Users/ddalton/.kube/flint.yaml kubectl apply -f -
apiVersion: v1
kind: Pod
metadata:
  name: test-ublk-io
  namespace: default
spec:
  containers:
  - name: test
    image: ubuntu:24.04
    command: ["sh", "-c", "while true; do dd if=/dev/zero of=/data/test bs=1M count=100; sync; sleep 5; done"]
    volumeMounts:
    - name: data
      mountPath: /data
  volumes:
  - name: data
    persistentVolumeClaim:
      claimName: test-ublk-workaround
EOF

# Monitor the pod
KUBECONFIG=/Users/ddalton/.kube/flint.yaml kubectl logs -f test-ublk-io
```

### Step 5: Check for Errors

```bash
# Look for ublk_start_disk errors in SPDK logs
KUBECONFIG=/Users/ddalton/.kube/flint.yaml kubectl logs -n flint-system \
  <pod-name> -c spdk-target | grep -i "ublk_start_disk\|EINVAL\|start dev"

# Check kernel logs on the node
KUBECONFIG=/Users/ddalton/.kube/flint.yaml kubectl debug node/<node-name> -it \
  --image=ubuntu:24.04 -- dmesg | grep -i ublk
```

---

## Expected Results

### ✅ If Workaround Works

**SPDK logs:**
```
[2025-12-22 10:00:00.123456] ublk.c: 568:ublk_get_features: *NOTICE*: Kernel supports UBLK_F_PER_IO_DAEMON - but DISABLED for compatibility
[2025-12-22 10:00:00.123457] ublk.c: 569:ublk_get_features: *NOTICE*: Using legacy per-queue daemon mode
[2025-12-22 10:00:01.234567] ublk.c:2005:ublk_start_dev: *NOTICE*: Starting ublk device 0
[2025-12-22 10:00:01.234568] ublk.c:2005:ublk_start_dev: Device started successfully
```

**Test pod:**
```
100+0 records in
100+0 records out
104857600 bytes (105 MB, 100 MiB) copied, 0.5 s, 210 MB/s
```

**Conclusion:** Kernel 6.17+ still supports legacy per-queue mode! ✅

---

### ❌ If Workaround Fails

**SPDK logs:**
```
[2025-12-22 10:00:01.234567] ublk.c:2005:ublk_ctrl_cmd_submit: *ERROR*: start dev 0 failed, rc Invalid argument
[2025-12-22 10:00:01.234568] ublk.c:2007:ublk_start_dev: *ERROR*: ublk_ctrl_cmd_submit failed
```

**Kernel dmesg:**
```
[12345.678901] ublk: per-IO daemon mode required but not set
[12345.678902] ublk: ublk_ctrl_start_dev failed: -EINVAL
```

**Conclusion:** Kernel 6.17+ mandates per-IO daemon mode. Must use different approach. ❌

---

## Updated Dockerfile

Here's the updated Dockerfile using the disable patch:

```dockerfile
# In docker/Dockerfile.spdk_simple

# Copy patches
COPY lvol-flush.patch /tmp/
COPY ublk-debug.patch /tmp/
COPY blob-recovery-optimized.patch /tmp/
COPY blob-shutdown-debug.patch /tmp/
# COPY ublk-per-io-daemon.patch /tmp/  # ❌ Remove old patch
COPY disable-per-io-daemon.patch /tmp/   # ✅ Use new patch

# Clone and build SPDK
RUN git clone https://github.com/spdk/spdk.git . && \
    git checkout v25.09.x && \
    git submodule update --init --recursive && \
    # ... other patches ...
    patch -p1 < /tmp/blob-shutdown-debug.patch && \
    echo "✅ Blobstore shutdown debug logging patch applied" && \
    # Apply disable per-IO daemon patch instead
    patch -p1 < /tmp/disable-per-io-daemon.patch && \
    echo "✅ UBLK_F_PER_IO_DAEMON DISABLED (legacy mode workaround)" && \
    rm /tmp/*.patch && \
    # ... continue build ...
```

---

## Decision Tree

```
Test the Workaround
    │
    ▼
Does ublk_start_dev() succeed?
    │
    ├─ YES ✅
    │  │
    │  ├─ Great! Keep using this approach
    │  ├─ No code changes needed
    │  ├─ Works on kernel 6.17+
    │  └─ Production ready immediately
    │
    └─ NO ❌
       │
       ├─ Kernel mandates per-IO daemon
       │
       └─ Choose next approach:
          │
          ├─ Option A: Fix SPDK threading (complex, ~4-6 weeks)
          │  └─ Restructure io_uring creation to worker threads
          │
          ├─ Option B: Hybrid SPDK/Rust (medium, ~12 weeks)
          │  └─ Use Rust rublk + SPDK for NVMe-oF only
          │
          └─ Option C: Pure Rust + nvmet (simple, ~10 weeks)
             └─ Full Rust stack as designed above
```

---

## What to Watch For

### 1. SPDK Startup Logs

Look for these specific messages:
```
UBLK_F_PER_IO_DAEMON - but DISABLED for compatibility
Using legacy per-queue daemon mode
```

### 2. Volume Creation Success

If volumes can be created and mounted, the workaround succeeded.

### 3. I/O Performance

Even if it works, check latency:
```bash
# Run fio benchmark
fio --name=test --filename=/data/test --size=1G \
    --rw=randread --bs=4k --iodepth=1 --numjobs=1 \
    --time_based --runtime=60 --group_reporting
```

Look for p99 latency < 100μs

---

## Risk Assessment

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| **Kernel rejects legacy mode** | Medium | High | Have Rust approach ready as backup |
| **Performance degradation** | Low | Medium | Benchmark before/after |
| **Future kernel breaks it** | Medium | High | Monitor kernel changes, plan migration |
| **Different behavior across kernel versions** | Medium | Medium | Test on 6.6, 6.17, 6.18 |

---

## Timeline

- **Day 1**: Apply patch, build, deploy
- **Day 2**: Test volume creation and I/O
- **Day 3**: Performance testing
- **Decision point**: Continue with workaround OR plan Rust migration

**Fastest possible fix**: Could be working in 3 days if kernel supports legacy mode!

---

## Rollback Plan

If the workaround doesn't work:

```bash
# 1. Revert to previous image
KUBECONFIG=/Users/ddalton/.kube/flint.yaml kubectl rollout undo \
  daemonset/flint-node -n flint-system

# 2. Or keep using kernel 6.6 nodes
# Pin pods to older kernel nodes using nodeSelector

# 3. Plan for Rust migration
# Use the RUST_NVMET_DESIGN.md as blueprint
```

---

## Next Steps

### If Workaround Succeeds ✅

1. **Document the limitation** in production docs
2. **Monitor kernel changes** for future releases
3. **Plan eventual migration** to proper solution (Rust)
4. **Celebrate** 🎉 - you bought time!

### If Workaround Fails ❌

1. **Confirm kernel version** and test environment
2. **Review kernel logs** for specific error
3. **Choose next approach**:
   - Rust + nvmet (recommended - 10 weeks)
   - Fix SPDK threading (complex - 6 weeks)
   - Hybrid SPDK/Rust (12 weeks)

---

## Recommendation

**Try this workaround FIRST** because:

1. ✅ **Minimal effort** - just a patch
2. ✅ **Fast to test** - 1-3 days
3. ✅ **No architectural changes** if it works
4. ✅ **Buys time** to properly plan Rust migration
5. ✅ **Low risk** - easy to rollback

Even if it doesn't work, you've only spent a few days and gained valuable information about kernel behavior.

**Start testing now!** 🚀
