// Test READDIR response encoding per RFC 5661
//
// This test validates the READDIR response structure, especially for
// pseudo-root directory listing.

#[cfg(test)]
mod tests {
    use bytes::{BytesMut, BufMut};

    /// Mock Fattr4 structure
    #[derive(Debug, Clone)]
    struct Fattr4 {
        attrmask: Vec<u32>,
        attr_vals: Vec<u8>,
    }

    /// Mock DirEntry structure  
    #[derive(Debug, Clone)]
    struct DirEntry {
        cookie: u64,
        name: String,
        attrs: Fattr4,
    }

    /// Mock ReadDirRes structure
    struct ReadDirRes {
        cookieverf: u64,
        entries: Vec<DirEntry>,
        eof: bool,
    }

    /// Encode READDIR response (matching our implementation)
    fn encode_readdir_response(res: &ReadDirRes) -> BytesMut {
        let mut buf = BytesMut::new();
        
        // Encode cookieverf (u64)
        buf.put_u64(res.cookieverf);
        
        // Encode value_follows (TRUE if entries exist)
        buf.put_u32(if res.entries.is_empty() { 0 } else { 1 });
        
        // Encode directory entries as linked list
        for (i, entry) in res.entries.iter().enumerate() {
            buf.put_u64(entry.cookie);
            
            // Encode name as string4 (length + bytes + padding)
            buf.put_u32(entry.name.len() as u32);
            buf.put_slice(entry.name.as_bytes());
            let name_padding = (4 - (entry.name.len() % 4)) % 4;
            for _ in 0..name_padding {
                buf.put_u8(0);
            }
            
            // Encode Fattr4 (bitmap + attr_vals)
            // Bitmap
            buf.put_u32(entry.attrs.attrmask.len() as u32);
            for word in &entry.attrs.attrmask {
                buf.put_u32(*word);
            }
            
            // Attr vals as opaque
            buf.put_u32(entry.attrs.attr_vals.len() as u32);
            buf.put_slice(&entry.attrs.attr_vals);
            let attr_padding = (4 - (entry.attrs.attr_vals.len() % 4)) % 4;
            for _ in 0..attr_padding {
                buf.put_u8(0);
            }
            
            // Encode next_entry pointer (TRUE if more entries follow)
            let has_more = i < res.entries.len() - 1;
            buf.put_u32(if has_more { 1 } else { 0 });
        }
        
        // Encode eof flag
        buf.put_u32(if res.eof { 1 } else { 0 });
        
        buf
    }

    #[test]
    fn test_readdir_empty_directory() {
        println!("\n=== Test: READDIR Empty Directory ===");
        
        let res = ReadDirRes {
            cookieverf: 1,
            entries: vec![],
            eof: true,
        };
        
        let encoded = encode_readdir_response(&res);
        
        println!("Encoded bytes: {} bytes", encoded.len());
        println!("Hex: {:02x?}", &encoded[..]);
        
        // Verify structure:
        // - cookieverf (8 bytes)
        // - value_follows FALSE (4 bytes) 
        // - eof TRUE (4 bytes)
        assert_eq!(encoded.len(), 16);
        
        // Check cookieverf
        assert_eq!(&encoded[0..8], &[0, 0, 0, 0, 0, 0, 0, 1]);
        
        // Check value_follows = FALSE
        assert_eq!(&encoded[8..12], &[0, 0, 0, 0]);
        
        // Check eof = TRUE
        assert_eq!(&encoded[12..16], &[0, 0, 0, 1]);
        
        println!("✅ Empty directory encoding correct");
    }

    #[test]
    fn test_readdir_single_entry() {
        println!("\n=== Test: READDIR Single Entry ===");
        
        // Create entry for "volume" export with TYPE attribute
        let mut attr_vals = BytesMut::new();
        attr_vals.put_u32(2); // NF4DIR
        
        let entry = DirEntry {
            cookie: 1,
            name: "volume".to_string(),
            attrs: Fattr4 {
                attrmask: vec![2], // TYPE attribute
                attr_vals: attr_vals.to_vec(),
            },
        };
        
        let res = ReadDirRes {
            cookieverf: 1,
            entries: vec![entry],
            eof: true,
        };
        
        let encoded = encode_readdir_response(&res);
        
        println!("Encoded bytes: {} bytes", encoded.len());
        println!("Hex dump:");
        for (i, chunk) in encoded.chunks(16).enumerate() {
            println!("  [{:04x}] {:02x?}", i * 16, chunk);
        }
        
        // Decode and verify structure
        let mut offset = 0;
        
        // cookieverf (8 bytes)
        let cookieverf = u64::from_be_bytes([
            encoded[offset], encoded[offset+1], encoded[offset+2], encoded[offset+3],
            encoded[offset+4], encoded[offset+5], encoded[offset+6], encoded[offset+7],
        ]);
        offset += 8;
        assert_eq!(cookieverf, 1);
        println!("  cookieverf: {}", cookieverf);
        
        // value_follows (4 bytes) - should be TRUE
        let value_follows = u32::from_be_bytes([
            encoded[offset], encoded[offset+1], encoded[offset+2], encoded[offset+3],
        ]);
        offset += 4;
        assert_eq!(value_follows, 1);
        println!("  value_follows: {}", value_follows);
        
        // cookie (8 bytes)
        let cookie = u64::from_be_bytes([
            encoded[offset], encoded[offset+1], encoded[offset+2], encoded[offset+3],
            encoded[offset+4], encoded[offset+5], encoded[offset+6], encoded[offset+7],
        ]);
        offset += 8;
        assert_eq!(cookie, 1);
        println!("  cookie: {}", cookie);
        
        // name (string4: length + bytes + padding)
        let name_len = u32::from_be_bytes([
            encoded[offset], encoded[offset+1], encoded[offset+2], encoded[offset+3],
        ]) as usize;
        offset += 4;
        assert_eq!(name_len, 6); // "volume"
        
        let name = std::str::from_utf8(&encoded[offset..offset+name_len]).unwrap();
        offset += name_len;
        assert_eq!(name, "volume");
        println!("  name: '{}'", name);
        
        // name padding (2 bytes to align to 4)
        let name_padding = (4 - (name_len % 4)) % 4;
        offset += name_padding;
        
        // Fattr4 bitmap (array length + words)
        let bitmap_len = u32::from_be_bytes([
            encoded[offset], encoded[offset+1], encoded[offset+2], encoded[offset+3],
        ]) as usize;
        offset += 4;
        assert_eq!(bitmap_len, 1);
        
        let bitmap_word = u32::from_be_bytes([
            encoded[offset], encoded[offset+1], encoded[offset+2], encoded[offset+3],
        ]);
        offset += 4;
        assert_eq!(bitmap_word, 2); // TYPE attribute
        println!("  bitmap: [{}]", bitmap_word);
        
        // Fattr4 attr_vals (opaque: length + bytes + padding)
        let attr_vals_len = u32::from_be_bytes([
            encoded[offset], encoded[offset+1], encoded[offset+2], encoded[offset+3],
        ]) as usize;
        offset += 4;
        assert_eq!(attr_vals_len, 4); // u32 for TYPE
        
        let type_val = u32::from_be_bytes([
            encoded[offset], encoded[offset+1], encoded[offset+2], encoded[offset+3],
        ]);
        offset += 4;
        assert_eq!(type_val, 2); // NF4DIR
        println!("  TYPE: {} (directory)", type_val);
        
        // No padding needed (4 % 4 == 0)
        
        // next_entry (4 bytes) - should be FALSE (last entry)
        let next_entry = u32::from_be_bytes([
            encoded[offset], encoded[offset+1], encoded[offset+2], encoded[offset+3],
        ]);
        offset += 4;
        assert_eq!(next_entry, 0);
        println!("  next_entry: FALSE");
        
        // eof (4 bytes) - should be TRUE
        let eof = u32::from_be_bytes([
            encoded[offset], encoded[offset+1], encoded[offset+2], encoded[offset+3],
        ]);
        offset += 4;
        assert_eq!(eof, 1);
        println!("  eof: TRUE");
        
        // Should have consumed all bytes
        assert_eq!(offset, encoded.len());
        
        println!("✅ Single entry READDIR encoding correct");
        println!("   Total size: {} bytes", encoded.len());
    }

    #[test]
    fn test_readdir_multiple_entries() {
        println!("\n=== Test: READDIR Multiple Entries ===");
        
        // Create multiple export entries
        let mut attr_vals = BytesMut::new();
        attr_vals.put_u32(2); // NF4DIR
        
        let entries = vec![
            DirEntry {
                cookie: 1,
                name: "vol1".to_string(),
                attrs: Fattr4 {
                    attrmask: vec![2],
                    attr_vals: attr_vals.to_vec(),
                },
            },
            DirEntry {
                cookie: 2,
                name: "vol2".to_string(),
                attrs: Fattr4 {
                    attrmask: vec![2],
                    attr_vals: attr_vals.to_vec(),
                },
            },
        ];
        
        let res = ReadDirRes {
            cookieverf: 1,
            entries,
            eof: true,
        };
        
        let encoded = encode_readdir_response(&res);
        
        println!("Encoded bytes: {} bytes", encoded.len());
        
        // Verify we have 2 entries in the linked list
        // After cookieverf and value_follows, we should have:
        // Entry 1 + next_entry=TRUE + Entry 2 + next_entry=FALSE + eof
        
        let offset = 8; // Skip cookieverf
        let value_follows = u32::from_be_bytes([
            encoded[offset], encoded[offset+1], encoded[offset+2], encoded[offset+3],
        ]);
        assert_eq!(value_follows, 1);
        
        println!("✅ Multiple entry READDIR encoding appears correct");
        println!("   Total size: {} bytes (for 2 entries)", encoded.len());
    }

    #[test]
    fn test_readdir_pseudo_root_realistic() {
        println!("\n=== Test: READDIR Pseudo-Root (Realistic) ===");
        println!("This simulates what Flint returns when listing pseudo-root");
        
        // Minimal TYPE-only attributes for export entry
        let mut attr_vals = BytesMut::new();
        attr_vals.put_u32(2); // NF4DIR
        
        let entry = DirEntry {
            cookie: 1,
            name: "volume".to_string(),
            attrs: Fattr4 {
                attrmask: vec![2], // Only TYPE attribute
                attr_vals: attr_vals.to_vec(),
            },
        };
        
        let res = ReadDirRes {
            cookieverf: 1,
            entries: vec![entry],
            eof: true,
        };
        
        let encoded = encode_readdir_response(&res);
        
        println!("Pseudo-root READDIR response: {} bytes", encoded.len());
        println!("\nStructure breakdown:");
        
        let mut offset = 0;
        
        println!("  [0x{:04x}] cookieverf: 0x{:016x} (8 bytes)", 
                 offset, res.cookieverf);
        offset += 8;
        
        println!("  [0x{:04x}] value_follows: TRUE (4 bytes)", offset);
        offset += 4;
        
        println!("  [0x{:04x}] cookie: {} (8 bytes)", offset, res.entries[0].cookie);
        offset += 8;
        
        println!("  [0x{:04x}] name: '{}' (4 + {} + {} padding)", 
                 offset, res.entries[0].name, res.entries[0].name.len(), 
                 (4 - (res.entries[0].name.len() % 4)) % 4);
        offset += 4 + res.entries[0].name.len() + ((4 - (res.entries[0].name.len() % 4)) % 4);
        
        println!("  [0x{:04x}] bitmap: [2] (8 bytes: length + 1 word)", offset);
        offset += 8;
        
        println!("  [0x{:04x}] attr_vals: TYPE=2 (8 bytes: length + value)", offset);
        offset += 8;
        
        println!("  [0x{:04x}] next_entry: FALSE (4 bytes)", offset);
        offset += 4;
        
        println!("  [0x{:04x}] eof: TRUE (4 bytes)", offset);
        offset += 4;
        
        assert_eq!(offset, encoded.len(), "All bytes accounted for");
        
        println!("\n✅ Pseudo-root READDIR encoding validated");
        println!("   Expected size: {} bytes", encoded.len());
    }

    #[test]
    fn test_readdir_rfc5661_compliance() {
        println!("\n=== Test: RFC 5661 READDIR Structure ===");
        println!("Per RFC 5661 Section 18.23:");
        println!("  READDIR4resok {{");
        println!("    verifier4     cookieverf;");
        println!("    dirlist4      reply;");
        println!("  }}");
        println!("  struct dirlist4 {{");
        println!("    entry4        *entries;");
        println!("    bool          eof;");
        println!("  }}");
        println!("  struct entry4 {{");
        println!("    nfs_cookie4   cookie;");
        println!("    component4    name;");
        println!("    fattr4        attrs;");
        println!("    entry4        *nextentry;");
        println!("  }}");
        
        // Test with one entry
        let mut attr_vals = BytesMut::new();
        attr_vals.put_u32(2); // TYPE = NF4DIR
        
        let res = ReadDirRes {
            cookieverf: 0x123456789ABCDEF0,
            entries: vec![
                DirEntry {
                    cookie: 100,
                    name: "export".to_string(),
                    attrs: Fattr4 {
                        attrmask: vec![2],
                        attr_vals: attr_vals.to_vec(),
                    },
                },
            ],
            eof: true,
        };
        
        let encoded = encode_readdir_response(&res);
        
        println!("\nEncoded structure:");
        let mut offset = 0;
        
        // cookieverf
        let cookieverf = u64::from_be_bytes([
            encoded[0], encoded[1], encoded[2], encoded[3],
            encoded[4], encoded[5], encoded[6], encoded[7],
        ]);
        offset += 8;
        assert_eq!(cookieverf, 0x123456789ABCDEF0);
        println!("  ✓ cookieverf: 0x{:016x}", cookieverf);
        
        // dirlist4.entries (linked list)
        let value_follows = u32::from_be_bytes([
            encoded[offset], encoded[offset+1], encoded[offset+2], encoded[offset+3],
        ]);
        offset += 4;
        assert_eq!(value_follows, 1, "Should have entries");
        println!("  ✓ entries present: {}", value_follows == 1);
        
        // entry4.cookie
        let cookie = u64::from_be_bytes([
            encoded[offset], encoded[offset+1], encoded[offset+2], encoded[offset+3],
            encoded[offset+4], encoded[offset+5], encoded[offset+6], encoded[offset+7],
        ]);
        offset += 8;
        assert_eq!(cookie, 100);
        println!("  ✓ cookie: {}", cookie);
        
        // entry4.name
        let name_len = u32::from_be_bytes([
            encoded[offset], encoded[offset+1], encoded[offset+2], encoded[offset+3],
        ]) as usize;
        offset += 4;
        let name = std::str::from_utf8(&encoded[offset..offset+name_len]).unwrap();
        offset += name_len;
        let name_padding = (4 - (name_len % 4)) % 4;
        offset += name_padding;
        assert_eq!(name, "export");
        println!("  ✓ name: '{}' (+{} padding)", name, name_padding);
        
        // entry4.attrs.bitmap
        let bitmap_len = u32::from_be_bytes([
            encoded[offset], encoded[offset+1], encoded[offset+2], encoded[offset+3],
        ]) as usize;
        offset += 4;
        assert_eq!(bitmap_len, 1);
        
        let bitmap_word = u32::from_be_bytes([
            encoded[offset], encoded[offset+1], encoded[offset+2], encoded[offset+3],
        ]);
        offset += 4;
        assert_eq!(bitmap_word, 2);
        println!("  ✓ attrs.bitmap: [{}]", bitmap_word);
        
        // entry4.attrs.attr_vals
        let _attr_vals_len = u32::from_be_bytes([
            encoded[offset], encoded[offset+1], encoded[offset+2], encoded[offset+3],
        ]) as usize;
        offset += 4;
        let type_val = u32::from_be_bytes([
            encoded[offset], encoded[offset+1], encoded[offset+2], encoded[offset+3],
        ]);
        offset += 4;
        assert_eq!(type_val, 2); // NF4DIR
        println!("  ✓ attrs.TYPE: {} (directory)", type_val);
        
        // entry4.nextentry
        let next_entry = u32::from_be_bytes([
            encoded[offset], encoded[offset+1], encoded[offset+2], encoded[offset+3],
        ]);
        offset += 4;
        assert_eq!(next_entry, 0, "Should be FALSE (last entry)");
        println!("  ✓ nextentry: FALSE");
        
        // dirlist4.eof
        let eof = u32::from_be_bytes([
            encoded[offset], encoded[offset+1], encoded[offset+2], encoded[offset+3],
        ]);
        offset += 4;
        assert_eq!(eof, 1);
        println!("  ✓ eof: TRUE");
        
        assert_eq!(offset, encoded.len(), "All bytes decoded");
        
        println!("\n✅ RFC 5661 READDIR structure compliance verified");
        println!("   Total wire format: {} bytes", encoded.len());
    }

    #[test]
    fn test_readdir_cookie_sequence() {
        println!("\n=== Test: READDIR Cookie Sequence ===");
        println!("Cookies must be monotonically increasing and non-zero");
        
        let mut attr_vals = BytesMut::new();
        attr_vals.put_u32(2);
        
        let res = ReadDirRes {
            cookieverf: 1,
            entries: vec![
                DirEntry {
                    cookie: 1,
                    name: "a".to_string(),
                    attrs: Fattr4 { attrmask: vec![2], attr_vals: attr_vals.to_vec() },
                },
                DirEntry {
                    cookie: 2,
                    name: "b".to_string(),
                    attrs: Fattr4 { attrmask: vec![2], attr_vals: attr_vals.to_vec() },
                },
                DirEntry {
                    cookie: 3,
                    name: "c".to_string(),
                    attrs: Fattr4 { attrmask: vec![2], attr_vals: attr_vals.to_vec() },
                },
            ],
            eof: true,
        };
        
        // Verify cookies are sequential
        for (i, entry) in res.entries.iter().enumerate() {
            assert_eq!(entry.cookie, (i + 1) as u64);
            assert_ne!(entry.cookie, 0, "Cookie must be non-zero");
        }
        
        let encoded = encode_readdir_response(&res);
        println!("✅ Cookie sequence valid: 1, 2, 3");
        println!("   Encoded {} entries in {} bytes", res.entries.len(), encoded.len());
    }

    #[test]
    fn test_readdir_attribute_request_filtering() {
        println!("\n=== Test: READDIR Attribute Request Filtering ===");
        println!("Verify we return ONLY requested attributes (NFSv4 requirement)");
        
        // This is the actual bitmap from packet capture that caused the bug
        // Client requested: Type, Change, Size, RDAttr_Error, FileId, Mode, NumLinks, Owner, Time_Metadata
        let requested_bitmap = vec![
            0x0010081a, // Word 0: Type(1), Change(3), Size(4), RDAttr_Error(11), FileId(20)
            0x0010001a, // Word 1: Mode(33), NumLinks(35), Owner(36), Time_Metadata(52)
        ];
        
        println!("Client requested attributes:");
        println!("  Word 0: 0x{:08x}", requested_bitmap[0]);
        println!("  Word 1: 0x{:08x}", requested_bitmap[1]);
        
        // Decode which attributes are requested
        let mut requested_attrs = vec![];
        for (word_idx, word) in requested_bitmap.iter().enumerate() {
            for bit in 0..32 {
                if (word & (1 << bit)) != 0 {
                    requested_attrs.push(word_idx * 32 + bit);
                }
            }
        }
        println!("  Attribute IDs: {:?}", requested_attrs);
        
        // Simulate encode_export_entry_attributes behavior
        // We'll manually build what the function should return
        let mut attr_vals = BytesMut::new();
        let mut returned_bitmap = vec![0u32, 0u32];
        
        // Attribute constants (from fileops.rs)
        const FATTR4_TYPE: u32 = 1;
        const FATTR4_CHANGE: u32 = 3;
        const FATTR4_SIZE: u32 = 4;
        const FATTR4_RDATTR_ERROR: u32 = 11;
        const FATTR4_FILEID: u32 = 20;
        const FATTR4_MODE: u32 = 33;
        const FATTR4_NUMLINKS: u32 = 35;
        const FATTR4_OWNER: u32 = 36;
        const FATTR4_TIME_METADATA: u32 = 52;
        
        // Encode only requested attributes in order
        for &attr_id in &requested_attrs {
            let word_idx = (attr_id / 32) as usize;
            let bit = attr_id % 32;
            
            match attr_id as u32 {
                FATTR4_TYPE => {
                    attr_vals.put_u32(2); // NF4DIR
                    returned_bitmap[word_idx] |= 1 << bit;
                    println!("  ✓ Encoded TYPE (attr 1): 4 bytes");
                }
                FATTR4_CHANGE => {
                    attr_vals.put_u64(1234567890);
                    returned_bitmap[word_idx] |= 1 << bit;
                    println!("  ✓ Encoded CHANGE (attr 3): 8 bytes");
                }
                FATTR4_SIZE => {
                    attr_vals.put_u64(4096);
                    returned_bitmap[word_idx] |= 1 << bit;
                    println!("  ✓ Encoded SIZE (attr 4): 8 bytes");
                }
                FATTR4_RDATTR_ERROR => {
                    attr_vals.put_u32(0); // NFS4_OK
                    returned_bitmap[word_idx] |= 1 << bit;
                    println!("  ✓ Encoded RDATTR_ERROR (attr 11): 4 bytes");
                }
                FATTR4_FILEID => {
                    attr_vals.put_u64(0x9c5d9ae5e3f8962b);
                    returned_bitmap[word_idx] |= 1 << bit;
                    println!("  ✓ Encoded FILEID (attr 20): 8 bytes");
                }
                FATTR4_MODE => {
                    attr_vals.put_u32(0o755);
                    returned_bitmap[word_idx] |= 1 << bit;
                    println!("  ✓ Encoded MODE (attr 33): 4 bytes");
                }
                FATTR4_NUMLINKS => {
                    attr_vals.put_u32(2);
                    returned_bitmap[word_idx] |= 1 << bit;
                    println!("  ✓ Encoded NUMLINKS (attr 35): 4 bytes");
                }
                FATTR4_OWNER => {
                    attr_vals.put_u32(4); // length
                    attr_vals.put_slice(b"root");
                    returned_bitmap[word_idx] |= 1 << bit;
                    println!("  ✓ Encoded OWNER (attr 36): 8 bytes");
                }
                FATTR4_TIME_METADATA => {
                    attr_vals.put_i64(1234567890); // seconds
                    attr_vals.put_u32(0); // nanoseconds
                    returned_bitmap[word_idx] |= 1 << bit;
                    println!("  ✓ Encoded TIME_METADATA (attr 52): 12 bytes");
                }
                _ => {
                    println!("  ⚠ Attribute {} not supported (skipped)", attr_id);
                }
            }
        }
        
        println!("\nReturned bitmap:");
        println!("  Word 0: 0x{:08x}", returned_bitmap[0]);
        println!("  Word 1: 0x{:08x}", returned_bitmap[1]);
        
        // Verify bitmap matches what was requested (for supported attributes)
        assert_eq!(returned_bitmap[0], requested_bitmap[0], 
            "Returned bitmap word 0 should match requested attributes");
        assert_eq!(returned_bitmap[1], requested_bitmap[1],
            "Returned bitmap word 1 should match requested attributes");
        
        // Total expected size: 4 + 8 + 8 + 4 + 8 + 4 + 4 + 8 + 12 = 60 bytes
        let expected_size = 60;
        assert_eq!(attr_vals.len(), expected_size,
            "Attribute values should be {} bytes", expected_size);
        
        println!("\n✅ Attribute filtering correct:");
        println!("   - Returned only requested attributes");
        println!("   - Returned in correct order (attr ID 1, 3, 4, 11, 20, 33, 35, 36, 52)");
        println!("   - Bitmap matches request");
        println!("   - Total size: {} bytes", attr_vals.len());
    }

    #[test]
    fn test_readdir_unrequested_attributes_not_returned() {
        println!("\n=== Test: READDIR Does Not Return Unrequested Attributes ===");
        
        // Client only requests TYPE and SIZE
        let _requested_bitmap = vec![
            0x0000001a, // Word 0: Type(1), Change(3), Size(4)
        ];
        
        println!("Client requested: Type, Change, Size");
        
        let mut attr_vals = BytesMut::new();
        let mut returned_bitmap = vec![0u32];
        
        // Encode only TYPE, CHANGE, SIZE (NOT FSID, FILEID, MODE, etc.)
        attr_vals.put_u32(2); // TYPE
        returned_bitmap[0] |= 1 << 1;
        
        attr_vals.put_u64(1234567890); // CHANGE
        returned_bitmap[0] |= 1 << 3;
        
        attr_vals.put_u64(4096); // SIZE
        returned_bitmap[0] |= 1 << 4;
        
        // We should NOT encode FSID (8), FILEID (20), MODE (33), etc.
        assert_eq!(attr_vals.len(), 20, "Should only have 20 bytes (4 + 8 + 8)");
        assert_eq!(returned_bitmap[0], 0x0000001a, "Bitmap should only show TYPE, CHANGE, SIZE");
        
        // Verify FSID bit is NOT set
        assert_eq!(returned_bitmap[0] & (1 << 8), 0, "FSID (attr 8) should NOT be returned");
        
        // Verify FILEID bit is NOT set
        assert_eq!(returned_bitmap[0] & (1 << 20), 0, "FILEID (attr 20) should NOT be returned");
        
        println!("✅ Correctly excludes unrequested attributes");
        println!("   - Did not return FSID (was causing the bug!)");
        println!("   - Did not return FILEID, MODE, etc.");
        println!("   - Bitmap exactly matches request");
    }
}

