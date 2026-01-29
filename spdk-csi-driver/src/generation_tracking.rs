// generation_tracking.rs - SPDK lvol xattr-based generation tracking for replica consistency
//
// This module implements generation tracking for detecting out-of-sync replicas using
// SPDK's blob xattr functionality. Generation metadata is stored in each replica's
// blob xattrs, providing self-contained, expansion-safe tracking with zero I/O overhead.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Generation metadata stored in lvol blob xattrs
/// Stored as 24-byte binary structure encoded in base64
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerationMetadata {
    /// Magic number for validation (0x4753504B = "GSPK")
    pub magic: u32,
    /// Monotonically increasing counter (incremented on each volume attach)
    pub generation: u64,
    /// Unix timestamp (seconds) when generation was set
    pub timestamp: u64,
    /// Node identifier that set this generation (hash of node name)
    pub node_id: u32,
}

impl GenerationMetadata {
    /// Magic number for validation ("GSPK")
    pub const MAGIC: u32 = 0x4753504B;
    
    /// Xattr key name used in SPDK blob storage
    pub const XATTR_NAME: &'static str = "csi.generation";
    
    /// Create new generation metadata
    pub fn new(generation: u64, node_name: &str) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        
        let node_id = Self::hash_node_name(node_name);
        
        Self {
            magic: Self::MAGIC,
            generation,
            timestamp,
            node_id,
        }
    }
    
    /// Create initial generation (generation 0)
    pub fn initial(node_name: &str) -> Self {
        Self::new(0, node_name)
    }
    
    /// Create next generation
    pub fn next(&self, node_name: &str) -> Self {
        Self::new(self.generation + 1, node_name)
    }
    
    /// Hash node name to u32 for compact storage
    fn hash_node_name(node_name: &str) -> u32 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        
        let mut hasher = DefaultHasher::new();
        node_name.hash(&mut hasher);
        (hasher.finish() & 0xFFFFFFFF) as u32
    }
    
    /// Pack metadata into binary format (24 bytes, little-endian)
    /// Format: magic(4) + generation(8) + timestamp(8) + node_id(4)
    pub fn pack(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(24);
        buf.extend_from_slice(&self.magic.to_le_bytes());
        buf.extend_from_slice(&self.generation.to_le_bytes());
        buf.extend_from_slice(&self.timestamp.to_le_bytes());
        buf.extend_from_slice(&self.node_id.to_le_bytes());
        buf
    }
    
    /// Pack metadata into base64 string for SPDK RPC transport
    pub fn pack_base64(&self) -> String {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        STANDARD.encode(self.pack())
    }
    
    /// Unpack metadata from binary format
    pub fn unpack(data: &[u8]) -> Result<Self, GenerationError> {
        if data.len() < 24 {
            return Err(GenerationError::InvalidFormat(
                format!("Expected 24 bytes, got {}", data.len())
            ));
        }
        
        let magic = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        if magic != Self::MAGIC {
            return Err(GenerationError::InvalidMagic {
                expected: Self::MAGIC,
                got: magic,
            });
        }
        
        let generation = u64::from_le_bytes([
            data[4], data[5], data[6], data[7],
            data[8], data[9], data[10], data[11],
        ]);
        
        let timestamp = u64::from_le_bytes([
            data[12], data[13], data[14], data[15],
            data[16], data[17], data[18], data[19],
        ]);
        
        let node_id = u32::from_le_bytes([data[20], data[21], data[22], data[23]]);
        
        Ok(Self {
            magic,
            generation,
            timestamp,
            node_id,
        })
    }
    
    /// Unpack metadata from base64 string
    pub fn unpack_base64(b64_data: &str) -> Result<Self, GenerationError> {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        
        let data = STANDARD.decode(b64_data)
            .map_err(|e| GenerationError::InvalidFormat(format!("Base64 decode error: {}", e)))?;
        
        Self::unpack(&data)
    }
    
    /// Check if this metadata is valid
    pub fn is_valid(&self) -> bool {
        self.magic == Self::MAGIC
    }
}

/// Generation tracking error types
#[derive(Debug)]
pub enum GenerationError {
    /// Invalid metadata format
    InvalidFormat(String),
    /// Magic number mismatch
    InvalidMagic { expected: u32, got: u32 },
    /// SPDK RPC error
    SpdkError(String),
    /// Xattr not found (replica not initialized)
    NotInitialized,
}

impl std::fmt::Display for GenerationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GenerationError::InvalidFormat(msg) => write!(f, "Invalid format: {}", msg),
            GenerationError::InvalidMagic { expected, got } => {
                write!(f, "Invalid magic: expected 0x{:08X}, got 0x{:08X}", expected, got)
            }
            GenerationError::SpdkError(msg) => write!(f, "SPDK error: {}", msg),
            GenerationError::NotInitialized => write!(f, "Replica not initialized"),
        }
    }
}

impl std::error::Error for GenerationError {}

/// Extended replica information with generation tracking
#[derive(Debug, Clone)]
pub struct ReplicaGenerationInfo {
    pub replica_index: usize,
    pub lvol_name: String,
    pub generation: u64,
    pub timestamp: u64,
    pub node_id: u32,
    pub is_current: bool, // true if generation matches max
}

/// Result of generation comparison across replicas
#[derive(Debug)]
pub struct GenerationComparisonResult {
    pub max_generation: u64,
    pub current_replicas: Vec<usize>, // Indices of replicas with max generation
    pub stale_replicas: Vec<usize>,   // Indices of replicas that need rebuild
    pub uninitialized_replicas: Vec<usize>, // Indices of replicas without generation
}

impl GenerationComparisonResult {
    /// Check if all replicas are in sync
    pub fn is_consistent(&self) -> bool {
        self.stale_replicas.is_empty() && self.uninitialized_replicas.is_empty()
    }
    
    /// Get total number of out-of-sync replicas
    pub fn out_of_sync_count(&self) -> usize {
        self.stale_replicas.len() + self.uninitialized_replicas.len()
    }
    
    /// Check if we have at least one current replica to rebuild from
    pub fn can_rebuild(&self) -> bool {
        !self.current_replicas.is_empty()
    }
}

/// Compare generations across multiple replicas
pub fn compare_generations(
    replica_gens: Vec<Option<GenerationMetadata>>
) -> GenerationComparisonResult {
    let mut max_generation = 0u64;
    let mut current_replicas = Vec::new();
    let mut stale_replicas = Vec::new();
    let mut uninitialized_replicas = Vec::new();
    
    // Find max generation
    for (i, gen_opt) in replica_gens.iter().enumerate() {
        if let Some(gen) = gen_opt {
            if gen.generation > max_generation {
                max_generation = gen.generation;
            }
        } else {
            uninitialized_replicas.push(i);
        }
    }
    
    // Classify replicas
    for (i, gen_opt) in replica_gens.iter().enumerate() {
        if let Some(gen) = gen_opt {
            if gen.generation == max_generation {
                current_replicas.push(i);
            } else {
                stale_replicas.push(i);
            }
        }
    }
    
    GenerationComparisonResult {
        max_generation,
        current_replicas,
        stale_replicas,
        uninitialized_replicas,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_generation_metadata_pack_unpack() {
        let gen = GenerationMetadata::new(42, "node-1");
        let packed = gen.pack();
        
        assert_eq!(packed.len(), 24);
        
        let unpacked = GenerationMetadata::unpack(&packed).unwrap();
        assert_eq!(unpacked.magic, GenerationMetadata::MAGIC);
        assert_eq!(unpacked.generation, 42);
        assert_eq!(unpacked.node_id, gen.node_id);
    }
    
    #[test]
    fn test_generation_metadata_base64() {
        let gen = GenerationMetadata::new(100, "node-2");
        let b64 = gen.pack_base64();
        
        let unpacked = GenerationMetadata::unpack_base64(&b64).unwrap();
        assert_eq!(unpacked.generation, 100);
    }
    
    #[test]
    fn test_generation_next() {
        let gen1 = GenerationMetadata::new(5, "node-1");
        let gen2 = gen1.next("node-2");
        
        assert_eq!(gen2.generation, 6);
    }
    
    #[test]
    fn test_invalid_magic() {
        let mut data = vec![0u8; 24];
        data[0..4].copy_from_slice(&0xDEADBEEFu32.to_le_bytes());
        
        let result = GenerationMetadata::unpack(&data);
        assert!(matches!(result, Err(GenerationError::InvalidMagic { .. })));
    }
    
    #[test]
    fn test_compare_generations_all_current() {
        let gen = GenerationMetadata::new(10, "node-1");
        let gens = vec![Some(gen.clone()), Some(gen.clone()), Some(gen.clone())];
        
        let result = compare_generations(gens);
        assert_eq!(result.max_generation, 10);
        assert_eq!(result.current_replicas.len(), 3);
        assert_eq!(result.stale_replicas.len(), 0);
        assert!(result.is_consistent());
    }
    
    #[test]
    fn test_compare_generations_with_stale() {
        let gen_old = GenerationMetadata::new(5, "node-1");
        let gen_new = GenerationMetadata::new(10, "node-2");
        
        let gens = vec![Some(gen_new.clone()), Some(gen_old), Some(gen_new)];
        
        let result = compare_generations(gens);
        assert_eq!(result.max_generation, 10);
        assert_eq!(result.current_replicas, vec![0, 2]);
        assert_eq!(result.stale_replicas, vec![1]);
        assert!(!result.is_consistent());
        assert!(result.can_rebuild());
    }
    
    #[test]
    fn test_compare_generations_with_uninitialized() {
        let gen = GenerationMetadata::new(7, "node-1");
        
        let gens = vec![Some(gen.clone()), None, Some(gen)];
        
        let result = compare_generations(gens);
        assert_eq!(result.max_generation, 7);
        assert_eq!(result.current_replicas, vec![0, 2]);
        assert_eq!(result.uninitialized_replicas, vec![1]);
        assert!(!result.is_consistent());
    }
}
