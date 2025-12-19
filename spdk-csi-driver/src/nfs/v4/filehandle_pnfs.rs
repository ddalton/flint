//! pNFS-Aware Filehandle Format
//!
//! For pNFS with independent DS storage, we need filehandles that work
//! across different servers without shared filesystem paths.
//!
//! This module implements file-ID based filehandles where:
//! - file_id uniquely identifies the file (hash of name or inode)
//! - stripe_index identifies which stripe segment (0, 1, 2...)
//! - Each DS maps (file_id, stripe_index) to its local storage path
//!
//! # Format
//! Version 2 (pNFS):
//! - version: 2 (1 byte)
//! - instance_id: cluster-wide shared ID (8 bytes)
//! - file_id: unique file identifier (8 bytes)
//! - stripe_index: which stripe (4 bytes)
//! - total: 21 bytes (fits well within 128 byte limit)

use crate::nfs::v4::protocol::Nfs4FileHandle;
use std::path::Path;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Generate file ID from filename
/// This must be deterministic so same filename always gets same ID
pub fn generate_file_id(filename: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    filename.hash(&mut hasher);
    hasher.finish()
}

/// Generate pNFS filehandle for a specific stripe on a specific DS
///
/// # Arguments
/// * `instance_id` - Cluster-wide shared instance ID
/// * `filename` - Original filename (e.g., "test.dat")
/// * `stripe_index` - Which stripe segment (0, 1, 2...)
///
/// # Returns
/// Filehandle that works on any DS with matching instance_id
pub fn generate_pnfs_filehandle(
    instance_id: u64,
    filename: &str,
    stripe_index: u32,
) -> Nfs4FileHandle {
    let file_id = generate_file_id(filename);
    
    let mut data = Vec::with_capacity(21);
    
    // Version 2 = pNFS file-ID based
    data.push(2);
    
    // Instance ID (cluster-wide)
    data.extend_from_slice(&instance_id.to_be_bytes());
    
    // File ID (deterministic from filename)
    data.extend_from_slice(&file_id.to_be_bytes());
    
    // Stripe index
    data.extend_from_slice(&stripe_index.to_be_bytes());
    
    Nfs4FileHandle { data }
}

/// Parse pNFS filehandle to extract components
///
/// # Returns
/// (instance_id, file_id, stripe_index)
pub fn parse_pnfs_filehandle(handle: &Nfs4FileHandle) -> Result<(u64, u64, u32), String> {
    if handle.data.len() < 21 {
        return Err("Filehandle too short for pNFS format".to_string());
    }
    
    // Check version
    if handle.data[0] != 2 {
        return Err(format!("Not a pNFS filehandle (version={})", handle.data[0]));
    }
    
    // Extract instance_id
    let mut instance_bytes = [0u8; 8];
    instance_bytes.copy_from_slice(&handle.data[1..9]);
    let instance_id = u64::from_be_bytes(instance_bytes);
    
    // Extract file_id
    let mut file_id_bytes = [0u8; 8];
    file_id_bytes.copy_from_slice(&handle.data[9..17]);
    let file_id = u64::from_be_bytes(file_id_bytes);
    
    // Extract stripe_index
    let mut stripe_bytes = [0u8; 4];
    stripe_bytes.copy_from_slice(&handle.data[17..21]);
    let stripe_index = u32::from_be_bytes(stripe_bytes);
    
    Ok((instance_id, file_id, stripe_index))
}

/// Map pNFS filehandle to local DS storage path
///
/// # Arguments
/// * `handle` - pNFS filehandle
/// * `base_path` - DS storage directory (e.g., "/mnt/pnfs-data")
///
/// # Returns
/// Full path where this stripe should be stored on this DS
/// Example: "/mnt/pnfs-data/12345678abcdef.stripe0"
pub fn filehandle_to_ds_path(handle: &Nfs4FileHandle, base_path: &Path) -> Result<std::path::PathBuf, String> {
    let (instance_id, file_id, stripe_index) = parse_pnfs_filehandle(handle)?;
    
    // Verify instance_id matches (stale check)
    let expected_instance = std::env::var("PNFS_INSTANCE_ID")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    
    if expected_instance != 0 && instance_id != expected_instance {
        return Err(format!("Stale filehandle: instance {} != {}", instance_id, expected_instance));
    }
    
    // Generate local storage path
    let filename = format!("{:016x}.stripe{}", file_id, stripe_index);
    let path = base_path.join(filename);
    
    Ok(path)
}

/// Check if a filehandle is pNFS format
pub fn is_pnfs_filehandle(handle: &Nfs4FileHandle) -> bool {
    handle.data.len() >= 21 && handle.data[0] == 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_and_parse() {
        let instance_id = 1734648000000000000u64;
        let filename = "test.dat";
        let stripe_index = 5;
        
        let fh = generate_pnfs_filehandle(instance_id, filename, stripe_index);
        
        assert_eq!(fh.data.len(), 21);
        assert_eq!(fh.data[0], 2); // version
        
        let (parsed_inst, parsed_file_id, parsed_stripe) = parse_pnfs_filehandle(&fh).unwrap();
        assert_eq!(parsed_inst, instance_id);
        assert_eq!(parsed_stripe, stripe_index);
        
        // Same filename should give same file_id
        let file_id2 = generate_file_id(filename);
        assert_eq!(parsed_file_id, file_id2);
    }
    
    #[test]
    fn test_deterministic_file_id() {
        let file_id1 = generate_file_id("myfile.txt");
        let file_id2 = generate_file_id("myfile.txt");
        assert_eq!(file_id1, file_id2);
        
        let file_id3 = generate_file_id("different.txt");
        assert_ne!(file_id1, file_id3);
    }
    
    #[test]
    fn test_ds_path_mapping() {
        let instance_id = 1734648000000000000u64;
        let fh = generate_pnfs_filehandle(instance_id, "test.dat", 3);
        
        let ds_path = filehandle_to_ds_path(&fh, Path::new("/mnt/pnfs-data")).unwrap();
        
        // Path should be based on file_id, not original filename
        assert!(ds_path.to_string_lossy().contains(".stripe3"));
        assert!(ds_path.starts_with("/mnt/pnfs-data"));
    }
}

