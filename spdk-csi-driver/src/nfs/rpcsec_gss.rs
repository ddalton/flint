//! RPCSEC_GSS Authentication Support
//!
//! Implementation of RFC 2203 - RPCSEC_GSS Protocol Specification
//! Provides Kerberos authentication for NFS via GSS-API
//!
//! # References
//! - RFC 2203: RPCSEC_GSS Protocol Specification
//! - RFC 2623: NFS Version 2 and Version 3 Security Issues and NFS Protocol's Use of RPCSEC_GSS and Kerberos V5
//! - RFC 1964: The Kerberos Version 5 GSS-API Mechanism

use crate::nfs::xdr::{XdrDecoder, XdrEncoder};
use crate::nfs::kerberos::{Keytab, KerberosContext};
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn, error};

/// RPCSEC_GSS version
pub const RPCSEC_GSS_VERSION: u32 = 1;

/// RPCSEC_GSS procedure numbers
pub mod procedure {
    pub const DATA: u32 = 0;
    pub const INIT: u32 = 1;
    pub const CONTINUE_INIT: u32 = 2;
    pub const DESTROY: u32 = 3;
}

/// RPCSEC_GSS service types (rpc_gss_service_t)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum GssService {
    None = 1,       // rpc_gss_svc_none - authentication only
    Integrity = 2,  // rpc_gss_svc_integrity - integrity protection
    Privacy = 3,    // rpc_gss_svc_privacy - privacy protection
}

impl GssService {
    pub fn from_u32(value: u32) -> Option<Self> {
        match value {
            1 => Some(GssService::None),
            2 => Some(GssService::Integrity),
            3 => Some(GssService::Privacy),
            _ => None,
        }
    }
}

/// RPCSEC_GSS credentials structure
#[derive(Debug, Clone)]
pub struct RpcGssCred {
    pub version: u32,
    pub procedure: u32,
    pub sequence_num: u32,
    pub service: GssService,
    pub handle: Vec<u8>,  // Context handle
}

impl RpcGssCred {
    /// Decode RPCSEC_GSS credentials from XDR
    pub fn decode(data: &Bytes) -> Result<Self, String> {
        let mut dec = XdrDecoder::new(data.clone());

        let version = dec.decode_u32()?;
        if version != RPCSEC_GSS_VERSION {
            return Err(format!("Unsupported RPCSEC_GSS version: {}", version));
        }

        let procedure = dec.decode_u32()?;
        let sequence_num = dec.decode_u32()?;

        let service_val = dec.decode_u32()?;
        let service = GssService::from_u32(service_val)
            .ok_or_else(|| format!("Invalid GSS service: {}", service_val))?;

        let handle = dec.decode_opaque()?.to_vec();

        Ok(Self {
            version,
            procedure,
            sequence_num,
            service,
            handle,
        })
    }
}

/// RPCSEC_GSS init result
#[derive(Debug, Clone)]
pub struct RpcGssInitRes {
    pub handle: Vec<u8>,
    pub major_status: u32,
    pub minor_status: u32,
    pub sequence_window: u32,
    pub gss_token: Vec<u8>,
}

impl RpcGssInitRes {
    /// Encode RPCSEC_GSS init result to XDR
    pub fn encode(&self) -> Bytes {
        let mut enc = XdrEncoder::new();

        enc.encode_opaque(&self.handle);
        enc.encode_u32(self.major_status);
        enc.encode_u32(self.minor_status);
        enc.encode_u32(self.sequence_window);
        enc.encode_opaque(&self.gss_token);

        enc.finish()
    }
}

/// GSS Context for a client session
#[derive(Debug)]
pub struct GssContext {
    pub handle: Vec<u8>,
    pub established: bool,
    pub service: GssService,
    pub sequence_window: u32,
    pub last_seq_num: u32,
    pub seq_bitmap: u128,  // Bitmap for tracking seen sequence numbers in window
    pub kerberos_ctx: Option<KerberosContext>,  // Actual Kerberos context
}

impl GssContext {
    pub fn new(handle: Vec<u8>, service: GssService) -> Self {
        Self {
            handle,
            established: false,
            service,
            sequence_window: 128,  // Default sequence window (must match bitmap size)
            last_seq_num: 0,
            seq_bitmap: 0,  // Initialize empty bitmap
            kerberos_ctx: None,
        }
    }

    pub fn with_kerberos(handle: Vec<u8>, service: GssService, krb_ctx: KerberosContext) -> Self {
        Self {
            handle,
            established: krb_ctx.established,
            service,
            sequence_window: 128,
            last_seq_num: 0,
            seq_bitmap: 0,  // Initialize empty bitmap
            kerberos_ctx: Some(krb_ctx),
        }
    }

    /// Verify sequence number to prevent replay attacks
    ///
    /// Uses a sliding window bitmap to track seen sequence numbers,
    /// allowing out-of-order packet acceptance within the window.
    ///
    /// Algorithm:
    /// 1. If seq_num > last_seq_num: Accept and advance window
    /// 2. If seq_num is within window: Check bitmap for replay
    /// 3. If seq_num is too old (outside window): Reject as replay
    pub fn verify_sequence(&mut self, seq_num: u32) -> bool {
        // Case 1: New highest sequence number - advance the window
        if seq_num > self.last_seq_num {
            let diff = seq_num - self.last_seq_num;

            if diff < self.sequence_window {
                // Shift bitmap left by diff positions, moving window forward
                // Set bit for last_seq_num (mark it as seen before advancing)
                self.seq_bitmap <<= diff;
                self.seq_bitmap |= 1;  // Mark current position as seen
            } else {
                // Gap is larger than window, reset bitmap
                self.seq_bitmap = 0;
            }

            self.last_seq_num = seq_num;
            debug!("Sequence number accepted (new highest): {}", seq_num);
            return true;
        }

        // Case 2: seq_num is within the window (out-of-order packet)
        let diff = self.last_seq_num - seq_num;

        if diff >= self.sequence_window {
            // Too old - outside the window
            warn!("Replay detected: seq_num {} is outside window (last: {}, window: {})",
                  seq_num, self.last_seq_num, self.sequence_window);
            return false;
        }

        // Check if this sequence number was already seen
        let bit_position = diff;
        let mask = 1u128 << bit_position;

        if (self.seq_bitmap & mask) != 0 {
            // Bit is set - this is a replay
            warn!("Replay detected: seq_num {} already seen (last: {})",
                  seq_num, self.last_seq_num);
            return false;
        }

        // Mark this sequence number as seen
        self.seq_bitmap |= mask;
        debug!("Sequence number accepted (within window): {} (diff: {})",
               seq_num, diff);
        true
    }
}

/// RPCSEC_GSS Context Manager
pub struct RpcSecGssManager {
    contexts: Arc<RwLock<HashMap<Vec<u8>, GssContext>>>,
    keytab: Option<Arc<Keytab>>,
}

impl RpcSecGssManager {
    /// Create a new RPCSEC_GSS manager with pure Rust Kerberos implementation
    pub fn new(keytab_path: Option<String>) -> Self {
        info!("🔐 Initializing RPCSEC_GSS manager (Pure Rust implementation)");
        
        let keytab = if let Some(path) = keytab_path {
            info!("📁 Loading keytab from: {}", path);
            match Keytab::load(&path) {
                Ok(kt) => {
                    info!("✅ Keytab loaded successfully with {} keys", kt.keys().len());
                    for key in kt.keys() {
                        debug!("  - {}@{} (kvno={}, enctype={:?})", 
                               key.principal, key.realm, key.kvno, key.enctype);
                    }
                    Some(Arc::new(kt))
                }
                Err(e) => {
                    error!("❌ Failed to load keytab: {}", e);
                    error!("   RPCSEC_GSS authentication will not work!");
                    None
                }
            }
        } else {
            warn!("⚠️  No keytab path specified, RPCSEC_GSS will use placeholder mode");
            None
        };

        Self {
            contexts: Arc::new(RwLock::new(HashMap::new())),
            keytab,
        }
    }

    /// Handle RPCSEC_GSS_INIT - establish new security context
    pub async fn handle_init(&self, cred: &RpcGssCred, init_token: &[u8]) -> RpcGssInitRes {
        info!("🔐 RPCSEC_GSS_INIT: service={:?}, token_len={}", cred.service, init_token.len());
        debug!("   Token (first 64 bytes): {:02x?}", &init_token[..std::cmp::min(64, init_token.len())]);

        // Generate a new context handle
        let handle = self.generate_handle();

        // Attempt to establish Kerberos context using PURE RUST implementation
        let (context, gss_token, major_status, minor_status) = if let Some(ref keytab) = self.keytab {
            match KerberosContext::accept_token(keytab, init_token) {
                Ok((krb_ctx, ap_rep)) => {
                    info!("✅ Kerberos context established (Pure Rust): client={}", krb_ctx.client_principal);
                    let ctx = GssContext::with_kerberos(handle.clone(), cred.service, krb_ctx);
                    (ctx, ap_rep, 0u32, 0u32)  // GSS_S_COMPLETE
                }
                Err(e) => {
                    error!("❌ Kerberos context establishment failed: {}", e);
                    let ctx = GssContext::new(handle.clone(), cred.service);
                    (ctx, Vec::new(), 1u32, 0u32)  // GSS_S_FAILURE
                }
            }
        } else {
            warn!("⚠️  No keytab loaded, using placeholder GSS context");
            let mut ctx = GssContext::new(handle.clone(), cred.service);
            ctx.established = true;  // Accept in placeholder mode
            (ctx, Vec::new(), 0u32, 0u32)  // GSS_S_COMPLETE (placeholder)
        };

        // Store the context
        let mut contexts = self.contexts.write().await;
        contexts.insert(handle.clone(), context);

        debug!("Created GSS context with handle: {:02x?}", handle);

        // Return init result
        RpcGssInitRes {
            handle,
            major_status,
            minor_status,
            sequence_window: 128,
            gss_token,
        }
    }

    /// Handle RPCSEC_GSS_CONTINUE_INIT - continue multi-step context establishment
    pub async fn handle_continue_init(&self, cred: &RpcGssCred, token: &[u8]) -> RpcGssInitRes {
        info!("RPCSEC_GSS_CONTINUE_INIT: handle_len={}, token_len={}",
              cred.handle.len(), token.len());

        let contexts = self.contexts.read().await;
        if let Some(context) = contexts.get(&cred.handle) {
            // TODO: Continue GSS-API context establishment
            RpcGssInitRes {
                handle: context.handle.clone(),
                major_status: 0,  // GSS_S_COMPLETE
                minor_status: 0,
                sequence_window: context.sequence_window,
                gss_token: Vec::new(),
            }
        } else {
            warn!("RPCSEC_GSS_CONTINUE_INIT: context not found");
            RpcGssInitRes {
                handle: cred.handle.clone(),
                major_status: 1,  // GSS_S_FAILURE
                minor_status: 0,
                sequence_window: 0,
                gss_token: Vec::new(),
            }
        }
    }

    /// Handle RPCSEC_GSS_DESTROY - destroy security context
    pub async fn handle_destroy(&self, cred: &RpcGssCred) {
        info!("RPCSEC_GSS_DESTROY: handle={:02x?}", cred.handle);

        let mut contexts = self.contexts.write().await;
        contexts.remove(&cred.handle);
    }

    /// Validate RPCSEC_GSS_DATA message
    pub async fn validate_data(&self, cred: &RpcGssCred) -> Result<(), String> {
        let mut contexts = self.contexts.write().await;

        let context = contexts.get_mut(&cred.handle)
            .ok_or_else(|| "Invalid GSS context handle".to_string())?;

        if !context.established {
            return Err("GSS context not established".to_string());
        }

        // Verify sequence number
        if !context.verify_sequence(cred.sequence_num) {
            return Err("Sequence number verification failed (replay attack?)".to_string());
        }

        // TODO: Verify GSS checksum/signature based on service level
        match cred.service {
            GssService::None => {
                // No integrity/privacy protection, just authentication
                debug!("GSS DATA: authentication only");
            }
            GssService::Integrity => {
                // TODO: Verify GSS_GetMIC checksum
                debug!("GSS DATA: integrity protection (checksum verification pending)");
            }
            GssService::Privacy => {
                // TODO: Decrypt with GSS_Unwrap
                debug!("GSS DATA: privacy protection (decryption pending)");
            }
        }

        Ok(())
    }

    /// Generate a unique context handle
    fn generate_handle(&self) -> Vec<u8> {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        (0..16).map(|_| rng.gen()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gss_service_conversion() {
        assert_eq!(GssService::from_u32(1), Some(GssService::None));
        assert_eq!(GssService::from_u32(2), Some(GssService::Integrity));
        assert_eq!(GssService::from_u32(3), Some(GssService::Privacy));
        assert_eq!(GssService::from_u32(99), None);
    }

    #[test]
    fn test_rpc_gss_cred_decode() {
        let mut enc = XdrEncoder::new();
        enc.encode_u32(1);  // version
        enc.encode_u32(procedure::INIT);  // procedure
        enc.encode_u32(0);  // sequence_num
        enc.encode_u32(1);  // service (None)
        enc.encode_opaque(&[1, 2, 3, 4]);  // handle

        let bytes = enc.finish();
        let cred = RpcGssCred::decode(&bytes).unwrap();

        assert_eq!(cred.version, 1);
        assert_eq!(cred.procedure, procedure::INIT);
        assert_eq!(cred.service, GssService::None);
        assert_eq!(cred.handle, vec![1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn test_gss_context_sequence_verification() {
        let mut ctx = GssContext::new(vec![1, 2, 3, 4], GssService::None);

        // Test basic sequence advancement
        assert!(ctx.verify_sequence(1));
        assert_eq!(ctx.last_seq_num, 1);

        assert!(ctx.verify_sequence(2));
        assert_eq!(ctx.last_seq_num, 2);

        // Test replay detection (same number)
        assert!(!ctx.verify_sequence(2));

        // Test replay detection (old number)
        assert!(!ctx.verify_sequence(1));

        // Test forward jump
        assert!(ctx.verify_sequence(10));
        assert_eq!(ctx.last_seq_num, 10);
    }

    #[tokio::test]
    async fn test_gss_sequence_window_out_of_order() {
        let mut ctx = GssContext::new(vec![1, 2, 3, 4], GssService::None);

        // Accept sequence numbers: 10, 5, 8, 3
        assert!(ctx.verify_sequence(10));  // New highest
        assert_eq!(ctx.last_seq_num, 10);

        // Out-of-order: 5 (within window, diff=5)
        assert!(ctx.verify_sequence(5));
        assert_eq!(ctx.last_seq_num, 10);  // Highest unchanged

        // Out-of-order: 8 (within window, diff=2)
        assert!(ctx.verify_sequence(8));

        // Out-of-order: 3 (within window, diff=7)
        assert!(ctx.verify_sequence(3));

        // Replay: 5 again (should fail)
        assert!(!ctx.verify_sequence(5));

        // Replay: 8 again (should fail)
        assert!(!ctx.verify_sequence(8));

        // New highest: 15
        assert!(ctx.verify_sequence(15));
    }

    #[tokio::test]
    async fn test_gss_sequence_window_boundaries() {
        let mut ctx = GssContext::new(vec![1, 2, 3, 4], GssService::None);

        // Set up window at seq 150
        assert!(ctx.verify_sequence(150));

        // Test within window (150 - 127 = 23, just inside)
        assert!(ctx.verify_sequence(23));

        // Test outside window (150 - 128 = 22, outside for 128-bit window)
        assert!(!ctx.verify_sequence(22));

        // Test far outside window (ancient packet)
        assert!(!ctx.verify_sequence(1));
    }

    #[tokio::test]
    async fn test_gss_sequence_large_gap() {
        let mut ctx = GssContext::new(vec![1, 2, 3, 4], GssService::None);

        // Start at 100
        assert!(ctx.verify_sequence(100));

        // Large jump (> window size) should reset bitmap
        assert!(ctx.verify_sequence(500));

        // Old sequence from before gap (should fail)
        assert!(!ctx.verify_sequence(100));

        // Within new window should work
        assert!(ctx.verify_sequence(490));
    }
}
