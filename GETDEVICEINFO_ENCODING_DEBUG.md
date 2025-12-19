# GETDEVICEINFO Encoding Debug

**Date**: December 19, 2025  
**Status**: 🐛 Debugging encoding issue

## Problem

GETDEVICEINFO is returning data, but the client rejects it:
```
nfs4_fl_alloc_deviceid_node ds_num 1952673792
NFS: multipath count 1952673792 greater than supported maximum 256
```

## What We Fixed

1. ✅ **Device ID decoding**: Changed from `decode_opaque()` to `decode_fixed_opaque(16)`
2. ✅ **Device address format**: Changed to `multipath_list4` (array of netaddr4)

## Current Response (from tcpdump)

```
0x0080:  0000 002f 0000 0000  ← opcode 47, status 0 ✅
0x0090:  0000 0000 0000 007f  ← ??? Extra bytes
         0000 0000 0000 0016  
0x00a0:  0000 0000 0000 0026  ← Length 38 bytes
         0000 0000 0002       ← ???
0x00b0:  0000 0000 0000 0000 6944 c34a  
0x00c0:  0000 0009 0000 0000 0000 0002 0000 0018
```

## Expected Format (RFC 5661 Section 18.40.3)

```
struct GETDEVICEINFO4res switch (nfsstat4 gdir_status) {
 case NFS4_OK:
     GETDEVICEINFO4resok  gdir_resok4;
 default:
     void;
};

struct GETDEVICEINFO4resok {
     device_addr4         gdir_device_addr;
     bitmap4              gdir_notification;
};

struct device_addr4 {
     layouttype4          da_layout_type;
     opaque               da_addr_body<>;
};
```

So the format should be:
1. layout_type (u32)
2. da_addr_body length (u32)
3. da_addr_body bytes
4. notification bitmap length (u32)
5. notification bitmap words (if any)

## What We're Encoding

```rust
let mut encoder = XdrEncoder::new();
encoder.encode_u32(layout_type);               // ✅
encoder.encode_opaque(&dev_addr_encoded);      // ✅
encoder.encode_u32(0);  // Empty notification   // ✅
```

This should produce:
```
0000 0001   ← layout_type (LAYOUT4_NFSV4_1_FILES)
0000 00XX   ← length of device address
[device address bytes]
0000 0000   ← notification array count = 0
```

## Hypothesis

The extra bytes `0000 0000 0000 007f 0000 0000 0000 0016` between the status and the data might be coming from the COMPOUND response encoder, not the GETDEVICEINFO handler.

These bytes look like:
- `0000 0000 0000 007f` = sessionid or slot info?
- `0000 0000 0000 0016` = stateid?

This suggests the COMPOUND response structure might be wrong for GETDEVICEINFO.

## Next Steps

1. Add debug logging to show exact bytes being encoded
2. Compare with a working pNFS server (e.g., Linux knfsd) 
3. Check if COMPOUND response has extra operations being encoded
4. Verify GetDeviceInfo result encoding in compound.rs

---

**Status**: Need to verify COMPOUND response format

