// capacity_cache.rs - In-memory capacity cache for node storage

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
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
        info!(ttl_seconds, "[CACHE] Creating capacity cache");
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
                    debug!(node_name, ?age, free_gb = capacity.free_capacity / (1024 * 1024 * 1024), "[CACHE] Hit");
                    return Ok(capacity.clone());
                } else {
                    debug!(node_name, ?age, ?self.ttl, "[CACHE] Stale entry");
                }
            } else {
                debug!(node_name, "[CACHE] Miss");
            }
        }

        // Cache miss or stale - refresh
        let capacity = self.refresh_node_capacity(node_name, driver).await?;

        // Update cache (write lock)
        {
            let mut cache = self.cache.write().await;
            cache.insert(node_name.to_string(), capacity.clone());
            debug!(node_name, free_gb = capacity.free_capacity / (1024 * 1024 * 1024), "[CACHE] Updated");
        }

        Ok(capacity)
    }

    /// Refresh capacity from node agent
    async fn refresh_node_capacity(
        &self,
        node_name: &str,
        driver: &SpdkCsiDriver,
    ) -> Result<NodeCapacity, MinimalStateError> {
        debug!(node_name, "[CACHE] Refreshing capacity");

        // Query node for disk information
        let disks = driver.get_initialized_disks_from_node(node_name).await?;

        let total_capacity: u64 = disks.iter().map(|d| d.size_bytes).sum();
        let free_capacity: u64 = disks.iter().map(|d| d.free_space).sum();
        let disk_count = disks.len() as u32;

        debug!(
            node_name,
            disk_count,
            free_gb = free_capacity / (1024 * 1024 * 1024),
            total_gb = total_capacity / (1024 * 1024 * 1024),
            "[CACHE] Refreshed"
        );

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
                debug!(
                    node_name,
                    reserved_gb = size_bytes / (1024 * 1024 * 1024),
                    remaining_gb = capacity.free_capacity / (1024 * 1024 * 1024),
                    "[CACHE] Reserved capacity"
                );
                return Ok(());
            } else {
                warn!(
                    node_name,
                    need_gb = size_bytes / (1024 * 1024 * 1024),
                    available_gb = capacity.free_capacity / (1024 * 1024 * 1024),
                    "[CACHE] Insufficient capacity"
                );
                return Err(MinimalStateError::InsufficientCapacity {
                    required: size_bytes,
                    available: capacity.free_capacity,
                });
            }
        }

        warn!(node_name, "[CACHE] Node not in cache for reservation");
        Err(MinimalStateError::InternalError {
            message: format!("Node {} not in capacity cache", node_name),
        })
    }

    /// Release capacity if volume creation fails
    pub async fn release_capacity(&self, node_name: &str, size_bytes: u64) {
        let mut cache = self.cache.write().await;

        if let Some(capacity) = cache.get_mut(node_name) {
            capacity.free_capacity += size_bytes;
            debug!(
                node_name,
                released_gb = size_bytes / (1024 * 1024 * 1024),
                new_free_gb = capacity.free_capacity / (1024 * 1024 * 1024),
                "[CACHE] Released capacity"
            );
        } else {
            warn!(node_name, "[CACHE] Could not release capacity - node not in cache");
        }
    }

    /// Invalidate cache entry (force refresh on next query)
    pub async fn invalidate(&self, node_name: &str) {
        let mut cache = self.cache.write().await;
        cache.remove(node_name);
        debug!(node_name, "[CACHE] Invalidated");
    }

    /// Invalidate all entries (force full refresh)
    pub async fn invalidate_all(&self) {
        let mut cache = self.cache.write().await;
        let count = cache.len();
        cache.clear();
        debug!(count, "[CACHE] Invalidated all entries");
    }

    /// Warm up cache on startup - parallel refresh of all nodes
    pub async fn warm_up(
        &self,
        driver: &SpdkCsiDriver,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!("[CACHE] Warming up capacity cache");

        // Get all nodes in cluster
        let all_nodes = driver.get_all_nodes().await?;
        info!(node_count = all_nodes.len(), "[CACHE] Found nodes to warm up");

        if all_nodes.is_empty() {
            warn!("[CACHE] No nodes found in cluster");
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
                        warn!(node, error = %e, "[CACHE] Failed to warm up node");
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
                    warn!(error = %e, "[CACHE] Task join error");
                }
            }
        }

        info!(success_count, "[CACHE] Warm up complete");

        Ok(())
    }

    /// Start background refresh task
    /// This keeps the cache fresh without waiting for TTL expiration
    pub fn start_background_refresh(
        cache: Arc<Self>,
        driver: Arc<SpdkCsiDriver>,
        interval_secs: u64,
    ) {
        info!(interval_secs, "[CACHE] Starting background refresh");

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));

            loop {
                interval.tick().await;

                debug!("[CACHE] Background refresh starting");

                // Get all nodes in the cluster (not just cached ones)
                // This ensures nodes that failed during warm_up still get refreshed
                let nodes = match driver.get_all_nodes().await {
                    Ok(nodes) => nodes,
                    Err(e) => {
                        warn!(error = %e, "[CACHE] Failed to get cluster nodes");
                        continue;
                    }
                };

                if nodes.is_empty() {
                    warn!("[CACHE] No nodes in cluster to refresh");
                    continue;
                }

                debug!(node_count = nodes.len(), "[CACHE] Refreshing nodes");

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
                                debug!(node_name, "[CACHE] Background refreshed");
                                true
                            }
                            Err(e) => {
                                warn!(node_name, error = %e, "[CACHE] Failed to refresh");
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
                            warn!(error = %e, "[CACHE] Task error");
                            failed += 1;
                        }
                    }
                }

                debug!(success, failed, "[CACHE] Background refresh complete");
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

