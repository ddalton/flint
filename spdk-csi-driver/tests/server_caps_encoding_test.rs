// Test for decode_server_caps GETATTR encoding
// Validates attributes {0, 2, 5, 6, 13, 16, 17, 75} per RFC 5661

#[test]
fn test_server_caps_attribute_sizes() {
    // Server caps GETATTR requests bitmap [204901, 0, 2048]
    // Which is attributes: {0, 2, 5, 6, 13, 16, 17, 75}
    
    // Per RFC 5661, these attributes have the following types and sizes:
    println!("Server caps attributes per RFC 5661:");
    println!("====================================");
    
    let attrs = vec![
        (0, "SUPPORTED_ATTRS", "bitmap4", "variable (array of u32)"),
        (2, "FH_EXPIRE_TYPE", "uint32_t", "4 bytes"),
        (5, "LINK_SUPPORT", "bool", "4 bytes"),
        (6, "SYMLINK_SUPPORT", "bool", "4 bytes"),
        (13, "ACLSUPPORT", "uint32_t", "4 bytes"),
        (16, "CASE_INSENSITIVE", "bool", "4 bytes"),
        (17, "CASE_PRESERVING", "bool", "4 bytes"),
        (75, "SUPPATTR_EXCLCREAT", "bitmap4", "variable (array of u32)"),
    ];
    
    for (id, name, typ, size) in &attrs {
        println!("  Attr {:2}: {:<25} {} ({})", id, name, typ, size);
    }
    
    // From our server logs, we encode:
    // - Attr 0: 8 bytes (SUPPORTED_ATTRS: bitmap4 with 2 u32 words = 4 + 4)
    // - Attr 2: 4 bytes (FH_EXPIRE_TYPE: u32)
    // - Attr 5: 4 bytes (LINK_SUPPORT: bool encoded as u32)
    // - Attr 6: 4 bytes (SYMLINK_SUPPORT: bool encoded as u32)
    // - Attr 13: 4 bytes (ACLSUPPORT: u32)
    // - Attr 16: 4 bytes (CASE_INSENSITIVE: bool encoded as u32)
    // - Attr 17: 4 bytes (CASE_PRESERVING: bool encoded as u32)
    // - Attr 75: 12 bytes (SUPPATTR_EXCLCREAT: bitmap4 array_len=2, word0, word1 = 4+4+4)
    
    let expected_sizes = vec![
        (0, 8),   // SUPPORTED_ATTRS: 4 (array len) + 4 (word0 + word1... wait, 2 words = 8 bytes?)
        (2, 4),   // FH_EXPIRE_TYPE
        (5, 4),   // LINK_SUPPORT
        (6, 4),   // SYMLINK_SUPPORT
        (13, 4),  // ACLSUPPORT
        (16, 4),  // CASE_INSENSITIVE
        (17, 4),  // CASE_PRESERVING
        (75, 12), // SUPPATTR_EXCLCREAT: 4 (array len=2) + 4 (word0) + 4 (word1)
    ];
    
    let total: usize = expected_sizes.iter().map(|(_, size)| size).sum();
    println!("\nExpected total attr_vals bytes: {}", total);
    println!("Server log shows: 44 bytes");
    
    assert_eq!(total, 44, "Attribute bytes should sum to 44");
    
    println!("\n✅ Server caps attribute sizes verified");
    
    // Now verify SUPPORTED_ATTRS encoding
    // It should be: array_len (4) + words (N * 4)
    // For attributes 0-55, we need 2 words (0-31, 32-63)
    // So: 4 (len=2) + 4 (word0) + 4 (word1) = 12 bytes?
    // But log shows 8 bytes...
    
    println!("\n⚠️  POTENTIAL ISSUE:");
    println!("  SUPPORTED_ATTRS logged as 8 bytes");
    println!("  But bitmap4 should be: array_len (4) + array_data (N*4)");
    println!("  If we have 2 words: 4 + 4 + 4 = 12 bytes");
    println!("  If we encode only the u64 value: 4 + 4 = 8 bytes");
    println!("  ^^^ THIS IS THE BUG! We're encoding as u64, not as bitmap4 array!");
}

#[test]
fn test_bitmap4_encoding_format() {
    // Per RFC 5661, bitmap4 is defined as:
    // typedef uint32_t bitmap4<>;
    // 
    // This means: XDR variable-length array of u32
    // Format: array_length (u32) + array_elements (each u32)
    //
    // For a 2-word bitmap:
    //   4 bytes: array length = 2
    //   4 bytes: word 0
    //   4 bytes: word 1
    //   Total: 12 bytes
    //
    // We're currently encoding SUPPORTED_ATTRS as:
    //   4 bytes: (supported >> 32) as u32  -- word 0
    //   4 bytes: supported as u32          -- word 1
    //   Total: 8 bytes (MISSING array length prefix!)
    
    println!("RFC 5661 bitmap4 definition:");
    println!("  typedef uint32_t bitmap4<>;");
    println!("");
    println!("Correct encoding:");
    println!("  array_length (u32) + array_elements (u32 * length)");
    println!("");
    println!("For 2-word bitmap:");
    println!("  [4 bytes: length=2] [4 bytes: word0] [4 bytes: word1] = 12 bytes");
    println!("");
    println!("❌ OUR BUG:");
    println!("  We encode: [4 bytes: word0] [4 bytes: word1] = 8 bytes");
    println!("  Missing: array length prefix!");
    println!("");
    println!("This causes verify_attr_len to fail:");
    println!("  - We declare some length X");
    println!("  - Kernel expects X bytes after decoding bitmap4 (with array length)");
    println!("  - But we encoded WITHOUT array length");
    println!("  - Byte count mismatch → EIO");
    
    panic!("SUPPORTED_ATTRS and SUPPATTR_EXCLCREAT must encode array_length prefix!");
}

