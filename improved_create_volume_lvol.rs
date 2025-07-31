    async fn create_volume_lvol(
        &self,
        disk: &SpdkDisk,
        size_bytes: i64,
        volume_id: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        println!("🚀 [DEBUG] create_volume_lvol called - volume_id: {}, size: {} bytes", volume_id, size_bytes);
        let rpc_url = self.driver.get_rpc_url_for_node(&disk.spec.node_id).await?;
        println!("🚀 [DEBUG] RPC URL: {}", rpc_url);
        let http_client = HttpClient::new();
        
        // Get the actual LVS name from the disk status (don't guess it from metadata name)
        let lvs_name = disk.status.as_ref()
            .and_then(|s| s.lvs_name.as_ref())
            .ok_or("Disk does not have LVS initialized or LVS name missing")?
            .clone();
        
        let lvol_name = format!("vol_{}", volume_id);

        // Convert bytes to MiB as required by SPDK bdev_lvol_create RPC
        let size_in_mib = (size_bytes + 1048575) / 1048576; // Round up to nearest MiB

        let create_params = json!({
            "method": "bdev_lvol_create",
            "params": {
                "lvs_name": lvs_name,
                "lvol_name": lvol_name,
                "size_in_mib": size_in_mib,
                "thin_provision": false,
                "clear_method": "write_zeroes"
            }
        });

        println!("🔧 [CREATE_LVOL] Creating logical volume with parameters:");
        println!("   LVS name: '{}'", lvs_name);
        println!("   LVOL name: '{}'", lvol_name);
        println!("   Size: {} bytes", size_bytes);
        println!("   RPC URL: {}", rpc_url);
        println!("   Full JSON payload: {}", serde_json::to_string_pretty(&create_params).unwrap_or_else(|_| "Failed to serialize".to_string()));

        let lvol_response = http_client
            .post(&rpc_url)
            .json(&create_params)
            .send()
            .await?;

        let response_status = lvol_response.status();
        println!("📥 [CREATE_LVOL] Response status: {}", response_status);
        
        if !response_status.is_success() {
            let error_text = lvol_response.text().await?;
            println!("❌ [CREATE_LVOL] HTTP request failed with status {}: {}", response_status, error_text);
            
            // Check if this is a "File exists" error - if so, try to handle it idempotently
            if error_text.contains("File exists") || error_text.contains("Code=-17") {
                return self.handle_existing_volume(&rpc_url, &lvol_name, size_bytes).await;
            }
            
            return Err(format!("Failed to create lvol: {}", error_text).into());
        }

        // Get the response text first to log it, then parse as JSON
        let response_text = lvol_response.text().await?;
        println!("📥 [CREATE_LVOL] Raw response: {}", response_text);
        
        let lvol_info: serde_json::Value = match serde_json::from_str(&response_text) {
            Ok(json) => {
                println!("✅ [CREATE_LVOL] Successfully parsed response JSON");
                json
            }
            Err(e) => {
                println!("❌ [CREATE_LVOL] Failed to parse response as JSON: {}", e);
                println!("❌ [CREATE_LVOL] Raw response was: {}", response_text);
                return Err(format!("Failed to parse SPDK response as JSON: {}", e).into());
            }
        };

        // Check if the response contains an error
        if let Some(error) = lvol_info.get("error") {
            println!("❌ [CREATE_LVOL] SPDK returned error: {}", serde_json::to_string_pretty(error).unwrap_or_else(|_| format!("{:?}", error)));
            
            // Handle "File exists" error with idempotency
            if let Some(error_code) = error.get("code").and_then(|c| c.as_i64()) {
                if error_code == -17 {  // SPDK "File exists" error
                    println!("⚠️ [CREATE_LVOL] Volume already exists, checking compatibility...");
                    return self.handle_existing_volume(&rpc_url, &lvol_name, size_bytes).await;
                }
            }
            
            return Err(format!("SPDK RPC error: {}", serde_json::to_string(error).unwrap_or_else(|_| format!("{:?}", error))).into());
        }
        
        println!("🔍 [CREATE_LVOL] Extracting UUID from result...");
        let lvol_uuid = lvol_info["result"]["uuid"]
            .as_str()
            .ok_or_else(|| {
                let result = lvol_info.get("result").cloned().unwrap_or(json!(null));
                println!("❌ [CREATE_LVOL] No UUID found in result. Result section: {}", serde_json::to_string_pretty(&result).unwrap_or_else(|_| format!("{:?}", result)));
                "Failed to get lvol UUID from SPDK response"
            })?
            .to_string();

        println!("✅ [CREATE_LVOL] Successfully created logical volume with UUID: {}", lvol_uuid);

        Ok(lvol_uuid)
    }

    /// Handle the case where a logical volume already exists - implement CSI idempotency
    async fn handle_existing_volume(
        &self,
        rpc_url: &str,
        lvol_name: &str,
        requested_size_bytes: i64,
    ) -> Result<String, Box<dyn std::error::Error>> {
        println!("🔍 [IDEMPOTENT] Checking existing volume: {}", lvol_name);
        
        let http_client = HttpClient::new();
        
        // Query the existing logical volume
        let query_params = json!({
            "method": "bdev_get_bdevs",
            "params": {
                "name": lvol_name
            }
        });

        let query_response = http_client
            .post(rpc_url)
            .json(&query_params)
            .send()
            .await?;

        if !query_response.status().is_success() {
            let error_text = query_response.text().await?;
            println!("❌ [IDEMPOTENT] Failed to query existing volume: {}", error_text);
            return Err(format!("Failed to query existing volume: {}", error_text).into());
        }

        let query_result: serde_json::Value = query_response.json().await?;
        
        if let Some(error) = query_result.get("error") {
            println!("❌ [IDEMPOTENT] SPDK query error: {}", serde_json::to_string_pretty(error).unwrap_or_else(|_| format!("{:?}", error)));
            return Err(format!("Failed to query existing volume: {}", serde_json::to_string(error).unwrap_or_else(|_| format!("{:?}", error))).into());
        }

        if let Some(bdevs) = query_result.get("result").and_then(|r| r.as_array()) {
            if bdevs.is_empty() {
                println!("❌ [IDEMPOTENT] Volume {} not found, but creation failed with 'File exists'", lvol_name);
                return Err("Volume creation failed with 'File exists' but volume cannot be found".into());
            }

            let existing_bdev = &bdevs[0];
            let existing_size_bytes = existing_bdev["num_blocks"]
                .as_u64()
                .and_then(|blocks| existing_bdev["block_size"].as_u64().map(|bs| blocks * bs))
                .ok_or("Failed to get existing volume size")?;
                
            let existing_uuid = existing_bdev["uuid"]
                .as_str()
                .ok_or("Failed to get existing volume UUID")?;

            println!("🔍 [IDEMPOTENT] Found existing volume:");
            println!("   Name: {}", lvol_name);
            println!("   UUID: {}", existing_uuid);
            println!("   Size: {} bytes", existing_size_bytes);
            println!("   Requested size: {} bytes", requested_size_bytes);

            // Check if the size is compatible (allow some tolerance for MiB alignment)
            let size_tolerance = 1048576; // 1 MiB tolerance
            if (existing_size_bytes as i64 - requested_size_bytes).abs() <= size_tolerance {
                println!("✅ [IDEMPOTENT] Existing volume is compatible, returning existing UUID");
                return Ok(existing_uuid.to_string());
            } else {
                println!("❌ [IDEMPOTENT] Size mismatch - existing: {} bytes, requested: {} bytes", 
                        existing_size_bytes, requested_size_bytes);
                return Err(format!(
                    "Volume {} already exists with different size: existing {} bytes, requested {} bytes",
                    lvol_name, existing_size_bytes, requested_size_bytes
                ).into());
            }
        } else {
            println!("❌ [IDEMPOTENT] Unexpected response format from bdev_get_bdevs");
            return Err("Unexpected response format when querying existing volume".into());
        }
    }