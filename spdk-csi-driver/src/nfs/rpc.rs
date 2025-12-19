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
}

impl CallMessage {
    pub fn decode(buf: Bytes) -> Result<Self, String> {
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
        let verf = Auth::decode(&mut dec)?;

        Ok(Self {
            xid,
            program,
            version,
            procedure,
            cred,
            verf,
        })
    }

    /// Decode RPC call and return both the CallMessage and remaining procedure arguments
    pub fn decode_with_args(buf: Bytes) -> Result<(Self, Bytes), String> {
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
        let verf = Auth::decode(&mut dec)?;

        let call_msg = Self {
            xid,
            program,
            version,
            procedure,
            cred,
            verf,
        };

        // Get remaining bytes (procedure arguments)
        let remaining_count = dec.remaining();
        eprintln!("DEBUG: After RPC header, {} bytes remaining for procedure args", remaining_count);

        // DEBUG: Print first 40 bytes before extraction
        if remaining_count > 0 {
            let peek_len = remaining_count.min(40);
            eprintln!("DEBUG: RPC args peek (first {} bytes): {:02x?}", peek_len, dec.peek_bytes(peek_len));
        }

        let args = dec.into_remaining_bytes();
        eprintln!("DEBUG: Extracted args bytes length: {}", args.len());
        eprintln!("DEBUG: Args first 40 bytes: {:02x?}", &args[..args.len().min(40)]);

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
