# PVC Clone Test

## Overview

Tests PVC cloning functionality - creating a new PVC from an existing PVC as a data source.

## What This Tests

1. **PVC cloning support** - CSI driver supports `dataSource: kind: PersistentVolumeClaim`
2. **Data preservation** - Cloned PVC contains all data from source PVC
3. **Clone independence** - Modifying source PVC doesn't affect the clone
4. **Clone detection** - Cloned volumes are properly detected to skip reformatting

## Test Flow

```
Step 0: Setup
  - Create source-pvc
  - Write data to source PVC
  - Wait for data to be written

Step 1: Clone
  - Create cloned-pvc with dataSource pointing to source-pvc
  - Wait for clone to be bound

Step 2: Verify Clone Data
  - Mount cloned PVC
  - Verify it contains original data from source

Step 3: Modify Source
  - Modify source PVC (add new data)
  - Ensure clone remains independent

Step 4: Verify Independence
  - Mount cloned PVC again
  - Verify it has original data only (no modifications from source)
```

## Expected Behavior

**Under the hood:**
1. CSI controller receives CreateVolume with volumeContentSource.volume
2. Finds source PVC's node and lvol UUID
3. Creates temporary snapshot of source lvol
4. Clones the snapshot to create new lvol
5. Marks new volume with `is-clone: true` in PV attributes
6. Node agent detects clone and skips reformatting

**Result:**
- Cloned PVC has all data from source at clone time
- Clone is independent (COW - copy-on-write)
- Modifications to source don't affect clone
- Modifications to clone don't affect source

## CSI Capabilities Required

- `CLONE_VOLUME` - Controller capability to create volumes from volumes

