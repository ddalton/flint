# RFC 7530/7862 GETATTR Requirements Analysis

## RFC 7530 Section 16.7 - GETATTR Operation

### GETATTR XDR Structure

```c
struct GETATTR4args {
    bitmap4 attr_request;
};

struct GETATTR4resok {
    fattr4 obj_attributes;
};

union GETATTR4res switch (nfsstat4 status) {
 case NFS4_OK:
    GETATTR4resok resok4;
 default:
    void;
};
```

### fattr4 Structure

```c
struct fattr4 {
    bitmap4     attrmask;    // Bitmap of attrs being returned
    attrlist4   attr_vals;   // XDR-encoded attribute values
};

typedef opaque attrlist4<>;
typedef uint32_t bitmap4<>;
```

## Key RFC Requirements

### 1. Attribute Bitmap Response

**RFC 7530 Section 5.8:**
> The server **MUST** return a bitmap representing the list of attributes
> successfully retrieved for the object.

**Our Implementation:** ✅ We return bitmap `[0x0010011a, 0x00b0a23a]` matching requested attributes.

### 2. Attribute Encoding Order

**RFC 7530 Section 5.8:**
> If the server supports an attribute on the target object but it cannot
> obtain the attribute's value, the server **MUST NOT** include the attribute
> in the result attribute bitmap.

> The attribute values returned **MUST** be in the same order as the
> corresponding attributes in the returned bitmap.

**Our Implementation:** ✅ We encode attributes in bitmap order.

### 3. XDR Encoding Rules (RFC 4506)

#### Strings (OWNER, OWNER_GROUP)
```c
string owner<>;  // opaque<> with length prefix

Encoding:
  length: 4 bytes (u32)
  data: variable
  padding: to 4-byte boundary
```

**Example:**
- "0" = length(1) + "0"(1 byte) + padding(3) = 8 bytes total
- "root" = length(4) + "root"(4 bytes) + padding(0) = 8 bytes total

**Our Implementation:** ✅ We use proper XDR string encoding.

#### Time Values (nfstime4)
```c
struct nfstime4 {
    int64_t seconds;   // 8 bytes (signed)
    uint32_t nseconds; // 4 bytes
};
// Total: 12 bytes
```

**Our Implementation:** ✅ We encode times as i64 + u32 = 12 bytes each.

#### 64-bit Integers (FILEID, SIZE, etc.)
```c
uint64  // 8 bytes, big-endian
```

**Our Implementation:** ✅ Using `buf.put_u64()` which is big-endian.

## Potential Issues from RFC Perspective

### Issue 1: FATTR4_SUPPORTED_ATTRS (Attribute 0)

**RFC 7530 Section 5.8:**
> FATTR4_SUPPORTED_ATTRS is a REQUIRED attribute in the NFSv4 protocol.

**Question:** Should we ALWAYS return SUPPORTED_ATTRS even if not requested?

Looking at our logs:
- Client requests: `{1, 3, 4, 8, 20, 33, 35, 36, 37, 41, 45, 47, 52, 53, 55}`
- SUPPORTED_ATTRS (0) is **NOT** requested
- We don't return it

**Is this correct?** 

Actually, RFC 7530 Section 5.1 says:
> "REQUIRED" attributes **MUST** be supported by all NFSv4 servers.

This means the server must SUPPORT them, not that they must always be RETURNED.

### Issue 2: RECOMMENDED vs REQUIRED Attributes

**RFC 7530 Section 5:**

**REQUIRED attributes (must be supported):**
- FATTR4_SUPPORTED_ATTRS (0)
- FATTR4_TYPE (1) ✅ We return this
- FATTR4_FH_EXPIRE_TYPE (2)
- FATTR4_CHANGE (3) ✅ We return this
- FATTR4_SIZE (4) ✅ We return this
- FATTR4_LINK_SUPPORT (5)
- FATTR4_SYMLINK_SUPPORT (6)
- FATTR4_NAMED_ATTR (7)
- FATTR4_FSID (8) ✅ We return this
- FATTR4_UNIQUE_HANDLES (9)
- FATTR4_LEASE_TIME (10)
- FATTR4_RDATTR_ERROR (11)
- FATTR4_FILEHANDLE (19)
- FATTR4_FILEID (20) ✅ We return this

**RECOMMENDED attributes:**
- FATTR4_ACL (12)
- FATTR4_ACLSUPPORT (12)
- FATTR4_ARCHIVE (34)
- FATTR4_CANSETTIME (35) ✅ We return this
- ... many others

The client only requests what it needs, and we return only what was requested. This should be correct.

### Issue 3: Attribute Value Correctness

Let me check if any of our values might be wrong:

#### FSID Encoding
```c
struct fsid4 {
    uint64_t major;
    uint64_t minor;
};
```

**Our encoding:**
```
00 00 00 00 00 00 00 00  ← major = 0
00 00 00 01              ← WAIT! Only 4 bytes for minor?
```

**🚨 POTENTIAL BUG FOUND!**

Looking at our hex dump:
```
[0014]: 00 00 00 00 00 00 00 00 00 00 00 01
```

That's only 12 bytes for FSID, but it should be 16 bytes (8+8)!

Let me verify by counting bytes from the start:
- Offset 0x14 (20 decimal) = after TYPE(4) + CHANGE(8) + SIZE(8) = 20 ✅
- FSID should be 16 bytes
- But our dump shows we're at offset 0x24 after FSID
- 0x24 - 0x14 = 16 bytes ✅ Actually it's correct!

Let me recount more carefully...

Actually, looking at the hex dump line:
```
[0020]: 00 00 00 01 00 00 00 00 00 20 3f f9 00 00 01 ff
```

The `00 00 00 01` at 0x20 might be the end of FSID (minor), not the start!

Let me create a proper byte map:

```
Offset | Value | Meaning
-------|-------|-------------------
0x00   | 00 00 00 02 | TYPE = 2
0x04   | 00 00 00 00 69 3a 08 01 | CHANGE
0x0C   | 00 00 00 00 00 00 10 00 | SIZE = 0x1000
0x14   | 00 00 00 00 00 00 00 00 | FSID major = 0
0x1C   | 00 00 00 00 00 00 00 01 | FSID minor = 1
```

Wait, that's `0x1C - 0x14 = 8 bytes` for FSID major, then next 8 bytes for minor.
But looking at the hex dump:
```
[0010]: 00 00 10 00 00 00 00 00 00 00 00 00 00 00 00 00
[0020]: 00 00 00 01 00 00 00 00 00 20 3f f9 00 00 01 ff
```

Offset 0x10 has the end of SIZE and start of FSID...

This is getting confusing. Let me check our actual FSID encoding code:

### Issue 4: Checking Our FSID Implementation

From `fileops.rs`:
```rust
FATTR4_FSID => {
    // Filesystem ID - major and minor (8 bytes each)
    buf.put_u64(0); // major
    buf.put_u64(1); // minor
    true
}
```

This is **16 bytes total** which is correct!

But our logs say:
```
Encoded attr 8: 16 bytes (total now: 36)
```

That's correct: 4 (TYPE) + 8 (CHANGE) + 8 (SIZE) + 16 (FSID) = 36 bytes ✅

## So Where Are The Missing 12 Bytes?

Since our encoding appears correct according to RFC, the 12-byte difference with Longhorn must be:

### Hypothesis A: Longhorn Returns Extra Attributes

Even though the client requests 15 attributes, maybe Longhorn returns MORE?

But Longhorn's bitmap should show what it's returning. If it returns the same bitmap as us, but with 12 more bytes, then...

### Hypothesis B: Different String Lengths

**Our OWNER/OWNER_GROUP:**
- Length: 1
- String: "0"
- Total: 8 bytes each (16 total)

**If Longhorn uses longer strings:**
- "root" (4 chars) = still 8 bytes
- "nobody" (6 chars + 2 pad) = 4 + 6 + 2 = 12 bytes!

If both OWNER and OWNER_GROUP are "nobody" (6 chars each):
- Our version: 8 + 8 = 16 bytes
- Longhorn version: 12 + 12 = 24 bytes
- Difference: 8 bytes!

Plus if there's another 4-byte difference somewhere, that's our 12 bytes!

### Hypothesis C: RDATTR_ERROR Attribute

**RFC 7530 Section 5.8:**
> FATTR4_RDATTR_ERROR (11) is a REQUIRED attribute that indicates whether
> an error occurred attempting to retrieve the requested attributes.

Maybe Longhorn returns this even though it's not requested? That would be 4 bytes.

Plus the 8 bytes from longer strings = 12 bytes!

## Conclusion

**Most Likely Issue:** String lengths differ (OWNER/OWNER_GROUP)

We're encoding "0" (1 char) but Longhorn might be encoding actual user/group names like:
- "nobody" (6 chars) = 12 bytes instead of 8
- "root" (4 chars) = 8 bytes (same)
- "1000" (4 chars) = 8 bytes (same)

**Action:** Change our OWNER/OWNER_GROUP encoding to return longer strings to match Longhorn.

