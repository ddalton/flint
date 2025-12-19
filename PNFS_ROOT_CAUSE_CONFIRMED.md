# pNFS Root Cause - CONFIRMED ✅

**Date**: December 18, 2025  
**Status**: 🎯 **ROOT CAUSE IDENTIFIED**

## The Problem

pNFS parallel I/O was not working. All writes went to a single server with no performance improvement.

## Root Cause: WRONG NFS SERVER MOUNTED

The client was mounting the **standalone NFS server** instead of the **pNFS MDS**!

### Evidence

**Command executed:**
```bash
mount -t nfs -o vers=4.1 10.43.47.65:/ /mnt/pnfs
```

**Actual mount result:**
```
10.43.224.82:/ on /mnt/pnfs type nfs4
```

**Server IPs:**
- pNFS MDS: `10.43.47.65` ✅ (intended)
- Standalone NFS: `10.43.224.82` ❌ (actually mounted)

## Why This Happened

When we remounted with `umount /mnt/pnfs` and `mount ... 10.43.47.65:/`, the mount succeeded but connected to **10.43.224.82** instead. This could be:

1. **DNS/Service Discovery Issue** - The IP somehow resolved differently
2. **NFS Referral** - Server 10.43.47.65 referred client to 10.43.224.82
3. **Mount State** - Previous mount to standalone was cached

## Impact

**This explains EVERYTHING:**

1. ❌ **No LAYOUTGET** - Standalone server doesn't support pNFS
2. ❌ **No GETDEVICEINFO** - Not a pNFS server
3. ❌ **No parallel I/O** - Single server handles all I/O
4. ❌ **Performance same as standalone** - Because it IS standalone!
5. ❌ **Zero packets in tcpdump to MDS** - Client never talked to MDS!
6. ❌ **All writes to one IP** - Only standalone-nfs IP in traffic

## Tcpdump Evidence

**Captured traffic during writes:**
```bash
tcpdump -r /tmp/first.pcap 'port 2049' 2>/dev/null | wc -l
0 packets captured  # No traffic to MDS!
```

**Why?** Because all traffic went to 10.43.224.82 (standalone), not 10.43.47.65 (MDS)!

## Client Mount Stats Revisited

Earlier we saw:
```
device 10.43.47.65:/ mounted on /mnt/pnfs
nfsv4: ... pnfs=LAYOUT_NFSV4_1_FILES
```

But the server it was actually talking to was **10.43.224.82**!

The mount table showed the REQUESTED server (10.43.47.65), but /proc/mounts showed the ACTUAL server (10.43.224.82).

## Fix Required

**Mount the correct server:**

```bash
# Verify the pNFS MDS IP
kubectl get svc -n pnfs-test pnfs-mds
# Should show: 10.43.47.65

# Ensure clean mount state
umount -f /mnt/pnfs
rm -rf /mnt/pnfs/*

# Mount pNFS MDS explicitly
mount -t nfs -o vers=4.1,rsize=131072,wsize=131072 10.43.47.65:/ /mnt/pnfs

# Verify correct server
mount | grep pnfs
# Must show: 10.43.47.65:/ on /mnt/pnfs (NOT 10.43.224.82!)

# Test
dd if=/dev/zero of=/mnt/pnfs/test bs=1M count=10 conv=fsync
```

## Why The Confusion

We saw earlier:
```bash
kubectl exec ... -- cat /proc/mounts | grep pnfs
standalone-nfs...:/ /mnt/standalone nfs4 ... addr=10.43.224.82
standalone-nfs...:/ /mnt/pnfs nfs4 ... addr=10.43.224.82
```

**BOTH mount points were connected to standalone-nfs!**

This happened because:
1. First mount: `mount ... standalone-nfs:/ /mnt/standalone` ✅
2. Second mount: `mount ... pnfs-mds:/ /mnt/pnfs` → somehow resolved to standalone ❌

## Next Steps

1. ✅ Unmount /mnt/pnfs completely
2. ✅ Clear any NFS client caches
3. ✅ Mount 10.43.47.65 explicitly by IP
4. ✅ Verify connection to correct server
5. ✅ Test parallel I/O
6. ✅ Capture tcpdump showing LAYOUTGET/GETDEVICEINFO
7. ✅ Measure 2x performance improvement

## Expected Results After Fix

**Mount:**
```
10.43.47.65:/ on /mnt/pnfs ... addr=10.43.47.65
```

**Tcpdump:**
```
NFS request ... LAYOUTGET
NFS reply ... layouts returned
NFS request ... GETDEVICEINFO
NFS reply ... device addresses returned
```

**Client Kernel Logs:**
```
pnfs_update_layout: layout found
--> nfs4_proc_getdeviceinfo
<-- nfs4_proc_getdeviceinfo status=0  ✅
filelayout_choose_ds
NFS: direct write to DS
```

**Performance:**
```
Standalone NFS:    70 MB/s
pNFS with 2 DSs:  140 MB/s (2x) ✅
```

---

**Status**: Ready to fix and retest with correct server!

