// Test to reproduce ENOTDIR issue
// Validates exact attribute sequence {1, 3, 4, 8, 20, 33, 35, 36, 37, 41, 45, 47, 52, 53, 55}

use spdk_csi_driver::nfs::xdr::{XdrEncoder, XdrDecoder};

#[test]
fn test_root_directory_attributes_encoding() {
    // This test encodes the exact attributes the kernel requests for the root directory
    // and verifies they can be decoded back with correct non-zero values
    
    // First encode just the attribute VALUES (not bitmap, not length)
    let mut attr_vals_encoder = XdrEncoder::new();
    
    // Encode attribute values in bitmap order for attrs {1, 3, 4, 8, 20, 33, 35, 36, 37, 41, 45, 47, 52, 53, 55}
    // Attr 1: TYPE = 2 (NF4DIR - directory)
    attr_vals_encoder.encode_u32(2);
    
    // Attr 3: CHANGE = 1765474980 (u64)
    attr_vals_encoder.encode_u64(1765474980);
    
    // Attr 4: SIZE = 4096 (u64)
    attr_vals_encoder.encode_u64(4096);
    
    // Attr 8: FSID = (0, 1) (2 u64s = 16 bytes)
    attr_vals_encoder.encode_u64(0); // major
    attr_vals_encoder.encode_u64(1); // minor
    
    // Attr 20: FILEID = 792437 (u64)
    attr_vals_encoder.encode_u64(792437);
    
    // Attr 33: MODE = 0755 (u32)
    attr_vals_encoder.encode_u32(0o755);
    
    // Attr 35: NUMLINKS = 2 (u32)
    attr_vals_encoder.encode_u32(2);
    
    // Attr 36: OWNER = "0" (string: len + data + padding = 8 bytes)
    attr_vals_encoder.encode_u32(1); // length
    attr_vals_encoder.append_raw(b"0");
    attr_vals_encoder.append_raw(&[0u8, 0u8, 0u8]); // padding
    
    // Attr 37: OWNER_GROUP = "0" (string: len + data + padding = 8 bytes)
    attr_vals_encoder.encode_u32(1); // length
    attr_vals_encoder.append_raw(b"0");
    attr_vals_encoder.append_raw(&[0u8, 0u8, 0u8]); // padding
    
    // Attr 41: RAWDEV = (0, 0) for directory (specdata4: 2 u32s)
    attr_vals_encoder.encode_u32(0); // major
    attr_vals_encoder.encode_u32(0); // minor
    
    // Attr 45: SPACE_USED = 4096 (u64)
    attr_vals_encoder.encode_u64(4096);
    
    // Attr 47: TIME_ACCESS = (1765474980, 336104601) (nfstime4: i64 + u32)
    let atime_bytes = 1765474980i64.to_be_bytes();
    attr_vals_encoder.append_raw(&atime_bytes);
    attr_vals_encoder.encode_u32(336104601);
    
    // Attr 52: TIME_METADATA = (1765474980, 336104601) (nfstime4: i64 + u32)
    let ctime_bytes = 1765474980i64.to_be_bytes();
    attr_vals_encoder.append_raw(&ctime_bytes);
    attr_vals_encoder.encode_u32(336104601);
    
    // Attr 53: TIME_MODIFY = (1765474980, 336104601) (nfstime4: i64 + u32)
    let mtime_bytes = 1765474980i64.to_be_bytes();
    attr_vals_encoder.append_raw(&mtime_bytes);
    attr_vals_encoder.encode_u32(336104601);
    
    // Attr 55: MOUNTED_ON_FILEID = 792437 (u64)
    attr_vals_encoder.encode_u64(792437);
    
    let attr_vals = attr_vals_encoder.finish();
    let attr_vals_len = attr_vals.len();
    
    println!("Encoded {} bytes of attribute values", attr_vals_len);
    println!("\nExpected breakdown:");
    println!("  TYPE (1): 4 bytes");
    println!("  CHANGE (3): 8 bytes");
    println!("  SIZE (4): 8 bytes");
    println!("  FSID (8): 16 bytes (2 u64s)");
    println!("  FILEID (20): 8 bytes");
    println!("  MODE (33): 4 bytes");
    println!("  NUMLINKS (35): 4 bytes");
    println!("  OWNER (36): 8 bytes (len + data + padding)");
    println!("  OWNER_GROUP (37): 8 bytes (len + data + padding)");
    println!("  RAWDEV (41): 8 bytes (2 u32s)");
    println!("  SPACE_USED (45): 8 bytes");
    println!("  TIME_ACCESS (47): 12 bytes (i64 + u32)");
    println!("  TIME_METADATA (52): 12 bytes (i64 + u32)");
    println!("  TIME_MODIFY (53): 12 bytes (i64 + u32)");
    println!("  MOUNTED_ON_FILEID (55): 8 bytes");
    println!("  TOTAL: 4+8+8+16+8+4+4+8+8+8+8+12+12+12+8 = 128 bytes");
    
    if attr_vals_len != 128 {
        println!("\n❌ ERROR: Got {} bytes instead of 128!", attr_vals_len);
        println!("   Difference: {} bytes", (attr_vals_len as i32 - 128));
        
        // Print actual bytes to debug
        let bytes: Vec<u8> = attr_vals.to_vec();
        println!("\n  First 64 bytes: {:02x?}", &bytes[..64.min(bytes.len())]);
    }
    
    // For now, just document what we got
    println!("\nActual encoded: {} bytes", attr_vals_len);
    
    // Now decode just the attr_vals back to verify
    let mut decoder = XdrDecoder::new(attr_vals.clone());
    
    // Decode attr 1: TYPE
    let type_val = decoder.decode_u32().expect("decode TYPE");
    assert_eq!(type_val, 2, "TYPE should be 2 (directory)");
    println!("✓ TYPE = {} (directory)", type_val);
    
    // Decode attr 3: CHANGE
    let change_val = decoder.decode_u64().expect("decode CHANGE");
    assert_eq!(change_val, 1765474980, "CHANGE should be non-zero");
    println!("✓ CHANGE = {}", change_val);
    
    // Decode attr 4: SIZE
    let size_val = decoder.decode_u64().expect("decode SIZE");
    assert_eq!(size_val, 4096, "SIZE should be 4096");
    println!("✓ SIZE = {}", size_val);
    
    // Decode attr 8: FSID
    let fsid_major = decoder.decode_u64().expect("decode FSID major");
    let fsid_minor = decoder.decode_u64().expect("decode FSID minor");
    assert_eq!(fsid_major, 0, "FSID major should be 0");
    assert_eq!(fsid_minor, 1, "FSID minor should be 1");
    println!("✓ FSID = ({}, {})", fsid_major, fsid_minor);
    
    // Decode attr 20: FILEID
    let fileid = decoder.decode_u64().expect("decode FILEID");
    assert_eq!(fileid, 792437, "FILEID should be 792437");
    println!("✓ FILEID = {}", fileid);
    
    // Decode attr 33: MODE
    let mode = decoder.decode_u32().expect("decode MODE");
    assert_eq!(mode, 0o755, "MODE should be 0755");
    println!("✓ MODE = {:o}", mode);
    
    // Decode attr 35: NUMLINKS
    let numlinks = decoder.decode_u32().expect("decode NUMLINKS");
    assert_eq!(numlinks, 2, "NUMLINKS should be 2");
    println!("✓ NUMLINKS = {}", numlinks);
    
    // Skip attrs 36, 37 (owner strings with padding - 8 bytes each)
    for _ in 0..4 {
        decoder.decode_u32().expect("skip owner/group");
    }
    
    // Decode attr 41: RAWDEV
    let rdev_major = decoder.decode_u32().expect("decode RAWDEV major");
    let rdev_minor = decoder.decode_u32().expect("decode RAWDEV minor");
    assert_eq!(rdev_major, 0, "RAWDEV major should be 0 for directory");
    assert_eq!(rdev_minor, 0, "RAWDEV minor should be 0 for directory");
    println!("✓ RAWDEV = ({}, {})", rdev_major, rdev_minor);
    
    // Decode attr 45: SPACE_USED
    let space_used = decoder.decode_u64().expect("decode SPACE_USED");
    assert_eq!(space_used, 4096, "SPACE_USED should be 4096");
    println!("✓ SPACE_USED = {}", space_used);
    
    // Decode attr 47: TIME_ACCESS (nfstime4: i64 seconds + u32 nsec = 12 bytes)
    let atime_secs = decoder.decode_u64().expect("decode atime secs") as i64;
    let atime_nsecs = decoder.decode_u32().expect("decode atime nsec");
    assert_eq!(atime_secs, 1765474980, "TIME_ACCESS seconds should match");
    println!("✓ TIME_ACCESS = ({}, {})", atime_secs, atime_nsecs);
    
    // Decode attr 52: TIME_METADATA
    let ctime_secs = decoder.decode_u64().expect("decode ctime secs") as i64;
    let ctime_nsecs = decoder.decode_u32().expect("decode ctime nsec");
    assert_eq!(ctime_secs, 1765474980, "TIME_METADATA seconds should match");
    println!("✓ TIME_METADATA = ({}, {})", ctime_secs, ctime_nsecs);
    
    // Decode attr 53: TIME_MODIFY
    let mtime_secs = decoder.decode_u64().expect("decode mtime secs") as i64;
    let mtime_nsecs = decoder.decode_u32().expect("decode mtime nsec");
    assert_eq!(mtime_secs, 1765474980, "TIME_MODIFY seconds should be non-zero!");
    assert_ne!(mtime_secs, 0, "❌ TIME_MODIFY must not be 0!");
    println!("✓ TIME_MODIFY = ({}, {}) - NOT ZERO!", mtime_secs, mtime_nsecs);
    
    // Decode attr 55: MOUNTED_ON_FILEID
    let mounted_fileid = decoder.decode_u64().expect("decode MOUNTED_ON_FILEID");
    assert_eq!(mounted_fileid, 792437, "MOUNTED_ON_FILEID should be 792437");
    assert_ne!(mounted_fileid, 0, "❌ MOUNTED_ON_FILEID must not be 0!");
    println!("✓ MOUNTED_ON_FILEID = {} - NOT ZERO!", mounted_fileid);
    
    assert_eq!(decoder.remaining(), 0, "Should have consumed all 128 bytes");
    
    println!("\n✅ All attributes decode correctly with non-zero values!");
    println!("   If kernel sees zeros, the issue is in how we're ENCODING, not decoding.");
}

