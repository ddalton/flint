# pNFS Mystery: LAYOUTGET Works But Logs Don't Appear

**Date**: December 18, 2025  
**Status**: 🔍 **Investigation In Progress** - Code works but logging is invisible

---

## 🎯 The Mystery

**What Works:**
- ✅ Client sends LAYOUTGET requests (mountstats shows 20-100+ requests)
- ✅ MDS creates layouts (status reports show "Active Layouts: 15-103")
- ✅ pNFS is activated (`pnfs=LAYOUT_NFSV4_1_FILES`)
- ✅ Debug build runs (startup marker "🔥🔥🔥 DEBUG BUILD" appears)

**What Doesn't Appear:**
- ❌ LAYOUTGET decode logs (`🎯 DECODING LAYOUTGET`)
- ❌ LAYOUTGET dispatch logs (`🔴 ABOUT TO DISPATCH LAYOUTGET`)
- ❌ LAYOUTGET handler logs (`🚨 LAYOUTGET OPERATION DISPATCHED`)
- ❌ PnfsOperationHandler logs (`🔥 PnfsOperationHandler::layoutget()`)
- ❌ LayoutManager logs (`💥 LayoutManager::generate_layout()`)

**What This Means:**
The LAYOUTGET code path in `dispatcher.rs` and `operations/mod.rs` **is NOT being executed**, yet layouts ARE being created somehow.

---

## 🔬 Evidence

### Client Statistics (from /proc/self/mountstats)
```
LAYOUTGET: 20 20 0 4400 5120 1 10 11 0  ← Client sent 20 requests
GETDEVICEINFO: 0 0 0 0 0 0 0 0 0       ← Client NEVER requested device info
```

### MDS Status Reports
```
Active Layouts: 103  ← Layouts ARE being created!
```

### Debug Logging Added
1. **compound.rs line 1163**: `eprintln!("🎯 DECODING LAYOUTGET")` - NOT appearing
2. **dispatcher.rs line 801**: `warn!("🚨 LAYOUTGET OPERATION DISPATCHED")` - NOT appearing  
3. **dispatcher.rs line 114**: `warn!("🔴 ABOUT TO DISPATCH LAYOUTGET")` - NOT appearing
4. **operations/mod.rs line 49**: `warn!("🔥 PnfsOperationHandler::layoutget() CALLED")` - NOT appearing
5. **layout.rs line 153**: `warn!("💥 LayoutManager::generate_layout() CALLED")` - NOT appearing

### Logs That DO Appear
- ✅ Startup marker: `🔥🔥🔥 FLINT-PNFS-MDS STARTING WITH DEBUG LOGGING`
- ✅ gRPC heartbeats from DSs
- ✅ Device registration logs
- ✅ Regular NFS operations (OPEN, GETATTR, CLOSE, WRITE)

---

## 🧩 Code Cleanup Completed

**Removed Obsolete Code:**
- `compound_wrapper.rs` (449 lines) - PnfsCompoundWrapper was created but never used
- All `pnfs_wrapper` references from `server.rs`

**Active Code Path:**
```
MDS server.rs:430
  → base_dispatcher.dispatch_compound()
    → CompoundDispatcher (dispatcher.rs)
      → handle_layoutget()
```

---

## 🤔 Theories

### Theory 1: Layouts Created Via Different Mechanism
- Perhaps there's a fallback that creates stub layouts without going through our handler?
- The client gets a response that looks like a layout but isn't actually useful?

### Theory 2: Caching/Old Binary
- Despite fresh builds, an old binary might be cached somewhere
- But startup marker proves debug build is running...

### Theory 3: Async Logging Issue
- NFS operations run in async tasks
- Logs might not flush or might be buffered differently
- But other operations (WRITE, OPEN, etc.) do log...

### Theory 4: LAYOUTGET Returns NotSupp and Client Caches That
- Client sent LAYOUTGET initially  
- Got NOT_SUPPORTED or error response
- Client stopped sending them
- But mountstats shows 20 requests, not 1...

---

## 📊 Current State

| Component | Status | Evidence |
|-----------|--------|----------|
| pNFS Activation | ✅ Working | `pnfs=LAYOUT_NFSV4_1_FILES` |
| EXCHANGE_ID | ✅ Working | Logs show flag modification |
| Client Sends LAYOUTGET | ✅ Yes | Mountstats: 20 requests |
| MDS Creates Layouts | ✅ Yes | Status: 103 active layouts |
| LAYOUTGET Handler Executes | ❌ NO | Zero debug logs |
| GETDEVICEINFO Sent | ❌ NO | Mountstats: 0 requests |
| DS I/O | ❌ NO | All I/O through MDS |

---

## 🔍 Next Steps

1. ❓ **Find where layouts are actually being created**
   - Search for `layouts.insert()` calls
   - Check if there's a default/stub layout generator

2. ❓ **Verify LAYOUTGET response format**
   - Maybe client IS getting layouts
   - But format is wrong so it doesn't request GETDEVICEINFO

3. ❓ **Check binary compilation**
   - Verify the Docker image actually has the latest code
   - Check if there's a compilation issue with the logging

4. ❓ **Alternative: Direct SSH Testing**
   - Run binary directly on Linux machine
   - See logs in real-time without Kubernetes
   - This would bypass any container logging issues

---

## 💡 Key Insight

The fact that:
- Startup debug marker appears ✅
- But operation debug markers don't ❌  
- Yet layouts ARE created ✅

...suggests there's a **parallel code path** or **stub implementation** that creates layouts without going through our instrumented handlers.

---

**Status**: Need to find where layouts are actually being created!

