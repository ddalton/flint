//! pNFS Protocol Types and XDR Encoding/Decoding
//!
//! This module defines pNFS-specific protocol structures and their
//! XDR encoding/decoding, completely isolated from the base NFS implementation.
//!
//! # Protocol References
//! - RFC 8881 Section 3.3 - pNFS Data Types
//! - RFC 8881 Section 12 - Parallel NFS
//! - RFC 8881 Section 13 - NFSv4.1 File Layout Type

use bytes::Bytes;
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
// Flexible File Layout (RFC 8435) Error Reporting and Statistics
// ============================================================================

/// Device error (RFC 8435 Section 4.2)
#[derive(Debug, Clone)]
pub struct DeviceError4 {
    pub device_id: [u8; 16],
    pub status: u32,  // NFS4ERR_* code
    pub opnum: u32,   // NFS operation that failed
}

impl DeviceError4 {
    pub fn new(device_id: [u8; 16], status: u32, opnum: u32) -> Self {
        Self {
            device_id,
            status,
            opnum,
        }
    }

    /// Decode from XDR
    pub fn decode(decoder: &mut XdrDecoder) -> Result<Self, String> {
        let device_id_bytes = decoder.decode_fixed_opaque(16)?;
        let mut device_id = [0u8; 16];
        device_id.copy_from_slice(&device_id_bytes);

        let status = decoder.decode_u32()?;
        let opnum = decoder.decode_u32()?;

        Ok(Self {
            device_id,
            status,
            opnum,
        })
    }

    /// Encode to XDR
    pub fn encode(&self, encoder: &mut XdrEncoder) {
        encoder.encode_fixed_opaque(&self.device_id);
        encoder.encode_u32(self.status);
        encoder.encode_u32(self.opnum);
    }
}

/// I/O error report (RFC 8435 Section 8.2)
#[derive(Debug, Clone)]
pub struct FfIoErr4 {
    pub offset: u64,
    pub length: u64,
    pub stateid: StateId,
    pub errors: Vec<DeviceError4>,
}

impl FfIoErr4 {
    pub fn new(offset: u64, length: u64, stateid: StateId, errors: Vec<DeviceError4>) -> Self {
        Self {
            offset,
            length,
            stateid,
            errors,
        }
    }

    /// Decode from XDR
    pub fn decode(decoder: &mut XdrDecoder) -> Result<Self, String> {
        let offset = decoder.decode_u64()?;
        let length = decoder.decode_u64()?;
        let stateid = decoder.decode_stateid()?;

        let error_count = decoder.decode_u32()?;
        let mut errors = Vec::with_capacity(error_count as usize);
        for _ in 0..error_count {
            errors.push(DeviceError4::decode(decoder)?);
        }

        Ok(Self {
            offset,
            length,
            stateid,
            errors,
        })
    }

    /// Encode to XDR
    pub fn encode(&self, encoder: &mut XdrEncoder) {
        encoder.encode_u64(self.offset);
        encoder.encode_u64(self.length);
        encoder.encode_stateid(&self.stateid);

        encoder.encode_u32(self.errors.len() as u32);
        for error in &self.errors {
            error.encode(encoder);
        }
    }
}

/// I/O information (RFC 7862)
#[derive(Debug, Clone, Default)]
pub struct IoInfo4 {
    pub bytes: u64,
    pub ops: u32,
}

impl IoInfo4 {
    pub fn new(bytes: u64, ops: u32) -> Self {
        Self { bytes, ops }
    }

    /// Decode from XDR
    pub fn decode(decoder: &mut XdrDecoder) -> Result<Self, String> {
        let bytes = decoder.decode_u64()?;
        let ops = decoder.decode_u32()?;
        Ok(Self { bytes, ops })
    }

    /// Encode to XDR
    pub fn encode(&self, encoder: &mut XdrEncoder) {
        encoder.encode_u64(self.bytes);
        encoder.encode_u32(self.ops);
    }
}

/// Layout update (RFC 8435)
#[derive(Debug, Clone)]
pub struct FfLayoutUpdate4 {
    // For now, this is opaque - implementation-specific
    pub data: Bytes,
}

impl FfLayoutUpdate4 {
    /// Decode from XDR
    pub fn decode(decoder: &mut XdrDecoder) -> Result<Self, String> {
        let data = decoder.decode_opaque()?;
        Ok(Self { data })
    }

    /// Encode to XDR
    pub fn encode(&self, encoder: &mut XdrEncoder) {
        encoder.encode_opaque(&self.data);
    }
}

/// I/O statistics (RFC 8435 Section 8.3)
#[derive(Debug, Clone)]
pub struct FfIoStats4 {
    pub offset: u64,
    pub length: u64,
    pub stateid: StateId,
    pub read: IoInfo4,
    pub write: IoInfo4,
    pub device_id: [u8; 16],
    pub layout_update: FfLayoutUpdate4,
}

impl FfIoStats4 {
    pub fn new(
        offset: u64,
        length: u64,
        stateid: StateId,
        read: IoInfo4,
        write: IoInfo4,
        device_id: [u8; 16],
    ) -> Self {
        Self {
            offset,
            length,
            stateid,
            read,
            write,
            device_id,
            layout_update: FfLayoutUpdate4 { data: Bytes::new() },
        }
    }

    /// Decode from XDR
    pub fn decode(decoder: &mut XdrDecoder) -> Result<Self, String> {
        let offset = decoder.decode_u64()?;
        let length = decoder.decode_u64()?;
        let stateid = decoder.decode_stateid()?;
        let read = IoInfo4::decode(decoder)?;
        let write = IoInfo4::decode(decoder)?;

        let device_id_bytes = decoder.decode_fixed_opaque(16)?;
        let mut device_id = [0u8; 16];
        device_id.copy_from_slice(&device_id_bytes);

        let layout_update = FfLayoutUpdate4::decode(decoder)?;

        Ok(Self {
            offset,
            length,
            stateid,
            read,
            write,
            device_id,
            layout_update,
        })
    }

    /// Encode to XDR
    pub fn encode(&self, encoder: &mut XdrEncoder) {
        encoder.encode_u64(self.offset);
        encoder.encode_u64(self.length);
        encoder.encode_stateid(&self.stateid);
        self.read.encode(encoder);
        self.write.encode(encoder);
        encoder.encode_fixed_opaque(&self.device_id);
        self.layout_update.encode(encoder);
    }
}

/// Flexible File Layout return structure (RFC 8435 Section 8)
#[derive(Debug, Clone)]
pub struct FfLayoutReturn4 {
    pub ioerr_report: Vec<FfIoErr4>,
    pub iostats_report: Vec<FfIoStats4>,
}

impl FfLayoutReturn4 {
    pub fn new(ioerr_report: Vec<FfIoErr4>, iostats_report: Vec<FfIoStats4>) -> Self {
        Self {
            ioerr_report,
            iostats_report,
        }
    }

    /// Decode from XDR
    pub fn decode(decoder: &mut XdrDecoder) -> Result<Self, String> {
        // Decode error reports
        let err_count = decoder.decode_u32()?;
        let mut ioerr_report = Vec::with_capacity(err_count as usize);
        for _ in 0..err_count {
            ioerr_report.push(FfIoErr4::decode(decoder)?);
        }

        // Decode stats reports
        let stats_count = decoder.decode_u32()?;
        let mut iostats_report = Vec::with_capacity(stats_count as usize);
        for _ in 0..stats_count {
            iostats_report.push(FfIoStats4::decode(decoder)?);
        }

        Ok(Self {
            ioerr_report,
            iostats_report,
        })
    }

    /// Encode to XDR
    pub fn encode(&self, encoder: &mut XdrEncoder) {
        // Encode error reports
        encoder.encode_u32(self.ioerr_report.len() as u32);
        for err in &self.ioerr_report {
            err.encode(encoder);
        }

        // Encode stats reports
        encoder.encode_u32(self.iostats_report.len() as u32);
        for stats in &self.iostats_report {
            stats.encode(encoder);
        }
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
    use crate::nfs::xdr::{XdrDecoder, XdrEncoder};

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

    #[test]
    fn test_device_error_encode_decode() {
        let device_id = [0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0,
                         0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
        let device_error = DeviceError4::new(
            device_id,
            10001,  // NFS4ERR_IO
            18,     // WRITE opcode
        );

        // Encode
        let mut encoder = XdrEncoder::new();
        device_error.encode(&mut encoder);
        let encoded = encoder.finish();

        // Decode
        let mut decoder = XdrDecoder::new(encoded);
        let decoded = DeviceError4::decode(&mut decoder).unwrap();

        assert_eq!(decoded.device_id, device_id);
        assert_eq!(decoded.status, 10001);
        assert_eq!(decoded.opnum, 18);
    }

    #[test]
    fn test_ff_ioerr4_encode_decode() {
        let stateid = StateId {
            seqid: 1,
            other: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
        };

        let device_id = [0x12u8; 16];
        let errors = vec![
            DeviceError4::new(device_id, 10001, 18),  // NFS4ERR_IO on WRITE
            DeviceError4::new(device_id, 10013, 25),  // NFS4ERR_NOSPC on ALLOCATE
        ];

        let ioerr = FfIoErr4::new(0, 1048576, stateid, errors);

        // Encode
        let mut encoder = XdrEncoder::new();
        ioerr.encode(&mut encoder);
        let encoded = encoder.finish();

        // Decode
        let mut decoder = XdrDecoder::new(encoded);
        let decoded = FfIoErr4::decode(&mut decoder).unwrap();

        assert_eq!(decoded.offset, 0);
        assert_eq!(decoded.length, 1048576);
        assert_eq!(decoded.stateid.seqid, 1);
        assert_eq!(decoded.errors.len(), 2);
        assert_eq!(decoded.errors[0].status, 10001);
        assert_eq!(decoded.errors[1].status, 10013);
    }

    #[test]
    fn test_io_info4_encode_decode() {
        let info = IoInfo4::new(1048576, 100);

        // Encode
        let mut encoder = XdrEncoder::new();
        info.encode(&mut encoder);
        let encoded = encoder.finish();

        // Decode
        let mut decoder = XdrDecoder::new(encoded);
        let decoded = IoInfo4::decode(&mut decoder).unwrap();

        assert_eq!(decoded.bytes, 1048576);
        assert_eq!(decoded.ops, 100);
    }

    #[test]
    fn test_ff_iostats4_encode_decode() {
        let stateid = StateId {
            seqid: 2,
            other: [0u8; 12],
        };

        let read = IoInfo4::new(10485760, 1000);   // 10 MB, 1000 ops
        let write = IoInfo4::new(20971520, 2000);  // 20 MB, 2000 ops
        let device_id = [0xabu8; 16];

        let iostats = FfIoStats4::new(0, 31457280, stateid, read, write, device_id);

        // Encode
        let mut encoder = XdrEncoder::new();
        iostats.encode(&mut encoder);
        let encoded = encoder.finish();

        // Decode
        let mut decoder = XdrDecoder::new(encoded);
        let decoded = FfIoStats4::decode(&mut decoder).unwrap();

        assert_eq!(decoded.offset, 0);
        assert_eq!(decoded.length, 31457280);
        assert_eq!(decoded.read.bytes, 10485760);
        assert_eq!(decoded.read.ops, 1000);
        assert_eq!(decoded.write.bytes, 20971520);
        assert_eq!(decoded.write.ops, 2000);
        assert_eq!(decoded.device_id, device_id);
    }

    #[test]
    fn test_ff_layoutreturn4_encode_decode() {
        let stateid = StateId {
            seqid: 1,
            other: [1u8; 12],
        };

        let device_id = [0x34u8; 16];

        // Create error reports
        let errors = vec![DeviceError4::new(device_id, 10001, 18)];
        let ioerr = FfIoErr4::new(0, 1048576, stateid, errors);

        // Create stats reports
        let read = IoInfo4::new(5242880, 500);
        let write = IoInfo4::new(5242880, 500);
        let iostats = FfIoStats4::new(0, 10485760, stateid, read, write, device_id);

        let ff_return = FfLayoutReturn4::new(vec![ioerr], vec![iostats]);

        // Encode
        let mut encoder = XdrEncoder::new();
        ff_return.encode(&mut encoder);
        let encoded = encoder.finish();

        // Decode
        let mut decoder = XdrDecoder::new(encoded);
        let decoded = FfLayoutReturn4::decode(&mut decoder).unwrap();

        assert_eq!(decoded.ioerr_report.len(), 1);
        assert_eq!(decoded.ioerr_report[0].errors.len(), 1);
        assert_eq!(decoded.ioerr_report[0].errors[0].status, 10001);

        assert_eq!(decoded.iostats_report.len(), 1);
        assert_eq!(decoded.iostats_report[0].read.bytes, 5242880);
        assert_eq!(decoded.iostats_report[0].write.bytes, 5242880);
    }

    #[test]
    fn test_ff_layoutreturn4_empty() {
        // Test with no errors and no stats
        let ff_return = FfLayoutReturn4::new(vec![], vec![]);

        // Encode
        let mut encoder = XdrEncoder::new();
        ff_return.encode(&mut encoder);
        let encoded = encoder.finish();

        // Decode
        let mut decoder = XdrDecoder::new(encoded);
        let decoded = FfLayoutReturn4::decode(&mut decoder).unwrap();

        assert_eq!(decoded.ioerr_report.len(), 0);
        assert_eq!(decoded.iostats_report.len(), 0);
    }

    #[test]
    fn test_multiple_device_errors() {
        // Test with errors from multiple devices
        let stateid = StateId {
            seqid: 5,
            other: [0xffu8; 12],
        };

        let device1 = [0x01u8; 16];
        let device2 = [0x02u8; 16];
        let device3 = [0x03u8; 16];

        let errors = vec![
            DeviceError4::new(device1, 10001, 18),  // Device 1: IO error on WRITE
            DeviceError4::new(device2, 10070, 18),  // Device 2: STALE error on WRITE
            DeviceError4::new(device3, 10013, 25),  // Device 3: NOSPC on ALLOCATE
        ];

        let ioerr = FfIoErr4::new(8388608, 16777216, stateid, errors);

        // Encode/decode
        let mut encoder = XdrEncoder::new();
        ioerr.encode(&mut encoder);
        let encoded = encoder.finish();

        let mut decoder = XdrDecoder::new(encoded);
        let decoded = FfIoErr4::decode(&mut decoder).unwrap();

        assert_eq!(decoded.errors.len(), 3);
        assert_eq!(decoded.errors[0].device_id, device1);
        assert_eq!(decoded.errors[1].device_id, device2);
        assert_eq!(decoded.errors[2].device_id, device3);
        assert_eq!(decoded.offset, 8388608);
        assert_eq!(decoded.length, 16777216);
    }
}

