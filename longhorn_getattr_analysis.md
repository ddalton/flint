# Longhorn GETATTR Response Analysis

## Raw Packet Data

From tcpdump capture of successful Longhorn mount:

```
23:23:26.298171 In IP 10.42.239.160.2049 > 10.65.171.171.18513: 
NFS reply xid 1978317062 reply ok 380 getattr NON 4
```

### Full Hex Dump (384 bytes TCP payload)

```
0x0040:  7bde 6c22 a045 60cc 8000 017c 75ea b906  {.l".E`....|u...
0x0050:  0000 0001 0000 0000 0000 0000 0000 0000  ................
0x0060:  0000 0000 0000 0000 0000 0000 0000 0004  ................
0x0070:  0000 0035 0000 0000 0400 0000 7bc2 3969  ...5........{.9i
0x0080:  0200 0000 0000 0000 0000 02dd 0000 0000  ................
0x0090:  0000 003f 0000 003f 0000 0000 0000 0018  ...?...?........
0x00a0:  0000 0000 0000 000a 0000 0000 0000 0080  ................
0x00b0:  4300 0000 7bcc 3946 2664 c9fa c301 002f  C...{.9F&d...../
0x00c0:  0000 0000 0000 0000 0000 0000 0000 0000  ................
0x00d0:  0000 0000 0000 0000 0000 0000 0000 0000  ................
0x00e0:  0000 0000 0000 0000 0000 0000 0000 0000  ................
0x00f0:  0000 0000 0000 0000 0000 0000 0000 0000  ................
0x0100:  0000 0000 0000 0000 0000 0000 0000 0000  ................
0x0110:  0000 0000 0000 0000 0000 0000 0000 0000  ................
0x0120:  0000 0000 0000 0000 0000 0000 0000 0000  ................
0x0130:  0000 0009 0000 0000 0000 0002 0010 011a  ................
0x0140:  00b0 a23a 0000 0080 0000 0002 187f ef98  ...:............
0x0150:  f24e 7d1b 0000 0000 0000 0000 0000 0000  .N}.............
0x0160:  0000 0098 0000 0000 0000 0098 0000 0000  ................
0x0170:  0000 0000 0000 01ed 0000 0003 0000 0001  ................
0x0180:  3000 0000 0000 0001 3000 0000 0000 0000  0.......0.......
0x0190:  0000 0000 0000 0000 0000 0000 0000 0000  ................
0x01a0:  6939 c27b 0783 7990 0000 0000 6939 c27b  i9.{..y.....i9.{
0x01b0:  07db 6f1b 0000 0000 6939 c27b 07db 6f1b  ..o.....i9.{..o.
0x01c0:  0000 0000 0000 0000                      ........
```

## Byte-by-Byte Parsing

### RPC Header
```
Offset 0x0040-0x004F: TCP/IP headers (not relevant)
Offset 0x0050: 8000 017c = RPC marker (last fragment, 380 bytes)
Offset 0x0054: 75ea b906 = XID (1978317062)
```

### RPC Reply
```
Offset 0x0058: 0000 0001 = MSG_ACCEPTED
Offset 0x005C: 0000 0000 = Verifier flavor (AUTH_NULL)
Offset 0x0060: 0000 0000 = Verifier length (0)
Offset 0x0064: 0000 0000 = Accept status (SUCCESS)
```

### COMPOUND Response
```
Offset 0x0068: 0000 0000 = Tag length (0)
Offset 0x006C: 0000 0004 = Number of operations (4)
```

### Operation Results

#### Result #0: SEQUENCE (opcode 53 = 0x35)
```
Offset 0x0070: 0000 0035 = Opcode 53 (SEQUENCE)
Offset 0x0074: 0000 0000 = Status NFS4_OK
Offset 0x0078: 0400 0000 = Session ID (first 4 bytes)
Offset 0x007C: 7bc2 3969 = Session ID continued
... (16 bytes total for session ID)
Offset 0x0088: 0200 0000 = Sequence ID (2)
Offset 0x008C: 0000 0000 = Slot ID (0)
Offset 0x0090: 0000 02dd = Highest slot ID (733)
Offset 0x0092: 0000 0000 = Target highest slot ID (0)
```

#### Result #1: PUTROOTFH (opcode 24)
```
Offset 0x0094: 0000 003f = Opcode? Wait, 0x3f = 63...
```

**WAIT!** Let me re-parse this. The offset is getting confused.

Let me start fresh from the COMPOUND response start:

### Clean Parse from COMPOUND Start

After RPC header (24 bytes), COMPOUND starts at offset in the payload:

```
Status: 00 00 00 00 (NFS4_OK)
Tag length: 00 00 00 00 (0 bytes)
Op count: 00 00 00 04 (4 operations)

--- Result #0 ---
Opcode: 00 00 00 35 (53 = SEQUENCE)
Status: 00 00 00 00 (NFS4_OK)
... session data ...

--- Result #1 ---  
Opcode: 00 00 00 18 (24 = PUTROOTFH)
Status: 00 00 00 00 (NFS4_OK)

--- Result #2 ---
Opcode: 00 00 00 0a (10 = GETFH)
Status: 00 00 00 00 (NFS4_OK)
FH length: 00 00 00 80 (128 bytes! - Not 50!)
... filehandle data ...

--- Result #3 ---
Opcode: 00 00 00 09 (9 = GETATTR)
Status: 00 00 00 00 (NFS4_OK)
Bitmap word count: 00 00 00 02 (2 words)
Bitmap[0]: 00 10 01 1a
Bitmap[1]: 00 b0 a2 3a
Attr vals length: 00 00 00 80 (128 bytes of attributes)
... attribute data ...
```

## 🔍 KEY FINDINGS

### Finding #1: Filehandle Size
**Longhorn uses 128-byte filehandles, not 50 bytes!**

```
Longhorn: 00 00 00 80 = 128 bytes
Flint:    00 00 00 32 = 50 bytes
```

### Finding #2: Bitmap Decoding

**Bitmap[0] = 0x0010011a = 0b 0000 0000 0001 0000 0000 0001 0001 1010**

Bits set in word 0:
- Bit 1: TYPE
- Bit 3: FH_EXPIRE_TYPE  
- Bit 4: CHANGE
- Bit 8: FSID
- Bit 12: ACLSUPPORT
- Bit 20: FILEID

**Bitmap[1] = 0x00b0a23a = 0b 0000 0000 1011 0000 1010 0010 0011 1010**

Bits set in word 1 (add 32 to get attribute ID):
- Bit 1 (33): MODE
- Bit 3 (35): CANSETTIME
- Bit 4 (36): CASE_INSENSITIVE
- Bit 5 (37): CASE_PRESERVING
- Bit 9 (41): MAXLINK
- Bit 13 (45): MAXNAME
- Bit 17 (49): SPACE_AVAIL
- Bit 21 (53): SPACE_FREE
- Bit 22 (54): SPACE_TOTAL
- Bit 23 (55): SPACE_USED
- Bit 28 (60): TIME_ACCESS
- Bit 29 (61): TIME_CREATE
- Bit 30 (62): TIME_DELTA
- Bit 31 (63): TIME_METADATA

### Finding #3: Attribute Value Analysis

Starting at offset 0x0140, we have 128 bytes (0x80) of attribute values.

Let me decode them in order:

```
Offset 0x0144: 00 00 00 02 = TYPE = 2 (directory)
Offset 0x0148: 18 7f ef 98 = FH_EXPIRE_TYPE? or CHANGE?
...
```

Actually, this is getting complex. Let me examine the request to see what was asked for:

## GETATTR Request Analysis

Looking back at the request:
```
0x00b0:  0000 0035 0400 0000 7bc2 3969 0200 0000  ...5....{.9i....
```

This shows:
```
Opcode: 00 00 00 35 (SEQUENCE)
...then compound operations...

For GETATTR operation:
Opcode: 00 00 00 09 (GETATTR)
Requested bitmap word count: 00 00 00 02
Requested bitmap[0]: 00 10 01 1a
Requested bitmap[1]: 00 b0 a2 3a
```

## 🎯 CRITICAL ISSUE IDENTIFIED

Looking at our Flint logs, we return:
```
Requested attrs: [1048858, 11575866]
Returned bitmap: [1048858, 11575866]
```

Converting to hex:
- 1048858 (decimal) = 0x0010011A (hex) ✅ MATCHES LONGHORN REQUEST
- 11575866 (decimal) = 0x00B0A23A (hex) ✅ MATCHES LONGHORN REQUEST

**So our bitmap is CORRECT!**

But let's check the attribute values...

## Attribute Value Encoding Order

Based on bitmap 0x0010011A, 0x00B0A23A, attributes should be encoded in this order:

1. TYPE (1)
2. FH_EXPIRE_TYPE (3)  ❓ We encode this as attribute 2, not 3!
3. CHANGE (4)
4. FSID (8)
5. ACLSUPPORT (12)
6. FILEID (20)
7. MODE (33)
8. CANSETTIME (35)
9. CASE_INSENSITIVE (36)  ❓ We have as attribute 39!
10. CASE_PRESERVING (37)  ❓ We have as attribute 40!
11. MAXLINK (41)
12. MAXNAME (45)
13. SPACE_AVAIL (49)  ❓ We have as attribute 47!
14. SPACE_FREE (53)  ❓ We have as attribute 48!
15. SPACE_TOTAL (54)  ❓ We have as attribute 49!
16. SPACE_USED (55)  ❓ We have as attribute 50!
17. TIME_ACCESS (60)  ❓ We have as attribute 51!
18. TIME_CREATE (61)
19. TIME_DELTA (62)
20. TIME_METADATA (63)  ❓ We have as attribute 52!

## 🚨 ROOT CAUSE FOUND!

**Our attribute constants are WRONG!**

We're using incorrect attribute IDs. Let me check RFC 7530 Section 5.8 for the correct attribute numbers:

According to RFC 7530:
- FATTR4_SUPPORTED_ATTRS = 0
- FATTR4_TYPE = 1
- FATTR4_FH_EXPIRE_TYPE = 2 ✅
- FATTR4_CHANGE = 3 ✅
- FATTR4_SIZE = 4 ✅
...
- FATTR4_OWNER = 36 ✅
- FATTR4_OWNER_GROUP = 37 ✅
- FATTR4_SPACE_AVAIL = 47 ✅  
- FATTR4_SPACE_FREE = 48 ✅
- FATTR4_SPACE_TOTAL = 49 ✅
- FATTR4_SPACE_USED = 50 ✅
- FATTR4_TIME_ACCESS = 51 ✅
- FATTR4_TIME_METADATA = 52 ✅
- FATTR4_TIME_MODIFY = 53 ✅

Wait, those look correct actually. Let me re-decode the bitmap more carefully...

## Bitmap Re-Analysis

Bitmap[1] = 0x00b0a23a

Let me convert this carefully:
```
0x00b0a23a = 0b00000000101100001010001000111010
```

Reading from right to left (bit 0 is rightmost):
- Bit 1: YES (attr 33 = MODE)
- Bit 3: YES (attr 35 = CANSETTIME)  
- Bit 4: YES (attr 36 = OWNER)
- Bit 5: YES (attr 37 = OWNER_GROUP)
- Bit 9: YES (attr 41 = ?)
- Bit 13: YES (attr 45 = ?)
- Bit 17: YES (attr 49 = ?)
- Bit 20: YES (attr 52 = TIME_METADATA)
- Bit 21: YES (attr 53 = TIME_MODIFY)
- Bit 23: YES (attr 55 = MOUNTED_ON_FILEID)

Let me check what attribute 36, 37 actually are in the spec...

Actually, I think I need to look at the actual attribute list from RFC 7862 to get the complete mapping.

## Summary

**Potential Issues Found:**
1. ✅ Bitmap encoding is correct
2. ❓ Need to verify attribute constant definitions match RFC exactly
3. ❓ Need to verify attribute value encoding format (especially strings, times)
4. ⚠️ **Filehandle size difference: 128 bytes (Longhorn) vs 50 bytes (Flint)**

**Next Step:** Verify our attribute constant definitions against RFC 7530/7862 table.

