# Flint v1.0.0 — AWS multi-node validation prompt

This file is the self-contained prompt for running pre-publish
validation of a Flint release candidate on a multi-node AWS Kubernetes
cluster. The validation machine doesn't need any context from the
release-prep session — everything load-bearing is in the prompt below
or pointed at via repo paths.

**Workflow.** On the validation machine: clone the repo, read this
file, do the **Pre-cluster setup** below, then copy the section
between the `---` markers into a fresh Claude Code session as your
first message, run. Report back to the release-prep session with the
TSV + markdown table + log excerpts.

## Pre-cluster setup (do this before pasting the prompt)

Three things must be ready before the validation Claude session runs:

* The pNFS bench container image must exist on a registry the cluster
  can pull from — see **(0) Build and publish the rc image** below.
* The cluster's worker nodes must have `ublk_drv` loaded and hugepages
  reserved — see **(1) and (2) Node bootstrap** below.
* The local NVMe must be visible inside pods — automatic on `i3en.xlarge`
  (no configuration); verified at the end of this section.

### (0) Build and publish the rc image

The bench harness consumes a single env var, `PNFS_IMAGE`, pointing at
a pNFS image that bundles the `flint-pnfs-mds` and `flint-pnfs-ds`
binaries (the dedicated Dockerfile is at
`spdk-csi-driver/docker/Dockerfile.pnfs`). For v1.0.0 validation, build
and push that image to Docker Hub at the **release-candidate tag**
`1.0.0-rc1`, **not `latest`**. The rc tag is immutable; `latest` would
let a concurrent push silently change the artifact between push and
bench, making "what was actually tested?" a forensic question.

You can run this from any x86-64 machine with Docker (your dev
laptop is fine). It does not have to be the validation machine; the
image just has to be pullable from Docker Hub by the time the bench
runs.

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

If `docker login` for `dilipdalton` isn't set up on the build
machine: `docker login -u dilipdalton` first. Use a Docker Hub
access token, not your account password.

After the push, verify the image is pullable:

```bash
docker pull dilipdalton/flint-pnfs:1.0.0-rc1
docker inspect dilipdalton/flint-pnfs:1.0.0-rc1 --format '{{.Architecture}}'
# Expect: amd64
```

If validation fails (bench doesn't pass), fix on `main`, build
`:1.0.0-rc2` from the new SHA, repeat. Don't reuse the `:1.0.0-rc1`
tag — each rc tag is immutable so the bench history stays
unambiguous.

### (1) and (2) Node bootstrap

Flint expects on each worker:

1. **`ublk_drv` kernel module loaded** — required by SPDK's CSI plugin
   to expose userspace block devices to pods.
2. **Hugepages reserved** — at least 2 GB (1024 × 2 MB) per worker, per
   the README's hardware requirements.
3. **Local NVMe present** — automatic on `i3en.xlarge`; the instance
   store NVMe attaches at boot as `/dev/nvme1n1`. No configuration
   needed.

If you have admin kubeconfig but **no SSH access** to nodes (typical
for managed EKS), you have two ways to satisfy (1) and (2). Pick one
based on what you can change:

### Option A — Set it at nodegroup creation (cleanest, do this if you can)

Use `eksctl`'s `preBootstrapCommands` so userdata runs before the
kubelet starts. With `cluster.yaml`:

```yaml
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
    preBootstrapCommands:
      - "modprobe ublk_drv"
      - "echo 'ublk_drv' > /etc/modules-load.d/ublk.conf"
      - "echo 'vm.nr_hugepages=1024' > /etc/sysctl.d/99-flint-hugepages.conf"
      - "sysctl --system"
```

Equivalent in Terraform: put the same shell content into the launch
template's `user_data` (base64-encoded). Equivalent in the AWS
Console: specify launch-template userdata when creating the managed
nodegroup.

This survives node replacement (autoscaling, failure-replacement)
automatically. **Use this for any longer-lived cluster.**

### Option B — Privileged init DaemonSet (works purely from kubectl)

If you can't change nodegroup config but can `kubectl apply`, use
this DaemonSet **before installing Flint**. Saves to a file and
applies once.

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
              # The container's mount/PID namespaces are irrelevant; the
              # kernel module belongs to the host.
              nsenter -t 1 -m -u -i -n -p -- modprobe ublk_drv
              nsenter -t 1 -m -u -i -n -p -- sh -c \
                "echo ublk_drv > /etc/modules-load.d/ublk.conf"
              # Reserve hugepages (1024 × 2MB = 2 GB).
              # Sysfs path varies by NUMA topology; this targets node0
              # which is correct on single-socket instances like
              # i3en.xlarge.
              nsenter -t 1 -m -u -i -n -p -- sh -c \
                "echo 1024 > /sys/devices/system/node/node0/hugepages/hugepages-2048kB/nr_hugepages"
              nsenter -t 1 -m -u -i -n -p -- sh -c \
                "echo 'vm.nr_hugepages=1024' > /etc/sysctl.d/99-flint-hugepages.conf"
              echo "[flint-node-init] kernel prep complete on $(hostname)"
              # Sleep so the DaemonSet pod doesn't restart-loop.
              sleep infinity
```

```bash
kubectl apply -f flint-node-init.yaml
kubectl rollout status -n kube-system ds/flint-node-init
kubectl logs -n kube-system -l app=flint-node-init --tail=5
```

The kubelet rediscovers hugepages within ~10 seconds.

**Caveat:** Bottlerocket OS deliberately blocks runtime kernel-module
loading. If your nodes run Bottlerocket, you must use Option A.
Default EKS managed nodegroup AMI is Amazon Linux 2023, which works
fine with Option B.

### Verifying from kubectl alone (no SSH needed)

After Option A or B, run all three checks. **All three must pass
before pasting the bench prompt.**

```bash
# 1. Hugepages — kubelet exposes them as a node-allocatable resource.
kubectl get nodes -o jsonpath='{range .items[*]}{.metadata.name}{": hugepages-2Mi="}{.status.allocatable.hugepages-2Mi}{"\n"}{end}'
# Expect: each worker shows non-zero hugepages-2Mi (e.g., "2Gi").

# 2. ublk module on each worker.
for n in $(kubectl get nodes -o name); do
  echo "─── $n ───"
  kubectl debug -q "$n" -it --image=busybox -- chroot /host lsmod | grep ublk \
    || echo "MISSING — ublk_drv not loaded on $n"
done
# Expect: each worker shows "ublk_drv ... ublk_drv".

# 3. Local NVMe is visible inside a privileged pod on a worker.
kubectl run nvme-check --rm -it --image=busybox --restart=Never \
  --overrides='{"spec":{"nodeName":"<one-worker-nodename>","hostPID":true,"containers":[{"name":"nvme-check","image":"busybox","securityContext":{"privileged":true},"command":["sh","-c","ls -la /dev/nvme*"]}]}}'
# Expect: /dev/nvme0n1 (root EBS) and /dev/nvme1n1 (instance store NVMe).
# /dev/nvme1n1 is what SPDK formats. Do not mount or format it manually.
```

### Cluster install order

These can run in parallel — image build and cluster bring-up are
independent. Order is "all of this must be done before pasting the
prompt":

0. Build and push `dilipdalton/flint-pnfs:1.0.0-rc1` (step (0) above).
   Doable from any x86-64 machine with Docker; doesn't need to be
   the validation machine.
1. Create cluster + nodegroup (Option A bakes node-init into the
   nodegroup; Option B adds it after).
2. Run the three verification checks above.
3. Install the VolumeSnapshot CRDs (cluster-wide prereq, separate
   from the Flint chart):
   ```bash
   kubectl apply -k https://github.com/kubernetes-csi/external-snapshotter/client/config/crd?ref=v8.2.0
   ```
4. Confirm the rc image is pullable from inside the cluster:
   ```bash
   kubectl run image-check --rm -it --image=dilipdalton/flint-pnfs:1.0.0-rc1 \
     --command --restart=Never -- /bin/true
   # If this returns 0 the image pulled successfully on a worker node.
   ```
4. Now paste the prompt below into a fresh Claude Code session.

---

## Prompt to paste into Claude Code on the validation machine

I'm running pre-publish validation for Flint CSI **v1.0.0**, a Kubernetes CSI driver providing high-performance local block storage (SPDK) plus parallel-server NFS (pNFS). The release candidate is committed to `main` of `https://github.com/ddalton/flint`. The release tag `v1.0.0` has **not** been created yet — the publish is gated on this validation passing. **Do not run `git tag` or `gh release create`. Do not push images to the `:1.0.0` tag (only `:1.0.0-rc<N>` is OK). Do not push to `origin/main`. Your job is the bench run and a written report.**

### Infrastructure I have

A Kubernetes cluster on AWS with **5 nodes** (1 control + 4 workers, all `i3en.xlarge`):
- 4 vCPU, 32 GB RAM each
- 1× 2.5 TB local NVMe per node (the device that SPDK / Flint will format)
- 25 Gbps inter-node network
- AMI with kernel 5.16+ and `ublk_drv` available

`KUBECONFIG` is set in my shell. `kubectl get nodes` lists 5 nodes, ready.

### What you need to do

1. **Confirm the cluster meets the bench prerequisites** documented in `tests/k8s/pnfs-bench/README.md`. Run `kubectl get nodes`, verify there are ≥4 worker nodes, and confirm `ublk_drv` is loaded on each worker (`kubectl debug node/<name> -it --image=busybox -- chroot /host lsmod | grep ublk`, or equivalent). If `ublk_drv` is missing, log it and stop — don't try to install kernel modules from this session; the user has to fix that on their AMI bootstrap.

2. **Confirm the rc container image exists on Docker Hub.** The user has built and pushed `dilipdalton/flint-pnfs:1.0.0-rc1` from `spdk-csi-driver/docker/Dockerfile.pnfs` per the pre-cluster-setup step in `tests/k8s/pnfs-bench/AWS_VALIDATION_PROMPT.md`. Verify the image exists and is reachable from your shell with:
   ```bash
   docker manifest inspect dilipdalton/flint-pnfs:1.0.0-rc1
   ```
   If this fails, stop and ask the user to complete the pre-cluster image-build step. **Do not build the image from this session.** The build is intentionally a separate concern from the bench; rebuilding here would diverge the artifact-under-test from what the user signed off on.

3. **Run the cross-host pNFS bench**:
   ```bash
   KUBECONFIG=$KUBECONFIG \
     PNFS_IMAGE=dilipdalton/flint-pnfs:1.0.0-rc1 \
     MDS_NODE=<worker-1-name> \
     DS_NODES="<worker-2-name> <worker-3-name>" \
     CLIENT_NODE=<worker-4-name> \
     make test-pnfs-cross-host
   ```
   The harness creates a Namespace, MDS+DS Deployments with `nodeName` pins, and a client Deployment. It runs `bs={4K, 1M} × {read, write}` fio sweeps and dumps a TSV + a markdown table. Capture both.

4. **Report back with**:
   - The full TSV that `make test-pnfs-cross-host` produces.
   - The markdown table summary.
   - `kubectl logs` from each MDS pod and each DS pod (last 100 lines).
   - Whether the run hit the **pass criterion** documented in `tests/k8s/pnfs-bench/README.md` (the README is authoritative — read it; the criterion was deliberately written before the bench had real numbers, so it's the published bar to hit).
   - Any `OOMKilled`, `CrashLoopBackOff`, or `Error` pod statuses during the run.
   - Total wall-clock time of the bench run.

5. **If the bench fails to complete or pass criterion is not hit**: do **not** delete the test namespace; leave the pods in their failed state so we can examine them later. Capture full logs, kubectl describe outputs, and report the failure mode.

### What success looks like

- `make test-pnfs-cross-host` exits 0.
- The TSV contains numbers across all four `bs × {read, write}` rows (no empty cells, no errors).
- Per-DS allocation is approximately balanced (each DS holds roughly half the written data; tolerance per the README).
- Pass criterion in `tests/k8s/pnfs-bench/README.md` is met.

### What to leave alone

- Don't run `git tag`, `git push --tags`, or `gh release create`. Tagging is the release-prep session's job once you report back green.
- Don't bump versions in `Cargo.toml`, `Chart.yaml`, or `CHANGELOG.md`.
- Don't push to a `:1.0.0` image tag (the immutable release tag); only `:1.0.0-rc<N>` is OK at this stage. Once validation passes, the main session will re-tag/re-push at `:1.0.0`.
- Don't modify `tests/k8s/pnfs-bench/*` to make the harness pass. If the harness has a real bug, file it as a separate finding in your report.
- Don't push to `origin/main`. If you find a bug that needs fixing in source, surface it as a finding for the user to decide on, don't fix-and-merge silently.

### Reference docs in the repo

- `tests/k8s/pnfs-bench/README.md` — authoritative bench spec, topology, pass criterion.
- `tests/lima/STATUS.md` — full project state. The "Picking up next session" section at the top has the v1.0 publish gate.
- `CHANGELOG.md` — what v1.0.0 ships and known limitations.
- `docs/plans/pnfs-production-readiness.md` — Phase A/B done, Phase C deferred.
- `docs/decisions/0001-3` — ADRs (one driver / perf baseline / write-perf deep dive).

If anything in those docs is ambiguous about the bench setup, surface it in your report — better to ask than to guess.

---

## Notes on this prompt's design

(Not part of the prompt above. For anyone editing this file later.)

- The prompt assumes the validation machine has a fresh Claude Code session with no prior context. Everything load-bearing is in the prompt itself or pointed at via repo paths.
- The "don't tag, don't release" guard is explicit because validation sessions can drift into "completing" the release if they're not told not to.
- The `:1.0.0-rc<N>` image tag avoids polluting the immutable `:1.0.0` tag that the final release should own. If validation passes, the release-prep session re-tags and re-pushes at `:1.0.0`.
- The pass criterion deliberately points at `tests/k8s/pnfs-bench/README.md` rather than restating it here, because that file is the single source of truth and could change.
- The "don't fix-and-merge silently" line is important: if the validation session finds a real bug (e.g., a controller-startup race that only fires under multi-node networking), the failure should come back to the release-prep session as the next problem to fix on `main`, not be patched-and-pushed in isolation.

## Outcome paths

When the validation machine session reports back:
- **Green:** the release-prep session tags `v1.0.0`, pushes, creates the GitHub Release, and proceeds to the final image build/push at `:1.0.0`.
- **Red:** the failure becomes the next session's load-bearing input — fix on `main`, build a `:1.0.0-rc2` image, repeat the validation.
