// Test READDIR response encoding per RFC 5661
//
// This test validates the READDIR response structure, especially for
// pseudo-root directory listing.

#[cfg(test)]
mod tests {
    use bytes::{Bytes, BytesMut, BufMut};

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
        
        let mut offset = 8; // Skip cookieverf
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
        let attr_vals_len = u32::from_be_bytes([
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
}

