# pNFS Parallel I/O - Root Cause Analysis

**Date**: December 18, 2025  
**Status**: 🔍 ROOT CAUSE IDENTIFIED

## Problem

pNFS is NOT achieving parallel I/O. All writes go through the MDS instead of being striped across the 2 Data Servers.

**Performance**:
- **Baseline (Standalone NFS)**: 70.7 MB/s ✅
- **pNFS with 2 DSs**: 57.1 MB/s ❌ (SLOWER than baseline!)

## Root Cause

**GETDEVICEINFO is failing with status=-5 (EIO)**, causing layouts to be immediately discarded.

### Evidence from Client Debug Logs

```
[347512.177504] --> pnfs_alloc_init_layoutget_args
[347512.177534] encode_layoutget: 1st type:0x1 iomode:2 off:0 len:18446744073709551615 mc:4096
[347512.178520] decode_layoutget roff:0 rlen:18446744073709551615 riomode:2, lo_type:0x1, lo.len:96
[347512.178552] pnfs_find_alloc_layout Begin ino=00000000cb76a30e layout=0000000000000000
[347512.178555] --> filelayout_alloc_lseg
[347512.178559] filelayout_decode_layout: nfl_util 0x800000 num_fh 1 fsi 0 po 0
[347512.178562] --> filelayout_check_layout
[347512.178563] --> filelayout_check_layout returns 0
[347512.178564] pnfs_generic_layout_insert_lseg:Begin
[347512.178565] pnfs_generic_layout_insert_lseg: inserted lseg 000000000e085745 iomode 2 offset 0 length 18446744073709551615 at tail
[347512.178931] pnfs_find_alloc_layout Begin ino=00000000cb76a30e layout=00000000b2ddd0fc
[347512.178934] pnfs_update_layout: inode 0:642/2167939 pNFS layout segment found for (read/write, offset: 0, length: 4096)
[347512.179428] pnfs_layout_io_set_failed Setting layout IOMODE_RW fail bit  ❌❌❌
[347512.179434] <-- nfs4_proc_layoutreturn status=0
[347512.179435] <-- pnfs_send_layoutreturn status: 0
```

And earlier:
```
[347435.553187] <-- _nfs4_proc_getdeviceinfo status=-5  ❌❌❌
```

### What's Happening

1. ✅ Client sends LAYOUTGET → MDS
2. ✅ MDS returns layout with device IDs
3. ✅ Client decodes layout successfully
4. ✅ Client inserts layout segment
5. ❌ **GETDEVICEINFO fails with status=-5 (EIO)**
6. ❌ Client marks layout as failed: `pnfs_layout_io_set_failed`
7. ❌ Client returns layout to MDS
8. ❌ Client falls back to MDS for all I/O

## Mystery: Where is GETDEVICEINFO Failing?

**The MDS logs show NO GETDEVICEINFO operations!**

This means one of:
1. Client isn't sending GETDEVICEINFO to the MDS
2. GETDEVICEINFO is being sent but not received
3. There's a protocol/network issue preventing delivery

## Client Mount Statistics

```
device 10.43.47.65:/ mounted on /mnt/pnfs with fstype nfs4
opts:	rw,vers=4.1,rsize=131072,wsize=131072,...
caps:	caps=0x800380b7,wtmult=512,dtsize=131072,bsize=0,namlen=255
nfsv4:	bm0=0xf8f3b77e,bm1=0x40b0be3a,bm2=0x2,acl=0x0,sessions,pnfs=LAYOUT_NFSV4_1_FILES ✅

RPC iostats:
        NULL: 1 1 0 44 24 0 0 0 0
       WRITE: 1200 1200 0 157521600 134400 6359 1763 8128 0  ❌ (All writes to MDS!)
      COMMIT: 150 150 0 26400 15600 0 813 815 0
```

**Note**: All 1200 WRITE operations went to the MDS (10.43.47.65), NOT to the DSs!

## Network Verification

✅ Client can ping both DSs:
- DS1: 10.42.214.21 (cdrv-1) - reachable
- DS2: 10.42.50.85 (cdrv-2) - reachable

✅ Client can connect to DS NFS ports:
```
10.42.214.21 (10.42.214.21:2049) open ✅
10.42.50.85 (10.42.50.85:2049) open ✅
```

## Code Analysis

### Device ID Generation

**DeviceInfo::generate_binary_id()**:
```rust
fn generate_binary_id(device_id: &str) -> DeviceId {
    let mut hasher = DefaultHasher::new();
    device_id.hash(&mut hasher);
    let hash = hasher.finish();
    
    let mut binary_id = [0u8; 16];
    binary_id[0..8].copy_from_slice(&hash.to_be_bytes());
    binary_id[8..16].copy_from_slice(&hash.to_be_bytes());
    binary_id
}
```

**Layout encoding (dispatcher.rs)**:
```rust
// Convert device_id string to 16-byte binary format
let mut hasher = DefaultHasher::new();
segment.device_id.hash(&mut hasher);
let hash = hasher.finish();
// ... same hashing
```

✅ Both use the same hashing algorithm, so device IDs **should match**.

### GETDEVICEINFO Handler

```rust
pub fn getdeviceinfo(&self, args: GetDeviceInfoArgs) 
    -> Result<GetDeviceInfoResult, GetDeviceInfoError> 
{
    // Validate layout type
    if args.layout_type != LayoutType::NfsV4_1Files {
        return Err(GetDeviceInfoError::UnknownLayoutType);
    }
    
    // Look up device
    let device_info = self.device_registry
        .get_by_binary_id(&args.device_id)
        .ok_or_else(|| {
            warn!("Device not found: {:?}", &args.device_id[0..4]);
            GetDeviceInfoError::NoEnt
        })?;
    
    // Build device address
    Ok(GetDeviceInfoResult {
        device_addr: DeviceAddr4 {
            netid: "tcp".to_string(),
            addr: device_info.primary_endpoint.clone(),
            multipath: device_info.endpoints.clone(),
        },
        notification: Vec::new(),
    })
}
```

✅ Code looks correct

## Registered Devices

```
MDS logs show:
✅ DS registered successfully: cdrv-1.vpc.cloudera.com-ds
✅ DS registered successfully: cdrv-2.vpc.cloudera.com-ds
✅ Heartbeat received from device: cdrv-1.vpc.cloudera.com-ds
✅ Heartbeat received from device: cdrv-2.vpc.cloudera.com-ds
```

## Next Steps to Debug

1. **Enable MDS RPC-level debug** - See if GETDEVICEINFO requests are arriving
2. **Check GETDEVICEINFO encoding/decoding** - Verify wire format is correct
3. **Tcpdump the GETDEVICEINFO packets** - See actual bytes on wire
4. **Add explicit GETDEVICEINFO logging** - Instrument every step
5. **Check if client is caching failed GETDEVICEINFO** - Client may have blacklisted the operation

## Hypothesis

The most likely scenario is:
1. Client sends GETDEVICEINFO to MDS
2. MDS receives it but there's an encoding/decoding error
3. MDS returns GARBAGE_ARGS or similar error
4. Client interprets this as EIO (-5)
5. Client gives up on pNFS

OR:
1. GETDEVICEINFO is malformed from client
2. Doesn't match expected XDR format
3. MDS rejects at RPC layer (before handler)
4. Client sees connection error as EIO

## Files to Investigate

- `src/nfs/v4/compound.rs` - GETDEVICEINFO decoding (line 1186-1198)
- `src/nfs/v4/dispatcher.rs` - GETDEVICEINFO dispatch (line 818-1043)
- `src/pnfs/mds/operations/mod.rs` - GETDEVICEINFO handler (line 99-134)
- `src/pnfs/protocol.rs` - Device address encoding (line 406-427)

---

## Success Criteria

When pNFS parallel I/O is working, we should see:

**Client Kernel Logs**:
```
pnfs_update_layout: layout found
--> nfs4_proc_getdeviceinfo
<-- nfs4_proc_getdeviceinfo status=0  ✅
filelayout_choose_ds_for_read: using device X
NFS: direct write to DS (not MDS)
```

**Performance**:
- **pNFS with 2 DSs**: ~140 MB/s (2x baseline) ✅

**RPC Stats**:
- Most WRITE operations to DS IPs, not MDS IP
- LAYOUTGET: successful
- GETDEVICEINFO: successful  
- Writes distributed across DSs

---

**Status**: Waiting for deeper debugging to understand why GETDEVICEINFO is failing

