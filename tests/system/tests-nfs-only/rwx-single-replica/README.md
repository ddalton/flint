# RWX Single-Replica Test

## Purpose

Tests ReadWriteMany (RWX) volume support with a single-replica volume using NFS.

## Test Scenario

1. **Create RWX PVC** (`00-pvc.yaml`)
   - Request `ReadWriteMany` access mode
   - Single replica volume (default)
   - Should trigger NFS server pod creation

2. **Deploy 3 Writer Pods** (`01-pods.yaml`)
   - All pods mount the same RWX PVC simultaneously
   - Each pod writes to:
     - Shared file: `/data/shared.log` (concurrent writes)
     - Individual file: `/data/writer-N.log` (per-pod file)
   - Tests concurrent write capability

3. **Verify Data Integrity** (`02-verify.yaml`)
   - Checks that `shared.log` contains writes from all 3 pods
   - Verifies individual logs exist and show completion
   - Confirms RWX volume is truly shared

4. **Cleanup** (`03-cleanup.yaml`)
   - Deletes all pods and PVC
   - NFS server pod should be automatically cleaned up

## Expected Behavior

### NFS Server Pod

- CSI controller should create `flint-nfs-<volume-id>` pod
- Pod scheduled on the node where the volume replica resides
- Exports volume over NFS on port 2049

### Client Pods

- All 3 writer pods should start successfully
- Each pod mounts NFS export (not ublk device)
- Concurrent writes to shared file work correctly
- Individual per-pod files are created

### Data Verification

- `shared.log` contains interleaved messages from all 3 writers
- Each writer's individual log shows completion
- Verifier pod can read all files

## Prerequisites

- NFS support must be enabled in Helm values (`nfs.enabled=true`)
- NFS server image must be available
- At least one node with available storage

## Running the Test

```bash
cd tests/system
kubectl kuttl test --test rwx-single-replica
```

## Success Criteria

- ✅ PVC bound successfully
- ✅ NFS server pod created and running
- ✅ All 3 writer pods running simultaneously  
- ✅ Shared log contains messages from all writers
- ✅ Verifier pod succeeds
- ✅ Cleanup completes without errors

## Failure Scenarios

### NFS Disabled

If `nfs.enabled=false`:
- PVC creation should fail with error message
- Test should fail gracefully

### NFS Server Issues

If NFS server pod fails to start:
- Writer pods will be stuck in `ContainerCreating`
- Test will timeout

### Mount Failures

If NFS mount fails:
- Pods will be stuck or in `CrashLoopBackOff`
- Check pod events and CSI driver logs

## Architecture Diagram

```
┌─────────────────────────────────────────────────────────────┐
│ Node A (has volume replica)                                 │
│                                                              │
│  ┌──────────────┐         ┌─────────────────┐              │
│  │ ublk Device  │◄────────│ NFS Server Pod  │              │
│  │ /dev/ublkb5  │         │ (flint-nfs-...) │              │
│  └──────────────┘         │                 │              │
│         ▲                 │ Exports:        │              │
│         │                 │ /exports/vol-id │              │
│    lvol UUID              └────────┬────────┘              │
│                                    │ NFS port 2049         │
└────────────────────────────────────┼────────────────────────┘
                                     │
                    ┌────────────────┼────────────────┐
                    │                │                │
         ┌──────────▼─────┐   ┌─────▼──────┐   ┌────▼────────┐
         │ Node B/C/D     │   │ Node B/C/D │   │ Node B/C/D  │
         │                │   │            │   │             │
         │ rwx-writer-1   │   │ rwx-writer-2   │ rwx-writer-3│
         │ (NFS mount)    │   │ (NFS mount)│   │ (NFS mount) │
         │ /data/         │   │ /data/     │   │ /data/      │
         └────────────────┘   └────────────┘   └─────────────┘
                    │                │                │
                    └────────────────┴────────────────┘
                                     │
                              Shared NFS export
                           All see same files
```

## Related Files

- **NFS Module**: `spdk-csi-driver/src/rwx_nfs.rs`
- **CSI Integration**: `spdk-csi-driver/src/main.rs` (CreateVolume, ControllerPublishVolume, NodePublishVolume)
- **Helm Configuration**: `flint-csi-driver-chart/values.yaml` (`nfs.enabled`)

