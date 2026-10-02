#!/usr/bin/env bash
# kind-spdk.sh — run the SPDK Chainsaw suites (standard, clean-shutdown,
# replica-rebuild) against the driver built from THIS tree, on a throwaway
# kind cluster whose SPDK targets sit on ONE real disk.
#
# The disk is WIPED. It becomes an LVM volume group with one logical volume
# per kind node, named after the node (SPDK's uring bdev needs a whole
# block device with a /sys/block entry; a partition has none, an LV --
# dm-N -- does). Every kind node binds the HOST's /dev: a kind node's own
# /dev is a static copy made at container start, so a device the kernel
# creates later -- the NVMe-oF namespace the nvmeof backend connects for
# every volume -- would never appear there. The chart's
# virtualDisk.device /dev/flintkindspdk/%NODE% picks each node's LV. The
# chart runs SPDK in kindMode (`--no-pci`, discovery off: no node agent can
# unbind or claim ANY PCI device of the host), and the kind SPDK entrypoint
# opens its node's volume with io_uring and LOADS its lvstore on restart, so
# a killed spdk-tgt comes back with its data (replica-rebuild).
#
# Not the GitHub gate: a hosted runner has no spare disk. Run it on a
# Linux host with docker, kind, helm, kubectl, chainsaw, cargo-zigbuild,
# lvm2 and passwordless sudo.
#
# Env: DISK (required, e.g. /dev/nvme1n1)  CONFIRM_WIPE (required: the
#      disk's serial, from `lsblk -dno SERIAL $DISK`)
#      CLUSTER (flint-chainsaw-spdk)  REG_PORT (5002)  TAG (git short sha)
#      BUILD (1; 0 = both images already exist locally)  KEEP (0)
#      SUITES ("standard clean-shutdown replica-rebuild")
#      SPDK_BASE (dilipdalton/spdk-tgt:<the chart's pinned tag>)
#      KIND_NODE_IMAGE (kindest/node:v1.34.0)  REPORT_DIR (unset)
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
CLUSTER="${CLUSTER:-flint-chainsaw-spdk}"
REG_PORT="${REG_PORT:-5002}"
REG_NAME="${CLUSTER}-registry"
TAG="${TAG:-$(git -C "$ROOT" rev-parse --short HEAD)}"
BUILD="${BUILD:-1}"
KEEP="${KEEP:-0}"
SUITES="${SUITES:-standard clean-shutdown replica-rebuild}"
KIND_NODE_IMAGE="${KIND_NODE_IMAGE:-kindest/node:v1.34.0}"
SPDK_TAG=$(awk '/^  spdkTarget:/{f=1} f&&/tag:/{gsub(/"/,"",$2); print $2; exit}' "$ROOT/flint-csi-driver-chart/values.yaml")
SPDK_BASE="${SPDK_BASE:-dilipdalton/spdk-tgt:${SPDK_TAG}}"
DRIVER_IMAGE="localhost:${REG_PORT}/dilipdalton/flint-driver:${TAG}"
SPDK_IMAGE="localhost:${REG_PORT}/dilipdalton/spdk-tgt-kind:${TAG}"
NODES=3   # 1 control plane + 2 workers; every node runs an SPDK target
export KUBECONFIG="${KUBECONFIG_PRIVATE:-$(mktemp -d)/kubeconfig}"

step() { printf '\n▶ %s\n' "$*" >&2; }
fail() { printf '\n✗ %s\n' "$*" >&2; exit 1; }

for t in docker kind kubectl helm chainsaw wipefs $( [ "$BUILD" = 1 ] && echo cargo-zigbuild ); do
  command -v "$t" >/dev/null || fail "$t not found in PATH"
done

# ── The disk: refuse anything that is not an idle, explicitly named disk ──
DISK="${DISK:?set DISK, e.g. DISK=/dev/nvme1n1}"
[ -b "$DISK" ] || fail "$DISK is not a block device"
[ "$(lsblk -dno TYPE "$DISK")" = disk ] || fail "$DISK is not a whole disk"
serial=$(lsblk -dno SERIAL "$DISK" | tr -d ' ')
[ -n "${CONFIRM_WIPE:-}" ] && [ "$CONFIRM_WIPE" = "$serial" ] ||
  fail "this WIPES $DISK (serial $serial); set CONFIRM_WIPE=$serial to proceed"
if lsblk -nro MOUNTPOINTS "$DISK" | grep -q .; then
  fail "$DISK (or a partition) is mounted: $(lsblk -nro NAME,MOUNTPOINTS "$DISK" | awk '$2')"
fi
base=$(basename "$DISK")
# (A disk with no partitions matches no glob; `|| true` keeps that
# from killing the script under set -e, silently, with status 2.)
VG=flintkindspdk
# A previous run's VG on THIS disk is ours to remove; anything else
# holding the disk is not.
if sudo -n pvs --noheadings -o vg_name "$DISK" 2>/dev/null | grep -qx " *$VG"; then
  sudo -n vgremove -f "$VG" >/dev/null
  sudo -n pvremove -f "$DISK" >/dev/null
fi
holders=$( { ls /sys/block/"$base"/holders; for p in /sys/block/"$base"/"$base"*/holders; do [ -d "$p" ] && ls "$p"; done; } 2>/dev/null || true)
[ -z "$holders" ] || fail "$DISK has holders (LVM/dm/md?): $holders"

cleanup() {
  rc=$?
  if [ "$rc" -ne 0 ]; then
    step "FAILED (rc=$rc): driver state for the log"
    kubectl get pods -A -o wide 2>/dev/null || true
    kubectl -n flint-system logs -l app=flint-csi-controller --all-containers --tail=200 2>/dev/null || true
    kubectl -n flint-system logs -l app=flint-csi-node -c spdk-tgt --tail=60 2>/dev/null || true
  fi
  if [ "$KEEP" = 1 ]; then
    echo "KEEP=1: cluster $CLUSTER and registry $REG_NAME left up; KUBECONFIG=$KUBECONFIG" >&2
  else
    kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
    docker rm -f "$REG_NAME" >/dev/null 2>&1 || true
    # The nvmeof backend connects KERNEL initiators, which live in the
    # host kernel, not in the deleted node containers. Remove the flint
    # ones only (subsystem NQN prefix), or they retry for ctrl_loss_tmo.
    sudo -n vgremove -f "$VG" >/dev/null 2>&1 || true
    sudo -n pvremove -f "$DISK" >/dev/null 2>&1 || true
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

step "LVM on $DISK (serial $serial): VG $VG, one LV per node"
sudo -n wipefs -a "$DISK" >/dev/null
sudo -n blockdev --rereadpt "$DISK"   # drop any stale partitions the kernel still holds
sudo -n pvcreate -ff -y "$DISK" >/dev/null
sudo -n vgcreate "$VG" "$DISK" >/dev/null
NODE_NAMES="${CLUSTER}-control-plane"
for i in $(seq 2 "$NODES"); do
  # kind names workers <cluster>-worker, <cluster>-worker2, ...
  if [ "$i" -eq 2 ]; then w="${CLUSTER}-worker"; else w="${CLUSTER}-worker$(( i - 1 ))"; fi
  NODE_NAMES="$NODE_NAMES $w"
done
i=0
for n in $NODE_NAMES; do
  i=$(( i + 1 ))
  if [ "$i" -lt "$NODES" ]; then
    sudo -n lvcreate -y -W y -l "$(( 100 / NODES ))%VG" -n "$n" "$VG" >/dev/null
  else
    sudo -n lvcreate -y -W y -l 100%FREE -n "$n" "$VG" >/dev/null
  fi
  sudo -n wipefs -a "/dev/$VG/$n" >/dev/null
done
sudo -n lvs "$VG" >&2

if [ "$BUILD" = 1 ]; then
  step "build $DRIVER_IMAGE (cargo zigbuild + Dockerfile.csi.prebuilt, as published)"
  (cd "$ROOT/spdk-csi-driver" &&
    cargo zigbuild --release --target x86_64-unknown-linux-musl --bin csi-driver --bin flint-nfs-server)
  ctx="$ROOT/spdk-csi-driver/target/kind-spdk-ctx"
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

step "registry $REG_NAME on 127.0.0.1:$REG_PORT"
docker rm -f "$REG_NAME" >/dev/null 2>&1 || true
docker run -d --restart=no --name "$REG_NAME" -p "127.0.0.1:${REG_PORT}:5000" registry:2 >/dev/null
docker push "$DRIVER_IMAGE" >/dev/null
docker push "$SPDK_IMAGE" >/dev/null

step "kind cluster $CLUSTER: $NODES nodes on the host's /dev, node <n> uses LV $VG/<n>"
mounts() { printf '  extraMounts:\n  - hostPath: /dev\n    containerPath: /dev\n'; }
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
- role: control-plane
EOF
  mounts 1
  for i in $(seq 2 "$NODES"); do echo "- role: worker"; mounts "$i"; done
} > "$(dirname "$KUBECONFIG")/kind.yaml"
kind create cluster --name "$CLUSTER" --image "$KIND_NODE_IMAGE" --kubeconfig "$KUBECONFIG" \
  --config "$(dirname "$KUBECONFIG")/kind.yaml"
docker network connect kind "$REG_NAME" 2>/dev/null || true
for n in $(kind get nodes --name "$CLUSTER"); do
  docker exec "$n" mkdir -p "/etc/containerd/certs.d/localhost:${REG_PORT}"
  printf '[host."http://%s:5000"]\n' "$REG_NAME" |
    docker exec -i "$n" cp /dev/stdin "/etc/containerd/certs.d/localhost:${REG_PORT}/hosts.toml"
  docker exec "$n" test -b "/dev/$VG/$n" || fail "$n does not see /dev/$VG/$n"
done

step "helm install flint-csi (SPDK kindMode on /dev/$VG/<node>, image tag $TAG)"
helm upgrade --install flint-csi "$ROOT/flint-csi-driver-chart" \
  --namespace flint-system --create-namespace \
  --set images.registry="localhost:${REG_PORT}" \
  --set images.flintCsiDriver.tag="$TAG" \
  --set spdkTarget.kindMode.enabled=true \
  --set spdkTarget.kindMode.image.tag="$TAG" \
  --set spdkTarget.kindMode.virtualDisk.device="/dev/$VG/%NODE%" \
  --set spdkTarget.kindMode.interruptMode=false \
  --set spdkTarget.hugepages.enabled=false \
  --set dashboard.enabled=false \
  --set storageClass.parameters.nfsEmptyDir=false \
  ${HELM_EXTRA:-} \
  --wait --timeout 10m
kubectl -n flint-system wait --for=condition=Ready pod -l app=flint-csi-node --timeout=300s
kubectl -n flint-system wait --for=condition=Ready pod -l app=flint-csi-controller --timeout=180s

step "every SPDK target opened its volume"
for p in $(kubectl -n flint-system get pods -l app=flint-csi-node -o name); do
  kubectl -n flint-system logs "$p" -c spdk-tgt | grep -E "Disk ready: .*device-backed" ||
    fail "$p: no device-backed disk in its spdk-tgt log"
done

step "pods run the images under test"
bad=$(kubectl -n flint-system get pods -o jsonpath='{range .items[*]}{range .spec.initContainers[*]}{.image}{"\n"}{end}{range .spec.containers[*]}{.image}{"\n"}{end}{end}' |
  grep -E '/(flint-driver|spdk-tgt-kind):' | grep -v ":${TAG}\$" || true)
[ -z "$bad" ] || fail "flint pods run another image: $bad"

cd "$HERE"
rc=0
for s in $SUITES; do
  case "$s" in
    standard) dir=tests-standard ;;
    clean-shutdown) dir=tests ;;
    replica-rebuild) dir=tests-replica-rebuild ;;
    *) fail "unknown suite $s" ;;
  esac
  step "chainsaw: $s ($dir)"
  report=()
  if [ -n "${REPORT_DIR:-}" ]; then
    mkdir -p "$REPORT_DIR"
    report=(--report-format JUNIT-TEST --report-path "$REPORT_DIR" --report-name "chainsaw-$s")
  fi
  chainsaw test --config "chainsaw-$s.yaml" --test-dir "$dir" "${report[@]}" || rc=1
done
exit "$rc"
