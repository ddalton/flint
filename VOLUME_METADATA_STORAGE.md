# Volume Metadata Storage Strategy

## Problem Statement

Currently, single-replica volumes don't store their location information in the PersistentVolume. The driver has to query nodes to find where a volume lives. For multi-replica, we need to store which nodes have replicas.

## Solution: Use CSI volumeAttributes

Store volume metadata in `spec.csi.volumeAttributes` - this is the standard CSI way.

## Current Single-Replica PV (No Metadata)

```yaml
apiVersion: v1
kind: PersistentVolume
spec:
  csi:
    driver: flint.csi.storage.io
    volumeHandle: pvc-fb4b0325-244e-4815-be43-dbee69649c12
    volumeAttributes:
      storage.kubernetes.io/csiProvisionerIdentity: 1763677150315-8081-flint.csi.storage.io
```

**Current behavior**:
- Volume ID: `pvc-fb4b0325-244e-4815-be43-dbee69649c12`
- Lvol name: `vol_pvc-fb4b0325-244e-4815-be43-dbee69649c12`
- Location: Unknown - must query nodes to find it

## Updated Single-Replica PV (With Metadata)

```yaml
apiVersion: v1
kind: PersistentVolume
spec:
  csi:
    driver: flint.csi.storage.io
    volumeHandle: pvc-fb4b0325-244e-4815-be43-dbee69649c12
    volumeAttributes:
      storage.kubernetes.io/csiProvisionerIdentity: 1763677150315-8081-flint.csi.storage.io
      # NEW: Store volume location
      flint.csi.storage.io/node-name: "ublk-2.vpc.cloudera.com"
      flint.csi.storage.io/lvol-uuid: "12345678-1234-1234-1234-123456789abc"
      flint.csi.storage.io/lvs-name: "lvs_ublk-2_0000-3b-00-0"
      flint.csi.storage.io/replica-count: "1"
```

**Benefits**:
- ✅ No need to query nodes to find volume
- ✅ Faster volume operations
- ✅ Consistent with multi-replica approach

## Multi-Replica PV (With Full Replica Info)

```yaml
apiVersion: v1
kind: PersistentVolume
spec:
  csi:
    driver: flint.csi.storage.io
    volumeHandle: pvc-abc123
    volumeAttributes:
      storage.kubernetes.io/csiProvisionerIdentity: 1763677150315-8081-flint.csi.storage.io
      # Multi-replica metadata
      flint.csi.storage.io/replica-count: "3"
      flint.csi.storage.io/replicas: |
        [
          {
            "node_name": "ublk-1.vpc.cloudera.com",
            "lvol_uuid": "11111111-1111-1111-1111-111111111111",
            "lvol_name": "vol_pvc-abc123_replica_0",
            "lvs_name": "lvs_ublk-1_0000-3b-00-0"
          },
          {
            "node_name": "ublk-2.vpc.cloudera.com",
            "lvol_uuid": "22222222-2222-2222-2222-222222222222",
            "lvol_name": "vol_pvc-abc123_replica_1",
            "lvs_name": "lvs_ublk-2_0000-3b-00-0"
          },
          {
            "node_name": "ublk-3.vpc.cloudera.com",
            "lvol_uuid": "33333333-3333-3333-3333-333333333333",
            "lvol_name": "vol_pvc-abc123_replica_2",
            "lvs_name": "lvs_ublk-3_0000-3b-00-0"
          }
        ]
```

## Implementation Changes

### 1. Update CreateVolume to Store Metadata

**File**: `spdk-csi-driver/src/main.rs`

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
            
            // Always add the provisioner identity (CSI spec requirement)
            volume_context.insert(
                "storage.kubernetes.io/csiProvisionerIdentity".to_string(),
                format!("{}-{}-{}", 
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis(),
                    8081,
                    "flint.csi.storage.io"
                )
            );

            // Add replica count
            volume_context.insert(
                "flint.csi.storage.io/replica-count".to_string(),
                result.replicas.len().to_string(),
            );

            if result.replicas.len() == 1 {
                // SINGLE REPLICA: Store simple metadata
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
            } else {
                // MULTI-REPLICA: Store full replica array as JSON
                let replicas_json = serde_json::to_string(&result.replicas)
                    .map_err(|e| tonic::Status::internal(format!("Failed to serialize replicas: {}", e)))?;
                
                volume_context.insert(
                    "flint.csi.storage.io/replicas".to_string(),
                    replicas_json,
                );
            }

            let response = CreateVolumeResponse {
                volume: Some(Volume {
                    volume_id: volume_id.clone(),
                    capacity_bytes: size_bytes as i64,
                    volume_context,  // Kubernetes stores this in PV.spec.csi.volumeAttributes
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

### 2. Update Single-Replica Creation to Return Metadata

**File**: `spdk-csi-driver/src/driver.rs`

```rust
async fn create_single_replica_volume(
    &self,
    volume_id: &str,
    size_bytes: u64,
    thin_provision: bool,
) -> Result<VolumeCreationResult, MinimalStateError> {
    println!("🎯 [DRIVER] Creating single-replica volume: {}", volume_id);

    let node_name = "ublk-2.vpc.cloudera.com"; // TODO: Make dynamic
    
    // Get disks with existing LVS
    let initialized_disks = self.get_initialized_disks_from_node(node_name).await?;
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
    
    println!("✅ [DRIVER] Selected disk: {} with LVS: {}", 
             selected_disk.device_name, lvs_name);
    
    // Create logical volume
    let lvol_uuid = self.create_lvol(node_name, lvs_name, volume_id, size_bytes, thin_provision).await?;
    
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

### 3. Update NodePublishVolume to Read from volumeAttributes

**File**: `spdk-csi-driver/src/main.rs`

```rust
async fn node_publish_volume(
    &self,
    request: tonic::Request<NodePublishVolumeRequest>,
) -> Result<tonic::Response<NodePublishVolumeResponse>, tonic::Status> {
    let req = request.into_inner();
    let volume_id = req.volume_id.clone();
    let target_path = req.target_path.clone();

    println!("📦 [NODE] Publishing volume {} to {}", volume_id, target_path);

    // Read replica count from volumeAttributes
    let replica_count_str = req.volume_context.get("flint.csi.storage.io/replica-count")
        .ok_or_else(|| tonic::Status::internal("Missing replica-count in volume context"))?;
    
    let replica_count: u32 = replica_count_str.parse()
        .map_err(|e| tonic::Status::internal(format!("Invalid replica-count: {}", e)))?;

    if replica_count == 1 {
        // SINGLE REPLICA PATH (existing + metadata lookup)
        return self.node_publish_single_replica_volume(&req, &volume_id, &target_path).await;
    } else {
        // MULTI-REPLICA PATH (RAID 1)
        return self.node_publish_multi_replica_volume(&req, &volume_id, &target_path).await;
    }
}

async fn node_publish_single_replica_volume(
    &self,
    req: &NodePublishVolumeRequest,
    volume_id: &str,
    target_path: &str,
) -> Result<tonic::Response<NodePublishVolumeResponse>, tonic::Status> {
    println!("📦 [NODE] Publishing single-replica volume: {}", volume_id);

    // Read volume metadata from volumeAttributes
    let node_name = req.volume_context.get("flint.csi.storage.io/node-name")
        .ok_or_else(|| tonic::Status::internal("Missing node-name in volume context"))?;
    
    let lvol_uuid = req.volume_context.get("flint.csi.storage.io/lvol-uuid")
        .ok_or_else(|| tonic::Status::internal("Missing lvol-uuid in volume context"))?;

    println!("📊 [NODE] Volume on node: {}, lvol UUID: {}", node_name, lvol_uuid);

    // Check if volume is local or needs NVMe-oF
    if *node_name == self.driver.node_id {
        // LOCAL: Use lvol bdev directly
        println!("✅ [NODE] Volume is LOCAL - using direct bdev access");
        let bdev_name = lvol_uuid;
        
        // Create ublk device
        let ublk_id = self.driver.generate_ublk_id(volume_id);
        let device_path = self.driver.create_ublk_device(bdev_name, ublk_id).await
            .map_err(|e| tonic::Status::internal(format!("Failed to create ublk: {}", e)))?;
        
        // Mount and publish
        // ... existing mount logic ...
    } else {
        // REMOTE: Need NVMe-oF (future enhancement for single-replica remote access)
        return Err(tonic::Status::unimplemented(
            "Single-replica remote access not yet implemented - volume must be on same node as Pod"
        ));
    }

    Ok(tonic::Response::new(NodePublishVolumeResponse {}))
}

async fn node_publish_multi_replica_volume(
    &self,
    req: &NodePublishVolumeRequest,
    volume_id: &str,
    target_path: &str,
) -> Result<tonic::Response<NodePublishVolumeResponse>, tonic::Status> {
    println!("📦 [NODE] Publishing multi-replica volume: {}", volume_id);

    // Read replica info from volumeAttributes
    let replicas_json = req.volume_context.get("flint.csi.storage.io/replicas")
        .ok_or_else(|| tonic::Status::internal("Missing replicas in volume context"))?;

    let replicas: Vec<ReplicaInfo> = serde_json::from_str(replicas_json)
        .map_err(|e| tonic::Status::internal(format!("Failed to parse replicas: {}", e)))?;

    println!("📊 [NODE] Volume has {} replicas", replicas.len());

    // Create RAID with mixed local/remote access
    let raid_bdev_name = self.create_raid_from_replicas(volume_id, &replicas).await
        .map_err(|e| tonic::Status::internal(format!("Failed to create RAID: {}", e)))?;

    // Create ublk device from RAID bdev
    let ublk_id = self.driver.generate_ublk_id(volume_id);
    let device_path = self.driver.create_ublk_device(&raid_bdev_name, ublk_id).await
        .map_err(|e| tonic::Status::internal(format!("Failed to create ublk: {}", e)))?;

    // Mount and publish
    // ... existing mount logic ...

    Ok(tonic::Response::new(NodePublishVolumeResponse {}))
}
```

### 4. Update DeleteVolume to Read from volumeAttributes

**File**: `spdk-csi-driver/src/main.rs`

```rust
async fn delete_volume(
    &self,
    request: tonic::Request<DeleteVolumeRequest>,
) -> Result<tonic::Response<DeleteVolumeResponse>, tonic::Status> {
    let req = request.into_inner();
    let volume_id = req.volume_id.clone();

    println!("🗑️ [CONTROLLER] Deleting volume: {}", volume_id);

    // Get PV to read volumeAttributes
    let replica_info = self.get_volume_metadata(&volume_id).await?;

    match replica_info {
        VolumeMetadata::SingleReplica { node_name, lvol_uuid, .. } => {
            println!("🗑️ [CONTROLLER] Deleting single-replica volume on node: {}", node_name);
            self.driver.delete_lvol(&node_name, &lvol_uuid).await
                .map_err(|e| tonic::Status::internal(format!("Failed to delete lvol: {}", e)))?;
        }
        VolumeMetadata::MultiReplica { replicas } => {
            println!("🗑️ [CONTROLLER] Deleting multi-replica volume ({} replicas)", replicas.len());
            
            for (i, replica) in replicas.iter().enumerate() {
                println!("🗑️ Deleting replica {} on node {}", i + 1, replica.node_name);
                
                match self.driver.delete_lvol(&replica.node_name, &replica.lvol_uuid).await {
                    Ok(()) => println!("✅ Deleted replica {}", i + 1),
                    Err(e) => println!("⚠️ Failed to delete replica {}: {}", i + 1, e),
                }
            }
        }
    }

    println!("✅ [CONTROLLER] Volume deleted: {}", volume_id);
    Ok(tonic::Response::new(DeleteVolumeResponse {}))
}

/// Get volume metadata from PV volumeAttributes
async fn get_volume_metadata(
    &self,
    volume_id: &str,
) -> Result<VolumeMetadata, tonic::Status> {
    use k8s_openapi::api::core::v1::PersistentVolume;
    use kube::Api;

    let pvs: Api<PersistentVolume> = Api::all(self.kube_client.clone());
    let pv_list = pvs.list(&Default::default()).await
        .map_err(|e| tonic::Status::internal(format!("Failed to list PVs: {}", e)))?;
    
    for pv in pv_list.items {
        if let Some(spec) = &pv.spec {
            if let Some(csi) = &spec.csi {
                if csi.volume_handle == volume_id {
                    // Found the PV - read volumeAttributes
                    if let Some(attrs) = &csi.volume_attributes {
                        let replica_count = attrs.get("flint.csi.storage.io/replica-count")
                            .and_then(|s| s.parse::<u32>().ok())
                            .unwrap_or(1);

                        if replica_count == 1 {
                            // Single replica
                            let node_name = attrs.get("flint.csi.storage.io/node-name")
                                .ok_or_else(|| tonic::Status::internal("Missing node-name"))?
                                .clone();
                            let lvol_uuid = attrs.get("flint.csi.storage.io/lvol-uuid")
                                .ok_or_else(|| tonic::Status::internal("Missing lvol-uuid"))?
                                .clone();
                            let lvs_name = attrs.get("flint.csi.storage.io/lvs-name")
                                .ok_or_else(|| tonic::Status::internal("Missing lvs-name"))?
                                .clone();

                            return Ok(VolumeMetadata::SingleReplica {
                                node_name,
                                lvol_uuid,
                                lvs_name,
                            });
                        } else {
                            // Multi-replica
                            let replicas_json = attrs.get("flint.csi.storage.io/replicas")
                                .ok_or_else(|| tonic::Status::internal("Missing replicas"))?;
                            
                            let replicas: Vec<ReplicaInfo> = serde_json::from_str(replicas_json)
                                .map_err(|e| tonic::Status::internal(format!("Failed to parse replicas: {}", e)))?;

                            return Ok(VolumeMetadata::MultiReplica { replicas });
                        }
                    }
                }
            }
        }
    }

    Err(tonic::Status::not_found(format!("PV not found for volume: {}", volume_id)))
}
```

### 5. Add VolumeMetadata Enum

**File**: `spdk-csi-driver/src/minimal_models.rs`

```rust
/// Volume metadata from PV volumeAttributes
#[derive(Debug, Clone)]
pub enum VolumeMetadata {
    SingleReplica {
        node_name: String,
        lvol_uuid: String,
        lvs_name: String,
    },
    MultiReplica {
        replicas: Vec<ReplicaInfo>,
    },
}
```

## Migration Path

### For Existing Single-Replica Volumes (Without Metadata)

**Backward Compatibility**: The code should handle both cases:

```rust
async fn node_publish_volume(/*...*/) {
    // Try to read metadata from volumeAttributes
    if let Some(node_name) = req.volume_context.get("flint.csi.storage.io/node-name") {
        // NEW: Use stored metadata
        println!("✅ Using stored volume metadata");
    } else {
        // FALLBACK: Query nodes to find volume (existing behavior)
        println!("⚠️ No metadata found - querying nodes to find volume");
        let volume_info = self.driver.get_volume_info(&volume_id).await?;
        // Use volume_info.node_name, volume_info.lvol_uuid
    }
}
```

This ensures existing volumes without metadata continue to work.

## Summary

1. **✅ Use `spec.csi.volumeAttributes`** - Standard CSI approach, not separate annotations
2. **✅ Single-Replica** - Store node, lvol UUID, lvs name
3. **✅ Multi-Replica** - Store full replica array as JSON
4. **✅ Backward Compatible** - Fallback to querying nodes if metadata missing
5. **✅ Consistent** - Same storage mechanism for both single and multi-replica

This approach:
- Follows CSI standards
- Works with Kubernetes cluster restarts
- Eliminates need to query nodes to find volumes
- Provides foundation for multi-replica support

