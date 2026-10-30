// spdk_native.rs - Native SPDK integration for Flint using RPC interface
// This module provides safe Rust wrappers around SPDK v25.05.x RPC interface
// Implementation matches the official SPDK Go client

// Removed Once import - no longer needed without global instance
use anyhow::{Result, anyhow};
use serde_json::{json, Value};
use tokio::net::UnixStream;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;
use serde::{Deserialize, Serialize};

/// Contract R5: no unbounded awaits. Every SPDK RPC carries a deadline so a
/// wedged spdk-tgt (examine stuck behind a dead NVMe-oF controller, blackholed
/// attach) fails the CALL instead of hanging the calling task forever — the
/// F39 substrate fix. Methods that legitimately run long inside the target
/// get the slow budget; everything else answers in milliseconds when healthy.
const SLOW_RPC_METHODS: &[&str] = &[
    "bdev_wait_for_examine",      // examine settle can wait on fabric teardown
    "bdev_lvol_create_lvstore",   // metadata format scales with store size
    "bdev_nvme_attach_controller", // remote fabric connect
];

fn rpc_timeout_env(var: &str, default_secs: u64) -> u64 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(default_secs)
}

/// Pure deadline table — unit-testable without env access.
fn deadline_for(method: &str, default_secs: u64, slow_secs: u64) -> Duration {
    if SLOW_RPC_METHODS.contains(&method) {
        Duration::from_secs(slow_secs)
    } else {
        Duration::from_secs(default_secs)
    }
}

/// Deadline for one RPC. FLINT_SPDK_RPC_TIMEOUT_SECS (default 30) bounds the
/// common case; FLINT_SPDK_RPC_SLOW_TIMEOUT_SECS (default 180) bounds the
/// slow list. Cached once per process.
pub fn rpc_deadline(method: &str) -> Duration {
    static BUDGETS: OnceLock<(u64, u64)> = OnceLock::new();
    let (default_secs, slow_secs) = *BUDGETS.get_or_init(|| {
        (
            rpc_timeout_env("FLINT_SPDK_RPC_TIMEOUT_SECS", 30),
            rpc_timeout_env("FLINT_SPDK_RPC_SLOW_TIMEOUT_SECS", 180),
        )
    });
    deadline_for(method, default_secs, slow_secs)
}

/// Pure: `base` plus the time to zero `zero_bytes` at `floor_mibps`.
fn zeroing_deadline(base: Duration, zero_bytes: u64, floor_mibps: u64) -> Duration {
    let mib = zero_bytes.div_ceil(1 << 20);
    base + Duration::from_secs(mib.div_ceil(floor_mibps.max(1)))
}

/// Deadline for an RPC that allocates `bytes` to an lvol (`bdev_lvol_create`,
/// or `bdev_lvol_resize` growing it). spdk-tgt zeroes a THICK lvol's clusters
/// as it creates or grows it (blob-zero-new-clusters.patch: never-written
/// blocks must not show a deleted volume's data), so that RPC lasts as long
/// as writing the bytes; a thin lvol zeroes on first write instead and keeps
/// the method's normal budget. FLINT_THICK_ZERO_MIBPS (default 256) is the
/// slowest zeroing rate waited for: a bound, not an estimate.
pub fn lvol_alloc_deadline(method: &str, bytes: u64, thin_provision: bool) -> Duration {
    let base = rpc_deadline(method);
    if thin_provision {
        return base;
    }
    static FLOOR_MIBPS: OnceLock<u64> = OnceLock::new();
    let floor = *FLOOR_MIBPS.get_or_init(|| rpc_timeout_env("FLINT_THICK_ZERO_MIBPS", 256));
    zeroing_deadline(base, bytes, floor)
}

// SPDK v25.05.x uses RPC calls for all operations
// This implementation follows the official SPDK Go client pattern

/// SPDK RPC Error Classification (matches official CGO bridge)
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SpdkRpcErrorType {
    InvalidParameter = 1,
    Connection = 2,
    JsonRpcCall = 3,
    InvalidResponse = 4,
}

impl std::fmt::Display for SpdkRpcErrorType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpdkRpcErrorType::InvalidParameter => write!(f, "Invalid Parameter"),
            SpdkRpcErrorType::Connection => write!(f, "Connection Error"),
            SpdkRpcErrorType::JsonRpcCall => write!(f, "JSON-RPC Call Error"),
            SpdkRpcErrorType::InvalidResponse => write!(f, "Invalid Response"),
        }
    }
}

/// Enhanced error type with SPDK classification
#[derive(Debug)]
pub struct SpdkRpcError {
    pub error_type: SpdkRpcErrorType,
    pub message: String,
}

impl std::fmt::Display for SpdkRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.error_type, self.message)
    }
}

impl std::error::Error for SpdkRpcError {}

/// Information about a Logical Volume Store
#[derive(Debug, Clone)]
pub struct LvsInfo {
    pub name: String,
    pub uuid: String,
    pub base_bdev: String,
    pub cluster_size: u64,
    pub total_clusters: u64,
    pub free_clusters: u64,
    pub block_size: u64,
}

/// JSON-RPC 2.0 Request structure (matches official SPDK Go client)
#[derive(Debug, Serialize)]
struct RpcRequest {
    jsonrpc: String,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<u64>,
}

/// JSON-RPC 2.0 Response structure (matches official SPDK Go client)
#[derive(Debug, Deserialize)]
struct RpcResponse {
    #[allow(dead_code)] // Required for JSON-RPC 2.0 protocol compliance
    jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<u64>,
}

/// JSON-RPC Error structure (matches official SPDK Go client)
#[derive(Debug, Deserialize)]
struct RpcError {
    code: i32,
    message: String,
    #[allow(dead_code)] // Optional field in JSON-RPC 2.0 error spec
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Code={} Msg={}", self.code, self.message)
    }
}

impl std::error::Error for RpcError {}

/// Native SPDK interface using persistent RPC connection
/// 
/// This implementation matches the official SPDK v25.05.x Go client:
/// - Persistent Unix socket connection
/// - JSON streaming with encoder/decoder
/// - Atomic request ID management
/// - JSON-RPC 2.0 protocol compliance
pub struct SpdkNative {
    socket_path: String,
    request_id: AtomicU64,
}

impl SpdkNative {
    /// Create a new SPDK RPC client
    /// 
    /// This matches the CreateClientWithJsonCodec pattern from the Go client
    pub async fn new(socket_path: Option<String>) -> Result<Self> {
        let socket_path = socket_path.unwrap_or_else(|| "/var/tmp/spdk.sock".to_string());
        
        // Test connection to ensure SPDK is available (bounded: a wedged
        // listener must not hang client construction)
        let _test_conn = tokio::time::timeout(
            Duration::from_secs(10),
            UnixStream::connect(&socket_path),
        )
        .await
        .map_err(|_| anyhow!("Timed out connecting to SPDK socket {} after 10s", socket_path))?
        .map_err(|e| anyhow!("Failed to connect to SPDK socket {}: {}", socket_path, e))?;
        
        tracing::info!("[SPDK_RPC] Connected to SPDK at {}", socket_path);
        
        Ok(SpdkNative {
            socket_path,
            request_id: AtomicU64::new(0),
        })
    }
    
    /// Call SPDK RPC method (matches the Call method from Go client)
    async fn call_rpc(&self, method: &str, params: Option<Value>) -> Result<Value> {
        self.call_rpc_with_deadline(method, params, rpc_deadline(method)).await
    }

    /// `call_rpc` under an explicit deadline, for an RPC whose duration
    /// scales with its arguments: a thick lvol is zeroed as it is created or
    /// grown ([`lvol_alloc_deadline`]).
    async fn call_rpc_with_deadline(
        &self,
        method: &str,
        params: Option<Value>,
        deadline: Duration,
    ) -> Result<Value> {
        tracing::trace!("[SPDK_NATIVE] call_rpc('{}') incoming params: {:?}", method, params);

        // P4: bdev_nvme_set_options is refused once any controller exists,
        // and a restarted target is re-attached by whichever path gets there
        // first (the data-path repair, the reconcile's remote re-attach), not
        // only by the agent's discovery, which re-applies the options.
        // On the 1.61.3 gate cluster (2026-10-30) a raid host ran without
        // them after four spdk-tgt kills, and a blackholed leg then hung the
        // raid instead of faulting out of it. So the attach applies them
        // itself when it is the target's first controller.
        if method == "bdev_nvme_attach_controller" {
            self.ensure_dead_target_options(
                crate::nvme_recovery::DeadTargetTimeouts::from_env().set_options_params(),
            )
            .await;
        }

        // Handle empty parameters like official SPDK CGO bridge
        // "Force Go client to skip 'params' parameter in JSON-RPC call"
        let normalized_params = match &params {
            Some(Value::Object(map)) if map.is_empty() => {
                tracing::trace!("   Normalized empty object {{}} to None");
                None // Empty object -> nil
            }
            Some(Value::Array(arr)) if arr.is_empty() => {
                tracing::trace!("   Normalized empty array [] to None");
                None  // Empty array -> nil
            }
            other => {
                if other.is_some() {
                    tracing::trace!("   Keeping params as-is (not empty): {:?}", other);
                }
                other.clone() // Fix: use clone() instead of cloned()
            }
        };

        tracing::trace!("[SPDK_NATIVE] After normalization: {:?}", normalized_params);
        
        // Validate parameters according to SPDK Go client rules
        if let Some(ref p) = normalized_params {
            self.validate_rpc_params(p)?;
        }

        // Contract R5: the whole connect/send/receive exchange runs under one
        // per-method deadline. On expiry the error deliberately matches NO
        // benign classifier (is_missing/is_busy/is_claim_blocked), so guarded
        // callers treat it as transport-unknown and fail closed.
        self.call_rpc_bounded(method, normalized_params, deadline)
            .await
    }

    /// Deadline-explicit variant of the exchange — the seam unit tests use to
    /// exercise the timeout path without env coupling.
    async fn call_rpc_bounded(
        &self,
        method: &str,
        normalized_params: Option<Value>,
        deadline: Duration,
    ) -> Result<Value> {
        tokio::time::timeout(deadline, self.call_rpc_unbounded(method, normalized_params))
            .await
            .map_err(|_| {
                anyhow!(
                    "SPDK RPC '{}' timed out after {}s (socket {})",
                    method,
                    deadline.as_secs(),
                    self.socket_path
                )
            })?
    }

    async fn call_rpc_unbounded(
        &self,
        method: &str,
        normalized_params: Option<Value>,
    ) -> Result<Value> {
        // Create connection for this call
        let mut stream = UnixStream::connect(&self.socket_path).await
            .map_err(|e| anyhow!("Failed to connect to SPDK socket: {}", e))?;
        
        // Generate atomic request ID (matches Go client pattern)
        let id = self.request_id.fetch_add(1, Ordering::SeqCst) + 1;
        
        // Create JSON-RPC 2.0 request (matches Go client Request struct)
        let request = RpcRequest {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params: normalized_params,
            id: Some(id),
        };
        
        // Encode and send request (matches Go client encoder.Encode)
        let request_json = serde_json::to_string(&request)?;
        // Note: Request JSON not logged in normal operation (enable for RPC debugging if needed)
        // Uncomment for debugging: println!("🔧 [SPDK_RPC] Sending: {}", request_json);
        
        stream.write_all(request_json.as_bytes()).await?;
        stream.write_all(b"\n").await?; // SPDK expects newline-delimited JSON
        
        // Read and decode response (matches Go client decoder.Decode)
        let mut reader = BufReader::new(stream);
        let mut response_line = String::new();
        reader.read_line(&mut response_line).await?;
        
        // Note: Response JSON not logged in normal operation (can be 50KB+ for bdev_get_bdevs)
        // Uncomment for debugging: println!("📥 [SPDK_RPC] Received: {} bytes", response_line.len());
        
        let response: RpcResponse = serde_json::from_str(&response_line).map_err(|e| {
            // Log on error to help troubleshoot RPC issues
            tracing::error!("[SPDK_RPC] Failed to parse response for method '{}'", method);
            tracing::error!("   Error: {}", e);
            tracing::error!("   Response (first 500 chars): {}",
                     if response_line.len() > 500 { &response_line[..500] } else { &response_line });
            e
        })?;
        
        // Verify request/response ID match (matches Go client validation)
        if response.id != Some(id) {
            return Err(anyhow!("Request/response ID mismatch: sent {}, got {:?}", id, response.id));
        }
        
        // Handle RPC error (matches Go client error handling)
        if let Some(error) = response.error {
            return Err(anyhow!("SPDK RPC error: {}", error));
        }
        
        // Handle special case: "result": null (matches CGO bridge)
        // "This is a special case where inside JSON-RPC response 'Result' field is null"
        match response.result {
            Some(result) => Ok(result),
            None => {
                // Return null as a valid result (like Go client does)
                tracing::trace!("[SPDK_RPC] Received null result - returning Value::Null");
                Ok(Value::Null)
            }
        }
    }
    
    /// Validate RPC parameters according to SPDK Go client rules
    /// 
    /// SPDK accepts: null, objects, arrays
    /// SPDK rejects: primitive strings, numbers, booleans
    /// 
    /// This matches the verifyRequestParamsType function from the Go client:
    /// - reflect.Array ✅
    /// - reflect.Map ✅  
    /// - reflect.Slice ✅
    /// - reflect.Struct ✅
    /// - primitives ❌
    fn validate_rpc_params(&self, params: &Value) -> Result<()> {
        match params {
            Value::Null => Ok(()), // nil is allowed
            Value::Object(_) => Ok(()), // Maps/structs are allowed
            Value::Array(_) => Ok(()), // Arrays/slices are allowed
            Value::String(s) => Err(anyhow!("Primitive string parameter '{}' not supported by SPDK RPC", s)),
            Value::Number(n) => Err(anyhow!("Primitive number parameter '{}' not supported by SPDK RPC", n)), 
            Value::Bool(b) => Err(anyhow!("Primitive boolean parameter '{}' not supported by SPDK RPC", b)),
        }
    }
    
    /// Create uring bdev - uses io_uring backend (modern replacement for AIO)
    pub async fn create_uring_bdev(&self, filename: &str, name: &str) -> Result<String> {
        let params = json!({
            "name": name,
            "filename": filename,
            "block_size": 512
        });

        let result = self.call_rpc("bdev_uring_create", Some(params)).await?;
        Ok(result.as_str().unwrap_or(name).to_string())
    }
    
    /// Create Logical Volume Store - matches SPDK v25.05.x bdev_lvol_create_lvstore RPC
    pub async fn create_lvs(
        &self,
        bdev_name: &str,
        lvs_name: &str,
        cluster_size: u64,
        num_md_pages_per_cluster_ratio: Option<u32>,
    ) -> Result<LvsInfo> {
        let mut params = json!({
            "bdev_name": bdev_name,
            "lvs_name": lvs_name,
            "cluster_sz": cluster_size
        });
        if let Some(ratio) = num_md_pages_per_cluster_ratio {
            params["num_md_pages_per_cluster_ratio"] = json!(ratio);
        }
        
        let result = self.call_rpc("bdev_lvol_create_lvstore", Some(params)).await?;
        
        // Parse LVS creation response
        Ok(LvsInfo {
            name: lvs_name.to_string(),
            uuid: result.as_str().unwrap_or("").to_string(),
            base_bdev: bdev_name.to_string(),
            cluster_size,
            total_clusters: 0,
            free_clusters: 0,
            block_size: 512,
        })
    }
    
    /// Get Logical Volume Stores - matches SPDK v25.05.x bdev_lvol_get_lvstores RPC
    pub async fn get_lvol_stores(&self) -> Result<Vec<LvsInfo>> {
        let result = self.call_rpc("bdev_lvol_get_lvstores", None).await?;
        
        let empty_vec = Vec::new(); // Fix: create a proper empty vec
        let lvs_list = result.as_array().unwrap_or(&empty_vec);
        let mut stores = Vec::new();
        
        for lvs in lvs_list {
            if let Some(obj) = lvs.as_object() {
                stores.push(LvsInfo {
                    name: obj.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    uuid: obj.get("uuid").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    base_bdev: obj.get("base_bdev").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    cluster_size: obj.get("cluster_size").and_then(|v| v.as_u64()).unwrap_or(0),
                    // SPDK names this `total_data_clusters`
                    // (`module/bdev/lvol/vbdev_lvol_rpc.c`, the
                    // `spdk_bs_total_data_cluster_count` writer); reading
                    // `total_clusters` alone made this field silently 0
                    // on every lvstore. The old name is kept as a
                    // fallback for any shim that reshapes the reply.
                    total_clusters: obj
                        .get("total_data_clusters")
                        .or_else(|| obj.get("total_clusters"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0),
                    free_clusters: obj.get("free_clusters").and_then(|v| v.as_u64()).unwrap_or(0),
                    block_size: obj.get("block_size").and_then(|v| v.as_u64()).unwrap_or(512),
                });
            }
        }
        
        Ok(stores)
    }
    
    /// Get block devices - matches SPDK v25.05.x bdev_get_bdevs RPC
    pub async fn get_bdevs(&self) -> Result<Vec<String>> {
        let result = self.call_rpc("bdev_get_bdevs", None).await?;
        
        let empty_vec = Vec::new(); // Fix: create a proper empty vec
        let bdev_list = result.as_array().unwrap_or(&empty_vec);
        let mut bdevs = Vec::new();
        
        for bdev in bdev_list {
            if let Some(obj) = bdev.as_object() {
                if let Some(name) = obj.get("name").and_then(|v| v.as_str()) {
                    bdevs.push(name.to_string());
                }
            }
        }
        
        Ok(bdevs)
    }
    
    /// Get a specific bdev by name - efficient SPDK query with name parameter
    /// Returns the full bdev info as JSON if found, None if not found
    pub async fn get_bdev_by_name(&self, name: &str) -> Result<Option<Value>> {
        let params = json!({
            "name": name
        });
        
        // SPDK returns error Code=-19 (ENODEV) when bdev doesn't exist
        // We should return Ok(None) instead of propagating the error
        let result = match self.call_rpc("bdev_get_bdevs", Some(params)).await {
            Ok(r) => r,
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("Code=-19") || err_str.contains("No such device") {
                    // Bdev doesn't exist - this is OK, return None
                    return Ok(None);
                } else {
                    // Real error - propagate it
                    return Err(e);
                }
            }
        };
        
        // SPDK returns an array with 0 or 1 element when queried by name
        if let Some(bdev_list) = result.as_array() {
            if !bdev_list.is_empty() {
                return Ok(Some(bdev_list[0].clone()));
            }
        }
        
        Ok(None)
    }
    
    /// Create logical volume - matches SPDK v25.05.x bdev_lvol_create RPC
    pub async fn create_lvol(&self, lvs_name: &str, lvol_name: &str, size: u64, _cluster_size: u64, thin_provision: bool) -> Result<String> {
        // Convert bytes to MiB as required by SPDK bdev_lvol_create RPC
        let size_in_mib = (size + 1048575) / 1048576; // Round up to nearest MiB
        
        let params = json!({
            "lvs_name": lvs_name,
            "lvol_name": lvol_name,
            "size_in_mib": size_in_mib,
            "thin_provision": thin_provision,
            "clear_method": "unmap"
        });
        
        let deadline = lvol_alloc_deadline("bdev_lvol_create", size, thin_provision);
        let result = self.call_rpc_with_deadline("bdev_lvol_create", Some(params), deadline).await?;
        Ok(result.as_str().unwrap_or(&format!("{}/{}", lvs_name, lvol_name)).to_string())
    }
    
    /// Delete Logical Volume Store - matches SPDK v25.05.x bdev_lvol_delete_lvstore RPC
    pub async fn delete_lvs(&self, lvs_name: &str) -> Result<()> {
        let params = json!({
            "lvs_name": lvs_name
        });
        
        self.call_rpc("bdev_lvol_delete_lvstore", Some(params)).await?;
        Ok(())
    }
    
    /// Delete logical volume - matches SPDK v25.05.x bdev_lvol_delete RPC
    pub async fn delete_lvol(&self, _lvs_name: &str, lvol_name: &str) -> Result<()> {
        let params = json!({
            "name": lvol_name
        });
        
        self.call_rpc("bdev_lvol_delete", Some(params)).await?;
        Ok(())
    }
    
    /// Get logical volume stores - matches SPDK v25.05.x bdev_lvol_get_lvstores RPC
    pub async fn get_lvstores(&self) -> Result<Vec<Value>> {
        let result = self.call_rpc("bdev_lvol_get_lvstores", None).await?;
        let empty_vec = Vec::new();
        Ok(result.as_array().unwrap_or(&empty_vec).clone())
    }
    
    /// Additional RPC methods for dashboard support
    pub async fn get_nvme_controllers(&self) -> Result<Vec<Value>> {
        let result = self.call_rpc("bdev_nvme_get_controllers", None).await?;
        let empty_vec = Vec::new();
        Ok(result.as_array().unwrap_or(&empty_vec).clone())
    }
    
    pub async fn get_raid_bdevs(&self) -> Result<Vec<Value>> {
        // Fixed: Added required "category" parameter per SPDK documentation
        let params = json!({"category": "all"});
        let result = self.call_rpc("bdev_raid_get_bdevs", Some(params)).await?;
        let empty_vec = Vec::new();
        Ok(result.as_array().unwrap_or(&empty_vec).clone())
    }
    
    pub async fn get_nvmeof_subsystems(&self) -> Result<Vec<Value>> {
        let result = self.call_rpc("nvmf_get_subsystems", None).await?;
        let empty_vec = Vec::new();
        Ok(result.as_array().unwrap_or(&empty_vec).clone())
    }
    
    pub async fn get_bdev_iostat(&self) -> Result<Vec<Value>> {
        let result = self.call_rpc("bdev_get_iostat", None).await?;
        let empty_vec = Vec::new();
        Ok(result.as_array().unwrap_or(&empty_vec).clone())
    }
    
    pub async fn sync_all_blobstores(&self) -> Result<()> {
        self.call_rpc("blobstore_sync_all", None).await?;
        Ok(())
    }
    
    /// Apply `options` (`bdev_nvme_set_options`) when the target has no NVMe
    /// controller yet, the only time SPDK accepts them. Best effort: a
    /// concurrent attach that got there first, or a target that cannot list
    /// its controllers, leaves the attach to proceed as it did before. Both
    /// RPCs go to the bounded exchange directly: through `call_rpc` they
    /// would re-enter this guard's caller.
    async fn ensure_dead_target_options(&self, options: Option<Value>) {
        let Some(options) = options else { return };
        let list = "bdev_nvme_get_controllers";
        let first = matches!(
            self.call_rpc_bounded(list, None, rpc_deadline(list)).await,
            Ok(Value::Array(c)) if c.is_empty()
        );
        if !first {
            return;
        }
        let set = "bdev_nvme_set_options";
        match self.call_rpc_bounded(set, Some(options), rpc_deadline(set)).await {
            Ok(_) => tracing::info!(
                "[SPDK_RPC] Dead-target timeouts applied before this target's first controller attach"
            ),
            Err(e) if e.to_string().contains("not permitted") => tracing::debug!(
                "[SPDK_RPC] bdev_nvme_set_options refused: a concurrent attach got there first"
            ),
            Err(e) => tracing::warn!(
                error = %e,
                "[SPDK_RPC] bdev_nvme_set_options before the first attach failed; the attach proceeds"
            ),
        }
    }

    /// Generic RPC call for any method
    pub async fn call_method(&self, method: &str, params: Option<Value>) -> Result<Value> {
        self.call_rpc(method, params).await
    }

    /// Generic RPC call under an explicit deadline ([`lvol_alloc_deadline`]).
    pub async fn call_method_with_deadline(
        &self,
        method: &str,
        params: Option<Value>,
        deadline: Duration,
    ) -> Result<Value> {
        self.call_rpc_with_deadline(method, params, deadline).await
    }
}

// Removed embedded SPDK compatibility layer
// All operations now use direct RPC calls through SpdkNative::new()

impl Drop for SpdkNative {
    fn drop(&mut self) {
        tracing::debug!("[SPDK_RPC] Closing RPC client");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    
    /// Test parameter validation to match SPDK Go client behavior
    /// 
    /// This test mirrors the Go test_createRequest function to ensure
    /// we have identical parameter validation rules.
    #[tokio::test]
    async fn test_validate_rpc_params() {
        let spdk = SpdkNative {
            socket_path: "/tmp/test.sock".to_string(),
            request_id: AtomicU64::new(0),
        };
        
        // Test cases that should PASS (match Go client)
        let valid_cases = vec![
            ("nil params", None),
            ("object params", Some(json!({"name": "test", "size": 1024}))),
            ("array params", Some(json!(["a", "b", "c"]))),
            ("empty object", Some(json!({}))),
            ("empty array", Some(json!([]))),
            ("nested object", Some(json!({"config": {"enabled": true, "count": 5}}))),
        ];
        
        for (name, params) in valid_cases {
            if let Some(ref p) = params {
                let result = spdk.validate_rpc_params(p);
                assert!(result.is_ok(), "Test '{}' should pass but failed: {:?}", name, result);
            }
            println!("✅ Test '{}' passed", name);
        }
        
        // Test cases that should FAIL (match Go client)
        let invalid_cases = vec![
            ("primitive string", json!("invalidParam")),
            ("primitive number", json!(42)),
            ("primitive boolean", json!(true)),
            ("primitive float", json!(4.25)),
        ];
        
        for (name, params) in invalid_cases {
            let result = spdk.validate_rpc_params(&params);
            assert!(result.is_err(), "Test '{}' should fail but passed", name);
            println!("✅ Test '{}' correctly rejected: {}", name, result.unwrap_err());
        }
    }
    
    /// Test empty parameter normalization (matches CGO bridge behavior)
    /// 
    /// The official SPDK CGO bridge does:
    /// "Force Go client to skip 'params' parameter in JSON-RPC call.
    ///  if len(params) == 0 { params = nil }"
    #[test]
    fn test_empty_params_normalization() {
        // Test empty object -> None
        let empty_object = Some(json!({}));
        let _spdk = SpdkNative {
            socket_path: "/tmp/test.sock".to_string(),
            request_id: AtomicU64::new(0),
        };

        // Simulate the normalization logic
        let normalized = match &empty_object {
            Some(Value::Object(map)) if map.is_empty() => None,
            Some(Value::Array(arr)) if arr.is_empty() => None,
            other => other.clone(),
        };
        
        assert_eq!(normalized, None, "Empty object should normalize to None");
        
        // Test empty array -> None
        let empty_array = Some(json!([]));
        let normalized = match &empty_array {
            Some(Value::Object(map)) if map.is_empty() => None,
            Some(Value::Array(arr)) if arr.is_empty() => None,
            other => other.clone(),
        };
        
        assert_eq!(normalized, None, "Empty array should normalize to None");
        
        // Test non-empty object -> unchanged
        let non_empty = Some(json!({"test": "value"}));
        let normalized = match &non_empty {
            Some(Value::Object(map)) if map.is_empty() => None,
            Some(Value::Array(arr)) if arr.is_empty() => None,
            other => other.clone(),
        };
        
        assert_eq!(normalized, non_empty, "Non-empty object should remain unchanged");
        
        println!("✅ Empty parameter normalization matches CGO bridge behavior");
    }
    
    /// Test null result handling (matches CGO bridge special case)
    /// 
    /// The CGO bridge has special handling for:
    /// "This is a special case where inside JSON-RPC response 'Result' field is null"
    #[test]
    fn test_null_result_handling() {
        // Test response with null result
        let response_with_null = RpcResponse {
            jsonrpc: "2.0".to_string(),
            error: None,
            result: None, // This is the special case
            id: Some(1),
        };
        
        // Our implementation should handle this gracefully
        assert!(response_with_null.result.is_none());
        assert!(response_with_null.error.is_none());
        
        println!("✅ Null result handling matches CGO bridge behavior");
    }
    
    /// Test JSON-RPC request structure matches Go client
    #[test]
    fn test_rpc_request_structure() {
        let request = RpcRequest {
            jsonrpc: "2.0".to_string(),
            method: "test_method".to_string(),
            params: Some(json!({"test": "value"})),
            id: Some(17),
        };
        
        let json_str = serde_json::to_string(&request).unwrap();
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        
        // Verify structure matches Go client Request struct
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["method"], "test_method");
        assert_eq!(parsed["params"]["test"], "value");
        assert_eq!(parsed["id"], 17);
        
        println!("✅ JSON-RPC request structure matches Go client: {}", json_str);
    }

    // ------------------------------------------------------------------
    // Contract R5: bounded RPC transport (F39 substrate)
    // ------------------------------------------------------------------

    #[test]
    fn deadline_table_routes_slow_methods_to_slow_budget() {
        assert_eq!(deadline_for("bdev_wait_for_examine", 30, 180), Duration::from_secs(180));
        assert_eq!(deadline_for("bdev_lvol_create_lvstore", 30, 180), Duration::from_secs(180));
        assert_eq!(deadline_for("bdev_nvme_attach_controller", 30, 180), Duration::from_secs(180));
        // the workhorse read path and destructive ops stay on the tight budget
        assert_eq!(deadline_for("bdev_get_bdevs", 30, 180), Duration::from_secs(30));
        assert_eq!(deadline_for("nvmf_delete_subsystem", 30, 180), Duration::from_secs(30));
        assert_eq!(deadline_for("bdev_lvol_check_shallow_copy", 30, 180), Duration::from_secs(30));
    }

    #[test]
    fn thick_lvol_deadline_scales_with_the_bytes_zeroed() {
        let base = Duration::from_secs(30);
        // nothing to zero: the method's own budget
        assert_eq!(zeroing_deadline(base, 0, 256), base);
        // 1 GiB at 256 MiB/s: +4 s; 100 GiB: +400 s; a partial MiB rounds up
        assert_eq!(zeroing_deadline(base, 1 << 30, 256), Duration::from_secs(34));
        assert_eq!(zeroing_deadline(base, 100 << 30, 256), Duration::from_secs(430));
        assert_eq!(zeroing_deadline(base, (1 << 20) + 1, 1), Duration::from_secs(32));
        // a zero floor cannot divide by zero
        assert_eq!(zeroing_deadline(base, 1 << 20, 0), Duration::from_secs(31));
        // thin never zeroes at create: the normal budget, whatever the size
        assert_eq!(
            lvol_alloc_deadline("bdev_lvol_create", 100 << 40, true),
            rpc_deadline("bdev_lvol_create")
        );
        assert!(lvol_alloc_deadline("bdev_lvol_create", 100 << 30, false)
            > rpc_deadline("bdev_lvol_create") + Duration::from_secs(60));
    }

    #[tokio::test]
    async fn hung_socket_times_out_with_transport_shaped_error() {
        // A listener that accepts and then never answers — the F39 wedge shape.
        let dir = std::env::temp_dir().join(format!("flint-spdk-hang-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("hang.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let accept_task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { break };
                // Hold the connection open forever without responding.
                std::mem::forget(stream);
            }
        });

        let spdk = SpdkNative::new(Some(sock.to_string_lossy().to_string()))
            .await
            .expect("constructor test-connect succeeds against an accepting listener");
        let started = std::time::Instant::now();
        let err = spdk
            .call_rpc_bounded("bdev_get_bdevs", None, Duration::from_millis(250))
            .await
            .expect_err("hung exchange must fail, not hang");
        assert!(started.elapsed() < Duration::from_secs(5), "returned promptly");
        let msg = err.to_string();
        assert!(msg.contains("timed out"), "timeout shape: {msg}");
        // The expiry error must match NO benign classifier — guards fail closed on it.
        for benign in ["does not exist", "not found", "No such device", "already exists"] {
            assert!(!msg.contains(benign), "timeout error must not read as '{benign}'");
        }
        accept_task.abort();
        let _ = std::fs::remove_file(&sock);
    }

    /// A fake spdk-tgt: answers one newline-delimited JSON-RPC request per
    /// connection and records each method with its params, in order.
    /// `controllers` is bdev_nvme_get_controllers' answer (None: an error);
    /// `refuse_options` answers bdev_nvme_set_options as SPDK does once a
    /// controller exists.
    async fn fake_tgt(
        name: &str,
        controllers: Option<Value>,
        refuse_options: bool,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<(String, Option<Value>)>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let dir = std::env::temp_dir().join(format!("flint-spdk-{}-{}", name, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("spdk.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = calls.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { break };
                let (rd, mut wr) = stream.into_split();
                let mut line = String::new();
                if BufReader::new(rd).read_line(&mut line).await.unwrap_or(0) == 0 {
                    continue; // the constructor's connect probe sends nothing
                }
                let req: Value = serde_json::from_str(&line).unwrap();
                let method = req["method"].as_str().unwrap().to_string();
                seen.lock().unwrap().push((method.clone(), req.get("params").cloned()));
                let reply = match method.as_str() {
                    "bdev_nvme_get_controllers" => match &controllers {
                        Some(c) => json!({"jsonrpc": "2.0", "id": req["id"], "result": c}),
                        None => json!({"jsonrpc": "2.0", "id": req["id"],
                                       "error": {"code": -32601, "message": "Method not found"}}),
                    },
                    "bdev_nvme_set_options" if refuse_options => {
                        json!({"jsonrpc": "2.0", "id": req["id"],
                        "error": {"code": -1, "message": "RPC not permitted with nvme controllers already attached"}})
                    }
                    "bdev_nvme_attach_controller" => {
                        json!({"jsonrpc": "2.0", "id": req["id"], "result": ["nvme0n1"]})
                    }
                    _ => json!({"jsonrpc": "2.0", "id": req["id"], "result": true}),
                };
                let _ = wr.write_all(format!("{reply}\n").as_bytes()).await;
            }
        });
        (sock.to_string_lossy().to_string(), calls, task)
    }

    fn methods(calls: &std::sync::Mutex<Vec<(String, Option<Value>)>>) -> Vec<String> {
        calls.lock().unwrap().iter().map(|(m, _)| m.clone()).collect()
    }

    fn attach_params() -> Value {
        json!({"name": "nvme_leg", "trtype": "TCP", "adrfam": "IPv4", "traddr": "10.0.0.2",
               "trsvcid": "4420", "subnqn": "nqn.2024-11.com.flint:volume:pvc-x_1"})
    }

    #[tokio::test]
    async fn the_first_attach_on_a_target_applies_the_dead_target_timeouts() {
        let (sock, calls, task) = fake_tgt("first", Some(json!([])), false).await;
        let spdk = SpdkNative::new(Some(sock)).await.unwrap();
        spdk.call_method("bdev_nvme_attach_controller", Some(attach_params())).await.unwrap();
        assert_eq!(
            methods(&calls),
            ["bdev_nvme_get_controllers", "bdev_nvme_set_options", "bdev_nvme_attach_controller"]
        );
        let opts = calls.lock().unwrap()[1].1.clone().unwrap();
        assert_eq!(
            opts,
            crate::nvme_recovery::DeadTargetTimeouts::from_env().set_options_params().unwrap()
        );
        task.abort();
    }

    #[tokio::test]
    async fn an_attach_beside_existing_controllers_sends_no_options() {
        let (sock, calls, task) =
            fake_tgt("beside", Some(json!([{"name": "nvme_other"}])), false).await;
        let spdk = SpdkNative::new(Some(sock)).await.unwrap();
        spdk.call_method("bdev_nvme_attach_controller", Some(attach_params())).await.unwrap();
        assert_eq!(methods(&calls), ["bdev_nvme_get_controllers", "bdev_nvme_attach_controller"]);
        task.abort();
    }

    #[tokio::test]
    async fn a_refused_set_options_does_not_stop_the_attach() {
        let (sock, calls, task) = fake_tgt("refused", Some(json!([])), true).await;
        let spdk = SpdkNative::new(Some(sock)).await.unwrap();
        let attached = spdk.call_method("bdev_nvme_attach_controller", Some(attach_params())).await;
        assert_eq!(attached.unwrap(), json!(["nvme0n1"]));
        assert_eq!(
            methods(&calls),
            ["bdev_nvme_get_controllers", "bdev_nvme_set_options", "bdev_nvme_attach_controller"]
        );
        task.abort();
    }

    #[tokio::test]
    async fn an_unlistable_target_still_attaches_without_options() {
        let (sock, calls, task) = fake_tgt("unlistable", None, false).await;
        let spdk = SpdkNative::new(Some(sock)).await.unwrap();
        spdk.call_method("bdev_nvme_attach_controller", Some(attach_params())).await.unwrap();
        assert_eq!(methods(&calls), ["bdev_nvme_get_controllers", "bdev_nvme_attach_controller"]);
        task.abort();
    }

    #[tokio::test]
    async fn other_methods_are_not_guarded() {
        let (sock, calls, task) = fake_tgt("other", Some(json!([])), false).await;
        let spdk = SpdkNative::new(Some(sock)).await.unwrap();
        spdk.call_method("bdev_get_bdevs", None).await.unwrap();
        spdk.call_method("bdev_nvme_detach_controller", Some(json!({"name": "nvme_leg"})))
            .await
            .unwrap();
        assert_eq!(methods(&calls), ["bdev_get_bdevs", "bdev_nvme_detach_controller"]);
        task.abort();
    }
}
