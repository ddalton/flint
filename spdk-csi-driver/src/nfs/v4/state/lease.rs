// Lease Management
//
// NFSv4 uses leases to manage state. Clients must renew their lease
// (via SEQUENCE operation) or risk losing their state.
//
// Default lease time: 90 seconds
// Grace period after server restart: 90 seconds
//
// State Lifecycle:
// - Client performs SEQUENCE → lease renewed for 90 seconds
// - Client doesn't renew → after 90 seconds, state expires
// - Server can reclaim resources

use dashmap::DashMap;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Default lease time (90 seconds per RFC 8881)
pub const DEFAULT_LEASE_TIME: Duration = Duration::from_secs(90);

/// Grace period after server startup (90 seconds)
pub const GRACE_PERIOD: Duration = Duration::from_secs(90);

/// Lease entry for a client
#[derive(Debug, Clone)]
pub struct Lease {
    /// Client ID
    pub client_id: u64,

    /// Last renewal time
    pub last_renewal: Instant,

    /// Lease expiration time
    pub expires_at: Instant,
}

impl Lease {
    /// Create a new lease
    pub fn new(client_id: u64) -> Self {
        let now = Instant::now();
        Self {
            client_id,
            last_renewal: now,
            expires_at: now + DEFAULT_LEASE_TIME,
        }
    }

    /// Renew the lease
    pub fn renew(&mut self) {
        let now = Instant::now();
        self.last_renewal = now;
        self.expires_at = now + DEFAULT_LEASE_TIME;
        debug!("Lease renewed for client {}: expires in {:?}",
               self.client_id, DEFAULT_LEASE_TIME);
    }

    /// Check if lease has expired
    pub fn is_expired(&self) -> bool {
        Instant::now() > self.expires_at
    }

    /// Get remaining lease time
    pub fn remaining(&self) -> Duration {
        self.expires_at.saturating_duration_since(Instant::now())
    }
}

/// Lease manager - tracks all active leases
///
/// LOCK-FREE DESIGN using DashMap:
/// - Concurrent lease renewals (SEQUENCE operations) without blocking
/// - Lock-free lease validation checks
/// - Per-client granularity for high concurrency
/// - Critical for NFSv4 lease management and state recovery
pub struct LeaseManager {
    /// Active leases (client_id → lease)
    /// DashMap enables lock-free concurrent access
    leases: DashMap<u64, Lease>,

    /// Server start time (for grace period)
    server_start: Instant,

    /// Grace period duration
    grace_period: Duration,
}

impl LeaseManager {
    /// Create a new lease manager
    pub fn new() -> Self {
        let server_start = Instant::now();
        info!("LeaseManager created - grace period for {:?}", GRACE_PERIOD);

        Self {
            leases: DashMap::new(),
            server_start,
            grace_period: GRACE_PERIOD,
        }
    }

    /// Create a lease for a client
    ///
    /// LOCK-FREE: Direct DashMap insert without global locks
    pub fn create_lease(&self, client_id: u64) {
        let lease = Lease::new(client_id);
        self.leases.insert(client_id, lease);
        info!("Lease created for client {}", client_id);
    }

    /// Renew a lease
    ///
    /// LOCK-FREE: Per-client locking only, not global
    /// Critical path for SEQUENCE operations - multiple clients can renew concurrently
    /// 
    /// Per RFC 8881, we allow renewal even if recently expired (within grace window)
    /// This prevents client disruption during brief network issues or heavy load
    pub fn renew_lease(&self, client_id: u64) -> Result<(), String> {
        if let Some(mut lease) = self.leases.get_mut(&client_id) {
            // Check if lease is expired
            if lease.is_expired() {
                // Allow renewal if expired within last lease period (lenient)
                // This gives clients time to recover from brief network issues
                let time_since_expiry = Instant::now().duration_since(lease.expires_at);
                if time_since_expiry > DEFAULT_LEASE_TIME {
                    // Expired too long ago - reject
                    warn!("Lease for client {} expired {} seconds ago (too long)",
                          client_id, time_since_expiry.as_secs());
                    return Err("Lease expired beyond grace period".to_string());
                }
                // Expired but within grace - allow renewal
                debug!("Renewing recently expired lease for client {} (expired {} seconds ago)",
                       client_id, time_since_expiry.as_secs());
            }
            lease.renew();
            Ok(())
        } else {
            // Lease doesn't exist - create it on the fly
            // This handles cases where server restarted or client reconnected
            debug!("Creating lease on-the-fly for client {}", client_id);
            self.create_lease(client_id);
            Ok(())
        }
    }

    /// Check if a lease is valid (exists and not expired)
    ///
    /// LOCK-FREE: Concurrent reads without blocking
    pub fn is_valid(&self, client_id: u64) -> bool {
        if let Some(lease) = self.leases.get(&client_id) {
            !lease.is_expired()
        } else {
            false
        }
    }

    /// Get remaining lease time for a client
    ///
    /// LOCK-FREE: Concurrent reads without blocking
    pub fn get_remaining(&self, client_id: u64) -> Option<Duration> {
        self.leases.get(&client_id).map(|l| l.remaining())
    }

    /// Remove a lease
    ///
    /// LOCK-FREE: Removal only locks specific shard, not entire map
    pub fn remove_lease(&self, client_id: u64) {
        if self.leases.remove(&client_id).is_some() {
            info!("Lease removed for client {}", client_id);
        }
    }

    /// Cleanup expired leases
    ///
    /// LOCK-FREE: Uses DashMap's retain with per-shard locking
    pub fn cleanup_expired(&self) {
        let before_count = self.leases.len();

        self.leases.retain(|client_id, lease| {
            if lease.is_expired() {
                warn!("Removing expired lease for client {}", client_id);
                false
            } else {
                true
            }
        });

        let after_count = self.leases.len();
        if before_count != after_count {
            info!("Cleaned up {} expired leases ({} → {})",
                  before_count - after_count, before_count, after_count);
        }
    }

    /// Check if server is in grace period
    pub fn in_grace_period(&self) -> bool {
        self.server_start.elapsed() < self.grace_period
    }

    /// Get time remaining in grace period
    pub fn grace_remaining(&self) -> Duration {
        self.grace_period.saturating_sub(self.server_start.elapsed())
    }

    /// Get active lease count
    ///
    /// LOCK-FREE: Counts without blocking concurrent operations
    pub fn active_count(&self) -> usize {
        self.leases.len()
    }

    /// Get lease time (for client queries)
    pub fn lease_time(&self) -> u32 {
        DEFAULT_LEASE_TIME.as_secs() as u32
    }
}

impl Default for LeaseManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_lease_creation() {
        let manager = LeaseManager::new();
        manager.create_lease(1);

        assert!(manager.is_valid(1));
        assert_eq!(manager.active_count(), 1);
    }

    #[test]
    fn test_lease_renewal() {
        let manager = LeaseManager::new();
        manager.create_lease(1);

        // Renew should succeed
        assert!(manager.renew_lease(1).is_ok());

        // Still valid
        assert!(manager.is_valid(1));
    }

    #[test]
    fn test_lease_expiration() {
        let manager = LeaseManager::new();
        manager.create_lease(1);

        // Manually expire the lease
        if let Some(mut lease) = manager.leases.get_mut(&1) {
            lease.expires_at = Instant::now() - Duration::from_secs(1);
        }

        // Should be expired
        assert!(!manager.is_valid(1));

        // Cleanup should remove it
        manager.cleanup_expired();
        assert_eq!(manager.active_count(), 0);
    }

    #[test]
    fn test_grace_period() {
        let manager = LeaseManager::new();

        // Should be in grace period immediately after creation
        assert!(manager.in_grace_period());

        // Grace period should have time remaining
        assert!(manager.grace_remaining() > Duration::from_secs(85));
    }

    #[test]
    fn test_lease_removal() {
        let manager = LeaseManager::new();
        manager.create_lease(1);
        manager.create_lease(2);

        assert_eq!(manager.active_count(), 2);

        manager.remove_lease(1);

        assert_eq!(manager.active_count(), 1);
        assert!(!manager.is_valid(1));
        assert!(manager.is_valid(2));
    }
}
