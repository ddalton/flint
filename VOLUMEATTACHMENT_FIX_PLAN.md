# VolumeAttachment Lifecycle Fix Plan

**Problem:** VolumeAttachments stay `attached=true` after pod deletion, blocking CSI cleanup

**Impact:** NodeUnstageVolume never called → ghost mounts accumulate

---

## 🔍 Investigation Steps

### 1. Check CSI Driver Capabilities

Verify we're advertising the right capabilities:

```bash
kubectl logs -n flint-system -l app=flint-csi-controller -c flint-csi-controller | \
  grep -i "capability\|plugin.*cap"
```

**Expected:**
- `CONTROLLER_SERVICE`: `CREATE_DELETE_VOLUME`, `PUBLISH_UNPUBLISH_VOLUME`
- `NODE_SERVICE`: `STAGE_UNSTAGE_VOLUME`

### 2. Check ControllerUnpublishVolume Response

The response format MUST match CSI spec:

```protobuf
message ControllerUnpublishVolumeResponse {
  // Intentionally empty
}
```

**Verify in driver.rs:**
```rust
async fn controller_unpublish_volume(
    &self,
    request: Request<ControllerUnpublishVolumeRequest>,
) -> Result<Response<ControllerUnpublishVolumeResponse>, Status> {
    // Should return: Ok(Response::new(ControllerUnpublishVolumeResponse {}))
}
```

### 3. Check external-attacher Logs

```bash
kubectl logs -n flint-system -l app=flint-csi-controller -c csi-attacher | \
  grep -E "error|detach|unpublish" -i | tail -50
```

Look for:
- Errors calling ControllerUnpublishVolume
- VolumeAttachment status update failures
- gRPC errors

### 4. Compare with Working CSI Driver

Check how other CSI drivers handle this:
- EBS CSI driver
- GCE PD CSI driver
- Look at their ControllerUnpublishVolume implementation

---

## 🛠️ Possible Fixes

### Fix 1: Return Proper gRPC Response

Ensure ControllerUnpublishVolume returns empty response (not nil):

```rust
async fn controller_unpublish_volume(
    &self,
    request: Request<ControllerUnpublishVolumeRequest>,
) -> Result<Response<ControllerUnpublishVolumeResponse>, Status> {
    let req = request.into_inner();
    let volume_id = &req.volume_id;
    let node_id = &req.node_id;
    
    println!("📥 [CONTROLLER] Unpublishing volume {} from node {:?}", 
             volume_id, node_id);
    
    // Do unpublish work...
    
    println!("✅ [CONTROLLER] Volume {} unpublished successfully", volume_id);
    
    // CRITICAL: Return empty struct, not {}
    Ok(Response::new(ControllerUnpublishVolumeResponse {}))
}
```

### Fix 2: Update VolumeAttachment Status Explicitly

The external-attacher might not be updating status. Add explicit status update:

**Note:** This is typically handled by external-attacher sidecar, not the driver itself. But we can investigate if the sidecar is working correctly.

### Fix 3: Implement Custom Detach Logic

If external-attacher is broken, implement our own VolumeAttachment cleanup:

```rust
// In controller cleanup logic
async fn cleanup_volume_attachment(&self, volume_id: &str) -> Result<()> {
    let kube_client = self.kube_client.clone();
    let api: Api<VolumeAttachment> = Api::all(kube_client);
    
    // Find VolumeAttachment for this volume
    let vas = api.list(&ListParams::default()).await?;
    for va in vas.items {
        if va.spec.source.persistent_volume_name == Some(volume_id.to_string()) {
            // Delete it
            api.delete(&va.metadata.name, &DeleteParams::default()).await?;
            println!("🗑️ Cleaned up VolumeAttachment: {}", va.metadata.name);
        }
    }
    
    Ok(())
}
```

### Fix 4: Add NodeUnstageVolume Fallback

If NodeUnstageVolume isn't being called, trigger it from DeleteVolume:

```rust
async fn delete_volume(&self, volume_id: &str) -> Result<()> {
    // BEFORE deleting lvol, check if volume is still staged
    let staging_paths = self.find_staging_paths(volume_id).await?;
    
    for path in staging_paths {
        println!("⚠️ [DELETE] Volume still staged at: {}", path);
        println!("🔄 [DELETE] Calling NodeUnstageVolume as fallback...");
        
        // Call our own NodeUnstageVolume logic
        self.node_unstage_volume_internal(volume_id, &path).await?;
    }
    
    // Now safe to delete lvol
    self.delete_lvol(volume_id).await?;
    
    Ok(())
}
```

---

## 🔬 Diagnostic Queries

### Check VolumeAttachment Details
```bash
kubectl get volumeattachments -o yaml | less
# Look for:
# - .status.attached (should transition to false)
# - .status.detachError (any errors)
# - .metadata.finalizers (should be removed after detach)
```

### Watch VolumeAttachment Lifecycle
```bash
kubectl get volumeattachments -w
# In another terminal:
kubectl delete pod <test-pod>
# Watch if attached changes from true -> false
```

### Check CSI Attacher Version
```bash
kubectl get pod -n flint-system -l app=flint-csi-controller -o jsonpath='{.items[0].spec.containers[?(@.name=="csi-attacher")].image}'
```

---

## 📚 References

- [CSI Spec](https://github.com/container-storage-interface/spec/blob/master/spec.md)
- [external-attacher](https://github.com/kubernetes-csi/external-attacher)
- [VolumeAttachment API](https://kubernetes.io/docs/reference/kubernetes-api/config-and-storage-resources/volume-attachment-v1/)
- [CSI Volume Lifecycle](https://kubernetes-csi.github.io/docs/volume-lifecycle.html)

---

## ⚡ Quick Workaround (For Testing)

Until we fix the root cause, use this workflow:

```bash
# 1. Create and run job
kubectl apply -f test-job.yaml
kubectl wait --for=condition=complete job/test-job -n flint-system

# 2. Delete job
kubectl delete job test-job -n flint-system

# 3. Delete PVC
kubectl delete pvc test-pvc -n flint-system

# 4. Wait a bit
sleep 5

# 5. Manually delete VolumeAttachment
VA=$(kubectl get volumeattachments -o name | grep $(kubectl get pv -o jsonpath='{.items[0].metadata.name}'))
kubectl delete $VA

# 6. Wait for cleanup
sleep 10

# 7. Verify clean
kubectl get pv,pvc,volumeattachments
```

---

**Bottom Line:** We've proven the ghost mount fix works. Now we need to fix the VolumeAttachment lifecycle so the full CSI cleanup flow can run automatically.

