# Ephemeral Inline Volume Test

This test verifies that the Flint CSI driver correctly supports **CSI ephemeral inline volumes**.

## What are Ephemeral Inline Volumes?

Ephemeral inline volumes are CSI volumes declared directly in a Pod spec without creating a PVC or PV. They have these characteristics:

- **No PVC required**: Volume definition is embedded in the Pod spec
- **Lifecycle tied to Pod**: Volume is created when Pod starts, deleted when Pod terminates
- **Automatic cleanup**: No manual cleanup needed - volume is garbage-collected with the Pod
- **Fast provisioning**: Ideal for temporary scratch space, caching, or test data

## Test Scenario

This test validates the complete lifecycle of an ephemeral inline volume:

### Steps

1. **Prerequisites Check** (`00-assert.yaml`)
   - Verify Flint CSI driver is installed
   - Confirm `Ephemeral` mode is enabled in CSIDriver object
   - Check controller pod is running

2. **Create Pod with Inline Volume** (`01-pod.yaml`)
   - Deploy a Pod with an inline CSI volume (no PVC)
   - Volume is specified directly in `volumes.csi` section
   - 100Mi volume with ext4 filesystem

3. **Verify Volume Works** (`01-assert.yaml`)
   - Wait for Pod to start and complete
   - Verify data was written to the ephemeral volume
   - Check CSI controller logs for ephemeral volume creation
   - Confirm Pod executed successfully

4. **Delete Pod** (`02-cleanup.yaml`)
   - Delete the Pod using `$patch: delete`
   - Triggers automatic volume cleanup

5. **Verify Automatic Cleanup** (`02-assert.yaml`)
   - Confirm Pod is deleted
   - Verify volume deletion in CSI logs
   - Ensure no orphaned PV/PVC resources remain
   - Validate complete cleanup

## Expected Behavior

✅ **Success Criteria:**
- Pod starts with ephemeral inline volume
- Data can be written and read from volume
- Pod completes successfully
- Volume is automatically deleted when Pod terminates
- No orphaned Kubernetes resources (PV, PVC)

❌ **Failure Indicators:**
- Pod fails to start (volume mount error)
- Data write/read failures
- Volume not cleaned up after Pod deletion
- Orphaned PV or PVC resources

## Prerequisites

- Kubernetes v1.25+ (or v1.21-v1.24 with `CSIInlineVolume=true` feature gate)
- Flint CSI driver installed with `driver.enableEphemeral: true` in Helm values
- At least one node with available disk space (100Mi+)

## Running the Test

```bash
# From the tests/system directory
kubectl kuttl test --config kuttl-testsuite.yaml --test ephemeral-inline

# Or run just this test
kubectl kuttl test --test ephemeral-inline
```

## Example Pod Spec

Here's what an ephemeral inline volume looks like in practice:

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: my-app
spec:
  containers:
  - name: app
    image: myapp:latest
    volumeMounts:
    - name: scratch
      mountPath: /tmp/scratch
  volumes:
  - name: scratch
    csi:
      driver: flint.csi.storage.io
      fsType: ext4
      volumeAttributes:
        size: "1Gi"
```

**No PVC needed!** The volume is provisioned automatically when the Pod starts.

## Use Cases for Ephemeral Volumes

- **Temporary scratch space** for data processing jobs
- **Build caches** for CI/CD pipelines
- **Test data** for integration tests
- **Fast local storage** for stateless workloads
- **Per-pod isolated storage** that doesn't persist

## Troubleshooting

### Pod Stuck in ContainerCreating

```bash
kubectl describe pod ephemeral-test-pod
```

Look for events like:
- `FailedMount`: CSI driver not installed or misconfigured
- `VolumeProvisioningFailed`: Check CSI controller logs

### Ephemeral Mode Not Enabled

```bash
kubectl get csidriver flint.csi.storage.io -o yaml
```

Should show:
```yaml
spec:
  volumeLifecycleModes:
    - Persistent
    - Ephemeral  # Must be present
```

If missing, update Helm values:
```yaml
driver:
  enableEphemeral: true
```

Then upgrade the Helm chart.

### Volume Not Cleaned Up

Check CSI controller logs:
```bash
kubectl logs -n kube-system -l app=flint-csi-controller --tail=100
```

Look for `DeleteVolume` calls after Pod deletion.

## Technical Details

### Volume Naming

Ephemeral volumes use a special naming format:
```
ephemeral-<pod-name>-<pod-uid>-<volume-name>
```

Example: `ephemeral-test-pod-abc123-ephemeral-storage`

### CSI RPC Flow

1. **Pod Creation**:
   - CreateVolume (on node)
   - NodeStageVolume
   - NodePublishVolume

2. **Pod Deletion**:
   - NodeUnpublishVolume
   - NodeUnstageVolume
   - DeleteVolume (automatic cleanup)

### Volume Context

Kubernetes sets a special marker in the volume context:
```
csi.storage.k8s.io/ephemeral: "true"
```

The Flint driver uses this to optimize ephemeral volumes (e.g., default to single replica).

## Differences from Persistent Volumes

| Feature | Persistent (PVC) | Ephemeral (Inline) |
|---------|------------------|---------------------|
| **Lifecycle** | Independent | Tied to Pod |
| **Definition** | Separate PVC | Inline in Pod |
| **Cleanup** | Manual (or with reclaim policy) | Automatic |
| **Reuse** | Yes (multiple Pods) | No (one Pod only) |
| **Snapshot** | Supported | Not applicable |
| **Clone** | Supported | Not applicable |

## References

- [Kubernetes CSI Ephemeral Volumes](https://kubernetes-csi.github.io/docs/ephemeral-local-volumes.html)
- [CSI Spec: Ephemeral Volumes](https://github.com/container-storage-interface/spec/blob/master/spec.md#createvolume)
- [Flint CSI Driver Architecture](../../../FLINT_CSI_ARCHITECTURE.md)

