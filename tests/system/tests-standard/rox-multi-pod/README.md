# Read-Only Multi-Pod Test

## Purpose

Validates that Flint CSI driver correctly supports read-only volume mounts (`readonly: true` flag), allowing multiple pods to simultaneously mount and read from the same volume.

## Test Scenario

1. ✅ Create PVC and write test data
2. ✅ Mount volume read-only on multiple pods simultaneously (same node)
3. ✅ Verify both pods can read the data
4. ✅ Implicit read-only enforcement via `-o ro` mount option

## What This Tests

This tests the `readonly` flag in `NodePublishVolume`, which adds `-o ro` to mount commands. This is different from full ReadOnlyMany (ROX) access mode support, which would require additional CSI capabilities for multi-node attachment.

**Current implementation**: Multiple pods on same node can mount read-only  
**Future**: Full ROX (ReadOnlyMany) for multi-node read-only access

## Test Flow

```
Step 00: Create PVC (RWO)
Step 01: Write data + assert PVC bound and writer succeeded  
Step 02: Delete writer pod
Step 03: Create 2 reader pods with readOnly mounts (prefer same node)
Step 04: Cleanup
```

## Success Criteria

| Check | Expected Result |
|-------|----------------|
| PVC creation | PVC binds when writer pod starts |
| Data write | Writer pod completes successfully |
| Multiple readers | Both pods running simultaneously with readonly mounts |
| Read-only mounts | Volumes mounted with `-o ro` flag |

## Running the Test

```bash
cd tests/system
KUBECONFIG=/path/to/kubeconfig kubectl kuttl test --config kuttl-testsuite.yaml --test rox-multi-pod
```

## Expected Duration
- **Total time**: ~30-40 seconds
