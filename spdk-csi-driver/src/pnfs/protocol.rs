//! pNFS Protocol Types and XDR Encoding/Decoding
//!
//! This module defines pNFS-specific protocol structures and their
//! XDR encoding/decoding, completely isolated from the base NFS implementation.
//!
//! # Protocol References
//! - RFC 8881 Section 3.3 - pNFS Data Types
//! - RFC 8881 Section 12 - Parallel NFS
//! - RFC 8881 Section 13 - NFSv4.1 File Layout Type

use bytes::{Bytes, BytesMut, BufMut, Buf};
use crate::nfs::xdr::{XdrDecoder, XdrEncoder};
use crate::nfs::v4::xdr::{Nfs4XdrDecoder, Nfs4XdrEncoder};
use crate::nfs::v4::protocol::StateId;

/// Layout type constants (RFC 8881 Section 3.3.23, RFC 8435)
pub mod layout_type {
    pub const LAYOUT4_NFSV4_1_FILES: u32 = 1;
    pub const LAYOUT4_BLOCK_VOLUME: u32 = 2;
    pub const LAYOUT4_OSD2_OBJECTS: u32 = 3;
    pub const LAYOUT4_FLEX_FILES: u32 = 4;  // RFC 8435 - Flexible File Layout
}

/// I/O mode constants (RFC 8881 Section 3.3.20)
pub mod iomode {
    pub const LAYOUTIOMODE4_READ: u32 = 1;
    pub const LAYOUTIOMODE4_RW: u32 = 2;
    pub const LAYOUTIOMODE4_ANY: u32 = 3;
}

/// Layout return type constants (RFC 8881 Section 18.44)
pub mod layout_return_type {
    pub const LAYOUTRETURN4_FILE: u32 = 1;
    pub const LAYOUTRETURN4_FSID: u32 = 2;
    pub const LAYOUTRETURN4_ALL: u32 = 3;
}

/// pNFS-specific error codes (RFC 8881 Section 15.1)
pub mod pnfs_error {
    pub const NFS4ERR_LAYOUTUNAVAILABLE: u32 = 10049;
    pub const NFS4ERR_NOMATCHING_LAYOUT: u32 = 10050;
    pub const NFS4ERR_RECALLCONFLICT: u32 = 10051;
    pub const NFS4ERR_UNKNOWN_LAYOUTTYPE: u32 = 10052;
    pub const NFS4ERR_LAYOUTTRYLATER: u32 = 10058;
    pub const NFS4ERR_BADIOMODE: u32 = 10033;
    pub const NFS4ERR_BADLAYOUT: u32 = 10051;
}

// ============================================================================
// LAYOUTGET (opcode 50)
// ============================================================================

/// LAYOUTGET arguments (RFC 8881 Section 18.43.1)
#[derive(Debug, Clone)]
pub struct LayoutGetArgs {
    pub signal_layout_avail: bool,
    pub layout_type: u32,
    pub iomode: u32,
    pub offset: u64,
    pub length: u64,
    pub minlength: u64,
    pub stateid: StateId,
    pub maxcount: u32,
}

/// LAYOUTGET result (RFC 8881 Section 18.43.2)
#[derive(Debug, Clone)]
pub struct LayoutGetResult {
    pub return_on_close: bool,
    pub stateid: StateId,
    pub layouts: Vec<Layout>,
}

/// Layout (RFC 8881 Section 3.3.13)
#[derive(Debug, Clone)]
pub struct Layout {
    pub offset: u64,
    pub length: u64,
    pub iomode: u32,
    pub layout_type: u32,
    pub layout_content: Bytes,  // Encoded layout-specific data
}

impl LayoutGetArgs {
    /// Decode from XDR
    pub fn decode(decoder: &mut XdrDecoder) -> Result<Self, String> {
        Ok(Self {
            signal_layout_avail: decoder.decode_bool()?,
            layout_type: decoder.decode_u32()?,
            iomode: decoder.decode_u32()?,
            offset: decoder.decode_u64()?,
            length: decoder.decode_u64()?,
            minlength: decoder.decode_u64()?,
            stateid: decoder.decode_stateid()?,
            maxcount: decoder.decode_u32()?,
        })
    }
}

impl LayoutGetResult {
    /// Encode to XDR
    pub fn encode(&self, encoder: &mut XdrEncoder) {
        encoder.encode_bool(self.return_on_close);
        encoder.encode_stateid(&self.stateid);
        
        // Encode layouts array
        encoder.encode_u32(self.layouts.len() as u32);
        for layout in &self.layouts {
            encoder.encode_u64(layout.offset);
            encoder.encode_u64(layout.length);
            encoder.encode_u32(layout.iomode);
            encoder.encode_u32(layout.layout_type);
            encoder.encode_opaque(&layout.layout_content);
        }
    }
}

// ============================================================================
// GETDEVICEINFO (opcode 47)
// ============================================================================

/// GETDEVICEINFO arguments (RFC 8881 Section 18.40.1)
#[derive(Debug, Clone)]
pub struct GetDeviceInfoArgs {
    pub device_id: [u8; 16],
    pub layout_type: u32,
    pub maxcount: u32,
    pub notify_types: Vec<u32>,
}

/// GETDEVICEINFO result (RFC 8881 Section 18.40.2)
#[derive(Debug, Clone)]
pub struct GetDeviceInfoResult {
    pub device_addr: DeviceAddr,
    pub notification: Vec<u32>,
}

/// Device address (RFC 8881 Section 3.3.14)
#[derive(Debug, Clone)]
pub struct DeviceAddr {
    pub layout_type: u32,
    pub addr_body: Bytes,  // Encoded netaddr4[] (netid + uaddr)
}

impl GetDeviceInfoArgs {
    /// Decode from XDR
    pub fn decode(decoder: &mut XdrDecoder) -> Result<Self, String> {
        let device_id_bytes = decoder.decode_fixed_opaque(16)?;
        let mut device_id = [0u8; 16];
        device_id.copy_from_slice(&device_id_bytes);
        
        let layout_type = decoder.decode_u32()?;
        let maxcount = decoder.decode_u32()?;
        
        let notify_types_len = decoder.decode_u32()?;
        let mut notify_types = Vec::with_capacity(notify_types_len as usize);
        for _ in 0..notify_types_len {
            notify_types.push(decoder.decode_u32()?);
        }
        
        Ok(Self {
            device_id,
            layout_type,
            maxcount,
            notify_types,
        })
    }
}

impl GetDeviceInfoResult {
    /// Encode to XDR
    pub fn encode(&self, encoder: &mut XdrEncoder) {
        encoder.encode_u32(self.device_addr.layout_type);
        encoder.encode_opaque(&self.device_addr.addr_body);
        
        // Encode notification array
        encoder.encode_u32(self.notification.len() as u32);
        for notify in &self.notification {
            encoder.encode_u32(*notify);
        }
    }
}

// ============================================================================
// LAYOUTRETURN (opcode 51)
// ============================================================================

/// LAYOUTRETURN arguments (RFC 8881 Section 18.44.1)
#[derive(Debug, Clone)]
pub struct LayoutReturnArgs {
    pub reclaim: bool,
    pub layout_type: u32,
    pub iomode: u32,
    pub return_type: u32,
    pub offset: u64,
    pub length: u64,
    pub stateid: StateId,
    pub layout_body: Bytes,
}

impl LayoutReturnArgs {
    /// Decode from XDR
    pub fn decode(decoder: &mut XdrDecoder) -> Result<Self, String> {
        let reclaim = decoder.decode_bool()?;
        let layout_type = decoder.decode_u32()?;
        let iomode = decoder.decode_u32()?;
        let return_type = decoder.decode_u32()?;
        
        // Decode return-type specific data
        let (offset, length, stateid, layout_body) = match return_type {
            layout_return_type::LAYOUTRETURN4_FILE => {
                let offset = decoder.decode_u64()?;
                let length = decoder.decode_u64()?;
                let stateid = decoder.decode_stateid()?;
                let layout_body = decoder.decode_opaque()?;
                (offset, length, stateid, layout_body)
            }
            _ => {
                // LAYOUTRETURN4_FSID or LAYOUTRETURN4_ALL
                let empty_stateid = StateId { seqid: 0, other: [0u8; 12] };
                (0, 0, empty_stateid, Bytes::new())
            }
        };
        
        Ok(Self {
            reclaim,
            layout_type,
            iomode,
            return_type,
            offset,
            length,
            stateid,
            layout_body,
        })
    }
}

// ============================================================================
// LAYOUTCOMMIT (opcode 49)
// ============================================================================

/// LAYOUTCOMMIT arguments (RFC 8881 Section 18.42.1)
#[derive(Debug, Clone)]
pub struct LayoutCommitArgs {
    pub offset: u64,
    pub length: u64,
    pub reclaim: bool,
    pub stateid: StateId,
    pub new_offset: Option<u64>,
    pub new_time: Option<u64>,
    pub layout_body: Bytes,
}

/// LAYOUTCOMMIT result (RFC 8881 Section 18.42.2)
#[derive(Debug, Clone)]
pub struct LayoutCommitResult {
    pub new_size: Option<u64>,
    pub new_time: Option<u64>,
}

impl LayoutCommitArgs {
    /// Decode from XDR
    pub fn decode(decoder: &mut XdrDecoder) -> Result<Self, String> {
        let offset = decoder.decode_u64()?;
        let length = decoder.decode_u64()?;
        let reclaim = decoder.decode_bool()?;
        let stateid = decoder.decode_stateid()?;
        
        let new_offset_present = decoder.decode_bool()?;
        let new_offset = if new_offset_present {
            Some(decoder.decode_u64()?)
        } else {
            None
        };
        
        let new_time_present = decoder.decode_bool()?;
        let new_time = if new_time_present {
            Some(decoder.decode_u64()?)
        } else {
            None
        };
        
        let layout_body = decoder.decode_opaque()?;
        
        Ok(Self {
            offset,
            length,
            reclaim,
            stateid,
            new_offset,
            new_time,
            layout_body,
        })
    }
}

impl LayoutCommitResult {
    /// Encode to XDR
    pub fn encode(&self, encoder: &mut XdrEncoder) {
        encoder.encode_bool(self.new_size.is_some());
        if let Some(size) = self.new_size {
            encoder.encode_u64(size);
        }
        
        encoder.encode_bool(self.new_time.is_some());
        if let Some(time) = self.new_time {
            encoder.encode_u64(time);
        }
    }
}

// ============================================================================
// GETDEVICELIST (opcode 48)
// ============================================================================

/// GETDEVICELIST arguments (RFC 8881 Section 18.41.1)
#[derive(Debug, Clone)]
pub struct GetDeviceListArgs {
    pub layout_type: u32,
    pub maxdevices: u32,
    pub cookie: u64,
    pub cookieverf: [u8; 8],
}

/// GETDEVICELIST result (RFC 8881 Section 18.41.2)
#[derive(Debug, Clone)]
pub struct GetDeviceListResult {
    pub cookie: u64,
    pub cookieverf: [u8; 8],
    pub device_ids: Vec<[u8; 16]>,
    pub eof: bool,
}

impl GetDeviceListArgs {
    /// Decode from XDR
    pub fn decode(decoder: &mut XdrDecoder) -> Result<Self, String> {
        let layout_type = decoder.decode_u32()?;
        let maxdevices = decoder.decode_u32()?;
        let cookie = decoder.decode_u64()?;
        
        let cookieverf_bytes = decoder.decode_fixed_opaque(8)?;
        let mut cookieverf = [0u8; 8];
        cookieverf.copy_from_slice(&cookieverf_bytes);
        
        Ok(Self {
            layout_type,
            maxdevices,
            cookie,
            cookieverf,
        })
    }
}

impl GetDeviceListResult {
    /// Encode to XDR
    pub fn encode(&self, encoder: &mut XdrEncoder) {
        encoder.encode_u64(self.cookie);
        encoder.encode_fixed_opaque(&self.cookieverf);
        
        // Encode device IDs array
        encoder.encode_u32(self.device_ids.len() as u32);
        for device_id in &self.device_ids {
            encoder.encode_fixed_opaque(device_id);
        }
        
        encoder.encode_bool(self.eof);
    }
}

// ============================================================================
// FILE Layout Encoding (RFC 8881 Section 13.2)
// ============================================================================

/// Encode FILE layout content for LAYOUTGET response
/// 
/// RFC 8881 Section 13.2 defines the nfsv4_1_file_layout4 structure
pub fn encode_file_layout(
    device_id: &[u8; 16],
    stripe_unit: u64,
    first_stripe_index: u32,
    pattern_offset: u64,
    filehandles: &[Vec<u8>],
) -> Bytes {
    let mut encoder = XdrEncoder::new();
    
    // deviceid4 (16 bytes fixed)
    encoder.encode_fixed_opaque(device_id);
    
    // nfl_util (stripe unit size)
    encoder.encode_u64(stripe_unit);
    
    // nfl_first_stripe_index
    encoder.encode_u32(first_stripe_index);
    
    // nfl_pattern_offset
    encoder.encode_u64(pattern_offset);
    
    // nfl_fh_list<> (array of filehandles)
    encoder.encode_u32(filehandles.len() as u32);
    for fh in filehandles {
        encoder.encode_opaque(fh);
    }
    
    encoder.finish()
}

/// Encode device address for GETDEVICEINFO response
///
/// RFC 8881 Section 13.2.1 defines nfsv4_1_file_layout_ds_addr4
pub fn encode_device_addr_file_layout(
    netid: &str,
    uaddr: &str,
    multipath_addrs: &[String],
) -> Bytes {
    let mut encoder = XdrEncoder::new();
    
    // Encode as multipath_list4
    // For simplicity, we encode a single netaddr4 for now
    // TODO: Support multiple paths for multipath
    
    encoder.encode_u32(1); // One address for now
    
    // netaddr4: netid + uaddr
    encoder.encode_string(netid);  // e.g., "tcp" or "rdma"
    encoder.encode_string(uaddr);  // e.g., "10.0.1.1.8.1" (IP in XDR format)
    
    encoder.finish()
}

/// Convert IP:port to NFSv4 universal address format
///
/// NFSv4 uses a dotted-decimal format: "h1.h2.h3.h4.p1.p2"
/// where h1-h4 are IP octets and p1.p2 are port high/low bytes
///
/// Example: "10.0.1.1:2049" -> "10.0.1.1.8.1"
pub fn endpoint_to_uaddr(endpoint: &str) -> Result<String, String> {
    let parts: Vec<&str> = endpoint.split(':').collect();
    if parts.len() != 2 {
        return Err(format!("Invalid endpoint format: {}", endpoint));
    }
    
    let ip = parts[0];
    let port: u16 = parts[1].parse()
        .map_err(|_| format!("Invalid port: {}", parts[1]))?;
    
    let port_high = (port >> 8) & 0xFF;
    let port_low = port & 0xFF;
    
    Ok(format!("{}.{}.{}", ip, port_high, port_low))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_endpoint_to_uaddr() {
        assert_eq!(
            endpoint_to_uaddr("10.0.1.1:2049").unwrap(),
            "10.0.1.1.8.1"
        );
        
        assert_eq!(
            endpoint_to_uaddr("192.168.1.100:2049").unwrap(),
            "192.168.1.100.8.1"
        );
    }

    #[test]
    fn test_file_layout_encoding() {
        let device_id = [1u8; 16];
        let filehandles = vec![vec![0x01, 0x02, 0x03]];
        
        let encoded = encode_file_layout(
            &device_id,
            8388608,  // 8 MB stripe
            0,
            0,
            &filehandles,
        );
        
        // Should have encoded data
        assert!(!encoded.is_empty());
    }
}

