// spdk_native_config.rs - SPDK Configuration from Custom Resources
// Configures SPDK based on SpdkRaidDisk, SpdkVolume, and SpdkSnapshot CRDs

use serde_json::{json, Value};
use std::collections::HashSet;
use kube::api::{Api, ListParams};
use kube::Client;
use crate::models::{SpdkRaidDisk, SpdkVolume, SpdkSnapshot, StorageBackend, RaidMemberState};

/// SPDK Configuration Manager based on Custom Resources
/// Reads from SpdkRaidDisk, SpdkVolume, and SpdkSnapshot CRDs to configure SPDK
#[derive(Clone)]
pub struct SpdkNativeConfig {
    pub spdk_rpc_url: String,
    pub node_id: String,
    pub kube_client: Client,
    pub namespace: String,
}

impl SpdkNativeConfig {
    pub fn new(spdk_rpc_url: String, node_id: String, kube_client: Client, namespace: String) -> Self {
        Self {
            spdk_rpc_url,
            node_id,
            kube_client,
            namespace,
        }
    }

    /// Simple RPC call to SPDK
    async fn call_rpc(&self, rpc_request: &Value) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let client = reqwest::Client::new();
        let response = client
            .post(&self.spdk_rpc_url)
            .json(rpc_request)
            .send()
            .await?;
        
        let result: Value = response.json().await?;
        
        // Check for RPC error
        if let Some(error) = result.get("error") {
            return Err(format!("RPC error: {}", error).into());
        }
        
        Ok(result)
    }

    /// Configure SPDK based on Custom Resources
    /// This is the main entry point that reads CRDs and configures SPDK accordingly
    pub async fn load_config(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🚀 [CRD_CONFIG] Configuring SPDK from Custom Resources for node: {}", self.node_id);

        // Step 1: Initialize SPDK framework if needed
        self.initialize_spdk_framework().await?;

        // Step 2: Discovery - get device information from Custom Resources
        let _discovered_devices = self.get_node_devices_from_crds().await?;

        // Step 3: Get current state of SPDK
        let current_bdevs = self.get_current_bdevs().await?;
        let current_lvstores = self.get_current_lvstores().await?;
        let current_nvmf_subsystems = self.get_current_nvmf_subsystems().await?;

        // Step 4: Configure RAID disks from SpdkRaidDisk CRDs
        self.configure_raid_disks(&current_bdevs).await?;

        // Step 5: Configure volumes from SpdkVolume CRDs
        self.configure_volumes(&current_lvstores).await?;

        // Step 6: Configure snapshots from SpdkSnapshot CRDs
        self.configure_snapshots().await?;

        // Step 7: Configure NVMe-oF subsystems if needed
        self.configure_nvmeof_subsystems(&current_nvmf_subsystems).await?;

        println!("✅ [CRD_CONFIG] SPDK configuration from CRDs completed");
        Ok(())
    }

    /// Initialize SPDK framework
    async fn initialize_spdk_framework(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔧 [INIT] Initializing SPDK framework");
        
        // Try to call framework_start_init
        match self.call_rpc(&json!({
            "method": "framework_start_init",
            "params": {}
        })).await {
            Ok(_) => println!("✅ [INIT] SPDK framework initialized"),
            Err(e) => {
                // This might fail if already initialized, which is fine
                println!("ℹ️ [INIT] Framework init returned: {} (may already be initialized)", e);
            }
        }
        
        Ok(())
    }

    /// Get current bdevs in SPDK
    async fn get_current_bdevs(&self) -> Result<HashSet<String>, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [DISCOVERY] Getting current bdevs");
        
        let response = self.call_rpc(&json!({
            "method": "bdev_get_bdevs",
            "params": {}
        })).await?;
        
        let mut bdev_names = HashSet::new();
        if let Some(bdevs) = response["result"].as_array() {
            for bdev in bdevs {
                if let Some(name) = bdev["name"].as_str() {
                    bdev_names.insert(name.to_string());
                }
            }
            println!("📦 [DISCOVERY] Found {} existing bdevs", bdev_names.len());
        }
        
        Ok(bdev_names)
    }

    /// Get current lvol stores in SPDK
    async fn get_current_lvstores(&self) -> Result<HashSet<String>, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [DISCOVERY] Getting current lvol stores");
        
        let response = self.call_rpc(&json!({
            "method": "bdev_lvol_get_lvstores",
            "params": {}
        })).await?;
        
        let mut lvs_names = HashSet::new();
        if let Some(stores) = response["result"].as_array() {
            for store in stores {
                if let Some(name) = store["name"].as_str() {
                    lvs_names.insert(name.to_string());
                }
            }
            println!("📦 [DISCOVERY] Found {} existing lvol stores", lvs_names.len());
        }
        
        Ok(lvs_names)
    }

    /// Get current NVMe-oF subsystems
    async fn get_current_nvmf_subsystems(&self) -> Result<HashSet<String>, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [DISCOVERY] Getting current NVMe-oF subsystems");
        
        let response = self.call_rpc(&json!({
            "method": "nvmf_get_subsystems",
            "params": {}
        })).await?;
        
        let mut nqns = HashSet::new();
        if let Some(subsystems) = response["result"].as_array() {
            for subsystem in subsystems {
                if let Some(nqn) = subsystem["nqn"].as_str() {
                    // Skip discovery subsystem
                    if nqn != "nqn.2014-08.org.nvmexpress.discovery" {
                        nqns.insert(nqn.to_string());
                    }
                }
            }
            println!("📦 [DISCOVERY] Found {} existing NVMe-oF subsystems", nqns.len());
        }
        
        Ok(nqns)
    }

    /// Configure RAID disks based on SpdkRaidDisk CRDs
    async fn configure_raid_disks(&self, current_bdevs: &HashSet<String>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔧 [RAID_CONFIG] Configuring RAID disks from CRDs");
        
        let raid_disks_api: Api<SpdkRaidDisk> = Api::namespaced(self.kube_client.clone(), &self.namespace);
        let lp = ListParams::default();
        let raid_disks = raid_disks_api.list(&lp).await?;
        
        for raid_disk in raid_disks.items {
            // Only process RAID disks for this node
            if raid_disk.spec.created_on_node != self.node_id {
                continue;
            }
            
            let raid_disk_id = &raid_disk.spec.raid_disk_id;
            let raid_bdev_name = format!("raid_{}", raid_disk_id);
            
            // Check if RAID bdev already exists
            if current_bdevs.contains(&raid_bdev_name) {
                println!("ℹ️ [RAID_CONFIG] RAID bdev {} already exists", raid_bdev_name);
                continue;
            }
            
            println!("🔨 [RAID_CONFIG] Creating RAID disk: {}", raid_disk_id);
            
            // First, create AIO bdevs for each member disk
            let mut base_bdevs = Vec::new();
            for member in &raid_disk.spec.member_disks {
                // Skip failed members
                if matches!(member.state, RaidMemberState::Failed) {
                    println!("⚠️ [RAID_CONFIG] Skipping failed member disk: {}", member.disk_ref);
                    continue;
                }
                
                // Only process members on this node
                if member.node_id != self.node_id {
                    continue;
                }
                
                let bdev_name = format!("aio_{}", member.disk_ref.replace("/", "_").replace("dev_", ""));
                
                // Check if bdev already exists
                if !current_bdevs.contains(&bdev_name) {
                    // CRITICAL SYSTEM DISK PROTECTION: Check if this is a system disk
                    let device_name = member.disk_ref.strip_prefix("/dev/").unwrap_or(&member.disk_ref);
                    
                    // Use comprehensive system disk check
                    if self.is_system_disk(device_name).await {
                        println!("🚨 [SYSTEM_DISK_PROTECTION] BLOCKED: {} is a system disk - cannot create AIO bdev", member.disk_ref);
                        println!("🚨 [SYSTEM_DISK_PROTECTION] Skipping RAID member to prevent system corruption");
                        continue; // Skip this member entirely
                    }
                    
                    // Create AIO bdev for this member disk
                    // Assuming disk_ref is like "/dev/nvme0n1"
                    println!("  Creating AIO bdev: {} for {}", bdev_name, member.disk_ref);
                    
                    let create_aio = json!({
                        "method": "bdev_aio_create",
                        "params": {
                            "name": bdev_name.clone(),
                            "filename": member.disk_ref.clone(),
                            "block_size": 512
                        }
                    });
                    
                    match self.call_rpc(&create_aio).await {
                        Ok(_) => {
                            println!("  ✅ Created AIO bdev: {}", bdev_name);
                            base_bdevs.push(bdev_name);
                        },
                        Err(e) => {
                            println!("  ⚠️ Failed to create AIO bdev {}: {}", bdev_name, e);
                        }
                    }
                } else {
                    base_bdevs.push(bdev_name);
                }
            }
            
            // Create RAID bdev if we have enough members
            if base_bdevs.len() >= raid_disk.spec.num_member_disks as usize {
                println!("  Creating RAID{} bdev: {} with {} members", 
                    raid_disk.spec.raid_level, raid_bdev_name, base_bdevs.len());
                
                let create_raid = json!({
                    "method": "bdev_raid_create",
                    "params": {
                        "name": raid_bdev_name.clone(),
                        "raid_level": raid_disk.spec.raid_level.clone(),
                        "base_bdevs": base_bdevs,
                        "strip_size_kb": raid_disk.spec.stripe_size_kb,
                        "superblock": raid_disk.spec.superblock_enabled
                    }
                });
                
                match self.call_rpc(&create_raid).await {
                    Ok(_) => {
                        println!("  ✅ Created RAID bdev: {}", raid_bdev_name);
                        
                        // Create lvol store on the RAID bdev if specified in status
                        if let Some(status) = &raid_disk.status {
                            if let Some(lvs_name) = &status.lvs_name {
                                println!("  📦 Creating LVS: {} on RAID bdev: {}", lvs_name, raid_bdev_name);
                                self.create_lvol_store(&raid_bdev_name, lvs_name).await?;
                                
                                // If LVS UUID is specified in status, we could validate it matches
                                if let Some(expected_uuid) = &status.lvs_uuid {
                                    println!("    Expected LVS UUID: {}", expected_uuid);
                                }
                            }
                        }
                    },
                    Err(e) => {
                        println!("  ⚠️ Failed to create RAID bdev {}: {}", raid_bdev_name, e);
                    }
                }
            } else {
                println!("  ⚠️ Not enough healthy members for RAID disk {}", raid_disk_id);
            }
        }
        
        Ok(())
    }

    /// Create lvol store if it doesn't exist
    async fn create_lvol_store(&self, bdev_name: &str, lvs_name: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("  📦 Creating lvol store: {} on bdev: {}", lvs_name, bdev_name);
        
        let create_lvs = json!({
            "method": "bdev_lvol_create_lvstore",
            "params": {
                "bdev_name": bdev_name,
                "lvs_name": lvs_name
            }
        });
        
        match self.call_rpc(&create_lvs).await {
            Ok(response) => {
                if let Some(uuid) = response["result"].as_str() {
                    println!("  ✅ Created lvol store: {} (UUID: {})", lvs_name, uuid);
                }
            },
            Err(e) => {
                // Might already exist
                println!("  ℹ️ Lvol store creation returned: {}", e);
            }
        }
        
        Ok(())
    }

    /// Configure volumes based on SpdkVolume CRDs
    async fn configure_volumes(&self, _current_lvstores: &HashSet<String>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔧 [VOLUME_CONFIG] Configuring volumes from CRDs");
        
        let volumes_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), &self.namespace);
        let lp = ListParams::default();
        let volumes = volumes_api.list(&lp).await?;
        
        for volume in volumes.items {
            // Only process volumes for this node
            let node_id = match &volume.spec.storage_backend {
                StorageBackend::RaidDisk { node_id, .. } => node_id,
            };
            
            if node_id != &self.node_id {
                continue;
            }
            
            let volume_id = &volume.spec.volume_id;
            let lvol_name = format!("lvol_{}", volume_id);
            
            println!("📀 [VOLUME_CONFIG] Processing volume: {}", volume_id);
            
            // Get the lvs_name from the volume spec
            if let Some(lvs_name) = &volume.spec.lvs_name {
                // Check if lvol already exists
                let check_lvol = json!({
                    "method": "bdev_lvol_get_lvols",
                    "params": {
                        "lvs_name": lvs_name
                    }
                });
                
                println!("🔍 [CONFIG_DEBUG] Calling bdev_lvol_get_lvols for LVS: {}", lvs_name);
                println!("🔍 [CONFIG_DEBUG] Request payload: {}", serde_json::to_string_pretty(&check_lvol).unwrap_or_else(|_| "Failed to serialize".to_string()));
                
                match self.call_rpc(&check_lvol).await {
                    Ok(response) => {
                        println!("✅ [CONFIG_DEBUG] bdev_lvol_get_lvols response: {}", serde_json::to_string_pretty(&response).unwrap_or_else(|_| "Failed to serialize response".to_string()));
                        let mut lvol_exists = false;
                        if let Some(lvols) = response["result"].as_array() {
                            for lvol in lvols {
                                if let Some(name) = lvol["name"].as_str() {
                                    if name == lvol_name {
                                        lvol_exists = true;
                                        println!("  ℹ️ Lvol {} already exists", lvol_name);
                                        break;
                                    }
                                }
                            }
                        }
                        
                        if !lvol_exists {
                            // Create the lvol
                            println!("  Creating lvol: {} in lvs: {}", lvol_name, lvs_name);
                            
                            let create_lvol = json!({
                                "method": "bdev_lvol_create",
                                "params": {
                                    "lvs_name": lvs_name,
                                    "lvol_name": lvol_name.clone(),
                                    "size_in_mib": volume.spec.size_bytes / (1024 * 1024)
                                }
                            });
                            
                            match self.call_rpc(&create_lvol).await {
                                Ok(response) => {
                                    if let Some(uuid) = response["result"].as_str() {
                                        println!("  ✅ Created lvol: {} (UUID: {})", lvol_name, uuid);
                                    }
                                },
                                Err(e) => {
                                    println!("  ⚠️ Failed to create lvol {}: {}", lvol_name, e);
                                }
                            }
                        }
                    },
                    Err(e) => {
                        println!("❌ [CONFIG_DEBUG] bdev_lvol_get_lvols failed with error: {}", e);
                        println!("  ⚠️ Failed to check lvols: {}", e);
                    }
                }
            }
        }
        
        Ok(())
    }

    /// Configure snapshots based on SpdkSnapshot CRDs
    async fn configure_snapshots(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔧 [SNAPSHOT_CONFIG] Configuring snapshots from CRDs");
        
        let snapshots_api: Api<SpdkSnapshot> = Api::namespaced(self.kube_client.clone(), &self.namespace);
        let lp = ListParams::default();
        let snapshots = snapshots_api.list(&lp).await?;
        
        for snapshot in snapshots.items {
            let snapshot_id = &snapshot.spec.snapshot_id;
            println!("📸 [SNAPSHOT_CONFIG] Processing snapshot: {}", snapshot_id);
            
            // Process replica snapshots for this node
            for replica_snapshot in &snapshot.spec.replica_snapshots {
                if replica_snapshot.node_name != self.node_id {
                    continue;
                }
                
                // Extract LVS name from the SpdkRaidDisk that contains this replica
                let lvs_name = match self.find_lvs_name_for_replica(&replica_snapshot.disk_ref).await {
                    Ok(name) => name,
                    Err(e) => {
                        eprintln!("⚠️ [SNAPSHOT_CONFIG] Failed to find LVS name for replica {}: {}", replica_snapshot.disk_ref, e);
                        continue; // Skip this replica snapshot
                    }
                };
                {
                    let snapshot_name = format!("snapshot_{}", snapshot_id);
                    
                    // Check if snapshot already exists
                    let check_snapshot = json!({
                        "method": "bdev_lvol_get_lvols",
                        "params": {
                            "lvs_name": lvs_name
                        }
                    });
                    
                    match self.call_rpc(&check_snapshot).await {
                        Ok(response) => {
                            let mut snapshot_exists = false;
                            if let Some(lvols) = response["result"].as_array() {
                                for lvol in lvols {
                                    if let Some(name) = lvol["name"].as_str() {
                                        if name == snapshot_name {
                                            snapshot_exists = true;
                                            println!("  ℹ️ Snapshot {} already exists", snapshot_name);
                                            break;
                                        }
                                    }
                                }
                            }
                            
                            if !snapshot_exists {
                                // Create the snapshot
                                let parent_name = &replica_snapshot.source_lvol_bdev;
                                println!("  Creating snapshot: {} from parent: {}", snapshot_name, parent_name);
                                
                                let create_snapshot = json!({
                                    "method": "bdev_lvol_snapshot",
                                    "params": {
                                        "lvol_name": format!("{}/{}", lvs_name, parent_name),
                                        "snapshot_name": snapshot_name.clone()
                                    }
                                });
                                
                                match self.call_rpc(&create_snapshot).await {
                                    Ok(_) => {
                                        println!("  ✅ Created snapshot: {}", snapshot_name);
                                    },
                                    Err(e) => {
                                        println!("  ⚠️ Failed to create snapshot {}: {}", snapshot_name, e);
                                    }
                                }
                            }
                        },
                        Err(e) => {
                            println!("  ⚠️ Failed to check snapshots: {}", e);
                        }
                    }
                }
            }
        }
        
        Ok(())
    }

    /// Configure NVMe-oF subsystems for volumes
    async fn configure_nvmeof_subsystems(&self, current_nvmf_subsystems: &HashSet<String>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔧 [NVMF_CONFIG] Configuring NVMe-oF subsystems");
        
        // Get volumes that need NVMe-oF export
        let volumes_api: Api<SpdkVolume> = Api::namespaced(self.kube_client.clone(), &self.namespace);
        let lp = ListParams::default();
        let volumes = volumes_api.list(&lp).await?;
        
        for volume in volumes.items {
            // Only process volumes for this node
            let node_id = match &volume.spec.storage_backend {
                StorageBackend::RaidDisk { node_id, .. } => node_id,
            };
            
            if node_id != &self.node_id {
                continue;
            }
            
            // Check if NVMe-oF is configured for this volume
            if volume.spec.nvmeof_transport.is_some() {
                let nqn = format!("nqn.2023-01.io.flint:volume.{}", volume.spec.volume_id);
                
                if !current_nvmf_subsystems.contains(&nqn) {
                    println!("  Creating NVMe-oF subsystem: {}", nqn);
                    
                    // Create NVMe-oF subsystem
                    let create_subsystem = json!({
                        "method": "nvmf_create_subsystem",
                        "params": {
                            "nqn": nqn.clone(),
                            "allow_any_host": true,
                            "serial_number": format!("FLINT{}", volume.spec.volume_id)
                        }
                    });
                    
                    match self.call_rpc(&create_subsystem).await {
                        Ok(_) => {
                            println!("  ✅ Created NVMe-oF subsystem: {}", nqn);
                            
                            // Add namespace to the subsystem
                            let lvol_name = format!("lvol_{}", volume.spec.volume_id);
                            if let Some(lvs_name) = &volume.spec.lvs_name {
                                let add_ns = json!({
                                    "method": "nvmf_subsystem_add_ns",
                                    "params": {
                                        "nqn": nqn.clone(),
                                        "namespace": {
                                            "bdev_name": format!("{}/{}", lvs_name, lvol_name)
                                        }
                                    }
                                });
                                
                                match self.call_rpc(&add_ns).await {
                                    Ok(_) => {
                                        println!("  ✅ Added namespace to subsystem");
                                    },
                                    Err(e) => {
                                        println!("  ⚠️ Failed to add namespace: {}", e);
                                    }
                                }
                            }
                        },
                        Err(e) => {
                            println!("  ⚠️ Failed to create NVMe-oF subsystem: {}", e);
                        }
                    }
                } else {
                    println!("  ℹ️ NVMe-oF subsystem {} already exists", nqn);
                }
            }
        }
        
        Ok(())
    }

    /// Get device information from Custom Resources for this node
    /// This replaces physical scanning since all device info is in the CRDs
    pub async fn get_node_devices_from_crds(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        println!("🔍 [DISCOVERY] Getting device information from Custom Resources for node: {}", self.node_id);
        
        let mut discovered_devices = Vec::new();
        
        // Get RAID disks for this node
        let raid_disks_api: Api<SpdkRaidDisk> = Api::namespaced(self.kube_client.clone(), &self.namespace);
        let lp = ListParams::default();
        let raid_disks = raid_disks_api.list(&lp).await?;
        
        for raid_disk in raid_disks.items {
            // Only process RAID disks for this node
            if raid_disk.spec.created_on_node != self.node_id {
                continue;
            }
            
            println!("📦 [DISCOVERY] RAID Disk: {} (Level: {})", 
                raid_disk.spec.raid_disk_id, raid_disk.spec.raid_level);
            
            if let Some(status) = &raid_disk.status {
                if let Some(raid_bdev_name) = &status.raid_bdev_name {
                    discovered_devices.push(raid_bdev_name.clone());
                    println!("  - RAID bdev: {}", raid_bdev_name);
                }
                
                if let Some(lvs_name) = &status.lvs_name {
                    println!("  - LVS: {} (UUID: {})", 
                        lvs_name, 
                        status.lvs_uuid.as_deref().unwrap_or("unknown"));
                }
            }
            
            // List member disks for this node
            for member in &raid_disk.spec.member_disks {
                if member.node_id == self.node_id {
                    println!("  - Member disk: {} (State: {:?})", 
                        member.disk_ref, member.state);
                    
                    // Show NVMe-oF endpoint info if available
                    println!("    Endpoint: {}://{}", 
                        member.nvmeof_endpoint.transport, 
                        member.nvmeof_endpoint.target_addr);
                }
            }
        }
        
        println!("✅ [DISCOVERY] Found {} RAID devices for node {}", 
            discovered_devices.len(), self.node_id);
        
        Ok(discovered_devices)
    }

    /// Find the LVS name for a replica by looking up the SpdkRaidDisk it belongs to
    async fn find_lvs_name_for_replica(&self, disk_ref: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let raid_disks_api: Api<SpdkRaidDisk> = Api::namespaced(self.kube_client.clone(), &self.namespace);
        let raid_disks = raid_disks_api.list(&ListParams::default()).await?;

        // Search through all RAID disks to find the one that references this disk
        for raid_disk in &raid_disks.items {
            // Check if any member disk references this disk_ref
            for member in &raid_disk.spec.member_disks {
                if member.disk_ref == disk_ref {
                    // Found the RAID disk that contains this replica
                    let lvs_name = raid_disk.spec.lvs_name();
                    println!("🔍 [LVS_LOOKUP] Found LVS '{}' for replica '{}'", lvs_name, disk_ref);
                    return Ok(lvs_name);
                }
            }
        }

        Err(format!("No SpdkRaidDisk found containing replica disk_ref: {}", disk_ref).into())
    }

    /// System disk detection - prevent using boot/system disks for SPDK storage
    async fn is_system_disk(&self, device_name: &str) -> bool {
        use std::process::Command;
        
        println!("🛡️ [SYSTEM_DISK_CHECK] Checking if {} is a system disk", device_name);
        
        // Convert SPDK bdev name to raw device name for mount checking
        let raw_device_name = if device_name.starts_with("aio_") {
            device_name.strip_prefix("aio_").unwrap_or(device_name)
        } else {
            device_name
        };
        
        // Method 1: Check if it's mounted on root filesystem
        if let Ok(output) = Command::new("findmnt").args(["-n", "-o", "SOURCE", "/"]).output() {
            let root_source = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if root_source.contains(raw_device_name) {
                println!("🚨 [SYSTEM_DISK_CHECK] {} is mounted as root filesystem", device_name);
                return true;
            }
        }
        
        // Method 2: Check boot partitions (including EFI)
        let boot_paths = ["/boot", "/boot/efi", "/efi"];
        for boot_path in &boot_paths {
            if let Ok(output) = Command::new("findmnt").args(["-n", "-o", "SOURCE", boot_path]).output() {
                let boot_source = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if boot_source.contains(raw_device_name) {
                    println!("🚨 [SYSTEM_DISK_CHECK] {} contains boot partition at {}", device_name, boot_path);
                    return true;
                }
            }
        }
        
        // Method 3: Check if any partition is mounted on critical system paths
        let critical_paths = ["/", "/boot", "/var", "/usr", "/opt", "/home", "/tmp"];
        for path in &critical_paths {
            if let Ok(output) = Command::new("findmnt").args(["-n", "-o", "SOURCE", path]).output() {
                let source = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if source.contains(raw_device_name) {
                    println!("🚨 [SYSTEM_DISK_CHECK] {} is mounted on critical path {}", device_name, path);
                    return true;
                }
            }
        }
        
        // Method 4: Check swap devices
        if let Ok(output) = Command::new("swapon").args(["--show=NAME", "--noheadings"]).output() {
            let swap_devices = String::from_utf8_lossy(&output.stdout);
            if swap_devices.contains(raw_device_name) {
                println!("🚨 [SYSTEM_DISK_CHECK] {} is used as swap device", device_name);
                return true;
            }
        }
        
        // Method 5: Check all partitions on this device for system use
        if let Ok(output) = Command::new("lsblk")
            .args(["-n", "-o", "NAME", "-r", &format!("/dev/{}", raw_device_name)])
            .output()
        {
            let partitions = String::from_utf8_lossy(&output.stdout);
            for line in partitions.lines() {
                let partition = line.trim();
                if !partition.is_empty() && partition != raw_device_name {
                    // Check if this partition is mounted on critical paths
                    for path in &critical_paths {
                        if let Ok(output) = Command::new("findmnt").args(["-n", "-o", "SOURCE", path]).output() {
                            let source = String::from_utf8_lossy(&output.stdout).trim().to_string();
                            if source.contains(partition) {
                                println!("🚨 [SYSTEM_DISK_CHECK] Partition {} of {} is mounted on critical path {}", partition, device_name, path);
                                return true;
                            }
                        }
                    }
                }
            }
        }
        
        println!("✅ [SYSTEM_DISK_CHECK] {} is safe for SPDK operations", device_name);
        false
    }
}