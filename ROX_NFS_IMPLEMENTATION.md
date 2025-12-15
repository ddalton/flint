# ROX (ReadOnlyMany) Implementation via NFS

**Date**: 2025-12-15  
**Status**: Design Decision  
**Branch**: `feature/rwx-nfs-support`

## Decision

**ROX volumes will use NFS**, similar to RWX volumes.

## Why Not NVMe-oF for ROX?

We initially attempted to implement ROX using NVMe-oF (commit `66b7c5c`), but this approach has a fundamental limitation:

### The Localhost NVMe-oF Problem

When a pod tries to connect via NVMe-oF to an lvol on the **same node**:

1. The local lvol already exists as a bdev with UUID `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`
2. NVMe-oF initiator connects and tries to create a new bdev for the same storage
3. SPDK attempts to register the UUID alias for the new bdev
4. **SPDK Error**: `Unable to add uuid:xxxxxxxx... alias for bdev nvme_...n1`
5. Result: NVMe controller connects, but bdev registration fails
6. `ublk_start_disk` fails: "No such device"

### Why This Happens

- SPDK uses UUID aliases to identify bdevs uniquely
- An lvol has a UUID that SPDK registers when the lvol is created
- When NVMe-oF exposes that lvol, the initiator sees the same UUID
- On localhost connections, both the local lvol bdev AND the NVMe bdev try to register the same UUID
- **SPDK rejects the duplicate UUID alias**

### Why We Can't Use Local Access

ROX volumes need to support multiple pods reading the same volume. Options we considered:

1. **Local bdev access**: Can't share - SPDK has exclusive bdev ownership (claim mechanism)
2. **Multiple ublk devices from same bdev**: Not possible - one bdev = one claim
3. **Read-only snapshots per pod**: Storage overhead, complexity
4. **Pod anti-affinity**: Limits ROX to one pod per node

## NFS Solution

NFS naturally handles read-only multi-pod access:

### Architecture

```
┌─────────────────────────────────────────┐
│  ROX Volume (AccessMode: ReadOnlyMany)  │
└─────────────────────────────────────────┘
                    │
                    ▼
         ┌──────────────────────┐
         │   NFS Server Pod     │
         │  (mounts lvol via    │
         │   ublk as RWO)       │
         └──────────────────────┘
                    │
         ┌──────────┴──────────┐
         ▼                     ▼
    ┌────────┐           ┌────────┐
    │ Pod 1  │           │ Pod 2  │
    │ (NFS   │    ...    │ (NFS   │
    │ mount) │           │ mount) │
    └────────┘           └────────┘
```

### Implementation

ROX volumes will follow the same pattern as RWX:

1. **Volume Creation**: No change - standard lvol creation
2. **ControllerPublishVolume**:
   - Detect ROX access mode
   - Create NFS server pod (if not exists)
   - NFS pod mounts the volume as RWO (exclusive ublk access)
   - Export via NFS with read-only option
   - Return NFS connection info in publish context
3. **NodeStageVolume**:
   - Mount the NFS export (read-only)
4. **ControllerUnpublishVolume**:
   - Last pod detaches → clean up NFS server pod

### Benefits

✅ **Handles any number of pods** - NFS scales to hundreds of clients  
✅ **Works regardless of scheduling** - Pods can be anywhere  
✅ **No SPDK conflicts** - Only NFS server has ublk/bdev access  
✅ **Proven pattern** - RWX already uses this successfully  
✅ **Standards compliant** - NFSv4 read-only exports  

### Tradeoffs

⚠️ **Extra hop**: NFS adds network latency vs. direct access  
⚠️ **NFS pod overhead**: ~50-100MB memory per volume  
⚠️ **Limited to NFS performance**: ~1-2GB/s per volume typically  

## Implementation Status

- [ ] Add ROX detection in ControllerPublishVolume (similar to RWX check)
- [ ] Share NFS pod creation logic between RWX and ROX
- [ ] Configure NFS exports as read-only for ROX
- [ ] Update ROX test suite to expect NFS mounts
- [ ] Document NFS performance characteristics

## Testing

The existing `rox-multi-pod` test will validate:
- Multiple reader pods accessing the same ROX volume
- Data consistency across readers
- Proper NFS server lifecycle management

## References

- **RWX NFS Implementation**: `spdk-csi-driver/src/rwx_nfs.rs`
- **NFS Server**: Uses kernel NFS server in dedicated pod
- **Related Design**: See `FLINT_CSI_ARCHITECTURE.md` NFS/RWX section
- **Test**: `tests/system/tests-standard/rox-multi-pod/`

---

**Conclusion**: NFS provides a clean, scalable solution for ROX volumes without the complexity and limitations of sharing SPDK bdevs or dealing with localhost NVMe-oF UUID conflicts.

