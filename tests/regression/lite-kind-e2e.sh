#!/usr/bin/env bash
# Flint-lite kind e2e — the CHART's hub, a REAL PVC, a REAL kernel client.
#
# This is the L1 packaging proof, and unlike the pnfs-block chart pass it
# IS a data-plane test: the hub is a userspace server (no kernel floor in
# the docker VM), and the consumer is the Lima VM's real Linux NFS client.
# The path under test is the one an operator actually deploys:
#
#   helm (lite profile) → Deployment on kind → PVC from kind's DEFAULT
#   StorageClass (local-path — proving "any CSI driver" end to end) →
#   NodePort → docker port-map on the macOS host → Lima VM kernel mount.
#
# Legs:
#   1  the chart installs and the hub becomes Ready with a Bound PVC
#      from the default SC, announcing the standalone posture.
#   2  a kernel client mounts through the NodePort chain and the agent
#      battery runs: md5 write/read, sqlite, git.
#   3  bytes actually landed on the PVC (verified from inside the pod),
#      and the hub log carries ZERO LAYOUTGET lines.
#   4  hub pod restart under the live mount: strategy Recreate + sqlite
#      state on the PVC = same server id, so the client's filehandles
#      survive and the same file reads back byte-identical.
#
# The hub image is built HERE from the working tree (zigbuild musl →
# alpine), so the drill always tests the code you are sitting on, not a
# published tag. KEEP=1 leaves cluster + mount standing.
#
# Cleanup order matters: umount in the VM BEFORE deleting the cluster —
# a dead server under a live mount D-states umount (VM restart to clear).
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CHART="$REPO_ROOT/flint-lite-chart"
CARGO_DIR="$REPO_ROOT/spdk-csi-driver"

CLUSTER="${CLUSTER:-flint-lite-e2e}"
NS=flint-lite
IMG=flint-lite-dev:local
NODEPORT=32049
LIMA_VM="${LIMA_VM:-flint-nfs-client}"
MNT=/mnt/lite-kind
KUBECONFIG_FILE="$(mktemp -t flint-lite-kubeconfig.XXXXXX)"
export KUBECONFIG="$KUBECONFIG_FILE"

say()  { echo; echo "── $*"; }
pass() { echo "  ✓ $*"; }
fail() { echo "  ✗ FAIL: $*"; exit 1; }
vm()   { limactl shell "$LIMA_VM" -- sudo sh -c "$1"; }

cleanup() {
  set +e
  if [ "${KEEP:-0}" = "1" ]; then
    echo "KEEP=1 — leaving cluster and mount standing (kubeconfig: $KUBECONFIG_FILE)"
    return
  fi
  # Umount FIRST — see header.
  vm "umount -lf $MNT 2>/dev/null" 2>/dev/null
  kind delete cluster --name "$CLUSTER" >/dev/null 2>&1
  rm -f "$KUBECONFIG_FILE"
}
trap cleanup EXIT

echo "══════════════════════════════════════════════════════════════════"
echo " flint-lite kind e2e — chart hub, default-SC PVC, kernel client"
echo "══════════════════════════════════════════════════════════════════"

# ── 0. preflight ─────────────────────────────────────────────────────
for t in kind kubectl helm docker limactl; do
  command -v "$t" >/dev/null || fail "$t not installed"
done
docker info >/dev/null 2>&1 || fail "docker daemon not reachable"
limactl list --quiet 2>/dev/null | grep -qx "$LIMA_VM" \
  || fail "Lima VM '$LIMA_VM' not running — run 'make lima-up'"

# The docker VM's arch decides the target triple and image platform.
DARCH=$(docker info --format '{{.Architecture}}')
case "$DARCH" in
  aarch64|arm64) TRIPLE=aarch64-unknown-linux-musl; PLATFORM=linux/arm64 ;;
  x86_64|amd64)  TRIPLE=x86_64-unknown-linux-musl;  PLATFORM=linux/amd64 ;;
  *) fail "unrecognized docker VM arch: $DARCH" ;;
esac

# ── 1. build the hub image from the working tree ─────────────────────
say "building flint-pnfs-mds ($TRIPLE) and the $IMG image"
(cd "$CARGO_DIR" && cargo zigbuild --release --target "$TRIPLE" \
  --bin flint-pnfs-mds >/tmp/lite-e2e-build.log 2>&1) \
  || { tail -5 /tmp/lite-e2e-build.log; fail "zigbuild failed"; }
IMGDIR=$(mktemp -d -t flint-lite-img.XXXXXX)
cp "$CARGO_DIR/target/$TRIPLE/release/flint-pnfs-mds" "$IMGDIR/"
cat >"$IMGDIR/Dockerfile" <<'EOF'
FROM alpine:3.20
COPY flint-pnfs-mds /usr/local/bin/flint-pnfs-mds
EOF
docker build --platform "$PLATFORM" -t "$IMG" "$IMGDIR" \
  >/tmp/lite-e2e-imgbuild.log 2>&1 || { tail -5 /tmp/lite-e2e-imgbuild.log; fail "docker build failed"; }
rm -rf "$IMGDIR"
pass "image $IMG built ($PLATFORM)"

# ── 2. a kind cluster whose NodePort reaches the macOS host ──────────
# listenAddress 0.0.0.0 is load-bearing: the default binds 127.0.0.1
# only, and the Lima VM reaches the host on a non-loopback path.
say "creating kind cluster '$CLUSTER' (hostPort $NODEPORT → NodePort)"
KIND_CFG=$(mktemp -t flint-lite-kind.XXXXXX.yaml)
cat >"$KIND_CFG" <<EOF
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
  - role: control-plane
    extraPortMappings:
      - containerPort: $NODEPORT
        hostPort: $NODEPORT
        listenAddress: "0.0.0.0"
        protocol: TCP
EOF
kind delete cluster --name "$CLUSTER" >/dev/null 2>&1
kind create cluster --name "$CLUSTER" --config "$KIND_CFG" --wait 120s \
  >/dev/null 2>&1 || fail "kind create cluster failed"
rm -f "$KIND_CFG"
kind load docker-image "$IMG" --name "$CLUSTER" >/dev/null 2>&1 \
  || fail "kind load docker-image failed"
pass "cluster up, image loaded"

# ── 3. install the lite profile; leg 1 assertions ────────────────────
say "leg 1: helm install (lite profile) — Ready hub, Bound default-SC PVC"
helm install flint-lite "$CHART" --namespace "$NS" --create-namespace \
  --set image.ref="$IMG" \
  --set service.type=NodePort \
  --set service.nodePort=$NODEPORT \
  --set persistence.size=2Gi \
  >/tmp/lite-e2e-helm.log 2>&1 || { tail -5 /tmp/lite-e2e-helm.log; fail "helm install failed"; }
kubectl -n "$NS" rollout status deployment/flint-lite --timeout=120s >/dev/null 2>&1 \
  || { kubectl -n "$NS" get pods; kubectl -n "$NS" describe pod -l app=flint-lite | tail -15; \
       fail "hub never became Ready"; }
PVC_STATE=$(kubectl -n "$NS" get pvc flint-lite-data -o jsonpath='{.status.phase}')
[ "$PVC_STATE" = "Bound" ] || fail "PVC is $PVC_STATE, not Bound"
PVC_SC=$(kubectl -n "$NS" get pvc flint-lite-data -o jsonpath='{.spec.storageClassName}')
kubectl -n "$NS" logs deployment/flint-lite >/tmp/lite-e2e-hub.log 2>&1
grep -qc "STANDALONE" /tmp/lite-e2e-hub.log >/dev/null \
  || fail "hub log never announced the standalone posture"
pass "hub Ready; PVC Bound via StorageClass '$PVC_SC' (the cluster default — any-CSI proven)"

# ── 4. leg 2: the kernel client, through the whole chain ─────────────
say "leg 2: Lima kernel client mounts through NodePort and runs the battery"
HOST_IP=$(vm "getent hosts host.lima.internal | awk '{print \$1}'" | tr -d '\r')
[ -n "$HOST_IP" ] || fail "could not resolve host.lima.internal in the VM"
vm "command -v nc >/dev/null && nc -z -w 3 $HOST_IP $NODEPORT" \
  || fail "VM cannot reach $HOST_IP:$NODEPORT — docker port publish not visible to Lima?"
vm "mountpoint -q $MNT && umount -lf $MNT; mkdir -p $MNT; \
    timeout 30 mount -t nfs4 -o minorversion=1,proto=tcp,port=$NODEPORT \
      $HOST_IP:/ $MNT" || fail "mount through the NodePort chain failed"
pass "mounted $HOST_IP:$NODEPORT at $MNT"

vm "dd if=/dev/urandom of=$MNT/shared.bin bs=1M count=16 status=none conv=fsync" \
  || fail "write failed"
MD5_W=$(vm "md5sum $MNT/shared.bin" | awk '{print $1}' | tr -d '\r')
vm "echo 3 > /proc/sys/vm/drop_caches"
MD5_R=$(vm "md5sum $MNT/shared.bin" | awk '{print $1}' | tr -d '\r')
[ "$MD5_W" = "$MD5_R" ] || fail "cold reread $MD5_R != written $MD5_W"
pass "16 MiB write + cold reread byte-identical ($MD5_W)"

if vm "command -v sqlite3 >/dev/null"; then
  vm "timeout 60 sqlite3 $MNT/agents.db 'PRAGMA busy_timeout=10000; \
      CREATE TABLE t(n INTEGER); INSERT INTO t VALUES(1),(2),(3);' >/dev/null \
      && [ \"\$(sqlite3 $MNT/agents.db 'SELECT count(*) FROM t;')\" = 3 ] \
      && [ \"\$(sqlite3 $MNT/agents.db 'PRAGMA integrity_check;')\" = ok ]" \
    || fail "sqlite battery failed"
  pass "sqlite: create+insert+integrity ok"
fi
if vm "command -v git >/dev/null"; then
  vm "cd $MNT && rm -rf repo && mkdir repo && cd repo && \
      timeout 120 git init -q && echo one >f && timeout 120 git add f && \
      timeout 120 git -c user.email=a@lite -c user.name=a commit -qm kind-e2e" \
    || fail "git battery failed"
  pass "git: init/add/commit ok"
fi

# ── 5. leg 3: the bytes are on the PVC; the posture held ─────────────
say "leg 3: bytes on the PVC, zero LAYOUTGET"
POD_BYTES=$(kubectl -n "$NS" exec deployment/flint-lite -- \
  sh -c "wc -c < /data/exports/shared.bin" 2>/dev/null | tr -d ' \r')
[ "${POD_BYTES:-0}" = "16777216" ] \
  || fail "pod sees $POD_BYTES bytes at /data/exports/shared.bin, expected 16777216"
kubectl -n "$NS" logs deployment/flint-lite >/tmp/lite-e2e-hub.log 2>&1
LG=$(grep -c "LAYOUTGET" /tmp/lite-e2e-hub.log)
[ "${LG:-1}" = "0" ] || fail "$LG LAYOUTGET line(s) in the hub log"
pass "16777216 bytes at /data/exports on the PVC; no LAYOUTGET ever"

# ── 6. leg 4: hub restart under the live mount ───────────────────────
say "leg 4: hub pod restart under the live mount (Recreate + sqlite state)"
kubectl -n "$NS" delete pod -l app=flint-lite --wait=false >/dev/null 2>&1
kubectl -n "$NS" rollout status deployment/flint-lite --timeout=180s >/dev/null 2>&1 \
  || fail "hub never came back after pod delete"
# The client's mount must survive: hard mount retries, and the sqlite
# state on the PVC keeps the server id, so pre-restart filehandles stay
# valid (no BADHANDLE). Give the client a bounded window to recover.
MD5_R2=$(vm "echo 3 > /proc/sys/vm/drop_caches; timeout 90 md5sum $MNT/shared.bin" \
  | awk '{print $1}' | tr -d '\r')
[ "$MD5_R2" = "$MD5_W" ] \
  || fail "post-restart read '$MD5_R2' != '$MD5_W' — filehandles did not survive the restart"
pass "same file byte-identical through a hub restart — server id persisted on the PVC"

echo
echo "══════════════════════════════════════════════════════════════════"
echo " PASS — the CHART's lite hub, on a default-SC PVC, served a real"
echo " kernel client through the NodePort chain, and survived a pod"
echo " restart under the live mount. This is the L1 packaging, proven."
echo "══════════════════════════════════════════════════════════════════"
