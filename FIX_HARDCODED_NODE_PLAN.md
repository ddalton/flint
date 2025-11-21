# Fix Hardcoded Node Name - Single Replica

## Problem

The current code hardcodes `"ublk-2.vpc.cloudera.com"` in multiple places:

**File: `spdk-csi-driver/src/driver.rs`**

```rust
// Line 60 - create_volume()
let node_name = "ublk-2.vpc.cloudera.com"; // ❌ HARDCODED!

// Line 638 - get_volume_info()
let node_name = "ublk-2.vpc.cloudera.com"; // ❌ HARDCODED!
```

This is a critical bug because:
- ❌ Only works on a specific node
- ❌ Ignores other nodes with available capacity
- ❌ Breaks when that node is unavailable
- ❌ Not scalable across cluster

## Solution

Implement dynamic node selection for single-replica volumes:

1. Query all nodes in cluster
2. Check available capacity on each node
3. Select a node with sufficient space
4. Create volume on selected node
5. Store node name in PV volumeAttributes

## Implementation

### Step 1: Update create_single_replica_volume()

**File: `spdk-csi-driver/src/driver.rs`**

```rust
async fn create_single_replica_volume(
    &self,
    volume_id: &str,
    size_bytes: u64,
    thin_provision: bool,
) -> Result<VolumeCreationResult, MinimalStateError> {
    println!("🎯 [DRIVER] Creating single-replica volume: {}", volume_id);

    // ❌ OLD: Hardcoded node
    // let node_name = "ublk-2.vpc.cloudera.com";

    // ✅ NEW: Dynamic node selection
    let node_name = self.select_node_for_single_replica(size_bytes).await?;
    
    println!("✅ [DRIVER] Selected node: {} for volume (size: {}GB)", 
             node_name, size_bytes / (1024*1024*1024));
    
    // Get disks with existing LVS on selected node
    let initialized_disks = self.get_initialized_disks_from_node(&node_name).await?;
    if initialized_disks.is_empty() {
        return Err(MinimalStateError::InsufficientCapacity { 
            required: size_bytes, 
            available: 0 
        });
    }
    
    // Find a disk with enough free space
    let selected_disk = initialized_disks.iter()
        .find(|d| d.free_space >= size_bytes)
        .ok_or_else(|| MinimalStateError::InsufficientCapacity { 
            required: size_bytes, 
            available: initialized_disks.iter().map(|d| d.free_space).max().unwrap_or(0)
        })?;
    
    let lvs_name = selected_disk.lvs_name.as_ref()
        .ok_or_else(|| MinimalStateError::InternalError { 
            message: "Selected disk has no LVS name".to_string() 
        })?;
    
    println!("✅ [DRIVER] Selected disk: {} with LVS: {} (free: {}GB)", 
             selected_disk.device_name, lvs_name,
             selected_disk.free_space / (1024*1024*1024));
    
    // Create logical volume
    let lvol_uuid = self.create_lvol(&node_name, lvs_name, volume_id, size_bytes, thin_provision).await?;
    
    println!("✅ [DRIVER] Single-replica volume created with lvol UUID: {}", lvol_uuid);
    
    // Return result with single replica info
    Ok(VolumeCreationResult {
        volume_id: volume_id.to_string(),
        size_bytes,
        replicas: vec![ReplicaInfo {
            node_name: node_name.to_string(),
            disk_pci_address: selected_disk.pci_address.clone(),
            lvol_uuid: lvol_uuid.clone(),
            lvol_name: format!("vol_{}", volume_id),
            lvs_name: lvs_name.clone(),
            nqn: None,
            target_ip: None,
            target_port: None,
            health: "online".to_string(),
        }],
    })
}
```

### Step 2: Implement select_node_for_single_replica()

**File: `spdk-csi-driver/src/driver.rs`**

```rust
/// Select a node for single-replica volume
/// Returns the first node with sufficient capacity
async fn select_node_for_single_replica(
    &self,
    size_bytes: u64,
) -> Result<String, MinimalStateError> {
    println!("🔍 [DRIVER] Finding node for single-replica volume (size: {} bytes)", size_bytes);

    // Get all nodes in cluster
    let all_nodes = self.get_all_nodes().await
        .map_err(|e| MinimalStateError::InternalError {
            message: format!("Failed to list nodes: {}", e)
        })?;

    println!("📊 [DRIVER] Found {} nodes in cluster", all_nodes.len());

    // Check each node for available capacity
    for node_name in all_nodes {
        println!("🔍 [DRIVER] Checking node: {}", node_name);

        // Query disks on this node
        match self.get_initialized_disks_from_node(&node_name).await {
            Ok(disks) => {
                // Check if any disk has enough space
                if let Some(disk) = disks.iter().find(|d| d.free_space >= size_bytes) {
                    println!("✅ [DRIVER] Node {} has sufficient space (disk: {}, free: {}GB)",
                             node_name, disk.device_name, disk.free_space / (1024*1024*1024));
                    return Ok(node_name);
                } else {
                    let max_free = disks.iter().map(|d| d.free_space).max().unwrap_or(0);
                    println!("   ⚠️ Node {} insufficient space (max free: {}GB, required: {}GB)",
                             node_name,
                             max_free / (1024*1024*1024),
                             size_bytes / (1024*1024*1024));
                }
            }
            Err(e) => {
                println!("   ⚠️ Skipping node {} (query failed: {})", node_name, e);
                continue;
            }
        }
    }

    // No node found with sufficient capacity
    Err(MinimalStateError::InsufficientCapacity {
        required: size_bytes,
        available: 0,
    })
}
```

### Step 3: Update get_volume_info() to Read from PV

**File: `spdk-csi-driver/src/driver.rs`**

Remove the hardcoded node lookup:

```rust
/// Get volume information (which node it's on, lvol UUID, etc.)
pub async fn get_volume_info(&self, volume_id: &str) -> Result<VolumeInfo, Box<dyn std::error::Error + Send + Sync>> {
    println!("🔍 [DRIVER] Getting info for volume: {}", volume_id);
    
    // ❌ OLD: Hardcoded node and query
    // let node_name = "ublk-2.vpc.cloudera.com";
    
    // ✅ NEW: Get from PV volumeAttributes (if available)
    // For backward compatibility, try PV first, then fall back to querying all nodes
    
    match self.get_volume_info_from_pv(volume_id).await {
        Ok(info) => {
            println!("✅ [DRIVER] Found volume info from PV: node={}", info.node_name);
            return Ok(info);
        }
        Err(e) => {
            println!("⚠️ [DRIVER] Could not get info from PV: {}, querying all nodes...", e);
        }
    }

    // FALLBACK: Query all nodes to find the volume (for volumes without metadata)
    let all_nodes = self.get_all_nodes().await?;
    
    for node_name in all_nodes {
        let payload = json!({
            "volume_id": volume_id
        });
        
        match self.call_node_agent(&node_name, "/api/volumes/get_info", &payload).await {
            Ok(response) => {
                let lvol_uuid = response["lvol_uuid"].as_str()
                    .ok_or("No lvol_uuid in response")?
                    .to_string();
                let lvs_name = response["lvs_name"].as_str()
                    .ok_or("No lvs_name in response")?
                    .to_string();
                let size_bytes = response["size_bytes"].as_u64()
                    .ok_or("No size_bytes in response")?;
                
                println!("✅ [DRIVER] Found volume on node: {}", node_name);
                
                return Ok(VolumeInfo {
                    volume_id: volume_id.to_string(),
                    node_name: node_name.to_string(),
                    lvol_uuid,
                    lvs_name,
                    size_bytes,
                });
            }
            Err(_) => {
                continue; // Try next node
            }
        }
    }
    
    Err(format!("Volume {} not found on any node", volume_id).into())
}

/// Get volume info from PV volumeAttributes
async fn get_volume_info_from_pv(&self, volume_id: &str) -> Result<VolumeInfo, Box<dyn std::error::Error + Send + Sync>> {
    use k8s_openapi::api::core::v1::PersistentVolume;
    use kube::Api;

    let pvs: Api<PersistentVolume> = Api::all(self.kube_client.clone());
    let pv_list = pvs.list(&Default::default()).await?;
    
    for pv in pv_list.items {
        if let Some(spec) = &pv.spec {
            if let Some(csi) = &spec.csi {
                if csi.volume_handle == volume_id {
                    // Found PV - read volumeAttributes
                    if let Some(attrs) = &csi.volume_attributes {
                        if let Some(node_name) = attrs.get("flint.csi.storage.io/node-name") {
                            let lvol_uuid = attrs.get("flint.csi.storage.io/lvol-uuid")
                                .ok_or("Missing lvol-uuid")?;
                            let lvs_name = attrs.get("flint.csi.storage.io/lvs-name")
                                .ok_or("Missing lvs-name")?;
                            
                            // Get size from PV capacity
                            let size_bytes = spec.capacity.as_ref()
                                .and_then(|c| c.get("storage"))
                                .and_then(|s| s.0.parse::<u64>().ok())
                                .unwrap_or(0);
                            
                            return Ok(VolumeInfo {
                                volume_id: volume_id.to_string(),
                                node_name: node_name.clone(),
                                lvol_uuid: lvol_uuid.clone(),
                                lvs_name: lvs_name.clone(),
                                size_bytes,
                            });
                        }
                    }
                }
            }
        }
    }
    
    Err("Volume not found in PVs or missing metadata".into())
}
```

### Step 4: Update CreateVolume to Store Node Info

This is already covered in `VOLUME_METADATA_STORAGE.md`, but let's ensure it's implemented:

**File: `spdk-csi-driver/src/main.rs`**

```rust
async fn create_volume(
    &self,
    request: tonic::Request<CreateVolumeRequest>,
) -> Result<tonic::Response<CreateVolumeResponse>, tonic::Status> {
    let req = request.into_inner();
    let volume_id = req.name.clone();
    
    // ... parameter extraction ...

    match self.driver.create_volume(&volume_id, size_bytes, replica_count, thin_provision).await {
        Ok(result) => {
            let mut volume_context = std::collections::HashMap::new();
            
            // Add replica count
            volume_context.insert(
                "flint.csi.storage.io/replica-count".to_string(),
                result.replicas.len().to_string(),
            );

            if result.replicas.len() == 1 {
                // SINGLE REPLICA: Store node and lvol info
                let replica = &result.replicas[0];
                volume_context.insert(
                    "flint.csi.storage.io/node-name".to_string(),
                    replica.node_name.clone(),
                );
                volume_context.insert(
                    "flint.csi.storage.io/lvol-uuid".to_string(),
                    replica.lvol_uuid.clone(),
                );
                volume_context.insert(
                    "flint.csi.storage.io/lvs-name".to_string(),
                    replica.lvs_name.clone(),
                );
                
                println!("✅ [CONTROLLER] Storing volume metadata: node={}, lvol={}", 
                         replica.node_name, replica.lvol_uuid);
            }

            let response = CreateVolumeResponse {
                volume: Some(Volume {
                    volume_id: volume_id.clone(),
                    capacity_bytes: size_bytes as i64,
                    volume_context,
                    content_source: None,
                    accessible_topology: vec![],
                }),
            };

            Ok(tonic::Response::new(response))
        }
        Err(e) => {
            Err(tonic::Status::internal(format!("Volume creation failed: {}", e)))
        }
    }
}
```

## Testing Plan

### 1. Test Single-Node Cluster

```bash
# Should work with only one node
kubectl apply -f test-pvc.yaml
kubectl get pvc -w
# Should see PVC bound

# Check PV metadata
kubectl get pv <pv-name> -o yaml
# Should see flint.csi.storage.io/node-name in volumeAttributes
```

### 2. Test Multi-Node Cluster

```bash
# Create multiple PVCs
for i in {1..5}; do
  cat <<EOF | kubectl apply -f -
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: test-pvc-$i
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 10Gi
  storageClassName: flint-csi
EOF
done

# Check that PVCs are distributed across nodes
kubectl get pv -o custom-columns=NAME:.metadata.name,NODE:.spec.csi.volumeAttributes.flint\.csi\.storage\.io/node-name
```

### 3. Test Insufficient Capacity

```bash
# Create PVC larger than any single node can handle
cat <<EOF | kubectl apply -f -
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: huge-pvc
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 10000Gi  # 10TB
  storageClassName: flint-csi
EOF

# Should stay Pending with appropriate error
kubectl describe pvc huge-pvc
# Should see event: "InsufficientCapacity"
```

### 4. Test Node Failure

```bash
# Create volume on node-1
kubectl apply -f test-pvc.yaml

# Drain node-1
kubectl drain node-1 --ignore-daemonsets

# Create new volume - should go to node-2 or node-3
kubectl apply -f test-pvc-2.yaml

# Verify different nodes
kubectl get pv -o custom-columns=NAME:.metadata.name,NODE:.spec.csi.volumeAttributes.flint\.csi\.storage\.io/node-name
```

### 5. Test Backward Compatibility

```bash
# For existing volumes without metadata
# Should fall back to querying all nodes
kubectl get pv old-pv -o yaml
# volumeAttributes should be empty or missing node-name

# Should still work (queries nodes to find volume)
POD_USING_OLD_VOLUME
```

## Rollout Plan

### Phase 1: Update Code (Week 1)
- Implement `select_node_for_single_replica()`
- Update `create_single_replica_volume()`
- Update `get_volume_info()` with PV lookup
- Update `create_volume()` to store metadata

### Phase 2: Test in Dev (Week 1)
- Deploy to dev cluster
- Test all scenarios above
- Verify logs show correct node selection
- Check PV metadata is stored

### Phase 3: Update Existing Volumes (Optional)
- Script to add metadata to existing PVs
- Not required (backward compatibility works)

### Phase 4: Deploy to Production (Week 2)
- After successful dev testing
- Monitor volume creation logs
- Verify node distribution

## Success Criteria

- ✅ No hardcoded node names in code
- ✅ Volumes distributed across all nodes with capacity
- ✅ PV volumeAttributes contains node metadata
- ✅ Handles insufficient capacity gracefully
- ✅ Backward compatible with existing volumes
- ✅ All existing tests pass

## Implementation Checklist

- [ ] Remove hardcoded `"ublk-2.vpc.cloudera.com"` from `create_volume()`
- [ ] Implement `select_node_for_single_replica()`
- [ ] Update `get_volume_info()` to read from PV first
- [ ] Implement `get_volume_info_from_pv()`
- [ ] Update `create_volume()` to store node metadata
- [ ] Test single-node cluster
- [ ] Test multi-node cluster
- [ ] Test insufficient capacity
- [ ] Test node failure scenario
- [ ] Test backward compatibility
- [ ] Update documentation
- [ ] Code review and merge

## After This Fix

Once this is working and tested, we can proceed with multi-replica implementation, which will use the same node selection logic but for multiple nodes:

```rust
// Single replica (this fix)
let node = select_node_for_single_replica(size_bytes).await?;

// Multi-replica (future)
let nodes = select_nodes_for_replicas(replica_count, size_bytes).await?;
```

The multi-replica implementation will build on this foundation.

---

**Priority**: 🔥 **HIGH** - Critical bug fix
**Effort**: 1-2 weeks
**Risk**: Low (backward compatible)

