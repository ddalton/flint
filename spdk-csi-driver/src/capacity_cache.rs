// capacity_cache.rs - In-memory capacity cache for node storage

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use crate::driver::SpdkCsiDriver;
use crate::minimal_models::MinimalStateError;

/// Cached capacity information for a node
#[derive(Clone, Debug)]
pub struct NodeCapacity {
    pub node_name: String,
    pub total_capacity: u64,
    pub free_capacity: u64,
    pub disk_count: u32,
    pub last_updated: Instant,
}

/// In-memory capacity cache with TTL
#[derive(Clone)]
pub struct CapacityCache {
    cache: Arc<RwLock<HashMap<String, NodeCapacity>>>,
    ttl: Duration,
}

impl CapacityCache {
    /// Create a new capacity cache with specified TTL in seconds
    pub fn new(ttl_seconds: u64) -> Self {
        println!("📦 [CACHE] Creating capacity cache with TTL: {}s", ttl_seconds);
        Self {
            cache: Arc::new(RwLock::new(HashMap::new())),
            ttl: Duration::from_secs(ttl_seconds),
        }
    }

    /// Get node capacity from cache or refresh if stale
    pub async fn get_node_capacity(
        &self,
        node_name: &str,
        driver: &SpdkCsiDriver,
    ) -> Result<NodeCapacity, MinimalStateError> {
        // Check cache first (read lock)
        {
            let cache = self.cache.read().await;
            if let Some(capacity) = cache.get(node_name) {
                let age = capacity.last_updated.elapsed();
                if age < self.ttl {
                    println!("✅ [CACHE] Hit for node: {} (age: {:?}, free: {}GB)",
                             node_name,
                             age,
                             capacity.free_capacity / (1024 * 1024 * 1024));
                    return Ok(capacity.clone());
                } else {
                    println!("⏰ [CACHE] Stale for node: {} (age: {:?}, TTL: {:?})",
                             node_name, age, self.ttl);
                }
            } else {
                println!("❌ [CACHE] Miss for node: {}", node_name);
            }
        }

        // Cache miss or stale - refresh
        let capacity = self.refresh_node_capacity(node_name, driver).await?;

        // Update cache (write lock)
        {
            let mut cache = self.cache.write().await;
            cache.insert(node_name.to_string(), capacity.clone());
            println!("🔄 [CACHE] Updated cache for node: {} (free: {}GB)",
                     node_name,
                     capacity.free_capacity / (1024 * 1024 * 1024));
        }

        Ok(capacity)
    }

    /// Refresh capacity from node agent
    async fn refresh_node_capacity(
        &self,
        node_name: &str,
        driver: &SpdkCsiDriver,
    ) -> Result<NodeCapacity, MinimalStateError> {
        println!("🔍 [CACHE] Refreshing capacity for node: {}", node_name);

        // Query node for disk information
        let disks = driver.get_initialized_disks_from_node(node_name).await?;

        let total_capacity: u64 = disks.iter().map(|d| d.size_bytes).sum();
        let free_capacity: u64 = disks.iter().map(|d| d.free_space).sum();
        let disk_count = disks.len() as u32;

        println!("✅ [CACHE] Refreshed node: {} - {} disks, {}GB free / {}GB total",
                 node_name,
                 disk_count,
                 free_capacity / (1024 * 1024 * 1024),
                 total_capacity / (1024 * 1024 * 1024));

        Ok(NodeCapacity {
            node_name: node_name.to_string(),
            total_capacity,
            free_capacity,
            disk_count,
            last_updated: Instant::now(),
        })
    }

    /// Reserve capacity (optimistic locking)
    /// This reduces race conditions during volume creation
    pub async fn reserve_capacity(
        &self,
        node_name: &str,
        size_bytes: u64,
    ) -> Result<(), MinimalStateError> {
        let mut cache = self.cache.write().await;

        if let Some(capacity) = cache.get_mut(node_name) {
            if capacity.free_capacity >= size_bytes {
                capacity.free_capacity -= size_bytes;
                println!("✅ [CACHE] Reserved {}GB on node: {} (remaining: {}GB)",
                         size_bytes / (1024 * 1024 * 1024),
                         node_name,
                         capacity.free_capacity / (1024 * 1024 * 1024));
                return Ok(());
            } else {
                println!("⚠️ [CACHE] Insufficient capacity on node: {} (need: {}GB, available: {}GB)",
                         node_name,
                         size_bytes / (1024 * 1024 * 1024),
                         capacity.free_capacity / (1024 * 1024 * 1024));
                return Err(MinimalStateError::InsufficientCapacity {
                    required: size_bytes,
                    available: capacity.free_capacity,
                });
            }
        }

        println!("⚠️ [CACHE] Node {} not in cache for reservation", node_name);
        Err(MinimalStateError::InternalError {
            message: format!("Node {} not in capacity cache", node_name),
        })
    }

    /// Release capacity if volume creation fails
    pub async fn release_capacity(&self, node_name: &str, size_bytes: u64) {
        let mut cache = self.cache.write().await;

        if let Some(capacity) = cache.get_mut(node_name) {
            capacity.free_capacity += size_bytes;
            println!("⚠️ [CACHE] Released {}GB on node: {} (new free: {}GB)",
                     size_bytes / (1024 * 1024 * 1024),
                     node_name,
                     capacity.free_capacity / (1024 * 1024 * 1024));
        } else {
            println!("⚠️ [CACHE] Could not release capacity - node {} not in cache", node_name);
        }
    }

    /// Invalidate cache entry (force refresh on next query)
    pub async fn invalidate(&self, node_name: &str) {
        let mut cache = self.cache.write().await;
        cache.remove(node_name);
        println!("🔄 [CACHE] Invalidated cache for node: {}", node_name);
    }

    /// Invalidate all entries (force full refresh)
    pub async fn invalidate_all(&self) {
        let mut cache = self.cache.write().await;
        let count = cache.len();
        cache.clear();
        println!("🔄 [CACHE] Invalidated all {} cache entries", count);
    }

    /// Warm up cache on startup - parallel refresh of all nodes
    pub async fn warm_up(
        &self,
        driver: &SpdkCsiDriver,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("🔥 [CACHE] Warming up capacity cache...");

        // Get all nodes in cluster
        let all_nodes = driver.get_all_nodes().await?;
        println!("🔥 [CACHE] Found {} nodes to warm up", all_nodes.len());

        if all_nodes.is_empty() {
            println!("⚠️ [CACHE] No nodes found in cluster");
            return Ok(());
        }

        // Refresh all nodes in parallel
        let mut tasks = Vec::new();
        for node_name in all_nodes {
            let cache = self.clone();
            let driver = driver.clone();

            tasks.push(tokio::spawn(async move {
                let node = node_name.clone();
                match cache.refresh_node_capacity(&node, &driver).await {
                    Ok(capacity) => Some(capacity),
                    Err(e) => {
                        println!("⚠️ [CACHE] Failed to warm up node {}: {}", node, e);
                        None
                    }
                }
            }));
        }

        // Wait for all refreshes and collect results
        let mut success_count = 0;
        for task in tasks {
            match task.await {
                Ok(Some(capacity)) => {
                    let mut cache = self.cache.write().await;
                    cache.insert(capacity.node_name.clone(), capacity);
                    success_count += 1;
                }
                Ok(None) => {} // Already logged error
                Err(e) => {
                    println!("⚠️ [CACHE] Task join error: {}", e);
                }
            }
        }

        println!("✅ [CACHE] Warm up complete: {}/{} nodes cached",
                 success_count,
                 success_count); // We only count successful ones

        Ok(())
    }

    /// Start background refresh task
    /// This keeps the cache fresh without waiting for TTL expiration
    pub fn start_background_refresh(
        cache: Arc<Self>,
        driver: Arc<SpdkCsiDriver>,
        interval_secs: u64,
    ) {
        println!("🔄 [CACHE] Starting background refresh (interval: {}s)", interval_secs);

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

                if nodes.is_empty() {
                    println!("⚠️ [CACHE] No nodes in cache to refresh");
                    continue;
                }

                println!("🔄 [CACHE] Refreshing {} nodes...", nodes.len());

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
                                println!("✅ [CACHE] Background refreshed: {}", node_name);
                                true
                            }
                            Err(e) => {
                                println!("⚠️ [CACHE] Failed to refresh {}: {}", node_name, e);
                                false
                            }
                        }
                    }));
                }

                // Wait for all refreshes
                let mut success = 0;
                let mut failed = 0;
                for task in tasks {
                    match task.await {
                        Ok(true) => success += 1,
                        Ok(false) => failed += 1,
                        Err(e) => {
                            println!("⚠️ [CACHE] Task error: {}", e);
                            failed += 1;
                        }
                    }
                }

                println!("✅ [CACHE] Background refresh complete: {} succeeded, {} failed",
                         success, failed);
            }
        });
    }

    /// Get cache statistics
    pub async fn get_stats(&self) -> CacheStats {
        let cache = self.cache.read().await;
        CacheStats {
            cached_nodes: cache.len(),
            total_capacity: cache.values().map(|c| c.total_capacity).sum(),
            total_free: cache.values().map(|c| c.free_capacity).sum(),
            oldest_entry_age: cache
                .values()
                .map(|c| c.last_updated.elapsed())
                .max()
                .unwrap_or(Duration::from_secs(0)),
        }
    }
}

/// Cache statistics
#[derive(Debug)]
pub struct CacheStats {
    pub cached_nodes: usize,
    pub total_capacity: u64,
    pub total_free: u64,
    pub oldest_entry_age: Duration,
}

impl std::fmt::Display for CacheStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Cache Stats: {} nodes, {}GB / {}GB free, oldest entry: {:?}",
            self.cached_nodes,
            self.total_free / (1024 * 1024 * 1024),
            self.total_capacity / (1024 * 1024 * 1024),
            self.oldest_entry_age
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_cache_creation() {
        let cache = CapacityCache::new(30);
        let stats = cache.get_stats().await;
        assert_eq!(stats.cached_nodes, 0);
    }

    #[tokio::test]
    async fn test_reserve_and_release() {
        let cache = CapacityCache::new(30);

        // Manually insert test data
        {
            let mut cache_map = cache.cache.write().await;
            cache_map.insert(
                "test-node".to_string(),
                NodeCapacity {
                    node_name: "test-node".to_string(),
                    total_capacity: 1000 * 1024 * 1024 * 1024, // 1000GB
                    free_capacity: 500 * 1024 * 1024 * 1024,   // 500GB
                    disk_count: 2,
                    last_updated: Instant::now(),
                },
            );
        }

        // Test reservation
        let reserve_size = 100 * 1024 * 1024 * 1024; // 100GB
        let result = cache.reserve_capacity("test-node", reserve_size).await;
        assert!(result.is_ok());

        // Check remaining capacity
        {
            let cache_map = cache.cache.read().await;
            let node = cache_map.get("test-node").unwrap();
            assert_eq!(node.free_capacity, 400 * 1024 * 1024 * 1024); // 400GB left
        }

        // Test release
        cache.release_capacity("test-node", reserve_size).await;

        // Check capacity restored
        {
            let cache_map = cache.cache.read().await;
            let node = cache_map.get("test-node").unwrap();
            assert_eq!(node.free_capacity, 500 * 1024 * 1024 * 1024); // Back to 500GB
        }
    }

    #[tokio::test]
    async fn test_insufficient_capacity() {
        let cache = CapacityCache::new(30);

        // Manually insert test data with limited capacity
        {
            let mut cache_map = cache.cache.write().await;
            cache_map.insert(
                "test-node".to_string(),
                NodeCapacity {
                    node_name: "test-node".to_string(),
                    total_capacity: 100 * 1024 * 1024 * 1024,
                    free_capacity: 50 * 1024 * 1024 * 1024, // 50GB
                    disk_count: 1,
                    last_updated: Instant::now(),
                },
            );
        }

        // Try to reserve more than available
        let reserve_size = 100 * 1024 * 1024 * 1024; // 100GB
        let result = cache.reserve_capacity("test-node", reserve_size).await;
        assert!(result.is_err());
    }
}

