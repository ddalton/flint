# Reserved Devices Integration Guide

## Integration into minimal_disk_service.rs

Add the following changes to integrate reserved devices filtering:

### 1. Add module declaration

```rust
// At the top of minimal_disk_service.rs or lib.rs
mod reserved_devices;
use reserved_devices::ReservedDevices;
```

### 2. Add field to MinimalDiskService

```rust
pub struct MinimalDiskService {
    spdk_socket: String,
    node_name: String,
    discovery_mode: DiscoveryMode,
    reserved_devices: Option<ReservedDevices>,  // ← ADD THIS
}
```

### 3. Initialize in constructor

```rust
impl MinimalDiskService {
    pub async fn new(
        spdk_socket: String,
        node_name: String,
        discovery_mode: DiscoveryMode,
    ) -> Result<Self, MinimalStateError> {
        // Load reserved devices configuration
        let namespace = std::env::var("POD_NAMESPACE")
            .unwrap_or_else(|_| "flint-system".to_string());

        let reserved_devices = match ReservedDevices::load(&namespace).await {
            Ok(rd) => {
                println!("✅ [MINIMAL_DISK] Loaded reserved devices configuration");
                Some(rd)
            }
            Err(e) => {
                println!("⚠️ [MINIMAL_DISK] Failed to load reserved devices: {} (continuing without filtering)", e);
                None
            }
        };

        Ok(Self {
            spdk_socket,
            node_name,
            discovery_mode,
            reserved_devices,  // ← ADD THIS
        })
    }
}
```

### 4. Add filtering during device discovery

```rust
async fn discover_physical_nvme_devices(&self) -> Result<Vec<PhysicalDevice>, MinimalStateError> {
    // ... existing discovery code ...

    let mut discovered_devices = Vec::new();

    for device in all_devices {
        // ← ADD THIS CHECK
        if let Some(ref reserved) = self.reserved_devices {
            if reserved.is_reserved(&device.pci_address) {
                println!("⏭️ [DEVICE_FILTER] Skipping {} ({}) - RESERVED for device plugin",
                         device.device_name, device.pci_address);
                continue;
            }
        }

        // Existing device processing logic
        discovered_devices.push(device);
    }

    Ok(discovered_devices)
}
```

### 5. Add API endpoint to update reserved devices

```rust
// In node_agent.rs HTTP API handlers

async fn handle_get_reserved_devices(
    node_agent: Arc<MinimalNodeAgent>,
) -> Result<impl warp::Reply, warp::Rejection> {
    if let Some(ref reserved) = node_agent.disk_service.reserved_devices {
        let devices: Vec<String> = reserved.get_reserved_devices()
            .iter()
            .cloned()
            .collect();

        Ok(warp::reply::json(&json!({
            "reserved_devices": devices,
            "count": devices.len()
        })))
    } else {
        Ok(warp::reply::json(&json!({
            "reserved_devices": [],
            "count": 0
        })))
    }
}

async fn handle_reload_reserved_devices(
    node_agent: Arc<MinimalNodeAgent>,
) -> Result<impl warp::Reply, warp::Rejection> {
    // Reload configuration from ConfigMap
    // This allows dynamic updates without pod restart

    if let Some(ref mut reserved) = node_agent.disk_service.reserved_devices {
        match reserved.reload().await {
            Ok(_) => {
                println!("✅ [HTTP_API] Reloaded reserved devices configuration");
                Ok(warp::reply::json(&json!({
                    "success": true,
                    "message": "Reserved devices configuration reloaded"
                })))
            }
            Err(e) => {
                println!("❌ [HTTP_API] Failed to reload reserved devices: {}", e);
                Ok(warp::reply::json(&json!({
                    "success": false,
                    "error": format!("{}", e)
                })))
            }
        }
    } else {
        Ok(warp::reply::json(&json!({
            "success": false,
            "error": "Reserved devices not configured"
        })))
    }
}

// Add routes in start():
let get_reserved = warp::path!("api" / "reserved-devices")
    .and(warp::get())
    .and(with_node_agent(node_agent.clone()))
    .and_then(Self::handle_get_reserved_devices);

let reload_reserved = warp::path!("api" / "reserved-devices" / "reload")
    .and(warp::post())
    .and(with_node_agent(node_agent.clone()))
    .and_then(Self::handle_reload_reserved_devices);

// Add to routes:
.or(get_reserved)
.or(reload_reserved)
```

## Usage Examples

### 1. Reserve a device via Helm

```yaml
# values.yaml
reservedDevices:
  list: |
    0000:02:00.0
  annotations: |
    0000:02:00.0: High-performance database workload
```

### 2. Reserve a device via kubectl

```bash
kubectl edit configmap flint-reserved-devices -n flint-system

# Add to reserved-devices:
data:
  reserved-devices: |
    0000:02:00.0
    0000:03:00.0
```

### 3. Check reserved devices via API

```bash
kubectl exec -n flint-system flint-csi-node-xxx -c flint-csi-driver -- \
  curl -s http://localhost:8081/api/reserved-devices | jq
```

### 4. Reload configuration after updating ConfigMap

```bash
kubectl exec -n flint-system flint-csi-node-xxx -c flint-csi-driver -- \
  curl -s -X POST http://localhost:8081/api/reserved-devices/reload | jq
```

## Dashboard Integration

The dashboard should provide:

1. **Device List View**
   - Show all discovered devices
   - Indicate which are reserved
   - Button to reserve/unreserve

2. **Reserve Device Flow**
   - User clicks "Reserve for High Performance"
   - UI updates ConfigMap
   - UI calls reload endpoint on all nodes
   - Device disappears from available CSI devices

3. **Visual Indicators**
   - Reserved devices: Red/Orange with lock icon
   - CSI-managed devices: Green with check icon
   - Show reservation reason/annotation
