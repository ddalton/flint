# ROX Test Issues - Next Steps

**Date**: 2025-12-15  
**Status**: ROX test failing due to NVMe-oF idempotency issue  
**Branch**: `feature/rwx-nfs-support`

## Current State

### ✅ Completed
1. **Documentation consolidation** - All docs merged into FLINT_CSI_ARCHITECTURE.md with NFS/RWX section
2. **Frontend CRD removal** - Dashboard works in minimal state mode
3. **System disk detection** - Partition-based heuristic (disks with partitions = system disks)
4. **Blobstore recovery logging** - Detailed progress visibility (every 10k pages)
5. **ROX NVMe-oF fix** - Always use NVMe-oF for ROX volumes (commit 66b7c5c)
6. **ublk idempotency** - Check if device exists before creating (commit f5104c9)
7. **LVS recovery on cdrv-2** - Completed successfully (10 minutes, 1M pages scanned)

### ❌ Current Issue: ROX Test Failing

**Test**: `rox-multi-pod` (ReadOnlyMany multi-pod access)  
**Symptom**: reader-pod-2 stuck in ContainerCreating  
**Error**: `ublk_start_disk failed: Code=-19 Msg=No such device`

## Root Cause Analysis

### The Problem

When `bdev_nvme_attach_controller` returns error **Code=-114** ("controller already exists"), the CSI driver assumes the connection is working and the bdev exists. However:

**Error -114 can mean TWO different things:**
1. ✅ Controller exists AND is connected (bdev available)
2. ❌ Controller exists but is in FAILED state (bdev NOT available)

**Current code treats both cases as success**, leading to:

```
Step 1: First attach attempt fails (connection refused, timeout, etc)
   → Controller registered in SPDK but in FAILED state
   → No bdev created

Step 2: Retry sees "controller already exists" (error -114)
   → CSI driver: "✅ Already connected!" (WRONG)
   → Assumes bdev exists: nvme_nqn_..._pvc-XXXn1
   
Step 3: Try to create ublk device
   → ublk_start_disk("nvme_nqn_..._pvc-XXXn1")
   → SPDK: ❌ "No such device" (bdev doesn't exist!)
```

### Evidence from Logs

**Location**: CSI driver logs on cdrv-1 (flint-csi-node-mhzqp)

```
❌ SPDK RPC call 'bdev_nvme_attach_controller' failed: 
   Code=-114 Msg=A controller named ... already exists

ℹ️ [DRIVER] Already connected to NVMe-oF target
✅ [NODE] Connected to NVMe-oF target, bdev: nvme_nqn_...n1

🔧 Creating ublk device for bdev: nvme_nqn_...n1
❌ ublk_start_disk failed: Code=-19 Msg=No such device
```

**SPDK target logs** (spdk-tgt container):
```
[ERROR] Failed to flush (111): Connection refused
[ERROR] controller reinitialization failed
[ERROR] in failed state
[INFO] ctrlr could not be connected
[ERROR] Resetting controller failed
```

**bdev query** shows NO nvme_nqn bdevs exist (only local lvols and uring bdevs).

## The Fix Required

### Location
`spdk-csi-driver/src/main.rs` - NodeStageVolume function (around line 1433-1460)

### Current Code (Problematic)
```rust
// Remote volume - connect via NVMe-oF
match self.driver.connect_nvmeof_target(&nqn, &target_ip, &target_port, &transport).await {
    Ok(bdev) => bdev,
    Err(e) if e.to_string().contains("already exists") => {
        // Assume controller exists and is working
        println!("ℹ️ [DRIVER] Already connected to NVMe-oF target");
        format!("nvme_{}n1", name)  // Construct expected bdev name
    }
    Err(e) => return Err(...)
}
```

### Proposed Fix
```rust
// Remote volume - connect via NVMe-oF
let bdev_name = match self.driver.connect_nvmeof_target(&nqn, &target_ip, &target_port, &transport).await {
    Ok(bdev) => bdev,
    Err(e) if e.to_string().contains("already exists") => {
        println!("ℹ️ [DRIVER] Controller already exists, verifying bdev...");
        
        // Construct expected bdev name
        let expected_bdev = format!("nvme_{}n1", name);
        
        // Verify the bdev actually exists
        match self.driver.verify_bdev_exists(&expected_bdev).await {
            Ok(true) => {
                println!("✅ [DRIVER] Bdev verified: {}", expected_bdev);
                expected_bdev
            }
            Ok(false) => {
                println!("⚠️ [DRIVER] Controller exists but bdev not found, cleaning up...");
                
                // Delete the failed controller
                let controller_name = format!("nvme_{}", name);
                if let Err(e) = self.driver.delete_nvme_controller(&controller_name).await {
                    println!("⚠️ [DRIVER] Failed to delete stale controller: {}", e);
                }
                
                // Retry the attach
                println!("🔄 [DRIVER] Retrying NVMe-oF attach after cleanup...");
                self.driver.connect_nvmeof_target(&nqn, &target_ip, &target_port, &transport).await
                    .map_err(|e| tonic::Status::internal(format!("Retry attach failed: {}", e)))?
            }
            Err(e) => {
                return Err(tonic::Status::internal(format!("Failed to verify bdev: {}", e)));
            }
        }
    }
    Err(e) => return Err(tonic::Status::internal(format!("NVMe-oF attach failed: {}", e)))
};
```

### Helper Functions Needed

**In `driver.rs`:**

```rust
/// Verify that a bdev exists
pub async fn verify_bdev_exists(&self, bdev_name: &str) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let rpc = json!({
        "method": "bdev_get_bdevs",
        "params": {
            "name": bdev_name
        }
    });
    
    match self.call_node_agent(&self.node_id, "/api/spdk/rpc", &rpc).await {
        Ok(response) => {
            if let Some(result) = response["result"].as_array() {
                Ok(!result.is_empty())
            } else {
                Ok(false)
            }
        }
        Err(_) => Ok(false)
    }
}

/// Delete a failed NVMe controller
pub async fn delete_nvme_controller(&self, controller_name: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let rpc = json!({
        "method": "bdev_nvme_detach_controller",
        "params": {
            "name": controller_name
        }
    });
    
    self.call_node_agent(&self.node_id, "/api/spdk/rpc", &rpc).await?;
    println!("✅ [DRIVER] Deleted stale NVMe controller: {}", controller_name);
    Ok(())
}
```

## Alternative Approaches

### Option A: Add Idempotency to NVMe-oF Setup (Recommended)
- Check if bdev exists when getting -114 error
- Clean up failed controllers
- Retry attach
- **Pros**: Handles all NVMe-oF flakiness robustly
- **Cons**: ~40 lines of code

### Option B: Revert ROX Fix, Use Local Access for Same-Node
- Go back to local lvol access for same-node ROX
- Add better ublk sharing for same-node multi-pod
- **Pros**: Avoids localhost NVMe-oF issues
- **Cons**: Complex ublk device sharing, original bdev claim conflict

### Option C: Fix NVMe-oF Localhost Connections
- Investigate why localhost NVMe-oF fails
- Might be SPDK bug, network configuration, or pod network issue
- **Pros**: Would work as intended
- **Cons**: Root cause unclear, may not be fixable

## Recommended Path Forward

**Implement Option A** - It's the most robust:

1. Add `verify_bdev_exists()` helper
2. Add `delete_nvme_controller()` helper  
3. Update NodeStageVolume to handle -114 properly
4. Test with rox-multi-pod

**Estimated effort**: 1-2 hours

## Testing After Fix

```bash
export KUBECONFIG=/Users/ddalton/.kube/config.cdrv

# Clean up
kubectl get ns | grep kuttl-test | awk '{print $1}' | xargs -r kubectl delete ns

# Run ROX test
cd /Users/ddalton/projects/rust/flint/tests/system
kubectl kuttl test --config kuttl-testsuite.yaml --test rox-multi-pod

# Then run full suite
kubectl kuttl test --config kuttl-testsuite.yaml
```

## Current Cluster State

**Nodes**: 2 (cdrv-1, cdrv-2)  
**Disks**:
- cdrv-1: nvme1n1 (1TB) - LVS initialized, 996GB free ✅
- cdrv-2: nvme1n1 (1TB) - LVS initialized, 996GB free ✅ (after 10-min recovery)

**CSI Driver**: flint-csi v0.4.0  
- Controller pod: Restarted (has ROX fix)
- Node pods: Restarted (have system disk detection, ublk idempotency, recovery logging)

**Known Working**:
- Volume creation ✅
- RWO volumes ✅
- System disk detection ✅
- Blobstore recovery with progress logging ✅

**Known Issues**:
- ROX multi-pod test fails due to NVMe-oF idempotency bug
- NVMe-oF controllers can exist in failed state without cleanup

## Commits Made This Session

| Commit | Description |
|--------|-------------|
| d050eef | Documentation consolidation |
| d5d4141 | Frontend: Remove CRD references |
| 46f1025 | Backend: Add system disk detection fields |
| 53c8535 | Helm: Mount host /proc for mount detection |
| 60ce1b6 | Simplify to partition-based system disk detection |
| eb07906 | Fix: Use actual DiskInfo fields in API |
| f5104c9 | ublk idempotency check (filesystem-based) |
| 66b7c5c | ROX: Always use NVMe-oF to avoid bdev conflicts |
| a547efc | Revert: Force shutdown timeout |
| d5746c2 | Add detailed recovery logging |
| 9c06d58-ca9d142 | Fix recovery patch format issues |
| 61f80ef | Merge recovery patches |
| b613a44 | Add progress logging every 10k pages |

## References

- **Test Directory**: `/Users/ddalton/projects/rust/flint/tests/system/tests-standard/rox-multi-pod/`
- **CSI Driver**: `/Users/ddalton/projects/rust/flint/spdk-csi-driver/src/main.rs` (NodeStageVolume ~line 1433)
- **Driver Helpers**: `/Users/ddalton/projects/rust/flint/spdk-csi-driver/src/driver.rs`
- **SPDK Code**: `/Users/ddalton/github/spdk/`

---

**Next Action**: Implement robust NVMe-oF idempotency with bdev verification and stale controller cleanup.

