# Multi-Replica Support Implementation Plan

## Executive Summary

This document outlines the implementation plan for adding **distributed RAID 1 multi-replica support** to the Flint CSI driver using SPDK RAID 1 functionality. This implementation provides true high availability by distributing replicas across different nodes.

## Design Approach

### ✅ Distributed RAID 1 Only
- **Replicas on Different Nodes**: Each replica MUST be on a different node
- **True High Availability**: Survives node failures (not just disk failures)
- **Simpler Implementation**: Single code path, no local RAID phase

### ✅ Core Design Principles

1. **Distributed Only**: Replicas MUST be on different nodes
2. **Static Replicas**: Replica locations stored in PersistentVolume metadata
3. **Smart RAID Creation**: Created on Pod's node with mixed local/remote access
4. **Degraded Operation**: Works with 2+ replicas (minimum 2 required)
5. **Automatic Rebuild**: Monitor down nodes and rebuild when they return
6. **Persistent State**: Survive cluster restarts by reading PV metadata

## Architecture Overview

### Volume Creation Flow

```
User creates PVC with numReplicas: "3"
    ↓
CSI Controller: Find 3 nodes with available space
    │
    ├─→ Found 3 nodes? YES → Continue
    │
    └─→ Found < 3 nodes? NO → Fail PVC with Event
    ↓
Create lvol on Node 1, Node 2, Node 3
    ↓
Store replica info in PV metadata:
    metadata.annotations:
      flint.csi.storage.io/replicas: |
        [
          {"node": "node1", "lvol_uuid": "...", "lvs_name": "..."},
          {"node": "node2", "lvol_uuid": "...", "lvs_name": "..."},
          {"node": "node3", "lvol_uuid": "...", "lvs_name": "..."}
        ]
    ↓
Return PV to Kubernetes
```

### Volume Attachment Flow (NodePublishVolume)

```
Pod scheduled on Node 2
    ↓
CSI Node: Read replica info from PV metadata
    ↓
For each replica:
    Replica on Node 2 (local) → Use local bdev directly
    Replica on Node 1 (remote) → Setup NVMe-oF, attach as nvme bdev
    Replica on Node 3 (remote) → Setup NVMe-oF, attach as nvme bdev
    ↓
Create RAID 1 bdev on Node 2:
    base_bdevs: [local_lvol, nvme_bdev_node1, nvme_bdev_node3]
    ↓
Expose RAID bdev via ublk → /dev/ublkb0
    ↓
Mount filesystem and publish to Pod
```

### Cluster Restart Recovery

```
Cluster restarts, Pods rescheduled
    ↓
CSI Node: NodePublishVolume called
    ↓
Read replica info from PV metadata (persistent)
    ↓
Check each replica node status:
    Node 1: UP → Attach (local or NVMe-oF)
    Node 2: UP → Attach (local or NVMe-oF)
    Node 3: DOWN → Skip (will rebuild later)
    ↓
Create RAID 1 bdev with available replicas (2/3):
    base_bdevs: [node1_bdev, node2_bdev]
    status: DEGRADED (but functional)
    ↓
Background monitor: Detect Node 3 comes up
    ↓
Rebuild: Add Node 3 back to RAID
    status: ONLINE (3/3)
```

## Implementation Details

### Phase 1: Multi-Node Replica Creation (Weeks 1-3)

#### 1.1 Update Volume Creation Logic

**File**: `spdk-csi-driver/src/driver.rs`

```rust
pub async fn create_volume(
    &self,
    volume_id: &str,
    size_bytes: u64,
    replica_count: u32,
    thin_provision: bool,
) -> Result<VolumeCreationResult, MinimalStateError> {
    println!("🎯 [DRIVER] Creating volume: {} ({} bytes, {} replicas)", 
             volume_id, size_bytes, replica_count);

    // CRITICAL: Single replica uses existing path (zero changes)
    if replica_count == 1 {
        return self.create_single_replica_volume(volume_id, size_bytes, thin_provision).await;
    }

    // Multi-replica: RAID 1 requires minimum 2 replicas
    if replica_count < 2 {
        return Err(MinimalStateError::InvalidParameter {
            message: "RAID 1 requires minimum 2 replicas".to_string()
        });
    }

    // Create distributed multi-replica volume
    self.create_distributed_multi_replica_volume(
        volume_id, 
        size_bytes, 
        replica_count, 
        thin_provision
    ).await
}

async fn create_distributed_multi_replica_volume(
    &self,
    volume_id: &str,
    size_bytes: u64,
    replica_count: u32,
    thin_provision: bool,
) -> Result<VolumeCreationResult, MinimalStateError> {
    println!("🎯 [DRIVER] Creating distributed multi-replica volume: {} ({} replicas)", 
             volume_id, replica_count);

    // Step 1: Find N nodes with available space (each on different node)
    let selected_nodes = self.select_nodes_for_replicas(replica_count, size_bytes).await?;
    
    if selected_nodes.len() < replica_count as usize {
        return Err(MinimalStateError::InsufficientNodes {
            required: replica_count,
            available: selected_nodes.len() as u32,
            message: format!(
                "Cannot create {} replicas: only {} nodes with sufficient space",
                replica_count, selected_nodes.len()
            )
        });
    }

    println!("✅ [DRIVER] Selected {} nodes for replicas:", selected_nodes.len());
    for (i, node_info) in selected_nodes.iter().enumerate() {
        println!("   Replica {}: node={}, disk={}, free={}GB",
                 i + 1,
                 node_info.node_name,
                 node_info.disk.device_name,
                 node_info.disk.free_space / (1024*1024*1024));
    }

    // Step 2: Create lvol on each selected node
    let mut replicas = Vec::new();
    for (i, node_info) in selected_nodes.iter().enumerate() {
        let replica_volume_id = format!("{}_replica_{}", volume_id, i);
        let lvs_name = node_info.disk.lvs_name.as_ref().unwrap();

        let lvol_uuid = self.create_lvol(
            &node_info.node_name,
            lvs_name,
            &replica_volume_id,
            size_bytes,
            thin_provision,
        ).await?;

        println!("✅ [DRIVER] Created replica {} on node {}: UUID={}", 
                 i + 1, node_info.node_name, lvol_uuid);

        replicas.push(ReplicaInfo {
            node_name: node_info.node_name.clone(),
            disk_pci_address: node_info.disk.pci_address.clone(),
            lvol_uuid: lvol_uuid.clone(),
            lvol_name: format!("vol_{}", replica_volume_id),
            lvs_name: lvs_name.clone(),
            nqn: None, // Will be set during NodePublishVolume
            target_ip: None,
            target_port: None,
            health: "online".to_string(),
        });
    }

    println!("✅ [DRIVER] Created {} replicas for volume {}", replicas.len(), volume_id);

    // Step 3: Return result with replica metadata
    // This will be stored in PV annotations by CSI controller
    Ok(VolumeCreationResult {
        volume_id: volume_id.to_string(),
        size_bytes,
        replicas,
    })
}

/// Select N nodes that each have a disk with sufficient space
/// CRITICAL: Each replica MUST be on a different node
async fn select_nodes_for_replicas(
    &self,
    replica_count: u32,
    size_bytes: u64,
) -> Result<Vec<NodeDiskSelection>, MinimalStateError> {
    println!("🔍 [DRIVER] Finding {} nodes for replicas (size: {} bytes)", 
             replica_count, size_bytes);

    // Get all nodes in cluster
    let all_nodes = self.get_all_nodes().await?;
    let mut selected = Vec::new();

    for node_name in all_nodes {
        if selected.len() >= replica_count as usize {
            break; // Found enough nodes
        }

        // Query disks on this node
        match self.get_initialized_disks_from_node(&node_name).await {
            Ok(disks) => {
                // Find first disk with enough space
                if let Some(disk) = disks.iter().find(|d| d.free_space >= size_bytes) {
                    selected.push(NodeDiskSelection {
                        node_name: node_name.clone(),
                        disk: disk.clone(),
                    });
                    println!("   ✓ Selected node: {} (disk: {}, free: {}GB)",
                             node_name, disk.device_name, disk.free_space / (1024*1024*1024));
                }
            }
            Err(e) => {
                println!("   ⚠️ Skipping node {} (query failed: {})", node_name, e);
                continue;
            }
        }
    }

    Ok(selected)
}
```

#### 1.2 Store Replica Info in PV volumeAttributes

> **📝 Note**: See `VOLUME_METADATA_STORAGE.md` for complete details on metadata storage strategy.

**File**: `spdk-csi-driver/src/main.rs` (CSI Controller)

```rust
async fn create_volume(
    &self,
    request: tonic::Request<CreateVolumeRequest>,
) -> Result<tonic::Response<CreateVolumeResponse>, tonic::Status> {
    let req = request.into_inner();
    let volume_id = req.name.clone();
    
    // ... parameter extraction ...

    // Create volume with replicas
    match self.driver.create_volume(&volume_id, size_bytes, replica_count, thin_provision).await {
        Ok(result) => {
            println!("✅ [CONTROLLER] Volume {} created with {} replicas", 
                     volume_id, result.replicas.len());

            let mut volume_context = std::collections::HashMap::new();
            
            // Add replica count (always)
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
            println!("❌ [CONTROLLER] Volume creation failed: {}", e);
            Err(tonic::Status::internal(format!("Volume creation failed: {}", e)))
        }
    }
}
```

**Result**: PV will have metadata in `spec.csi.volumeAttributes`:

```yaml
# Single Replica
spec:
  csi:
    volumeAttributes:
      flint.csi.storage.io/replica-count: "1"
      flint.csi.storage.io/node-name: "ublk-2.vpc.cloudera.com"
      flint.csi.storage.io/lvol-uuid: "12345678-..."
      flint.csi.storage.io/lvs-name: "lvs_ublk-2_..."

# Multi-Replica
spec:
  csi:
    volumeAttributes:
      flint.csi.storage.io/replica-count: "3"
      flint.csi.storage.io/replicas: '[{"node_name":"node1",...},...]'
```

#### 1.3 Add PVC Event for Insufficient Nodes

```rust
async fn create_distributed_multi_replica_volume(
    &self,
    volume_id: &str,
    size_bytes: u64,
    replica_count: u32,
    thin_provision: bool,
) -> Result<VolumeCreationResult, MinimalStateError> {
    // ... node selection ...

    if selected_nodes.len() < replica_count as usize {
        // Create Kubernetes Event on the PVC
        self.create_pvc_event(
            &volume_id,
            "InsufficientNodes",
            &format!(
                "Cannot create volume with {} replicas: only {} nodes have sufficient space ({}GB required per node)",
                replica_count,
                selected_nodes.len(),
                size_bytes / (1024*1024*1024)
            ),
        ).await?;

        return Err(MinimalStateError::InsufficientNodes {
            required: replica_count,
            available: selected_nodes.len() as u32,
            message: "Not enough nodes with sufficient space".to_string()
        });
    }

    // ... continue with volume creation ...
}

/// Create a Kubernetes Event for a PVC
async fn create_pvc_event(
    &self,
    volume_id: &str,
    reason: &str,
    message: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use k8s_openapi::api::core::v1::Event;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};
    use chrono::Utc;

    // Note: In real implementation, we'd need to find the PVC from volume_id
    // For now, just log the event
    println!("📢 [EVENT] PVC Event: reason={}, message={}", reason, message);

    // TODO: Create actual Kubernetes Event object
    // let events_api: Api<Event> = Api::namespaced(self.kube_client.clone(), namespace);
    // events_api.create(...).await?;

    Ok(())
}
```

### Phase 2: Smart RAID Creation on Pod Node (Weeks 4-6)

#### 2.1 NodePublishVolume Implementation

**File**: `spdk-csi-driver/src/main.rs` (CSI Node Service)

```rust
async fn node_publish_volume(
    &self,
    request: tonic::Request<NodePublishVolumeRequest>,
) -> Result<tonic::Response<NodePublishVolumeResponse>, tonic::Status> {
    let req = request.into_inner();
    let volume_id = req.volume_id.clone();
    let target_path = req.target_path.clone();

    println!("📦 [NODE] Publishing volume {} to {}", volume_id, target_path);

    // Extract replica info from volume_context (from PV annotations)
    let replicas_json = req.volume_context.get("flint.csi.storage.io/replicas")
        .ok_or_else(|| tonic::Status::internal("Missing replica info in volume context"))?;

    let replicas: Vec<ReplicaInfo> = serde_json::from_str(replicas_json)
        .map_err(|e| tonic::Status::internal(format!("Failed to parse replicas: {}", e)))?;

    println!("📊 [NODE] Volume has {} replicas:", replicas.len());
    for (i, replica) in replicas.iter().enumerate() {
        println!("   Replica {}: node={}, lvol={}", 
                 i + 1, replica.node_name, replica.lvol_uuid);
    }

    // Create RAID 1 bdev with mixed local/remote access
    let raid_bdev_name = self.create_raid_from_replicas(&volume_id, &replicas).await
        .map_err(|e| tonic::Status::internal(format!("Failed to create RAID: {}", e)))?;

    // Create ublk device
    let ublk_id = self.driver.generate_ublk_id(&volume_id);
    let device_path = self.driver.create_ublk_device(&raid_bdev_name, ublk_id).await
        .map_err(|e| tonic::Status::internal(format!("Failed to create ublk device: {}", e)))?;

    // Format and mount (if needed)
    // ... existing mount logic ...

    println!("✅ [NODE] Volume {} published to {}", volume_id, target_path);
    Ok(tonic::Response::new(NodePublishVolumeResponse {}))
}

/// Create RAID 1 bdev from replicas with smart local/remote access
async fn create_raid_from_replicas(
    &self,
    volume_id: &str,
    replicas: &[ReplicaInfo],
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let current_node = &self.driver.node_id;
    
    println!("🔧 [NODE] Creating RAID 1 on node: {}", current_node);
    println!("🔧 [NODE] Processing {} replicas...", replicas.len());

    // Check minimum replica requirement
    let available_replicas: Vec<&ReplicaInfo> = replicas.iter()
        .filter(|r| self.is_node_available(&r.node_name))
        .collect();

    if available_replicas.len() < 2 {
        return Err(format!(
            "Cannot create RAID 1: need minimum 2 replicas, only {} available",
            available_replicas.len()
        ).into());
    }

    if available_replicas.len() < replicas.len() {
        println!("⚠️ [NODE] DEGRADED: {}/{} replicas available", 
                 available_replicas.len(), replicas.len());
    }

    // Attach each replica (local or remote)
    let mut base_bdevs = Vec::new();

    for (i, replica) in available_replicas.iter().enumerate() {
        if replica.node_name == *current_node {
            // LOCAL: Use lvol bdev directly
            println!("   Replica {}: LOCAL access (lvol: {})", 
                     i + 1, replica.lvol_uuid);
            base_bdevs.push(replica.lvol_uuid.clone());
        } else {
            // REMOTE: Setup NVMe-oF and attach
            println!("   Replica {}: REMOTE access (node: {}, setting up NVMe-oF...)", 
                     i + 1, replica.node_name);

            // Create NVMe-oF target on remote node
            let nqn = format!("nqn.2024-11.com.flint:volume:{}:replica:{}", volume_id, i);
            let target_info = self.setup_nvmeof_target_for_replica(
                &replica.node_name,
                &replica.lvol_uuid,
                &nqn,
            ).await?;

            // Attach NVMe-oF target from current node
            let nvme_bdev = self.driver.connect_to_nvmeof_target(&target_info).await?;
            println!("   ✓ Attached remote replica as: {}", nvme_bdev);
            base_bdevs.push(nvme_bdev);
        }
    }

    // Create RAID 1 bdev
    let raid_name = format!("raid_{}", volume_id);
    println!("🔧 [NODE] Creating RAID 1 bdev: {} with {} base bdevs", 
             raid_name, base_bdevs.len());

    let raid_bdev_name = self.driver.create_raid1_bdev(
        current_node,
        &raid_name,
        base_bdevs,
    ).await?;

    println!("✅ [NODE] RAID 1 bdev created: {}", raid_bdev_name);
    
    // Start background monitor for missing replicas
    if available_replicas.len() < replicas.len() {
        self.start_replica_monitor(volume_id, replicas.to_vec()).await;
    }

    Ok(raid_bdev_name)
}

/// Check if a node is available (for degraded operation)
fn is_node_available(&self, node_name: &str) -> bool {
    // Try to connect to node agent
    // In production, this would check node readiness
    match self.driver.call_node_agent(node_name, "/health", &json!({})).await {
        Ok(_) => true,
        Err(_) => {
            println!("   ⚠️ Node {} is not available", node_name);
            false
        }
    }
}
```

#### 2.2 Setup NVMe-oF for Remote Replicas

```rust
async fn setup_nvmeof_target_for_replica(
    &self,
    target_node: &str,
    lvol_uuid: &str,
    nqn: &str,
) -> Result<NvmeofConnectionInfo, Box<dyn std::error::Error + Send + Sync>> {
    println!("🌐 [NODE] Setting up NVMe-oF target on {}", target_node);

    // Get target node IP
    let target_ip = self.driver.get_node_ip(target_node).await?;

    // Create NVMe-oF subsystem on remote node
    let payload = json!({
        "method": "nvmf_create_subsystem",
        "params": {
            "nqn": nqn,
            "allow_any_host": true,
            "serial_number": format!("SPDK{:016x}", 
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64),
            "model_number": "SPDK CSI Replica"
        }
    });

    self.driver.call_node_agent(target_node, "/api/spdk/rpc", &payload).await?;

    // Add namespace (lvol) to subsystem
    let payload = json!({
        "method": "nvmf_subsystem_add_ns",
        "params": {
            "nqn": nqn,
            "namespace": {
                "nsid": 1,
                "bdev_name": lvol_uuid
            }
        }
    });

    self.driver.call_node_agent(target_node, "/api/spdk/rpc", &payload).await?;

    // Add listener
    let payload = json!({
        "method": "nvmf_subsystem_add_listener",
        "params": {
            "nqn": nqn,
            "listen_address": {
                "trtype": "TCP",
                "traddr": target_ip.clone(),
                "trsvcid": self.driver.nvmeof_target_port.to_string(),
                "adrfam": "ipv4"
            }
        }
    });

    self.driver.call_node_agent(target_node, "/api/spdk/rpc", &payload).await?;

    println!("✅ [NODE] NVMe-oF target created: {}:{}", target_ip, self.driver.nvmeof_target_port);

    Ok(NvmeofConnectionInfo {
        nqn: nqn.to_string(),
        target_ip,
        target_port: self.driver.nvmeof_target_port,
        transport: "TCP".to_string(),
    })
}
```

### Phase 3: Automatic Rebuild for Down Nodes (Weeks 7-9)

#### 3.1 Background Replica Monitor

```rust
/// Start background task to monitor missing replicas
async fn start_replica_monitor(
    &self,
    volume_id: &str,
    all_replicas: Vec<ReplicaInfo>,
) {
    let driver = self.driver.clone();
    let volume_id = volume_id.to_string();

    tokio::spawn(async move {
        println!("🔍 [MONITOR] Started replica monitor for volume: {}", volume_id);

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;

            // Check each replica node
            for replica in &all_replicas {
                if !Self::is_replica_attached(&volume_id, &replica.node_name).await {
                    // Node might be back up - try to add it
                    match Self::try_add_replica_to_raid(&driver, &volume_id, replica).await {
                        Ok(()) => {
                            println!("✅ [MONITOR] Added replica back: node={}", replica.node_name);
                            
                            // Trigger rebuild
                            Self::trigger_raid_rebuild(&driver, &volume_id).await;
                        }
                        Err(e) => {
                            println!("⚠️ [MONITOR] Cannot add replica yet: {}", e);
                        }
                    }
                }
            }

            // Check if all replicas are back
            if Self::all_replicas_attached(&volume_id, &all_replicas).await {
                println!("✅ [MONITOR] All replicas available - stopping monitor");
                break;
            }
        }
    });
}

async fn try_add_replica_to_raid(
    driver: &Arc<SpdkCsiDriver>,
    volume_id: &str,
    replica: &ReplicaInfo,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("🔧 [MONITOR] Attempting to add replica: node={}", replica.node_name);

    // Check if node is available
    if !driver.call_node_agent(&replica.node_name, "/health", &json!({})).await.is_ok() {
        return Err("Node not available yet".into());
    }

    // Setup NVMe-oF target for this replica
    let nqn = format!("nqn.2024-11.com.flint:volume:{}:node:{}", volume_id, replica.node_name);
    let target_info = Self::setup_nvmeof_target_for_replica_internal(
        driver,
        &replica.node_name,
        &replica.lvol_uuid,
        &nqn,
    ).await?;

    // Attach NVMe-oF target
    let nvme_bdev = driver.connect_to_nvmeof_target(&target_info).await?;

    // Add to existing RAID bdev
    let raid_name = format!("raid_{}", volume_id);
    let payload = json!({
        "method": "bdev_raid_add_base_bdev",
        "params": {
            "raid_bdev": raid_name,
            "base_bdev": nvme_bdev
        }
    });

    driver.call_node_agent(&driver.node_id, "/api/spdk/rpc", &payload).await?;

    println!("✅ [MONITOR] Replica added to RAID: {}", nvme_bdev);
    Ok(())
}

async fn trigger_raid_rebuild(
    driver: &Arc<SpdkCsiDriver>,
    volume_id: &str,
) {
    println!("🔄 [MONITOR] Triggering RAID rebuild for volume: {}", volume_id);
    
    // SPDK RAID 1 automatically rebuilds when a base bdev is added
    // We just need to monitor the rebuild progress
    
    tokio::spawn({
        let driver = driver.clone();
        let volume_id = volume_id.to_string();
        
        async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;

                let raid_name = format!("raid_{}", volume_id);
                match Self::get_raid_rebuild_status(&driver, &raid_name).await {
                    Ok(status) => {
                        if status.rebuilding {
                            println!("🔄 [REBUILD] Progress: {}% ({})",
                                     status.progress_percent,
                                     raid_name);
                        } else {
                            println!("✅ [REBUILD] Complete: {}", raid_name);
                            break;
                        }
                    }
                    Err(e) => {
                        println!("⚠️ [REBUILD] Status check failed: {}", e);
                    }
                }
            }
        }
    });
}
```

### Phase 4: Volume Deletion (Week 10)

#### 4.1 Delete Multi-Replica Volume

```rust
async fn delete_volume(
    &self,
    request: tonic::Request<DeleteVolumeRequest>,
) -> Result<tonic::Response<DeleteVolumeResponse>, tonic::Status> {
    let req = request.into_inner();
    let volume_id = req.volume_id.clone();

    println!("🗑️ [CONTROLLER] Deleting volume: {}", volume_id);

    // Get volume info from PV (which may have replica metadata)
    match self.get_volume_replicas(&volume_id).await {
        Ok(Some(replicas)) => {
            // Multi-replica volume
            println!("🗑️ [CONTROLLER] Deleting multi-replica volume ({} replicas)", replicas.len());
            
            // Delete each replica lvol
            for (i, replica) in replicas.iter().enumerate() {
                println!("🗑️ [CONTROLLER] Deleting replica {} on node {}", 
                         i + 1, replica.node_name);
                
                match self.driver.delete_lvol(&replica.node_name, &replica.lvol_uuid).await {
                    Ok(()) => println!("✅ Deleted replica {} (UUID: {})", i + 1, replica.lvol_uuid),
                    Err(e) => println!("⚠️ Failed to delete replica {}: {}", i + 1, e),
                }

                // Cleanup NVMe-oF target if it exists
                if let Some(nqn) = &replica.nqn {
                    let _ = self.driver.remove_nvmeof_target(&replica.node_name, nqn).await;
                }
            }

            println!("✅ [CONTROLLER] Multi-replica volume deleted: {}", volume_id);
        }
        Ok(None) => {
            // Single replica volume - use existing logic
            println!("🗑️ [CONTROLLER] Deleting single-replica volume");
            // ... existing single-replica deletion ...
        }
        Err(e) => {
            println!("⚠️ [CONTROLLER] Could not determine volume type: {}", e);
            // Best effort - try to clean up what we can
        }
    }

    Ok(tonic::Response::new(DeleteVolumeResponse {}))
}

/// Get replica info from PV annotations
async fn get_volume_replicas(
    &self,
    volume_id: &str,
) -> Result<Option<Vec<ReplicaInfo>>, Box<dyn std::error::Error + Send + Sync>> {
    use k8s_openapi::api::core::v1::PersistentVolume;
    use kube::Api;

    let pvs: Api<PersistentVolume> = Api::all(self.kube_client.clone());
    
    // Find PV by volume handle
    let pv_list = pvs.list(&Default::default()).await?;
    
    for pv in pv_list.items {
        if let Some(spec) = &pv.spec {
            if let Some(csi) = &spec.csi {
                if csi.volume_handle == volume_id {
                    // Found the PV - check for replica annotations
                    if let Some(attributes) = &csi.volume_attributes {
                        if let Some(replicas_json) = attributes.get("flint.csi.storage.io/replicas") {
                            let replicas: Vec<ReplicaInfo> = serde_json::from_str(replicas_json)?;
                            return Ok(Some(replicas));
                        }
                    }
                    return Ok(None); // PV found but no replicas (single replica)
                }
            }
        }
    }

    Err("PV not found".into())
}
```

### Phase 5: Data Models (Week 11)

#### 5.1 Update Models

**File**: `spdk-csi-driver/src/minimal_models.rs`

```rust
/// Volume creation result with replica information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeCreationResult {
    pub volume_id: String,
    pub size_bytes: u64,
    pub replicas: Vec<ReplicaInfo>,
}

/// Node and disk selection for a replica
#[derive(Debug, Clone)]
pub struct NodeDiskSelection {
    pub node_name: String,
    pub disk: DiskInfo,
}

/// RAID rebuild status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RaidRebuildStatus {
    pub raid_name: String,
    pub rebuilding: bool,
    pub progress_percent: f32,
    pub estimated_time_remaining_sec: Option<u64>,
}

/// Enhanced error types
#[derive(Debug)]
pub enum MinimalStateError {
    // ... existing errors ...
    
    InsufficientNodes {
        required: u32,
        available: u32,
        message: String,
    },
    RaidCreationFailed {
        message: String,
        available_replicas: u32,
        required_replicas: u32,
    },
}
```

## Testing Strategy

### Phase 6: Comprehensive Testing (Weeks 12-14)

#### Test Suite 1: Multi-Node Replica Creation

**File**: `tests/system/tests/multi-replica-creation/`

```yaml
# 00-storageclass.yaml
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: flint-multi-replica
provisioner: flint.csi.storage.io
parameters:
  numReplicas: "3"
  thinProvision: "false"

# 01-pvc.yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: test-multi-replica
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 5Gi
  storageClassName: flint-multi-replica

# 02-assert-pvc-bound.yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: test-multi-replica
status:
  phase: Bound

# 03-verify-replicas.yaml
apiVersion: kuttl.dev/v1beta1
kind: TestStep
commands:
  - script: |
      # Verify PV has replica annotations
      kubectl get pv $(kubectl get pvc test-multi-replica -o jsonpath='{.spec.volumeName}') \
        -o jsonpath='{.spec.csi.volumeAttributes.flint\.csi\.storage\.io/replicas}' | \
        jq '. | length' | grep 3
```

#### Test Suite 2: Pod Scheduling and RAID Creation

```yaml
# 04-pod.yaml
apiVersion: v1
kind: Pod
metadata:
  name: test-pod
spec:
  containers:
  - name: app
    image: busybox
    command: ["sh", "-c", "dd if=/dev/urandom of=/data/testfile bs=1M count=100 && md5sum /data/testfile > /data/checksum && sleep 3600"]
    volumeMounts:
    - name: storage
      mountPath: /data
  volumes:
  - name: storage
    persistentVolumeClaim:
      claimName: test-multi-replica

# 05-verify-raid.yaml
apiVersion: kuttl.dev/v1beta1
kind: TestStep
commands:
  - script: |
      # Get node where pod is running
      NODE=$(kubectl get pod test-pod -o jsonpath='{.spec.nodeName}')
      
      # Check SPDK logs on that node for RAID creation
      kubectl logs -n kube-system -l app=flint-csi-node,node=$NODE | \
        grep "RAID 1 bdev created"
```

#### Test Suite 3: Insufficient Nodes Failure

```yaml
# Test with more replicas than available nodes
# Should fail with proper event
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: test-insufficient-nodes
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 5Gi
  storageClassName: flint-multi-replica-10  # numReplicas: "10"

# Assert: PVC should remain Pending
# Event should indicate insufficient nodes
```

#### Test Suite 4: Node Failure and Recovery

```yaml
# Simulate node failure and verify degraded operation
# Then bring node back and verify rebuild
```

#### Test Suite 5: Cluster Restart

```yaml
# Restart all CSI pods and verify volumes can be re-attached
# using replica info from PV metadata
```

### Regression Tests

**CRITICAL**: All existing tests must pass without modification:

```bash
cd tests/system
kubectl kuttl test --test clean-shutdown
kubectl kuttl test --test volume-expansion
kubectl kuttl test --test snapshot-restore
kubectl kuttl test --test rwo-pvc-migration
```

## Configuration

### StorageClass Examples

```yaml
# Single Replica (Default - Existing Behavior)
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: flint-csi
provisioner: flint.csi.storage.io
parameters:
  numReplicas: "1"

---
# 2-Way Mirror (2 nodes)
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: flint-csi-ha
provisioner: flint.csi.storage.io
parameters:
  numReplicas: "2"

---
# 3-Way Mirror (3 nodes)
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: flint-csi-ha-3way
provisioner: flint.csi.storage.io
parameters:
  numReplicas: "3"
```

### PersistentVolume Annotations (Auto-Generated)

```yaml
apiVersion: v1
kind: PersistentVolume
metadata:
  name: pvc-abc123
spec:
  csi:
    driver: flint.csi.storage.io
    volumeHandle: pvc-abc123
    volumeAttributes:
      flint.csi.storage.io/replica-count: "3"
      flint.csi.storage.io/replicas: |
        [
          {
            "node_name": "worker-1",
            "disk_pci_address": "0000:3b:00.0",
            "lvol_uuid": "12345678-1234-1234-1234-123456789abc",
            "lvol_name": "vol_pvc-abc123_replica_0",
            "lvs_name": "lvs_worker-1_0000-3b-00-0",
            "health": "online"
          },
          {
            "node_name": "worker-2",
            "disk_pci_address": "0000:3b:00.0",
            "lvol_uuid": "87654321-4321-4321-4321-cba987654321",
            "lvol_name": "vol_pvc-abc123_replica_1",
            "lvs_name": "lvs_worker-2_0000-3b-00-0",
            "health": "online"
          },
          {
            "node_name": "worker-3",
            "disk_pci_address": "0000:3b:00.0",
            "lvol_uuid": "11111111-2222-3333-4444-555555555555",
            "lvol_name": "vol_pvc-abc123_replica_2",
            "lvs_name": "lvs_worker-3_0000-3b-00-0",
            "health": "online"
          }
        ]
```

## Timeline

| Week | Phase | Deliverable |
|------|-------|-------------|
| 1-3 | Multi-Node Replicas | Select nodes, create replicas, store in PV |
| 4-6 | Smart RAID Creation | Mixed local/remote, NVMe-oF setup |
| 7-9 | Auto Rebuild | Monitor down nodes, add back, rebuild |
| 10 | Deletion | Clean up replicas and NVMe-oF targets |
| 11 | Data Models | Update models, errors, annotations |
| 12-14 | Testing | Unit, integration, regression, failure scenarios |
| 15 | Release | Documentation, GA release |

**Total**: 15 weeks

## Success Criteria

### Must Have
- ✅ Replicas MUST be on different nodes
- ✅ PVC creation fails with event if insufficient nodes
- ✅ RAID created on Pod's node with mixed local/remote access
- ✅ Replica info persisted in PV annotations
- ✅ Works with 2+ replicas (minimum 2 for RAID 1)
- ✅ Degraded operation when some nodes are down
- ✅ Auto-rebuild when nodes come back
- ✅ Survives cluster restart
- ✅ Zero regressions in single-replica volumes

### Should Have
- ✅ Monitoring dashboard shows replica status
- ✅ Kubernetes events for replica health changes
- ✅ Rebuild progress reporting
- ✅ Performance metrics

## Summary of Key Changes from v1.0

1. **❌ Removed Local RAID 1**: Only distributed RAID 1 across nodes
2. **✅ Different Nodes Required**: Replicas must be on different nodes
3. **✅ Smart RAID Location**: Created on Pod's node (not replica nodes)
4. **✅ Mixed Access**: Local replica uses direct bdev, remote uses NVMe-oF
5. **✅ Persistent Replica Info**: Stored in PV annotations
6. **✅ Degraded Operation**: Works with 2+ replicas even if some nodes down
7. **✅ Auto-Rebuild**: Monitor and add back replicas when nodes return
8. **✅ Minimum 2 Replicas**: RAID 1 cannot be created with just 1 replica

This design provides true distributed high availability while maintaining simplicity and operational excellence.

---

**Status**: Ready for Implementation  
**Last Updated**: November 21, 2025

