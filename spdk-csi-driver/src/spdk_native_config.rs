// spdk_native_config.rs - SPDK Native Configuration Management
// Rust implementation of SPDK's save_config/load_config functionality

use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use kube::api::{Api, PostParams};
use kube::Client;
use k8s_openapi::api::core::v1::ConfigMap;
use std::collections::BTreeMap;


/// SPDK Native Configuration Manager
/// Implements save_config/load_config logic from SPDK's Python RPC script
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

    /// Simple RPC call to SPDK - avoiding complex imports
    async fn call_rpc(&self, rpc_request: &Value) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let client = reqwest::Client::new();
        let response = client
            .post(&self.spdk_rpc_url)
            .json(rpc_request)
            .send()
            .await?;
        
        let result: Value = response.json().await?;
        Ok(result)
    }

    /// Save current SPDK configuration using native save_config logic
    /// Equivalent to SPDK's Python save_config() function
    pub async fn save_config(&self) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        println!("📸 [NATIVE_SAVE] Capturing SPDK configuration for node: {}", self.node_id);

        // Step 1: Get all subsystems (equivalent to framework_get_subsystems)
        let subsystems_response = self.call_rpc(&json!({
            "method": "framework_get_subsystems",
            "params": {}
        })).await?;

        let subsystems_list = subsystems_response["result"].as_array()
            .ok_or("Invalid framework_get_subsystems response")?;

        // Step 2: Build dependency map (like Python implementation)
        let mut dependencies: HashMap<String, HashSet<String>> = HashMap::new();
        for subsystem in subsystems_list {
            let name = subsystem["subsystem"].as_str()
                .ok_or("Missing subsystem name")?;
            
            let mut deps = HashSet::new();
            deps.insert(name.to_string());
            
            if let Some(depends_on) = subsystem["depends_on"].as_array() {
                for dep in depends_on {
                    if let Some(dep_name) = dep.as_str() {
                        if let Some(dep_set) = dependencies.get(dep_name) {
                            deps.extend(dep_set.clone());
                        }
                    }
                }
            }
            
            dependencies.insert(name.to_string(), deps);
        }

        // Step 3: Get configuration for each subsystem
        let mut config = json!({
            "subsystems": []
        });

        for subsystem in subsystems_list {
            let subsystem_name = subsystem["subsystem"].as_str()
                .ok_or("Missing subsystem name")?;

            println!("🔧 [NATIVE_SAVE] Getting config for subsystem: {}", subsystem_name);

            // Get subsystem configuration (equivalent to framework_get_config)
            let config_response = self.call_rpc(&json!({
                "method": "framework_get_config", 
                "params": {
                    "name": subsystem_name
                }
            })).await?;

            let subsystem_config = config_response["result"].clone();

            // Only include subsystems with non-empty configuration
            if let Some(config_array) = subsystem_config.as_array() {
                if !config_array.is_empty() {
                    config["subsystems"].as_array_mut().unwrap().push(json!({
                        "subsystem": subsystem_name,
                        "config": subsystem_config
                    }));
                    
                    println!("✅ [NATIVE_SAVE] Captured {} config items for {}", 
                             config_array.len(), subsystem_name);
                }
            }
        }

        println!("✅ [NATIVE_SAVE] Successfully captured SPDK configuration");
        Ok(config)
    }

    /// Load SPDK configuration using native load_config logic
    /// Equivalent to SPDK's Python load_config() function
    pub async fn load_config(&self, config: &Value) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🚀 [NATIVE_LOAD] Loading SPDK configuration for node: {}", self.node_id);

        let subsystems = config["subsystems"].as_array()
            .ok_or("Invalid config format: missing subsystems")?;

        if subsystems.is_empty() {
            println!("ℹ️ [NATIVE_LOAD] No subsystems to configure, calling framework_start_init");
            
            self.call_rpc(&json!({
                "method": "framework_start_init",
                "params": {}
            })).await?;
            
            return Ok(());
        }

        // Step 1: Validate all methods exist (like Python implementation)
        let allowed_methods_response = self.call_rpc(&json!({
            "method": "rpc_get_methods",
            "params": {
                "include_aliases": false
            }
        })).await?;

        let allowed_methods: HashSet<String> = allowed_methods_response["result"].as_array()
            .ok_or("Invalid rpc_get_methods response")?
            .iter()
            .filter_map(|method| method.as_str())
            .map(|s| s.to_string())
            .collect();

        // Validate all methods in config are known
        for subsystem in subsystems {
            if let Some(config_items) = subsystem["config"].as_array() {
                for item in config_items {
                    if let Some(method) = item["method"].as_str() {
                        if !allowed_methods.contains(method) {
                            return Err(format!("Unknown method in config: {}", method).into());
                        }
                    }
                }
            }
        }

        // Step 2: Apply configuration iteratively (like Python implementation)
        let mut remaining_subsystems: Vec<Value> = subsystems.iter().cloned().collect();

        while !remaining_subsystems.is_empty() {
            // Get currently allowed methods
            let current_methods_response = self.call_rpc(&json!({
                "method": "rpc_get_methods",
                "params": {
                    "current": true,
                    "include_aliases": false
                }
            })).await?;

            let current_methods: HashSet<String> = current_methods_response["result"].as_array()
                .ok_or("Invalid current methods response")?
                .iter()
                .filter_map(|method| method.as_str())
                .map(|s| s.to_string())
                .collect();

            let mut progress_made = false;

            // Try to apply configurations that are currently allowed
            for subsystem in &mut remaining_subsystems {
                if let Some(config_items) = subsystem["config"].as_array_mut() {
                    let mut items_to_remove = Vec::new();
                    
                    for (index, item) in config_items.iter().enumerate() {
                        if let Some(method) = item["method"].as_str() {
                            if current_methods.contains(method) {
                                println!("🔧 [NATIVE_LOAD] Applying: {}", method);
                                
                                // Execute the RPC call
                                let result = self.call_rpc(item).await;
                                
                                match result {
                                    Ok(_) => {
                                        println!("✅ [NATIVE_LOAD] Successfully applied: {}", method);
                                        items_to_remove.push(index);
                                        progress_made = true;
                                    },
                                    Err(e) => {
                                        println!("⚠️ [NATIVE_LOAD] Failed to apply {}: {}", method, e);
                                        // Continue with other methods
                                    }
                                }
                            }
                        }
                    }
                    
                    // Remove successfully applied items (in reverse order to maintain indices)
                    for &index in items_to_remove.iter().rev() {
                        config_items.remove(index);
                    }
                }
            }

            // Remove subsystems with no remaining configuration
            remaining_subsystems.retain(|subsystem| {
                if let Some(config_items) = subsystem["config"].as_array() {
                    !config_items.is_empty()
                } else {
                    false
                }
            });

            // Call framework_start_init if available
            if current_methods.contains("framework_start_init") {
                println!("🚀 [NATIVE_LOAD] Calling framework_start_init");
                
                let _ = self.call_rpc(&json!({
                    "method": "framework_start_init",
                    "params": {}
                })).await;
                
                progress_made = true;
            }

            // If no progress was made, exit to avoid infinite loop
            if !progress_made {
                if !remaining_subsystems.is_empty() {
                    println!("⚠️ [NATIVE_LOAD] Some configurations could not be applied - RPC state may have moved past applicable window");
                }
                break;
            }
        }

        println!("✅ [NATIVE_LOAD] SPDK configuration loading completed");
        Ok(())
    }

    /// Save configuration to Kubernetes ConfigMap
    pub async fn save_to_configmap(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Get current SPDK configuration
        let config = self.save_config().await?;
        let config_json = serde_json::to_string_pretty(&config)?;

        println!("💾 [CONFIGMAP] Saving {} bytes of SPDK config to ConfigMap", config_json.len());

        let config_maps: Api<ConfigMap> = Api::namespaced(self.kube_client.clone(), &self.namespace);
        let config_map_name = format!("spdk-native-config-{}", self.node_id);

        let mut data = BTreeMap::new();
        data.insert("spdk-config.json".to_string(), config_json);
        data.insert("saved-at".to_string(), chrono::Utc::now().to_rfc3339());
        data.insert("node-id".to_string(), self.node_id.clone());

        let config_map = ConfigMap {
            metadata: kube::api::ObjectMeta {
                name: Some(config_map_name.clone()),
                namespace: Some(self.namespace.clone()),
                labels: Some({
                    let mut labels = BTreeMap::new();
                    labels.insert("app".to_string(), "spdk-csi-driver".to_string());
                    labels.insert("component".to_string(), "native-config".to_string());
                    labels.insert("node".to_string(), self.node_id.clone());
                    labels
                }),
                ..Default::default()
            },
            data: Some(data),
            ..Default::default()
        };

        // Replace or create ConfigMap
        match config_maps.replace(&config_map_name, &PostParams::default(), &config_map).await {
            Ok(_) => println!("✅ [CONFIGMAP] Updated ConfigMap: {}", config_map_name),
            Err(_) => {
                config_maps.create(&PostParams::default(), &config_map).await?;
                println!("✅ [CONFIGMAP] Created ConfigMap: {}", config_map_name);
            }
        }

        Ok(())
    }

    /// Load configuration from Kubernetes ConfigMap
    pub async fn load_from_configmap(&self) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let config_maps: Api<ConfigMap> = Api::namespaced(self.kube_client.clone(), &self.namespace);
        let config_map_name = format!("spdk-native-config-{}", self.node_id);

        match config_maps.get_opt(&config_map_name).await? {
            Some(config_map) => {
                if let Some(data) = config_map.data {
                    if let Some(config_json) = data.get("spdk-config.json") {
                        println!("📋 [CONFIGMAP] Found saved configuration ({} bytes)", config_json.len());

                        let config: Value = serde_json::from_str(config_json)?;
                        self.load_config(&config).await?;

                        println!("✅ [CONFIGMAP] Successfully restored SPDK configuration");
                        return Ok(true);
                    }
                }
                println!("⚠️ [CONFIGMAP] ConfigMap exists but missing config data");
                Ok(false)
            },
            None => {
                println!("ℹ️ [CONFIGMAP] No saved configuration found");
                Ok(false)
            }
        }
    }

    /// Start periodic configuration saving (every 5 minutes)
    pub async fn start_periodic_save(&self) {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(300)); // 5 minutes
        
        loop {
            interval.tick().await;
            
            println!("⏰ [PERIODIC_SAVE] Running 5-minute SPDK config save");
            
            match self.save_to_configmap().await {
                Ok(_) => println!("✅ [PERIODIC_SAVE] Successfully saved SPDK configuration"),
                Err(e) => println!("⚠️ [PERIODIC_SAVE] Failed to save configuration: {}", e),
            }
        }
    }
}
