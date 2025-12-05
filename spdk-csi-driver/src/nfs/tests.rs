//! Integration tests for NFSv3 server
//!
//! These tests verify RFC 1813 compliance and proper operation of the NFS server.

use super::*;
use crate::nfs::protocol::FileType;
use std::sync::Arc;
use tempfile::TempDir;

    /// Helper to create a test NFS server
    async fn create_test_server() -> (NfsServer, TempDir, u16) {
        let tmpdir = TempDir::new().unwrap();
        
        // Create a test file
        std::fs::write(tmpdir.path().join("test.txt"), "hello world").unwrap();
        
        let fs = Arc::new(vfs::LocalFilesystem::new(tmpdir.path().to_path_buf()).unwrap());
        
        let config = server::NfsConfig {
            bind_addr: "127.0.0.1".to_string(),
            bind_port: 0, // Let OS pick a port
            volume_id: "test-vol".to_string(),
            export_path: tmpdir.path().to_path_buf(),
        };
        
        let server = NfsServer::new(config, fs).unwrap();
        
        (server, tmpdir, 2049) // Would need to get actual port from server
    }

    #[tokio::test]
    async fn test_xdr_encoding_decoding() {
        use xdr::{XdrEncoder, XdrDecoder};
        
        // Test u32
        let mut enc = XdrEncoder::new();
        enc.encode_u32(42);
        enc.encode_u32(0xDEADBEEF);
        let bytes = enc.finish();
        
        let mut dec = XdrDecoder::new(bytes);
        assert_eq!(dec.decode_u32().unwrap(), 42);
        assert_eq!(dec.decode_u32().unwrap(), 0xDEADBEEF);
        
        // Test string
        let mut enc = XdrEncoder::new();
        enc.encode_string("hello");
        let bytes = enc.finish();
        
        let mut dec = XdrDecoder::new(bytes);
        assert_eq!(dec.decode_string().unwrap(), "hello");
        
        // Test opaque with padding
        for len in 0..8 {
            let mut enc = XdrEncoder::new();
            let data = vec![0xFF; len];
            enc.encode_opaque(&data);
            let bytes = enc.finish();
            
            // Length should be rounded up to 4-byte boundary
            let expected_len = 4 + ((len + 3) & !3);
            assert_eq!(bytes.len(), expected_len, "Failed for length {}", len);
        }
    }

    #[tokio::test]
    async fn test_rpc_message_encoding() {
        use rpc::CallMessage;
        use xdr::XdrEncoder;
        
        // Encode a call message
        let mut enc = XdrEncoder::new();
        enc.encode_u32(12345); // XID
        enc.encode_u32(0); // CALL
        enc.encode_u32(2); // RPC version
        enc.encode_u32(rpc::NFS_PROGRAM);
        enc.encode_u32(rpc::NFS_VERSION);
        enc.encode_u32(1); // GETATTR
        
        // Null auth
        enc.encode_u32(0); // flavor
        enc.encode_opaque(&[]);
        enc.encode_u32(0); // verf flavor
        enc.encode_opaque(&[]);
        
        let bytes = enc.finish();
        
        // Decode and verify
        let call = CallMessage::decode(bytes).unwrap();
        assert_eq!(call.xid, 12345);
        assert_eq!(call.program, rpc::NFS_PROGRAM);
        assert_eq!(call.version, rpc::NFS_VERSION);
        assert_eq!(call.procedure, 1);
    }

    #[tokio::test]
    async fn test_file_handle_cache() {
        use filehandle::HandleCache;
        
        let tmpdir = TempDir::new().unwrap();
        std::fs::write(tmpdir.path().join("test.txt"), "data").unwrap();
        
        let cache = HandleCache::new(tmpdir.path().to_path_buf());
        
        // Get root handle
        let root_fh = cache.root_handle().unwrap();
        let resolved = cache.resolve(&root_fh).unwrap();
        assert_eq!(resolved, tmpdir.path());
        
        // Lookup child
        let child_fh = cache.lookup_child(&root_fh, "test.txt").unwrap();
        let resolved = cache.resolve(&child_fh).unwrap();
        assert_eq!(resolved, tmpdir.path().join("test.txt"));
        
        // Test path traversal protection
        let result = cache.lookup_child(&root_fh, "..");
        assert!(result.is_err(), "Path traversal should be blocked");
    }

    #[tokio::test]
    async fn test_local_filesystem_operations() {
        use vfs::LocalFilesystem;
        
        let tmpdir = TempDir::new().unwrap();
        let fs = LocalFilesystem::new(tmpdir.path().to_path_buf()).unwrap();
        let root_fh = fs.root_handle().unwrap();
        
        // Test CREATE
        let (file_fh, attrs) = fs.create(&root_fh, "newfile.txt", 0o644).await.unwrap();
        assert_eq!(attrs.file_type, FileType::Regular);
        
        // Test WRITE
        let data = b"Hello, NFS!";
        let written = fs.write(&file_fh, 0, data).await.unwrap();
        assert_eq!(written, data.len() as u32);
        
        // Test READ
        let read_data = fs.read(&file_fh, 0, 100).await.unwrap();
        assert_eq!(read_data.as_ref(), data);
        
        // Test GETATTR
        let attrs = fs.getattr(&file_fh).await.unwrap();
        assert_eq!(attrs.size, data.len() as u64);
        
        // Test LOOKUP
        let (lookup_fh, _) = fs.lookup(&root_fh, "newfile.txt").await.unwrap();
        let lookup_data = fs.read(&lookup_fh, 0, 100).await.unwrap();
        assert_eq!(lookup_data.as_ref(), data);
        
        // Test MKDIR
        let (_dir_fh, dir_attrs) = fs.mkdir(&root_fh, "testdir", 0o755).await.unwrap();
        assert_eq!(dir_attrs.file_type, FileType::Directory);
        
        // Test READDIR
        let entries = fs.readdir(&root_fh, 0, 8192).await.unwrap();
        assert_eq!(entries.len(), 2); // newfile.txt + testdir
        
        let names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&"newfile.txt".to_string()));
        assert!(names.contains(&"testdir".to_string()));
        
        // Test REMOVE
        fs.remove(&root_fh, "newfile.txt").await.unwrap();
        let entries = fs.readdir(&root_fh, 0, 8192).await.unwrap();
        assert_eq!(entries.len(), 1);
        
        // Test RMDIR
        fs.rmdir(&root_fh, "testdir").await.unwrap();
        let entries = fs.readdir(&root_fh, 0, 8192).await.unwrap();
        assert_eq!(entries.len(), 0);
    }

    #[tokio::test]
    async fn test_filesystem_stats() {
        use vfs::LocalFilesystem;
        
        let tmpdir = TempDir::new().unwrap();
        let fs = LocalFilesystem::new(tmpdir.path().to_path_buf()).unwrap();
        let root_fh = fs.root_handle().unwrap();
        
        // Test FSSTAT
        let stats = fs.statfs(&root_fh).await.unwrap();
        assert!(stats.tbytes > 0, "Total bytes should be > 0");
        assert!(stats.fbytes > 0, "Free bytes should be > 0");
        assert!(stats.fbytes <= stats.tbytes, "Free bytes should be <= total");
        
        // Test FSINFO
        let info = fs.fsinfo();
        assert_eq!(info.rtmax, 1024 * 1024); // 1 MB
        assert_eq!(info.wtmax, 1024 * 1024); // 1 MB
    }

    #[tokio::test]
    async fn test_concurrent_writes() {
        use vfs::LocalFilesystem;
        
        let tmpdir = TempDir::new().unwrap();
        let fs = Arc::new(LocalFilesystem::new(tmpdir.path().to_path_buf()).unwrap());
        let root_fh = fs.root_handle().unwrap();
        
        // Create a file
        let (file_fh, _) = fs.create(&root_fh, "concurrent.txt", 0o644).await.unwrap();
        
        // Write to different offsets concurrently
        let mut handles = vec![];
        for i in 0..10 {
            let fs_clone = fs.clone();
            let fh_clone = file_fh.clone();
            
            handles.push(tokio::spawn(async move {
                let data = format!("Line {}\n", i).into_bytes();
                let offset = (i * 10) as u64;
                fs_clone.write(&fh_clone, offset, &data).await.unwrap();
            }));
        }
        
        // Wait for all writes
        for handle in handles {
            handle.await.unwrap();
        }
        
        // Read entire file
        let data = fs.read(&file_fh, 0, 1000).await.unwrap();
        
        // Verify all lines are present
        let content = String::from_utf8_lossy(&data);
        for i in 0..10 {
            assert!(content.contains(&format!("Line {}", i)));
        }
    }

    #[tokio::test]
    async fn test_large_file_operations() {
        use vfs::LocalFilesystem;
        
        let tmpdir = TempDir::new().unwrap();
        let fs = LocalFilesystem::new(tmpdir.path().to_path_buf()).unwrap();
        let root_fh = fs.root_handle().unwrap();
        
        let (file_fh, _) = fs.create(&root_fh, "large.txt", 0o644).await.unwrap();
        
        // Write 1 MB
        let chunk_size = 64 * 1024; // 64 KB
        let data = vec![0xAB; chunk_size];
        
        for i in 0..16 {
            let offset = (i * chunk_size) as u64;
            fs.write(&file_fh, offset, &data).await.unwrap();
        }
        
        // Verify file size
        let attrs = fs.getattr(&file_fh).await.unwrap();
        assert_eq!(attrs.size, (16 * chunk_size) as u64);
        
        // Read back and verify
        for i in 0..16 {
            let offset = (i * chunk_size) as u64;
            let read_data = fs.read(&file_fh, offset, chunk_size as u32).await.unwrap();
            assert_eq!(read_data.len(), chunk_size);
            assert!(read_data.iter().all(|&b| b == 0xAB));
        }
    }

    #[tokio::test]
    async fn test_deep_directory_tree() {
        use vfs::LocalFilesystem;
        
        let tmpdir = TempDir::new().unwrap();
        let fs = LocalFilesystem::new(tmpdir.path().to_path_buf()).unwrap();
        let root_fh = fs.root_handle().unwrap();
        
        // Create nested directories: dir1/dir2/dir3
        let (dir1_fh, _) = fs.mkdir(&root_fh, "dir1", 0o755).await.unwrap();
        let (dir2_fh, _) = fs.mkdir(&dir1_fh, "dir2", 0o755).await.unwrap();
        let (dir3_fh, _) = fs.mkdir(&dir2_fh, "dir3", 0o755).await.unwrap();
        
        // Create a file in the deepest directory
        let (file_fh, _) = fs.create(&dir3_fh, "deep.txt", 0o644).await.unwrap();
        fs.write(&file_fh, 0, b"nested file").await.unwrap();
        
        // Navigate and read
        let (d1_fh, _) = fs.lookup(&root_fh, "dir1").await.unwrap();
        let (d2_fh, _) = fs.lookup(&d1_fh, "dir2").await.unwrap();
        let (d3_fh, _) = fs.lookup(&d2_fh, "dir3").await.unwrap();
        let (f_fh, _) = fs.lookup(&d3_fh, "deep.txt").await.unwrap();
        
        let data = fs.read(&f_fh, 0, 100).await.unwrap();
        assert_eq!(data.as_ref(), b"nested file");
    }

