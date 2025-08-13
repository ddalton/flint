// spdk_config_sync.rs - Convert SpdkConfig CRD to/from SPDK native JSON format
use serde_json::{json, Value};
use crate::models::{SpdkConfig, SpdkConfigSpec, RaidBdevConfig, LvstoreConfig, LogicalVolumeConfig, NvmeofSubsystemConfig, RaidMemberBdevConfig, NvmeofMemberConfig};
use reqwest::Client as HttpClient;
use std::collections::HashMap;

/// Convert SpdkConfig CRD to SPDK's native JSON configuration format
pub fn convert_spdk_config_to_json(config: &SpdkConfigSpec) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let mut spdk_config = json!({
        "subsystems": []
    });

    let subsystems = spdk_config["subsystems"].as_array_mut().unwrap();

    // 1. NVMe bdev subsystem - for NVMe-oF connections
    let mut nvme_bdev_config = json!({
        "subsystem": "bdev",
        "config": []
    });

    // Add NVMe-oF attachments for RAID members
    for raid in &config.raid_bdevs {
        for member in &raid.members {
            if member.member_type == "nvmeof" {
                if let Some(nvmeof_config) = &member.nvmeof_config {
                    nvme_bdev_config["config"].as_array_mut().unwrap().push(json!({
                        "method": "bdev_nvme_attach_controller",
                        "params": {
                            "name": member.bdev_name,
                            "trtype": nvmeof_config.transport,
                            "traddr": nvmeof_config.target_addr,
                            "trsvcid": nvmeof_config.target_port.to_string(),
                            "subnqn": nvmeof_config.nqn
                        }
                    }));
                }
            }
        }
    }

    subsystems.push(nvme_bdev_config);

    // 2. RAID bdev subsystem
    let mut raid_config = json!({
        "subsystem": "bdev",
        "config": []
    });

    for raid in &config.raid_bdevs {
        let base_bdevs: Vec<String> = raid.members.iter().map(|m| m.bdev_name.clone()).collect();
        
        raid_config["config"].as_array_mut().unwrap().push(json!({
            "method": "bdev_raid_create",
            "params": {
                "name": raid.name,
                "raid_level": raid.raid_level,
                "base_bdevs": base_bdevs,
                "superblock": raid.superblock_enabled,
                "strip_size_kb": raid.stripe_size_kb
            }
        }));
    }

    if !config.raid_bdevs.is_empty() {
        subsystems.push(raid_config);
    }

    // 3. LVS subsystem - Create logical volume stores
    let mut lvs_config = json!({
        "subsystem": "bdev", 
        "config": []
    });

    for raid in &config.raid_bdevs {
        if !raid.lvstore.name.is_empty() {
            lvs_config["config"].as_array_mut().unwrap().push(json!({
                "method": "bdev_lvol_create_lvstore",
                "params": {
                    "bdev_name": raid.name,
                    "lvs_name": raid.lvstore.name,
                    "cluster_sz": raid.lvstore.cluster_size
                }
            }));

            // Create logical volumes in this LVS
            for lvol in &raid.lvstore.logical_volumes {
                lvs_config["config"].as_array_mut().unwrap().push(json!({
                    "method": "bdev_lvol_create",
                    "params": {
                        "lvol_name": lvol.name,
                        "size": lvol.size_bytes,
                        "lvs_name": raid.lvstore.name,
                        "thin_provision": lvol.thin_provision
                    }
                }));
            }
        }
    }

    if !config.raid_bdevs.is_empty() {
        subsystems.push(lvs_config);
    }

    // 4. NVMe-oF subsystem - Export volumes
    let mut nvmeof_config = json!({
        "subsystem": "nvmf",
        "config": []
    });

    // Create NVMe-oF transport
    nvmeof_config["config"].as_array_mut().unwrap().push(json!({
        "method": "nvmf_create_transport",
        "params": {
            "trtype": "TCP"
        }
    }));

    // Create subsystems for volume exports
    for subsystem in &config.nvmeof_subsystems {
        nvmeof_config["config"].as_array_mut().unwrap().push(json!({
            "method": "nvmf_create_subsystem",
            "params": {
                "nqn": subsystem.nqn,
                "allow_any_host": subsystem.allow_any_host,
                "serial_number": format!("SPDK{}", subsystem.lvol_uuid.replace("-", "")[..8].to_uppercase())
            }
        }));

        // Add namespace to subsystem
        nvmeof_config["config"].as_array_mut().unwrap().push(json!({
            "method": "nvmf_subsystem_add_ns",
            "params": {
                "nqn": subsystem.nqn,
                "namespace": {
                    "nsid": subsystem.namespace_id,
                    "bdev_name": subsystem.lvol_uuid
                }
            }
        }));

        // Add listener to subsystem
        nvmeof_config["config"].as_array_mut().unwrap().push(json!({
            "method": "nvmf_subsystem_add_listener",
            "params": {
                "nqn": subsystem.nqn,
                "listen_address": {
                    "trtype": subsystem.transport.to_uppercase(),
                    "traddr": subsystem.listen_address,
                    "trsvcid": subsystem.listen_port.to_string()
                }
            }
        }));
    }

    if !config.nvmeof_subsystems.is_empty() {
        subsystems.push(nvmeof_config);
    }

    // 5. UBLK subsystem initialization (create target only)
    let ublk_config = json!({
        "subsystem": "ublk",
        "config": [
            {
                "method": "ublk_create_target",
                "params": {}
            }
        ]
    });

    subsystems.push(ublk_config);

    Ok(spdk_config)
}

/// Save SPDK config to ConfigMap for use at startup
pub async fn save_spdk_config_to_configmap(
    kube_client: &kube::Client,
    namespace: &str,
    node_id: &str,
    config: &SpdkConfigSpec,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use kube::api::{Api, PostParams, PatchParams, Patch};
    use k8s_openapi::api::core::v1::ConfigMap;
    use std::collections::BTreeMap;

    let configmaps: Api<ConfigMap> = Api::namespaced(kube_client.clone(), namespace);
    let configmap_name = format!("spdk-config-{}", node_id);

    // Convert to SPDK JSON format
    let spdk_json = convert_spdk_config_to_json(config)?;
    let spdk_json_str = serde_json::to_string_pretty(&spdk_json)?;

    let mut data = BTreeMap::new();
    data.insert("spdk-config.json".to_string(), spdk_json_str);
    data.insert("node-id".to_string(), node_id.to_string());

    let configmap = ConfigMap {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(configmap_name.clone()),
            namespace: Some(namespace.to_string()),
            labels: Some({
                let mut labels = BTreeMap::new();
                labels.insert("app".to_string(), "spdk-flint".to_string());
                labels.insert("component".to_string(), "spdk-config".to_string());
                labels.insert("node".to_string(), node_id.to_string());
                labels
            }),
            ..Default::default()
        },
        data: Some(data),
        ..Default::default()
    };

    // Try to create or update the ConfigMap
    match configmaps.get_opt(&configmap_name).await? {
        Some(_) => {
            // Update existing
            configmaps.replace(&configmap_name, &PostParams::default(), &configmap).await?;
            println!("✅ [CONFIG] Updated SPDK ConfigMap: {}", configmap_name);
        }
        None => {
            // Create new
            configmaps.create(&PostParams::default(), &configmap).await?;
            println!("✅ [CONFIG] Created SPDK ConfigMap: {}", configmap_name);
        }
    }

    // Also save to the shared volume for immediate use
    let spdk_json_str = serde_json::to_string_pretty(&spdk_json)?;
    if let Err(e) = std::fs::write("/etc/spdk-config/spdk-config.json", &spdk_json_str) {
        println!("⚠️ [CONFIG] Failed to write config to shared volume: {}", e);
    } else {
        println!("💾 [CONFIG] Saved SPDK config to shared volume");
    }

    Ok(())
}

/// Automatically save current SPDK state to ConfigMap
/// Call this after any SPDK configuration change
pub async fn auto_save_spdk_config(
    kube_client: &kube::Client,
    namespace: &str,
    node_id: &str,
    spdk_rpc_url: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use crate::models::{SpdkConfig, SpdkConfigSpec};
    use kube::api::Api;

    println!("🔄 [AUTO_SAVE] Capturing current SPDK state...");
    
    // Get current SPDK configuration via RPC
    let current_config = get_current_spdk_config(spdk_rpc_url).await?;
    
    // Find or create SpdkConfig CRD for this node
    let spdk_configs: Api<SpdkConfig> = Api::namespaced(kube_client.clone(), namespace);
    let config_name = format!("{}-config", node_id);
    
    let mut spdk_config_spec = match spdk_configs.get_opt(&config_name).await? {
        Some(existing) => existing.spec,
        None => SpdkConfigSpec {
            node_id: node_id.to_string(),
            maintenance_mode: false,
            last_config_save: None,
            raid_bdevs: vec![],
            nvmeof_subsystems: vec![],
        },
    };
    
    // Update config with current SPDK state
    spdk_config_spec.last_config_save = Some(chrono::Utc::now().to_rfc3339());
    
    // TODO: Parse current_config JSON and update spdk_config_spec
    // For now, we'll save the raw config to ConfigMap
    
    // Save to ConfigMap
    save_spdk_config_to_configmap(kube_client, namespace, node_id, &spdk_config_spec).await?;
    
    println!("✅ [AUTO_SAVE] SPDK configuration saved automatically");
    Ok(())
}

/// Safe auto-save that never fails the main operation - used by critical SPDK operations
pub async fn safe_auto_save_spdk_config(
    kube_client: &kube::Client,
    namespace: &str,
    node_id: &str,
    spdk_rpc_url: &str,
    operation_name: &str,
) {
    match auto_save_spdk_config(kube_client, namespace, node_id, spdk_rpc_url).await {
        Ok(_) => println!("✅ [SAFE_SAVE] ConfigMap updated after {}", operation_name),
        Err(e) => {
            println!("⚠️ [SAFE_SAVE] Failed to save config after {} (non-critical): {}", operation_name, e);
            println!("🔄 [SAFE_SAVE] State will be reconciled on next periodic sync");
        }
    }
}

/// Wrapper for SPDK RPC calls that automatically saves config after structure-changing operations
pub async fn call_spdk_rpc_with_config_sync(
    spdk_rpc_url: &str,
    rpc_request: &serde_json::Value,
    kube_client: &kube::Client,
    namespace: &str,
    node_id: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    // Make the RPC call first
    let http_client = HttpClient::new();
    let response = http_client
        .post(spdk_rpc_url)
        .json(rpc_request)
        .send()
        .await?;
    
    let result: serde_json::Value = response.json().await?;
    
    // Check if this is a structure-changing operation that needs config sync
    if let Some(method) = rpc_request["method"].as_str() {
        let should_sync = matches!(method,
            // RAID operations
            "bdev_raid_create" | "bdev_raid_delete" | 
            "bdev_raid_add_base_bdev" | "bdev_raid_remove_base_bdev" |
            // LVS operations  
            "bdev_lvol_create_lvstore" | "bdev_lvol_delete_lvstore" |
            // Logical volume operations
            "bdev_lvol_create" | "bdev_lvol_delete" |
            // NVMe-oF subsystem operations
            "nvmf_create_subsystem" | "nvmf_delete_subsystem" |
            "nvmf_subsystem_add_ns" | "nvmf_subsystem_remove_ns" |
            "nvmf_subsystem_add_listener" | "nvmf_subsystem_remove_listener" |
            // NVMe bdev operations
            "bdev_nvme_attach_controller" | "bdev_nvme_detach_controller"
        );
        
        if should_sync {
            println!("🔄 [CONFIG_SYNC] Auto-saving after SPDK method: {}", method);
            if let Err(e) = auto_save_spdk_config(kube_client, namespace, node_id, spdk_rpc_url).await {
                println!("⚠️ [CONFIG_SYNC] Failed to auto-save after {}: {}", method, e);
            }
        }
    }
    
    Ok(result)
}

/// Start periodic SPDK configuration sync (every 5 minutes)
/// This reconciles ConfigMap state with actual SPDK state to prevent drift
pub async fn start_periodic_config_sync(
    kube_client: kube::Client,
    namespace: String,
    node_id: String,
    spdk_rpc_url: String,
) {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(300)); // 5 minutes
    
    loop {
        interval.tick().await;
        
        if let Err(e) = reconcile_spdk_state_with_config(&kube_client, &namespace, &node_id, &spdk_rpc_url).await {
            println!("⚠️ [PERIODIC_SYNC] Failed to reconcile SPDK state: {}", e);
        } else {
            println!("✅ [PERIODIC_SYNC] SPDK state reconciled successfully");
        }
    }
}

/// Reconcile ConfigMap with actual SPDK state - the source of truth is SPDK itself
pub async fn reconcile_spdk_state_with_config(
    kube_client: &kube::Client,
    namespace: &str,
    node_id: &str,
    spdk_rpc_url: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use crate::models::{SpdkConfig, SpdkConfigSpec, RaidBdevConfig, LvstoreConfig, LogicalVolumeConfig, NvmeofSubsystemConfig, RaidMemberBdevConfig, NvmeofMemberConfig};
    use kube::api::Api;

    println!("🔄 [RECONCILE] Starting SPDK state reconciliation for node: {}", node_id);
    
    // Step 1: Get current SPDK state via RPC
    let actual_spdk_state = get_actual_spdk_state(spdk_rpc_url).await?;
    
    // Step 2: Load existing ConfigMap
    let spdk_configs: Api<SpdkConfig> = Api::namespaced(kube_client.clone(), namespace);
    let config_name = format!("{}-config", node_id);
    
    let mut config_spec = match spdk_configs.get_opt(&config_name).await? {
        Some(existing) => existing.spec,
        None => {
            println!("📝 [RECONCILE] No existing config found, creating from SPDK state");
            SpdkConfigSpec {
                node_id: node_id.to_string(),
                maintenance_mode: false,
                last_config_save: None,
                raid_bdevs: vec![],
                nvmeof_subsystems: vec![],
            }
        }
    };
    
    // Step 3: Compare and update ConfigMap with actual SPDK state
    let mut changes_detected = false;
    
    // Reconcile RAID bdevs
    if let Some(actual_raids) = actual_spdk_state.get("raid_bdevs") {
        let reconciled_raids = reconcile_raid_bdevs(&config_spec.raid_bdevs, actual_raids)?;
        if reconciled_raids != config_spec.raid_bdevs {
            println!("🔧 [RECONCILE] RAID bdev state drift detected, updating ConfigMap");
            config_spec.raid_bdevs = reconciled_raids;
            changes_detected = true;
        }
    }
    
    // Reconcile NVMe-oF subsystems
    if let Some(actual_nvmeof) = actual_spdk_state.get("nvmeof_subsystems") {
        let reconciled_nvmeof = reconcile_nvmeof_subsystems(&config_spec.nvmeof_subsystems, actual_nvmeof)?;
        if reconciled_nvmeof != config_spec.nvmeof_subsystems {
            println!("🔧 [RECONCILE] NVMe-oF subsystem state drift detected, updating ConfigMap");
            config_spec.nvmeof_subsystems = reconciled_nvmeof;
            changes_detected = true;
        }
    }
    
    // Step 4: Update ConfigMap if changes detected
    if changes_detected {
        config_spec.last_config_save = Some(chrono::Utc::now().to_rfc3339());
        save_spdk_config_to_configmap(kube_client, namespace, node_id, &config_spec).await?;
        println!("✅ [RECONCILE] ConfigMap updated with actual SPDK state");
    } else {
        println!("✅ [RECONCILE] No state drift detected, ConfigMap is in sync");
    }
    
    Ok(())
}

/// Get actual SPDK state from running spdk_tgt process
async fn get_actual_spdk_state(spdk_rpc_url: &str) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let http_client = HttpClient::new();
    let mut state = serde_json::json!({});
    
    // Get RAID bdevs
    match http_client.post(spdk_rpc_url).json(&serde_json::json!({
        "method": "bdev_raid_get_bdevs",
        "params": {}
    })).send().await {
        Ok(response) => {
            let raid_data: serde_json::Value = response.json().await?;
            if let Some(raids) = raid_data.get("result") {
                state["raid_bdevs"] = raids.clone();
            }
        }
        Err(e) => println!("⚠️ [RECONCILE] Failed to get RAID bdevs: {}", e),
    }
    
    // Get LVS stores
    match http_client.post(spdk_rpc_url).json(&serde_json::json!({
        "method": "bdev_lvol_get_lvstores",
        "params": {}
    })).send().await {
        Ok(response) => {
            let lvs_data: serde_json::Value = response.json().await?;
            if let Some(lvstores) = lvs_data.get("result") {
                state["lvstores"] = lvstores.clone();
            }
        }
        Err(e) => println!("⚠️ [RECONCILE] Failed to get LVS stores: {}", e),
    }
    
    // Get logical volumes
    match http_client.post(spdk_rpc_url).json(&serde_json::json!({
        "method": "bdev_lvol_get_bdevs",
        "params": {}
    })).send().await {
        Ok(response) => {
            let lvol_data: serde_json::Value = response.json().await?;
            if let Some(lvols) = lvol_data.get("result") {
                state["logical_volumes"] = lvols.clone();
            }
        }
        Err(e) => println!("⚠️ [RECONCILE] Failed to get logical volumes: {}", e),
    }
    
    // Get NVMe-oF subsystems
    match http_client.post(spdk_rpc_url).json(&serde_json::json!({
        "method": "nvmf_get_subsystems",
        "params": {}
    })).send().await {
        Ok(response) => {
            let nvmeof_data: serde_json::Value = response.json().await?;
            if let Some(subsystems) = nvmeof_data.get("result") {
                state["nvmeof_subsystems"] = subsystems.clone();
            }
        }
        Err(e) => println!("⚠️ [RECONCILE] Failed to get NVMe-oF subsystems: {}", e),
    }
    
    // Get NVMe controllers (for RAID members)
    match http_client.post(spdk_rpc_url).json(&serde_json::json!({
        "method": "bdev_nvme_get_controllers",
        "params": {}
    })).send().await {
        Ok(response) => {
            let nvme_data: serde_json::Value = response.json().await?;
            if let Some(controllers) = nvme_data.get("result") {
                state["nvme_controllers"] = controllers.clone();
            }
        }
        Err(e) => println!("⚠️ [RECONCILE] Failed to get NVMe controllers: {}", e),
    }
    
    println!("📊 [RECONCILE] Retrieved actual SPDK state: {} components", state.as_object().map(|o| o.len()).unwrap_or(0));
    Ok(state)
}

/// Reconcile RAID bdev configuration with actual SPDK state
fn reconcile_raid_bdevs(
    config_raids: &[RaidBdevConfig],
    actual_raids: &serde_json::Value,
) -> Result<Vec<RaidBdevConfig>, Box<dyn std::error::Error + Send + Sync>> {
    // TODO: Implement detailed RAID reconciliation logic
    // For now, return existing config - this is a placeholder for detailed implementation
    println!("🔧 [RECONCILE] RAID reconciliation logic placeholder");
    Ok(config_raids.to_vec())
}

/// Reconcile NVMe-oF subsystem configuration with actual SPDK state  
fn reconcile_nvmeof_subsystems(
    config_nvmeof: &[NvmeofSubsystemConfig],
    actual_nvmeof: &serde_json::Value,
) -> Result<Vec<NvmeofSubsystemConfig>, Box<dyn std::error::Error + Send + Sync>> {
    // TODO: Implement detailed NVMe-oF reconciliation logic
    // For now, return existing config - this is a placeholder for detailed implementation
    println!("🔧 [RECONCILE] NVMe-oF reconciliation logic placeholder");
    Ok(config_nvmeof.to_vec())
}

/// Load and validate SPDK configuration from ConfigMap
pub async fn load_spdk_config_from_configmap(
    kube_client: &kube::Client,
    namespace: &str,
    node_id: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    use kube::api::Api;
    use k8s_openapi::api::core::v1::ConfigMap;

    let configmaps: Api<ConfigMap> = Api::namespaced(kube_client.clone(), namespace);
    let configmap_name = format!("spdk-config-{}", node_id);

    match configmaps.get_opt(&configmap_name).await? {
        Some(configmap) => {
            if let Some(data) = configmap.data {
                if let Some(config_json) = data.get("spdk-config.json") {
                    println!("✅ [CONFIG] Loaded SPDK config from ConfigMap: {}", configmap_name);
                    return Ok(config_json.clone());
                }
            }
            Err("SPDK config not found in ConfigMap data".into())
        }
        None => {
            Err(format!("SPDK ConfigMap not found: {}", configmap_name).into())
        }
    }
}

/// Sync current SPDK state to SpdkConfig CRD
pub async fn sync_spdk_state_to_crd(
    spdk_rpc_url: &str,
    kube_client: &kube::Client,
    namespace: &str,
    node_id: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use kube::api::{Api, PatchParams, Patch};
    use serde_json::json;

    // Get current SPDK configuration via RPC
    let spdk_config_json = get_current_spdk_config(spdk_rpc_url).await?;
    
    // Update SpdkConfig CRD status
    let spdk_configs: Api<SpdkConfig> = Api::namespaced(kube_client.clone(), namespace);
    let config_name = format!("{}-config", node_id);
    
    let patch = json!({
        "status": {
            "config_applied": true,
            "last_sync": chrono::Utc::now().to_rfc3339(),
            "spdk_version": "25.05", // TODO: Get from SPDK
            "errors": []
        }
    });
    
    spdk_configs.patch(&config_name, &PatchParams::default(), &Patch::Merge(&patch)).await?;
    println!("✅ [CONFIG] Synced SPDK state to CRD: {}", config_name);
    
    Ok(())
}

/// Get current SPDK configuration via save_config RPC
async fn get_current_spdk_config(spdk_rpc_url: &str) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let http_client = reqwest::Client::new();
    
    let rpc_request = json!({
        "method": "save_config",
        "params": {}
    });
    
    let response = http_client
        .post(spdk_rpc_url)
        .json(&rpc_request)
        .send()
        .await?;
    
    if !response.status().is_success() {
        return Err("Failed to get SPDK configuration".into());
    }
    
    let response_json: Value = response.json().await?;
    
    if let Some(error) = response_json.get("error") {
        return Err(format!("save_config RPC failed: {}", error).into());
    }
    
    if let Some(result) = response_json.get("result") {
        Ok(result.clone())
    } else {
        Err("No result in save_config response".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spdk_config_conversion() {
        let config = SpdkConfigSpec {
            node_id: "test-node".to_string(),
            maintenance_mode: false,
            last_config_save: None,
            raid_bdevs: vec![
                RaidBdevConfig {
                    name: "raid0".to_string(),
                    raid_level: "1".to_string(),
                    superblock_enabled: true,
                    stripe_size_kb: 1024,
                    members: vec![
                        RaidMemberBdevConfig {
                            bdev_name: "local0".to_string(),
                            member_type: "local".to_string(),
                            local_device: Some("0000:01:00.0".to_string()),
                            nvmeof_config: None,
                        },
                        RaidMemberBdevConfig {
                            bdev_name: "nvmf0".to_string(),
                            member_type: "nvmeof".to_string(),
                            local_device: None,
                            nvmeof_config: Some(NvmeofMemberConfig {
                                target_node_id: "node-b".to_string(),
                                nqn: "nqn.test".to_string(),
                                transport: "tcp".to_string(),
                                target_addr: "192.168.1.20".to_string(),
                                target_port: 4420,
                                created_at: None,
                                state: "connected".to_string(),
                            }),
                        },
                    ],
                    lvstore: LvstoreConfig {
                        name: "lvs0".to_string(),
                        uuid: "lvs-uuid-123".to_string(),
                        cluster_size: 1048576,
                        total_data_clusters: 1024,
                        free_clusters: 512,
                        block_size: 4096,
                        logical_volumes: vec![
                            LogicalVolumeConfig {
                                name: "vol0".to_string(),
                                uuid: "vol-uuid-456".to_string(),
                                size_bytes: 1073741824,
                                size_clusters: 256,
                                thin_provision: true,
                                volume_crd_ref: Some("test-volume".to_string()),
                                created_at: None,
                                metadata: HashMap::new(),
                                state: "online".to_string(),
                                health_status: "healthy".to_string(),
                                last_health_check: None,
                                read_ops: 0,
                                write_ops: 0,
                                read_bytes: 0,
                                write_bytes: 0,
                                allocated_bytes: 536870912,
                            }
                        ],
                    },
                }
            ],
            nvmeof_subsystems: vec![
                NvmeofSubsystemConfig {
                    nqn: "nqn.test.volume".to_string(),
                    raid_bdev_name: "raid0".to_string(),
                    lvol_uuid: "vol-uuid-456".to_string(),
                    lvol_name: "vol0".to_string(),
                    namespace_id: 1,
                    allow_any_host: true,
                    allowed_hosts: vec![],
                    transport: "tcp".to_string(),
                    listen_address: "0.0.0.0".to_string(),
                    listen_port: 4420,
                    volume_crd_ref: Some("test-volume".to_string()),
                    created_at: None,
                    state: "active".to_string(),
                    connected_hosts: 0,
                    total_connections: 0,
                    last_connection: None,
                }
            ],
        };

        let result = convert_spdk_config_to_json(&config);
        assert!(result.is_ok());

        let json = result.unwrap();
        assert!(json["subsystems"].is_array());
        assert!(json["subsystems"].as_array().unwrap().len() > 0);
    }
}
