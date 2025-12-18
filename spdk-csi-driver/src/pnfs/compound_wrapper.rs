//! pNFS COMPOUND Wrapper
//!
//! This module provides a zero-overhead wrapper that intercepts pNFS operations
//! while delegating all other operations to the existing NFS dispatcher.
//!
//! # Design Goals
//! - **Zero overhead**: Non-pNFS operations bypass wrapper entirely
//! - **Complete isolation**: No changes to existing NFS code
//! - **Additive only**: pNFS operations are handled separately
//!
//! # Performance
//! 
//! For non-pNFS clients:
//! - Single opcode check (5 pNFS opcodes: 47, 48, 49, 50, 51)
//! - Branch prediction optimized (pNFS opcodes are rare)
//! - Direct delegation to existing dispatcher
//! - Zero memory allocation overhead
//!
//! For pNFS clients:
//! - pNFS operations handled in O(1) time
//! - Metadata operations delegated to existing code
//! - Only data plane optimized (READ/WRITE still go through existing paths)

use crate::nfs::v4::protocol::opcode;
use crate::nfs::v4::protocol::Nfs4Status;
use crate::nfs::v4::xdr::{Nfs4XdrDecoder, Nfs4XdrEncoder};
use crate::nfs::xdr::{XdrDecoder, XdrEncoder};
use crate::pnfs::protocol::*;
use crate::pnfs::context::CompoundContext;
use crate::pnfs::mds::PnfsOperationHandler;
use bytes::Bytes;
use std::sync::Arc;
use tracing::{debug, warn};

/// pNFS COMPOUND operation wrapper
///
/// Intercepts only pNFS opcodes (47, 48, 49, 50, 51) and delegates
/// everything else to the existing NFS dispatcher.
pub struct PnfsCompoundWrapper {
    /// pNFS operation handler (MDS operations)
    pnfs_handler: Arc<PnfsOperationHandler>,
}

impl PnfsCompoundWrapper {
    /// Create a new pNFS compound wrapper
    pub fn new(pnfs_handler: Arc<PnfsOperationHandler>) -> Self {
        Self { pnfs_handler }
    }

    /// Check if an opcode is a pNFS operation
    ///
    /// This is optimized for the common case (non-pNFS operations).
    /// Branch prediction will favor the `false` path.
    #[inline(always)]
    pub fn is_pnfs_opcode(opcode: u32) -> bool {
        matches!(
            opcode,
            opcode::GETDEVICEINFO |   // 47
            opcode::GETDEVICELIST |   // 48
            opcode::LAYOUTCOMMIT |    // 49
            opcode::LAYOUTGET |       // 50
            opcode::LAYOUTRETURN      // 51
        )
    }

    /// Handle a pNFS operation with context
    ///
    /// Called only when `is_pnfs_opcode()` returns true.
    /// Returns (status, encoded_result_data).
    pub fn handle_pnfs_operation(
        &self,
        opcode: u32,
        args: &mut XdrDecoder,
        ctx: &CompoundContext,
    ) -> Result<(Nfs4Status, Bytes), String> {
        debug!("Handling pNFS operation: opcode={}", opcode);

        match opcode {
            opcode::LAYOUTGET => self.handle_layoutget(args, ctx),
            opcode::GETDEVICEINFO => self.handle_getdeviceinfo(args, ctx),
            opcode::LAYOUTRETURN => self.handle_layoutreturn(args, ctx),
            opcode::LAYOUTCOMMIT => self.handle_layoutcommit(args, ctx),
            opcode::GETDEVICELIST => self.handle_getdevicelist(args, ctx),
            _ => {
                // Should never happen (is_pnfs_opcode should have filtered)
                warn!("Invalid pNFS opcode: {}", opcode);
                Ok((Nfs4Status::NotSupp, Bytes::new()))
            }
        }
    }

    /// Handle LAYOUTGET operation
    fn handle_layoutget(
        &self,
        decoder: &mut XdrDecoder,
        ctx: &CompoundContext,
    ) -> Result<(Nfs4Status, Bytes), String> {
        let args = LayoutGetArgs::decode(decoder)?;
        
        // Get current filehandle from context
        let filehandle = ctx.current_fh()
            .ok_or_else(|| "LAYOUTGET requires current filehandle (use PUTFH first)".to_string())?;
        
        // Convert to internal types
        let internal_args = crate::pnfs::mds::operations::LayoutGetArgs {
            signal_layout_avail: args.signal_layout_avail,
            layout_type: match args.layout_type {
                layout_type::LAYOUT4_NFSV4_1_FILES => {
                    crate::pnfs::mds::layout::LayoutType::NfsV4_1Files
                }
                _ => {
                    return Ok((Nfs4Status::NotSupp, Bytes::new()));
                }
            },
            iomode: match args.iomode {
                iomode::LAYOUTIOMODE4_READ => crate::pnfs::mds::layout::IoMode::Read,
                iomode::LAYOUTIOMODE4_RW => crate::pnfs::mds::layout::IoMode::ReadWrite,
                iomode::LAYOUTIOMODE4_ANY => crate::pnfs::mds::layout::IoMode::Any,
                _ => {
                    return Ok((Nfs4Status::BadIoMode, Bytes::new()));
                }
            },
            offset: args.offset,
            length: args.length,
            minlength: args.minlength,
            stateid: args.stateid.to_bytes(),
            maxcount: args.maxcount,
            filehandle: filehandle.data.clone(),
        };

        match self.pnfs_handler.layoutget(internal_args) {
            Ok(result) => {
                // Encode result
                let mut encoder = XdrEncoder::new();
                
                encoder.encode_bool(result.return_on_close);
                encoder.encode_stateid(&args.stateid);  // Return same stateid for now
                
                // Encode layouts
                encoder.encode_u32(result.layouts.len() as u32);
                for layout in result.layouts {
                    encoder.encode_u64(layout.offset);
                    encoder.encode_u64(layout.length);
                    encoder.encode_u32(args.iomode);
                    encoder.encode_u32(args.layout_type);
                    
                    // Encode FILE layout content
                    let layout_content = encode_file_layout_for_segments(&layout.segments);
                    encoder.encode_opaque(&layout_content);
                }
                
                Ok((Nfs4Status::Ok, encoder.finish()))
            }
            Err(e) => {
                warn!("LAYOUTGET failed: {:?}", e);
                Ok((Nfs4Status::LayoutUnavail, Bytes::new()))
            }
        }
    }

    /// Handle GETDEVICEINFO operation
    fn handle_getdeviceinfo(
        &self,
        decoder: &mut XdrDecoder,
        _ctx: &CompoundContext,
    ) -> Result<(Nfs4Status, Bytes), String> {
        let args = GetDeviceInfoArgs::decode(decoder)?;
        
        // Convert to internal types
        let internal_args = crate::pnfs::mds::operations::GetDeviceInfoArgs {
            device_id: args.device_id,
            layout_type: match args.layout_type {
                layout_type::LAYOUT4_NFSV4_1_FILES => {
                    crate::pnfs::mds::layout::LayoutType::NfsV4_1Files
                }
                _ => {
                    return Ok((Nfs4Status::NotSupp, Bytes::new()));
                }
            },
            maxcount: args.maxcount,
            notify_types: args.notify_types,
        };

        match self.pnfs_handler.getdeviceinfo(internal_args) {
            Ok(result) => {
                let mut encoder = XdrEncoder::new();
                
                // Encode device address
                encoder.encode_u32(args.layout_type);
                
                // Encode FILE layout device address
                let addr_body = encode_device_addr_file_layout(
                    &result.device_addr.netid,
                    &result.device_addr.addr,
                    &result.device_addr.multipath,
                );
                encoder.encode_opaque(&addr_body);
                
                // Encode notification (empty for now)
                encoder.encode_u32(0);
                
                Ok((Nfs4Status::Ok, encoder.finish()))
            }
            Err(e) => {
                warn!("GETDEVICEINFO failed: {:?}", e);
                Ok((Nfs4Status::NoEnt, Bytes::new()))
            }
        }
    }

    /// Handle LAYOUTRETURN operation
    fn handle_layoutreturn(
        &self,
        decoder: &mut XdrDecoder,
        _ctx: &CompoundContext,
    ) -> Result<(Nfs4Status, Bytes), String> {
        let args = LayoutReturnArgs::decode(decoder)?;
        
        // Convert to internal types
        let internal_args = crate::pnfs::mds::operations::LayoutReturnArgs {
            reclaim: args.reclaim,
            layout_type: match args.layout_type {
                layout_type::LAYOUT4_NFSV4_1_FILES => {
                    crate::pnfs::mds::layout::LayoutType::NfsV4_1Files
                }
                _ => {
                    return Ok((Nfs4Status::NotSupp, Bytes::new()));
                }
            },
            iomode: match args.iomode {
                iomode::LAYOUTIOMODE4_READ => crate::pnfs::mds::layout::IoMode::Read,
                iomode::LAYOUTIOMODE4_RW => crate::pnfs::mds::layout::IoMode::ReadWrite,
                iomode::LAYOUTIOMODE4_ANY => crate::pnfs::mds::layout::IoMode::Any,
                _ => crate::pnfs::mds::layout::IoMode::Any,
            },
            return_type: match args.return_type {
                layout_return_type::LAYOUTRETURN4_FILE => {
                    crate::pnfs::mds::operations::LayoutReturnType::File {
                        offset: args.offset,
                        length: args.length,
                        stateid: args.stateid.to_bytes(),
                        layout_body: args.layout_body.to_vec(),
                    }
                }
                layout_return_type::LAYOUTRETURN4_FSID => {
                    crate::pnfs::mds::operations::LayoutReturnType::Fsid
                }
                layout_return_type::LAYOUTRETURN4_ALL => {
                    crate::pnfs::mds::operations::LayoutReturnType::All
                }
                _ => crate::pnfs::mds::operations::LayoutReturnType::All,
            },
        };

        match self.pnfs_handler.layoutreturn(internal_args) {
            Ok(_result) => {
                // LAYOUTRETURN has minimal response (just status on success)
                let encoder = XdrEncoder::new();
                Ok((Nfs4Status::Ok, encoder.finish()))
            }
            Err(e) => {
                warn!("LAYOUTRETURN failed: {:?}", e);
                Ok((Nfs4Status::BadStateId, Bytes::new()))
            }
        }
    }

    /// Handle LAYOUTCOMMIT operation
    fn handle_layoutcommit(
        &self,
        decoder: &mut XdrDecoder,
        _ctx: &CompoundContext,
    ) -> Result<(Nfs4Status, Bytes), String> {
        let args = LayoutCommitArgs::decode(decoder)?;
        
        // Convert to internal types
        let internal_args = crate::pnfs::mds::operations::LayoutCommitArgs {
            offset: args.offset,
            length: args.length,
            reclaim: args.reclaim,
            stateid: args.stateid.to_bytes(),
            new_offset: args.new_offset,
            new_time: args.new_time,
            layout_body: args.layout_body.to_vec(),
        };

        match self.pnfs_handler.layoutcommit(internal_args) {
            Ok(result) => {
                let mut encoder = XdrEncoder::new();
                
                encoder.encode_bool(result.new_size.is_some());
                if let Some(size) = result.new_size {
                    encoder.encode_u64(size);
                }
                
                encoder.encode_bool(result.new_time.is_some());
                if let Some(time) = result.new_time {
                    encoder.encode_u64(time);
                }
                
                Ok((Nfs4Status::Ok, encoder.finish()))
            }
            Err(e) => {
                warn!("LAYOUTCOMMIT failed: {:?}", e);
                Ok((Nfs4Status::BadStateId, Bytes::new()))
            }
        }
    }

    /// Handle GETDEVICELIST operation
    fn handle_getdevicelist(
        &self,
        decoder: &mut XdrDecoder,
        _ctx: &CompoundContext,
    ) -> Result<(Nfs4Status, Bytes), String> {
        let args = GetDeviceListArgs::decode(decoder)?;
        
        // Convert to internal types
        let internal_args = crate::pnfs::mds::operations::GetDeviceListArgs {
            layout_type: match args.layout_type {
                layout_type::LAYOUT4_NFSV4_1_FILES => {
                    crate::pnfs::mds::layout::LayoutType::NfsV4_1Files
                }
                _ => {
                    return Ok((Nfs4Status::NotSupp, Bytes::new()));
                }
            },
            maxdevices: args.maxdevices,
            cookie: args.cookie,
            cookieverf: args.cookieverf,
        };

        match self.pnfs_handler.getdevicelist(internal_args) {
            Ok(result) => {
                let mut encoder = XdrEncoder::new();
                
                encoder.encode_u64(result.cookie);
                encoder.encode_fixed_opaque(&result.cookieverf);
                
                // Encode device IDs
                encoder.encode_u32(result.device_ids.len() as u32);
                for device_id in &result.device_ids {
                    encoder.encode_fixed_opaque(device_id);
                }
                
                encoder.encode_bool(result.eof);
                
                Ok((Nfs4Status::Ok, encoder.finish()))
            }
            Err(e) => {
                warn!("GETDEVICELIST failed: {:?}", e);
                Ok((Nfs4Status::NotSupp, Bytes::new()))
            }
        }
    }
}

/// Helper: Encode FILE layout content from layout segments
fn encode_file_layout_for_segments(
    segments: &[crate::pnfs::mds::layout::LayoutSegment],
) -> Bytes {
    // For now, create a simple FILE layout
    // TODO: Properly encode multiple segments
    
    if segments.is_empty() {
        return Bytes::new();
    }
    
    // Use first segment for now
    let segment = &segments[0];
    
    // Generate device ID from string
    let mut device_id = [0u8; 16];
    let device_id_bytes = segment.device_id.as_bytes();
    let copy_len = device_id_bytes.len().min(16);
    device_id[..copy_len].copy_from_slice(&device_id_bytes[..copy_len]);
    
    // Encode FILE layout (RFC 8881 Section 13.2)
    encode_file_layout(
        &device_id,
        8 * 1024 * 1024,  // 8 MB stripe unit
        segment.stripe_index,
        segment.pattern_offset,
        &vec![],  // File handles - TODO: populate from MDS
    )
}

/// Helper trait for StateId conversion
trait StateIdExt {
    fn to_bytes(&self) -> [u8; 16];
}

impl StateIdExt for crate::nfs::v4::protocol::StateId {
    fn to_bytes(&self) -> [u8; 16] {
        let mut bytes = [0u8; 16];
        bytes[0..4].copy_from_slice(&self.seqid.to_be_bytes());
        bytes[4..16].copy_from_slice(&self.other);
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_pnfs_opcode() {
        // pNFS opcodes
        assert!(PnfsCompoundWrapper::is_pnfs_opcode(opcode::LAYOUTGET));
        assert!(PnfsCompoundWrapper::is_pnfs_opcode(opcode::GETDEVICEINFO));
        assert!(PnfsCompoundWrapper::is_pnfs_opcode(opcode::LAYOUTRETURN));
        assert!(PnfsCompoundWrapper::is_pnfs_opcode(opcode::LAYOUTCOMMIT));
        assert!(PnfsCompoundWrapper::is_pnfs_opcode(opcode::GETDEVICELIST));
        
        // Non-pNFS opcodes
        assert!(!PnfsCompoundWrapper::is_pnfs_opcode(opcode::READ));
        assert!(!PnfsCompoundWrapper::is_pnfs_opcode(opcode::WRITE));
        assert!(!PnfsCompoundWrapper::is_pnfs_opcode(opcode::OPEN));
        assert!(!PnfsCompoundWrapper::is_pnfs_opcode(opcode::GETATTR));
    }

    #[test]
    fn test_endpoint_conversion() {
        let uaddr = endpoint_to_uaddr("10.0.1.1:2049").unwrap();
        assert_eq!(uaddr, "10.0.1.1.8.1");
    }
}

