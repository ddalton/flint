# ReadOnlyMany (ROX) Multi-Node Test

## Purpose

Validates that Flint CSI driver correctly supports ReadOnlyMany (ROX) access mode, allowing multiple pods on different nodes to simultaneously mount and read from the same volume.

## Test Workflow

ROX volumes are created from snapshots:

1. ✅ Create RWO PVC and write test data
2. ✅ Create snapshot of the RWO volume
3. ✅ Create ReadOnlyMany PVC from snapshot  
4. ✅ Multiple pods mount ROX PVC simultaneously (different nodes)
5. ✅ Verify all pods can read the data

## Why Snapshots?

In Kubernetes, a PVC can only have ONE access mode. You cannot have a single PVC that is both ReadWriteOnce and ReadOnlyMany. The standard workflow for ROX is:

- **Source PVC**: ReadWriteOnce (for writing data)
- **Snapshot**: Capture the data
- **ROX PVC**: ReadOnlyMany (created from snapshot for reading)

## Test Flow

```
Step 00: Create RWO PVC
Step 01: Write data + assert bound
Step 02: Create snapshot  
Step 03: Delete writer pod
Step 04: Create ROX PVC from snapshot + assert bound
Step 05: Create 2 reader pods (anti-affinity for different nodes)
Step 06: Verify both pods can read data
Step 07: Cleanup
```

## Success Criteria

| Check | Expected Result |
|-------|----------------|
| RWO PVC creation | PVC binds successfully |
| Data write | Writer pod completes |
| Snapshot creation | Snapshot readyToUse=true |
| ROX PVC creation | ROX PVC binds from snapshot |
| Multi-node readers | Pods on different nodes, both Running |
| Data access | All pods read identical data |

## What This Tests

### CSI Driver Functionality
- ✅ **ReadOnlyMany support** - MULTI_NODE_READER_ONLY capability
- ✅ **Snapshot restoration** - Create volume from snapshot
- ✅ **Multi-node attachment** - Same volume on multiple nodes
- ✅ **Read-only mounts** - Proper `-o ro` mount options

### Real-World ROX Use Cases
- Shared configuration across pods
- ML training data distribution
- Static website content distribution
- Shared read-only databases

## Running the Test

```bash
cd tests/system
KUBECONFIG=/path/to/kubeconfig kubectl kuttl test --config kuttl-testsuite.yaml --test rox-multi-pod
```

## Expected Duration
- **Total time**: ~60-80 seconds (includes snapshot creation)
