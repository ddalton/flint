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
use crate::nfs::kerberos::{Keytab, KerberosContext, KerberosError};
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
    pub kerberos_ctx: Option<KerberosContext>,  // Actual Kerberos context
}

impl GssContext {
    pub fn new(handle: Vec<u8>, service: GssService) -> Self {
        Self {
            handle,
            established: false,
            service,
            sequence_window: 128,  // Default sequence window
            last_seq_num: 0,
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
            kerberos_ctx: Some(krb_ctx),
        }
    }

    /// Verify sequence number to prevent replay attacks
    pub fn verify_sequence(&mut self, seq_num: u32) -> bool {
        // Simple check: sequence number must be greater than last seen
        // TODO: Implement proper sequence window bitmap for out-of-order packets
        if seq_num > self.last_seq_num {
            self.last_seq_num = seq_num;
            true
        } else {
            warn!("Replay detected: seq_num {} <= last_seq_num {}", seq_num, self.last_seq_num);
            false
        }
    }
}

/// RPCSEC_GSS Context Manager
pub struct RpcSecGssManager {
    contexts: Arc<RwLock<HashMap<Vec<u8>, GssContext>>>,
    keytab: Option<Arc<Keytab>>,
}

impl RpcSecGssManager {
    /// Create a new RPCSEC_GSS manager
    pub fn new(keytab_path: Option<String>) -> Self {
        info!("🔐 Initializing RPCSEC_GSS manager");
        
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

        // Attempt to establish Kerberos context
        let (context, gss_token, major_status, minor_status) = if let Some(ref keytab) = self.keytab {
            match KerberosContext::accept_token(keytab, init_token) {
                Ok((krb_ctx, ap_rep)) => {
                    info!("✅ Kerberos context established: client={}", krb_ctx.client_principal);
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

        assert!(ctx.verify_sequence(1));
        assert!(ctx.verify_sequence(2));
        assert!(!ctx.verify_sequence(1));  // Replay
        assert!(ctx.verify_sequence(10));
    }
}
