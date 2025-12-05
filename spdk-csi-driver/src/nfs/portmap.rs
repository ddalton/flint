//! Portmapper Registration
//!
//! Registers NFS and MOUNT services with the portmapper (rpcbind) on port 111.
//! This is required for NFS clients to discover the NFS server.
//!
//! RFC 1833 - Binding Protocols for ONC RPC Version 2

use super::xdr::XdrEncoder;
use bytes::Bytes;
use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

// Portmapper program and procedures
const PMAP_PROGRAM: u32 = 100000;
const PMAP_VERSION: u32 = 2;
const PMAP_PROC_SET: u32 = 1;
const PMAP_PROC_UNSET: u32 = 2;

// Protocol numbers
const IPPROTO_TCP: u32 = 6;
const IPPROTO_UDP: u32 = 17;

// NFS and MOUNT program numbers
const NFS_PROGRAM: u32 = 100003;
const NFS_VERSION: u32 = 3;
const MOUNT_PROGRAM: u32 = 100005;
const MOUNT_VERSION: u32 = 3;

/// Register NFS and MOUNT services with the portmapper
pub async fn register_with_portmapper(port: u16) -> io::Result<()> {
    info!("Registering with portmapper on port 111...");
    
    // Connect to portmapper
    let mut stream = match TcpStream::connect("127.0.0.1:111").await {
        Ok(s) => s,
        Err(e) => {
            warn!("Could not connect to portmapper: {}", e);
            warn!("NFS server will still work if clients use explicit port mounting");
            return Ok(()); // Don't fail - server can still work
        }
    };
    
    // Register NFS v3 TCP
    register_service(&mut stream, NFS_PROGRAM, NFS_VERSION, IPPROTO_TCP, port).await?;
    info!("✅ Registered NFS v3 TCP on port {}", port);
    
    // Register NFS v3 UDP
    register_service(&mut stream, NFS_PROGRAM, NFS_VERSION, IPPROTO_UDP, port).await?;
    info!("✅ Registered NFS v3 UDP on port {}", port);
    
    // Register MOUNT v3 TCP
    register_service(&mut stream, MOUNT_PROGRAM, MOUNT_VERSION, IPPROTO_TCP, port).await?;
    info!("✅ Registered MOUNT v3 TCP on port {}", port);
    
    // Register MOUNT v3 UDP
    register_service(&mut stream, MOUNT_PROGRAM, MOUNT_VERSION, IPPROTO_UDP, port).await?;
    info!("✅ Registered MOUNT v3 UDP on port {}", port);
    
    info!("✅ Portmapper registration complete");
    
    Ok(())
}

/// Unregister services from portmapper
pub async fn unregister_from_portmapper(port: u16) -> io::Result<()> {
    debug!("Unregistering from portmapper...");
    
    let mut stream = match TcpStream::connect("127.0.0.1:111").await {
        Ok(s) => s,
        Err(_) => return Ok(()), // Portmapper not running, nothing to unregister
    };
    
    // Unregister all services
    unregister_service(&mut stream, NFS_PROGRAM, NFS_VERSION, IPPROTO_TCP, port).await?;
    unregister_service(&mut stream, NFS_PROGRAM, NFS_VERSION, IPPROTO_UDP, port).await?;
    unregister_service(&mut stream, MOUNT_PROGRAM, MOUNT_VERSION, IPPROTO_TCP, port).await?;
    unregister_service(&mut stream, MOUNT_PROGRAM, MOUNT_VERSION, IPPROTO_UDP, port).await?;
    
    debug!("Unregistered from portmapper");
    
    Ok(())
}

/// Register a single service with the portmapper
async fn register_service(
    stream: &mut TcpStream,
    program: u32,
    version: u32,
    protocol: u32,
    port: u16,
) -> io::Result<()> {
    let xid = rand::random::<u32>();
    
    let mut enc = XdrEncoder::new();
    
    // RPC Call Header
    enc.encode_u32(xid);                    // XID
    enc.encode_u32(0);                      // CALL
    enc.encode_u32(2);                      // RPC version
    enc.encode_u32(PMAP_PROGRAM);           // Portmapper program
    enc.encode_u32(PMAP_VERSION);           // Portmapper version
    enc.encode_u32(PMAP_PROC_SET);          // PMAP_SET procedure
    
    // Auth NULL
    enc.encode_u32(0);                      // AUTH_NULL
    enc.encode_u32(0);                      // length 0
    enc.encode_u32(0);                      // Verf AUTH_NULL
    enc.encode_u32(0);                      // length 0
    
    // PMAP_SET arguments (struct mapping)
    enc.encode_u32(program);                // prog
    enc.encode_u32(version);                // vers
    enc.encode_u32(protocol);               // prot (6=TCP, 17=UDP)
    enc.encode_u32(port as u32);            // port
    
    let call_bytes = enc.finish();
    
    // Send RPC record marker (TCP framing)
    let marker = 0x80000000 | (call_bytes.len() as u32);
    stream.write_all(&marker.to_be_bytes()).await?;
    stream.write_all(&call_bytes).await?;
    stream.flush().await?;
    
    // Read reply
    let mut marker_buf = [0u8; 4];
    stream.read_exact(&mut marker_buf).await?;
    let reply_marker = u32::from_be_bytes(marker_buf);
    let reply_len = (reply_marker & 0x7FFFFFFF) as usize;
    
    let mut reply_buf = vec![0u8; reply_len];
    stream.read_exact(&mut reply_buf).await?;
    
    // Parse reply to check success
    // For PMAP_SET: success returns boolean TRUE (1)
    if reply_len >= 28 {
        let result = u32::from_be_bytes([
            reply_buf[24],
            reply_buf[25],
            reply_buf[26],
            reply_buf[27],
        ]);
        
        if result != 1 {
            warn!("Portmapper registration returned false for program {}", program);
        }
    }
    
    Ok(())
}

/// Unregister a single service from the portmapper
async fn unregister_service(
    stream: &mut TcpStream,
    program: u32,
    version: u32,
    protocol: u32,
    port: u16,
) -> io::Result<()> {
    let xid = rand::random::<u32>();
    
    let mut enc = XdrEncoder::new();
    
    // RPC Call Header
    enc.encode_u32(xid);
    enc.encode_u32(0);                      // CALL
    enc.encode_u32(2);                      // RPC version
    enc.encode_u32(PMAP_PROGRAM);
    enc.encode_u32(PMAP_VERSION);
    enc.encode_u32(PMAP_PROC_UNSET);        // PMAP_UNSET procedure
    
    // Auth NULL
    enc.encode_u32(0);
    enc.encode_u32(0);
    enc.encode_u32(0);
    enc.encode_u32(0);
    
    // PMAP_UNSET arguments
    enc.encode_u32(program);
    enc.encode_u32(version);
    enc.encode_u32(protocol);
    enc.encode_u32(port as u32);
    
    let call_bytes = enc.finish();
    
    // Send
    let marker = 0x80000000 | (call_bytes.len() as u32);
    stream.write_all(&marker.to_be_bytes()).await?;
    stream.write_all(&call_bytes).await?;
    stream.flush().await?;
    
    // Read reply (but don't care about result on unregister)
    let mut marker_buf = [0u8; 4];
    if stream.read_exact(&mut marker_buf).await.is_ok() {
        let reply_marker = u32::from_be_bytes(marker_buf);
        let reply_len = (reply_marker & 0x7FFFFFFF) as usize;
        let mut reply_buf = vec![0u8; reply_len];
        let _ = stream.read_exact(&mut reply_buf).await;
    }
    
    Ok(())
}

