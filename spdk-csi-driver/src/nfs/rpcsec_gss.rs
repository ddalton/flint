//! RPCSEC_GSS Authentication Support
//!
//! Implementation of RFC 2203 - RPCSEC_GSS Protocol Specification
//! Provides Kerberos authentication for NFS via GSS-API
//!
//! # References
//! - RFC 2203: RPCSEC_GSS Protocol Specification
//! - RFC 2623: NFS Version 2 and Version 3 Security Issues and NFS Protocol's Use of RPCSEC_GSS and Kerberos V5
//! - RFC 1964: The Kerberos Version 5 GSS-API Mechanism

use crate::nfs::gss_framing::{GssReject, ValidatedCall};
use crate::nfs::xdr::{XdrDecoder, XdrEncoder};
use crate::nfs::kerberos::{Keytab, KerberosContext};
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
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

/// RFC 2203 §5.3.3.1: sequence numbers live in [0, RPCSEC_GSS_MAXSEQ).
/// A client that would exceed it must destroy the context and establish
/// a new one; a server that sees one must refuse rather than wrap.
pub const RPCSEC_GSS_MAXSEQ: u32 = 0x8000_0000;

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

/// Whether this process can actually do RPCSEC_GSS.
///
/// Set once, when the manager is built. SECINFO reads it so the server
/// never advertises a mechanism it would then refuse — see
/// `nfs::v4::compound::encode_secinfo_flavors`.
static GSS_AVAILABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// True when a keytab loaded (or the placeholder is explicitly enabled).
pub fn gss_is_available() -> bool {
    GSS_AVAILABLE.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
pub fn set_gss_available_for_test(v: bool) {
    GSS_AVAILABLE.store(v, std::sync::atomic::Ordering::Relaxed);
}

/// Opt-in for the keytab-less placeholder context.
///
/// OFF BY DEFAULT. With no keytab the server cannot authenticate
/// anyone, so it must not establish a context for anyone; this exists
/// only so a test rig can exercise the GSS code path without a KDC.
pub const PLACEHOLDER_ENV: &str = "FLINT_NFS_GSS_INSECURE_PLACEHOLDER";

/// Bounds on the GSS context table.
///
/// Every context is a standing allocation that only an explicit
/// RPCSEC_GSS_DESTROY ever removed, and the NFSv4 `StateQuotas` has
/// never seen this map. These are the two bounds that keep it finite:
/// a ceiling on live contexts, and an idle TTL swept on admission and
/// enforced on use.
#[derive(Debug, Clone, Copy)]
pub struct GssQuotas {
    /// Live contexts (`FLINT_NFS_MAX_GSS_CONTEXTS`, default 1024).
    /// At the cap a new INIT is REFUSED rather than evicting an
    /// existing context: evicting would let a newcomer knock out a
    /// live peer, which is the wrong failure under attack.
    pub max_contexts: usize,
    /// Idle lifetime (`FLINT_NFS_GSS_CONTEXT_TTL_SECS`, default 3600).
    /// Measured from last use, not from creation, so an active client
    /// is never cut off mid-session.
    pub context_ttl: Duration,
}

impl GssQuotas {
    pub fn from_env() -> Self {
        fn env_or(name: &str, default: u64) -> u64 {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|&n| n > 0)
                .unwrap_or(default)
        }
        Self {
            max_contexts: env_or("FLINT_NFS_MAX_GSS_CONTEXTS", 1024) as usize,
            context_ttl: Duration::from_secs(env_or("FLINT_NFS_GSS_CONTEXT_TTL_SECS", 3600)),
        }
    }
}

impl Default for GssQuotas {
    fn default() -> Self {
        Self::from_env()
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
    /// Last DATA call on this context (creation time until then). The
    /// table is swept against this, so an abandoned context cannot hold
    /// a slot forever.
    pub last_used: Instant,
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
            last_used: Instant::now(),
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
            last_used: Instant::now(),
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
        // RFC 2203 §5.3.3.1: the sequence space is bounded. A client that
        // would exceed it must destroy the context and establish a new
        // one, so a number at or above the ceiling is never legitimate --
        // and accepting it would let a peer park `last_seq_num` at the top
        // of the range, after which every honest number is "old".
        if seq_num >= RPCSEC_GSS_MAXSEQ {
            warn!(
                "seq_num {} is at or beyond RPCSEC_GSS_MAXSEQ ({}); refusing",
                seq_num, RPCSEC_GSS_MAXSEQ
            );
            return false;
        }

        // Case 1: New highest sequence number - advance the window
        if seq_num > self.last_seq_num {
            let diff = seq_num - self.last_seq_num;

            if diff < self.sequence_window {
                // Shift bitmap left by diff positions, moving window forward.
                // checked_shl keeps a window wider than the bitmap from
                // panicking in debug and wrapping in release.
                self.seq_bitmap = self.seq_bitmap.checked_shl(diff).unwrap_or(0);
            } else {
                // Gap is larger than window, reset bitmap
                self.seq_bitmap = 0;
            }

            // Bit 0 is the NEW highest, so it is marked in BOTH arms. It
            // used to be set only when the window slid: a gap wider than
            // the window reset the bitmap and left the very number that
            // caused the reset unmarked, so it could be replayed exactly
            // once -- including the first call on a fresh context, whose
            // last_seq_num starts at 0.
            self.seq_bitmap |= 1;

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
    quotas: GssQuotas,
    /// Establish contexts with no keytab and no authentication. See
    /// [`PLACEHOLDER_ENV`] — off unless explicitly asked for.
    allow_placeholder: bool,
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

        let allow_placeholder = matches!(
            std::env::var(PLACEHOLDER_ENV).ok().as_deref(),
            Some("1") | Some("true") | Some("TRUE")
        );
        if keytab.is_none() {
            if allow_placeholder {
                error!(
                    "🚨 {} is set with no keytab: every RPCSEC_GSS_INIT will be ACCEPTED \
                     WITHOUT AUTHENTICATION. This is a test-rig setting — never run it in \
                     production.",
                    PLACEHOLDER_ENV
                );
            } else {
                warn!(
                    "⚠️  No keytab loaded — RPCSEC_GSS_INIT will be REFUSED. Set KRB5_KTNAME \
                     for Kerberos, or {}=1 in a test rig.",
                    PLACEHOLDER_ENV
                );
            }
        }

        Self::with_policy_and_keytab(keytab, GssQuotas::from_env(), allow_placeholder)
    }

    /// Construct with explicit policy instead of reading the
    /// environment. Used by tests, which must not race each other over
    /// process-wide env vars.
    pub fn with_policy(
        keytab_path: Option<String>,
        quotas: GssQuotas,
        allow_placeholder: bool,
    ) -> Self {
        let keytab = keytab_path
            .and_then(|path| Keytab::load(&path).ok())
            .map(Arc::new);
        Self::with_policy_and_keytab(keytab, quotas, allow_placeholder)
    }

    fn with_policy_and_keytab(
        keytab: Option<Arc<Keytab>>,
        quotas: GssQuotas,
        allow_placeholder: bool,
    ) -> Self {
        // SECINFO consults this so the advertisement matches reality.
        GSS_AVAILABLE.store(
            keytab.is_some() || allow_placeholder,
            std::sync::atomic::Ordering::Relaxed,
        );

        Self {
            contexts: Arc::new(RwLock::new(HashMap::new())),
            keytab,
            quotas,
            allow_placeholder,
        }
    }

    /// The per-message token machinery for an established context.
    ///
    /// Needed by RPCSEC_GSS_INIT, whose reply verifier is a MIC over the
    /// sequence window (RFC 2203 §5.2.3.1) and therefore has to be built
    /// after the context exists but before any DATA call arrives.
    pub async fn tokens_for(
        &self,
        handle: &[u8],
    ) -> Option<crate::nfs::krb::token::PerMessageTokens<crate::nfs::krb::token::ContextKey>> {
        let contexts = self.contexts.read().await;
        contexts
            .get(handle)
            .and_then(|c| c.kerberos_ctx.as_ref())
            .and_then(|k| k.per_message_tokens().ok())
    }

    /// Number of live contexts. The bound this reports is the one the
    /// quota enforces; exposed so a test can assert a refusal stored
    /// nothing rather than trusting the status code alone.
    pub async fn context_count(&self) -> usize {
        self.contexts.read().await.len()
    }

    /// GSS_S_FAILURE, storing nothing.
    fn init_failure(handle: Vec<u8>) -> RpcGssInitRes {
        RpcGssInitRes {
            handle,
            major_status: 1, // GSS_S_FAILURE
            minor_status: 0,
            sequence_window: 0,
            gss_token: Vec::new(),
        }
    }

    /// Handle RPCSEC_GSS_INIT - establish new security context
    pub async fn handle_init(&self, cred: &RpcGssCred, init_token: &[u8]) -> RpcGssInitRes {
        info!("🔐 RPCSEC_GSS_INIT: service={:?}, token_len={}", cred.service, init_token.len());
        debug!("   Token (first 64 bytes): {:02x?}", &init_token[..std::cmp::min(64, init_token.len())]);

        let handle = self.generate_handle();

        // Attempt to establish Kerberos context using PURE RUST implementation.
        // NOTHING is stored unless it establishes: a failed exchange used to
        // leave a context behind, so a peer with no credential at all grew
        // this map by one entry per RPC.
        let (context, gss_token) = if let Some(ref keytab) = self.keytab {
            match KerberosContext::accept_token(keytab, init_token) {
                Ok((krb_ctx, ap_rep)) => {
                    info!("✅ Kerberos context established (Pure Rust): client={}", krb_ctx.client_principal);
                    (GssContext::with_kerberos(handle.clone(), cred.service, krb_ctx), ap_rep)
                }
                Err(e) => {
                    error!("❌ Kerberos context establishment failed: {}", e);
                    return Self::init_failure(handle);
                }
            }
        } else if self.allow_placeholder {
            warn!("⚠️  No keytab loaded — establishing an UNAUTHENTICATED placeholder GSS context");
            let mut ctx = GssContext::new(handle.clone(), cred.service);
            ctx.established = true;
            (ctx, Vec::new())
        } else {
            error!(
                "❌ RPCSEC_GSS_INIT refused: no keytab loaded, so this caller cannot be \
                 authenticated. Set KRB5_KTNAME, or {}=1 in a test rig.",
                PLACEHOLDER_ENV
            );
            return Self::init_failure(handle);
        };

        // Belt and braces: `with_kerberos` inherits the Kerberos
        // context's own flag, so an accept_token that returns Ok with an
        // unestablished context must not reach the table either.
        if !context.established {
            error!("❌ RPCSEC_GSS_INIT refused: context did not establish");
            return Self::init_failure(handle);
        }

        {
            let mut contexts = self.contexts.write().await;
            let ttl = self.quotas.context_ttl;
            let before = contexts.len();
            contexts.retain(|_, c| c.last_used.elapsed() < ttl);
            if before != contexts.len() {
                debug!("Swept {} idle GSS contexts", before - contexts.len());
            }
            if contexts.len() >= self.quotas.max_contexts {
                error!(
                    "❌ RPCSEC_GSS_INIT refused: {} live GSS contexts is at the cap \
                     (FLINT_NFS_MAX_GSS_CONTEXTS); refusing rather than evicting a live peer",
                    contexts.len()
                );
                return Self::init_failure(handle);
            }
            contexts.insert(handle.clone(), context);
        }

        debug!("Created GSS context with handle: {:02x?}", handle);

        // Return init result
        RpcGssInitRes {
            handle,
            major_status: 0, // GSS_S_COMPLETE
            minor_status: 0,
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
            // The table holds only established contexts (see
            // `handle_init`), so there is never a partial exchange to
            // continue. Report what the context actually is rather than
            // an unconditional GSS_S_COMPLETE.
            RpcGssInitRes {
                handle: context.handle.clone(),
                major_status: if context.established { 0 } else { 1 },
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

    /// Advance the replay window for a call whose checksum has ALREADY
    /// been verified (RFC 2203 §5.3.3.1).
    ///
    /// Split out of `validate_data` so that an unauthenticated peer cannot
    /// move the window by sending a well-formed credential with a bad MIC.
    pub async fn accept_sequence(&self, cred: &RpcGssCred) -> Result<(), GssReject> {
        let mut contexts = self.contexts.write().await;
        let context = contexts
            .get_mut(&cred.handle)
            .ok_or_else(|| GssReject::CtxProblem("invalid GSS context handle".into()))?;
        if context.verify_sequence(cred.sequence_num) {
            Ok(())
        } else {
            Err(GssReject::CredProblem(
                "sequence number verification failed (replay?)".into(),
            ))
        }
    }

    /// Handle RPCSEC_GSS_DESTROY - destroy security context
    pub async fn handle_destroy(&self, cred: &RpcGssCred) {
        info!("RPCSEC_GSS_DESTROY: handle={:02x?}", cred.handle);

        let mut contexts = self.contexts.write().await;
        contexts.remove(&cred.handle);
    }

    /// Validate RPCSEC_GSS_DATA and hand back what the body needs.
    ///
    /// SPENDS NOTHING. The sequence window is advanced by
    /// [`Self::accept_sequence`], which the dispatcher calls only after the
    /// call verifier has proved the caller holds the key -- the order RFC
    /// 2203 §5.3.3.1 gives, and the reverse of what this did. Advancing
    /// first let anyone holding a captured record park `last_seq_num` at a
    /// number of their choosing, with a MIC that was never going to verify,
    /// and every later call from the real client fell outside the window.
    ///
    /// This used to answer `Ok(())` for `Integrity` and `Privacy` having
    /// verified nothing — the caller was told its RPC had been validated
    /// while the payload travelled unchecked — and then briefly refused
    /// them outright. Both are now implemented, so it returns the
    /// per-message machinery instead of a bare unit, because the caller
    /// cannot unseal the body or sign the reply without it.
    pub async fn validate_data(&self, cred: &RpcGssCred) -> Result<ValidatedCall, GssReject> {
        let mut contexts = self.contexts.write().await;
        let ttl = self.quotas.context_ttl;
        let mut expired = false;

        let result = {
            let context = contexts
                .get_mut(&cred.handle)
                .ok_or_else(|| GssReject::CtxProblem("invalid GSS context handle".into()))?;

            if !context.established {
                Err(GssReject::CtxProblem("GSS context not established".into()))
            } else if context.last_used.elapsed() >= ttl {
                // An idle context must not keep working merely because
                // no INIT has arrived to run the sweep.
                expired = true;
                Err(GssReject::CtxProblem("GSS context expired".into()))
            } else {
                // RFC 4121 §2 base key, via the Kerberos context. The
                // keytab-less placeholder holds none, so it can serve
                // svc_none and nothing else.
                let tokens = match &context.kerberos_ctx {
                    Some(k) => Some(
                        k.per_message_tokens()
                            .map_err(|e| GssReject::CtxProblem(e.to_string()))?,
                    ),
                    None => None,
                };
                if cred.service != GssService::None && tokens.is_none() {
                    Err(GssReject::CtxProblem(
                        "context holds no key material (placeholder mode); per-message \
                         protection is impossible"
                            .into(),
                    ))
                } else {
                    debug!("GSS DATA: service={:?}", cred.service);
                    context.last_used = Instant::now();
                    Ok(ValidatedCall {
                        service: cred.service,
                        seq_num: cred.sequence_num,
                        client_principal: context
                            .kerberos_ctx
                            .as_ref()
                            .map(|k| k.client_principal.clone()),
                        tokens,
                    })
                }
            }
        };

        if expired {
            contexts.remove(&cred.handle);
        }
        result
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

    // ---- context-table policy -------------------------------------
    //
    // These cover four defects that shipped together: a keytab-less
    // server established a context for anyone; a FAILED exchange still
    // stored one; the table had no ceiling and no expiry; and DATA
    // calls asking for integrity or privacy were answered Ok(()) with
    // nothing verified.

    fn quotas(max: usize, ttl_ms: u64) -> GssQuotas {
        GssQuotas { max_contexts: max, context_ttl: Duration::from_millis(ttl_ms) }
    }

    fn cred(service: GssService, procedure: u32, handle: Vec<u8>, seq: u32) -> RpcGssCred {
        RpcGssCred {
            version: RPCSEC_GSS_VERSION,
            procedure,
            sequence_num: seq,
            service,
            handle,
        }
    }

    /// The bypass: no keytab meant "accept everybody".
    #[tokio::test]
    async fn a_keytab_less_server_refuses_gss_init_and_stores_nothing() {
        let mgr = RpcSecGssManager::with_policy(None, quotas(1024, 60_000), false);
        let res = mgr
            .handle_init(&cred(GssService::None, procedure::INIT, vec![], 1), b"token")
            .await;

        assert_eq!(res.major_status, 1, "keytab-less INIT must be GSS_S_FAILURE");
        assert_eq!(mgr.context_count().await, 0, "a refused INIT must store no context");
    }

    /// Anti-vacuity control for the test above: the refusal is the
    /// POLICY, not an INIT path that cannot succeed at all. Same
    /// manager, same call, placeholder enabled — it establishes.
    #[tokio::test]
    async fn the_placeholder_context_is_opt_in_and_still_works() {
        let mgr = RpcSecGssManager::with_policy(None, quotas(1024, 60_000), true);
        let res = mgr
            .handle_init(&cred(GssService::None, procedure::INIT, vec![], 1), b"token")
            .await;

        assert_eq!(res.major_status, 0, "placeholder mode should establish");
        assert_eq!(mgr.context_count().await, 1);
    }

    /// INIT now accepts all three services — krb5i and krb5p are
    /// implemented — so what is left to refuse is a context with no key
    /// material behind it.
    #[tokio::test]
    async fn init_accepts_every_service_now_that_they_are_implemented() {
        for service in [GssService::None, GssService::Integrity, GssService::Privacy] {
            let mgr = RpcSecGssManager::with_policy(None, quotas(1024, 60_000), true);
            let res = mgr
                .handle_init(&cred(service, procedure::INIT, vec![], 1), b"token")
                .await;
            assert_eq!(res.major_status, 0, "{:?} should establish", service);
            assert_eq!(mgr.context_count().await, 1);
        }
    }

    /// RFC 2203 §5.3.3.1 orders the acceptor's work: verify the header
    /// checksum FIRST, then the sequence number. flint had it the other
    /// way round, and the consequence was not academic -- the wire drill
    /// found it. `verify_sequence` MUTATES `last_seq_num`, so a peer with
    /// a captured record and no key at all could rewrite its seq_num to a
    /// large value, watch the MIC check reject the call, and leave the
    /// context's window parked at that number. Every subsequent call from
    /// the real client then fell outside the window and was refused as a
    /// replay: an unauthenticated wedge of a live mount.
    ///
    /// So `validate_data` must SPEND NOTHING. Advancing the window is
    /// `accept_sequence`, which the dispatcher calls only once the MIC has
    /// proved the caller holds the key.
    #[tokio::test]
    async fn validate_data_spends_no_sequence_number() {
        let mgr = RpcSecGssManager::with_policy(None, quotas(1024, 60_000), true);
        let init = mgr
            .handle_init(&cred(GssService::None, procedure::INIT, vec![], 1), b"token")
            .await;
        let handle = init.handle.clone();
        let c = |seq| cred(GssService::None, procedure::DATA, handle.clone(), seq);

        // Repeated validation of the SAME seq must not consume it: until
        // the checksum is verified, nothing about this call is trusted.
        mgr.validate_data(&c(7)).await.expect("first validate");
        mgr.validate_data(&c(7)).await.expect("validate must not spend the seq");
        // A wild seq_num must not move the window either.
        let _ = mgr.validate_data(&c(9_000)).await;
        mgr.validate_data(&c(8)).await.expect("the window must not have moved");

        // Spending is explicit, and only then does replay bite.
        mgr.accept_sequence(&c(7)).await.expect("first spend");
        assert!(
            mgr.accept_sequence(&c(7)).await.is_err(),
            "a spent sequence number must be refused"
        );
        mgr.accept_sequence(&c(8)).await.expect("a fresh one still works");
    }

    /// The placeholder context holds NO keys, so per-message protection is
    /// impossible on it — and that must be a CTXPROBLEM (re-init and
    /// retry), never a silent pass.
    #[tokio::test]
    async fn a_keyless_context_refuses_per_message_protection() {
        let mgr = RpcSecGssManager::with_policy(None, quotas(1024, 60_000), true);
        let h = mgr
            .handle_init(&cred(GssService::None, procedure::INIT, vec![], 1), b"t")
            .await
            .handle;

        // Control: svc_none needs no key material, so it validates.
        let v = mgr
            .validate_data(&cred(GssService::None, procedure::DATA, h.clone(), 1))
            .await
            .expect("svc_none needs no keys");
        assert!(v.tokens.is_none(), "placeholder really has no keys");

        // Distinct sequence numbers: the replay window is live, and reusing
        // one here would refuse the second call for the WRONG reason.
        for (i, service) in [GssService::Integrity, GssService::Privacy].iter().enumerate() {
            let err = mgr
                .validate_data(&cred(*service, procedure::DATA, h.clone(), 2 + i as u32))
                .await
                .expect_err("no key material means no per-message protection");
            assert_eq!(
                err.auth_stat(),
                crate::nfs::rpc::AuthStat::RpcsecGssCtxProblem,
                "{:?} must tell the client to re-init, not fail the op",
                service
            );
        }
    }

    #[tokio::test]
    async fn the_context_table_is_capped() {
        let mgr = RpcSecGssManager::with_policy(None, quotas(2, 60_000), true);
        for i in 0..2 {
            let res = mgr
                .handle_init(&cred(GssService::None, procedure::INIT, vec![], i), b"t")
                .await;
            assert_eq!(res.major_status, 0, "init {i} should be admitted");
        }

        let over = mgr
            .handle_init(&cred(GssService::None, procedure::INIT, vec![], 3), b"t")
            .await;
        assert_eq!(over.major_status, 1, "the third INIT is over the cap");
        assert_eq!(mgr.context_count().await, 2, "the cap must not be exceeded");
    }

    #[tokio::test]
    async fn an_idle_context_expires_and_frees_its_slot() {
        let mgr = RpcSecGssManager::with_policy(None, quotas(1, 50), true);
        let h = mgr
            .handle_init(&cred(GssService::None, procedure::INIT, vec![], 1), b"t")
            .await
            .handle;

        // Control: before the TTL it validates, so the refusal below is
        // the expiry and not a context that never worked.
        assert!(mgr
            .validate_data(&cred(GssService::None, procedure::DATA, h.clone(), 1))
            .await
            .is_ok());

        tokio::time::sleep(Duration::from_millis(80)).await;

        let err = mgr
            .validate_data(&cred(GssService::None, procedure::DATA, h.clone(), 2))
            .await
            .expect_err("an idle context past its TTL must not validate");
        assert!(err.reason().contains("expired"), "unexpected error: {err}");
        assert_eq!(mgr.context_count().await, 0, "the expired context is dropped on use");

        // And the slot is genuinely reclaimed: the cap is 1, so this
        // only succeeds if the sweep removed the old entry.
        let res = mgr
            .handle_init(&cred(GssService::None, procedure::INIT, vec![], 4), b"t")
            .await;
        assert_eq!(res.major_status, 0, "the swept slot should be reusable");
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

    /// The gap case above proves an OLD number is refused after a reset.
    /// It never asks about the number that CAUSED the reset.
    #[test]
    fn the_jump_that_resets_the_window_cannot_be_replayed() {
        let mut ctx = GssContext::new(vec![1, 2, 3, 4], GssService::None);
        assert!(ctx.verify_sequence(100));
        assert!(ctx.verify_sequence(500), "the jump itself is legitimate");
        assert!(
            !ctx.verify_sequence(500),
            "500 was already spent on the call that reset the window"
        );
    }

    /// The same hole on a fresh context: last_seq_num starts at 0 with an
    /// empty bitmap, so a first call far above the window leaves itself
    /// unmarked.
    #[test]
    fn a_first_call_beyond_the_window_cannot_be_replayed() {
        let mut ctx = GssContext::new(vec![1, 2, 3, 4], GssService::None);
        assert!(ctx.verify_sequence(200));
        assert!(!ctx.verify_sequence(200), "200 was already spent");
    }

    /// RFC 2203 §5.3.3.1: a seq_num at or above RPCSEC_GSS_MAXSEQ must be
    /// refused; the client is expected to destroy the context and start a
    /// new one rather than wrap.
    #[test]
    fn a_sequence_number_at_maxseq_is_refused() {
        let mut ctx = GssContext::new(vec![1, 2, 3, 4], GssService::None);
        assert!(!ctx.verify_sequence(RPCSEC_GSS_MAXSEQ));
        assert!(!ctx.verify_sequence(u32::MAX));
    }
}
