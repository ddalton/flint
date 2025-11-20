# CSI Driver System Test Framework

This is a declarative test framework for testing CSI drivers on Kubernetes using [Kuttl](https://kuttl.dev/).

## Prerequisites

1. **Install Kuttl**
   ```bash
   # Using kubectl plugin
   kubectl krew install kuttl
   
   # Or download binary directly
   # https://github.com/kudobuilder/kuttl/releases
   ```

2. **Kubernetes Cluster**
   - A running Kubernetes cluster with your CSI driver installed
   - `kubectl` configured to access the cluster

## Project Structure

```
.
├── kuttl-testsuite.yaml           # Main test suite configuration
├── tests/
│   ├── clean-shutdown/            # Test: Clean shutdown and fast remount ⭐ NEW
│   │   ├── 00-pvc.yaml            # Create PVC
│   │   ├── 00-assert.yaml         # Assert PVC bound
│   │   ├── 01-writer-pod.yaml     # Write data and sync
│   │   ├── 01-assert.yaml         # Assert write succeeded
│   │   ├── 02-delete-writer.yaml  # Trigger clean shutdown
│   │   ├── 02-assert.yaml         # Assert deletion
│   │   ├── 03-verify-logs.yaml    # Verify BLOBSTORE UNLOAD in logs
│   │   ├── 04-fast-remount.yaml   # Remount and verify data
│   │   ├── 04-assert.yaml         # Assert fast remount (< 30s)
│   │   ├── 05-verify-no-recovery.yaml  # Verify no recovery triggered
│   │   ├── 06-rapid-cycle.yaml    # Test rapid mount/unmount
│   │   ├── 06-assert.yaml         # Assert rapid cycle works
│   │   └── README.md              # Detailed test documentation
│   ├── rwo-pvc-migration/         # Test: RWO PVC migration between nodes
│   │   ├── 00-assert.yaml         # Initial state check
│   │   ├── 01-pvc.yaml            # Create PVC
│   │   ├── 01-assert.yaml         # Assert PVC is bound
│   │   ├── 02-writer-pod.yaml     # Pod that writes data
│   │   ├── 02-assert.yaml         # Assert writer completed
│   │   ├── 03-delete-writer.yaml  # Delete writer pod
│   │   ├── 03-assert.yaml         # Assert deletion
│   │   ├── 04-reader-pod.yaml     # Pod that reads data on different node
│   │   └── 04-assert.yaml         # Assert reader succeeded
│   ├── rwx-multi-pod/             # Test: Multiple pods with RWX
│   ├── snapshot-restore/          # Test: Snapshot and restore
│   └── volume-expansion/          # Test: Volume expansion
└── README.md
```

## Running Tests

### Run All Tests
```bash
kubectl kuttl test
```

### Run Specific Test
```bash
kubectl kuttl test --test rwo-pvc-migration
```

### Run with Custom Timeout
```bash
kubectl kuttl test --timeout 600
```

### Run Against Specific Namespace
```bash
kubectl kuttl test --namespace csi-tests
```

### Verbose Output (for debugging)
```bash
kubectl kuttl test --config kuttl-testsuite.yaml --suppress=
```

## Configuration

Edit `kuttl-testsuite.yaml` to adjust:
- **timeout**: Maximum time for each test step (default: 300s)
- **parallel**: Number of tests to run in parallel
- **testDirs**: Directories containing tests

### Storage Class Configuration

In each test that creates a PVC (e.g., `01-pvc.yaml`), uncomment and set your storage class:

```yaml
spec:
  storageClassName: your-csi-driver-storage-class
```

## Test Details

### ⭐ Clean Shutdown Test (NEW - CRITICAL)

**Purpose**: Verify that SPDK blobstore properly handles clean shutdown operations with all required patches applied.

**Critical Issue**: Without patches, blobstore isn't marked "clean" on unmount → 3-5 minute recovery on every pod restart.

**Steps**:
1. Create PVC and write test data
2. Delete pod (triggers clean shutdown)
3. Verify SPDK logs show "BLOBSTORE UNLOAD COMPLETE"
4. Remount volume in new pod (must complete < 30 seconds)
5. Verify no recovery was triggered ("Clean blobstore, no recovery needed")
6. Test rapid mount/unmount cycles
7. Verify data integrity throughout

**What it tests**:
- SPDK patch application (lvol-flush, ublk-debug, blob-shutdown-debug, blob-recovery-progress)
- FLUSH support through entire stack
- Blobstore clean shutdown sequence
- Fast remount without recovery
- Production-ready pod migration performance

**See**: `tests/clean-shutdown/README.md` for detailed documentation

### RWO PVC Migration Test

**Purpose**: Verify that data written to a RWO PVC persists and can be read by another pod on a different node.

**Steps**:
1. Create a PVC with ReadWriteOnce access mode
2. Verify PVC is bound
3. Create a writer pod that:
   - Writes data to the volume
   - Calls sync to flush data
   - Completes successfully
4. Delete the writer pod
5. Create a reader pod on a different node
6. Verify the reader pod can read the written data

**What it tests**:
- Volume provisioning
- Volume attachment/detachment
- Data persistence
- Node migration (pod rescheduling)

## Creating New Tests

1. Create a new directory under `tests/`
2. Add numbered YAML files for each step:
   - `XX-*.yaml` - Resources to create/update
   - `XX-assert.yaml` - Assertions to verify

### Example Test Structure

```
tests/my-new-test/
├── 00-setup.yaml       # Create initial resources
├── 00-assert.yaml      # Verify setup completed
├── 01-test-step.yaml   # Execute test action
├── 01-assert.yaml      # Verify test step succeeded
└── 02-cleanup.yaml     # Optional cleanup
```

## Debugging Failed Tests

### View Test Logs
```bash
# Kuttl creates temporary namespaces like kuttl-test-*
kubectl get pods -A | grep kuttl-test
kubectl logs <pod-name> -n <namespace>
```

### Keep Test Resources After Failure
```bash
kubectl kuttl test --skip-delete
```

### Check Events
```bash
kubectl get events -n <test-namespace> --sort-by='.lastTimestamp'
```

## Advanced Features

### Using TestAssert for Custom Commands

```yaml
apiVersion: kuttl.dev/v1beta1
kind: TestAssert
commands:
  - command: kubectl exec reader-pod -- cat /data/test-file.txt
    ignoreFailure: false
```

### Using TestStep for Complex Logic

```yaml
apiVersion: kuttl.dev/v1beta1
kind: TestStep
commands:
  - command: kubectl apply -f custom-resource.yaml
  - command: sleep 5
  - command: kubectl wait --for=condition=ready pod/my-pod --timeout=60s
```

### Node Affinity

To test specific node scenarios, use node selectors:

```yaml
spec:
  nodeSelector:
    kubernetes.io/hostname: specific-node-name
```

Or anti-affinity to ensure different nodes:

```yaml
spec:
  affinity:
    podAntiAffinity:
      requiredDuringSchedulingIgnoredDuringExecution:
        - labelSelector:
            matchLabels:
              app: writer
          topologyKey: kubernetes.io/hostname
```

## CI/CD Integration

### GitHub Actions Example

```yaml
name: CSI Driver Tests
on: [push, pull_request]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3
      - name: Install Kuttl
        run: |
          curl -LO https://github.com/kudobuilder/kuttl/releases/download/v0.15.0/kubectl-kuttl_0.15.0_linux_x86_64
          chmod +x kubectl-kuttl_0.15.0_linux_x86_64
          sudo mv kubectl-kuttl_0.15.0_linux_x86_64 /usr/local/bin/kubectl-kuttl
      - name: Setup K8s cluster
        # ... your cluster setup
      - name: Run Tests
        run: kubectl kuttl test --config kuttl-testsuite.yaml
```

## Best Practices

1. **Keep tests independent**: Each test should be self-contained
2. **Use unique names**: Avoid naming conflicts between tests
3. **Add assertions**: Verify every important state change
4. **Clean up resources**: Use `$patch: delete` or rely on namespace cleanup
5. **Test negative cases**: Include tests for failure scenarios
6. **Use meaningful timeouts**: Adjust based on expected operation duration

## Troubleshooting

| Issue | Solution |
|-------|----------|
| PVC not binding | Check storage class, CSI driver logs |
| Pod stuck pending | Check node resources, taints, affinity rules |
| Test timeout | Increase timeout in kuttl-testsuite.yaml |
| Data not persisting | Check CSI driver attach/detach logic |
| Anti-affinity not working | Ensure multiple nodes available in cluster |

## Additional Resources

- [Kuttl Documentation](https://kuttl.dev/)
- [Kubernetes CSI Documentation](https://kubernetes-csi.github.io/docs/)
- [CSI Driver Testing Best Practices](https://kubernetes-csi.github.io/docs/testing-drivers.html)
