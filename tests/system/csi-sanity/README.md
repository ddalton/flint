# CSI Compliance Tests (KUTTL-based)

This directory contains KUTTL-based tests that validate CSI specification compliance for the Flint CSI driver. These tests are inspired by the official [csi-sanity](https://github.com/kubernetes-csi/csi-test) test suite but adapted for split controller/node architectures.

## Why KUTTL Instead of csi-sanity Binary?

The official `csi-sanity` test binary expects all CSI services (Identity, Controller, Node) on a single gRPC socket. Flint uses a **split deployment model**:
- Controller pod: Identity + Controller services
- Node pods: Identity + Node services

KUTTL tests provide:
- ✅ Better fit for split architecture
- ✅ Real Kubernetes environment testing
- ✅ Clear pass/fail validation
- ✅ Same framework as other Flint tests

## Test Coverage

These tests validate CSI specification compliance:

### Basic Volume Lifecycle
- **create-delete-volume**: Basic CreateVolume → DeleteVolume flow
- **volume-idempotency**: Calling CreateVolume multiple times with same name

### Node Operations
- **node-stage-unstage**: NodeStageVolume → NodeUnstageVolume flow  
- **node-publish-unpublish**: Complete mount/unmount workflow
- **node-idempotency**: Calling Node RPCs multiple times

### Error Handling
- **invalid-parameters**: Missing required parameters should fail
- **non-existent-volume**: Operations on non-existent volumes should fail gracefully

### Advanced Features
- **volume-expansion**: ControllerExpandVolume → NodeExpandVolume
- **snapshots**: CreateSnapshot → DeleteSnapshot
- **cloning**: CreateVolume from source volume

## Running the Tests

```bash
cd tests/system
kubectl kuttl test --config kuttl-testsuite-csi-sanity.yaml
```

Or run individual tests:
```bash
kubectl kuttl test --test csi-sanity/create-delete-volume
```

## Comparison with csi-sanity

| Aspect | csi-sanity Binary | KUTTL CSI Tests |
|--------|-------------------|-----------------|
| **Architecture** | Single socket | Split controller/node ✅ |
| **Environment** | Standalone | Real Kubernetes ✅ |
| **Setup** | Complex (Go binary) | Simple (YAML) ✅ |
| **Coverage** | 90+ unit tests | Key compliance scenarios |
| **Debugging** | gRPC errors | Kubernetes events + logs ✅ |

## Test Structure

Each test follows CSI specification requirements:

```
csi-sanity/
  create-delete-volume/
    00-create.yaml      # Create PVC
    00-assert.yaml      # Verify volume created
    01-delete.yaml      # Delete PVC
    01-assert.yaml      # Verify volume deleted
```

## References

- [CSI Specification](https://github.com/container-storage-interface/spec)
- [csi-sanity Test Suite](https://github.com/kubernetes-csi/csi-test/tree/master/cmd/csi-sanity)
- [Flint Architecture](../../FLINT_CSI_ARCHITECTURE.md)

