# CSI Driver Scalability Analysis

## Problem: 1000 PVCs in Minutes

Creating 1000 PVCs in a few minutes requires handling:
- ~17 PVCs per second (for 1 minute)
- ~8 PVCs per second (for 2 minutes)
- Each PVC creation involves multiple operations

## Current Architecture Bottlenecks

### 1. Node Capacity Query for Every Volume

**Current approach** (`select_node_for_single_replica()`):
```rust
async fn select_node_for_single_replica(&self, size_bytes: u64) -> Result<String, MinimalStateError> {
    let all_nodes = self.get_all_nodes().await?;  // Kubernetes API call
    
    for node_name in all_nodes {
        // Query EACH node via HTTP
        let disks = self.get_initialized_disks_from_node(&node_name).await?;
        
        if disk has space {
            return Ok(node_name);
        }
    }
}
```

**Problem**:
- For 1000 volumes with 10 nodes: **10,000 HTTP queries** to node agents
- For 1000 volumes: **1000 Kubernetes API calls** to list nodes
- Sequential checking (not parallelized)
- No caching

**Estimated time**: 1000 volumes × (0.1s K8s API + 10 nodes × 0.05s HTTP) = **600 seconds = 10 minutes**

### 2. Race Conditions

**Scenario**:
```
Time T0: Volume 1 queries Node A → 100GB free
Time T1: Volume 2 queries Node A → 100GB free (still shows old value)
Time T2: Volume 1 allocates 80GB on Node A
Time T3: Volume 2 tries to allocate 80GB on Node A → FAILS (only 20GB left)
```

No capacity reservation during selection → volumes fail after selection.

### 3. No Load Distribution

**Current**: First-fit algorithm
- First few nodes get all volumes
- Last nodes remain empty
- Unbalanced cluster

### 4. SPDK RPC Serialization

Each volume creation involves:
1. Query node capacity (SPDK RPC)
2. Create lvol (SPDK RPC)
3. Setup NVMe-oF target (SPDK RPC - for multi-replica)

SPDK itself is fast, but HTTP overhead adds up.

## Optimization Strategy

### Phase 1: Capacity Caching (Immediate - Week 1)

**Implement in-memory capacity cache**:

```rust
use std::sync::Arc;
use tokio::sync::RwLock;
use std::time::{Duration, Instant};

/// Cached node capacity information
#[derive(Clone)]
struct NodeCapacity {
    node_name: String,
    total_capacity: u64,
    free_capacity: u64,
    disk_count: u32,
    last_updated: Instant,
}

/// Capacity cache with TTL
struct CapacityCache {
    cache: Arc<RwLock<HashMap<String, NodeCapacity>>>,
    ttl: Duration,
}

impl CapacityCache {
    fn new(ttl_seconds: u64) -> Self {
        Self {
            cache: Arc::new(RwLock::new(HashMap::new())),
            ttl: Duration::from_secs(ttl_seconds),
        }
    }

    /// Get cached capacity or refresh if stale
    async fn get_node_capacity(&self, node_name: &str, driver: &SpdkCsiDriver) -> Result<NodeCapacity, MinimalStateError> {
        // Check cache first
        {
            let cache = self.cache.read().await;
            if let Some(capacity) = cache.get(node_name) {
                if capacity.last_updated.elapsed() < self.ttl {
                    println!("✅ [CACHE] Hit for node: {} (age: {:?})", node_name, capacity.last_updated.elapsed());
                    return Ok(capacity.clone());
                }
            }
        }

        // Cache miss or stale - refresh
        println!("🔄 [CACHE] Miss for node: {}, refreshing...", node_name);
        let capacity = self.refresh_node_capacity(node_name, driver).await?;

        // Update cache
        {
            let mut cache = self.cache.write().await;
            cache.insert(node_name.to_string(), capacity.clone());
        }

        Ok(capacity)
    }

    /// Refresh capacity from node agent
    async fn refresh_node_capacity(&self, node_name: &str, driver: &SpdkCsiDriver) -> Result<NodeCapacity, MinimalStateError> {
        let disks = driver.get_initialized_disks_from_node(node_name).await?;
        
        let total_capacity: u64 = disks.iter().map(|d| d.size_bytes).sum();
        let free_capacity: u64 = disks.iter().map(|d| d.free_space).sum();
        let disk_count = disks.len() as u32;

        Ok(NodeCapacity {
            node_name: node_name.to_string(),
            total_capacity,
            free_capacity,
            disk_count,
            last_updated: Instant::now(),
        })
    }

    /// Reserve capacity (optimistic locking)
    async fn reserve_capacity(&self, node_name: &str, size_bytes: u64) -> Result<(), MinimalStateError> {
        let mut cache = self.cache.write().await;
        
        if let Some(capacity) = cache.get_mut(node_name) {
            if capacity.free_capacity >= size_bytes {
                capacity.free_capacity -= size_bytes;
                println!("✅ [CACHE] Reserved {}GB on node: {} (remaining: {}GB)",
                         size_bytes / (1024*1024*1024),
                         node_name,
                         capacity.free_capacity / (1024*1024*1024));
                return Ok(());
            } else {
                return Err(MinimalStateError::InsufficientCapacity {
                    required: size_bytes,
                    available: capacity.free_capacity,
                });
            }
        }

        Err(MinimalStateError::InternalError {
            message: format!("Node {} not in cache", node_name),
        })
    }

    /// Release capacity if volume creation fails
    async fn release_capacity(&self, node_name: &str, size_bytes: u64) {
        let mut cache = self.cache.write().await;
        
        if let Some(capacity) = cache.get_mut(node_name) {
            capacity.free_capacity += size_bytes;
            println!("⚠️ [CACHE] Released {}GB on node: {}", 
                     size_bytes / (1024*1024*1024), node_name);
        }
    }

    /// Invalidate cache entry (after successful volume creation)
    async fn invalidate(&self, node_name: &str) {
        let mut cache = self.cache.write().await;
        cache.remove(node_name);
        println!("🔄 [CACHE] Invalidated cache for node: {}", node_name);
    }

    /// Warm up cache on startup
    async fn warm_up(&self, driver: &SpdkCsiDriver) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔥 [CACHE] Warming up capacity cache...");
        
        let all_nodes = driver.get_all_nodes().await?;
        
        // Parallel cache warming
        let mut tasks = Vec::new();
        for node_name in all_nodes {
            let cache = self.clone();
            let driver = driver.clone();
            
            tasks.push(tokio::spawn(async move {
                cache.refresh_node_capacity(&node_name, &driver).await
            }));
        }

        // Wait for all to complete
        for task in tasks {
            match task.await {
                Ok(Ok(capacity)) => {
                    let mut cache = self.cache.write().await;
                    cache.insert(capacity.node_name.clone(), capacity);
                }
                Ok(Err(e)) => println!("⚠️ [CACHE] Failed to warm up node: {}", e),
                Err(e) => println!("⚠️ [CACHE] Task failed: {}", e),
            }
        }

        let cache = self.cache.read().await;
        println!("✅ [CACHE] Warmed up {} nodes", cache.len());
        
        Ok(())
    }
}
```

**Integration**:

```rust
pub struct SpdkCsiDriver {
    pub kube_client: Client,
    pub target_namespace: String,
    pub node_id: String,
    pub spdk_rpc_url: String,
    pub nvmeof_transport: String,
    pub nvmeof_target_port: u16,
    pub spdk_node_urls: Arc<Mutex<HashMap<String, String>>>,
    
    // NEW: Capacity cache
    pub capacity_cache: CapacityCache,
}

impl SpdkCsiDriver {
    pub fn new(/*...*/) -> Self {
        Self {
            // ... existing fields ...
            capacity_cache: CapacityCache::new(30), // 30 second TTL
        }
    }

    /// Optimized node selection with caching
    async fn select_node_for_single_replica(&self, size_bytes: u64) -> Result<String, MinimalStateError> {
        println!("🔍 [DRIVER] Selecting node with caching (size: {}GB)", size_bytes / (1024*1024*1024));

        let all_nodes = self.get_all_nodes().await?;

        // Check cached capacities (parallel)
        let mut tasks = Vec::new();
        for node_name in all_nodes {
            let cache = self.capacity_cache.clone();
            let driver_clone = self.clone();
            
            tasks.push(tokio::spawn(async move {
                cache.get_node_capacity(&node_name, &driver_clone).await
            }));
        }

        // Wait for all capacity checks
        let mut candidates = Vec::new();
        for task in tasks {
            if let Ok(Ok(capacity)) = task.await {
                if capacity.free_capacity >= size_bytes {
                    candidates.push(capacity);
                }
            }
        }

        if candidates.is_empty() {
            return Err(MinimalStateError::InsufficientCapacity {
                required: size_bytes,
                available: 0,
            });
        }

        // Sort by free capacity (descending) for load balancing
        candidates.sort_by(|a, b| b.free_capacity.cmp(&a.free_capacity));

        // Select node with most free space
        let selected = &candidates[0];
        
        // Reserve capacity
        self.capacity_cache.reserve_capacity(&selected.node_name, size_bytes).await?;

        println!("✅ [DRIVER] Selected node: {} (free: {}GB)", 
                 selected.node_name, selected.free_capacity / (1024*1024*1024));

        Ok(selected.node_name.clone())
    }
}
```

**Benefits**:
- ✅ 1000 volumes: **1 Kubernetes API call** (on startup) instead of 1000
- ✅ 1000 volumes: **~33 HTTP queries** (with 30s TTL) instead of 10,000
- ✅ **30x faster** node selection
- ✅ Load balancing (selects node with most free space)
- ✅ Optimistic capacity reservation reduces race conditions

**Estimated time with cache**: 1000 volumes × 0.1s = **100 seconds = 1.7 minutes**

### Phase 2: Parallel Volume Creation (Week 2)

**Controller concurrent request handling**:

```rust
// In main.rs - CSI Controller Service
#[tonic::async_trait]
impl Controller for MinimalControllerService {
    async fn create_volume(
        &self,
        request: tonic::Request<CreateVolumeRequest>,
    ) -> Result<tonic::Response<CreateVolumeResponse>, tonic::Status> {
        // Each request runs in its own tokio task
        // Tokio runtime handles concurrency automatically
        
        // Current code is already concurrent-safe if we use proper locking
        // The capacity_cache uses RwLock for thread-safe concurrent access
        
        // ... existing implementation ...
    }
}
```

**Tokio runtime tuning**:

```rust
// In main.rs
#[tokio::main(flavor = "multi_thread", worker_threads = 16)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ... existing code ...
}
```

**Benefits**:
- ✅ Handle 16 concurrent CreateVolume requests
- ✅ Better CPU utilization
- ✅ Lower latency for concurrent requests

### Phase 3: Background Cache Refresh (Week 2)

**Proactive cache updates**:

```rust
impl CapacityCache {
    /// Start background refresh task
    pub fn start_background_refresh(cache: Arc<Self>, driver: Arc<SpdkCsiDriver>, interval_secs: u64) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
            
            loop {
                interval.tick().await;
                
                println!("🔄 [CACHE] Background refresh starting...");
                
                // Get all cached nodes
                let nodes: Vec<String> = {
                    let cache_lock = cache.cache.read().await;
                    cache_lock.keys().cloned().collect()
                };

                // Refresh all nodes in parallel
                let mut tasks = Vec::new();
                for node_name in nodes {
                    let cache_clone = cache.clone();
                    let driver_clone = driver.clone();
                    
                    tasks.push(tokio::spawn(async move {
                        match cache_clone.refresh_node_capacity(&node_name, &driver_clone).await {
                            Ok(capacity) => {
                                let mut cache_lock = cache_clone.cache.write().await;
                                cache_lock.insert(node_name.clone(), capacity);
                                println!("✅ [CACHE] Refreshed node: {}", node_name);
                            }
                            Err(e) => {
                                println!("⚠️ [CACHE] Failed to refresh node {}: {}", node_name, e);
                            }
                        }
                    }));
                }

                // Wait for all refreshes
                for task in tasks {
                    let _ = task.await;
                }

                println!("✅ [CACHE] Background refresh complete");
            }
        });
    }
}

// In driver initialization
impl SpdkCsiDriver {
    pub async fn initialize(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🚀 [DRIVER] Initializing...");
        
        // Warm up cache
        self.capacity_cache.warm_up(self).await?;
        
        // Start background refresh (every 15 seconds)
        CapacityCache::start_background_refresh(
            Arc::new(self.capacity_cache.clone()),
            Arc::new(self.clone()),
            15,
        );

        println!("✅ [DRIVER] Initialization complete");
        Ok(())
    }
}
```

**Benefits**:
- ✅ Cache always fresh
- ✅ Zero cache misses after warmup
- ✅ Faster response times

### Phase 4: Batch Processing (Optional - Week 3)

For extreme scale (10,000+ volumes):

```rust
/// Batch volume creation for better throughput
async fn create_volumes_batch(
    &self,
    requests: Vec<VolumeRequest>,
) -> Vec<Result<VolumeCreationResult, MinimalStateError>> {
    println!("📦 [DRIVER] Creating {} volumes in batch", requests.len());

    // Group by size for better packing
    let mut by_size: HashMap<u64, Vec<VolumeRequest>> = HashMap::new();
    for req in requests {
        by_size.entry(req.size_bytes).or_insert_with(Vec::new).push(req);
    }

    // Process each size group
    let mut results = Vec::new();
    for (_size, group) in by_size {
        // Create all volumes of this size in parallel
        let tasks: Vec<_> = group.into_iter().map(|req| {
            let driver = self.clone();
            tokio::spawn(async move {
                driver.create_single_replica_volume(&req.volume_id, req.size_bytes, req.thin_provision).await
            })
        }).collect();

        // Collect results
        for task in tasks {
            results.push(task.await.unwrap());
        }
    }

    results
}
```

## Performance Comparison

### Without Optimization (Current)
```
1000 volumes, 10 nodes:
- K8s API calls: 1000 × 0.1s = 100s
- Node queries: 1000 × 10 × 0.05s = 500s
- Total: ~600 seconds = 10 minutes
- Throughput: 1.7 volumes/second
```

### With Phase 1 (Caching)
```
1000 volumes, 10 nodes:
- Initial cache warm-up: 10 nodes × 0.05s = 0.5s (one time)
- Cache hits: 970 volumes × 0.001s = 0.97s
- Cache refreshes: 30 × 10 nodes × 0.05s = 15s
- Volume creation: 1000 × 0.1s = 100s
- Total: ~116 seconds = 2 minutes
- Throughput: 8.6 volumes/second (5x improvement)
```

### With Phase 1+2 (Caching + Parallel)
```
1000 volumes, 10 nodes, 16 workers:
- Cache warm-up: 0.5s
- Volume creation (16 parallel): 1000 / 16 × 0.1s = 6.25s
- SPDK operations: 1000 / 16 × 0.1s = 6.25s
- Total: ~13 seconds
- Throughput: 77 volumes/second (46x improvement)
```

### With All Phases (Caching + Parallel + Background Refresh)
```
1000 volumes, 10 nodes, 16 workers:
- Cache always fresh (background refresh)
- Zero cache misses
- Full parallel processing
- Total: ~10 seconds
- Throughput: 100 volumes/second (60x improvement)
```

## Configuration

```yaml
# Environment variables for CSI controller
env:
  - name: CAPACITY_CACHE_TTL
    value: "30"  # seconds
  - name: CAPACITY_CACHE_BACKGROUND_REFRESH
    value: "15"  # seconds
  - name: MAX_CONCURRENT_VOLUME_CREATES
    value: "16"
  - name: TOKIO_WORKER_THREADS
    value: "16"
```

## Monitoring

Add Prometheus metrics:

```rust
use prometheus::{Counter, Histogram, IntGauge};

lazy_static! {
    static ref VOLUME_CREATE_DURATION: Histogram = register_histogram!(
        "flint_volume_create_duration_seconds",
        "Volume creation duration"
    ).unwrap();
    
    static ref CACHE_HITS: Counter = register_counter!(
        "flint_capacity_cache_hits_total",
        "Capacity cache hits"
    ).unwrap();
    
    static ref CACHE_MISSES: Counter = register_counter!(
        "flint_capacity_cache_misses_total",
        "Capacity cache misses"
    ).unwrap();
    
    static ref CONCURRENT_VOLUME_CREATES: IntGauge = register_int_gauge!(
        "flint_concurrent_volume_creates",
        "Number of concurrent volume creations"
    ).unwrap();
}
```

## Testing Plan

### Load Test 1: 100 PVCs
```bash
# Create 100 PVCs rapidly
for i in {1..100}; do
  cat <<EOF | kubectl apply -f - &
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: load-test-$i
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 10Gi
  storageClassName: flint-csi
EOF
done

# Wait for all
kubectl wait --for=condition=Bound pvc -l app=load-test --timeout=300s

# Measure time
```

### Load Test 2: 1000 PVCs
```bash
# Use a script for better control
python3 create_pvcs.py --count 1000 --size 10Gi --parallel 50
```

### Metrics to Monitor
- Volume creation duration (p50, p95, p99)
- Cache hit rate
- Node query rate
- Error rate
- CPU/memory usage

## Rollout Plan

**Week 1**: Implement and test caching (Phase 1)
- Add CapacityCache
- Update select_node_for_single_replica()
- Test with 100 PVCs
- Measure improvement

**Week 2**: Add parallel processing and background refresh (Phase 2+3)
- Tune tokio runtime
- Add background refresh
- Test with 1000 PVCs
- Verify cache hit rate

**Week 3**: Optimize and deploy (Phase 4)
- Add Prometheus metrics
- Load testing
- Production deployment

## Success Criteria

- ✅ Create 1000 PVCs in < 2 minutes (without Phase 4)
- ✅ Create 1000 PVCs in < 15 seconds (with all phases)
- ✅ Cache hit rate > 95%
- ✅ No capacity reservation race conditions
- ✅ Even distribution across nodes
- ✅ CPU usage < 80% during load
- ✅ Memory usage stable

## Risk Mitigation

**Cache Staleness**:
- Mitigation: Short TTL (30s) and background refresh
- Verification step after reservation

**Race Conditions**:
- Mitigation: Optimistic locking with capacity reservation
- Rollback on creation failure

**Memory Usage**:
- Cache size: 1000 nodes × 200 bytes = 200KB (negligible)
- Monitor with Prometheus

---

**Priority**: 🔥 **HIGH** - Required for production scale
**Effort**: 2-3 weeks
**Impact**: 60x throughput improvement

