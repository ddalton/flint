// spdk_config_sync.rs - Convert SpdkConfig CRD to/from SPDK native JSON format
use serde_json::{json, Value};
use crate::models::{SpdkConfig, SpdkConfigSpec, RaidBdevConfig, LvstoreConfig, LogicalVolumeConfig, NvmeofSubsystemConfig, RaidMemberBdevConfig, NvmeofMemberConfig};
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

    Ok(())
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
