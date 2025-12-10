# Byte-by-Byte Analysis: Finding the 12-Byte Gap

## Correction: ACLSUPPORT is NOT Requested!

**I was wrong!** Bit 12 is NOT set in bitmap word 0 (0x0010011a).

### Verified Bits in 0x0010011a:
- Bit 1: SET → Attribute 1 ✓
- Bit 3: SET → Attribute 3 ✓
- Bit 4: SET → Attribute 4 ✓
- Bit 8: SET → Attribute 8 ✓
- **Bit 12: NOT SET** ← ACLSUPPORT is NOT requested!
- Bit 20: SET → Attribute 20 ✓

So the client requests exactly 15 attributes, which is what we're encoding!

## The 12-Byte Mystery

**Flint:** 116 bytes  
**Longhorn:** 128 bytes  
**Gap:** 12 bytes

Since we're encoding all requested attributes, the gap must come from:
1. **Different encoding lengths** for existing attributes
2. **Longer strings** (OWNER/OWNER_GROUP in Longhorn)
3. **Different padding** or alignment

## Flint's Attribute Encoding (116 bytes)

From the hex dump:
```
[0000]: 00 00 00 02 00 00 00 00 69 3a 08 01 00 00 00 00  ← TYPE, CHANGE
[0010]: 00 00 10 00 00 00 00 00 00 00 00 00 00 00 00 00  ← SIZE, FSID
[0020]: 00 00 00 01 00 00 00 00 00 20 3f f9 00 00 01 ff  ← FSID, FILEID, MODE
[0030]: 00 00 00 01 00 00 00 01 30 00 00 00 00 00 00 01  ← CANSETTIME, OWNER
[0040]: 30 00 00 00 00 00 ff ff 00 00 00 ff 00 00 00 19  ← OWNER_GROUP, MAXLINK, MAXNAME
[0050]: 00 00 00 00 00 00 00 00 69 3a 08 01 20 51 e9 dd  ← SPACE_AVAIL, TIME_METADATA
[0060]: 00 00 00 00 69 3a 08 01 20 51 e9 dd 00 00 00 00  ← TIME_MODIFY
[0070]: 00 20 3f f9                                      ← MOUNTED_ON_FILEID
```

### Decoding Each Attribute:

| Offset | Bytes | Decoded | Attribute |
|--------|-------|---------|-----------|
| 0x00 | 00 00 00 02 | TYPE = 2 (directory) | 4 bytes |
| 0x04 | 00 00 00 00 69 3a 08 01 | CHANGE = 0x693a0801 | 8 bytes |
| 0x0C | 00 00 00 00 00 00 10 00 | SIZE = 0x1000 (4096) | 8 bytes |
| 0x14 | 00 00 00 00 00 00 00 00<br>00 00 00 01 | FSID = major:0, minor:1 | 16 bytes |
| 0x24 | 00 00 00 00 00 20 3f f9 | FILEID = 0x00203ff9 | 8 bytes |
| 0x2C | 00 00 01 ff | MODE = 0o777 | 4 bytes |
| 0x30 | 00 00 00 01 | CANSETTIME = 1 | 4 bytes |
| 0x34 | 00 00 00 01 30 00 00 00 | OWNER = len:1, "0" + pad | 8 bytes |
| 0x3C | 00 00 00 01 30 00 00 00 | OWNER_GROUP = len:1, "0" + pad | 8 bytes |
| 0x44 | 00 00 ff ff | MAXLINK = 65535 | 4 bytes |
| 0x48 | 00 00 00 ff | MAXNAME = 255 | 4 bytes |
| 0x4C | 00 00 00 19 00 00 00 00<br>00 00 00 00 | SPACE_AVAIL = 0x1900000000 | 8 bytes |
| 0x58 | 69 3a 08 01 20 51 e9 dd<br>00 00 00 00 | TIME_METADATA = secs:0x693a0801, nsecs:0x2051e9dd | 12 bytes |
| 0x64 | 69 3a 08 01 20 51 e9 dd<br>00 00 00 00 | TIME_MODIFY = secs:0x693a0801, nsecs:0x2051e9dd | 12 bytes |
| 0x70 | 00 00 00 00 00 20 3f f9 | MOUNTED_ON_FILEID = 0x00203ff9 | 8 bytes |

**Total: 116 bytes** ✓

## Where Could the 12 Extra Bytes Be?

### Hypothesis 1: Longer Strings in Longhorn

If Longhorn returns:
- OWNER: "0" (1 char + 3 pad = 4) + length (4) = 8 bytes (same as us)
- OWNER_GROUP: "0" (1 char + 3 pad = 4) + length (4) = 8 bytes (same as us)

But what if Longhorn returns numeric UIDs as strings like "1000"?
- "1000" = 4 chars + 0 pad = 4 bytes + 4 length = 8 bytes (still same!)

Or what about "0" vs "root"?
- "root" = 4 chars + 0 pad = 4 bytes + 4 length = 8 bytes (still same!)

### Hypothesis 2: Additional Padding

Maybe Longhorn adds padding between attributes?

### Hypothesis 3: Different FSID Encoding

Our FSID: `00 00 00 00 00 00 00 00 00 00 00 01` (16 bytes)
- major: 0 (8 bytes)
- minor: 1 (8 bytes) 

What if the encoding is wrong? Let me check the XDR spec...

Actually, looking at Longhorn's FSID from my earlier decode:
```
major=0 (0x0000000000000000), minor=652835028992 (0x0000009800000000)
```

That's still 16 bytes total.

### Hypothesis 4: Look at Longhorn's Actual Response Structure

From my earlier Longhorn packet analysis, the attribute values section was 128 bytes starting at a specific offset. But I had trouble parsing the strings correctly.

Let me reconsider: maybe the issue isn't the attribute values themselves, but HOW they're framed in the GETATTR response!

## Re-examining the GETATTR XDR Structure

```
GETATTR response:
- status: u32 (4 bytes)
- bitmap_len: u32 (4 bytes)
- bitmap[]: u32[] (bitmap_len * 4 bytes)
- attr_vals_len: u32 (4 bytes)
- attr_vals: opaque<> (attr_vals_len bytes)
```

Both should have:
- status: 4 bytes
- bitmap_len: 4 bytes (value: 2)
- bitmap[0]: 4 bytes
- bitmap[1]: 4 bytes
- attr_vals_len: 4 bytes
- attr_vals: 116 or 128 bytes

Total overhead: 24 bytes

So the difference is purely in the `attr_vals` content!

## Next Step: Direct Packet Capture Comparison

We need to capture Flint's actual NFS response packet and compare it byte-by-byte with Longhorn's packet to find where the 12 bytes differ.

The difference might be in:
1. String encoding (though unlikely based on analysis)
2. Time field encoding (12 bytes each - maybe ours are wrong format?)
3. Some subtle XDR encoding difference
4. An attribute that's being encoded twice or skipped

**Action:** Need to capture raw packet data from Flint and compare with Longhorn packet.

