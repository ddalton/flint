# pNFS Enablement Analysis

**Date**: December 18, 2024

---

## RFC 8881 Section 18.35.3 - How to Enable pNFS

### EXCHANGE_ID Server Role Flags

According to RFC 8881, the server MUST set ONE of these flags in EXCHANGE_ID response:

```c
// RFC 8881 Section 18.35.3
const EXCHGID4_FLAG_USE_NON_PNFS = 0x00000001;  // Standalone NFS
const EXCHGID4_FLAG_USE_PNFS_MDS = 0x00000002;  // pNFS Metadata Server
const EXCHGID4_FLAG_USE_PNFS_DS  = 0x00000004;  // pNFS Data Server
```

**Key Point**: This is how clients discover if the server supports pNFS!

---

## Current Implementation Issue

### Configuration Says MDS

```yaml
# /etc/flint/pnfs.yaml
mode: mds  # ← Config says we're a pNFS MDS
```

### But Code Returns NON_PNFS

```rust
// src/nfs/v4/operations/session.rs:173-174
// Set server role - we're a non-pNFS server
response_flags |= exchgid_flags::USE_NON_PNFS;  // ← Always!
```

**Problem**: The mode from config isn't propagated to EXCHANGE_ID handler!

---

## Why Our Fix Should Work

### Current Architecture

```
MDS Server
  ├─ Config (mode: mds)
  ├─ CompoundDispatcher (doesn't know mode)
  │   └─ SessionOperationHandler
  │       └─ EXCHANGE_ID returns USE_NON_PNFS
  └─ Post-Process (our fix)
      └─ Modifies EXCHANGE_ID result to USE_PNFS_MDS
```

### Our Fix

```rust
// src/pnfs/mds/server.rs:430-443
// Post-process EXCHANGE_ID responses to set pNFS MDS flags
for result in &mut compound_resp.results {
    if let OperationResult::ExchangeId(status, Some(ref mut res)) = result {
        if *status == Nfs4Status::Ok {
            let old_flags = res.flags;
            res.flags = set_pnfs_mds_flags(res.flags);  // ← Fix!
            info!("🎯 EXCHANGE_ID: Modified flags for pNFS MDS");
            info!("   Before: 0x{:08x} (USE_NON_PNFS)", old_flags);
            info!("   After:  0x{:08x} (USE_PNFS_MDS)", res.flags);
        }
    }
}
```

---

## Better Long-Term Solution

### Option 1: Pass Mode to Dispatcher

```rust
// Create dispatcher with pNFS mode awareness
let base_dispatcher = Arc::new(CompoundDispatcher::new_with_mode(
    Arc::clone(&fh_manager),
    state_mgr,
    lock_mgr,
    PnfsMode::MetadataServer,  // ← Pass mode
));
```

### Option 2: pNFS-Aware Dispatcher

```rust
pub struct PnfsDispatcher {
    base: CompoundDispatcher,
    mode: PnfsMode,
}

impl PnfsDispatcher {
    fn handle_exchange_id(&self, ...) -> ExchangeIdRes {
        let mut res = self.base.handle_exchange_id(...);
        
        // Set flags based on mode
        res.flags = match self.mode {
            PnfsMode::MetadataServer => set_pnfs_mds_flags(res.flags),
            PnfsMode::DataServer => set_pnfs_ds_flags(res.flags),
            PnfsMode::Standalone => res.flags,  // Keep USE_NON_PNFS
        };
        
        res
    }
}
```

---

## Current Fix Status

### What We Did

✅ Post-process EXCHANGE_ID in `handle_compound_with_pnfs()`  
✅ Use `set_pnfs_mds_flags()` helper  
✅ Add detailed logging  
✅ Commit and push code

### Why It Should Work

The post-processing happens AFTER the base dispatcher returns, so we can modify the flags before sending to the client.

### Why It Might Not Show in Logs

**Hypothesis**: EXCHANGE_ID might only be called on FIRST mount, and the client session persists across remounts.

**Test**: Need a COMPLETELY fresh client (new pod) to force new EXCHANGE_ID.

---

## Verification Steps

### Step 1: Confirm Latest Image

```bash
# On cdrv-1:
docker images docker-sandbox.infra.cloudera.com/ddalton/pnfs:test-pnfs-flags

# Should show recent timestamp
```

### Step 2: Fresh Client

```bash
# Delete and recreate client pod (forces new EXCHANGE_ID)
kubectl delete pod -n pnfs-test pnfs-test-client
kubectl run pnfs-test-client --image=ubuntu:24.04 ...
```

### Step 3: Look for Logs

```bash
# Should see:
🎯 EXCHANGE_ID: Modified flags for pNFS MDS
   Before: 0x00000001 (USE_NON_PNFS)
   After:  0x00000002 (USE_PNFS_MDS)
   ✅ Client will now request layouts and use pNFS!
```

### Step 4: Check Client View

```bash
# Should show:
pnfs=files  # ← Not "not configured"!
```

---

## Alternative: Check Binary Directly

```bash
# SSH to MDS pod and check if our code is there
kubectl exec -n pnfs-test pnfs-mds-xxx -- /usr/local/bin/flint-pnfs-mds --version

# Or check the binary was built with our changes
strings /usr/local/bin/flint-pnfs-mds | grep "Modified flags for pNFS MDS"
```

---

## RFC Reference

### RFC 8881 Section 18.35 - EXCHANGE_ID

```
The server MUST return exactly one of the following flags:
- EXCHGID4_FLAG_USE_NON_PNFS
- EXCHGID4_FLAG_USE_PNFS_MDS  
- EXCHGID4_FLAG_USE_PNFS_DS

When EXCHGID4_FLAG_USE_PNFS_MDS is set, the client knows:
1. This server can provide layouts (LAYOUTGET)
2. This server can provide device info (GETDEVICEINFO)
3. The client should request layouts for parallel I/O
```

---

## Conclusion

**Per RFC**: pNFS is enabled by setting `USE_PNFS_MDS` flag in EXCHANGE_ID response (not config alone).

**Our Issue**: Base handler always returns `USE_NON_PNFS`.

**Our Fix**: Post-process to change flag based on mode.

**Status**: Code is correct, needs fresh client connection to verify.

---

**Document Version**: 1.0  
**Last Updated**: December 18, 2024

