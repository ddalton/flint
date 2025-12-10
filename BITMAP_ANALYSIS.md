# NFSv4 Bitmap Analysis - Flint vs Longhorn

## Flint Server Logs

From the Flint server logs, we see:
```
GETATTR: Requested attributes: {1, 3, 4, 8, 20, 33, 35, 36, 37, 41, 45, 47, 52, 53, 55}
GETATTR: Returning 116 bytes of attributes
Requested attrs: [1048858, 11575866]
Returned bitmap: [1048858, 11575866]
```

## Bitmap Decoding

### Decimal to Hex Conversion
- `1048858` (decimal) = `0x0010011A` (hex)
- `11575866` (decimal) = `0x00B0A23A` (hex)

### Bitmap Word 0: 0x0010011A

Binary: `0000 0000 0001 0000 0000 0001 0001 1010`

Bits set (0-indexed from right):
- Bit 1: FATTR4_TYPE (1)
- Bit 3: FATTR4_FH_EXPIRE_TYPE (3)  
- Bit 4: FATTR4_CHANGE (4)
- Bit 8: FATTR4_FSID (8)
- Bit 12: FATTR4_ACLSUPPORT (12)
- Bit 20: FATTR4_FILEID (20)

### Bitmap Word 1: 0x00B0A23A

Binary: `0000 0000 1011 0000 1010 0010 0011 1010`

Bits set in word 1 (add 32 to bit position):
- Bit 1 → Attr 33: FATTR4_MODE
- Bit 3 → Attr 35: FATTR4_CANSETTIME
- Bit 4 → Attr 36: FATTR4_OWNER
- Bit 5 → Attr 37: FATTR4_OWNER_GROUP
- Bit 9 → Attr 41: FATTR4_MAXLINK
- Bit 13 → Attr 45: FATTR4_MAXNAME
- Bit 15 → Attr 47: FATTR4_SPACE_AVAIL
- Bit 20 → Attr 52: FATTR4_TIME_METADATA
- Bit 21 → Attr 53: FATTR4_TIME_MODIFY
- Bit 23 → Attr 55: FATTR4_MOUNTED_ON_FILEID

## Verification Against Flint Logs

Flint logs say: `{1, 3, 4, 8, 20, 33, 35, 36, 37, 41, 45, 47, 52, 53, 55}`

Let's verify:
- ✅ 1 (TYPE) - Bit 1 in word 0
- ✅ 3 (FH_EXPIRE_TYPE) - Bit 3 in word 0  (WAIT - should this be attribute 2 or 3?)
- ✅ 4 (CHANGE) - Bit 4 in word 0
- ✅ 8 (FSID) - Bit 8 in word 0
- ✅ 20 (FILEID) - Bit 20 in word 0
- ✅ 33 (MODE) - Bit 1 in word 1
- ✅ 35 (CANSETTIME) - Bit 3 in word 1
- ✅ 36 (OWNER) - Bit 4 in word 1
- ✅ 37 (OWNER_GROUP) - Bit 5 in word 1
- ✅ 41 (MAXLINK) - Bit 9 in word 1
- ✅ 45 (MAXNAME) - Bit 13 in word 1
- ✅ 47 (SPACE_AVAIL) - Bit 15 in word 1
- ✅ 52 (TIME_METADATA) - Bit 20 in word 1
- ✅ 53 (TIME_MODIFY) - Bit 21 in word 1
- ✅ 55 (MOUNTED_ON_FILEID) - Bit 23 in word 1

## 🔍 Wait - Bitmap Decoding Issue!

Let me re-check bit 3 in word 0 (0x0010011A):

```
0x0010011A = 0b 0000 0000 0001 0000 0000 0001 0001 1010
                  ^bit31                             ^bit0
```

Reading from right to left (LSB = bit 0):
- Bit 0: 0
- Bit 1: 1 ✅ → Attribute 1 (TYPE)
- Bit 2: 0
- Bit 3: 1 ✅ → Attribute 3 (CHANGE?)
- Bit 4: 1 ✅ → Attribute 4 (SIZE?)
- Bit 5: 0
...
- Bit 8: 1 ✅ → Attribute 8 (FSID)
...
- Bit 12: 1 ✅ → Attribute 12 (ACLSUPPORT)
...
- Bit 20: 1 ✅ → Attribute 20 (FILEID)

**WAIT - The client is asking for attribute 3, not 2!**

But our logs show the client requested: `{1, 3, 4, 8, 20, ...}`

This means:
- Attribute 1 = TYPE ✅
- Attribute 3 = CHANGE (not FH_EXPIRE_TYPE!)
- Attribute 4 = SIZE (not CHANGE!)

## 🚨 CRITICAL ERROR FOUND!

Our attribute constants are OFF BY ONE for attributes 2-4!

According to RFC 7530 Section 5.8:

```
Attribute ID | Name
-------------|-------------------
0            | SUPPORTED_ATTRS
1            | TYPE
2            | FH_EXPIRE_TYPE
3            | CHANGE
4            | SIZE
5            | LINK_SUPPORT
...
```

But our code has:
```rust
const FATTR4_TYPE: u32 = 1;              // ✅ Correct
const FATTR4_FH_EXPIRE_TYPE: u32 = 2;   // ✅ Correct
const FATTR4_CHANGE: u32 = 3;            // ✅ Correct
const FATTR4_SIZE: u32 = 4;              // ✅ Correct
```

Wait, our constants ARE correct!

So the client IS requesting:
- Attribute 1 (TYPE) ✅
- Attribute 3 (CHANGE) ✅  
- Attribute 4 (SIZE) ✅
- Attribute 8 (FSID) ✅
- Attribute 12 (ACLSUPPORT) ✅
- Attribute 20 (FILEID) ✅
- ...

But the Flint log says: `{1, 3, 4, 8, 20, 33, 35, 36, 37, 41, 45, 47, 52, 53, 55}`

Hmm, but that doesn't include attribute 12 (ACLSUPPORT)! Let me recount the bits...

## Re-Bitmap Analysis

0x0010011A = 0b00000000000100000000000100011010

Let me count more carefully from bit 0 (rightmost):

```
Position: 31 30 29 28 27 26 25 24 23 22 21 20 19 18 17 16 15 14 13 12 11 10  9  8  7  6  5  4  3  2  1  0
Bit:       0  0  0  0  0  0  0  0  0  0  0  1  0  0  0  0  0  0  0  0  0  0  0  1  0  0  0  1  1  0  1  0

Bits set: 20, 8, 4, 3, 1
```

So word 0 has bits: {1, 3, 4, 8, 20} ✅ Matches!

For word 1 (0x00B0A23A = 0b00000000101100001010001000111010):

```
Position: 31 30 29 28 27 26 25 24 23 22 21 20 19 18 17 16 15 14 13 12 11 10  9  8  7  6  5  4  3  2  1  0  
Bit:       0  0  0  0  0  0  0  0  1  0  1  1  0  0  0  0  1  0  1  0  0  0  1  0  0  0  1  1  1  0  1  0

Bits set: 23, 21, 20, 17, 13, 9, 5, 4, 3, 1
```

Word 1 bits: {1, 3, 4, 5, 9, 13, 17, 20, 21, 23}

Adding 32 to get attribute IDs: {33, 35, 36, 37, 41, 45, 49, 52, 53, 55}

But Flint logs show: {33, 35, 36, 37, 41, 45, 47, 52, 53, 55}

**DISCREPANCY FOUND!**

Flint logs show attribute 47 (SPACE_AVAIL), but the bitmap shows bit 17 → attribute 49 (SPACE_TOTAL)!

## 🎯 THE BUG

Either:
1. Our bitmap parsing is wrong, OR
2. Our logging of "Requested attributes" is wrong

Let me check: is bit 15 or bit 17 set in 0x00B0A23A?

```
0x00B0A23A = 0b00000000101100001010001000111010
                     bit17^  ^bit15
```

- Bit 15: 1 ✅ → Attribute 47 (SPACE_AVAIL)
- Bit 17: 1 ✅ → Attribute 49 (SPACE_TOTAL)

**BOTH are set!**

So the correct attribute list should be: {33, 35, 36, 37, 41, 45, 47, 49, 52, 53, 55}

But Flint log says: {33, 35, 36, 37, 41, 45, 47, 52, 53, 55}

**Missing attribute 49 (SPACE_TOTAL) in our encoding!**

## Hypothesis

We're not encoding SPACE_TOTAL (49) even though the client requests it!

Let me check our code...

Looking at fileops.rs line 637:
```rust
FATTR4_SPACE_TOTAL => {
    // Total space (1TB)
    buf.put_u64(1024 * 1024 * 1024 * 1024);
    true
}
```

We DO encode it! So why is it missing from the log?

## Next Step

Need to add debug logging to see if `encode_single_attribute` is actually being called for attribute 49.

The client is definitely requesting it (bit 17 in word 1 is set).

We should be encoding it (we have a match arm for it).

But the Flint log suggests we're not returning it in the "Requested attributes" set.

This suggests our bitmap parsing code (`encode_attributes` function) might have a bug in how it extracts attribute IDs from the bitmap.

