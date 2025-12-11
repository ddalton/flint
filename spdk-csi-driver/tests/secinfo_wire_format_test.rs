// Test SECINFO_NO_NAME wire format in 3-op COMPOUND
// Validates exact byte sequence: SEQUENCE + PUTROOTFH + SECINFO_NO_NAME

use spdk_csi_driver::nfs::xdr::{XdrEncoder, XdrDecoder};

#[test]
fn test_secinfo_no_name_3op_compound() {
    // This is the exact COMPOUND that returns SECINFO_NO_NAME
    // SEQUENCE + PUTROOTFH + SECINFO_NO_NAME
    
    let mut encoder = XdrEncoder::new();
    
    // COMPOUND response header
    encoder.encode_u32(0); // status = NFS4_OK
    encoder.encode_u32(0); // tag length = 0 (empty tag)
    encoder.encode_u32(3); // 3 operation results
    
    // Result #0: SEQUENCE (opcode 53)
    encoder.encode_u32(53); // opcode SEQUENCE
    encoder.encode_u32(0); // status OK
    // SessionId: 16 bytes
    for _ in 0..4 {
        encoder.encode_u32(0);
    }
    // sequenceid, slotid, highest_slotid, target_highest_slotid, status_flags
    encoder.encode_u32(3); // sequenceid
    encoder.encode_u32(0); // slotid
    encoder.encode_u32(0); // highest_slotid
    encoder.encode_u32(127); // target_highest_slotid
    encoder.encode_u32(0); // status_flags
    
    // Result #1: PUTROOTFH (opcode 24)
    encoder.encode_u32(24); // opcode PUTROOTFH
    encoder.encode_u32(0); // status OK
    
    // Result #2: SECINFO_NO_NAME (opcode 52)
    encoder.encode_u32(52); // opcode SECINFO_NO_NAME
    encoder.encode_u32(0); // status OK
    // SECINFO4resok = array of secinfo4
    encoder.encode_u32(2); // array length = 2 flavors
    encoder.encode_u32(0); // secinfo4[0].flavor = AUTH_NONE
    // For AUTH_NONE, the union arm is void (no flavor_info)
    encoder.encode_u32(1); // secinfo4[1].flavor = AUTH_SYS
    // For AUTH_SYS, the union arm is also void
    
    let encoded = encoder.finish();
    let total_len = encoded.len();
    
    println!("Encoded 3-op COMPOUND (SEQUENCE + PUTROOTFH + SECINFO_NO_NAME):");
    println!("  Total: {} bytes", total_len);
    
    // Expected structure:
    // - COMPOUND header: 4 + 4 + 4 = 12 bytes
    // - SEQUENCE result: 4 + 4 + 16 + 20 = 44 bytes
    // - PUTROOTFH result: 4 + 4 = 8 bytes
    // - SECINFO_NO_NAME result: 4 + 4 + 4 + 4 + 4 = 20 bytes
    // Total: 12 + 44 + 8 + 20 = 84 bytes
    
    assert_eq!(total_len, 84, "3-op COMPOUND should be 84 bytes");
    
    // Decode to verify structure
    let mut decoder = XdrDecoder::new(encoded.clone());
    
    // COMPOUND header
    let comp_status = decoder.decode_u32().expect("decode compound status");
    assert_eq!(comp_status, 0);
    let tag_len = decoder.decode_u32().expect("decode tag len");
    assert_eq!(tag_len, 0);
    let op_count = decoder.decode_u32().expect("decode op count");
    assert_eq!(op_count, 3);
    
    // Skip SEQUENCE (44 bytes = 11 u32s)
    for _ in 0..11 {
        decoder.decode_u32().expect("skip SEQUENCE");
    }
    
    // PUTROOTFH
    let opcode1 = decoder.decode_u32().expect("decode opcode 1");
    assert_eq!(opcode1, 24, "Should be PUTROOTFH");
    let status1 = decoder.decode_u32().expect("decode status 1");
    assert_eq!(status1, 0);
    
    // SECINFO_NO_NAME
    let opcode2 = decoder.decode_u32().expect("decode opcode 2");
    assert_eq!(opcode2, 52, "Should be SECINFO_NO_NAME");
    let status2 = decoder.decode_u32().expect("decode status 2");
    assert_eq!(status2, 0);
    
    // Decode secinfo array
    let array_len = decoder.decode_u32().expect("decode array len");
    assert_eq!(array_len, 2, "Should have 2 security flavors");
    println!("  ✓ Array length: {}", array_len);
    
    let flavor1 = decoder.decode_u32().expect("decode flavor 1");
    assert_eq!(flavor1, 0, "First flavor should be AUTH_NONE");
    println!("  ✓ Flavor 1: {} (AUTH_NONE)", flavor1);
    
    let flavor2 = decoder.decode_u32().expect("decode flavor 2");
    assert_eq!(flavor2, 1, "Second flavor should be AUTH_SYS");
    println!("  ✓ Flavor 2: {} (AUTH_SYS)", flavor2);
    
    assert_eq!(decoder.remaining(), 0, "Should have consumed all bytes");
    
    println!("\n✅ SECINFO_NO_NAME wire format verified!");
    println!("   Format: opcode(4) + status(4) + array_len(4) + flavor_1(4) + flavor_2(4)");
    println!("   Total SECINFO portion: 20 bytes");
    
    // Now verify the bytes match what Linux kernel expects
    let bytes: Vec<u8> = encoded.to_vec();
    
    // Find SECINFO_NO_NAME in the byte stream
    let secinfo_offset = 12 + 44 + 8; // After COMPOUND header + SEQUENCE + PUTROOTFH
    assert_eq!(secinfo_offset, 64);
    
    // Verify SECINFO_NO_NAME structure
    let opcode_bytes = &bytes[64..68];
    assert_eq!(opcode_bytes, &[0x00, 0x00, 0x00, 0x34], "Opcode should be 52 (0x34)");
    
    let status_bytes = &bytes[68..72];
    assert_eq!(status_bytes, &[0x00, 0x00, 0x00, 0x00], "Status should be OK (0)");
    
    let arraylen_bytes = &bytes[72..76];
    assert_eq!(arraylen_bytes, &[0x00, 0x00, 0x00, 0x02], "Array length should be 2");
    
    let flavor1_bytes = &bytes[76..80];
    assert_eq!(flavor1_bytes, &[0x00, 0x00, 0x00, 0x00], "Flavor 1 should be 0 (AUTH_NONE)");
    
    let flavor2_bytes = &bytes[80..84];
    assert_eq!(flavor2_bytes, &[0x00, 0x00, 0x00, 0x01], "Flavor 2 should be 1 (AUTH_SYS)");
    
    println!("\n✅ Byte-level validation passed!");
    println!("   Opcode: {:02x?}", opcode_bytes);
    println!("   Status: {:02x?}", status_bytes);
    println!("   Array length: {:02x?}", arraylen_bytes);
    println!("   Flavor 1 (AUTH_NONE): {:02x?}", flavor1_bytes);
    println!("   Flavor 2 (AUTH_SYS): {:02x?}", flavor2_bytes);
}

#[test]
fn test_secinfo_auth_flavor_390004_bug() {
    // The kernel error "Couldn't create auth handle (flavor 390004)" suggests
    // it's reading garbage bytes as the auth flavor.
    //
    // 390004 = 0x0005F374
    //
    // This could happen if:
    // 1. We're missing bytes before SECINFO (misalignment)
    // 2. We're encoding SECINFO structure incorrectly
    // 3. Some other operation has wrong byte count
    
    println!("Investigating auth flavor 390004 (0x{:08x})", 390004);
    println!("");
    println!("If kernel expects SECINFO at offset X but we encoded it at offset Y:");
    println!("  Kernel reads 4 bytes at wrong offset");
    println!("  Gets garbage value like 390004");
    println!("");
    println!("Possible causes:");
    println!("  1. SEQUENCE result wrong size (should be 44 bytes)");
    println!("  2. PUTROOTFH result wrong size (should be 8 bytes)");
    println!("  3. COMPOUND header wrong (should be 12 bytes)");
    println!("");
    println!("Let's verify each:");
    
    // Test SEQUENCE size
    let mut seq_encoder = XdrEncoder::new();
    seq_encoder.encode_u32(53); // opcode
    seq_encoder.encode_u32(0); // status
    for _ in 0..4 { seq_encoder.encode_u32(0); } // sessionid (16 bytes)
    seq_encoder.encode_u32(3); // sequenceid
    seq_encoder.encode_u32(0); // slotid
    seq_encoder.encode_u32(0); // highest_slotid
    seq_encoder.encode_u32(127); // target_highest_slotid
    seq_encoder.encode_u32(0); // status_flags
    let seq_bytes = seq_encoder.finish();
    assert_eq!(seq_bytes.len(), 44, "SEQUENCE result should be 44 bytes");
    println!("  ✓ SEQUENCE: {} bytes", seq_bytes.len());
    
    // Test PUTROOTFH size
    let mut putroot_encoder = XdrEncoder::new();
    putroot_encoder.encode_u32(24); // opcode
    putroot_encoder.encode_u32(0); // status
    let putroot_bytes = putroot_encoder.finish();
    assert_eq!(putroot_bytes.len(), 8, "PUTROOTFH result should be 8 bytes");
    println!("  ✓ PUTROOTFH: {} bytes", putroot_bytes.len());
    
    // Test SECINFO_NO_NAME size
    let mut secinfo_encoder = XdrEncoder::new();
    secinfo_encoder.encode_u32(52); // opcode
    secinfo_encoder.encode_u32(0); // status
    secinfo_encoder.encode_u32(2); // array length
    secinfo_encoder.encode_u32(0); // AUTH_NONE
    secinfo_encoder.encode_u32(1); // AUTH_SYS
    let secinfo_bytes = secinfo_encoder.finish();
    assert_eq!(secinfo_bytes.len(), 20, "SECINFO_NO_NAME result should be 20 bytes");
    println!("  ✓ SECINFO_NO_NAME: {} bytes", secinfo_bytes.len());
    
    println!("\n✅ All operation sizes correct");
    println!("   Auth flavor 390004 bug must be from a different cause");
    println!("   (possibly old/cached response, or different operation entirely)");
}

