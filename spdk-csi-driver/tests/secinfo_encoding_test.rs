// Test SECINFO_NO_NAME encoding per RFC 5661
// Validates secinfo4 union structure

use spdk_csi_driver::nfs::xdr::{XdrEncoder, XdrDecoder};

#[test]
fn test_secinfo_no_name_encoding() {
    // Per RFC 5661, SECINFO_NO_NAME response:
    // typedef secinfo4 SECINFO4resok<>;
    // 
    // Where secinfo4 is:
    // union secinfo4 switch (uint32_t flavor) {
    //   case RPCSEC_GSS:
    //     rpcsec_gss_info flavor_info;
    //   default:
    //     void;
    // };
    //
    // For AUTH_NONE (0) and AUTH_SYS (1), the union arm is void (no data)
    
    let mut encoder = XdrEncoder::new();
    
    // Encode SECINFO_NO_NAME response with AUTH_NONE and AUTH_SYS
    encoder.encode_u32(52); // opcode SECINFO_NO_NAME
    encoder.encode_u32(0); // status OK
    
    // Encode SECINFO4resok (array of secinfo4)
    encoder.encode_u32(2); // array length = 2 flavors
    
    // First secinfo4: AUTH_NONE
    encoder.encode_u32(0); // flavor = AUTH_NONE (discriminant)
    // Union arm is void for AUTH_NONE, so no additional data
    
    // Second secinfo4: AUTH_SYS  
    encoder.encode_u32(1); // flavor = AUTH_SYS (discriminant)
    // Union arm is void for AUTH_SYS, so no additional data
    
    let encoded = encoder.finish();
    
    // Decode it back to verify
    let mut decoder = XdrDecoder::new(encoded.clone());
    
    let opcode = decoder.decode_u32().expect("decode opcode");
    assert_eq!(opcode, 52, "Should be SECINFO_NO_NAME");
    
    let status = decoder.decode_u32().expect("decode status");
    assert_eq!(status, 0, "Should be OK");
    
    let array_len = decoder.decode_u32().expect("decode array length");
    assert_eq!(array_len, 2, "Should have 2 flavors");
    
    let flavor1 = decoder.decode_u32().expect("decode flavor 1");
    assert_eq!(flavor1, 0, "First flavor should be AUTH_NONE");
    
    let flavor2 = decoder.decode_u32().expect("decode flavor 2");
    assert_eq!(flavor2, 1, "Second flavor should be AUTH_SYS");
    
    assert_eq!(decoder.remaining(), 0, "Should have consumed all bytes");
    
    println!("✅ SECINFO_NO_NAME encoding verified:");
    println!("   Total: {} bytes", encoded.len());
    println!("   Format: opcode(4) + status(4) + array_len(4) + flavor1(4) + flavor2(4) = 20 bytes");
    
    assert_eq!(encoded.len(), 20, "SECINFO_NO_NAME response should be 20 bytes");
}

#[test]
fn test_secinfo_no_name_in_compound() {
    // Test SECINFO_NO_NAME as part of a COMPOUND response
    // This is typically: SEQUENCE + PUTROOTFH + SECINFO_NO_NAME
    
    let mut encoder = XdrEncoder::new();
    
    // COMPOUND response header
    encoder.encode_u32(0); // status = NFS4_OK
    encoder.encode_u32(0); // tag length = 0
    encoder.encode_u32(3); // 3 operations
    
    // Result #0: SEQUENCE (opcode 53)
    encoder.encode_u32(53); // opcode
    encoder.encode_u32(0); // status OK
    // SessionId (16 bytes) + other fields (20 bytes) = 36 bytes
    for _ in 0..9 {
        encoder.encode_u32(0);
    }
    
    // Result #1: PUTROOTFH (opcode 24)
    encoder.encode_u32(24); // opcode
    encoder.encode_u32(0); // status OK
    
    // Result #2: SECINFO_NO_NAME (opcode 52)
    encoder.encode_u32(52); // opcode
    encoder.encode_u32(0); // status OK
    encoder.encode_u32(2); // 2 flavors
    encoder.encode_u32(0); // AUTH_NONE
    encoder.encode_u32(1); // AUTH_SYS
    
    let encoded = encoder.finish();
    let encoded_len = encoded.len();
    
    // Decode to verify structure
    let mut decoder = XdrDecoder::new(encoded);
    
    // Skip COMPOUND header and SEQUENCE
    for _ in 0..12 {
        decoder.decode_u32().expect("skip header + sequence");
    }
    
    // Decode PUTROOTFH
    let opcode1 = decoder.decode_u32().expect("decode opcode 1");
    assert_eq!(opcode1, 24);
    let status1 = decoder.decode_u32().expect("decode status 1");
    assert_eq!(status1, 0);
    
    // Decode SECINFO_NO_NAME
    let opcode2 = decoder.decode_u32().expect("decode opcode 2");
    assert_eq!(opcode2, 52);
    let status2 = decoder.decode_u32().expect("decode status 2");
    assert_eq!(status2, 0);
    let array_len = decoder.decode_u32().expect("decode secinfo array len");
    assert_eq!(array_len, 2);
    let flavor1 = decoder.decode_u32().expect("decode flavor 1");
    assert_eq!(flavor1, 0);
    let flavor2 = decoder.decode_u32().expect("decode flavor 2");
    assert_eq!(flavor2, 1);
    
    assert_eq!(decoder.remaining(), 0, "Should have consumed all bytes");
    
    println!("✅ SECINFO_NO_NAME in COMPOUND verified:");
    println!("   Total COMPOUND: {} bytes", encoded_len);
    println!("   SECINFO portion: 20 bytes (opcode + status + array_len + 2 flavors)");
}

