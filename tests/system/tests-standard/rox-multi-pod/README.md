# ROX (ReadOnlyMany) Multi-Pod Test

## Purpose

Validates that Flint CSI driver correctly supports read-only volume mounts, allowing multiple pods on different nodes to simultaneously mount and read from the same volume.

## Test Scenario

1. ✅ Create PVC and write test data
2. ✅ Mount volume read-only on multiple pods simultaneously
3. ✅ Verify both pods can read the data
4. ✅ Verify read-only enforcement (implicit via mount options)

## Test Flow

```
Step 00: Create PVC (RWO)
  └─ PVC bound successfully

Step 01: Write data to volume
  ├─ Pod writes test data
  └─ Pod completes successfully

Step 02: Delete writer pod
  └─ Volume unmounted

Step 03: Create multiple reader pods with readOnly mounts
  ├─ Pod 1 on node-1 (read-only mount)
  ├─ Pod 2 on node-2 (read-only mount)
  └─ Both pods running simultaneously

Step 04: Cleanup
  ├─ Delete reader pods
  └─ Delete PVC
```

## Success Criteria

| Check | Expected Result |
|-------|----------------|
| PVC creation | PVC binds successfully |
| Data write | Writer pod completes |
| Multi-node readers | Both pods running simultaneously |
| Read-only mounts | Volumes mounted with `-o ro` flag |

## What This Tests

### CSI Driver Functionality
- ✅ **Read-only mount support** - `readonly` flag passed to NodePublishVolume
- ✅ **Multi-attach for read** - Same volume accessible from multiple nodes (read-only)
- ✅ **Mount options** - Proper `-o ro` passed to mount command

### Real-World ROX Use Cases
- Shared configuration files
- Static datasets (ML training data)
- Content distribution (static websites)
- Read-only reference data

## Running the Test

```bash
cd tests/system
KUBECONFIG=/path/to/kubeconfig kubectl kuttl test --config kuttl-testsuite.yaml --test rox-multi-pod
```

## Expected Duration
- **Total time**: ~30-40 seconds
