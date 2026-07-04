# Building x86-64 images on a cluster worker node

Building `linux/amd64` images on an Apple Silicon Mac runs every `RUN` step
under QEMU emulation — the SPDK image build takes hours. Instead, we use an
x86-64 worker node of the test cluster as a **native remote build host**,
reached purely through the Kubernetes API (no SSH access or `.pem` key
needed — the kubeconfig is sufficient).

```
docker CLI (Mac)
  │  DOCKER_HOST=tcp://127.0.0.1:23750
  ▼
kubectl port-forward  ──►  socat proxy pod  ──►  /var/run/docker.sock
        (TLS via kube API)   (on build node)        dockerd on the node
```

The build context is streamed from the Mac through the port-forward, the
build runs natively on the node, and `docker push` uses the **Mac's** Docker
Hub credentials (the client sends registry auth per request), so the node
never needs `docker login`.

First used 2026-06-10 on cluster `kubeconfig-test.yaml`, node `test-aws-1`
(Amazon Linux 2023, 2 vCPU, 16 GiB, 8 GiB root EBS + 436 GiB instance-store
NVMe) to build the `1.1.0` phase-0 images.

## Prerequisites

- Kubeconfig with cluster-admin (e.g. `export KUBECONFIG=~/Downloads/kubeconfig-test.yaml`)
- An x86-64 worker node (check: `kubectl get nodes -o wide`)
- Docker CLI on the Mac (the local daemon/QEMU VM is not used and may be stopped)
- `docker login` done locally if you intend to push

## Running commands on a node without SSH

All node setup is done through a short-lived privileged pod that `nsenter`s
into the host's namespaces (PID 1). Helper — run a script on a node:

```sh
node_run() {  # usage: node_run <node-name> <<'EOF' ... EOF
  local node=$1 b64=$(base64 | tr -d '\n')
  kubectl run node-exec --rm -i --restart=Never --image=busybox:1.36 \
    --overrides="{\"spec\":{\"nodeName\":\"$node\",\"hostPID\":true,
      \"tolerations\":[{\"operator\":\"Exists\"}],
      \"containers\":[{\"name\":\"node-exec\",\"image\":\"busybox:1.36\",
        \"command\":[\"nsenter\",\"-t\",\"1\",\"-m\",\"-u\",\"-i\",\"-n\",\"-p\",\"--\",
                     \"sh\",\"-c\",\"echo $b64 | base64 -d | sh\"],
        \"securityContext\":{\"privileged\":true},\"stdin\":true}]}}" \
    --timeout=300s
}
```

(The script is base64-encoded to avoid JSON/shell quoting issues.)

## Step 0 — bulk-init E2E drill (standard for fresh spot builders)

A freshly joined builder node is the ONE moment the cluster has a disk that
is uninitialized, unmounted, and non-system — i.e. actually eligible for the
dashboard's bulk-init flow — and that we are about to reformat anyway. Run
the end-to-end drill against it BEFORE the docker-data setup below:

```sh
cd ~/github/flint/spdk-dashboard
npm install --no-save playwright-core
kubectl -n flint-system port-forward deploy/spdk-dashboard 13000:3000 &
DASHBOARD_ADMIN_PW=$(kubectl -n flint-system get secret spdk-dashboard-auth     -o jsonpath='{.data.admin-password}' | base64 -d) TARGET_NODE=<new-node-name> TARGET_PCI=0000:00:1f.0   node scripts/bulk-init-drill.mjs
```

The drill exercises selection → BulkConfirmModal manifest → confirm →
runInitBatch per-disk status → LVS Ready, against a real agent. It leaves an
SPDK LVS on the scratch disk; hand the disk back before the docker setup:

```sh
node_run <new-node-name> <<'EOF'
wipefs -a /dev/nvme1n1
EOF
```

(The docker-data guard below refuses any disk with a signature, so a skipped
wipefs fails safe.) First validated live 2026-07-04 groundwork; the flow's
rails (eligibility, manifest, typed-phrase gate) are unit-tested in
`spdk-dashboard/src/components/setup/BulkInitPanels.test.tsx`.

## One-time node setup

### 1. Docker data-root on the instance-store NVMe

The root EBS volume is 8 GiB with <3 GiB free — not enough for the SPDK
build (~6 GiB of layers + cache). The node's instance-store NVMe
(`/dev/nvme1n1`, 436 GiB) gets a 100 GiB partition for `/var/lib/docker`;
the rest stays unallocated.

> **Trade-off — SPDK testing on the same node.** SPDK's `setup.sh` claims
> whole PCI devices and skips disks with mounted filesystems, so while
> Docker occupies a partition of `nvme1n1`, flint disk-setup will *skip
> that disk* on this node. Fine for a build/test cluster; if you need the
> disk back for storage testing: `systemctl stop docker`, `umount
> /var/lib/docker`, remove the fstab entry, `wipefs -a /dev/nvme1n1`.

> **Ephemerality.** Instance-store data vanishes on instance stop/replace.
> The fstab entry uses `nofail`, so the node still boots — but Docker then
> silently falls back to the small root disk. Re-run this setup after a
> node replacement (the guard below makes it refuse to overwrite an
> already-set-up disk, so it is safe to re-run blindly).

```sh
node_run test-aws-1 <<'EOF'
set -e
# refuse to touch the disk if it already has a signature/partitions
if blkid /dev/nvme1n1 >/dev/null 2>&1 || [ -e /dev/nvme1n1p1 ]; then
  echo "REFUSING: /dev/nvme1n1 already has a signature or partitions"; exit 1
fi
parted -s /dev/nvme1n1 mklabel gpt mkpart docker ext4 1MiB 100GiB
sleep 2
mkfs.ext4 -q -L dockerdata /dev/nvme1n1p1
mkdir -p /var/lib/docker
grep -q dockerdata /etc/fstab || \
  echo 'LABEL=dockerdata /var/lib/docker ext4 defaults,nofail 0 2' >> /etc/fstab
mount /var/lib/docker
EOF
```

### 2. Install Docker (Amazon Linux 2023)

The AL2023 `docker` RPM coexists with the kubelet's containerd: dockerd runs
its own containerd instance, and images live in the separate data-root, so
the Kubernetes runtime is untouched (verified: node stays `Ready`, no pod
restarts).

```sh
node_run test-aws-1 <<'EOF'
set -e
dnf install -y -q docker
systemctl enable --now docker
docker info --format 'docker {{.ServerVersion}} root={{.DockerRootDir}}'
df -h /var/lib/docker
EOF
```

Expected: `docker 25.x root=/var/lib/docker` on the 98 G partition.

### 3. Socket proxy pod

Exposes the node's Docker socket as TCP **inside the cluster only** (no
hostNetwork, no Service) — it is reachable solely via `kubectl port-forward`,
i.e. authenticated through the kube API.

```sh
kubectl apply -f - <<'EOF'
apiVersion: v1
kind: Pod
metadata:
  name: docker-build-proxy
  namespace: default
  labels: {app: docker-build-proxy}
spec:
  nodeName: test-aws-1
  containers:
  - name: socat
    image: alpine/socat:latest
    args: ["TCP-LISTEN:2375,fork,reuseaddr", "UNIX-CONNECT:/var/run/docker.sock"]
    volumeMounts:
    - {name: docker-sock, mountPath: /var/run/docker.sock}
  volumes:
  - name: docker-sock
    hostPath: {path: /var/run/docker.sock, type: Socket}
EOF
```

> **Security note:** access to a Docker socket is root on the node, and this
> pod hands the socket to anyone who can port-forward to it. That is the
> same trust level as cluster-admin on this single-tenant test cluster. On a
> shared cluster, delete the pod when not building
> (`kubectl delete pod docker-build-proxy`).

## Building (each session)

```sh
export KUBECONFIG=~/Downloads/kubeconfig-test.yaml
kubectl port-forward pod/docker-build-proxy 23750:2375 &   # keep running
export DOCKER_HOST=tcp://127.0.0.1:23750

docker version --format 'server={{.Server.Version}} arch={{.Server.Arch}}'
# → server=25.0.16 arch=amd64

cd ~/github/flint/spdk-csi-driver
docker buildx build -f docker/Dockerfile.csi  -t dilipdalton/flint-driver:1.1.0 .
docker buildx build -f docker/Dockerfile.spdk -t dilipdalton/spdk-tgt:1.1.0 .

# push straight from the node, using the Mac's Docker Hub login:
docker push dilipdalton/flint-driver:1.1.0
docker push dilipdalton/spdk-tgt:1.1.0

unset DOCKER_HOST   # back to the local daemon
```

## Releasing

Don't push release images by hand from the list above — it is not the
source of truth, and 1.2.0 shipped with `spdk-dashboard-frontend:1.2.0`
unpublished (every install sat in ImagePullBackOff) precisely because the
image set was maintained by hand. Use the release gate instead, with
`DOCKER_HOST` still pointing at the build node for the heavy images:

```sh
scripts/release.sh check    # what does the chart reference, what's missing?
scripts/release.sh images   # build + push only the missing images
scripts/release.sh chart    # verify ALL published, then helm-push the chart
```

It derives the image list from `flint-csi-driver-chart/values.yaml` and
refuses to push a chart whose image references aren't all on Docker Hub.

No `--platform` flag is needed — the daemon is natively amd64. Builds are
fully cached on the node across sessions (BuildKit cache lives in the
data-root).

Because the images land in the **node's** Docker daemon (not containerd's
`k8s.io` namespace), the cluster cannot run them directly — push to Docker
Hub and let pods pull, per the normal release flow.

## Build context: keep `.dockerignore`

`spdk-csi-driver/.dockerignore` excludes `target/` (~9 GiB of Rust build
artifacts). Without it the context upload through the port-forward is 9.4 GiB;
with it, ~2.4 MiB. If a new `COPY` in a Dockerfile ever needs a path listed
there, remove that entry.

## CPU-architecture portability of SPDK images

DPDK builds with `-march=native`, so an `spdk-tgt` image **embeds the
build host's ISA extensions**. Observed 2026-07-01: `spdk-tgt:1.1.1`,
built on an Ice Lake node (`i4i`, the original `test-aws-1`), crashes at
startup on a Skylake `c5d.4xlarge` —

```
ERROR: This system does not support "VPCLMULQDQ".
EAL: unsupported cpu type.
```

Build on the **oldest microarchitecture in the fleet** (an image built
on Skylake runs on Ice Lake, not vice versa), or pin DPDK's machine type
in the Dockerfile. `tier2-spike-v3`, built natively on the c5d, runs on
both generations.

## Troubleshooting

- **`Cannot connect to the Docker daemon`** — the port-forward died; re-run
  it. If the proxy pod is gone (node replaced), re-apply step 3.
- **`no space left on device` during build** — `df -h /var/lib/docker` on the
  node. If it shows the 8 G root disk, the NVMe mount is gone (instance was
  stopped — instance store wiped): re-run step 1 + remount. Otherwise prune:
  `DOCKER_HOST=tcp://127.0.0.1:23750 docker builder prune -af`.
- **Slow compile** — `test-aws-1` has 2 vCPUs; the SPDK build takes ~30-60 min
  native (vs. many hours under QEMU). A bigger worker speeds this up
  linearly (`make -j$(nproc)`).
- **Node disruption worry** — installing/restarting docker does not touch the
  kubelet's containerd; verify with `kubectl get nodes` / `kubectl get pods -A`.
