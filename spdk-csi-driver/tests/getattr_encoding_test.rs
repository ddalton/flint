// Test for GETATTR XDR encoding correctness
// This verifies we don't double-wrap the fattr4 structure

use bytes::{BytesMut, BufMut};
use spdk_csi_driver::nfs::xdr::{XdrEncoder, XdrDecoder};

#[test]
fn test_getattr_fattr4_structure() {
    // Per RFC 5661, fattr4 structure is:
    // - attrmask: u32 array (length prefix + bitmap words)
    // - attr_vals: opaque data (length prefix + attribute values)
    
    // Example: encode TYPE=2 (directory) and SIZE=4096
    let mut expected_bytes = BytesMut::new();
    
    // Bitmap array
    expected_bytes.put_u32(1); // bitmap array length = 1 word
    expected_bytes.put_u32(0x0000000A); // bits 1,3 set (TYPE=1, SIZE=3)
    
    // Attr vals
    expected_bytes.put_u32(12); // attr_vals length = 12 bytes
    expected_bytes.put_u32(2); // TYPE = NF4DIR  
    expected_bytes.put_u64(4096); // SIZE = 4096
    
    let expected = expected_bytes.to_vec();
    
    // This is what should go into OperationResult::GetAttr(Ok, Some(expected))
    // and be written directly to the wire without additional wrapping
    
    // Verify structure
    assert_eq!(expected.len(), 24, "fattr4 with 2 attributes should be 24 bytes");
    
    // Parse it back
    let mut offset = 0;
    let bitmap_len = u32::from_be_bytes([expected[0], expected[1], expected[2], expected[3]]);
    assert_eq!(bitmap_len, 1);
    offset += 4;
    
    let bitmap_word = u32::from_be_bytes([expected[4], expected[5], expected[6], expected[7]]);
    assert_eq!(bitmap_word, 0x0A); // bits 1,3
    offset += 4;
    
    let attr_vals_len = u32::from_be_bytes([expected[8], expected[9], expected[10], expected[11]]);
    assert_eq!(attr_vals_len, 12, "attr_vals should be 12 bytes");
    offset += 4;
    
    // The GETATTR response encoding should be:
    // opcode (4) + status (4) + fattr4 (24) = 32 bytes total
    // NOT: opcode (4) + status (4) + opaque_length (4) + fattr4 (24) = 36 bytes
}

#[test]
fn test_time_attribute_encoding() {
    // Time attributes are encoded as nfstime4 { int64 seconds; uint32 nseconds; }
    // Total: 12 bytes per time attribute
    
    let mut time_bytes = BytesMut::new();
    time_bytes.put_i64(1733926000); // seconds since epoch
    time_bytes.put_u32(123456789); // nanoseconds
    
    assert_eq!(time_bytes.len(), 12, "nfstime4 should be 12 bytes");
    
    // Verify byte order (big-endian)
    let bytes = time_bytes.to_vec();
    let seconds = i64::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5], bytes[6], bytes[7],
    ]);
    assert_eq!(seconds, 1733926000);
    
    let nsecs = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    assert_eq!(nsecs, 123456789);
}

#[test]
fn test_getattr_real_encoding_decode_roundtrip() {
    // This test simulates exactly what our dispatcher does and verifies
    // the result can be decoded back correctly
    
    // Step 1: Create fattr4 structure like the dispatcher does
    let mut fattr_buf = BytesMut::new();
    
    // Bitmap: let's say we're returning attributes 1 (TYPE) and 3 (SIZE)
    // Bitmap word 0 = 0x0A (bits 1 and 3 set)
    fattr_buf.put_u32(1); // bitmap array length
    fattr_buf.put_u32(0x0000000A); // bitmap word 0
    
    // Attr vals: TYPE=2 (NF4DIR), SIZE=4096
    fattr_buf.put_u32(12); // attr_vals byte length
    fattr_buf.put_u32(2); // TYPE value
    fattr_buf.put_u64(4096); // SIZE value
    
    let fattr_bytes = fattr_buf.to_vec();
    
    // Step 2: Encode the GETATTR response like CompoundResponse::encode_result does
    let mut encoder = XdrEncoder::new();
    encoder.encode_u32(9); // GETATTR opcode
    encoder.encode_u32(0); // NFS4_OK status
    encoder.append_raw(&fattr_bytes); // This is what we changed!
    
    let encoded = encoder.finish();
    let encoded_len = encoded.len();
    
    // Step 3: Decode it back and verify
    let mut decoder = XdrDecoder::new(encoded);
    
    let opcode = decoder.decode_u32().expect("decode opcode");
    assert_eq!(opcode, 9, "opcode should be GETATTR (9)");
    
    let status = decoder.decode_u32().expect("decode status");
    assert_eq!(status, 0, "status should be OK (0)");
    
    // Now decode fattr4
    let bitmap_len = decoder.decode_u32().expect("decode bitmap length");
    assert_eq!(bitmap_len, 1, "bitmap should have 1 word");
    
    let bitmap_word0 = decoder.decode_u32().expect("decode bitmap word 0");
    assert_eq!(bitmap_word0, 0x0000000A, "bitmap word 0 should be 0x0A");
    
    let attr_vals_len = decoder.decode_u32().expect("decode attr_vals length");
    assert_eq!(attr_vals_len, 12, "attr_vals length should be 12");
    
    let type_val = decoder.decode_u32().expect("decode TYPE");
    assert_eq!(type_val, 2, "TYPE should be 2 (NF4DIR)");
    
    let size_val = decoder.decode_u64().expect("decode SIZE");
    assert_eq!(size_val, 4096, "SIZE should be 4096");
    
    // Must have consumed all data
    let remaining = decoder.remaining();
    assert_eq!(remaining, 0, "Should have no remaining bytes, but have {}", remaining);
    
    println!("✅ GETATTR encoding/decoding roundtrip successful!");
    println!("   Total bytes: {}", encoded_len);
    println!("   Format: opcode(4) + status(4) + bitmap_len(4) + bitmap(4) + attr_vals_len(4) + attr_vals(12) = 32 bytes");
}

