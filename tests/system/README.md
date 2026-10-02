# CSI Driver System Test Framework

This is a declarative test framework for testing CSI drivers on Kubernetes using [Chainsaw](https://kyverno.github.io/chainsaw/) (converted from kuttl 2026-10-02; see [Migration from kuttl](#migration-from-kuttl)).

## Prerequisites

1. **Install Chainsaw** (CI pins v0.2.15)
   ```bash
   brew install kyverno/chainsaw/chainsaw
   # or: go install github.com/kyverno/chainsaw@v0.2.15
   # or: https://github.com/kyverno/chainsaw/releases
   ```

2. **Kubernetes Cluster**
   - A running Kubernetes cluster with Flint CSI driver installed
   - `kubectl` configured to access the cluster

3. **⚠️ UBLK Kernel Module (REQUIRED)**
   - The `ublk_drv` kernel module must be loaded on **all worker nodes**
   - Load it with: `sudo modprobe ublk_drv`
   - Verify with: `lsmod | grep ublk`
   - Make persistent: `echo "ublk_drv" | sudo tee /etc/modules-load.d/ublk.conf`
   - **After loading the module, you MUST restart all CSI driver pods** (see Troubleshooting section)

## Project Structure

```
tests/system/
├── chainsaw-standard.yaml          # suite configs: one per suite
├── chainsaw-nfs-only.yaml
├── chainsaw-clean-shutdown.yaml
├── chainsaw-replica-rebuild.yaml
├── kind-nfs-only.sh                # build this tree, run nfs-only on kind (the CI gate)
├── kind-spdk.sh                    # SPDK suites on kind over one real disk (single-host limits: README below)
├── tests-standard/                 # SPDK suite (StorageClass `flint`)
│   ├── rwo-pvc-migration/
│   │   ├── chainsaw-test.yaml      # the steps, in order
│   │   ├── 01-pvc.yaml  01-assert.yaml  02-writer-pod.yaml  02-assert.yaml
│   │   └── 04-reader-pod.yaml  04-assert.yaml
│   ├── ephemeral-inline/  multi-replica/  pvc-clone/  rox-multi-pod/
│   └── rwx-single-replica/  snapshot-restore/  volume-expansion/
├── tests-nfs-only/                 # no-SPDK suite (StorageClass `flint-nfs`)
│   └── ephemeral-inline/  rwo-pvc-migration/  rwx-single-replica/  volume-expansion/
├── tests/clean-shutdown/           # SPDK, run alone (chainsaw-clean-shutdown.yaml)
├── tests-replica-rebuild/          # SPDK, run alone (chainsaw-replica-rebuild.yaml)
└── results/                        # dated run evidence (e.g. 2026-10-02-aws-spdk)
```

Steps that were kuttl TestStep/TestAssert files are now operations inside
each test's `chainsaw-test.yaml`; the numbered YAML files that remain are
the resources those operations apply or assert.

## Running Tests

There are two Chainsaw suites plus two standalone isolated tests. Each
test is a directory holding a `chainsaw-test.yaml` (the steps) and the
resource files it applies and asserts:

| Suite | Config | Test dir | Tests | StorageClass | Backend |
|-------|--------|----------|-------|--------------|---------|
| SPDK (standard) | `chainsaw-standard.yaml` | `tests-standard` | 8 | `flint` (`nfsEmptyDir: false`) | SPDK blobstore |
| NFS-only | `chainsaw-nfs-only.yaml` | `tests-nfs-only` | 4 | `flint-nfs` (`nfsEmptyDir: true`) | emptyDir + NFS |
| Clean shutdown | `chainsaw-clean-shutdown.yaml` | `tests` | 1 | `flint` | SPDK blobstore |
| Replica rebuild | `chainsaw-replica-rebuild.yaml` | `tests-replica-rebuild` | 1 | created in-test (`numReplicas: 2`) | SPDK blobstore |

**CI.** `.github/workflows/system-chainsaw.yml` runs the NFS-only suite on
every change to the driver, its path dependencies, the charts or these
tests: `./kind-nfs-only.sh` builds the driver image from the tree the way
it is published (cargo-zigbuild musl + `Dockerfile.csi.prebuilt`), starts a
3-node kind cluster pulling from a local registry, installs the chart with
`deployment.nodeMode=nfs-only`, and runs the suite. The same script
reproduces a CI result on any Linux host with docker. The SPDK suites need
hugepages and a ublk-capable kernel, which a hosted runner lacks; they stay
on the metal/AWS rigs.

The replica-rebuild test kills a replica leg's spdk-tgt under active writes
and asserts the incremental heal pipeline (stale → catch-up → hot rejoin →
in_sync) with zero acked-write loss — see
`tests-replica-rebuild/replica-rebuild/README.md` for requirements and
isolation rules.

### Pre-requisites

Both StorageClasses must exist before running:

```bash
# Check existing classes
kubectl get sc

# The Helm chart creates 'flint'. Create 'flint-nfs' manually:
kubectl apply -f - <<EOF
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: flint-nfs
provisioner: disk.csi.chert.us
parameters:
  nfsEmptyDir: "true"
  numReplicas: "1"
  thinProvision: "false"
reclaimPolicy: Delete
volumeBindingMode: WaitForFirstConsumer
allowVolumeExpansion: true
EOF
```

For AWS clusters using NVMe (i3en instances), also ensure:
```bash
# Kernel modules on all nodes:
modprobe nvme-fabrics nvme-tcp

# Hugepages (2Gi per node) — kubelet restart needed after:
echo 1024 > /proc/sys/vm/nr_hugepages

# Disk initialization via node-agent API at PCI address (e.g. 0000:00:1f.0)
```

### Clean up before each run

Always ensure no leftover state from previous runs:

```bash
# Delete any orphaned test namespaces (Chainsaw names them chainsaw-*)
kubectl get ns -o name | grep '^namespace/chainsaw-' | xargs -r kubectl delete --wait=true

# Delete any orphaned NFS server pods
kubectl -n flint-system get pods | grep "flint-nfs-pvc-" | grep -E "Error|Completed" | awk '{print $1}' | xargs -r kubectl -n flint-system delete pod --force --grace-period=0

# Verify all CSI node pods are healthy (4/4 Running, 0 restarts ideal)
kubectl -n flint-system get pods -l app=flint-csi-node -o wide
```

### Run All Tests
```bash
# 1. SPDK suite (8 tests, parallel)
chainsaw test --config chainsaw-standard.yaml --test-dir tests-standard

# 2. NFS-only suite (4 tests, parallel)
chainsaw test --config chainsaw-nfs-only.yaml --test-dir tests-nfs-only

# 3. Clean shutdown test (runs in isolation)
chainsaw test --config chainsaw-clean-shutdown.yaml --test-dir tests

# 4. Replica rebuild (runs in isolation; see its README first)
chainsaw test --config chainsaw-replica-rebuild.yaml --test-dir tests-replica-rebuild
```

### Run Specific Test
```bash
chainsaw test --config chainsaw-standard.yaml --test-dir tests-standard/rwo-pvc-migration
chainsaw test --config chainsaw-standard.yaml --test-dir tests-standard/multi-replica
chainsaw test --config chainsaw-standard.yaml --test-dir tests-standard/snapshot-restore
chainsaw test --config chainsaw-standard.yaml --test-dir tests-standard/volume-expansion
```

### Run with Custom Timeout
```bash
chainsaw test --config chainsaw-standard.yaml --test-dir tests-standard --assert-timeout 600s --exec-timeout 600s
```

### Verbose Output (for debugging)
```bash
chainsaw test --config chainsaw-standard.yaml --test-dir tests-standard  # every operation's output is printed by default
```

## Configuration

Edit the suite's `chainsaw-*.yaml` to adjust:
- **timeouts**: the default for every operation (kuttl's suite timeout). An
  operation in a `chainsaw-test.yaml` that sets its own `timeout` overrides it.
- **execution.parallel**: Number of tests to run in parallel

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

**⚠️ Important**: This test **must run in isolation** (not in parallel with other tests) to ensure clean SPDK logs and accurate verification of shutdown behavior. Use `make test-clean-shutdown` or the dedicated config file.

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

1. Create a new directory under the suite's test dir (e.g. `tests-standard/`)
2. Add the resource files, and a `chainsaw-test.yaml` whose steps `apply`
   and `assert` them by `file:` (copy an existing test). Prefer the
   declarative operations (`apply`, `assert`, `error`, `delete`) over a
   `script`; a script runs ONCE, so a script that asserts must loop until
   its condition holds (see the converted tests).

### Example Test Structure

```
tests/my-new-test/
├── chainsaw-test.yaml  # The steps, in order
├── 00-setup.yaml       # Create initial resources
├── 00-assert.yaml      # Verify setup completed
├── 01-test-step.yaml   # Execute test action
└── 01-assert.yaml      # Verify test step succeeded
```

## Debugging Failed Tests

### View Test Logs
```bash
# Chainsaw creates temporary namespaces like chainsaw-<adjective>-<animal>
kubectl get pods -A | grep chainsaw-
kubectl logs <pod-name> -n <namespace>
```

### Keep Test Resources After Failure
```bash
chainsaw test --config chainsaw-standard.yaml --test-dir tests-standard --skip-delete
```

### Check Events
```bash
kubectl get events -n <test-namespace> --sort-by='.lastTimestamp'
```

## Advanced Features

### Asserting with a command

A Chainsaw `script` runs once. To assert a condition that takes time to
hold, loop until it does and let the operation's `timeout` bound it:

```yaml
- script:
    shell: bash
    timeout: 60s
    content: |
      until kubectl -n "$NAMESPACE" exec reader-pod -- cat /data/test-file.txt; do sleep 2; done
```

### Asserting that something is gone

```yaml
- error:
    resource:
      apiVersion: v1
      kind: Pod
      metadata:
        name: writer-pod
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

`.github/workflows/system-chainsaw.yml` is the gate; it runs
`./kind-nfs-only.sh` and nothing else. Run that script on a Linux host with
docker, kind, helm, kubectl, chainsaw and cargo-zigbuild to reproduce a run
(`KEEP=1` leaves the cluster up; it prints the private KUBECONFIG).

## Best Practices

1. **Keep tests independent**: Each test should be self-contained
2. **Use unique names**: Avoid naming conflicts between tests
3. **Add assertions**: Verify every important state change
4. **Clean up resources**: Use a `delete` operation, or rely on namespace cleanup. (kuttl's `$patch: delete` form deleted NOTHING: kuttl >= 0.15 ignored it.)
5. **Test negative cases**: Include tests for failure scenarios
6. **Use meaningful timeouts**: Adjust based on expected operation duration

## Troubleshooting

### UBLK Driver Issues

**Problem**: Test pods fail to mount volumes with error:
```
MountVolume.MountDevice failed for volume "pvc-xxx" : rpc error: code = Internal 
desc = Failed to create ublk device: Node agent HTTP call failed: 
{"error":"SPDK RPC call 'ublk_start_disk' failed: SPDK RPC error: Code=-19 Msg=No such device"}
```

**Solution**: The ublk kernel module must be loaded on all worker nodes **before** starting the CSI driver.

#### Step-by-Step Fix:

1. **Load the ublk module on all nodes**:
   ```bash
   # SSH to each worker node and run:
   sudo modprobe ublk_drv
   
   # Verify it's loaded:
   lsmod | grep ublk
   ```

2. **Make ublk module persistent across reboots**:
   ```bash
   # On each node:
   echo "ublk_drv" | sudo tee /etc/modules-load.d/ublk.conf
   ```

3. **Restart all Flint CSI driver pods**:
   ```bash
   # Delete node agent pods (they will be recreated by DaemonSet)
   kubectl delete pods -n flint-system -l app=flint-csi-node
   
   # Delete controller pods
   kubectl delete pods -n flint-system -l app=flint-csi-controller
   
   # Wait for pods to restart
   kubectl wait --for=condition=ready pod -l app=flint-csi-node -n flint-system --timeout=120s
   kubectl wait --for=condition=ready pod -l app=flint-csi-controller -n flint-system --timeout=120s
   ```

4. **Verify CSI driver is healthy**:
   ```bash
   # Check all pods are running
   kubectl get pods -n flint-system
   
   # Check node agent logs
   kubectl logs -n flint-system -l app=flint-csi-node --tail=50
   ```

5. **Clean up any stuck test resources and retry**:
   ```bash
   # Clean up test namespaces
   kubectl get ns -o name | grep '^namespace/chainsaw-' | xargs -r kubectl delete

   # Run tests again
   KUBECONFIG=/path/to/kubeconfig chainsaw test --config chainsaw-standard.yaml --test-dir tests-standard
   ```

### Stale NFS Mount Hang

**Problem**: A CSI node pod becomes unresponsive (no new log output). New pods
on that node get stuck in `ContainerCreating` with "connection refused" on the
CSI socket.

**Root cause**: A previous test run's NFS server pod was killed (namespace
deletion) while a client still held a mount. The CSI driver's
`NodeUnpublishVolume` tries to unmount the dead NFS mount and the kernel enters
D-state (uninterruptible sleep) on `umount -f`. The `timeout` wrapper cannot
kill a D-state process.

**Fix (applied in 1.0.0)**: The driver now tries `umount -l` (lazy-only) first,
which detaches the VFS mount point immediately without contacting the server.
Only falls back to `umount -f -l` if the lazy attempt fails.

**Recovery if it happens**:
```bash
# Identify the stuck node pod
kubectl -n flint-system get pods -l app=flint-csi-node -o wide

# Delete any errored NFS pods on that node
kubectl -n flint-system get pods | grep "flint-nfs-pvc-" | grep Error | awk '{print $1}' | xargs -r kubectl -n flint-system delete pod --force --grace-period=0

# Restart the stuck CSI node pod (DaemonSet recreates it)
kubectl -n flint-system delete pod <stuck-node-pod>

# Wait for it to come back
kubectl -n flint-system wait --for=condition=ready pod -l app=flint-csi-node --timeout=120s
```

### Common Test Failures

| Issue | Solution |
|-------|----------|
| PVC not binding | Check storage class exists: `kubectl get sc` |
| Pod stuck pending | Check node resources, taints, affinity rules |
| Test timeout | Increase `timeouts` in the suite's chainsaw-*.yaml |
| Data not persisting | Check CSI driver attach/detach logic |
| Anti-affinity not working | Ensure multiple nodes available in cluster |
| Mount device failed | **See UBLK Driver Issues above** |
| CSI socket "connection refused" | **See Stale NFS Mount Hang above** |
| NFS-only tests fail with `flint` SC | Ensure `flint-nfs` StorageClass exists |

## Additional Resources

- [Chainsaw Documentation](https://kyverno.github.io/chainsaw/)
- [Kubernetes CSI Documentation](https://kubernetes-csi.github.io/docs/)
- [CSI Driver Testing Best Practices](https://kubernetes-csi.github.io/docs/testing-drivers.html)

## Migration from kuttl

Converted 2026-10-02. `chainsaw migrate kuttl` converted 6 of the 14 tests
and silently changed the rest (it dropped every TestAssert `timeout`, and
it turned retried TestAssert commands into run-once scripts), so the
conversion was done by a script that keeps kuttl's semantics:

- step order: TestStep `delete`, then `commands`, then the applied files,
  then the asserts;
- a TestAssert `timeout` becomes that step's assert timeout; the suite
  timeout becomes the configuration's default for every operation;
- a TestAssert command, which kuttl RETRIED until the timeout, becomes a
  script that loops until it passes;
- every script runs under bash (the scripts use `&>`, which dash, Ubuntu's
  `sh`, parses as "background, then truncate", so `if cmd &> /dev/null`
  always took the then-branch).

What the conversion found, and changed on purpose:

- **Every nfs-only PVC was unservable** (driver defect, fixed with the
  migration). The first kind run of the nfs-only suite showed each
  per-volume NFS server crash-looping on `F30 REFUSAL (exit 57): export
  "/mnt/volume" has neither identity marker nor flint state`. F30
  (2026-07-21) stamps the marker at NodeStage, but the emptyDir backend is
  a hostPath the kubelet creates empty and nothing stages. The controller
  now passes `--fresh-backing` for that backend only, and the server stamps
  a WHOLLY empty export at first boot (any other unmarked export is still
  refused). Nothing had run this suite since before F30.

- **`$patch: delete` deleted nothing.** kuttl >= 0.15 ignores that form
  (re-applies the object, warns "unknown field"). Ten deletes across six
  tests were no-ops, including the writer delete in `rwo-pvc-migration`,
  whose reader therefore mounted a volume the writer still held. They are
  now `delete` operations.
- **`rwo-pvc-migration` pinned "a different node" with a pod anti-affinity
  on the writer's label**, which holds nothing once the writer is deleted.
  The writer's node is now recorded, the reader is scheduled with a
  `NotIn` node affinity, and the step asserts the two nodes differ.
- **`rwx-single-replica`'s `03-cleanup` re-applied bare objects** (no
  `$patch` at all), a no-op; it is now `delete` operations.
- **`kubectl get pod X --ignore-not-found` exits 0 whether or not X
  exists**, so the "writer is gone" asserts could not fail. They are
  `error` operations now.
- A TestStep `timeout` (kuttl has no such field; it was ignored) and
  `ignoreFailure` on a TestAssert command (not a field there either) were
  dropped, as kuttl dropped them.

- **An RWO nfs-only volume could never move nodes** (driver defect, fixed
  with the migration). NodeUnstage takes the unmount-only path for volumes
  whose PV is RWX/ROX or pNFS; an RWO claim on the nfs-only class resolves
  to the Block role, so it fell through to the SPDK teardown, which fails
  on an nfs-only node (no `spdk.sock`). The unstage failed on every retry,
  the kubelet kept the volume in use, it never detached, and the reader on
  another node sat on `Multi-Attach error`. NodeUnstage now also reads the
  PV's `nfs.chert.us/backend` and unstages `emptydir` volumes unmount-only.
  (Under kuttl the writer was never deleted, so the test never reached it.)

**Seen once, not reproduced:** in one parallel run the RWO writers hung in
`close()` (D state in `nfs_wb_all`, the client's state manager in open-state
recovery) before the NodeUnstage fix; three later runs, one with the same
parallelism, did not show it. A hung hard NFS mount leaves D-state tasks in
the HOST kernel that outlive the kind cluster: clear them with
`nsenter -t <pid> -m umount -f /data` (the force flag aborts the RPCs even
when the umount itself reports busy).

Not changed, still weak: `clean-shutdown/03-verify-logs` (SPDK) passes
whether or not the unload line is found, and `ephemeral-inline`'s cleanup
step (both suites) only logs whether the controller deleted the volume.
