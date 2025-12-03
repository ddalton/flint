# Snapshot Test Investigation - Current Status

## Test Status: FAILING ❌

**Error**: Restored volume is empty (no files from snapshot)

## What We Know (Confirmed via SPDK)

### SPDK Level - Everything is Correct ✅

```bash
# Snapshot exists with data
Snapshot UUID: fe65bad4
- allocated: 1024 clusters (full 1GB of data)
- snapshot: true
- Has 2 clones referencing it

# Source volume (after snapshotting becomes a clone)
Source UUID: a4a66bca  
- clone: true
- base_snapshot: snap_pvc-f012c4a9..._1764790393
- allocated: 46 clusters (modifications after snapshot)

# Restored volume (clone from snapshot)
Restored UUID: 904651fe
- clone: true  
- base_snapshot: snap_pvc-f012c4a9..._1764790393 (SAME snapshot!)
- allocated: 39 clusters (Something wrote to it!)
```

**Key Insight**: Both source and restored volumes are thin clones of the snapshot (SPDK COW behavior).

## The Problem

**Restored clone has 39 allocated clusters** = Data was written/formatted!

But we expected:
- Clone detection → skip wipefs → preserve snapshot's filesystem
- Clone should have 0 allocated clusters (pure COW, no writes)

**The fact that 39 clusters are allocated means the clone was reformatted.**

## Possible Causes

### Theory 1: NodeStageVolume Not Being Called
- No NodeStageVolume logs found for restored volume in recent logs
- If staging doesn't happen, how did 39 clusters get allocated?

### Theory 2: Debug Logging Not in Running Image
- Added comprehensive logging in commit `c03bba7`
- Should log "SPDK bdev_get_bdevs response: {full JSON}"
- NOT seeing these logs → image may not have latest code

### Theory 3: NVMe-oF Access (Even with Local Scheduling)
- Earlier tests showed volumes accessed remotely via NVMe-oF
- NVMe bdev format (not lvol) prevents clone detection
- But current test has all pods on ublk-1 (should be local)

## Commits with Fixes

All on `uring` branch:

1. **35017e9** - Snapshot restore metadata (volume_context population)
2. **0bbd379** - ublk wipefs fix (clear stale cache)
3. **34821a5** - lvs_name extraction from alias
4. **d8f1232** - Clone detection from SPDK metadata
5. **c03bba7** - Debug logging for clone detection ← LATEST
6. **9b870ec** - Force pods to ublk-1 (test only)
7. **770676c** - Additional sync in test

## Required Actions

### 1. Verify Image Has Latest Code

```bash
# Check current commit
cd /Users/ddalton/projects/rust/flint
git log -1 --oneline
# Should show: c03bba7 Add comprehensive debug logging for clone detection

# Rebuild from latest
cargo build --release --bin csi-driver

# Push image (must include commit c03bba7)
```

### 2. Look for Debug Logs After Rebuild

After restarting pods with new image, look for:

```
🔍 [NODE] SPDK bdev_get_bdevs response: {
  ... full JSON ...
}
```

If you see this → new image is running
If you don't see this → old image still running

### 3. Check Which Volumes Are Being Staged

```bash
kubectl logs -n flint-system <node-pod> -c flint-csi-driver | grep "Staging volume pvc-"
```

Should see staging for:
- Source PVC (pvc-f012c4a9)
- Restored PVC (pvc-e8c2cce3)

### 4. Verify Clone Detection Works

Look for one of these messages:

**For clones (should preserve):**
```
📋 [NODE] SPDK metadata: clone=true, base_snapshot="snap_pvc-xxx"
✅ [NODE] This lvol is a snapshot clone - will preserve filesystem
[NO wipefs]
[NO formatting]
```

**For new volumes:**
```
🆕 [NODE] SPDK metadata: clone=false (new volume)
🧹 [NODE] New volume - clearing ublk device cache
🔧 [NODE] Formatting device
```

## Mystery to Solve

**Why does the restored clone have 39 allocated clusters?**

If NodeStageVolume was called and ran wipefs+format → that would allocate clusters ❌
If clone detection worked → no wipefs, no format → should have 0 allocated clusters ✅

The 39 allocated clusters suggests the clone WAS reformatted, but we can't find the logs showing this happened.

## Next Debug Steps

1. Confirm latest image (c03bba7) is running
2. Run test again
3. Check for "SPDK bdev_get_bdevs response" in logs
4. If present → analyze the JSON to see bdev structure
5. If not present → image doesn't have latest code, rebuild required

## Alternative: Manual SPDK Test

Test SPDK snapshot/clone directly to verify COW works:

```bash
# Connect to SPDK on ublk-1
kubectl exec -n flint-system flint-csi-node-5qwpf -c spdk-tgt -- sh

# Create test lvol
/usr/local/scripts/rpc.py bdev_lvol_create \
  lvs_ublk-1.vpc.cloudera.com_0000-00-1d-0 test_vol 100

# Write data via ublk
ublk_start_disk test_vol 999
mkfs.ext4 /dev/ublkb999
mount /dev/ublkb999 /mnt
echo "TEST DATA" > /mnt/test.txt
umount /mnt
ublk_stop_disk 999

# Create snapshot
/usr/local/scripts/rpc.py bdev_lvol_snapshot test_vol test_snap

# Clone snapshot  
/usr/local/scripts/rpc.py bdev_lvol_clone test_snap test_clone

# Read from clone
ublk_start_disk test_clone 998
mount /dev/ublkb998 /mnt
cat /mnt/test.txt  # Should show "TEST DATA"
```

If this works → SPDK is fine, issue is in CSI driver logic
If this fails → SPDK COW issue with ublk

---

# TROUBLESHOOTING UPDATE - December 3, 2025

## New Issue Discovered: CSI Socket Connection Failures

### Current Situation
When running the snapshot-restore test, pods fail to start with the following error:

```
Warning  FailedMount  kubelet  MountVolume.MountDevice failed for volume "pvc-9a10d427..." : 
rpc error: code = Unavailable desc = connection error: desc = "error reading server preface: 
read unix @->/csi/csi.sock: use of closed network connection"
```

### Investigation Results

**✅ CSI Driver Process Running:**
```bash
kubectl get pods -n flint-system
# flint-csi-node-5qwpf: 4/4 Running (18m age)

kubectl exec -n flint-system flint-csi-node-5qwpf -c flint-csi-driver -- ps aux
# root   1  /usr/local/bin/csi-driver  (running)

ls -la /csi/
# srwxr-xr-x csi.sock exists
```

**❌ NO CSI Requests Reaching Driver:**
```bash
kubectl logs flint-csi-node-5qwpf -c flint-csi-driver | grep "pvc-9a10d427"
# (empty - no logs for this volume)

kubectl logs flint-csi-node-5qwpf -c flint-csi-driver | grep "NodeStage"
# (empty - no staging requests)
```

**✅ CSI Node Registration OK:**
```bash
kubectl get csinode
# ublk-1.vpc.cloudera.com   1 driver   19d
```

### The Problem

The CSI socket exists and the driver process is running, but kubelet is getting "use of closed network connection" errors when trying to communicate with it. The CSI driver logs show ONLY HTTP API requests (disk discovery), but NO gRPC CSI requests (NodeStageVolume, NodePublishVolume, etc.).

### Possible Root Causes

1. **CSI gRPC server not starting properly** - The main CSI driver binary might not be starting its gRPC server
2. **Socket permissions issue** - Unix socket might not be accessible to kubelet
3. **Driver crashed/restarted** - Pods show 18m age (restarted around when test started?)
4. **Binary issue** - Latest code with clone detection might have broken CSI server initialization

### Next Steps

1. **Check driver binary startup** - Look for CSI server initialization in logs:
   ```bash
   kubectl logs flint-csi-node-5qwpf -c flint-csi-driver | head -200 | grep -i "grpc\|server\|starting"
   ```

2. **Check socket connectivity from host** - Verify socket is actually listening:
   ```bash
   kubectl exec flint-csi-node-5qwpf -c flint-csi-driver -- netstat -l | grep csi
   ```

3. **Review main.rs** - Check if CSI server initialization was accidentally broken in recent commits

4. **Test with simple volume** - Try creating a non-snapshot volume to see if basic CSI operations work

5. **Restart CSI driver** - Force restart and watch startup logs:
   ```bash
   kubectl delete pod flint-csi-node-5qwpf -n flint-system
   kubectl logs -f flint-csi-node-5qwpf -c flint-csi-driver
   ```

### Critical Question

**Has the snapshot test EVER worked on this cluster?** If not, this might be a pre-existing CSI driver issue unrelated to the clone detection code.

---

##  UPDATE: After CSI Driver Restart

### Test Now Runs But FAILS with Empty Restored Volume

After restarting the CSI driver pods (`kubectl delete pod flint-csi-node-5qwpf`), the test progresses much further:

**✅ Volumes Mount Successfully:**
```bash
kubectl get pods -n kuttl-test-suitable-halibut
# initial-data-writer:  Completed
# data-modifier:        Completed
# snapshot-verifier:    Error (exit 1)
```

**❌ Verification Fails - File Missing:**
```bash
kubectl logs snapshot-verifier
# Verifying snapshot data...
# cat: can't open '/data/snapshot-test.txt': No such file or directory
# ERROR: Original snapshot data not found!
```

**This Confirms the Original Issue:** The restored volume from snapshot is EMPTY - the filesystem was reformatted instead of being preserved as a clone.

### Mystery: NO CSI Staging Logs

Despite volumes being successfully mounted, there are ZERO logs for NodeStageVolume/NodePublishVolume:

```bash
kubectl logs flint-csi-node-5qwpf -c flint-csi-driver | grep "pvc-b8625840"
# (empty)

kubectl logs flint-csi-node-5qwpf -c flint-csi-driver | grep "Staging volume"
# (empty)

kubectl logs flint-csi-node-5qwpf -c flint-csi-driver | grep "This lvol is a snapshot clone"
# (empty)
```

The CSI driver logs show ONLY HTTP API requests (dashboard backend on port 8081), but NO gRPC CSI operations.

### Next Investigation Steps

1. **Check if println! logs are being captured** - The main.rs uses `println!()` for logging. Verify these are going to stdout/container logs.

2. **Add explicit logging initialization** - Consider adding env_logger or tracing to ensure logs are captured:
   ```rust
   env_logger::init();
   tracing_subscriber::fmt::init();
   ```

3. **Check if CSI gRPC server is actually handling requests** - The socket exists and kubelet can connect (volumes mount), but we see no logs. Possible issues:
   - Logs are going somewhere else (stderr, file)
   - Buffering issue (logs not flushed)
   - Binary mismatch (old binary without new logging)

4. **Verify image has latest code** - Check if the running image actually contains commit c03bba7:
   ```bash
   kubectl exec flint-csi-node-5qwpf -c flint-csi-driver -- /usr/local/bin/csi-driver --version
   # Should show commit hash or version info
   ```

5. **Manual test with debug binary** - Build and deploy a version with extra debug output to see what's happening during NodeStageVolume.

---

## CRITICAL FINDING: Local vs NVMe-oF Access

### Question: Is the behavior different for local vs NVMe-oF access?

**Answer: Volumes ARE being accessed LOCALLY (as lvols), NOT via NVMe-oF** ✅

```bash
kubectl exec flint-csi-node-wqgzz -c spdk-tgt -- \
  /usr/local/scripts/rpc.py bdev_get_bdevs | jq '.[] | select(.aliases[] | contains("pvc-b8625840"))'
```

**Result:**
```json
{
  "name": "dc2684b4-5e43-466d-9fe0-2a8d4660b130",
  "product_name": "Logical Volume",  // ← Local lvol, NOT "NVMe bdev"!
  "aliases": ["lvs_ublk-1.vpc.cloudera.com_0000-00-1d-0/vol_pvc-b8625840..."],
  "driver_specific": {
    "lvol": {
      "clone": true,  // ← SPDK knows it's a clone!
      "base_snapshot": "snap_pvc-c9982284-a347-4e91-823c-6a1b4ad427b5_1764791854",
      "num_allocated_clusters": 39  // ← But it was FORMATTED (should be 0!)
    }
  }
}
```

### What This Proves

✅ **NVMe-oF is NOT the problem:**
- Restored volume is a `Logical Volume` (lvol), not an `NVMe bdev`
- Would be `product_name: "NVMe bdev"` if accessed via NVMe-oF
- All pods scheduled to ublk-1 (same node as storage)

✅ **SPDK has correct metadata:**
- `clone: true` ✓
- `base_snapshot` field populated ✓  
- Listed in snapshot's `clones` array ✓

❌ **But volume was REFORMATTED anyway:**
- `num_allocated_clusters: 39` (should be 0 for pure COW clone)
- 39 clusters = wipefs + mkfs.ext4 wrote to the volume
- This means clone detection logic did NOT skip formatting

### The Real Problem

**The CSI driver reformatted a clone that SPDK correctly identified.**

This can only happen if:

1. **Clone detection code didn't run** - NodeStageVolume wasn't called, or the bdev query failed
2. **Clone detection failed** - Code ran but didn't extract `clone: true` from SPDK response
3. **Wrong code path** - Volume was formatted through a different path (ControllerPublishVolume?)
4. **Binary doesn't have latest code** - The running image predates the clone detection commits

Since we have **ZERO logs** for NodeStageVolume, the most likely cause is #1 or #4.

### Next Actions

1. **Verify running binary has clone detection code:**
   ```bash
   # Check if image was built after commit c03bba7
   kubectl describe pod flint-csi-node-wqgzz | grep Image:
   # docker-sandbox.infra.cloudera.com/ddalton/flint-driver:latest
   # Tag: cb8677f6730c... (need to verify this includes c03bba7)
   ```

2. **Force image rebuild and redeploy:**
   ```bash
   cd spdk-csi-driver
   cargo build --release
   # Build and push new image with clone detection code
   # Update deployment to pull new image
   ```

3. **Add startup logging to verify code version:**
   ```rust
   println!("🔧 [STARTUP] Clone detection enabled (commit c03bba7)");
   ```

---

## ENHANCED LOGGING ADDED

### Comprehensive Debug Logging Now in Place

Added extensive `eprintln!()` logging (goes to stderr for better capture) with visual delimiters throughout the critical code paths:

**1. Server Startup Logging:**
```
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
✅ [CSI_SERVER] Minimal State SPDK CSI Driver starting
   Mode: node
   Endpoint: unix:///csi/csi.sock
   Node ID: ublk-1.vpc.cloudera.com
   Clone Detection: ENABLED (commit c03bba7)
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
✅ [CSI_SERVER] CSI gRPC server listening on: /csi/csi.sock
   Waiting for CSI requests from kubelet...
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
```

**2. NodeStageVolume Entry:**
```
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
🔵 [GRPC] *** NodeStageVolume CALLED ***
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
📦 [NODE_STAGE] Volume ID: pvc-xxx
📦 [NODE_STAGE] Staging path: /var/lib/kubelet/...
📦 [NODE_STAGE] Publish context keys: [...]
```

**3. Clone Detection Flow:**
```
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
🔍 [CLONE_DETECTION] Starting clone detection for bdev: xxx
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
🔍 [CLONE_DETECTION] Calling SPDK RPC: bdev_get_bdevs(xxx)
✅ [CLONE_DETECTION] SPDK RPC call succeeded
🔍 [CLONE_DETECTION] SPDK bdev_get_bdevs response:
{... full JSON ...}
✅ [CLONE_DETECTION] Response is an array with 1 elements
✅ [CLONE_DETECTION] Got bdev from array
   product_name: Logical Volume
   has_lvol_metadata: true
✅ [CLONE_DETECTION] This IS an lvol - checking clone field
   clone field: true/false
   base_snapshot: "snap_xxx" or None
   num_allocated_clusters: 39
```

**4. Clone Detection Result:**
```
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
✅ [CLONE_DETECTION] CLONE DETECTED!
   base_snapshot: "snap_pvc-xxx"
   RESULT: is_clone = TRUE (will PRESERVE filesystem)
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
```

OR

```
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
🆕 [CLONE_DETECTION] NOT A CLONE (new volume)
   RESULT: is_clone = FALSE (will format)
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
```

**5. Formatting Decision:**
```
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
📊 [FORMATTING_DECISION] is_clone = true/false
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
✅ [FORMATTING_DECISION] SKIPPING wipefs and format
   Reason: Volume is a snapshot clone
   Action: Preserving existing filesystem with snapshot data
```

OR

```
🧹 [FORMATTING_DECISION] RUNNING wipefs and will format if needed
   Reason: Volume is new (not a clone)
   Action: Clearing ublk device cache before checking filesystem
```

**6. NodePublishVolume Entry:**
```
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
🔵 [GRPC] *** NodePublishVolume CALLED ***
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
📦 [NODE_PUBLISH] Volume ID: pvc-xxx
📦 [NODE_PUBLISH] Target path: /var/lib/kubelet/pods/.../volumes/...
```

### What to Look For After Rebuild

After rebuilding and redeploying the image, check logs:

```bash
# 1. Verify server started with clone detection enabled
kubectl logs -n flint-system <node-pod> -c flint-csi-driver 2>&1 | head -20

# Should see:
#   ✅ [CSI_SERVER] ... Clone Detection: ENABLED (commit c03bba7)
#   ✅ [CSI_SERVER] CSI gRPC server listening on: /csi/csi.sock

# 2. Watch logs during test
kubectl logs -n flint-system <node-pod> -c flint-csi-driver -f 2>&1 | grep -E "GRPC|CLONE_DETECTION|FORMATTING_DECISION"

# 3. After test failure, check for clone detection
kubectl logs -n flint-system <node-pod> -c flint-csi-driver 2>&1 | grep -A20 "CLONE_DETECTION"
```

### Expected Output for Restored Volume

When the restored PVC is staged, we should see:

```
🔵 [GRPC] *** NodeStageVolume CALLED ***
📦 [NODE_STAGE] Volume ID: pvc-b8625840-941e-43bb-aadb-2e3ecedd0ef7
...
✅ [CLONE_DETECTION] CLONE DETECTED!
   base_snapshot: "snap_pvc-c9982284-a347-4e91-823c-6a1b4ad427b5_1764791854"
   RESULT: is_clone = TRUE (will PRESERVE filesystem)
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
✅ [FORMATTING_DECISION] SKIPPING wipefs and format
   Reason: Volume is a snapshot clone
```

If we DON'T see these logs, or if we see "NOT A CLONE", then we've found the problem!

---

## 🎯 ROOT CAUSE FOUND!

### Bug: SPDK Response Parsing Error

**Symptom:**
```
❌ [CLONE_DETECTION] ERROR: SPDK response is not an array!
```

**The Problem:**

The SPDK RPC response has this structure:
```json
{
  "result": [
    { ... bdev data ... }
  ]
}
```

But the code was trying to parse `response` directly as an array:
```rust
if let Some(bdev_array) = response.as_array() {  // ❌ FAILS!
```

The response is an **OBJECT** with a `"result"` field, not an array!

**The Fix:**

Extract the `"result"` field first:
```rust
let bdev_array = response.get("result")
    .and_then(|r| r.as_array());

if let Some(bdev_array) = bdev_array {  // ✅ WORKS!
```

**Impact:**

- Clone detection ALWAYS failed
- Every restored volume was treated as new
- Wipefs ran on every clone (clearing COW filesystem)
- Volume was reformatted (explaining the 39 allocated clusters)

**Status:** Fixed in commit 84566a1 (partial), but this fix was incomplete.

---

## 🎯 SECOND BUG FOUND: Wrong Bdev from Array

### The Enhanced Logs Revealed Another Issue

After fixing the first bug, logs showed:
```
✅ [CLONE_DETECTION] Response.result is an array with 24 elements
✅ [CLONE_DETECTION] Got bdev from array
⚠️ [CLONE_DETECTION] NOT AN LVOL!  ← WRONG!
```

### The Problem

When we query SPDK: `bdev_get_bdevs(71b120de-7c7e-4fbf-be34-8ac53ff7df0e)`

SPDK returns **ALL 24 bdevs** in the system, NOT just the one we asked for!

The code was doing:
```rust
if let Some(bdev) = bdev_array.first() {  // ❌ Takes first bdev (uring_nvme0n1)
```

Since the first bdev in the array is always `uring_nvme0n1` (URING bdev, not an lvol), the clone detection would fail with "NOT AN LVOL".

### The Fix (Commit 63a4620)

Search through the array to find the matching bdev:
```rust
let target_bdev = bdev_array.iter().find(|b| {
    // Match by name (UUID)
    if let Some(name) = b.get("name").and_then(|n| n.as_str()) {
        if name == bdev_name {
            return true;
        }
    }
    // Also check aliases
    if let Some(aliases) = b.get("aliases").and_then(|a| a.as_array()) {
        for alias in aliases {
            if let Some(alias_str) = alias.as_str() {
                if alias_str.contains(bdev_name) || bdev_name.contains(alias_str) {
                    return true;
                }
            }
        }
    }
    false
});
```

### Complete Fix Chain

1. ✅ **Commit 84566a1**: Fixed `response.as_array()` → `response["result"].as_array()`
2. ✅ **Commit 63a4620**: Fixed `.first()` → `.find(|b| b.name == bdev_name)`

**Status:** Both bugs fixed. Ready for rebuild and test!

---

## 🎯 THIRD BUG FOUND: Node Agent Ignoring SPDK Params

### Enhanced Logging Revealed the Real Issue

After fixing the first two bugs and adding comprehensive logging, we discovered:

**Line 861 in minimal_disk_service.rs:**
```rust
"bdev_get_bdevs" => {
    // Use generic RPC call to get full bdev objects, not just names
    spdk.call_method("bdev_get_bdevs", None).await  // ❌ Hardcoded None!
```

**The Problem:**

The node agent HTTP proxy extracts params from the controller's request:
```json
{
  "method": "bdev_get_bdevs",
  "params": {"name": "71b120de-7c7e-4fbf-be34-8ac53ff7df0e"}
}
```

But then **throws away the params** and calls SPDK with `None`!

**Verification:**

Direct SPDK test confirms filtering works when params are passed:
```bash
# With name param: returns 1 bdev
/usr/local/scripts/rpc.py bdev_get_bdevs -b 71b120de... | jq 'length'
# 1

# Without name param: returns all bdevs  
/usr/local/scripts/rpc.py bdev_get_bdevs | jq 'length'
# 24
```

**The Fix (Commit 63a4620+):**

1. Extract params from incoming request
2. Forward them to SPDK
3. Add comprehensive logging to monitor:
   - What params are received
   - What's sent to SPDK (via existing "🔧 [SPDK_RPC] Sending:" log)
   - Whether filtering worked (result array length)
   - Monitor other RPC methods for regressions

**Regression Safety:**

Added monitoring for other SPDK RPC methods:
- `bdev_lvol_get_lvstores` - no params, should still work ✓
- `bdev_nvme_get_controllers` - no params, should still work ✓
- `bdev_lvol_create` - manually extracts params from rpc_request, unaffected ✓

**Expected Logs After Fix:**
```
🔧 [SPDK_PARAMS] Method: bdev_get_bdevs
   Params from request: {"name": "71b120de-7c7e-4fbf-be34-8ac53ff7df0e"}
   Will forward to SPDK: YES
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
🔧 [SPDK_RPC] Sending: {"jsonrpc":"2.0","method":"bdev_get_bdevs","params":{"name":"71b120de..."},"id":1}
📥 [SPDK_RPC] Received: {"jsonrpc":"2.0","id":1,"result":[{...}]}  ← Only 1 bdev!
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
✅ [SPDK_FIX] bdev_get_bdevs returned 1 bdev(s)
   Requested: name=71b120de-7c7e-4fbf-be34-8ac53ff7df0e
   Expected: 1 bdev
   Actual: 1 bdev(s)
   ✅ FILTERING WORKED!
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
```

### Complete Fix Summary

Three bugs in clone detection chain:

1. ✅ **Commit 223394e**: Parse `response["result"]` not `response` directly
2. ✅ **Commit 63a4620**: Search for matching bdev instead of using `.first()`  
3. ✅ **Commit 63a4620+**: Forward params to SPDK (was hardcoded `None`)

All three bugs prevented clone detection from working. With all fixes in place, snapshot restore should work correctly!

