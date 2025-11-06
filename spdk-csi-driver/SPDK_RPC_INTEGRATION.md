# SPDK RPC Integration Guide

## Overview

The Flint dashboard backend service communicates directly with SPDK targets using **native Rust code over Unix sockets**. This eliminates Python script dependencies and provides high-performance, type-safe SPDK integration.

## Architecture

```
┌─────────────────┐    HTTP/JSON     ┌─────────────────┐    Unix Socket     ┌─────────────────┐
│   Dashboard     │ ──────────────► │   Rust Backend  │ ─────────────────► │   SPDK Target   │
│   Frontend      │ ◄────────────── │   (Warp/Tokio)  │ ◄───────────────── │   (C++ RPC)     │
└─────────────────┘                 └─────────────────┘                    └─────────────────┘
     React/TS                        enhanced_migration_api.rs               JSON-RPC 2.0
```

## Native Rust SPDK RPC Client

### Connection Method
```rust
// Direct Unix socket communication - no Python dependencies
let mut stream = UnixStream::connect("/var/tmp/spdk.sock").await?;

// JSON-RPC 2.0 format
let request = json!({
    "jsonrpc": "2.0",
    "method": "bdev_raid_replace_member",
    "params": {
        "name": "raid1_node1",
        "old_member": "nvme0n1",
        "new_member": "nvme2n1"
    },
    "id": 1
});

// Send with newline delimiter (SPDK requirement)
stream.write_all(format!("{}\n", request).as_bytes()).await?;
```

### Response Handling
```rust
// Read response
let mut reader = BufReader::new(stream);
let mut response_line = String::new();
reader.read_line(&mut response_line).await?;

// Parse JSON-RPC 2.0 response
let response: Value = serde_json::from_str(&response_line.trim())?;

// Check for errors
if let Some(error) = response.get("error") {
    return Err(format!("SPDK RPC error: {}", error).into());
}

// Extract result
let result = response.get("result").unwrap();
```

## Migration API Implementation

### 1. Enhanced Migration API (`enhanced_migration_api.rs`)

**Core Components:**
- `SpdkRpcClient` - Native Rust Unix socket client
- `EnhancedMigrationApi` - High-level migration operations
- `EnhancedMigrationOperation` - Migration state tracking

**Key SPDK RPC Methods Used:**
```rust
// RAID Operations
"bdev_raid_get_bdevs"      // Get RAID information
"bdev_raid_create"         // Create RAID device  
"bdev_raid_replace_member" // Replace failed member
"bdev_raid_add_member"     // Add new member
"bdev_raid_remove_member"  // Remove old member

// NVMe-oF Operations  
"bdev_nvme_attach_controller"  // Connect to NVMe-oF target
"bdev_nvme_detach_controller" // Disconnect NVMe-oF target
"nvmf_get_subsystems"         // List NVMe-oF subsystems

// Block Device Operations
"bdev_get_bdevs"          // List all block devices
"bdev_lvol_get_lvstores"  // Get logical volume stores
```

### 2. API Endpoints (`migration_api_endpoints.rs`)

**HTTP Endpoints for Dashboard:**
```
GET  /api/migration/targets           # Get available migration targets
POST /api/migration/start             # Start migration operation
GET  /api/migration/operations        # List active operations
GET  /api/migration/monitor           # Real-time monitoring
POST /api/migration/operations/{id}/retry   # Retry failed operation
POST /api/migration/operations/{id}/cancel  # Cancel operation

POST /api/alerts/{volume_id}/enhanced-migrate  # Alert-triggered migration
```

## Migration Operation Types

### 1. RAID Member Migration
**Purpose:** Replace failed or degraded RAID member
**SPDK RPC Flow:**
```rust
// 1. Attach new target (if NVMe-oF)
bdev_nvme_attach_controller {
    "name": "nvme_migration_001",
    "trtype": "TCP",
    "traddr": "192.168.1.100", 
    "trsvcid": "4420",
    "subnqn": "nqn.2023.io.spdk:storage.target1"
}

// 2. Replace RAID member
bdev_raid_replace_member {
    "name": "raid1_node1",
    "old_member": "nvme0n1",
    "new_member": "nvme_migration_001"
}

// 3. Monitor rebuild progress
bdev_raid_get_bdevs {
    "name": "raid1_node1"
}
// Check rebuild_info.progress_percentage

// 4. Cleanup - remove old member (automatic)
```

### 2. RAID Member Addition
**Purpose:** Add new member to increase capacity/redundancy
**SPDK RPC Flow:**
```rust
// 1. Prepare new member
bdev_nvme_attach_controller { ... }

// 2. Add to RAID
bdev_raid_add_member {
    "name": "raid1_node1", 
    "member": "nvme_addition_001"
}

// 3. Monitor synchronization
bdev_raid_get_bdevs { "name": "raid1_node1" }
```

### 3. Node Migration
**Purpose:** Move entire RAID volume to different node
**SPDK RPC Flow:**
```rust
// 1. Create RAID on target node
bdev_raid_create {
    "name": "raid1_node2",
    "raid_level": 1,
    "base_bdevs": ["nvme0n1", "nvme1n1"]
}

// 2. Copy data (implementation specific)
// 3. Update references
// 4. Remove from source node
```

## Progress Monitoring

### Real-time Progress Tracking
```rust
// Monitor rebuild progress
async fn monitor_rebuild_progress(&self, raid_name: &str) -> Result<f64> {
    loop {
        let result = client.call_rpc("bdev_raid_get_bdevs", 
            Some(json!({"name": raid_name}))).await?;
            
        if let Some(rebuild_info) = result.get("rebuild_info") {
            if let Some(progress) = rebuild_info.get("progress_percentage") {
                return Ok(progress.as_f64().unwrap_or(0.0));
            }
        } else {
            // No rebuild info = rebuild complete
            return Ok(100.0);
        }
        
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
```

### Cleanup Verification
```rust
// Verify RAID integrity after migration
async fn verify_raid_integrity(&self, raid_name: &str) -> Result<bool> {
    let raid_info = self.call_rpc("bdev_raid_get_bdevs", 
        Some(json!({"name": raid_name}))).await?;
        
    if let Some(state) = raid_info.get("state") {
        return Ok(state.as_str() == Some("online"));
    }
    
    Ok(false)
}
```

## Dashboard Integration

### Frontend API Calls
```typescript
// 1. Get available targets
const targets = await fetch('/api/migration/targets?raid_name=raid1_node1');

// 2. Start migration
const operation = await fetch('/api/alerts/vol-123/enhanced-migrate', {
    method: 'POST',
    body: JSON.stringify({
        operation_type: 'member_migration',
        target_type: 'local_disk',
        target_disk_id: 'nvme2n1',
        confirmation: true
    })
});

// 3. Monitor progress
const status = await fetch('/api/migration/monitor');
```

### Backend SPDK Integration
```rust
// Dashboard backend automatically handles:
impl MigrationApiState {
    async fn start_migration(&self, request: EnhancedMigrationRequest) -> Result<Operation> {
        // 1. Validate request
        // 2. Create operation
        // 3. Make SPDK RPC calls
        // 4. Track progress
        // 5. Handle cleanup
    }
}
```

## Error Handling

### SPDK RPC Error Response
```json
{
    "jsonrpc": "2.0",
    "error": {
        "code": -32602,
        "message": "Invalid params",
        "data": "RAID 'raid1_node1' not found"
    },
    "id": 1
}
```

### Rust Error Handling
```rust
// Parse and handle SPDK errors
if let Some(error) = response.get("error") {
    let error_code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    let error_message = error.get("message").and_then(|m| m.as_str()).unwrap_or("Unknown error");
    
    return Err(format!("SPDK RPC error {}: {}", error_code, error_message).into());
}
```

## Configuration

### SPDK Node Registration
```rust
// Dashboard backend registers SPDK nodes
let mut migration_api = EnhancedMigrationApi::new();

// Add each cluster node
migration_api.add_spdk_node("worker-node-1".to_string(), 
    "unix:///var/tmp/spdk_node1.sock".to_string());
migration_api.add_spdk_node("worker-node-2".to_string(), 
    "unix:///var/tmp/spdk_node2.sock".to_string());
```

### Socket Paths
```
/var/tmp/spdk.sock           # Default SPDK socket
/var/tmp/spdk_node1.sock     # Node-specific socket
/run/spdk/rpc.sock          # Alternative location
```

## Key Benefits

### ✅ **Native Performance**
- Direct Unix socket communication
- No Python process overhead
- Async/await with Tokio
- Zero-copy JSON parsing

### ✅ **Type Safety**  
- Rust compile-time guarantees
- Structured error handling
- serde JSON serialization
- No runtime type errors

### ✅ **No External Dependencies**
- Pure Rust implementation
- No Python scripts required
- Self-contained binaries
- Simplified deployment

### ✅ **Real-time Monitoring**
- Live progress updates
- WebSocket integration
- Background task tracking
- Comprehensive logging

## Migration Workflow Summary

1. **Frontend Request** → HTTP API call to dashboard backend
2. **Backend Validation** → Parse request, validate parameters
3. **SPDK Discovery** → Query available targets via Unix socket RPC
4. **Migration Start** → Create operation, begin SPDK calls
5. **Progress Monitoring** → Poll SPDK for rebuild status
6. **Cleanup Phase** → Remove old members, verify integrity
7. **Completion** → Update UI, send alerts

All SPDK communication happens through native Rust code using direct Unix socket connections, providing high performance and reliability for enterprise storage operations.


