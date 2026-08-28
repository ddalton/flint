//! Sun RPC (Remote Procedure Call) implementation
//!
//! Implementation of RFC 5531 - RPC: Remote Procedure Call Protocol Specification Version 2
//! https://datatracker.ietf.org/doc/html/rfc5531
//!
//! RPC provides the foundation for NFS. Each NFS operation is an RPC call.

use super::xdr::{XdrDecoder, XdrEncoder};
use bytes::Bytes;

/// RPC program number for NFS
pub const NFS_PROGRAM: u32 = 100003;

/// NFS version 3
pub const NFS_VERSION: u32 = 3;

/// RPC program number for MOUNT protocol
pub const MOUNT_PROGRAM: u32 = 100005;

/// MOUNT protocol version 3
pub const MOUNT_VERSION: u32 = 3;

/// RPC message types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    Call = 0,
    Reply = 1,
}

/// RPC reply status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyStatus {
    Accepted = 0,
    Denied = 1,
}

/// RPC accept status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptStatus {
    Success = 0,
    ProgUnavail = 1,
    ProgMismatch = 2,
    ProcUnavail = 3,
    GarbageArgs = 4,
    SystemErr = 5,
}

/// RPC authentication status for a MSG_DENIED / AUTH_ERROR reply
/// (RFC 5531 §9, extended by RFC 2203 §5.3.3.3).
///
/// The two RPCSEC_GSS values are the ones that matter: a client that is
/// told CREDPROBLEM or CTXPROBLEM **refreshes its context and retries**,
/// where any accepted-status error makes it give up. Collapsing GSS
/// failures into SYSTEM_ERR — which is what this server did — turns a
/// recoverable context expiry into a hard mount error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStat {
    Ok = 0,
    BadCred = 1,
    RejectedCred = 2,
    BadVerf = 3,
    RejectedVerf = 4,
    TooWeak = 5,
    /// RFC 2203: the credential was bad — client should re-init the context.
    RpcsecGssCredProblem = 13,
    /// RFC 2203: the context is gone or expired — client should re-init.
    RpcsecGssCtxProblem = 14,
}

/// RPC authentication flavor
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthFlavor {
    Null = 0,
    Unix = 1,
    RpcsecGss = 6,  // RPCSEC_GSS (RFC 2203)
}

/// Authentication credentials
#[derive(Debug, Clone)]
pub struct Auth {
    pub flavor: AuthFlavor,
    pub body: Bytes,
}

impl Auth {
    pub fn null() -> Self {
        Self {
            flavor: AuthFlavor::Null,
            body: Bytes::new(),
        }
    }

    pub fn encode(&self, enc: &mut XdrEncoder) {
        enc.encode_u32(self.flavor as u32);
        enc.encode_opaque(&self.body);
    }

    pub fn decode(dec: &mut XdrDecoder) -> Result<Self, String> {
        let flavor_val = dec.decode_u32()?;
        let flavor = match flavor_val {
            0 => AuthFlavor::Null,
            1 => AuthFlavor::Unix,
            6 => AuthFlavor::RpcsecGss,
            _ => return Err(format!("Unknown auth flavor: {}", flavor_val)),
        };
        let body = dec.decode_opaque()?;

        Ok(Self { flavor, body })
    }

    /// The caller's unix (uid, gid) when this credential is AUTH_SYS.
    /// None for AUTH_NONE / GSS / an undecodable body. Used by the file
    /// operations to stamp ownership on created objects — without it every
    /// file lands owned by the server process (root) and
    /// ownership-sensitive workloads (postgres checks st_uid == geteuid)
    /// refuse to run on an RWX volume.
    pub fn unix_uid_gid(&self) -> Option<(u32, u32)> {
        if self.flavor != AuthFlavor::Unix {
            return None;
        }
        // authsys_parms = { stamp:u32, machinename:string<255>,
        //                   uid:u32, gid:u32, gids:u32<16> }
        let mut dec = XdrDecoder::new(self.body.clone());
        let _stamp = dec.decode_u32().ok()?;
        let _machinename = dec.decode_opaque().ok()?;
        let uid = dec.decode_u32().ok()?;
        let gid = dec.decode_u32().ok()?;
        Some((uid, gid))
    }

    /// The caller's SUPPLEMENTARY groups (`authsys_parms.gids<16>`).
    ///
    /// These were decoded and discarded until permission checking
    /// existed, at which point they stop being cosmetic: a user whose
    /// access to a file comes from a supplementary group is WRONGLY
    /// DENIED without them, which turns a security fix into an
    /// availability bug. RFC 5531 caps the array at 16; anything longer
    /// is a malformed frame and is truncated rather than trusted.
    pub fn unix_gids(&self) -> Vec<u32> {
        if self.flavor != AuthFlavor::Unix {
            return Vec::new();
        }
        let mut dec = XdrDecoder::new(self.body.clone());
        if dec.decode_u32().is_err() { return Vec::new(); }          // stamp
        if dec.decode_opaque().is_err() { return Vec::new(); }       // machinename
        if dec.decode_u32().is_err() { return Vec::new(); }          // uid
        if dec.decode_u32().is_err() { return Vec::new(); }          // gid
        let Ok(n) = dec.decode_u32() else { return Vec::new() };
        let n = (n as usize).min(16);
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            match dec.decode_u32() {
                Ok(g) => out.push(g),
                Err(_) => break,
            }
        }
        out
    }

    /// Compute a principal identity from this credential.
    ///
    /// Used by EXCHANGE_ID's RFC 8881 §18.35.5 client-record state machine
    /// to detect "same client owner, *different* principal" — which means
    /// "another security principal is trying to use the same client
    /// identifier" and changes the EXCHANGE_ID outcome.
    ///
    /// Encoding (designed to be stable for equality comparison only):
    ///   * AUTH_NONE → empty Vec
    ///   * AUTH_SYS  → "sys:<machinename>:<uid>" — derived from the
    ///                authsys_parms struct, ignoring the per-call timestamp
    ///                and gid list.
    ///   * RPCSEC_GSS → "gss:<context handle>" — the credential also
    ///                carries a PER-CALL SEQUENCE NUMBER, so using the whole
    ///                body made every call from one client look like a
    ///                different principal. Measured against a real Linux
    ///                client: CREATE_SESSION reported a principal collision
    ///                between two calls of the same mount. The AUTH_SYS arm
    ///                above already takes this care with its stamp.
    ///   * unknown   → "raw:<flavor>:<body>"
    pub fn principal(&self) -> Vec<u8> {
        match self.flavor {
            AuthFlavor::Null => Vec::new(),
            AuthFlavor::Unix => {
                // authsys_parms = { stamp:u32, machinename:string<255>,
                //                   uid:u32, gid:u32, gids:u32<16> }
                let mut dec = XdrDecoder::new(self.body.clone());
                let _stamp = dec.decode_u32().unwrap_or(0);
                let machinename = dec.decode_opaque().unwrap_or_default();
                let uid = dec.decode_u32().unwrap_or(0);
                let mut p = Vec::with_capacity(machinename.len() + 16);
                p.extend_from_slice(b"sys:");
                p.extend_from_slice(&machinename);
                p.extend_from_slice(b":");
                p.extend_from_slice(uid.to_string().as_bytes());
                p
            }
            AuthFlavor::RpcsecGss => {
                // rpc_gss_cred_t = { version, proc, seq_num, service,
                //                    handle<> }. Take ONLY the handle:
                // seq_num changes on every single call.
                let mut dec = XdrDecoder::new(self.body.clone());
                let _version = dec.decode_u32().unwrap_or(0);
                let _proc = dec.decode_u32().unwrap_or(0);
                let _seq = dec.decode_u32().unwrap_or(0);
                let _service = dec.decode_u32().unwrap_or(0);
                let handle = dec.decode_opaque().unwrap_or_default();
                let mut p = Vec::with_capacity(handle.len() + 4);
                p.extend_from_slice(b"gss:");
                p.extend_from_slice(&handle);
                p
            }
        }
    }
}

/// RPC call message
#[derive(Debug)]
pub struct CallMessage {
    pub xid: u32,
    pub program: u32,
    pub version: u32,
    pub procedure: u32,
    pub cred: Auth,
    pub verf: Auth,
    /// The RPC header from byte 0 up to and INCLUDING the credential,
    /// and stopping before the verifier.
    ///
    /// RFC 2203 §5.3.1 makes the call verifier a `GSS_GetMIC` over
    /// exactly this span, so RPCSEC_GSS cannot authenticate a call
    /// without it. It could not be recovered later — decoding is
    /// destructive and this struct kept only parsed fields — so every
    /// GSS call arrived with its verifier unverifiable and the server
    /// merely debug-printed it.
    ///
    /// Zero-copy: `Bytes::slice` over the same allocation.
    pub cred_span: Bytes,
}

impl CallMessage {
    pub fn decode(buf: Bytes) -> Result<Self, String> {
        let original = buf.clone();
        let total = original.len();
        let mut dec = XdrDecoder::new(buf);

        let xid = dec.decode_u32()?;

        let msg_type = dec.decode_u32()?;
        if msg_type != MessageType::Call as u32 {
            return Err(format!("Expected CALL message, got {}", msg_type));
        }

        let rpc_version = dec.decode_u32()?;
        if rpc_version != 2 {
            return Err(format!("Unsupported RPC version: {}", rpc_version));
        }

        let program = dec.decode_u32()?;
        let version = dec.decode_u32()?;
        let procedure = dec.decode_u32()?;

        let cred = Auth::decode(&mut dec)?;
        // RFC 2203 §5.3.1: the call verifier MICs everything up to here.
        // Taken BEFORE the verifier is decoded, on purpose.
        let cred_span = original.slice(0..total.saturating_sub(dec.remaining()));
        let verf = Auth::decode(&mut dec)?;

        Ok(Self {
            xid,
            program,
            version,
            procedure,
            cred,
            verf,
            cred_span,
        })
    }

    /// Decode RPC call and return both the CallMessage and remaining procedure arguments
    pub fn decode_with_args(buf: Bytes) -> Result<(Self, Bytes), String> {
        let original = buf.clone();
        let total = original.len();
        let mut dec = XdrDecoder::new(buf);

        let xid = dec.decode_u32()?;

        let msg_type = dec.decode_u32()?;
        if msg_type != MessageType::Call as u32 {
            return Err(format!("Expected CALL message, got {}", msg_type));
        }

        let rpc_version = dec.decode_u32()?;
        if rpc_version != 2 {
            return Err(format!("Unsupported RPC version: {}", rpc_version));
        }

        let program = dec.decode_u32()?;
        let version = dec.decode_u32()?;
        let procedure = dec.decode_u32()?;

        let cred = Auth::decode(&mut dec)?;
        // RFC 2203 §5.3.1: the call verifier MICs everything up to here.
        // Taken BEFORE the verifier is decoded, on purpose.
        let cred_span = original.slice(0..total.saturating_sub(dec.remaining()));
        let verf = Auth::decode(&mut dec)?;

        let call_msg = Self {
            xid,
            program,
            version,
            procedure,
            cred,
            verf,
            cred_span,
        };

        // Get remaining bytes (procedure arguments)
        let remaining_count = dec.remaining();
        tracing::debug!("After RPC header, {} bytes remaining for procedure args", remaining_count);

        // DEBUG: Print first 40 bytes before extraction
        if remaining_count > 0 {
            let peek_len = remaining_count.min(40);
            tracing::debug!("RPC args peek (first {} bytes): {:02x?}", peek_len, dec.peek_bytes(peek_len));
        }

        let args = dec.into_remaining_bytes();
        tracing::debug!("Extracted args bytes length: {}", args.len());
        tracing::debug!("Args first 40 bytes: {:02x?}", &args[..args.len().min(40)]);

        Ok((call_msg, args))
    }
}

/// RPC reply builder
pub struct ReplyBuilder {
    enc: XdrEncoder,
}

impl ReplyBuilder {
    /// Create a success reply
    pub fn success(xid: u32) -> Self {
        let mut enc = XdrEncoder::new();

        // XID
        enc.encode_u32(xid);

        // Message type: REPLY
        enc.encode_u32(MessageType::Reply as u32);

        // Reply status: ACCEPTED
        enc.encode_u32(ReplyStatus::Accepted as u32);

        // Verifier (null auth)
        Auth::null().encode(&mut enc);

        // Accept status: SUCCESS
        enc.encode_u32(AcceptStatus::Success as u32);

        Self { enc }
    }

    /// Create an error reply
    pub fn error(xid: u32, status: AcceptStatus) -> Bytes {
        let mut enc = XdrEncoder::new();

        // XID
        enc.encode_u32(xid);

        // Message type: REPLY
        enc.encode_u32(MessageType::Reply as u32);

        // Reply status: ACCEPTED
        enc.encode_u32(ReplyStatus::Accepted as u32);

        // Verifier (null auth)
        Auth::null().encode(&mut enc);

        // Accept status
        enc.encode_u32(status as u32);

        enc.finish()
    }

    /// Create program unavailable error
    pub fn prog_unavail(xid: u32) -> Bytes {
        Self::error(xid, AcceptStatus::ProgUnavail)
    }

    /// Create procedure unavailable error
    pub fn proc_unavail(xid: u32) -> Bytes {
        Self::error(xid, AcceptStatus::ProcUnavail)
    }

    /// Create garbage args error
    pub fn garbage_args(xid: u32) -> Bytes {
        Self::error(xid, AcceptStatus::GarbageArgs)
    }

    /// Create system error
    pub fn system_err(xid: u32) -> Bytes {
        Self::error(xid, AcceptStatus::SystemErr)
    }

    /// A MSG_DENIED / AUTH_ERROR reply (RFC 5531 §9).
    ///
    /// Shape differs from every other reply here: a denied message carries
    /// NO verifier and NO accept-status, so it cannot be built by
    /// `error()`.
    pub fn auth_error(xid: u32, stat: AuthStat) -> Bytes {
        let mut enc = XdrEncoder::new();
        enc.encode_u32(xid);
        enc.encode_u32(MessageType::Reply as u32);
        enc.encode_u32(ReplyStatus::Denied as u32);
        enc.encode_u32(1); // reject_stat = AUTH_ERROR (0 would be RPC_MISMATCH)
        enc.encode_u32(stat as u32);
        enc.finish()
    }

    /// A success reply carrying an explicit verifier.
    ///
    /// RFC 2203 §5.3.3.2: an RPCSEC_GSS reply's verifier is a `GSS_GetMIC`
    /// over the request's sequence number, not the null verifier every
    /// other reply here uses. A client checks it, so emitting AUTH_NONE
    /// on a GSS call is a protocol error even when the body is right.
    pub fn success_with_verf(xid: u32, verf: &Auth) -> Self {
        let mut enc = XdrEncoder::new();
        enc.encode_u32(xid);
        enc.encode_u32(MessageType::Reply as u32);
        enc.encode_u32(ReplyStatus::Accepted as u32);
        verf.encode(&mut enc);
        enc.encode_u32(AcceptStatus::Success as u32);
        Self { enc }
    }

    /// Get the encoder to add result data
    pub fn encoder(&mut self) -> &mut XdrEncoder {
        &mut self.enc
    }

    /// Finish building the reply
    pub fn finish(self) -> Bytes {
        self.enc.finish()
    }
}

#[cfg(test)]
mod tests {
    /// RFC 2203 §5.3.1: the call verifier is a GSS_GetMIC over the header
    /// up to and INCLUDING the credential. One octet either way and every
    /// MIC fails, so the boundary is pinned by construction here rather
    /// than by round-tripping our own encoder.
    #[test]
    fn cred_span_ends_at_the_credential_and_excludes_the_verifier() {
        use crate::nfs::xdr::XdrEncoder;

        // 6 fixed u32s, then cred (flavor + 5-octet body padded to 8),
        // then verf, then args.
        let cred_body: &[u8] = &[0xAA, 0xBB, 0xCC, 0xDD, 0xEE];
        let verf_body: &[u8] = &[0x11, 0x22, 0x33, 0x44];

        let mut e = XdrEncoder::new();
        e.encode_u32(0xDEADBEEF); // xid
        e.encode_u32(0); // CALL
        e.encode_u32(2); // rpc version
        e.encode_u32(100003); // program
        e.encode_u32(4); // version
        e.encode_u32(1); // procedure
        e.encode_u32(6); // cred flavor = RPCSEC_GSS
        e.encode_opaque(cred_body);
        let expected_end = 6 * 4 + 4 + 4 + 8; // 5 body octets pad to 8
        e.encode_u32(6); // verf flavor
        e.encode_opaque(verf_body);
        e.encode_u32(0x5A5A5A5A); // one word of args
        let wire = e.finish();

        let (call, args) = CallMessage::decode_with_args(wire.clone()).expect("decode");

        assert_eq!(call.cred_span.len(), expected_end, "span length");
        assert_eq!(&call.cred_span[..], &wire[..expected_end], "span content");
        assert_eq!(call.cred.body.as_ref(), cred_body, "credential body");
        assert_eq!(call.verf.body.as_ref(), verf_body, "verifier body");
        assert_eq!(args.len(), 4, "one word of args survives");

        // The span must NOT reach the verifier: the verifier's flavor word
        // is the very next thing on the wire, so its absence is the check.
        assert!(
            !call.cred_span.ends_with(verf_body),
            "span must stop before the verifier"
        );
        assert_eq!(
            &wire[expected_end..expected_end + 4],
            &6u32.to_be_bytes()[..],
            "the octet after the span is the verifier flavor"
        );

        // `decode` must agree with `decode_with_args`.
        let plain = CallMessage::decode(wire.clone()).expect("decode");
        assert_eq!(plain.cred_span, call.cred_span);
    }

    use super::*;

    #[test]
    fn test_call_decode() {
        let mut enc = XdrEncoder::new();

        // XID
        enc.encode_u32(12345);

        // Message type: CALL
        enc.encode_u32(MessageType::Call as u32);

        // RPC version
        enc.encode_u32(2);

        // Program, version, procedure
        enc.encode_u32(NFS_PROGRAM);
        enc.encode_u32(NFS_VERSION);
        enc.encode_u32(1); // GETATTR

        // Credentials and verifier (null auth)
        Auth::null().encode(&mut enc);
        Auth::null().encode(&mut enc);

        let bytes = enc.finish();
        let call = CallMessage::decode(bytes).unwrap();

        assert_eq!(call.xid, 12345);
        assert_eq!(call.program, NFS_PROGRAM);
        assert_eq!(call.version, NFS_VERSION);
        assert_eq!(call.procedure, 1);
    }

    #[test]
    fn test_reply_success() {
        let reply = ReplyBuilder::success(12345);
        let mut enc = reply.enc;
        enc.encode_u32(42); // Result data
        let bytes = enc.finish();

        let mut dec = XdrDecoder::new(bytes);

        assert_eq!(dec.decode_u32().unwrap(), 12345); // XID
        assert_eq!(dec.decode_u32().unwrap(), MessageType::Reply as u32);
        assert_eq!(dec.decode_u32().unwrap(), ReplyStatus::Accepted as u32);

        // Skip verifier
        Auth::decode(&mut dec).unwrap();

        assert_eq!(dec.decode_u32().unwrap(), AcceptStatus::Success as u32);
        assert_eq!(dec.decode_u32().unwrap(), 42); // Result data
    }
}
