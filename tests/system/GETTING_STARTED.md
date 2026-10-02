# Getting Started with CSI Driver Testing

## What You Have

A complete, production-ready test framework for testing CSI drivers on Kubernetes using **Chainsaw** - a declarative, YAML-based testing tool (converted from kuttl 2026-10-02; README.md, "Migration from kuttl").

## Why This Approach?

✅ **Declarative** - Pure YAML, easy to maintain
✅ **No Code Required** - Anyone can read and modify tests
✅ **Modern** - Used by major K8s projects
✅ **Comprehensive** - 4 complete test suites included
✅ **CI/CD Ready** - GitHub Actions workflow included

## Quick Start

### 1. Prerequisites Check

⚠️ **CRITICAL**: Before running tests, ensure:
- ✅ ublk kernel module is loaded on all worker nodes
- ✅ CSI driver pods restarted after loading ublk module
- ✅ kubectl configured with cluster access
- ✅ Chainsaw installed (`brew install kyverno/chainsaw/chainsaw`)

```bash
# Verify ublk module on each node
ssh user@worker-node "lsmod | grep ublk"

# If not loaded, load it and restart CSI pods (see Troubleshooting section)
```

### 2. Run Your First Test
```bash
# Run a single test
make test-rwo

# Or run all tests (standard tests in parallel, then clean-shutdown separately)
make test

# Or run individual test suites
make test-multi-replica
make test-clean-shutdown   # Runs in isolation
make test-snapshot
make test-expand
```

### 3. View Results
Tests create temporary namespaces (`chainsaw-*`) and clean them up when they finish.

On failure, resources are kept for debugging:
```bash
kubectl get ns | grep chainsaw-
kubectl get all -n <namespace>
```

## What Tests Are Included?

### 1. **RWO PVC Migration** (`tests-standard/rwo-pvc-migration/`)
Tests pod migration between nodes with data persistence
- **Use Case**: Simulates pod rescheduling or node failures
- **Duration**: ~2-3 minutes

### 2. **Multi-Replica** (`tests-standard/multi-replica/`)
Tests multi-replica volume support
- **Use Case**: High availability storage with multiple replicas
- **Duration**: ~2-3 minutes

### 3. **Volume Expansion** (`tests-standard/volume-expansion/`)
Tests online volume expansion without data loss
- **Use Case**: Growing storage needs (databases, etc.)
- **Duration**: ~3-4 minutes

### 4. **Snapshot & Restore** (`tests-standard/snapshot-restore/`)
Tests volume snapshot and restore functionality
- **Use Case**: Backup/restore, environment cloning
- **Duration**: ~3-4 minutes

## Configuration

### Update Storage Class

Before running tests, update the storage class in PVC manifests:

```yaml
# In files like tests-standard/*/00-pvc.yaml or tests-standard/*/01-pvc.yaml
spec:
  storageClassName: your-csi-driver-storage-class  # <-- Update this
```

The SPDK tests ask for `flint`, the nfs-only tests for `flint-nfs`; the
rigs (`kind-nfs-only.sh`, `kind-spdk.sh`) create what they need.

### Update Snapshot Class

For the snapshot test, ensure you have a VolumeSnapshotClass:

```yaml
# In tests-standard/snapshot-restore/01-snapshot.yaml
spec:
  volumeSnapshotClassName: csi-snapclass  # <-- Update this
```

## Command Reference

| Command | Description |
|---------|-------------|
| `make test` | Run all tests (standard + clean-shutdown) |
| `make test-rwo` | Run RWO migration test |
| `make test-multi-replica` | Run multi-replica test |
| `make test-clean-shutdown` | Run clean shutdown test (isolated) |
| `make test-snapshot` | Run snapshot restore test |
| `make test-expand` | Run volume expansion test |
| `make test-single TEST=<name>` | Run specific test |
| `make test-verbose` | Run with detailed output |
| `make debug` | Keep resources on failure |
| `make clean` | Remove test namespaces |
| `make list-tests` | Show available tests |
| `make restart-csi` | Restart CSI driver pods |

## Understanding Test Structure

Each test follows this pattern:

```
tests-standard/my-test/
├── 00-*.yaml       # Setup: Create initial resources
├── 00-assert.yaml  # Assert setup completed
├── 01-*.yaml       # Action: Execute test operation
├── 01-assert.yaml  # Assert operation succeeded
├── 02-*.yaml       # Next step...
└── 02-assert.yaml  # ...and verification
```

Chainsaw runs the steps of a test's `chainsaw-test.yaml` in order; an `assert` retries until it holds or its timeout expires.

## Creating Your Own Tests

1. **Create test directory**:
   ```bash
   mkdir tests-standard/my-new-test
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
   chainsaw test --config chainsaw-standard.yaml --test-dir tests-standard/my-new-test
   ```

## CI/CD Integration

You can integrate these tests into your CI/CD pipeline. See the **CI/CD Integration** section in `README.md` for a GitHub Actions example that you can adapt for your needs.

## Debugging Failed Tests

### View Logs
```bash
# Find test namespace
kubectl get ns | grep chainsaw-

# View pod logs
kubectl logs <pod-name> -n <test-namespace>

# View events
kubectl get events -n <test-namespace> --sort-by='.lastTimestamp'
```

### Keep Resources
```bash
# Run test and keep resources on failure
chainsaw test --config chainsaw-standard.yaml --test-dir tests-standard --skip-delete
```

### Increase Timeout
Edit the suite's `chainsaw-*.yaml`:
```yaml
spec:
  timeouts:
    assert: 600s  # and apply/delete/error/exec/cleanup alike
```

## Common Issues & Troubleshooting

### Critical: UBLK Driver Setup

⚠️ **Before running tests**, ensure the ublk kernel module is loaded on all worker nodes.

**Symptoms**:
- Tests fail with `MountVolume.MountDevice failed`
- Error: `SPDK RPC call 'ublk_start_disk' failed: Code=-19 Msg=No such device`

**Fix**:
```bash
# 1. Load ublk module on ALL worker nodes
ssh user@worker-node
sudo modprobe ublk_drv
lsmod | grep ublk

# Make it persistent
echo "ublk_drv" | sudo tee /etc/modules-load.d/ublk.conf

# 2. Restart CSI driver pods (REQUIRED after loading module)
kubectl delete pods -n flint-system -l app=flint-csi-node
kubectl delete pods -n flint-system -l app=flint-csi-controller

# 3. Wait for restart
kubectl wait --for=condition=ready pod -l app=flint-csi-node -n flint-system --timeout=120s

# 4. Clean up test namespaces and retry
kubectl get ns -o name | grep '^namespace/chainsaw-' | xargs -r kubectl delete
```

### Other Common Problems

| Problem | Solution |
|---------|----------|
| PVC stays Pending | Check storage class exists: `kubectl get sc` |
| Pod can't schedule | Check node resources: `kubectl describe nodes` |
| Anti-affinity fails | Need 2+ nodes in cluster |
| Snapshot test fails | Install snapshot CRDs and controller |
| Test timeout | Increase `timeouts` in the suite's `chainsaw-*.yaml` |
| Mount device failed | **See UBLK Driver Setup above** |

## Next Steps

1. ✅ Ensure ublk module is loaded and CSI pods restarted
2. ✅ Run `make test-rwo` to verify basic functionality
3. ✅ Review test results
4. ✅ Run additional tests (`make test` for all)
5. ✅ Customize tests for your specific needs

## Documentation

- **README.md** - Comprehensive documentation with troubleshooting
- **Test READMEs** - Each test directory has detailed documentation
- **Chainsaw Docs** - https://kyverno.github.io/chainsaw/

## Support

If tests fail:
1. Check CSI driver logs: `kubectl logs -l app=csi-driver`
2. Review test resources: `kubectl get all -n <test-namespace>`
3. Check events: `kubectl get events -n <test-namespace>`
4. Run with verbose output: `make test-verbose`

---

**Happy Testing! 🚀**

Your Flint CSI driver testing framework is ready to use. Ensure ublk is loaded, restart CSI pods with `make restart-csi`, and run tests with `make test`.
