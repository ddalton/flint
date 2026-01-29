//! pNFS Operations
//!
//! Implements pNFS-specific NFS operations as per RFC 8881:
//! - LAYOUTGET (opcode 50) - Get layout information
//! - LAYOUTRETURN (opcode 51) - Return layout to server
//! - LAYOUTCOMMIT (opcode 52) - Commit layout changes
//! - GETDEVICEINFO (opcode 47) - Get device addressing information
//! - GETDEVICELIST (opcode 48) - List all devices
//!
//! # Protocol References
//! - RFC 8881 Section 18.40 - GETDEVICEINFO
//! - RFC 8881 Section 18.41 - GETDEVICELIST
//! - RFC 8881 Section 18.42 - LAYOUTCOMMIT
//! - RFC 8881 Section 18.43 - LAYOUTGET
//! - RFC 8881 Section 18.44 - LAYOUTRETURN
//! - RFC 8881 Chapter 13 - NFSv4.1 File Layout Type

use crate::pnfs::mds::layout::{IoMode, LayoutManager, LayoutSegment, LayoutType};
use crate::pnfs::mds::device::{DeviceId, DeviceRegistry};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// pNFS operation handler
pub struct PnfsOperationHandler {
    layout_manager: Arc<LayoutManager>,
    device_registry: Arc<DeviceRegistry>,
}

impl PnfsOperationHandler {
    /// Create a new pNFS operation handler
    pub fn new(
        layout_manager: Arc<LayoutManager>,
        device_registry: Arc<DeviceRegistry>,
    ) -> Self {
        Self {
            layout_manager,
            device_registry,
        }
    }

    /// Handle LAYOUTGET operation (opcode 50)
    /// 
    /// Returns layout information telling the client which data servers
    /// to use for I/O on a specific byte range.
    pub fn layoutget(
        &self,
        args: LayoutGetArgs,
    ) -> Result<LayoutGetResult, LayoutGetError> {
        warn!("🔥🔥🔥 PnfsOperationHandler::layoutget() CALLED 🔥🔥🔥");
        info!(
            "📥 LAYOUTGET: offset={}, length={}, iomode={:?}, layout_type={:?}",
            args.offset, args.length, args.iomode, args.layout_type
        );

        // Check available devices
        let active_devices = self.device_registry.count_by_status(
            crate::pnfs::mds::device::DeviceStatus::Active
        );
        info!("   Available data servers: {}", active_devices);

        // Validate layout type (support FILE and FFLv4)
        match args.layout_type {
            LayoutType::NfsV4_1Files | LayoutType::FlexFiles => {
                // Supported
            }
            _ => {
                warn!("❌ Unsupported layout type: {:?}", args.layout_type);
                return Err(LayoutGetError::UnknownLayoutType);
            }
        }

        // Generate layout
        let layout = self.layout_manager
            .generate_layout(
                args.filehandle.clone(),
                args.offset,
                args.length,
                args.iomode,
            )
            .map_err(|e| {
                warn!("❌ Layout generation failed: {}", e);
                LayoutGetError::LayoutUnavailable
            })?;

        info!("✅ LAYOUTGET successful: {} segments returned", layout.segments.len());

        Ok(LayoutGetResult {
            return_on_close: layout.return_on_close,
            stateid: layout.stateid,
            layouts: vec![Layout {
                offset: args.offset,
                length: args.length,
                iomode: args.iomode,
                layout_type: args.layout_type,
                segments: layout.segments,
            }],
        })
    }

    /// Handle GETDEVICEINFO operation (opcode 47)
    /// 
    /// Returns network addressing information for a specific data server
    /// device ID.
    pub fn getdeviceinfo(
        &self,
        args: GetDeviceInfoArgs,
    ) -> Result<GetDeviceInfoResult, GetDeviceInfoError> {
        warn!(
            "🔥 GETDEVICEINFO: device_id={:02x?}, layout_type={:?}",
            &args.device_id[0..8],
            args.layout_type
        );

        // Validate layout type
        match args.layout_type {
            LayoutType::NfsV4_1Files | LayoutType::FlexFiles => {
                // Supported
            }
            _ => {
                warn!("❌ Unsupported layout type: {:?}", args.layout_type);
                return Err(GetDeviceInfoError::UnknownLayoutType);
            }
        }

        // Try to look up device as single DS
        let device_addr = if let Some(device_info) = self.device_registry.get_by_binary_id(&args.device_id) {
            // Single DS device found
            warn!("✅ Found single device: id={}, primary_endpoint={}", 
                  device_info.device_id, device_info.primary_endpoint);

            DeviceAddr4 {
                netid: "tcp".to_string(),
                addr: device_info.primary_endpoint.clone(),
                multipath: device_info.endpoints.clone(),
            }
        } else {
            // Not found as single device - could be composite stripe device
            // Get ALL active DSes and return them as stripe pattern
            warn!("🔧 Device not found as single DS - treating as composite stripe device");
            
            let devices = self.device_registry.list_active();
            if devices.is_empty() {
                warn!("❌ No active devices found");
                return Err(GetDeviceInfoError::NoEnt);
            }
            
            warn!("✅ Found {} active DSes for stripe", devices.len());
            
            // Return first DS as primary, rest as multipath (for striping)
            let mut multipath = Vec::new();
            for device in devices.iter().skip(1) {
                multipath.push(device.primary_endpoint.clone());
                warn!("   Stripe DS: {}", device.primary_endpoint);
            }
            
            DeviceAddr4 {
                netid: "tcp".to_string(),
                addr: devices[0].primary_endpoint.clone(),
                multipath,
            }
        };

        warn!("📤 Returning device address with {} total DSes", 
              1 + device_addr.multipath.len());

        Ok(GetDeviceInfoResult {
            device_addr,
            notification: Vec::new(),
        })
    }

    /// Handle LAYOUTRETURN operation (opcode 51)
    ///
    /// Client returns a layout to the server, indicating it no longer
    /// needs it.
    pub fn layoutreturn(
        &self,
        args: LayoutReturnArgs,
    ) -> Result<LayoutReturnResult, LayoutReturnError> {
        debug!(
            "LAYOUTRETURN: layout_type={:?}, iomode={:?}, return_type={:?}",
            args.layout_type, args.iomode, args.return_type
        );

        // Validate layout type (support both FILE and FlexFiles)
        match args.layout_type {
            LayoutType::NfsV4_1Files | LayoutType::FlexFiles => {
                // Supported
            }
            _ => {
                warn!("Unsupported layout type: {:?}", args.layout_type);
                return Err(LayoutReturnError::UnknownLayoutType);
            }
        }

        match args.return_type {
            LayoutReturnType::File { stateid, layout_body, .. } => {
                // Process FFLv4 layout return body if present
                if args.layout_type == LayoutType::FlexFiles && !layout_body.is_empty() {
                    let body_bytes = bytes::Bytes::from(layout_body);
                    self.process_fflv4_layout_return(&body_bytes, &stateid)?;
                }

                // Return specific layout
                self.layout_manager
                    .return_layout(&stateid)
                    .map_err(|e| {
                        warn!("Layout return failed: {}", e);
                        LayoutReturnError::BadStateId
                    })?;
            }
            LayoutReturnType::Fsid => {
                // Return all layouts for filesystem
                info!("LAYOUTRETURN FSID - returning all layouts for filesystem");
                // TODO: Implement filesystem-wide layout return
                // For now, this is a no-op
            }
            LayoutReturnType::All => {
                // Return all layouts
                info!("LAYOUTRETURN ALL - returning all layouts for client");
                // TODO: Implement client-wide layout return
                // For now, this is a no-op
            }
        }

        Ok(LayoutReturnResult {
            new_stateid: None,
        })
    }

    /// Process FFLv4 layout return body (errors and statistics)
    fn process_fflv4_layout_return(
        &self,
        layout_body: &bytes::Bytes,
        stateid: &[u8; 16],
    ) -> Result<(), LayoutReturnError> {
        use crate::nfs::xdr::XdrDecoder;
        use crate::pnfs::protocol::FfLayoutReturn4;

        let mut decoder = XdrDecoder::new(layout_body.clone());
        let ff_return = FfLayoutReturn4::decode(&mut decoder)
            .map_err(|e| {
                warn!("Failed to decode FFLv4 layout return: {}", e);
                LayoutReturnError::Inval
            })?;

        // Process error reports
        if !ff_return.ioerr_report.is_empty() {
            info!(
                "📋 LAYOUTRETURN received {} error reports for layout {:?}",
                ff_return.ioerr_report.len(),
                &stateid[0..4]
            );

            for (i, err_report) in ff_return.ioerr_report.iter().enumerate() {
                info!(
                    "   Error report {}: offset={}, length={}, {} device errors",
                    i, err_report.offset, err_report.length, err_report.errors.len()
                );

                for (j, dev_err) in err_report.errors.iter().enumerate() {
                    warn!(
                        "      Device error {}: device_id={:02x?}, status=0x{:x}, opnum={}",
                        j,
                        &dev_err.device_id[0..4],
                        dev_err.status,
                        dev_err.opnum
                    );

                    // Mark device as degraded if errors are persistent
                    // TODO: Implement error threshold and device health tracking
                    if dev_err.status != 0 {
                        warn!("      ⚠️ Device {:02x?} experienced I/O error - may need recovery",
                              &dev_err.device_id[0..4]);
                    }
                }
            }
        }

        // Process statistics reports
        if !ff_return.iostats_report.is_empty() {
            info!(
                "📊 LAYOUTRETURN received {} statistics reports for layout {:?}",
                ff_return.iostats_report.len(),
                &stateid[0..4]
            );

            for (i, stats) in ff_return.iostats_report.iter().enumerate() {
                info!(
                    "   Stats report {}: offset={}, length={}, device={:02x?}",
                    i, stats.offset, stats.length, &stats.device_id[0..4]
                );
                info!(
                    "      Read: {} bytes, {} ops",
                    stats.read.bytes, stats.read.ops
                );
                info!(
                    "      Write: {} bytes, {} ops",
                    stats.write.bytes, stats.write.ops
                );

                // TODO: Store statistics for performance monitoring and optimization
                // This data can be used to:
                // - Identify hot files/ranges
                // - Optimize layout policies
                // - Detect performance bottlenecks
                // - Trigger data migration
            }
        }

        Ok(())
    }

    /// Handle LAYOUTCOMMIT operation (opcode 52)
    ///
    /// Client commits changes made through a layout (e.g., updates file size
    /// after writes to data servers).
    ///
    /// Per RFC 8435 Section 7, the MDS must ensure data stability before
    /// processing LAYOUTCOMMIT and updating metadata.
    pub fn layoutcommit(
        &self,
        args: LayoutCommitArgs,
    ) -> Result<LayoutCommitResult, LayoutCommitError> {
        info!(
            "📝 LAYOUTCOMMIT: offset={}, length={}, stateid={:?}",
            args.offset,
            args.length,
            &args.stateid[0..4]
        );

        // Verify layout exists
        let layout = self.layout_manager
            .get_layout(&args.stateid)
            .ok_or_else(|| {
                warn!("Layout not found for commit: {:?}", &args.stateid[0..4]);
                LayoutCommitError::BadStateId
            })?;

        // Extract file information from layout
        info!(
            "   Layout has {} segments for filehandle length={}",
            layout.segments.len(),
            layout.filehandle.len()
        );

        // Update file metadata if new offset is provided
        let new_size = if let Some(new_offset) = args.new_offset {
            info!("   Updating file size to {} bytes", new_offset);

            // Try to update file size via filehandle
            if let Err(e) = self.update_file_size(&layout.filehandle, new_offset) {
                warn!("   Failed to update file size: {}", e);
                // Don't fail the operation - the metadata update is best-effort
            }

            Some(new_offset)
        } else {
            info!("   No size update requested");
            None
        };

        // Update modification time
        let new_time = if args.new_time.is_some() {
            args.new_time
        } else {
            // Use current time if not specified
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_nanos() as u64);

            if let Some(time) = now {
                info!("   Setting mtime to current time: {}", time);
                if let Err(e) = self.update_file_mtime(&layout.filehandle, time) {
                    warn!("   Failed to update mtime: {}", e);
                }
            }

            now
        };

        info!("   ✅ LAYOUTCOMMIT completed successfully");

        Ok(LayoutCommitResult {
            new_size,
            new_time,
        })
    }

    /// Update file size based on filehandle
    fn update_file_size(&self, filehandle: &[u8], new_size: u64) -> Result<(), String> {
        use std::fs;
        
        use crate::nfs::v4::filehandle_pnfs;

        // Parse filehandle to get file path
        let path = if filehandle.len() >= 21 && filehandle[0] == 2 {
            // pNFS filehandle - extract file_id
            let fh = crate::nfs::v4::protocol::Nfs4FileHandle {
                data: filehandle.to_vec(),
            };

            match filehandle_pnfs::parse_pnfs_filehandle(&fh) {
                Ok((_, file_id, stripe_index)) => {
                    // For MDS, we need to map file_id back to original file
                    // Since we don't have a persistent mapping yet, use a simple approach
                    // TODO: Implement persistent file_id -> path mapping
                    let base_path = std::path::Path::new("/data");
                    base_path.join(format!("{:016x}", file_id))
                }
                Err(e) => {
                    return Err(format!("Failed to parse pNFS filehandle: {}", e));
                }
            }
        } else {
            // Traditional filehandle - we can't easily extract path
            // TODO: Implement filehandle -> path mapping
            return Err("Traditional filehandle path resolution not implemented".to_string());
        };

        // Truncate or extend file to new size
        match fs::OpenOptions::new().write(true).open(&path) {
            Ok(file) => {
                if let Err(e) = file.set_len(new_size) {
                    return Err(format!("Failed to set file size: {}", e));
                }
                Ok(())
            }
            Err(e) => {
                // File might not exist on MDS if it's on DS
                Err(format!("File not found on MDS: {}", e))
            }
        }
    }

    /// Update file modification time
    fn update_file_mtime(&self, filehandle: &[u8], mtime_nanos: u64) -> Result<(), String> {
        
        use filetime::{FileTime, set_file_mtime};
        use crate::nfs::v4::filehandle_pnfs;

        // Parse filehandle to get file path (same logic as update_file_size)
        let path = if filehandle.len() >= 21 && filehandle[0] == 2 {
            let fh = crate::nfs::v4::protocol::Nfs4FileHandle {
                data: filehandle.to_vec(),
            };

            match filehandle_pnfs::parse_pnfs_filehandle(&fh) {
                Ok((_, file_id, _)) => {
                    let base_path = std::path::Path::new("/data");
                    base_path.join(format!("{:016x}", file_id))
                }
                Err(e) => {
                    return Err(format!("Failed to parse pNFS filehandle: {}", e));
                }
            }
        } else {
            return Err("Traditional filehandle path resolution not implemented".to_string());
        };

        // Convert nanos to seconds for filetime
        let secs = (mtime_nanos / 1_000_000_000) as i64;
        let nsecs = (mtime_nanos % 1_000_000_000) as u32;
        let mtime = FileTime::from_unix_time(secs, nsecs);

        set_file_mtime(&path, mtime)
            .map_err(|e| format!("Failed to set mtime: {}", e))
    }

    /// Handle GETDEVICELIST operation (opcode 48)
    /// 
    /// Returns a list of all available device IDs.
    pub fn getdevicelist(
        &self,
        args: GetDeviceListArgs,
    ) -> Result<GetDeviceListResult, GetDeviceListError> {
        debug!(
            "GETDEVICELIST: layout_type={:?}, maxdevices={}",
            args.layout_type, args.maxdevices
        );

        // Validate layout type
        if args.layout_type != LayoutType::NfsV4_1Files {
            warn!("Unsupported layout type: {:?}", args.layout_type);
            return Err(GetDeviceListError::UnknownLayoutType);
        }

        // Get all active devices
        let devices = self.device_registry.list_active();
        let device_ids: Vec<DeviceId> = devices
            .iter()
            .take(args.maxdevices as usize)
            .map(|d| d.binary_device_id)
            .collect();

        Ok(GetDeviceListResult {
            cookie: 0,
            cookieverf: [0u8; 8],
            device_ids,
            eof: true,
        })
    }
}

// ============================================================================
// Operation Arguments and Results
// ============================================================================

/// LAYOUTGET arguments (RFC 8881 Section 18.43.1)
#[derive(Debug, Clone)]
pub struct LayoutGetArgs {
    pub signal_layout_avail: bool,
    pub layout_type: LayoutType,
    pub iomode: IoMode,
    pub offset: u64,
    pub length: u64,
    pub minlength: u64,
    pub stateid: [u8; 16],
    pub maxcount: u32,
    pub filehandle: Vec<u8>,
}

/// LAYOUTGET result (RFC 8881 Section 18.43.2)
#[derive(Debug, Clone)]
pub struct LayoutGetResult {
    pub return_on_close: bool,
    pub stateid: [u8; 16],
    pub layouts: Vec<Layout>,
}

/// Layout structure
#[derive(Debug, Clone)]
pub struct Layout {
    pub offset: u64,
    pub length: u64,
    pub iomode: IoMode,
    pub layout_type: LayoutType,
    pub segments: Vec<LayoutSegment>,
}

/// LAYOUTGET errors
#[derive(Debug, Clone, Copy)]
pub enum LayoutGetError {
    LayoutUnavailable,
    UnknownLayoutType,
    BadStateId,
    Io,
}

/// GETDEVICEINFO arguments (RFC 8881 Section 18.40.1)
#[derive(Debug, Clone)]
pub struct GetDeviceInfoArgs {
    pub device_id: DeviceId,
    pub layout_type: LayoutType,
    pub maxcount: u32,
    pub notify_types: Vec<u32>,
}

/// GETDEVICEINFO result (RFC 8881 Section 18.40.2)
#[derive(Debug, Clone)]
pub struct GetDeviceInfoResult {
    pub device_addr: DeviceAddr4,
    pub notification: Vec<u32>,
}

/// Device address structure (RFC 8881 Section 3.3.14)
#[derive(Debug, Clone)]
pub struct DeviceAddr4 {
    pub netid: String,
    pub addr: String,
    pub multipath: Vec<String>,
}

/// GETDEVICEINFO errors
#[derive(Debug, Clone, Copy)]
pub enum GetDeviceInfoError {
    NoEnt,
    UnknownLayoutType,
    TooSmall,
}

/// LAYOUTRETURN arguments (RFC 8881 Section 18.44.1)
#[derive(Debug, Clone)]
pub struct LayoutReturnArgs {
    pub reclaim: bool,
    pub layout_type: LayoutType,
    pub iomode: IoMode,
    pub return_type: LayoutReturnType,
}

/// Layout return type
#[derive(Debug, Clone)]
pub enum LayoutReturnType {
    File {
        offset: u64,
        length: u64,
        stateid: [u8; 16],
        layout_body: Vec<u8>,
    },
    Fsid,
    All,
}

/// LAYOUTRETURN result (RFC 8881 Section 18.44.2)
#[derive(Debug, Clone)]
pub struct LayoutReturnResult {
    pub new_stateid: Option<[u8; 16]>,
}

/// LAYOUTRETURN errors
#[derive(Debug, Clone, Copy)]
pub enum LayoutReturnError {
    BadStateId,
    UnknownLayoutType,
    Inval,
}

/// LAYOUTCOMMIT arguments (RFC 8881 Section 18.42.1)
#[derive(Debug, Clone)]
pub struct LayoutCommitArgs {
    pub offset: u64,
    pub length: u64,
    pub reclaim: bool,
    pub stateid: [u8; 16],
    pub new_offset: Option<u64>,
    pub new_time: Option<u64>,
    pub layout_body: Vec<u8>,
}

/// LAYOUTCOMMIT result (RFC 8881 Section 18.42.2)
#[derive(Debug, Clone)]
pub struct LayoutCommitResult {
    pub new_size: Option<u64>,
    pub new_time: Option<u64>,
}

/// LAYOUTCOMMIT errors
#[derive(Debug, Clone, Copy)]
pub enum LayoutCommitError {
    BadStateId,
    Inval,
    Io,
}

/// GETDEVICELIST arguments (RFC 8881 Section 18.41.1)
#[derive(Debug, Clone)]
pub struct GetDeviceListArgs {
    pub layout_type: LayoutType,
    pub maxdevices: u32,
    pub cookie: u64,
    pub cookieverf: [u8; 8],
}

/// GETDEVICELIST result (RFC 8881 Section 18.41.2)
#[derive(Debug, Clone)]
pub struct GetDeviceListResult {
    pub cookie: u64,
    pub cookieverf: [u8; 8],
    pub device_ids: Vec<DeviceId>,
    pub eof: bool,
}

/// GETDEVICELIST errors
#[derive(Debug, Clone, Copy)]
pub enum GetDeviceListError {
    UnknownLayoutType,
    TooSmall,
}



// Implement PnfsOperations trait for PnfsOperationHandler
impl crate::pnfs::PnfsOperations for PnfsOperationHandler {
    fn layoutget(&self, args: LayoutGetArgs) -> Result<LayoutGetResult, LayoutGetError> {
        self.layoutget(args)
    }
    
    fn getdeviceinfo(&self, args: GetDeviceInfoArgs) -> Result<GetDeviceInfoResult, GetDeviceInfoError> {
        self.getdeviceinfo(args)
    }
    
    fn layoutreturn(&self, args: LayoutReturnArgs) -> Result<(), String> {
        self.layoutreturn(args).map(|_| ()).map_err(|e| format!("{:?}", e))
    }
}

