# ReadOnlyMany (ROX) Multi-Node Test

> **F71 fixed in the test, 2026-09-27 (not yet run on a cluster).** Until
> then this test could not fail in the ways its claim table implied. See
> [F71](../../../../spdk-csi-driver/docs/f71-rox-multi-pod-cannot-fail.md).
> **Step 08 is the cluster-level proof of
> [F70](../../../../spdk-csi-driver/docs/f70-rox-export-is-not-enforced-server-side.md),
> fixed server-side 2026-09-28** (the NFS server now answers NFS4ERR_ROFS to
> every mutating op on a ROX export). It failed against every server before
> that fix, and it has not yet been run against the fixed one. **Since
> 2026-09-29 the client also mounts `ro` for a reader-only access mode**, so
> step 07's write now fails on the client — step 08 no longer observes the
> server fence through a CSI mount (see the note in `07-mount-without-readonly.yaml`).

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
Step 05: Create 2 reader pods (REQUIRED anti-affinity: two nodes or the step fails)
Step 06: Both read the data; their nodeNames differ; a write through each
         readOnly mount is refused
Step 07: A pod mounts the ROX PVC WITHOUT readOnly and tries to write
Step 08: That write must be REFUSED (failed while F70 was open; now refused
         by the client's access-mode `ro` AND the server — not yet run)
Step 09: Cleanup
```

## Success Criteria

| Check | Expected Result |
|-------|----------------|
| RWO PVC creation | PVC binds successfully |
| Data write | Writer pod completes |
| Snapshot creation | Snapshot readyToUse=true |
| ROX PVC creation | ROX PVC binds from snapshot |
| Multi-node readers | Pods on different nodes (step 06 compares `spec.nodeName`) |
| Data access | Both readers read the snapshot's data |
| Read-only mount | A write through a `readOnly` pod mount is refused (step 06) |
| ROX enforced | A write from a pod that did NOT ask for `readOnly` is refused (step 08) |

## What This Tests

### CSI Driver Functionality
- ✅ **ReadOnlyMany support** - MULTI_NODE_READER_ONLY capability
- ✅ **Snapshot restoration** - Create volume from snapshot
- ✅ **Multi-node attachment** - Same volume on multiple nodes
- ✅ **Read-only mounts** - a write through a `readOnly` mount is refused
- 🟡 **ROX enforced by the volume** - step 08; F70 fixed server-side 2026-09-28
  and client-side 2026-09-29, not yet run against either fix; the server's
  fence needs a direct-mount leg to be observed on a cluster (not written)

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
