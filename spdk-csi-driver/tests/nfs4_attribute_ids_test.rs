// Test to verify NFSv4 attribute IDs match RFC 5661 Table 3
// This prevents attribute ID mismatches that cause XDR decoding failures

#[test]
fn test_nfs4_attribute_ids_match_rfc5661() {
    // From RFC 5661 Table 3: Defined Attributes
    // https://datatracker.ietf.org/doc/html/rfc5661#section-5.8
    
    // These are the CORRECT values per RFC 5661
    let rfc_attribute_ids = vec![
        ("SUPPORTED_ATTRS", 0),
        ("TYPE", 1),
        ("FH_EXPIRE_TYPE", 2),
        ("CHANGE", 3),
        ("SIZE", 4),
        ("LINK_SUPPORT", 5),
        ("SYMLINK_SUPPORT", 6),
        ("NAMED_ATTR", 7),
        ("FSID", 8),
        ("UNIQUE_HANDLES", 9),
        ("LEASE_TIME", 10),
        ("RDATTR_ERROR", 11),
        ("ACLSUPPORT", 12),
        ("ACL", 13),
        ("ARCHIVE", 14),
        ("CANSETTIME", 15),  // ❌ We have 35 - WRONG!
        ("CASE_INSENSITIVE", 16),
        ("CASE_PRESERVING", 17),
        ("CHOWN_RESTRICTED", 18),
        ("FILEHANDLE", 19),
        ("FILEID", 20),
        ("FILES_AVAIL", 21),
        ("FILES_FREE", 22),
        ("FILES_TOTAL", 23),
        ("FS_LOCATIONS", 24),
        ("HIDDEN", 25),
        ("HOMOGENEOUS", 26),
        ("MAXFILESIZE", 27),
        ("MAXLINK", 28),  // ❌ We have 41 - WRONG!
        ("MAXNAME", 29),  // ❌ We have 45 - WRONG!
        ("MAXREAD", 30),
        ("MAXWRITE", 31),
        ("MIMETYPE", 32),
        ("MODE", 33),
        ("NO_TRUNC", 34),
        ("NUMLINKS", 35),  // ❌ We have 27 - WRONG!
        ("OWNER", 36),
        ("OWNER_GROUP", 37),
        ("QUOTA_AVAIL_HARD", 38),
        ("QUOTA_AVAIL_SOFT", 39),
        ("QUOTA_USED", 40),
        ("RAWDEV", 41),  // ❌ We're missing this!
        ("SPACE_AVAIL", 42),  // ❌ We have 47 - WRONG!
        ("SPACE_FREE", 43),  // ❌ We have 48 - WRONG!
        ("SPACE_TOTAL", 44),  // ❌ We have 49 - WRONG!
        ("SPACE_USED", 45),  // ❌ We have 50 - WRONG!
        ("SYSTEM", 46),
        ("TIME_ACCESS", 47),  // ❌ We have 51 - WRONG!
        ("TIME_ACCESS_SET", 48),
        ("TIME_BACKUP", 49),
        ("TIME_CREATE", 50),
        ("TIME_DELTA", 51),
        ("TIME_METADATA", 52),
        ("TIME_MODIFY", 53),
        ("TIME_MODIFY_SET", 54),
        ("MOUNTED_ON_FILEID", 55),
    ];
    
    // Import the constants from our code (these will be tested)
    // Since they're private to fileops, we can't import them directly
    // Instead, document what we SHOULD have:
    
    println!("RFC 5661 Attribute IDs:");
    println!("========================");
    for (name, id) in &rfc_attribute_ids {
        println!("  FATTR4_{:<25} = {}", name, id);
    }
    
    println!("\n❌ OUR BUGS FOUND:");
    println!("  FATTR4_CANSETTIME should be 15, not 35");
    println!("  FATTR4_MAXLINK should be 28, not 41");
    println!("  FATTR4_MAXNAME should be 29, not 45");
    println!("  FATTR4_NUMLINKS should be 35, not 27");
    println!("  FATTR4_RAWDEV (41) is missing!");
    println!("  FATTR4_SPACE_AVAIL should be 42, not 47");
    println!("  FATTR4_SPACE_FREE should be 43, not 48");
    println!("  FATTR4_SPACE_TOTAL should be 44, not 49");
    println!("  FATTR4_SPACE_USED should be 45, not 50");
    println!("  FATTR4_TIME_ACCESS should be 47, not 51");
    
    println!("\n🔍 IMPACT:");
    println!("  When client requests attr 35 (NUMLINKS), we encode CANSETTIME (bool)");
    println!("  When client requests attr 41 (RAWDEV), we encode MAXLINK (u32)");
    println!("  This causes byte count mismatches → verify_attr_len fails → EIO");
    
    // This test documents the bug - fix is needed in fileops.rs
    panic!("Attribute IDs in fileops.rs DO NOT match RFC 5661! See output above.");
}

