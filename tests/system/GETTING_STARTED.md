# Getting Started with CSI Driver Testing

## What You Have

A complete, production-ready test framework for testing CSI drivers on Kubernetes using **Kuttl** - a declarative, YAML-based testing tool.

## Why This Approach?

✅ **Declarative** - Pure YAML, easy to maintain
✅ **No Code Required** - Anyone can read and modify tests
✅ **Modern** - Used by major K8s projects
✅ **Comprehensive** - 4 complete test suites included
✅ **CI/CD Ready** - GitHub Actions workflow included

## Quick Start

### 1. Prerequisites Check
```bash
# Run the interactive setup script
./quick-start.sh
```

This script will:
- Verify kubectl is installed
- Check cluster connectivity
- Install Kuttl if needed
- Detect your storage class
- Update test manifests automatically

### 2. Run Your First Test
```bash
# Run a single test
make test-rwo

# Or run all tests
make test
```

### 3. View Results
Tests create temporary namespaces (`kuttl-test-*`) and automatically clean up on success.

On failure, resources are kept for debugging:
```bash
kubectl get ns | grep kuttl-test
kubectl get all -n <namespace>
```

## What Tests Are Included?

### 1. **RWO PVC Migration** (`tests/rwo-pvc-migration/`)
Tests pod migration between nodes with data persistence
- **Use Case**: Simulates pod rescheduling or node failures
- **Duration**: ~2-3 minutes

### 2. **RWX Multi-Pod** (`tests/rwx-multi-pod/`)
Tests concurrent access from multiple pods
- **Use Case**: Shared file systems, collaborative workloads
- **Duration**: ~2-3 minutes

### 3. **Volume Expansion** (`tests/volume-expansion/`)
Tests online volume expansion without data loss
- **Use Case**: Growing storage needs (databases, etc.)
- **Duration**: ~3-4 minutes

### 4. **Snapshot & Restore** (`tests/snapshot-restore/`)
Tests volume snapshot and restore functionality
- **Use Case**: Backup/restore, environment cloning
- **Duration**: ~3-4 minutes

## Configuration

### Update Storage Class

Before running tests, update the storage class in PVC manifests:

```yaml
# In files like tests/*/00-pvc.yaml or tests/*/01-pvc.yaml
spec:
  storageClassName: your-csi-driver-storage-class  # <-- Update this
```

Or use the quick-start script which does this automatically.

### Update Snapshot Class

For the snapshot test, ensure you have a VolumeSnapshotClass:

```yaml
# In tests/snapshot-restore/01-snapshot.yaml
spec:
  volumeSnapshotClassName: csi-snapclass  # <-- Update this
```

## Command Reference

| Command | Description |
|---------|-------------|
| `make test` | Run all tests |
| `make test-rwo` | Run RWO migration test |
| `make test-rwx` | Run RWX multi-pod test |
| `make test-expand` | Run volume expansion test |
| `make test-single TEST=<name>` | Run specific test |
| `make test-verbose` | Run with detailed output |
| `make debug` | Keep resources on failure |
| `make clean` | Remove test namespaces |
| `make list-tests` | Show available tests |

## Understanding Test Structure

Each test follows this pattern:

```
tests/my-test/
├── 00-*.yaml       # Setup: Create initial resources
├── 00-assert.yaml  # Assert setup completed
├── 01-*.yaml       # Action: Execute test operation
├── 01-assert.yaml  # Assert operation succeeded
├── 02-*.yaml       # Next step...
└── 02-assert.yaml  # ...and verification
```

Kuttl executes files in alphabetical order, waiting for assertions before proceeding.

## Creating Your Own Tests

1. **Create test directory**:
   ```bash
   mkdir tests/my-new-test
   ```

2. **Add test steps** (numbered YAML files):
   ```yaml
   # 00-setup.yaml
   apiVersion: v1
   kind: PersistentVolumeClaim
   metadata:
     name: my-pvc
   spec:
     accessModes: [ReadWriteOnce]
     resources:
       requests:
         storage: 1Gi
   ```

3. **Add assertions**:
   ```yaml
   # 00-assert.yaml
   apiVersion: v1
   kind: PersistentVolumeClaim
   metadata:
     name: my-pvc
   status:
     phase: Bound
   ```

4. **Run your test**:
   ```bash
   kubectl kuttl test --test my-new-test
   ```

## CI/CD Integration

A complete GitHub Actions workflow is included in `.github/workflows/csi-tests.yaml`.

It automatically:
- Creates a Kind cluster
- Installs your CSI driver
- Runs all tests
- Collects logs on failure
- Uploads results

Just push to your repo and it runs!

## Debugging Failed Tests

### View Logs
```bash
# Find test namespace
kubectl get ns | grep kuttl-test

# View pod logs
kubectl logs <pod-name> -n <test-namespace>

# View events
kubectl get events -n <test-namespace> --sort-by='.lastTimestamp'
```

### Keep Resources
```bash
# Run test and keep resources on failure
kubectl kuttl test --skip-delete
```

### Increase Timeout
Edit `kuttl-testsuite.yaml`:
```yaml
timeout: 600  # Increase from 300 to 600 seconds
```

## Common Issues

| Problem | Solution |
|---------|----------|
| PVC stays Pending | Check storage class exists: `kubectl get sc` |
| Pod can't schedule | Check node resources: `kubectl describe nodes` |
| Anti-affinity fails | Need 2+ nodes in cluster |
| Snapshot test fails | Install snapshot CRDs and controller |
| Test timeout | Increase timeout in `kuttl-testsuite.yaml` |

## Next Steps

1. ✅ Run `./quick-start.sh` to validate setup
2. ✅ Run `make test-rwo` to verify basic functionality
3. ✅ Review test results
4. ✅ Customize tests for your specific CSI driver features
5. ✅ Add tests to your CI/CD pipeline

## Documentation

- **README.md** - Comprehensive documentation
- **PROJECT_STRUCTURE.md** - Detailed project overview
- **Kuttl Docs** - https://kuttl.dev/

## Support

If tests fail:
1. Check CSI driver logs: `kubectl logs -l app=csi-driver`
2. Review test resources: `kubectl get all -n <test-namespace>`
3. Check events: `kubectl get events -n <test-namespace>`
4. Run with verbose output: `make test-verbose`

---

**Happy Testing! 🚀**

Your CSI driver testing framework is ready to use. Start with `./quick-start.sh` and you'll be running tests in minutes.
