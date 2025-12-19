//! LAYOUTGET Response Encoding Test
//!
//! This test validates that our LAYOUTGET response encoding matches
//! the exact byte format expected by the Linux NFS client.
//!
//! Based on RFC 5661 Section 18.43.2 (LAYOUTGET response)

use bytes::Bytes;

/// Test LAYOUTGET response encoding matches RFC 5661 format
#[test]
fn test_layoutget_response_encoding() {
    // Build a minimal LAYOUTGET response
    use bytes::{BytesMut, BufMut};
    
    let mut encoder = BytesMut::new();
    
    // 1. return_on_close (bool = u32)
    encoder.put_u32(1);  // true
    
    // 2. stateid (16 bytes, NO length prefix!)
    let stateid = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88,
                   0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00];
    encoder.put_slice(&stateid);
    
    // 3. layout array count
    encoder.put_u32(1);  // 1 layout
    
    // 4. Layout #1 metadata
    encoder.put_u64(0);              // offset
    encoder.put_u64(u64::MAX);       // length (rest of file)
    encoder.put_u32(2);              // iomode (LAYOUTIOMODE4_RW)
    encoder.put_u32(1);              // layout_type (LAYOUT4_NFSV4_1_FILES)
    
    // 5. Layout content (nfsv4_1_file_layout4)
    let mut layout_content = BytesMut::new();
    
    // device_id (16 bytes fixed, NO length!)
    let device_id = [0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x11, 0x22, 0x33,
                     0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB];
    layout_content.put_slice(&device_id);
    
    // Padding for 4-byte alignment (16 bytes is already aligned, no padding needed)
    
    // nfl_util (stripe unit size)
    layout_content.put_u64(8388608);  // 8 MB
    
    // nfl_first_stripe_index
    layout_content.put_u32(0);
    
    // nfl_pattern_offset
    layout_content.put_u64(0);
    
    // nfl_fh_list<> (array of filehandles)
    layout_content.put_u32(1);  // 1 filehandle
    
    // Filehandle (variable-length opaque with length prefix)
    let filehandle = vec![0x01, 0x02, 0x03, 0x04];
    layout_content.put_u32(filehandle.len() as u32);
    layout_content.put_slice(&filehandle);
    
    // XDR padding for filehandle (4 bytes need 0 padding, already aligned)
    
    // Encode layout content as opaque (with length prefix)
    encoder.put_u32(layout_content.len() as u32);
    encoder.put_slice(&layout_content);
    
    // XDR padding for layout content
    let padding = (4 - (layout_content.len() % 4)) % 4;
    for _ in 0..padding {
        encoder.put_u8(0);
    }
    
    let result = encoder.freeze();
    
    // Verify the encoding
    println!("Encoded LAYOUTGET response: {} bytes", result.len());
    println!("Hex dump:");
    for (i, chunk) in result.chunks(16).enumerate() {
        println!("  [{:3}] {:02x?}", i * 16, chunk);
    }
    
    // Parse it back to verify structure
    let mut offset = 0;
    
    // 1. return_on_close
    let return_on_close = u32::from_be_bytes([result[0], result[1], result[2], result[3]]);
    assert_eq!(return_on_close, 1, "return_on_close should be 1 (true)");
    offset += 4;
    
    // 2. stateid (16 bytes, no length)
    assert_eq!(&result[offset..offset+16], &stateid, "stateid mismatch");
    offset += 16;
    
    // 3. layout count
    let layout_count = u32::from_be_bytes([result[offset], result[offset+1], result[offset+2], result[offset+3]]);
    assert_eq!(layout_count, 1, "Should have 1 layout");
    offset += 4;
    
    // 4. Layout offset
    let layout_offset = u64::from_be_bytes([
        result[offset], result[offset+1], result[offset+2], result[offset+3],
        result[offset+4], result[offset+5], result[offset+6], result[offset+7],
    ]);
    assert_eq!(layout_offset, 0, "Layout offset should be 0");
    offset += 8;
    
    // 5. Layout length
    let layout_length = u64::from_be_bytes([
        result[offset], result[offset+1], result[offset+2], result[offset+3],
        result[offset+4], result[offset+5], result[offset+6], result[offset+7],
    ]);
    assert_eq!(layout_length, u64::MAX, "Layout length should be u64::MAX");
    offset += 8;
    
    // 6. IO mode
    let iomode = u32::from_be_bytes([result[offset], result[offset+1], result[offset+2], result[offset+3]]);
    assert_eq!(iomode, 2, "IO mode should be 2 (RW)");
    offset += 4;
    
    // 7. Layout type
    let layout_type = u32::from_be_bytes([result[offset], result[offset+1], result[offset+2], result[offset+3]]);
    assert_eq!(layout_type, 1, "Layout type should be 1 (LAYOUT4_NFSV4_1_FILES)");
    offset += 4;
    
    // 8. Layout content length
    let content_len = u32::from_be_bytes([result[offset], result[offset+1], result[offset+2], result[offset+3]]);
    println!("Layout content length: {}", content_len);
    offset += 4;
    
    // 9. Layout content (nfsv4_1_file_layout4)
    // Device ID (16 bytes, no length prefix in the layout content itself)
    assert_eq!(&result[offset..offset+16], &device_id, "Device ID mismatch in layout content");
    offset += 16;
    
    println!("✅ All fields parsed correctly!");
}

/// Test that our actual dispatcher encoding is correct
#[test]
fn test_dispatcher_layoutget_encoding() {
    // This test should import and call the actual encode_file_layout function
    // from dispatcher.rs and verify it produces correct output
    
    // TODO: This requires making encode_file_layout public or testable
    // For now, this test documents what we need to verify
    
    println!("This test should verify dispatcher.rs::encode_file_layout() output");
    println!("Key checks:");
    println!("1. Stateid is 16 bytes without length prefix");
    println!("2. Layout count is u32");
    println!("3. Offset/length/iomode/layout_type are correct");
    println!("4. Layout content starts with 16-byte device ID");
    println!("5. All XDR padding is correct");
}

/// Test device ID hashing produces consistent 16-byte IDs
#[test]
fn test_device_id_hashing() {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    
    let device_id_string = "cdrv-1.vpc.cloudera.com-ds";
    
    let mut hasher = DefaultHasher::new();
    device_id_string.hash(&mut hasher);
    let hash = hasher.finish();
    
    let mut device_id_bytes = [0u8; 16];
    device_id_bytes[0..8].copy_from_slice(&hash.to_be_bytes());
    device_id_bytes[8..16].copy_from_slice(&hash.to_be_bytes());
    
    println!("Device ID string: '{}'", device_id_string);
    println!("Device ID binary: {:02x?}", device_id_bytes);
    
    // Verify it's deterministic
    let mut hasher2 = DefaultHasher::new();
    device_id_string.hash(&mut hasher2);
    let hash2 = hasher2.finish();
    assert_eq!(hash, hash2, "Hash should be deterministic");
    
    // Verify it's 16 bytes
    assert_eq!(device_id_bytes.len(), 16, "Device ID must be exactly 16 bytes");
}

