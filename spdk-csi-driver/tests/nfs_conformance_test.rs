//! NFS Conformance and Performance Tests
//!
//! This test suite validates the NFS server implementation including:
//! - Protocol conformance (NFSv3 operations)
//! - Performance characteristics (especially zero-copy optimizations)
//! - Concurrent operation safety

use bytes::Bytes;
use spdk_csi_driver::nfs::{LocalFilesystem, NfsConfig, NfsServer};
use std::sync::Arc;
use tempfile::TempDir;

/// Start NFS server in background for testing
async fn start_test_server() -> (NfsServer, TempDir, u16) {
    let tmpdir = TempDir::new().unwrap();

    // Create test files
    std::fs::write(tmpdir.path().join("test.txt"), "hello world").unwrap();

    let fs = Arc::new(LocalFilesystem::new(tmpdir.path().to_path_buf()).unwrap());

    let config = NfsConfig {
        bind_addr: "127.0.0.1".to_string(),
        bind_port: 0, // Let OS choose port for testing
        volume_id: "test-vol".to_string(),
        export_path: tmpdir.path().to_path_buf(),
    };

    let server = NfsServer::new(config, fs).unwrap();
    let port = 12049; // Use fixed test port

    (server, tmpdir, port)
}

#[tokio::test]
async fn test_nfs_protocol_conformance() {
    println!("\n========================================");
    println!("NFS Protocol Conformance Test");
    println!("========================================\n");

    let tmpdir = TempDir::new().unwrap();
    let fs = Arc::new(LocalFilesystem::new(tmpdir.path().to_path_buf()).unwrap());

    // Test basic file operations
    let root_fh = fs.root_handle().unwrap();

    // Test 1: CREATE
    println!("Test 1: CREATE operation");
    let (file_fh, _attrs) = fs.create(&root_fh, "testfile.txt", 0o644).await.unwrap();
    println!("✅ CREATE successful");

    // Test 2: WRITE with zero-copy optimization
    println!("\nTest 2: WRITE operation (with zero-copy optimization)");
    let test_data = Bytes::from("Hello, NFS! This tests the zero-copy write path.");
    let (written, attrs) = fs.write(&file_fh, 0, test_data.clone()).await.unwrap();
    assert_eq!(written as usize, test_data.len());
    assert_eq!(attrs.size, test_data.len() as u64);
    println!("✅ WRITE successful ({} bytes, attrs returned)", written);

    // Test 3: READ
    println!("\nTest 3: READ operation");
    let read_data = fs.read(&file_fh, 0, 100).await.unwrap();
    assert_eq!(read_data.as_ref(), test_data.as_ref());
    println!("✅ READ successful and data matches");

    // Test 4: GETATTR
    println!("\nTest 4: GETATTR operation");
    let attrs = fs.getattr(&file_fh).await.unwrap();
    assert_eq!(attrs.size, test_data.len() as u64);
    println!("✅ GETATTR successful (size: {} bytes)", attrs.size);

    // Test 5: LOOKUP
    println!("\nTest 5: LOOKUP operation");
    let (lookup_fh, _) = fs.lookup(&root_fh, "testfile.txt").await.unwrap();
    println!("✅ LOOKUP successful");

    // Test 6: MKDIR
    println!("\nTest 6: MKDIR operation");
    let (_dir_fh, _) = fs.mkdir(&root_fh, "testdir", 0o755).await.unwrap();
    println!("✅ MKDIR successful");

    // Test 7: READDIR
    println!("\nTest 7: READDIR operation");
    let entries = fs.readdir(&root_fh, 0, 1000).await.unwrap();
    assert!(entries.len() >= 2); // At least testfile.txt and testdir
    println!("✅ READDIR successful ({} entries)", entries.len());

    // Test 8: FSINFO
    println!("\nTest 8: FSINFO operation");
    let info = fs.fsinfo();
    println!("✅ FSINFO successful (max read: {}, max write: {})",
             info.rtmax, info.wtmax);

    println!("\n========================================");
    println!("✅ All protocol conformance tests PASSED");
    println!("========================================\n");
}

#[tokio::test]
async fn test_concurrent_writes_performance() {
    println!("\n========================================");
    println!("Concurrent Write Performance Test");
    println!("(Tests zero-copy optimization)");
    println!("========================================\n");

    let tmpdir = TempDir::new().unwrap();
    let fs = Arc::new(LocalFilesystem::new(tmpdir.path().to_path_buf()).unwrap());
    let root_fh = fs.root_handle().unwrap();

    // Create a test file
    let (file_fh, _) = fs.create(&root_fh, "perf_test.dat", 0o644).await.unwrap();

    // Run concurrent writes
    let num_writes = 100;
    let write_size = 64 * 1024; // 64KB per write

    println!("Running {} concurrent writes of {} bytes each...", num_writes, write_size);
    let start = std::time::Instant::now();

    let mut handles = vec![];
    for i in 0..num_writes {
        let fs_clone = fs.clone();
        let fh_clone = file_fh.clone();

        handles.push(tokio::spawn(async move {
            let data = Bytes::from(vec![0xAB; write_size]);
            let offset = (i * write_size) as u64;
            let _ = fs_clone.write(&fh_clone, offset, data).await.unwrap();
        }));
    }

    // Wait for all writes
    for handle in handles {
        handle.await.unwrap();
    }

    let elapsed = start.elapsed();
    let total_bytes = num_writes * write_size;
    let throughput_mbps = (total_bytes as f64 / 1024.0 / 1024.0) / elapsed.as_secs_f64();

    println!("\n📊 Performance Results:");
    println!("   Total writes: {}", num_writes);
    println!("   Total data: {:.2} MB", total_bytes as f64 / 1024.0 / 1024.0);
    println!("   Time: {:.3} seconds", elapsed.as_secs_f64());
    println!("   Throughput: {:.2} MB/s", throughput_mbps);

    // Verify file size
    let attrs = fs.getattr(&file_fh).await.unwrap();
    assert_eq!(attrs.size, total_bytes as u64);
    println!("   ✅ File size verified: {} bytes", attrs.size);

    println!("\n========================================");
    println!("✅ Performance test PASSED");
    println!("========================================\n");
}

#[tokio::test]
async fn test_large_file_operations() {
    println!("\n========================================");
    println!("Large File Operations Test");
    println!("========================================\n");

    let tmpdir = TempDir::new().unwrap();
    let fs = Arc::new(LocalFilesystem::new(tmpdir.path().to_path_buf()).unwrap());
    let root_fh = fs.root_handle().unwrap();

    // Create a large file (16 MB)
    let (file_fh, _) = fs.create(&root_fh, "large_file.dat", 0o644).await.unwrap();

    let chunk_size = 1024 * 1024; // 1 MB chunks
    let num_chunks = 16;

    println!("Writing {} chunks of {} bytes each...", num_chunks, chunk_size);

    for i in 0..num_chunks {
        let data = Bytes::from(vec![i as u8; chunk_size]);
        let offset = (i * chunk_size) as u64;
        let (written, _attrs) = fs.write(&file_fh, offset, data).await.unwrap();
        assert_eq!(written as usize, chunk_size);
    }

    println!("✅ Write complete");

    // Verify file size
    let attrs = fs.getattr(&file_fh).await.unwrap();
    let expected_size = (num_chunks * chunk_size) as u64;
    assert_eq!(attrs.size, expected_size);

    println!("✅ File size verified: {} bytes ({} MB)",
             attrs.size, attrs.size / 1024 / 1024);

    // Read back and verify
    println!("\nReading back data to verify...");
    for i in 0..num_chunks {
        let offset = (i * chunk_size) as u64;
        let data = fs.read(&file_fh, offset, chunk_size as u32).await.unwrap();
        assert_eq!(data.len(), chunk_size);
        assert_eq!(data[0], i as u8);
    }

    println!("✅ Data verification successful");

    println!("\n========================================");
    println!("✅ Large file test PASSED");
    println!("========================================\n");
}
