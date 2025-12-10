# 🎯 Flint vs Longhorn GETATTR Byte-by-Byte Comparison

## Critical Finding

**Flint is NOT encoding attribute 12 (ACLSUPPORT) even though the client requests it!**

## Client Request (Same for Both)

```
Bitmap: [0x0010011A, 0x00B0A23A]
```

### Bitmap Decoding

**Word 0: 0x0010011A**
- Bit 1 → Attr 1 (TYPE) ✅
- Bit 3 → Attr 3 (CHANGE) ✅
- Bit 4 → Attr 4 (SIZE) ✅
- Bit 8 → Attr 8 (FSID) ✅
- **Bit 12 → Attr 12 (ACLSUPPORT)** ❌ **MISSING IN FLINT!**
- Bit 20 → Attr 20 (FILEID) ✅

**Word 1: 0x00B0A23A**
- Bit 1 → Attr 33 (MODE) ✅
- Bit 3 → Attr 35 (CANSETTIME) ✅
- Bit 4 → Attr 36 (OWNER) ✅
- Bit 5 → Attr 37 (OWNER_GROUP) ✅
- Bit 9 → Attr 41 (MAXLINK) ✅
- Bit 13 → Attr 45 (MAXNAME) ✅
- Bit 15 → Attr 47 (SPACE_AVAIL) ✅
- Bit 20 → Attr 52 (TIME_METADATA) ✅
- Bit 21 → Attr 53 (TIME_MODIFY) ✅
- Bit 23 → Attr 55 (MOUNTED_ON_FILEID) ✅

**Total requested: 16 attributes**

## Flint's Response

### Attributes Encoded
```
{1, 3, 4, 8, 20, 33, 35, 36, 37, 41, 45, 47, 52, 53, 55}
Total: 15 attributes (MISSING attribute 12!)
```

### Byte Breakdown
| Attr | Name              | Bytes | Total |
|------|-------------------|-------|-------|
| 1    | TYPE              | 4     | 4     |
| 3    | CHANGE            | 8     | 12    |
| 4    | SIZE              | 8     | 20    |
| 8    | FSID              | 16    | 36    |
| 20   | FILEID            | 8     | 44    |
| 33   | MODE              | 4     | 48    |
| 35   | CANSETTIME        | 4     | 52    |
| 36   | OWNER             | 8     | 60    |
| 37   | OWNER_GROUP       | 8     | 68    |
| 41   | MAXLINK           | 4     | 72    |
| 45   | MAXNAME           | 4     | 76    |
| 47   | SPACE_AVAIL       | 8     | 84    |
| 52   | TIME_METADATA     | 12    | 96    |
| 53   | TIME_MODIFY       | 12    | 108   |
| 55   | MOUNTED_ON_FILEID | 8     | 116   |

**Total: 116 bytes**

### Flint's Hex Dump
```
[0000]: 00 00 00 02 00 00 00 00 69 3a 05 69 00 00 00 00
[0010]: 00 00 10 00 00 00 00 00 00 00 00 00 00 00 00 00
[0020]: 00 00 00 01 00 00 00 00 00 20 3f f9 00 00 01 ff
[0030]: 00 00 00 01 30 00 00 00 00 00 00 01 30 00 00 00
[0040]: 00 00 ff ff 00 00 00 ff 00 00 00 19 00 00 00 00
[0050]: 00 00 00 00 69 3a 05 69 33 77 dc ff 00 00 00 00
[0060]: 69 3a 05 69 33 77 dc ff 00 00 00 00 00 20 3f f9
```

## Longhorn's Response

### Attributes Should Include
```
{1, 3, 4, 8, 12, 20, 33, 35, 36, 37, 41, 45, 47, 52, 53, 55}
Total: 16 attributes (includes ACLSUPPORT!)
```

### Expected Byte Breakdown
| Attr | Name              | Bytes | Total |
|------|-------------------|-------|-------|
| 1    | TYPE              | 4     | 4     |
| 3    | CHANGE            | 8     | 12    |
| 4    | SIZE              | 8     | 20    |
| 8    | FSID              | 16    | 36    |
| **12** | **ACLSUPPORT**  | **4** | **40** |
| 20   | FILEID            | 8     | 48    |
| 33   | MODE              | 4     | 52    |
| 35   | CANSETTIME        | 4     | 56    |
| 36   | OWNER             | ?     | ?     |
| 37   | OWNER_GROUP       | ?     | ?     |
| 41   | MAXLINK           | 4     | ?     |
| 45   | MAXNAME           | 4     | ?     |
| 47   | SPACE_AVAIL       | 8     | ?     |
| 52   | TIME_METADATA     | 12    | ?     |
| 53   | TIME_MODIFY       | 12    | ?     |
| 55   | MOUNTED_ON_FILEID | 8     | ?     |

**Longhorn's total: 128 bytes**

## The 12-Byte Gap Explained

**Missing from Flint:**
1. **ACLSUPPORT (attr 12): 4 bytes**
2. **Additional 8 bytes somewhere else**

### Hypothesis on the Extra 8 Bytes

Possibilities:
1. OWNER/OWNER_GROUP strings are longer in Longhorn
2. ACLSUPPORT might return more than just a u32
3. There's padding we're not accounting for
4. One of our encoded values is wrong size

### OWNER/OWNER_GROUP Analysis

**Flint:**
- OWNER: 8 bytes total (likely "0" = 1 char + 3 pad + 4 length = 8)
- OWNER_GROUP: 8 bytes total (likely "0" = 1 char + 3 pad + 4 length = 8)

**Need to verify Longhorn's strings are the same length or longer**

## Root Cause

### Primary Issue: Missing ACLSUPPORT
Our bitmap parsing code is **NOT** extracting attribute 12 from the bitmap!

```rust
// This code should find attribute 12, but it doesn't:
for (word_idx, &bitmap_word) in requested_bitmap.iter().enumerate() {
    for bit in 0..32 {
        if (bitmap_word & (1 << bit)) != 0 {
            let attr_id = (word_idx * 32 + bit) as u32;
            requested_attrs.insert(attr_id);
        }
    }
}
```

Let me verify: 0x0010011A has bit 12 set?
```
0x0010011A = 0b00000000000100000000000100011010
                           ^bit12
```

**YES! Bit 12 IS set!**

So why isn't our code finding it?

### DEBUG: Let's manually check
```python
bitmap_word = 0x0010011A
for bit in range(32):
    if bitmap_word & (1 << bit):
        print(f"Bit {bit} is set → Attribute {bit}")
```

Should print:
- Bit 1 → Attribute 1
- Bit 3 → Attribute 3
- Bit 4 → Attribute 4
- Bit 8 → Attribute 8
- Bit 12 → Attribute 12 ✅
- Bit 20 → Attribute 20

But our logs show we're missing attribute 12!

## Next Action

**BUG IN BITMAP PARSING!**

Our code is correctly implemented, so the issue must be that the client's bitmap is different than we think, OR our logging is showing the wrong thing.

Let me check the actual request bitmap from the logs...

Actually, looking at the logs again:
```
GETATTR: Requested attributes: {1, 3, 4, 8, 20, 33, 35, 36, 37, 41, 45, 47, 52, 53, 55}
```

This is AFTER parsing. So our parsing code IS missing attribute 12!

## Solution

Need to debug why the bitmap parsing is skipping attribute 12. Let me add more detailed bitmap parsing logs.

