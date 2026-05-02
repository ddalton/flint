# Regression test suite — SPDK CSI

Locks down the existing SPDK driver's user-visible contract (see
`docs/spdk-driver-inventory.md`) before the `flint-pnfs-csi` carve-out
refactor begins. Each scenario is a self-contained bash script that
talks to a real Kubernetes cluster via `kubectl`.

## Scenarios

| Script | Backend | What it asserts | Time |
|---|---|---|---|
| `spdk-rwo-basic.sh` | A (block) | RWO PVC bind → write → restart pod → read → cleanup; PV carries the volume_context keys from inventory §6 | ~2 min |
| `spdk-rwo-snapshot.sh` *(planned)* | A | PVC + write → snapshot → new PVC from snapshot → data matches | |
| `spdk-rwo-clone.sh` *(planned)* | A | PVC + write → cloned PVC → data matches | |
| `spdk-rwo-expand.sh` *(planned)* | A | 1Gi → 2Gi expansion visible in pod | |
| `spdk-rwo-multi-replica.sh` *(planned)* | A (RAID-1) | `numReplicas: 2` → write → kill replica node → read still works | |
| `spdk-rwx-emptydir.sh` *(planned)* | B (NFS pod) | RWX with `nfsEmptyDir: true`, two pods on different nodes | |
| `spdk-rwx-pvc.sh` *(planned)* | B (NFS pod) | RWX backed by underlying PVC, two pods | |

## Running

Two modes: against an existing test cluster (preferred — exercises real
NVMe paths) or against a self-hosted kind cluster (for hermetic local
runs and CI).

### Mode A — kind cluster (hermetic, recommended for first run)

```bash
tests/regression/kind-up.sh                # creates kind cluster + helm install (~3-5 min)

# First time: capture baselines.
REGRESSION_RECORD=1 tests/regression/spdk-rwo-basic.sh

# Every subsequent run / refactor PR check:
tests/regression/spdk-rwo-basic.sh

tests/regression/kind-down.sh              # tear down when done
```

What kind buys you: a clean, reproducible cluster with the helm chart's
existing `kindMode` (a malloc-backed virtual disk inside the SPDK target)
giving Backend A a small but functional storage backend. RWX-via-NFS-pod
(Backend B) works fully because it's pure Kubernetes + `mount -t nfs4`.

What kind doesn't buy you: real NVMe-oF paths, hugepages, perf-realistic
numbers. The contract assertions still hold; the latency assertions
(none today, but if added later) wouldn't.

### Mode B — existing test cluster

```bash
# KUBECONFIG points at your real cluster with driver pre-installed.
REGRESSION_RECORD=1 tests/regression/spdk-rwo-basic.sh
tests/regression/spdk-rwo-basic.sh
```

The script `exit 0`s on PASS, `exit 1`s with a `FAIL: <reason>` line on
any contract violation. Diagnostic state (events, describe, controller +
node logs) is captured to `/tmp/regression-<scenario>-$$.log` on failure.

## Configuration

Override via environment:

| Variable | Default | Purpose |
|---|---|---|
| `REGRESSION_KUBECONFIG` | `$KUBECONFIG` or `~/.kube/config` | Cluster to test against |
| `REGRESSION_NAMESPACE` | `flint-regression` | Per-run scratch namespace; recreated each run |
| `REGRESSION_STORAGE_CLASS` | `flint` | StorageClass to use (helm chart default) |
| `REGRESSION_PROVISIONER` | `flint.csi.storage.io` | CSIDriver name for preflight check |
| `REGRESSION_TIMEOUT` | `180` | Per-step wait timeout in seconds |
| `REGRESSION_RECORD` | `0` | When `1`, captures baselines instead of comparing |

## Baselines

Stored at `tests/baseline/spdk-e2e/<scenario>.<key>`. Each is a single
line of text — most often a sorted, comma-joined set of expected keys.

For example, `spdk-rwo-basic.pv-attribute-keys` records the *exact set*
of `volume_context` keys present on a single-replica RWO PV. After a
refactor, if a key is added or removed, the baseline diff catches it
immediately. Updating a baseline is an explicit step (rerun with
`REGRESSION_RECORD=1`) so changes don't sneak through silently.

## Adding a new scenario

Each scenario is a single bash script that:

1. Sources `lib/common.sh`.
2. Calls `regression::setup "<scenario-name>"`.
3. Sets a `trap regression::teardown EXIT`.
4. Uses the helpers (`regression::apply`, `regression::wait_pvc_bound`,
   `regression::assert_pv_attribute`, `regression::baseline`, etc.) to
   exercise the flow.
5. Calls `regression::ok "<scenario-name> PASS"` at the end.

Look at `spdk-rwo-basic.sh` as the template — the structure is meant to
be mechanical to copy.

## What this suite is *not*

- Not a perf test. Wall-times are not asserted (latency is not part of
  the contract).
- Not a unit test replacement. `cargo test --release --lib` still runs
  separately.

## Open issues to resolve before scenario 2

- **No CI today.** The repo has no `.github/workflows/`. Once
  scenarios 1–7 exist, we should add a workflow that runs them on every
  refactor PR. The exact runner shape (kind in GitHub Actions vs.
  external test cluster reachable from CI) is a decision worth making
  before we lean on the suite as a merge gate.
- **Multi-node tests need a multi-node cluster.** Scenarios 5–7 won't
  pass on a single-node cluster. The harness will need to detect node
  count and either skip with a clear "requires N nodes" message, or
  bring up a multi-node kind cluster automatically.
- **`flint-system` namespace must exist** for Backend B (RWX) tests.
  The harness currently assumes the helm chart created it. Worth a
  preflight check before scenario 6/7 lands.
