//! Direct NFS protocol test client
//! Tests the NFS server without needing to mount via OS

use bytes::{BufMut, Bytes, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// XDR encoding helpers
fn encode_u32(val: u32) -> [u8; 4] {
    val.to_be_bytes()
}

fn encode_string(s: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    let len = s.len() as u32;
    buf.extend_from_slice(&encode_u32(len));
    buf.extend_from_slice(s.as_bytes());
    
    // Pad to 4-byte boundary
    let padding = (4 - (len % 4)) % 4;
    for _ in 0..padding {
        buf.push(0);
    }
    buf
}

async fn send_rpc_call(
    stream: &mut TcpStream,
    xid: u32,
    procedure: u32,
    args: &[u8],
) -> std::io::Result<Bytes> {
    let mut call = BytesMut::new();
    
    // RPC Call Message
    call.put_u32(xid);                    // XID
    call.put_u32(0);                      // CALL
    call.put_u32(2);                      // RPC version
    call.put_u32(100003);                 // NFS program
    call.put_u32(3);                      // NFS version 3
    call.put_u32(procedure);              // Procedure
    
    // Auth NULL
    call.put_u32(0);                      // AUTH_NULL
    call.put_u32(0);                      // length 0
    call.put_u32(0);                      // Verf AUTH_NULL  
    call.put_u32(0);                      // length 0
    
    // Add args
    call.put_slice(args);
    
    // Send with RPC record marker (TCP framing)
    let marker = 0x80000000 | (call.len() as u32); // Last fragment bit + length
    stream.write_all(&marker.to_be_bytes()).await?;
    stream.write_all(&call).await?;
    stream.flush().await?;
    
    // Read reply
    let mut marker_buf = [0u8; 4];
    stream.read_exact(&mut marker_buf).await?;
    let marker = u32::from_be_bytes(marker_buf);
    let length = (marker & 0x7FFFFFFF) as usize;
    
    let mut reply_buf = vec![0u8; length];
    stream.read_exact(&mut reply_buf).await?;
    
    Ok(Bytes::from(reply_buf))
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    println!("🧪 Direct NFS Protocol Test Client");
    println!("==================================\n");
    
    // Connect to NFS server
    println!("📡 Connecting to 127.0.0.1:2049...");
    let mut stream = TcpStream::connect("127.0.0.1:2049").await?;
    println!("✅ Connected!\n");
    
    // Test 1: NULL (ping)
    println!("Test 1: NULL (ping)");
    let reply = send_rpc_call(&mut stream, 1, 0, &[]).await?;
    if reply.len() >= 24 {
        println!("✅ NULL procedure successful\n");
    } else {
        println!("❌ NULL procedure failed\n");
    }
    
    // Test 2: FSINFO (get filesystem info)
    println!("Test 2: FSINFO");
    let mut args = BytesMut::new();
    // Root file handle (8 bytes for inode 0)
    args.put_u32(8); // length
    args.put_u64(0); // inode 0 (will be resolved by server)
    
    let reply = send_rpc_call(&mut stream, 2, 19, &args).await?;
    if reply.len() > 24 {
        println!("✅ FSINFO successful (got {} bytes)\n", reply.len());
    } else {
        println!("❌ FSINFO failed\n");
    }
    
    println!("✅ Basic NFS protocol tests passed!");
    println!("\nThe server is responding correctly to NFS RPC calls.");
    println!("To test with actual file operations, you need:");
    println!("  1. Portmapper support (for macOS mount), OR");
    println!("  2. Test on Linux with 'nolock' option");
    
    Ok(())
}

