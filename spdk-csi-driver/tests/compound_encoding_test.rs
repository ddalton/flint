// Test for full COMPOUND response encoding
// This verifies the multi-operation response structure matches RFC 5661

use spdk_csi_driver::nfs::xdr::{XdrEncoder, XdrDecoder};

#[test]
fn test_compound_response_4_operations() {
    // This test simulates: SEQUENCE + PUTROOTFH + GETFH + GETATTR
    // which is the exact sequence that fails in mount
    
    let mut encoder = XdrEncoder::new();
    
    // COMPOUND response header
    encoder.encode_u32(0); // status = NFS4_OK
    encoder.encode_u32(0); // tag length = 0 (empty tag)
    encoder.encode_u32(4); // 4 operation results
    
    // Result #0: SEQUENCE
    encoder.encode_u32(53); // opcode SEQUENCE
    encoder.encode_u32(0); // status OK
    // SessionId (16 bytes)
    for _ in 0..16 {
        encoder.append_raw(&[0u8]);
    }
    encoder.encode_u32(3); // sequenceid
    encoder.encode_u32(0); // slotid
    encoder.encode_u32(0); // highest_slotid
    encoder.encode_u32(127); // target_highest_slotid
    encoder.encode_u32(0); // status_flags
    
    // Result #1: PUTROOTFH
    encoder.encode_u32(24); // opcode PUTROOTFH
    encoder.encode_u32(0); // status OK
    
    // Result #2: GETFH
    encoder.encode_u32(10); // opcode GETFH
    encoder.encode_u32(0); // status OK
    // File handle (opaque)
    let fh_data = vec![1u8; 93]; // 93-byte file handle
    encoder.encode_u32(93); // fh length
    encoder.append_raw(&fh_data);
    // XDR padding for 93 bytes: (4 - 93 % 4) = 3 bytes
    encoder.append_raw(&[0u8, 0u8, 0u8]);
    
    // Result #3: GETATTR
    encoder.encode_u32(9); // opcode GETATTR
    encoder.encode_u32(0); // status OK
    
    // fattr4 structure
    // Bitmap: 2 words
    encoder.encode_u32(2); // bitmap array length
    encoder.encode_u32(0x0010011A); // word 0
    encoder.encode_u32(0x00B0A23A); // word 1
    
    // Attr vals: 116 bytes (from our actual response)
    encoder.encode_u32(116); // attr_vals length
    // Just put placeholder data for now
    for _ in 0..116 {
        encoder.append_raw(&[0u8]);
    }
    
    let encoded = encoder.finish();
    
    // Now try to decode it back
    let mut decoder = XdrDecoder::new(encoded.clone());
    
    // Decode COMPOUND header
    let status = decoder.decode_u32().expect("decode compound status");
    assert_eq!(status, 0, "COMPOUND status should be OK");
    
    let tag_len = decoder.decode_u32().expect("decode tag length");
    assert_eq!(tag_len, 0, "Tag should be empty");
    
    let op_count = decoder.decode_u32().expect("decode op count");
    assert_eq!(op_count, 4, "Should have 4 operations");
    
    // Decode result #0: SEQUENCE
    let opcode0 = decoder.decode_u32().expect("decode opcode 0");
    assert_eq!(opcode0, 53, "First opcode should be SEQUENCE");
    
    let status0 = decoder.decode_u32().expect("decode status 0");
    assert_eq!(status0, 0, "SEQUENCE status should be OK");
    
    // Skip SEQUENCE result data (16 + 4*5 = 36 bytes)
    // SessionId: 16 bytes (4 u32s)
    for _ in 0..4 {
        decoder.decode_u32().expect("skip sessionid");
    }
    // Then 5 more u32s (sequenceid, slotid, highest_slotid, target_highest_slotid, status_flags)
    for _ in 0..5 {
        decoder.decode_u32().expect("skip SEQUENCE fields");
    }
    
    // Decode result #1: PUTROOTFH
    let opcode1 = decoder.decode_u32().expect("decode opcode 1");
    assert_eq!(opcode1, 24, "Second opcode should be PUTROOTFH");
    
    let status1 = decoder.decode_u32().expect("decode status 1");
    assert_eq!(status1, 0, "PUTROOTFH status should be OK");
    
    // Decode result #2: GETFH
    let opcode2 = decoder.decode_u32().expect("decode opcode 2");
    assert_eq!(opcode2, 10, "Third opcode should be GETFH");
    
    let status2 = decoder.decode_u32().expect("decode status 2");
    assert_eq!(status2, 0, "GETFH status should be OK");
    
    let fh_len = decoder.decode_u32().expect("decode fh length");
    assert_eq!(fh_len, 93, "File handle should be 93 bytes");
    
    // Skip file handle + padding  
    // XdrDecoder's decode_opaque handles padding automatically, but since we
    // need to skip it, we'll use decode_u32 for efficiency (93 bytes + 3 padding = 96 bytes = 24 u32s)
    for _ in 0..24 {
        decoder.decode_u32().expect("skip fh data (4 bytes)");
    }
    
    // Decode result #3: GETATTR
    let opcode3 = decoder.decode_u32().expect("decode opcode 3");
    assert_eq!(opcode3, 9, "Fourth opcode should be GETATTR");
    
    let status3 = decoder.decode_u32().expect("decode status 3");
    assert_eq!(status3, 0, "GETATTR status should be OK");
    
    // Decode fattr4
    let bitmap_len = decoder.decode_u32().expect("decode bitmap length");
    assert_eq!(bitmap_len, 2, "Bitmap should have 2 words");
    
    let bitmap0 = decoder.decode_u32().expect("decode bitmap word 0");
    assert_eq!(bitmap0, 0x0010011A, "Bitmap word 0 should match");
    
    let bitmap1 = decoder.decode_u32().expect("decode bitmap word 1");
    assert_eq!(bitmap1, 0x00B0A23A, "Bitmap word 1 should match");
    
    let attr_vals_len = decoder.decode_u32().expect("decode attr_vals length");
    assert_eq!(attr_vals_len, 116, "Attr vals should be 116 bytes");
    
    // Skip attr vals (116 bytes = 29 u32s)
    let u32_count = (attr_vals_len + 3) / 4; // Round up
    for _ in 0..u32_count {
        decoder.decode_u32().expect("skip attr val");
    }
    
    // Should have consumed all data
    let remaining = decoder.remaining();
    assert_eq!(remaining, 0, "Should have no remaining bytes, but have {}", remaining);
    
    println!("✅ Full COMPOUND response (4 ops) encodes/decodes correctly!");
    println!("   Total bytes: {}", encoded.len());
}

#[test]
fn test_getfh_opaque_padding() {
    // Verify file handle padding is correct
    // A 93-byte file handle needs 3 bytes of padding to reach 96 (next 4-byte boundary)
    
    let mut encoder = XdrEncoder::new();
    
    // Encode a file handle as opaque data
    let fh_data = vec![0xAAu8; 93];
    encoder.encode_u32(93); // length
    encoder.append_raw(&fh_data);
    // XDR padding: (4 - (93 % 4)) % 4 = 3 bytes
    encoder.append_raw(&[0u8, 0u8, 0u8]);
    
    let encoded = encoder.finish();
    
    // Should be: 4 (length) + 93 (data) + 3 (padding) = 100 bytes
    assert_eq!(encoded.len(), 100, "93-byte opaque should encode to 100 bytes");
    
    // Verify padding bytes are zero
    let bytes: Vec<u8> = encoded.to_vec();
    assert_eq!(bytes[97], 0, "Padding byte 1 should be 0");
    assert_eq!(bytes[98], 0, "Padding byte 2 should be 0");
    assert_eq!(bytes[99], 0, "Padding byte 3 should be 0");
    
    // Decode it back - first manually read length
    let mut decoder = XdrDecoder::new(encoded.clone());
    let decoded_len = decoder.decode_u32().expect("decode length");
    assert_eq!(decoded_len, 93);
    
    // Now use full decoder to verify opaque decoding works
    let mut decoder2 = XdrDecoder::new(encoded);
    let decoded_bytes = decoder2.decode_opaque().expect("decode opaque");
    let decoded_data: Vec<u8> = decoded_bytes.to_vec();
    assert_eq!(decoded_data, fh_data);
    
    // Padding should be consumed automatically by decode_opaque
    assert_eq!(decoder2.remaining(), 0, "All bytes including padding should be consumed");
}

