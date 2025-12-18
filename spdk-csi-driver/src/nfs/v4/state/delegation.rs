//! NFSv4 Delegation Management
//!
//! Delegations allow clients to cache file data and metadata without
//! contacting the server for every operation, dramatically improving
//! performance for read-heavy workloads.
//!
//! # Protocol References
//! - RFC 8881 Section 10.4 - Delegations
//! - RFC 8881 Section 10.5 - Delegation Recovery

use super::super::protocol::{StateId, Nfs4Status};
use dashmap::DashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tracing::{debug, info, warn};

/// Delegation type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationType {
    /// Read delegation - client can cache file data and attributes
    /// Safe to grant when no writers exist
    Read,
    
    /// Write delegation - client has exclusive access
    /// More complex, requires careful recall logic
    /// NOT IMPLEMENTED YET - future enhancement
    Write,
}

/// Delegation state
#[derive(Debug, Clone)]
pub struct Delegation {
    /// Delegation stateid
    pub stateid: StateId,
    
    /// Client that owns this delegation
    pub client_id: u64,
    
    /// File handle this delegation applies to
    pub filehandle: Vec<u8>,
    
    /// File path (for reverse lookup)
    pub file_path: PathBuf,
    
    /// Type of delegation (Read or Write)
    pub delegation_type: DelegationType,
    
    /// When the delegation was granted
    pub granted_time: Instant,
    
    /// Has this delegation been recalled?
    pub recalled: bool,
}

impl Delegation {
    /// Create a new delegation
    pub fn new(
        stateid: StateId,
        client_id: u64,
        filehandle: Vec<u8>,
        file_path: PathBuf,
        delegation_type: DelegationType,
    ) -> Self {
        Self {
            stateid,
            client_id,
            filehandle,
            file_path,
            delegation_type,
            granted_time: Instant::now(),
            recalled: false,
        }
    }

    /// Mark this delegation as recalled
    pub fn recall(&mut self) {
        self.recalled = true;
        info!(
            "Delegation {:?} for file {:?} recalled from client {}",
            self.stateid, self.file_path, self.client_id
        );
    }
}

/// Delegation manager - tracks all active delegations
///
/// LOCK-FREE DESIGN using DashMap:
/// - Concurrent delegation lookups without blocking
/// - Lock-free delegation grant and revocation
/// - Per-delegation granularity for high concurrency
pub struct DelegationManager {
    /// Counter for generating unique delegation stateids
    next_stateid: AtomicU64,
    
    /// Active delegations (stateid → delegation)
    /// DashMap enables lock-free concurrent access
    delegations: DashMap<StateId, Delegation>,
    
    /// Delegations by file path (for conflict detection)
    /// Maps file path → list of delegation stateids
    by_file: DashMap<PathBuf, Vec<StateId>>,
    
    /// Delegations by client (for cleanup)
    /// Maps client_id → list of delegation stateids
    by_client: DashMap<u64, Vec<StateId>>,
}

impl DelegationManager {
    /// Create a new delegation manager
    pub fn new() -> Self {
        info!("DelegationManager created");
        
        Self {
            next_stateid: AtomicU64::new(1),
            delegations: DashMap::new(),
            by_file: DashMap::new(),
            by_client: DashMap::new(),
        }
    }

    /// Grant a read delegation
    ///
    /// # Arguments
    /// * `client_id` - Client requesting the delegation
    /// * `filehandle` - File handle for the file
    /// * `file_path` - Path to the file
    ///
    /// # Returns
    /// StateId for the delegation, or None if delegation cannot be granted
    pub fn grant_read_delegation(
        &self,
        client_id: u64,
        filehandle: Vec<u8>,
        file_path: PathBuf,
    ) -> Option<StateId> {
        // Check if we can grant a read delegation
        // We can grant if there are no write delegations on this file
        if !self.can_grant_read_delegation(&file_path) {
            debug!(
                "Cannot grant read delegation for {:?}: conflicts exist",
                file_path
            );
            return None;
        }

        // Generate delegation stateid
        let stateid = self.generate_stateid();

        // Create delegation
        let delegation = Delegation::new(
            stateid,
            client_id,
            filehandle,
            file_path.clone(),
            DelegationType::Read,
        );

        // Store delegation
        self.delegations.insert(stateid, delegation);

        // Add to file index
        self.by_file
            .entry(file_path.clone())
            .or_insert_with(Vec::new)
            .push(stateid);

        // Add to client index
        self.by_client
            .entry(client_id)
            .or_insert_with(Vec::new)
            .push(stateid);

        info!(
            "✅ Granted read delegation {:?} for file {:?} to client {}",
            stateid, file_path, client_id
        );

        Some(stateid)
    }

    /// Check if we can grant a read delegation for a file
    ///
    /// Read delegations can be granted if:
    /// - No write delegations exist for the file
    /// - File is not being modified
    fn can_grant_read_delegation(&self, file_path: &PathBuf) -> bool {
        // Check if there are any delegations for this file
        if let Some(stateids) = self.by_file.get(file_path) {
            // Check if any are write delegations
            for stateid in stateids.iter() {
                if let Some(deleg) = self.delegations.get(stateid) {
                    if deleg.delegation_type == DelegationType::Write {
                        // Write delegation exists, cannot grant read
                        return false;
                    }
                }
            }
        }

        // No conflicts, can grant
        true
    }

    /// Return a delegation (client returning it voluntarily or after recall)
    ///
    /// # Arguments
    /// * `stateid` - Delegation stateid to return
    ///
    /// # Returns
    /// Ok if delegation was found and returned, Err otherwise
    pub fn return_delegation(&self, stateid: &StateId) -> Result<(), Nfs4Status> {
        // Remove delegation
        if let Some((_, delegation)) = self.delegations.remove(stateid) {
            // Remove from file index
            if let Some(mut stateids) = self.by_file.get_mut(&delegation.file_path) {
                stateids.retain(|sid| sid != stateid);
            }

            // Remove from client index
            if let Some(mut stateids) = self.by_client.get_mut(&delegation.client_id) {
                stateids.retain(|sid| sid != stateid);
            }

            info!(
                "✅ Delegation {:?} for file {:?} returned by client {}",
                stateid, delegation.file_path, delegation.client_id
            );

            Ok(())
        } else {
            warn!("❌ Attempted to return unknown delegation {:?}", stateid);
            Err(Nfs4Status::BadStateId)
        }
    }

    /// Recall all read delegations for a file
    ///
    /// This is called when someone wants to write to a file that has
    /// read delegations. Clients must return their delegations before
    /// the write can proceed.
    ///
    /// # Arguments
    /// * `file_path` - Path to the file
    ///
    /// # Returns
    /// List of delegation stateids that need to be recalled
    pub fn recall_read_delegations(&self, file_path: &PathBuf) -> Vec<StateId> {
        let mut recalled = Vec::new();

        if let Some(stateids) = self.by_file.get(file_path) {
            for stateid in stateids.iter() {
                if let Some(mut deleg) = self.delegations.get_mut(stateid) {
                    if deleg.delegation_type == DelegationType::Read && !deleg.recalled {
                        deleg.recall();
                        recalled.push(*stateid);
                    }
                }
            }
        }

        if !recalled.is_empty() {
            info!(
                "📢 Recalling {} read delegations for file {:?}",
                recalled.len(),
                file_path
            );
        }

        recalled
    }

    /// Get delegation by stateid
    pub fn get_delegation(&self, stateid: &StateId) -> Option<Delegation> {
        self.delegations.get(stateid).map(|d| d.clone())
    }

    /// Get all delegations for a file
    pub fn get_delegations_for_file(&self, file_path: &PathBuf) -> Vec<Delegation> {
        let mut delegations = Vec::new();

        if let Some(stateids) = self.by_file.get(file_path) {
            for stateid in stateids.iter() {
                if let Some(deleg) = self.delegations.get(stateid) {
                    delegations.push(deleg.clone());
                }
            }
        }

        delegations
    }

    /// Get all delegations for a client
    pub fn get_delegations_for_client(&self, client_id: u64) -> Vec<Delegation> {
        let mut delegations = Vec::new();

        if let Some(stateids) = self.by_client.get(&client_id) {
            for stateid in stateids.iter() {
                if let Some(deleg) = self.delegations.get(stateid) {
                    delegations.push(deleg.clone());
                }
            }
        }

        delegations
    }

    /// Clean up all delegations for a client (when client expires)
    pub fn cleanup_client_delegations(&self, client_id: u64) {
        if let Some(stateids) = self.by_client.get(&client_id) {
            let stateids_copy = stateids.clone();
            drop(stateids); // Release lock

            for stateid in stateids_copy {
                let _ = self.return_delegation(&stateid);
            }

            info!("🧹 Cleaned up delegations for client {}", client_id);
        }
    }

    /// Get delegation statistics
    pub fn stats(&self) -> DelegationStats {
        let total = self.delegations.len();
        let mut read_count = 0;
        let mut write_count = 0;
        let mut recalled_count = 0;

        for entry in self.delegations.iter() {
            match entry.delegation_type {
                DelegationType::Read => read_count += 1,
                DelegationType::Write => write_count += 1,
            }
            if entry.recalled {
                recalled_count += 1;
            }
        }

        DelegationStats {
            total,
            read_count,
            write_count,
            recalled_count,
        }
    }

    /// Generate a unique delegation stateid
    fn generate_stateid(&self) -> StateId {
        let id = self.next_stateid.fetch_add(1, Ordering::SeqCst);
        
        // Create stateid with seqid=1 and unique other bits
        let mut other = [0u8; 12];
        other[0..8].copy_from_slice(&id.to_be_bytes());
        
        StateId {
            seqid: 1,
            other,
        }
    }
}

impl Default for DelegationManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Delegation statistics
#[derive(Debug, Clone)]
pub struct DelegationStats {
    pub total: usize,
    pub read_count: usize,
    pub write_count: usize,
    pub recalled_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grant_read_delegation() {
        let mgr = DelegationManager::new();
        let file_path = PathBuf::from("/test/file.txt");

        // Grant first delegation
        let stateid1 = mgr.grant_read_delegation(1, vec![1, 2, 3], file_path.clone());
        assert!(stateid1.is_some());

        // Grant second delegation (should succeed - multiple read delegations allowed)
        let stateid2 = mgr.grant_read_delegation(2, vec![1, 2, 3], file_path.clone());
        assert!(stateid2.is_some());

        // Check stats
        let stats = mgr.stats();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.read_count, 2);
    }

    #[test]
    fn test_return_delegation() {
        let mgr = DelegationManager::new();
        let file_path = PathBuf::from("/test/file.txt");

        let stateid = mgr.grant_read_delegation(1, vec![1, 2, 3], file_path.clone()).unwrap();

        // Return delegation
        assert!(mgr.return_delegation(&stateid).is_ok());

        // Stats should show 0
        let stats = mgr.stats();
        assert_eq!(stats.total, 0);
    }

    #[test]
    fn test_recall_delegations() {
        let mgr = DelegationManager::new();
        let file_path = PathBuf::from("/test/file.txt");

        // Grant two read delegations
        mgr.grant_read_delegation(1, vec![1, 2, 3], file_path.clone());
        mgr.grant_read_delegation(2, vec![1, 2, 3], file_path.clone());

        // Recall all delegations for the file
        let recalled = mgr.recall_read_delegations(&file_path);
        assert_eq!(recalled.len(), 2);

        // Check that delegations are marked as recalled
        let delegations = mgr.get_delegations_for_file(&file_path);
        assert_eq!(delegations.len(), 2);
        assert!(delegations.iter().all(|d| d.recalled));
    }

    #[test]
    fn test_cleanup_client_delegations() {
        let mgr = DelegationManager::new();
        let file_path = PathBuf::from("/test/file.txt");

        // Grant delegations to client 1
        mgr.grant_read_delegation(1, vec![1, 2, 3], file_path.clone());
        mgr.grant_read_delegation(1, vec![1, 2, 3], PathBuf::from("/test/file2.txt"));

        // Cleanup client 1
        mgr.cleanup_client_delegations(1);

        // Stats should show 0
        let stats = mgr.stats();
        assert_eq!(stats.total, 0);
    }
}

