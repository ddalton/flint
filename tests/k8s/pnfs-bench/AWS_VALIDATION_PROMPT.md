# Flint v1.0.0 — AWS multi-node validation prompt

Self-contained instructions for running pre-publish validation of a
Flint release candidate on a multi-node AWS Kubernetes cluster. The
validation machine doesn't need any context from the release-prep
session — everything load-bearing is in this file or pointed at via
repo paths.

**v1.0.0 publish has two validation phases. Both must pass before tag.**

| Phase | What | Runs on | Time |
|---|---|---|---|
| **1** | pNFS multi-host scalability bench (the architectural-claim test) + NVMe IOPS sanity check | Plain EKS, no SPDK setup | ~30 min |
| **2** | SPDK KUTTL system tests (multi-replica, snapshot-restore, volume-expansion, pvc-clone, ephemeral-inline) | Same cluster + ublk/hugepages bootstrap + Flint Helm install | ~45 min |

Phase 2 runs on the same cluster as Phase 1. Phase 1 is a clean run
that doesn't pollute the cluster — ublk and hugepages get added via
DaemonSet/userdata between phases.

If Phase 1 fails, fix on `main`, build a `:1.0.0-rc2` image, repeat
Phase 1. Don't proceed to Phase 2 until Phase 1 is green.

---

# Phase 1 — pNFS multi-host scalability validation

## Phase 1 pre-cluster setup

Two things must be ready before the Phase 1 Claude session runs:

* **The pNFS bench container image** must exist on a registry the
  cluster can pull from — see **(0) Build and publish the rc image**.
* **The cluster** must have ≥4 workers with local NVMe and ≥10 GbE
  inter-worker network — see **(1) Provision the cluster**.

What Phase 1 specifically does **not** require:

* `ublk_drv` kernel module loaded — pNFS bench pods don't use ublk.
* Hugepages reserved — pNFS bench pods don't use SPDK.
* `/dev/nvme1n1` formatted — bench uses `emptyDir` volumes (the
  harness comment is explicit: *"Local-disk emptyDir keeps the bench
  measuring pNFS, not network-attached PV layers"*).
* `VolumeSnapshot` CRDs — bench doesn't snapshot.

These all become Phase 2 prerequisites. Phase 1's setup is
deliberately minimal so a fresh EKS cluster is bench-ready in
~10 minutes.

### (0) Build and publish the rc image

The bench harness consumes a single env var, `PNFS_IMAGE`, pointing at
a pNFS image that bundles the `flint-pnfs-mds` and `flint-pnfs-ds`
binaries (Dockerfile at `spdk-csi-driver/docker/Dockerfile.pnfs`).
For v1.0.0 validation, build and push the image at the
**release-candidate tag** `1.0.0-rc1`, **not `latest`**. The rc tag
is immutable; `latest` would let a concurrent push silently change
the artifact between push and bench, making "what was actually
tested?" a forensic question.

You can run this from any x86-64 machine with Docker (your dev
laptop is fine). It does not have to be the validation machine; the
image just has to be pullable from Docker Hub by the time the bench
runs.

#### Pre-flight: confirm Docker daemon and `dilipdalton` login (no credential exposure)

These checks are read-only and never print the auth token to your
terminal — safe to run anywhere.

```bash
# 1. Daemon running?
docker version --format '{{.Server.Version}}' \
  || echo "Docker daemon not running — start Docker Desktop or 'sudo systemctl start docker'"

# 2. Already logged in as dilipdalton?
#    Two paths depending on how Docker stores credentials:

#    (a) Daemon is up — fastest, single command:
docker info 2>/dev/null | grep '^ Username:'
#    Expected: " Username: dilipdalton"
#    If empty: not logged in (or different account). Run: docker login -u dilipdalton

#    (b) Daemon is down OR you want to verify without starting it.
#        macOS (default credsStore: osxkeychain — entries live in Keychain):
security find-internet-password -s "index.docker.io" -a "dilipdalton" 2>&1 \
  | grep -E '^class|"acct"'
#    Expected output (no -g flag → password is never extracted):
#      class: "inet"
#          "acct"<blob>="dilipdalton"
#    Exit code 0 = logged in. Exit code 44 = not logged in for that account.

#        Linux with secretservice (GNOME/KDE keyring):
secret-tool search server index.docker.io 2>/dev/null | grep -E '^attribute.*username|^label'
#    Expected: a label or username attribute referencing dilipdalton.

#        Linux with pass (`pass` credential helper):
pass show docker-credential-helpers/aHR0cHM6Ly9pbmRleC5kb2NrZXIuaW8vdjEv/dilipdalton >/dev/null 2>&1 \
  && echo "logged in as dilipdalton" || echo "no pass entry — run docker login -u dilipdalton"
#    (`pass show` prints the secret if you don't redirect; the >/dev/null above
#     keeps the credential off the terminal — only the exit code is consulted.)

# 3. buildx available?
docker buildx version
#    Expected: any v0.8+ release. v0.12+ is recommended for clean
#    multi-arch behaviour but single-arch (--platform linux/amd64)
#    works on older versions too.
```

If check (2) fails — i.e., no `dilipdalton` credential found — run
`docker login -u dilipdalton` and provide a Docker Hub **access
token** (not your account password). Personal access tokens scoped
to "Read & Write" are sufficient and revocable; create one at
`https://hub.docker.com/settings/security` if you don't have one.

#### Build and push

```bash
# From the repo root, on an x86-64 machine with Docker:
git checkout main
git pull origin main

# Build and push linux/amd64 only (per the v1.0.0 release plan).
# buildx is required for --platform; if missing:
#   docker buildx create --use
docker buildx build \
  --platform linux/amd64 \
  --tag dilipdalton/flint-pnfs:1.0.0-rc1 \
  --file spdk-csi-driver/docker/Dockerfile.pnfs \
  --push \
  spdk-csi-driver

# Optional: also push :latest as a moving alias (free; same SHA).
docker buildx build \
  --platform linux/amd64 \
  --tag dilipdalton/flint-pnfs:latest \
  --file spdk-csi-driver/docker/Dockerfile.pnfs \
  --push \
  spdk-csi-driver
```

After the push, verify the image is pullable:

```bash
docker pull dilipdalton/flint-pnfs:1.0.0-rc1
docker inspect dilipdalton/flint-pnfs:1.0.0-rc1 --format '{{.Architecture}}'
# Expect: amd64
```

If Phase 1 validation fails, fix on `main`, build `:1.0.0-rc2` from
the new SHA, repeat. Don't reuse the `:1.0.0-rc1` tag.

### (1) Provision the cluster

Plain EKS managed nodegroup, 4 workers of `i3en.xlarge`, no
userdata required. Example with `eksctl`:

```yaml
# cluster.yaml — Phase 1 setup
apiVersion: eksctl.io/v1alpha5
kind: ClusterConfig
metadata:
  name: flint-validation
  region: us-east-1
managedNodeGroups:
  - name: flint-workers
    instanceType: i3en.xlarge
    desiredCapacity: 4
    minSize: 4
    maxSize: 4
    privateNetworking: true
    # No preBootstrapCommands needed for Phase 1.
    # See "Phase 2 pre-cluster setup additions" further down.
```

```bash
eksctl create cluster -f cluster.yaml
kubectl get nodes
# Expect: 4 worker nodes Ready.
```

### (2) NVMe IOPS sanity check (catches the wrong instance type)

`i3en.xlarge` advertises 1× 2.5 TB local NVMe SSD with ~50K random
read IOPS at QD=16. If you accidentally provisioned an instance
*without* local NVMe (e.g., `r6a.xlarge`, which has only EBS), or
the local NVMe didn't attach for some reason, the pNFS bench will
"work" but produce numbers that reflect EBS gp3 ceilings instead.
This check catches that ahead of time.

Apply the IOPS check Job (parallel across all 4 workers, runs ~30s
each, exits cleanly):

```yaml
# nvme-iops-check.yaml
apiVersion: batch/v1
kind: Job
metadata:
  name: nvme-iops-check
  namespace: kube-system
spec:
  parallelism: 4
  completions: 4
  completionMode: Indexed
  template:
    metadata:
      labels:
        app: nvme-iops-check
    spec:
      restartPolicy: Never
      tolerations:
        - operator: Exists
      affinity:
        podAntiAffinity:
          requiredDuringSchedulingIgnoredDuringExecution:
            - topologyKey: kubernetes.io/hostname
              labelSelector:
                matchLabels:
                  app: nvme-iops-check
      containers:
        - name: fio
          image: alpine:3.19
          securityContext:
            privileged: true
          command:
            - sh
            - -c
            - |
              set -e
              apk add --no-cache fio nvme-cli > /dev/null
              echo "═══ Node: $(hostname) ═══"
              echo "─── nvme list ───"
              nvme list || echo "(nvme-cli unavailable)"
              echo
              echo "─── lsblk ───"
              lsblk
              echo
              if [ ! -e /dev/nvme1n1 ]; then
                echo "FAIL: /dev/nvme1n1 not present on this node"
                echo "      (Are you sure this is i3en.xlarge or another instance with local NVMe?)"
                exit 1
              fi
              echo "─── fio randread 4K QD=16 30s (read-only) ───"
              fio --name=randread \
                  --filename=/dev/nvme1n1 \
                  --rw=randread \
                  --bs=4k \
                  --iodepth=16 \
                  --numjobs=4 \
                  --runtime=30 \
                  --time_based \
                  --direct=1 \
                  --readonly \
                  --group_reporting \
                  --output-format=normal | grep -E 'IOPS|BW|lat \(usec\)'
              echo
              echo "─── fio seqread 1M QD=16 15s (read-only) ───"
              fio --name=seqread \
                  --filename=/dev/nvme1n1 \
                  --rw=read \
                  --bs=1M \
                  --iodepth=16 \
                  --runtime=15 \
                  --time_based \
                  --direct=1 \
                  --readonly \
                  --group_reporting \
                  --output-format=normal | grep -E 'IOPS|BW|lat \(usec\)'
              echo
              echo "═══ Node $(hostname) complete ═══"
          volumeMounts:
            - name: dev
              mountPath: /dev
      volumes:
        - name: dev
          hostPath:
            path: /dev
            type: Directory
```

```bash
kubectl apply -f nvme-iops-check.yaml
kubectl wait --for=condition=complete -n kube-system job/nvme-iops-check --timeout=5m
kubectl logs -n kube-system -l app=nvme-iops-check --tail=-1 --prefix=true
```

**Pass criterion** (per node):
- `/dev/nvme1n1` present.
- 4K random read IOPS at QD=16 ≥ **30,000** (i3en.xlarge spec is
  ~50K; allow ~40% margin for environmental overhead).
- 1M sequential read bandwidth ≥ **1 GB/s** (spec is ~1.5 GB/s).

If any node fails, the cluster is not ready for the pNFS bench. Fix
the cluster (verify instance type, check `kubectl describe node` for
NVMe in `status.allocatable` / `status.capacity`) before proceeding.

This test is read-only; it does not write to or format
`/dev/nvme1n1`. SPDK can still claim the device unmodified for
Phase 2.

Cleanup after the check:

```bash
kubectl delete -f nvme-iops-check.yaml
```

### (3) Confirm the rc image is pullable from inside the cluster

```bash
kubectl run image-check --rm -it --image=dilipdalton/flint-pnfs:1.0.0-rc1 \
  --command --restart=Never -- /bin/true
# If this returns 0 the image pulled successfully on a worker node.
```

If the pull fails (private repo / Docker Hub rate-limit), set up
imagePullSecrets in the bench namespace before pasting the prompt.

---

## Phase 1 prompt — paste below into a fresh Claude Code session

I'm running Phase 1 of pre-publish validation for Flint CSI **v1.0.0** — the **pNFS multi-host scalability bench**. The release candidate is committed to `main` of `https://github.com/ddalton/flint`. The release tag `v1.0.0` has **not** been created yet — the publish is gated on this validation passing AND a separate Phase 2 (SPDK KUTTL tests) passing. **Do not run `git tag` or `gh release create`. Do not push images to the `:1.0.0` tag (only `:1.0.0-rc<N>` is OK). Do not push to `origin/main`. Do not run any SPDK-related tests in this session — Phase 2 is separate. Your job is the bench run and a written report.**

### Infrastructure I have

A Kubernetes cluster on AWS with **5 nodes** (1 control + 4 workers, all `i3en.xlarge`):
- 4 vCPU, 32 GB RAM each
- 1× 2.5 TB local NVMe per node (verified via the NVMe IOPS check)
- 25 Gbps inter-node network
- Plain EKS — no ublk, no hugepages, no Flint Helm install (those are Phase 2)
- VolumeSnapshot CRDs not installed (not needed for Phase 1)

`KUBECONFIG` is set in my shell. `kubectl get nodes` lists 5 nodes, ready. The pre-cluster NVMe IOPS check passed on all four workers.

### What you need to do

1. **Confirm the rc container image exists on Docker Hub.** The user has built and pushed `dilipdalton/flint-pnfs:1.0.0-rc1` from `spdk-csi-driver/docker/Dockerfile.pnfs`. Verify the image exists and is reachable with:
   ```bash
   docker manifest inspect dilipdalton/flint-pnfs:1.0.0-rc1
   ```
   If this fails, stop and ask the user to complete the pre-cluster image-build step. **Do not build the image from this session.**

2. **Run the cross-host pNFS bench**:
   ```bash
   KUBECONFIG=$KUBECONFIG \
     PNFS_IMAGE=dilipdalton/flint-pnfs:1.0.0-rc1 \
     MDS_NODE=<worker-1-name> \
     DS_NODES="<worker-2-name> <worker-3-name>" \
     CLIENT_NODE=<worker-4-name> \
     make test-pnfs-cross-host
   ```
   The harness creates a Namespace, MDS+DS Deployments with `nodeName` pins, and a client Deployment. It runs `bs={4K, 1M} × {read, write}` fio sweeps and dumps a TSV + a markdown table. Capture both.

3. **Report back with**:
   - The full TSV that `make test-pnfs-cross-host` produces.
   - The markdown table summary.
   - `kubectl logs` from each MDS pod and each DS pod (last 100 lines).
   - Whether the run hit the **pass criterion** documented in `tests/k8s/pnfs-bench/README.md`.
   - Any `OOMKilled`, `CrashLoopBackOff`, or `Error` pod statuses during the run.
   - Total wall-clock time of the bench run.

4. **If the bench fails to complete or the pass criterion is not hit**: do **not** delete the test namespace; leave the pods in their failed state. Capture full logs, kubectl describe outputs, and report the failure mode.

### Phase 1 success criteria

- `make test-pnfs-cross-host` exits 0.
- The TSV contains numbers across all four `bs × {read, write}` rows (no empty cells, no errors).
- Per-DS allocation is approximately balanced (each DS holds roughly half the written data; tolerance per the bench README).
- Pass criterion in `tests/k8s/pnfs-bench/README.md` is met.

### What to leave alone, and what's OK to fix in-session

**Hard prohibitions** (never do these — they irreversibly publish the release):

- Don't run `git tag v1.0.0`, `git push --tags`, or `gh release create`. Tagging is the release-prep session's job once **both** phases pass.
- Don't bump versions in `Cargo.toml`, `Chart.yaml`, or `CHANGELOG.md`.
- Don't push to the immutable `:1.0.0` image tag. Only `:1.0.0-rc<N>` tags are OK during validation.
- Don't run Phase 2 work — no Helm install, no KUTTL tests, no SPDK setup. The user runs Phase 2 separately after Phase 1 reports green.

**OK to fix in-session, with discipline** (lesson from the rc3 cycle: small blocker fixes don't need to wait for a session handoff — but they have to be done cleanly):

A small, well-scoped fix to a real release-blocker bug found during the bench may be committed directly to `main` if **all four** are true:

1. The fix is **localized** (a few files; not an architectural change).
2. The fix has a **regression test** that fails before the change and passes after — the test pins the fix so the bug can't silently recur.
3. The fix bumps the **rc tag** — push a new image at `:1.0.0-rc<N+1>` (e.g., `:1.0.0-rc2` after `:1.0.0-rc1`), so each rc is bound to a specific source state. Don't reuse an existing rc tag.
4. The bench is **re-run against the new rc** to confirm the fix passes validation. Report the rc number that produced the green run.

Anything that does *not* meet all four — architectural changes, behavioral changes, anything where you'd reasonably want a second opinion, or fixes you don't have time to write a regression test for — must be surfaced as a finding in the report, not silent-merged. The release-prep session decides whether to apply.

The rc3 fix in main's history (commit `6bcff55`) is the canonical example of an in-session fix done right: three localized fixes, three new unit tests pinning the regressions, image rebuilt at `:1.0.0-rc3`, bench re-run with passing scaling numbers, all documented in the commit message.

**Bench harness fixes** (`tests/k8s/pnfs-bench/*`) follow the same rule: small fixes for real harness bugs (e.g., the rc3 `sync` D-state hang) are OK with the same four-condition test. Don't modify the harness to mask a real failure.

### Reference docs in the repo

- `tests/k8s/pnfs-bench/README.md` — authoritative bench spec, topology, pass criterion.
- `tests/lima/STATUS.md` — full project state.
- `CHANGELOG.md` — what v1.0.0 ships and known limitations.
- `docs/plans/pnfs-production-readiness.md` — Phase A/B done, Phase C deferred.
- `docs/decisions/0001-3` — ADRs.

If anything in those docs is ambiguous about the bench setup, surface it in your report — better to ask than to guess.

---

# Phase 2 — SPDK validation

Run **only after Phase 1 passes**. Same cluster; this phase adds the
SPDK prerequisites and runs the KUTTL system test suite.

## Phase 2 pre-cluster setup additions

These are required for SPDK volumes (Phase 1 didn't need them):

0. **Three additional `dilipdalton/*` images built and pushed** — the
   chart's pods reference them.
1. **`ublk_drv` kernel module loaded** on each worker.
2. **Hugepages reserved** — at least 2 GB (1024 × 2 MB) per worker.
3. **VolumeSnapshot CRDs installed** cluster-wide.
4. **Flint installed via Helm** with the rc image tags overridden.

### (P2-0) Build and push the chart's images

Phase 1 only needed `flint-pnfs` (used by the bench harness directly).
Phase 2 installs the full Flint Helm chart, which references three
additional images. Build them at the **same rc tag** as the
Phase-1-validated `flint-pnfs` image so the source state under test
is unambiguous (i.e., if Phase 1 validated against `:1.0.0-rc3`,
Phase 2 validates `flint-driver:1.0.0-rc3`, `spdk-tgt:1.0.0-rc3`,
and `spdk-dashboard-frontend:1.0.0-rc3` — all from the same `main`
SHA).

| Image | Dockerfile | Approx build time |
|---|---|---|
| `dilipdalton/flint-driver` | `spdk-csi-driver/docker/Dockerfile.csi` | ~5–10 min cold, <2 min cached |
| `dilipdalton/spdk-tgt` | `spdk-csi-driver/docker/Dockerfile.spdk` | ~30–60 min cold, ~5 min cached (ubuntu:24.04 + SPDK from source — slow) |
| `dilipdalton/spdk-dashboard-frontend` | `spdk-dashboard/Dockerfile.frontend` | ~2–3 min |

```bash
# From repo root, on the same x86-64 build machine you used for flint-pnfs:
git checkout main && git pull origin main

# Replace 1.0.0-rc3 with the actual rc number Phase 1 validated against.
RC_TAG=1.0.0-rc3

docker buildx build \
  --platform linux/amd64 \
  --tag dilipdalton/flint-driver:$RC_TAG \
  --file spdk-csi-driver/docker/Dockerfile.csi \
  --push \
  spdk-csi-driver

docker buildx build \
  --platform linux/amd64 \
  --tag dilipdalton/spdk-tgt:$RC_TAG \
  --file spdk-csi-driver/docker/Dockerfile.spdk \
  --push \
  spdk-csi-driver

docker buildx build \
  --platform linux/amd64 \
  --tag dilipdalton/spdk-dashboard-frontend:$RC_TAG \
  --file spdk-dashboard/Dockerfile.frontend \
  --push \
  spdk-dashboard
```

Verify all three are pullable before proceeding:

```bash
for img in flint-driver spdk-tgt spdk-dashboard-frontend; do
  echo "─── $img ───"
  docker manifest inspect dilipdalton/$img:$RC_TAG > /dev/null \
    && echo "✓ pullable" \
    || echo "✗ MISSING — re-run the build above"
done
```

CSI sidecars (external-provisioner / external-attacher / external-
resizer / external-snapshotter / csi-node-driver-registrar /
livenessprobe) are pulled from `registry.k8s.io/sig-storage/*` at
versions pinned in `values.yaml`. They are not built by Flint.

### (P2-1) Load `ublk_drv` and reserve hugepages

If you used `eksctl preBootstrapCommands` for the cluster, you can
add the bootstrap commands and recreate the nodegroup. Otherwise,
apply this DaemonSet (works purely from `kubectl admin`):

```yaml
# flint-node-init.yaml
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: flint-node-init
  namespace: kube-system
spec:
  selector:
    matchLabels:
      app: flint-node-init
  template:
    metadata:
      labels:
        app: flint-node-init
    spec:
      hostPID: true
      hostNetwork: true
      tolerations:
        - operator: Exists
      containers:
        - name: init
          image: alpine:3.19
          securityContext:
            privileged: true
          command:
            - sh
            - -c
            - |
              set -e
              # Load ublk into the HOST kernel via nsenter into PID 1.
              nsenter -t 1 -m -u -i -n -p -- modprobe ublk_drv
              nsenter -t 1 -m -u -i -n -p -- sh -c \
                "echo ublk_drv > /etc/modules-load.d/ublk.conf"
              # Reserve hugepages (1024 × 2MB = 2 GB).
              nsenter -t 1 -m -u -i -n -p -- sh -c \
                "echo 1024 > /sys/devices/system/node/node0/hugepages/hugepages-2048kB/nr_hugepages"
              nsenter -t 1 -m -u -i -n -p -- sh -c \
                "echo 'vm.nr_hugepages=1024' > /etc/sysctl.d/99-flint-hugepages.conf"
              echo "[flint-node-init] kernel prep complete on $(hostname)"
              sleep infinity
```

```bash
kubectl apply -f flint-node-init.yaml
kubectl rollout status -n kube-system ds/flint-node-init
kubectl logs -n kube-system -l app=flint-node-init --tail=5
```

**Caveat:** Bottlerocket OS deliberately blocks runtime kernel-module
loading. If your nodes run Bottlerocket, you must use the
`preBootstrapCommands` approach. Default EKS managed nodegroup AMI is
Amazon Linux 2023, which works fine with this DaemonSet.

### (P2-2) Verify ublk + hugepages

```bash
# Hugepages — kubelet exposes them as a node-allocatable resource.
kubectl get nodes -o jsonpath='{range .items[*]}{.metadata.name}{": hugepages-2Mi="}{.status.allocatable.hugepages-2Mi}{"\n"}{end}'
# Expect: each worker shows non-zero hugepages-2Mi (e.g., "2Gi").

# ublk module on each worker.
for n in $(kubectl get nodes -o name); do
  echo "─── $n ───"
  kubectl debug -q "$n" -it --image=busybox -- chroot /host lsmod | grep ublk \
    || echo "MISSING — ublk_drv not loaded on $n"
done
# Expect: each worker shows "ublk_drv ... ublk_drv".
```

### (P2-3) Install VolumeSnapshot CRDs

```bash
kubectl apply -k https://github.com/kubernetes-csi/external-snapshotter/client/config/crd?ref=v8.2.0
```

### (P2-4) Install Flint

The chart's `values.yaml` defaults to `tag: latest` for each image —
fine for dev, wrong for release validation. Override the tags at
install time to pin to the rc validated in Phase 1 (replace
`1.0.0-rc3` with the actual rc number if it's different):

```bash
RC_TAG=1.0.0-rc3

helm install flint-csi ./flint-csi-driver-chart \
  --namespace flint-system \
  --create-namespace \
  --set images.flintCsiDriver.tag=$RC_TAG \
  --set images.spdkTarget.tag=$RC_TAG \
  --set dashboard.frontend.tag=$RC_TAG

kubectl rollout status -n flint-system ds/flint-csi-node --timeout=5m
kubectl rollout status -n flint-system deploy/flint-csi-controller --timeout=5m
```

Confirm the running pods are using the rc image (not `latest`):

```bash
kubectl get pods -n flint-system -o jsonpath='{range .items[*]}{.metadata.name}{": "}{range .spec.containers[*]}{.image}{" "}{end}{"\n"}{end}'
# Every flintCsiDriver / spdkTarget / dashboard image should end in :1.0.0-rc3
# (or your actual rc tag). Any :latest is a sign the --set didn't take.
```

Then initialize the local NVMe disks via the dashboard:

```bash
kubectl port-forward -n flint-system svc/flint-dashboard 3000:3000 &
# Open http://localhost:3000 in a browser, navigate to "Disk Setup",
# select /dev/nvme1n1 on each worker, click Initialize.
# Verify all disks show "Ready".
```

## Phase 2 prompt — paste below into a fresh Claude Code session

I'm running Phase 2 of pre-publish validation for Flint CSI **v1.0.0** — the **SPDK KUTTL system tests**. Phase 1 (pNFS scalability) has already passed and was reported green. Same cluster, now with SPDK prerequisites in place. **Do not run `git tag` or `gh release create`. Do not push images to the `:1.0.0` tag. Do not push to `origin/main`. Your job is the test runs and a written report.**

### Infrastructure I have

Same 5-node `i3en.xlarge` EKS cluster from Phase 1, plus:
- `ublk_drv` loaded on each worker (verified).
- 2 GB hugepages reserved on each worker (verified — `hugepages-2Mi` non-zero in node allocatable).
- VolumeSnapshot CRDs installed cluster-wide.
- Flint installed via Helm in namespace `flint-system`; controller and node DaemonSet pods Ready.
- `/dev/nvme1n1` initialized on each worker via the dashboard (status: Ready).

`KUBECONFIG` is set in my shell.

### What you need to do

Phase 2 runs **two KUTTL test suites** to cover both Flint backends. Both must pass.

1. **Confirm Flint is healthy** before running tests:
   ```bash
   kubectl get pods -n flint-system
   kubectl get csidrivers flint.csi.storage.io
   kubectl get storageclass
   ```
   All Flint pods should be Running. The CSIDriver should exist. Both the SPDK-backed default StorageClass `flint` and an NFS-only StorageClass should be present (per the chart's `values.yaml`; if the NFS-only StorageClass isn't registered, surface that as a finding before running suite 2).

2. **Run the SPDK suite** — covers full Flint+SPDK code paths including **RWX with SPDK**:
   ```bash
   cd tests/system
   KUBECONFIG=$KUBECONFIG kubectl kuttl test --config kuttl-testsuite.yaml
   ```
   Tests: `multi-replica`, `snapshot-restore`, `volume-expansion`, `pvc-clone`, `ephemeral-inline`, `rwo-pvc-migration`, **`rwx-single-replica`** (RWX-via-NFS-on-SPDK), `rox-multi-pod`. Capture the full output.

3. **Run the no-SPDK suite** — covers **RWX without SPDK** plus other no-SPDK paths:
   ```bash
   KUBECONFIG=$KUBECONFIG kubectl kuttl test --config kuttl-testsuite-nfs-only.yaml
   ```
   Tests: `ephemeral-inline`, `rwo-pvc-migration`, **`rwx-single-replica`** (RWX-via-NFS-emptyDir, no SPDK), `volume-expansion`. The suite description is explicit: *"Tests compatible with nfs-only mode (no SPDK). Excludes: multi-replica (requires SPDK replication), snapshot-restore (requires SPDK snapshots), rox-multi-pod (requires snapshot-based ROX volumes)."* Capture the full output.

   This suite uses the no-SPDK StorageClass (`parameters.nfsEmptyDir: "true"`). If the test PVCs reference `storageClassName: flint` but the suite expects an NFS-only StorageClass, the StorageClass naming setup may need adjustment — if either suite fails with `provisioning failed: storage class not found` or `wrong backend`, surface that as a finding rather than mutating the tests.

4. **Report back with** (separately for each suite):
   - The full KUTTL output (PASS/FAIL per test, total wall-clock time).
   - Per-test failure details if any test failed: pod logs, kubectl describe, leftover resources in the test namespace.
   - Any `OOMKilled`, `CrashLoopBackOff`, or `Error` pod statuses during the run.
   - Output of `kubectl logs -n flint-system -l app=flint-csi-controller --tail=200` after each suite completes.
   - Confirm that **`rwx-single-replica` passed in both suites** — that's the RWX-with-SPDK and RWX-without-SPDK pair, and both must work for v1.0.0 to ship.

5. **If a test fails**: do **not** delete the test namespace; leave it for inspection. Report the failure mode. Don't proceed to the next suite if the first has unresolved failures — surface and let the release-prep session decide.

### Phase 2 success criteria

- **All tests in `kuttl-testsuite.yaml` pass** (SPDK paths, including RWX-with-SPDK).
- **All tests in `kuttl-testsuite-nfs-only.yaml` pass** (no-SPDK paths, including RWX-without-SPDK).
- Both `rwx-single-replica` runs (one per suite) explicitly pass — RWX is the use case where Flint diverges most from a stock single-server NFS, so both backends getting it right is load-bearing.
- No Flint controller or node-pod restarts during either run.
- No leftover orphan resources after each suite completes (KUTTL cleans up between tests).

### What to leave alone, and what's OK to fix in-session

Same rules as Phase 1: hard prohibitions on tag, release, version bump, and pushing to the immutable `:1.0.0` image tag. **Small, well-scoped fixes that turn a red KUTTL test green ARE OK to commit to `main`** if they meet the same four-condition test as Phase 1: localized, regression-tested, rc bumped (e.g., `:1.0.0-rc4`), and the test re-run against the new rc reports green. See the Phase 1 "What to leave alone" section for the full text. Anything bigger surfaces as a finding for the release-prep session.

### Reference docs in the repo

- `tests/system/README.md` — KUTTL suite overview.
- `tests/system/tests-standard/<name>/` — individual test specs.
- `CHANGELOG.md` — what v1.0.0 ships.

---

## Notes on this prompt's design

(Not part of the prompts above. For anyone editing this file later.)

- The two-phase split is deliberate: Phase 1 validates the unproven
  architectural claim (pNFS cross-host scaling, which loopback /
  Lima can't measure); Phase 2 validates known-good code paths
  (SPDK CSI features that have shipped through earlier dev cycles).
  Failing Phase 1 means a deeper rework; failing Phase 2 typically
  means a regression introduced by recent changes — different
  failure semantics, different debugging strategies.
- Image build is intentionally outside both Claude sessions. Build is
  a development concern; test is a validation concern. Mixing them
  required Docker on the validation machine and re-built the
  artifact-under-test on every run.
- The `:1.0.0-rc<N>` image tag (immutable) avoids polluting the
  immutable `:1.0.0` tag that the final release will own. If
  validation passes, the release-prep session re-tags and re-pushes
  at `:1.0.0`.
- The NVMe IOPS check catches "wrong instance type" early — the
  failure mode it prevents is "the bench worked, but the numbers
  came from EBS, not NVMe."
- "Don't fix-and-merge silently" was the original guard, but the
  rc3 cycle (commit `6bcff55`) showed it was too strict for blocker
  bugs found mid-bench. The guard now allows small, well-scoped,
  regression-tested fixes with an rc bump — and forbids anything
  bigger. The four-condition test in the "What to leave alone"
  sections is the load-bearing rule. Architectural changes,
  behavioral changes, and anything that needs a second opinion
  still come back to the release-prep session as a finding.
- The rc tag must bump on every fix that ships back through the
  bench (`:1.0.0-rc1` → `:1.0.0-rc2` → ...). Reusing an rc tag
  defeats the immutability guarantee that lets you say "the bench
  passed against this exact source state."

## Outcome paths

After both phases report back to the release-prep session:

- **Both green:** the release-prep session tags `v1.0.0`, pushes the
  tag, creates the GitHub Release, and re-tags/re-pushes the images
  at `:1.0.0`.
- **Phase 1 red:** fix on `main`, build `:1.0.0-rc2`, repeat Phase 1.
  Don't proceed to Phase 2 until Phase 1 is green.
- **Phase 1 green, Phase 2 red:** depends on the failure. A regression
  in a code path that hasn't materially changed since the last KUTTL
  run is a real release blocker. A flake or environmental issue may
  warrant a re-run before fixing.
