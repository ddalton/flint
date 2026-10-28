#!/usr/bin/env bash
# kind-sanity.sh -- run csi-sanity (kubernetes-csi/csi-test) against the
# driver built from THIS tree, on a throwaway kind cluster whose SPDK
# targets run on in-memory disks. Each pass is one replica count in one
# mode:
#   raw   csi-sanity talks to the controller directly: argument checks,
#         error codes, and idempotency of the calls that need no PV. The
#         suite's own names (48-128 bytes, uppercase) exceed the driver's
#         46-byte storage-id limit, so every spec that creates a volume
#         fails with INVALID_ARGUMENT here by design.
#   shim  csi-sanity talks to coshim (this directory), which writes the PV
#         the external-provisioner would and names volumes pvc-<uuid>: the
#         calls that read the PV (publish, snapshots, expand, stage).
# After each pass a census lists what it left: lvols per node, coshim PVs,
# and the host's NVMe controllers (kind nodes share one kernel, so a
# stranded initiator shows up there).
#
# Not a CI gate: a hosted runner has no nvme_tcp. Run it on a Linux host
# with docker, kind, kubectl, helm, go, cargo-zigbuild (BUILD=1),
# passwordless sudo (NICE_SPDK=1) and the nvme_tcp module loaded. Memory:
# ~2 GiB per node for SPDK (3 nodes) plus the cluster. With the snap
# docker, keep the checkout under $HOME: it cannot read a build context
# elsewhere (files reach the kind nodes through stdin for the same reason).
#
# Env: CLUSTER (flint-sanity)  REG_PORT (5002)  TAG (git short sha)
#      BUILD (1; 0 = both images already exist locally)  KEEP (0)
#      PASSES ("raw-r1 raw-r3 shim-r1 shim-r3")  CSI_TEST_VERSION (v5.6.0)
#      WORK ($ROOT/spdk-csi-driver/target/sanity: tools, results)
#      NICE_SPDK (0; 1 = renice the polling SPDK reactors to 19, for a
#      host shared with other work)  KIND_NODE_IMAGE (kindest/node:v1.34.0)
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
CLUSTER="${CLUSTER:-flint-sanity}"
REG_PORT="${REG_PORT:-5002}"
REG_NAME="${CLUSTER}-registry"
TAG="${TAG:-$(git -C "$ROOT" rev-parse --short HEAD)}"
BUILD="${BUILD:-1}"
KEEP="${KEEP:-0}"
PASSES="${PASSES:-raw-r1 raw-r3 shim-r1 shim-r3}"
CSI_TEST_VERSION="${CSI_TEST_VERSION:-v5.6.0}"
WORK="${WORK:-$ROOT/spdk-csi-driver/target/sanity}"
NICE_SPDK="${NICE_SPDK:-0}"
KIND_NODE_IMAGE="${KIND_NODE_IMAGE:-kindest/node:v1.34.0}"
SPDK_TAG=$(awk '/^  spdkTarget:/{f=1} f&&/tag:/{gsub(/"/,"",$2); print $2; exit}' "$ROOT/flint-csi-driver-chart/values.yaml")
SPDK_BASE="${SPDK_BASE:-dilipdalton/spdk-tgt:${SPDK_TAG}}"
DRIVER_IMAGE="localhost:${REG_PORT}/dilipdalton/flint-driver:${TAG}"
SPDK_IMAGE="localhost:${REG_PORT}/dilipdalton/spdk-tgt-kind:${TAG}"
NODE="${CLUSTER}-worker"   # csi-sanity runs inside this kind node
SOCK=/var/lib/kubelet/plugins/disk.csi.chert.us/csi.sock
mkdir -p "$WORK/bin" "$WORK/results"
export KUBECONFIG="$WORK/kubeconfig"

step() { printf '\n▶ %s\n' "$*" >&2; }
fail() { printf '\n✗ %s\n' "$*" >&2; exit 1; }

for t in docker kind kubectl helm go $( [ "$BUILD" = 1 ] && echo cargo-zigbuild ); do
  command -v "$t" >/dev/null || fail "$t not found in PATH"
done
grep -q '^nvme_tcp ' /proc/modules || fail "nvme_tcp is not loaded (NodeStage connects kernel NVMe/TCP initiators)"

cleanup() {
  rc=$?
  if [ "$KEEP" = 1 ]; then
    echo "KEEP=1: cluster $CLUSTER and registry $REG_NAME left up; KUBECONFIG=$KUBECONFIG" >&2
  else
    kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
    docker rm -f "$REG_NAME" >/dev/null 2>&1 || true
    # Kernel initiators outlive the node containers (one host kernel):
    # remove the flint ones, or they retry for ctrl_loss_tmo.
    for c in /sys/class/nvme/nvme*; do
      [ -r "$c/subsysnqn" ] || continue
      case "$(cat "$c/subsysnqn")" in
        nqn.2024-11.com.flint*) echo 1 | sudo -n tee "$c/delete_controller" >/dev/null || true ;;
      esac
    done
  fi
  exit "$rc"
}
trap cleanup EXIT

if [ "$BUILD" = 1 ]; then
  step "build $DRIVER_IMAGE (cargo zigbuild + Dockerfile.csi.prebuilt, as published)"
  (cd "$ROOT/spdk-csi-driver" &&
    cargo zigbuild --locked --release --target x86_64-unknown-linux-musl --bin csi-driver --bin flint-nfs-server)
  ctx="$ROOT/spdk-csi-driver/target/kind-sanity-ctx"
  rm -rf "$ctx" && mkdir -p "$ctx/amd64"
  cp "$ROOT/spdk-csi-driver/target/x86_64-unknown-linux-musl/release/"{csi-driver,flint-nfs-server} "$ctx/amd64/"
  DOCKER_BUILDKIT=1 docker build --platform linux/amd64 \
    -f "$ROOT/spdk-csi-driver/docker/Dockerfile.csi.prebuilt" --build-arg BIN_DIR=. -t "$DRIVER_IMAGE" "$ctx"
  rm -rf "$ctx"
  step "build $SPDK_IMAGE (Dockerfile.spdk_kind FROM $SPDK_BASE)"
  DOCKER_BUILDKIT=1 docker build -f "$ROOT/spdk-csi-driver/docker/Dockerfile.spdk_kind" \
    --build-arg BASE_IMAGE="$SPDK_BASE" -t "$SPDK_IMAGE" "$ROOT/spdk-csi-driver/docker"
fi
docker image inspect "$DRIVER_IMAGE" >/dev/null || fail "image $DRIVER_IMAGE not found locally"
docker image inspect "$SPDK_IMAGE" >/dev/null || fail "image $SPDK_IMAGE not found locally"

step "csi-sanity $CSI_TEST_VERSION and coshim into $WORK/bin"
export GOPATH="$WORK/gopath" GOCACHE="$WORK/gocache" CGO_ENABLED=0
GOBIN="$WORK/bin" go install "github.com/kubernetes-csi/csi-test/v5/cmd/csi-sanity@${CSI_TEST_VERSION}"
(cd "$HERE/coshim" && go build -o "$WORK/bin/coshim" .)

step "registry $REG_NAME on 127.0.0.1:$REG_PORT"
docker rm -f "$REG_NAME" >/dev/null 2>&1 || true
docker run -d --restart=no --name "$REG_NAME" -p "127.0.0.1:${REG_PORT}:5000" registry:2 >/dev/null
docker push "$DRIVER_IMAGE" >/dev/null
docker push "$SPDK_IMAGE" >/dev/null

step "kind cluster $CLUSTER: 3 nodes on the host's /dev"
kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
{
  cat <<EOF
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
containerdConfigPatches:
- |-
  [plugins."io.containerd.grpc.v1.cri".registry]
    config_path = "/etc/containerd/certs.d"
nodes:
EOF
  # Every kind node binds the HOST's /dev: a node's own /dev is a copy made
  # at container start, so the NVMe-oF namespace NodeStage connects would
  # never appear in it.
  for role in control-plane worker worker; do
    printf -- '- role: %s\n  extraMounts:\n  - hostPath: /dev\n    containerPath: /dev\n' "$role"
  done
} > "$WORK/kind.yaml"
kind create cluster --name "$CLUSTER" --image "$KIND_NODE_IMAGE" --kubeconfig "$KUBECONFIG" --config "$WORK/kind.yaml"
docker network connect kind "$REG_NAME" 2>/dev/null || true
for n in $(kind get nodes --name "$CLUSTER"); do
  # kind mounts /sys read-only in the node and the node pods inherit it:
  # `nvme disconnect` then fails silently (nvme-cli 3.1 exits 0) and every
  # unstage strands a kernel controller in `connecting`. A real node's
  # privileged pod has it writable.
  docker exec "$n" mount -o remount,rw /sys
  docker exec "$n" mkdir -p "/etc/containerd/certs.d/localhost:${REG_PORT}"
  printf '[host."http://%s:5000"]\n' "$REG_NAME" |
    docker exec -i "$n" cp /dev/stdin "/etc/containerd/certs.d/localhost:${REG_PORT}/hosts.toml"
done

step "helm install flint-csi (SPDK kindMode on a 1536 MB malloc disk per node, sanity proxy on)"
helm upgrade --install flint-csi "$ROOT/flint-csi-driver-chart" \
  --namespace flint-system --create-namespace \
  --set images.registry="localhost:${REG_PORT}" \
  --set images.flintCsiDriver.tag="$TAG" \
  --set spdkTarget.kindMode.enabled=true \
  --set spdkTarget.kindMode.image.tag="$TAG" \
  --set spdkTarget.kindMode.virtualDisk.sizeMB=1536 \
  --set spdkTarget.kindMode.interruptMode=false \
  --set spdkTarget.hugepages.enabled=false \
  --set dashboard.enabled=false \
  --set storageClass.parameters.nfsEmptyDir=false \
  --set driver.enableSanityTestProxy=true \
  --wait --timeout 12m
kubectl -n flint-system wait --for=condition=Ready pod -l app=flint-csi-node --timeout=300s
kubectl -n flint-system wait --for=condition=Ready pod -l app=flint-csi-controller --timeout=180s
for p in $(kubectl -n flint-system get pods -l app=flint-csi-node -o name); do
  # Not `grep -q`: it exits at the first match, kubectl dies of SIGPIPE,
  # and pipefail reads the found line as a failure.
  kubectl -n flint-system logs "$p" -c spdk-tgt | grep "Virtual disk ready: .*malloc-backed" >/dev/null ||
    fail "$p: no malloc-backed disk in its spdk-tgt log"
done
bad=$(kubectl -n flint-system get pods -o jsonpath='{range .items[*]}{range .spec.initContainers[*]}{.image}{"\n"}{end}{range .spec.containers[*]}{.image}{"\n"}{end}{end}' |
  grep -E '/(flint-driver|spdk-tgt-kind):' | grep -v ":${TAG}\$" || true)
[ -z "$bad" ] || fail "flint pods run another image: $bad"
if [ "$NICE_SPDK" = 1 ]; then
  for pid in $(ps -eo pid=,args= | awk '$2 ~ /(^|\/)spdk_tgt$/ {print $1}'); do
    for t in /proc/"$pid"/task/*; do sudo -n renice -n 19 -p "${t##*/}" >/dev/null; done
  done
fi

step "csi-sanity, coshim and the controller's kubeconfig into $NODE"
docker exec -i "$NODE" sh -c 'cat > /usr/local/bin/csi-sanity && chmod +x /usr/local/bin/csi-sanity' < "$WORK/bin/csi-sanity"
docker exec -i "$NODE" sh -c 'cat > /usr/local/bin/coshim && chmod +x /usr/local/bin/coshim' < "$WORK/bin/coshim"
docker exec -i "$NODE" sh -c 'cat > /usr/local/bin/rmpath && chmod +x /usr/local/bin/rmpath' < "$HERE/rmpath.sh"
docker exec "$CLUSTER-control-plane" cat /etc/kubernetes/admin.conf |
  docker exec -i "$NODE" sh -c 'cat > /root/admin.conf'

lvols() {
  for p in $(kubectl -n flint-system get pods -l app=flint-csi-node -o name); do
    printf '%s=%s ' "$(kubectl -n flint-system get "$p" -o jsonpath='{.spec.nodeName}')" \
      "$(kubectl -n flint-system exec "$p" -c spdk-tgt -- /usr/local/scripts/rpc.py bdev_lvol_get_lvols | grep -c '"alias"')"
  done
}

run_pass() {
  local pass=$1 mode=${1%-r*} replicas=${1##*-r} out="$WORK/results/$1" ctrl ep
  rm -rf "$out" && mkdir -p "$out"
  ctrl="$(kubectl -n flint-system get pod -l app=flint-csi-controller -o jsonpath='{.items[0].status.podIP}'):10000"
  ep=$ctrl
  if [ "$mode" = shim ]; then
    docker exec "$NODE" sh -c 'pkill -x coshim; : > /root/coshim.log' || true
    docker exec -d "$NODE" sh -c "exec coshim -controller $ctrl -kubeconfig /root/admin.conf >> /root/coshim.log 2>&1"
    for _ in $(seq 10); do docker exec "$NODE" grep -q 'coshim:' /root/coshim.log && break; sleep 1; done
    ep=127.0.0.1:10001
  fi
  # csi-sanity makes mountdir/stagingdir with a plain mkdir (the parent
  # must exist); rmpath stands in for its os.Remove so a file the driver
  # leaves behind is logged instead of failing every later spec's setup.
  docker exec "$NODE" sh -c "rmpath /var/lib/kubelet/sanity/$pass && mkdir -p /var/lib/kubelet/sanity/$pass && : > /root/rmpath.log" ||
    fail "a mount is still live under /var/lib/kubelet/sanity/$pass"
  printf 'numReplicas: "%s"\nthinProvision: "true"\n' "$replicas" | docker exec -i "$NODE" sh -c "cat > /root/params-$pass.yaml"
  step "pass $pass: numReplicas $replicas, controller endpoint $ep"
  local since
  since=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  docker exec "$NODE" csi-sanity \
    --csi.endpoint="unix://$SOCK" --csi.controllerendpoint="$ep" \
    --csi.mountdir="/var/lib/kubelet/sanity/$pass/mount" --csi.stagingdir="/var/lib/kubelet/sanity/$pass/stage" \
    --csi.testvolumesize=$((64 << 20)) --csi.testvolumeexpandsize=$((128 << 20)) \
    --csi.testvolumeparameters="/root/params-$pass.yaml" \
    --csi.removemountpathcmd=rmpath --csi.removestagingpathcmd=rmpath \
    --ginkgo.junit-report="/root/junit-$pass.xml" --ginkgo.no-color --ginkgo.v --ginkgo.timeout=90m \
    > "$out/sanity.log" 2>&1 || true
  docker exec "$NODE" cat "/root/junit-$pass.xml" > "$out/junit.xml" 2>/dev/null || true
  docker exec "$NODE" cat /root/rmpath.log > "$out/rmpath.log" 2>/dev/null || true
  if [ "$mode" = shim ]; then
    docker exec "$NODE" sh -c 'pkill -x coshim; cat /root/coshim.log' > "$out/coshim.log" 2>&1 || true
  fi
  kubectl -n flint-system logs -l app=flint-csi-controller -c flint-csi-controller --since-time="$since" --tail=-1 > "$out/controller.log" 2>&1 || true
  for p in $(kubectl -n flint-system get pods -l app=flint-csi-node -o name); do
    kubectl -n flint-system logs "$p" --all-containers --prefix --since-time="$since" --tail=-1 \
      > "$out/node-$(kubectl -n flint-system get "$p" -o jsonpath='{.spec.nodeName}').log" 2>&1 || true
  done
  {
    echo "lvols: $(lvols)"
    echo "coshim PVs: $(kubectl get pv -l app.kubernetes.io/managed-by=coshim --no-headers 2>/dev/null | wc -l), VolumeAttachments: $(kubectl get volumeattachment -l app.kubernetes.io/managed-by=coshim --no-headers 2>/dev/null | wc -l)"
    echo "host nvme: $(for c in /sys/class/nvme/nvme*; do [ -e "$c" ] || continue; printf '%s:%s:%s ' "${c##*/}" "$(cat "$c/transport")" "$(cat "$c/state")"; done)"
    echo "leftover in the mount/staging dirs: $(wc -l < "$out/rmpath.log")"
  } > "$out/census.txt"
  printf '%-8s %s\n' "$pass" "$(grep -E '^(SUCCESS|FAIL)!' "$out/sanity.log" | tail -1)" >> "$WORK/results/summary.txt"
  sed 's/^/         /' "$out/census.txt" >> "$WORK/results/summary.txt"
}

: > "$WORK/results/summary.txt"
for pass in $PASSES; do
  case "$pass" in raw-r[0-9]|shim-r[0-9]) run_pass "$pass" ;; *) fail "unknown pass $pass" ;; esac
done
step "summary ($WORK/results)"
cat "$WORK/results/summary.txt"
