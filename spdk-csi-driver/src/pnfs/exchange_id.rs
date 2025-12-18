//! pNFS EXCHANGE_ID Handler
//!
//! Handles EXCHANGE_ID operation for pNFS MDS, setting the appropriate
//! server role flags to indicate pNFS support.
//!
//! # Protocol Reference
//! - RFC 8881 Section 18.35 - EXCHANGE_ID operation
//! - RFC 8881 Section 18.35.3 - Server role flags

use crate::nfs::v4::protocol::exchgid_flags;

/// Modify EXCHANGE_ID response flags for pNFS MDS
///
/// When running as pNFS MDS, we need to set the USE_PNFS_MDS flag
/// to tell clients that this server supports pNFS and can provide layouts.
///
/// # Arguments
/// * `flags` - Original flags from base EXCHANGE_ID handler
///
/// # Returns
/// Modified flags with pNFS MDS role set
pub fn set_pnfs_mds_flags(flags: u32) -> u32 {
    // Clear any existing pNFS role flags
    let mut new_flags = flags & !exchgid_flags::MASK_PNFS;
    
    // Set USE_PNFS_MDS flag (RFC 8881 Section 18.35.3)
    new_flags |= exchgid_flags::USE_PNFS_MDS;
    
    new_flags
}

/// Check if server is in pNFS MDS mode
///
/// This can be used to conditionally enable pNFS features based on
/// server configuration.
pub fn is_pnfs_mds_mode(flags: u32) -> bool {
    (flags & exchgid_flags::USE_PNFS_MDS) != 0
}

/// Check if server is in pNFS DS mode
pub fn is_pnfs_ds_mode(flags: u32) -> bool {
    (flags & exchgid_flags::USE_PNFS_DS) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_pnfs_mds_flags() {
        // Start with USE_NON_PNFS
        let flags = exchgid_flags::USE_NON_PNFS;
        
        // Convert to MDS mode
        let mds_flags = set_pnfs_mds_flags(flags);
        
        // Should have USE_PNFS_MDS set
        assert!(is_pnfs_mds_mode(mds_flags));
        assert!(!is_pnfs_ds_mode(mds_flags));
        
        // Should NOT have USE_NON_PNFS
        assert_eq!(mds_flags & exchgid_flags::USE_NON_PNFS, 0);
    }

    #[test]
    fn test_flag_detection() {
        let mds_flags = exchgid_flags::USE_PNFS_MDS;
        assert!(is_pnfs_mds_mode(mds_flags));
        assert!(!is_pnfs_ds_mode(mds_flags));

        let ds_flags = exchgid_flags::USE_PNFS_DS;
        assert!(!is_pnfs_mds_mode(ds_flags));
        assert!(is_pnfs_ds_mode(ds_flags));

        let non_pnfs = exchgid_flags::USE_NON_PNFS;
        assert!(!is_pnfs_mds_mode(non_pnfs));
        assert!(!is_pnfs_ds_mode(non_pnfs));
    }
}

