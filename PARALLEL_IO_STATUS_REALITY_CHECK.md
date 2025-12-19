# Parallel I/O Status - Reality Check

**Date**: December 19, 2025  
**Status**: ⚠️ **pNFS Striping NOT Working** - Regular NFS Only

---

## ❌ File Striping: NOT OCCURRING

### Evidence
1. **Data Server filesystems are empty:**
   ```bash
   DS #1 (cdrv-1): /mnt/pnfs-data/  → EMPTY
   DS #2 (cdrv-2): /mnt/pnfs-data/  → EMPTY
   ```

2. **All files are on MDS:**
   ```bash
   MDS: /data/bigfile (200 MB)
   MDS: /data/stripe-test (50 MB)
   MDS: /data/parallel-test (100 MB)
   ```

3. **Zero LAYOUTGET operations** in MDS logs
4. **Zero I/O operations** to Data Servers
5. **Single device ID** seen by client

### What Actually Happened
- ✅ Regular NFSv4.1 mount successful
- ✅ Files written/read through MDS only
- ❌ No pNFS layouts requested
- ❌ No direct DS connections for I/O
- ❌ No file striping across multiple DSes

### Performance Results
- Write: 62.8 - 88.3 MB/s (through MDS only)
- Read: 7.6 GB/s (from cache, not DS)
- **This is single-server NFS performance, not parallel I/O**

---

## 🔍 Root Cause Analysis

### Why pNFS Isn't Being Used

#### Client Side
- ✅ Kernel message: `set_pnfs_layoutdriver: pNFS module for 1 set`
  - Client loaded the pNFS file layout driver
  - Client recognized server supports pNFS
- ❌ But client never requests layouts (no LAYOUTGET)

#### Server Side  
- ✅ MDS code exists to modify EXCHANGE_ID flags
- ✅ Should set `USE_PNFS_MDS` (0x00020000)
- ❌ No log messages showing "🎯 EXCHANGE_ID: Modified flags"
- ❌ No LAYOUTGET operations logged

### Possible Issues

1. **EXCHANGE_ID flag not being set correctly**
   - Code exists in `handle_compound_with_pnfs()` but may not execute
   - Client may be receiving `USE_NON_PNFS` instead of `USE_PNFS_MDS`

2. **FS_LAYOUT_TYPES attribute not advertised**
   - Client needs to see layout types in GETATTR response
   - May not be querying or receiving this attribute

3. **Data Servers not registered**
   - MDS may not have active DSes to stripe across
   - Need to verify DS registration via gRPC

---

## ✅ What IS Working

### Kerberos/RPCSEC_GSS Implementation
- ✅ 724 lines of pure Rust code
- ✅ 13 unit tests
- ✅ Server processes RPCSEC_GSS tokens correctly
- ✅ Generates valid AP-REP responses
- ✅ Keytab loading working
- ✅ Infrastructure complete

### Basic NFS Functionality
- ✅ NFSv4.1 mounting works
- ✅ File I/O works (62-88 MB/s)
- ✅ Sessions working
- ✅ State management working

---

## 🎯 What Needs to Be Fixed

### Priority 1: Verify EXCHANGE_ID Flags
```rust
// In handle_compound_with_pnfs(), line 443-453
// This code should be logging, but isn't
if let OperationResult::ExchangeId(status, Some(ref mut res)) = result {
    res.flags = set_pnfs_mds_flags(res.flags);  // Sets USE_PNFS_MDS
    info!("🎯 EXCHANGE_ID: Modified flags...");  // NOT APPEARING IN LOGS
}
```

**Action needed:** Debug why this code path isn't executing

### Priority 2: Verify DS Registration
```bash
kubectl logs -l app=pnfs-mds | grep "Registering new device"
# Should show 2 DSes registered
```

**Action needed:** Verify DSes are actually registered and active

### Priority 3: Force LAYOUTGET
Even with pNFS enabled, client may not request layouts if:
- File is too small (< stripe size)
- O_DIRECT not used
- pNFS disabled by mount option

**Action needed:** Trigger explicit LAYOUTGET via larger files or specific workload

---

## 🔧 Next Steps to Enable True Parallel I/O

1. **Add debug logging to EXCHANGE_ID path**
   - Verify flags are being set correctly
   - Log the actual flag values sent to client

2. **Verify Data Server registration**
   - Check gRPC control plane
   - Verify device registry has 2 active DSes

3. **Force layout requests**
   - Use larger files (> 100 MB)
   - Use direct I/O
   - Check if client receives layout

4. **Verify stripe file creation**
   - Should see files in `/mnt/pnfs-data/` on both DSes
   - Each DS should have chunks of the file

---

## 📊 Honest Assessment

### What We Accomplished
✅ Complete pure Rust Kerberos/RPCSEC_GSS implementation (production-ready)  
✅ Server infrastructure and deployment working  
✅ Basic NFS functionality verified (60-90 MB/s)  

### What We Haven't Achieved Yet
❌ Actual file striping across multiple Data Servers  
❌ True parallel I/O with concurrent DS access  
❌ Performance scaling with multiple DSes  

### The Gap
The pNFS **protocol implementation** exists, but the **E2E flow** isn't triggering:
- Code is there
- Infrastructure is there  
- But LAYOUTGET never happens
- Files stay on MDS only

---

## 🎯 Recommendation

**We need to debug the pNFS activation path:**

1. Add extensive logging to EXCHANGE_ID handler
2. Verify `USE_PNFS_MDS` flag is actually being sent
3. Check DS registration status
4. Force LAYOUTGET with proper workload
5. Verify stripe files appear on DSes

**Estimated time:** 2-3 hours to debug and fix the pNFS activation issue.

---

## Summary

**Kerberos Implementation:** ✅ Complete and tested  
**File Striping:** ❌ Not yet working  
**Parallel I/O:** ❌ Not yet achieved  
**Current Performance:** Regular NFS (not parallel)  

We have the infrastructure and code, but the E2E integration needs debugging.

