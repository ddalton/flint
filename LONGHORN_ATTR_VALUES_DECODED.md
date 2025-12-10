# Longhorn GETATTR Attribute Values Decoding

## Longhorn GETATTR Response (380 bytes)

From the packet capture, the GETATTR response from Longhorn that succeeds:

### Response Structure
```
Status: NFS4_OK (0x00000000)
Bitmap word count: 0x00000002 (2 words)
Bitmap[0]: 0x0010011A  
Bitmap[1]: 0x00B0A23A
Attr vals length: 0x00000080 (128 bytes!)
... attribute values ...
```

### Attribute Values (128 bytes starting at offset 0x0144)

Hex dump of attribute values section:
```
0x0144: 00 00 00 02  18 7f ef 98  f24e 7d1b  0000 0000
0x0154: 0000 0000  0000 0000  0000 0098  0000 0000
0x0164: 0000 0098  0000 0000  0000 0000  0000 01ed
0x0174: 0000 0003  0000 0001  3000 0000  0000 0001
0x0184: 3000 0000  0000 0000  0000 0000  0000 0000
0x0194: 0000 0000  0000 0000  6939 c27b  0783 7990
0x01a4: 0000 0000  6939 c27b  07db 6f1b  0000 0000
0x01b4: 6939 c27b  07db 6f1b  0000 0000  0000 0000
```

### Decode Attributes in Bitmap Order

Attributes requested: {1, 3, 4, 8, 20, 33, 35, 36, 37, 41, 45, 47, 52, 53, 55}

#### Attribute 1: TYPE (4 bytes - u32)
```
Offset: 0x0144
Value: 00 00 00 02
Decoded: 2 = NF4DIR (directory)
```

#### Attribute 3: CHANGE (8 bytes - u64)  
```
Offset: 0x0148
Value: 18 7f ef 98  f24e 7d1b
Decoded: 0x187fef98f24e7d1b = 1764185931732647195
```

#### Attribute 4: SIZE (8 bytes - u64)
```
Offset: 0x0150
Value: 0000 0000  0000 0000
Decoded: 0 bytes (empty directory)
```

#### Attribute 8: FSID (16 bytes - u64 major + u64 minor)
```
Offset: 0x0158
Value: 0000 0000  0000 0000  0000 0098  0000 0000
Decoded: major=0x0000000000000000, minor=0x0000000000000098 (152)
```

#### Attribute 20: FILEID (8 bytes - u64)
```
Offset: 0x0168
Value: 0000 0000  0000 0098
Decoded: 152 (inode number)
```

#### Attribute 33: MODE (4 bytes - u32)
```
Offset: 0x0170
Value: 0000 0000
Decoded: 0 (no permissions? This seems wrong!)
Wait, let me recount offsets...
```

Actually, let me recalculate offsets more carefully. Each attribute is XDR-encoded, which means values are padded to 4-byte boundaries.

Let me start over with proper XDR decoding:

## Proper XDR Decode

### Starting from attribute values (after bitmap):

**Position 0: Attribute 1 (TYPE) - u32**
```
Bytes: 00 00 00 02
Value: 2 (directory)
Next position: 4
```

**Position 4: Attribute 3 (CHANGE) - u64**
```
Bytes: 18 7f ef 98 f2 4e 7d 1b
Value: 0x187fef98f24e7d1b
Next position: 12
```

**Position 12: Attribute 4 (SIZE) - u64**
```
Bytes: 00 00 00 00 00 00 00 00
Value: 0
Next position: 20
```

**Position 20: Attribute 8 (FSID) - u64 + u64**
```
Bytes: 00 00 00 00 00 00 00 00  00 00 00 98 00 00 00 00
Value: major=0, minor=0x0000009800000000 (big-endian!)
Wait, that's not right either...
```

Let me look at the actual hex more carefully from the packet:

```
0x0140:  00b0 a23a 0000 0080 0000 0002 187f ef98
```

Starting at 0x0148:
- `0000 0002` = TYPE (2 = directory) ✅
- `187f ef98` = first 4 bytes of CHANGE
- `f24e 7d1b` = last 4 bytes of CHANGE = 0x187fef98f24e7d1b ✅
-  `0000 0000` = first 4 bytes of SIZE
- `0000 0000` = last 4 bytes of SIZE = 0 ✅
- ...

This is getting complex. Let me check what our Flint server is actually returning and compare.

## Key Question

**Flint returns 116 bytes of attribute values**
**Longhorn returns 128 bytes of attribute values** 

That's a **12-byte difference**!

This could be the issue - we're not encoding enough data for some attributes!

## Hypothesis

One or more of our attribute encodings is shorter than Longhorn's. Likely candidates:
1. String attributes (OWNER, OWNER_GROUP) - maybe our strings are shorter?
2. Time attributes - maybe we're encoding them differently?
3. Some attribute is missing entirely?

## Next Step

Need to:
1. Capture Flint's actual GETATTR response bytes (not just logs)
2. Compare byte-by-byte with Longhorn
3. Find the 12-byte discrepancy

