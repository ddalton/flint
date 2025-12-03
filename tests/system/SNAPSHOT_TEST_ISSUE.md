# Snapshot Restore Test Failure Analysis

## Issue Summary

The `snapshot-restore` test fails because volumes restored from snapshots are missing required Flint metadata in the PersistentVolume's `volumeAttributes`.

## Root Cause

When `CreateVolume` is called with a snapshot data source, the Flint CSI controller:

1. ✅ **Successfully creates the volume from the snapshot**
   - Clone UUID: `9f22500b-8a92-4d31-91b1-278da75afe6b`
   - Log shows: "Volume from snapshot created successfully"

2. ❌ **Fails to return the volume metadata in the CreateVolumeResponse**
   - The CSI provisioner uses this response to populate PV `volumeAttributes`
   - Without metadata, subsequent attach operations fail

## Evidence

### Source PV (works correctly)
```yaml
csi:
  volumeAttributes:
    flint.csi.storage.io/lvol-uuid: 2d746fde-8d81-43d1-aa26-fb06a44369c8
    flint.csi.storage.io/lvs-name: lvs_ublk-1.vpc.cloudera.com_0000-00-1d-0
    flint.csi.storage.io/node-name: ublk-1.vpc.cloudera.com
    flint.csi.storage.io/replica-count: "1"
```

### Restored PV (missing metadata)
```yaml
csi:
  volumeAttributes:
    storage.kubernetes.io/csiProvisionerIdentity: 1764779443441-8081-flint.csi.storage.io
    # ❌ No Flint metadata!
  volumeHandle: pvc-09b8dcc3-7b46-447c-b842-b3f588ec2cd7
```

### Error When Attaching
```
AttachVolume.Attach failed for volume "pvc-09b8dcc3-7b46-447c-b842-b3f588ec2cd7" : 
rpc error: code = NotFound desc = Volume not found: Volume pvc-09b8dcc3-7b46-447c-b842-b3f588ec2cd7 
metadata not found in PV: PV found but missing flint metadata in volumeAttributes
```

## Code Location

The bug is in the CSI controller's `CreateVolume` implementation:

**File**: `spdk-csi-driver/src/controller_operator.rs` or `driver.rs`

**Function**: `CreateVolume` RPC handler

**Problem**: When creating a volume from a snapshot, the code path that handles snapshot cloning doesn't return the volume metadata (node name, lvol UUID, lvs name, replica count) in the `CreateVolumeResponse`.

## Fix Required

The `CreateVolume` function needs to:

1. After successfully cloning from snapshot, query the node agent to get the new lvol's metadata
2. Populate the `volume_context` map in the `CreateVolumeResponse` with:
   - `flint.csi.storage.io/node-name`
   - `flint.csi.storage.io/lvol-uuid`
   - `flint.csi.storage.io/lvs-name`
   - `flint.csi.storage.io/replica-count`

This is already done correctly for regular volume creation (not from snapshot), so the same pattern should be followed for snapshot restore.

## Test Status

- ✅ **rwo-pvc-migration**: PASSED
- ✅ **volume-expansion**: PASSED  
- ✅ **multi-replica**: PASSED
- ✅ **clean-shutdown**: PASSED (runs in isolation)
- ❌ **snapshot-restore**: FAILED (metadata bug)

## Workaround

None available - this requires a code fix in the CSI driver.

## Related Files

- `spdk-csi-driver/src/controller_operator.rs` - Main controller logic
- `spdk-csi-driver/src/driver.rs` - CSI RPC handlers
- `spdk-csi-driver/src/snapshot/` - Snapshot implementation

## Log Excerpt

```
🎯 [CONTROLLER] Creating volume: pvc-09b8dcc3-7b46-447c-b842-b3f588ec2cd7
🔄 [CONTROLLER] Creating volume from snapshot: 20e9ede0-2b36-42b0-9908-8ff93094ec8c
✅ [CONTROLLER] Volume pvc-09b8dcc3-7b46-447c-b842-b3f588ec2cd7 created from snapshot (clone UUID: 9f22500b-8a92-4d31-91b1-278da75afe6b)
🎉 [CONTROLLER] Volume from snapshot created successfully
📤 [CONTROLLER] Publishing volume pvc-09b8dcc3-7b46-447c-b842-b3f588ec2cd7 to node ublk-2.vpc.cloudera.com
🔍 [DRIVER] Getting volume info from PV metadata: pvc-09b8dcc3-7b46-447c-b842-b3f588ec2cd7
❌ [DRIVER] Volume metadata not found in PV: PV found but missing flint metadata in volumeAttributes
```

The volume is created successfully, but when trying to attach it, the driver can't find its metadata because it was never stored in the PV's volumeAttributes during creation.

