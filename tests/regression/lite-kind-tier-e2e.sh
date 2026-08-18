#!/usr/bin/env bash
# Flint-lite kind e2e WITH THE S3 TIER — the L3 chart wiring, proven live.
#
# lite-kind-e2e.sh proves the L1 packaging (chart hub, default-SC PVC,
# real kernel client). This drill proves what L3 added on top: the
# chart-rendered tier config drives the whole S3 loop against an
# IN-CLUSTER MinIO — the shape an operator's `lite.tier.endpoint`
# actually points at — with credentials riding the operator Secret via
# envFrom, exactly as the chart wires them.
#
#   helm (lite profile + lite.tier.*) → hub Deployment on kind →
#   MinIO Deployment+Service in the same cluster → NodePort → docker
#   port-map → Lima VM kernel mount.
#
# Legs:
#   1  MinIO up, bucket pre-created (the hub never creates buckets);
#      the chart installs with tier on: hub Ready, startupProbe present
#      on the live pod, the epoch CLAIMED (log) and the epoch control
#      object in the bucket under keyPrefix/.flint/.
#   2  kernel client writes through the mount; the flush pipeline
#      publishes: the data object appears under keyPrefix/ at full
#      size, and a .flint/ manifest rides the barrier.
#   3  hub pod restart under the live mount: self-recognition re-claims
#      the epoch from the SAME PVC state (no takeover wait), the
#      client's filehandles survive, reread is byte-identical.
#   4  DR FROM THE BUCKET ALONE: umount, helm uninstall (the chart PVC
#      dies with the release — local-path reclaims the data), reinstall
#      fresh. The hub waits out the dead holder's lease, takes the
#      epoch over, and import-on-start restores the namespace as
#      EVICTED STUBS — so the client's reread also proves LIVE
#      HYDRATION through the chart config (readers park on DELAY until
#      the restore commits). Contents must come back byte-identical.
#
# Tier knobs are set small (flush floor 3s, heartbeat 2s × 3 misses)
# so the loop and the takeover fit an e2e; the knob NAMES go through
# the chart's settings map, so a schema drift fails at render, not in
# the pod. KEEP=1 leaves everything standing.
#
# Cleanup order matters: umount in the VM BEFORE deleting the cluster —
# a dead server under a live mount D-states umount (VM restart to clear).
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CHART="$REPO_ROOT/flint-csi-driver-chart"
CARGO_DIR="$REPO_ROOT/spdk-csi-driver"

CLUSTER="${CLUSTER:-flint-lite-tier-e2e}"
NS=flint-lite
IMG=flint-lite-dev:local
NODEPORT=32050
LIMA_VM="${LIMA_VM:-flint-nfs-client}"
MNT=/mnt/lite-tier-kind
BUCKET=flint-lite-e2e
PREFIX=vol1/
MINIO_USER=flintdrill
MINIO_PASS=flintdrill123
PF_PORT=39000
PF_PID=""
KUBECONFIG_FILE="$(mktemp -t flint-tier-kubeconfig.XXXXXX)"
export KUBECONFIG="$KUBECONFIG_FILE"

say()  { echo; echo "── $*"; }
pass() { echo "  ✓ $*"; }
fail() { echo "  ✗ FAIL: $*"; exit 1; }
vm()   { limactl shell "$LIMA_VM" -- sudo sh -c "$1"; }
s3() {
  env -u AWS_PROFILE \
    AWS_ACCESS_KEY_ID=$MINIO_USER AWS_SECRET_ACCESS_KEY=$MINIO_PASS \
    AWS_DEFAULT_REGION=us-east-1 AWS_EC2_METADATA_DISABLED=true \
    aws --endpoint-url "http://127.0.0.1:$PF_PORT" "$@"
}

cleanup() {
  set +e
  [ -n "$PF_PID" ] && kill "$PF_PID" 2>/dev/null
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

# The MinIO port-forward drops whenever its pod restarts; (re)arm it and
# wait until the API answers through it.
pf_up() {
  [ -n "$PF_PID" ] && kill "$PF_PID" 2>/dev/null
  kubectl -n "$NS" port-forward svc/minio "$PF_PORT:9000" >/dev/null 2>&1 &
  PF_PID=$!
  for _ in $(seq 1 20); do
    curl -sf "http://127.0.0.1:$PF_PORT/minio/health/live" >/dev/null && return 0
    sleep 1
  done
  fail "MinIO port-forward never became healthy"
}

echo "══════════════════════════════════════════════════════════════════"
echo " flint-lite kind e2e + S3 TIER — chart wiring, MinIO, DR, hydrate"
echo "══════════════════════════════════════════════════════════════════"

# ── 0. preflight ─────────────────────────────────────────────────────
for t in kind kubectl helm docker limactl aws curl; do
  command -v "$t" >/dev/null || fail "$t not installed"
done
docker info >/dev/null 2>&1 || fail "docker daemon not reachable"
limactl list --quiet 2>/dev/null | grep -qx "$LIMA_VM" \
  || fail "Lima VM '$LIMA_VM' not running — run 'make lima-up'"

DARCH=$(docker info --format '{{.Architecture}}')
case "$DARCH" in
  aarch64|arm64) TRIPLE=aarch64-unknown-linux-musl; PLATFORM=linux/arm64 ;;
  x86_64|amd64)  TRIPLE=x86_64-unknown-linux-musl;  PLATFORM=linux/amd64 ;;
  *) fail "unrecognized docker VM arch: $DARCH" ;;
esac

# ── 1. build the hub image from the working tree ─────────────────────
say "building flint-pnfs-mds ($TRIPLE) and the $IMG image"
(cd "$CARGO_DIR" && cargo zigbuild --release --target "$TRIPLE" \
  --bin flint-pnfs-mds >/tmp/lite-tier-build.log 2>&1) \
  || { tail -5 /tmp/lite-tier-build.log; fail "zigbuild failed"; }
IMGDIR=$(mktemp -d -t flint-tier-img.XXXXXX)
cp "$CARGO_DIR/target/$TRIPLE/release/flint-pnfs-mds" "$IMGDIR/"
cat >"$IMGDIR/Dockerfile" <<'EOF'
FROM alpine:3.20
COPY flint-pnfs-mds /usr/local/bin/flint-pnfs-mds
EOF
docker build --platform "$PLATFORM" -t "$IMG" "$IMGDIR" \
  >/tmp/lite-tier-imgbuild.log 2>&1 || { tail -5 /tmp/lite-tier-imgbuild.log; fail "docker build failed"; }
rm -rf "$IMGDIR"
pass "image $IMG built ($PLATFORM)"

# ── 2. kind cluster + in-cluster MinIO + bucket + creds Secret ───────
say "creating kind cluster '$CLUSTER' (hostPort $NODEPORT → NodePort)"
KIND_CFG=$(mktemp -t flint-tier-kind.XXXXXX.yaml)
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

say "MinIO in-cluster + bucket $BUCKET + Secret flint-tier-s3"
kubectl create namespace "$NS" >/dev/null 2>&1
kubectl -n "$NS" apply -f - >/dev/null <<EOF || fail "MinIO manifests refused"
apiVersion: apps/v1
kind: Deployment
metadata: { name: minio }
spec:
  replicas: 1
  selector: { matchLabels: { app: minio } }
  template:
    metadata: { labels: { app: minio } }
    spec:
      containers:
        - name: minio
          image: quay.io/minio/minio
          args: ["server", "/data"]
          env:
            - { name: MINIO_ROOT_USER, value: "$MINIO_USER" }
            - { name: MINIO_ROOT_PASSWORD, value: "$MINIO_PASS" }
          ports: [{ containerPort: 9000 }]
          volumeMounts: [{ name: data, mountPath: /data }]
      volumes: [{ name: data, emptyDir: {} }]
---
apiVersion: v1
kind: Service
metadata: { name: minio }
spec:
  selector: { app: minio }
  ports: [{ port: 9000, targetPort: 9000 }]
EOF
kubectl -n "$NS" create secret generic flint-tier-s3 \
  --from-literal=AWS_ACCESS_KEY_ID=$MINIO_USER \
  --from-literal=AWS_SECRET_ACCESS_KEY=$MINIO_PASS >/dev/null \
  || fail "creds Secret refused"
kubectl -n "$NS" rollout status deployment/minio --timeout=180s >/dev/null 2>&1 \
  || fail "MinIO never became Ready"
pf_up
s3 s3 mb "s3://$BUCKET" >/dev/null || fail "bucket create failed"
pass "MinIO Ready, bucket created, Secret in place"

# ── 3. leg 1: helm install with the tier ON ──────────────────────────
say "leg 1: helm install (lite + tier) — Ready hub, epoch claimed, epoch object in the bucket"
helm install flint-lite "$CHART" --namespace "$NS" \
  --set lite.enabled=true \
  --set lite.image.ref="$IMG" \
  --set lite.service.type=NodePort \
  --set lite.service.nodePort=$NODEPORT \
  --set lite.persistence.size=2Gi \
  --set lite.tier.enabled=true \
  --set lite.tier.bucket=$BUCKET \
  --set lite.tier.keyPrefix=$PREFIX \
  --set lite.tier.endpoint=http://minio.$NS.svc:9000 \
  --set lite.tier.region=us-east-1 \
  --set lite.tier.credentialsSecret=flint-tier-s3 \
  --set lite.tier.settings.flushFloorSecs=3 \
  --set lite.tier.settings.quiesceSecs=1 \
  --set lite.tier.settings.tickSecs=2 \
  --set lite.tier.settings.epochHeartbeatSecs=2 \
  --set lite.tier.settings.epochLeaseMisses=3 \
  >/tmp/lite-tier-helm.log 2>&1 || { tail -5 /tmp/lite-tier-helm.log; fail "helm install failed"; }
kubectl -n "$NS" rollout status deployment/flint-lite --timeout=180s >/dev/null 2>&1 \
  || { kubectl -n "$NS" get pods; kubectl -n "$NS" describe pod -l app=flint-lite | tail -15; \
       fail "hub never became Ready"; }
SP=$(kubectl -n "$NS" get pod -l app=flint-lite \
  -o jsonpath='{.items[0].spec.containers[0].startupProbe.failureThreshold}')
[ "$SP" = "60" ] || fail "startupProbe missing on the live pod (failureThreshold '$SP')"
kubectl -n "$NS" logs deployment/flint-lite >/tmp/lite-tier-hub.log 2>&1
grep -q "epoch .* held" /tmp/lite-tier-hub.log \
  || fail "hub log never announced the epoch claim"
s3 s3api list-objects-v2 --bucket "$BUCKET" --prefix "${PREFIX}.flint/" \
  --query 'KeyCount' --output text | grep -qv '^0$' \
  || fail "no ${PREFIX}.flint/ control object in the bucket after the claim"
pass "hub Ready with startupProbe(${SP}x10s); epoch claimed; control object present"

# ── 4. leg 2: kernel client writes; the flush pipeline publishes ─────
say "leg 2: kernel client writes; objects appear under ${PREFIX} in the bucket"
HOST_IP=$(vm "getent hosts host.lima.internal | awk '{print \$1}'" | tr -d '\r')
[ -n "$HOST_IP" ] || fail "could not resolve host.lima.internal in the VM"
vm "mountpoint -q $MNT && umount -lf $MNT; mkdir -p $MNT; \
    timeout 30 mount -t nfs4 -o minorversion=1,proto=tcp,port=$NODEPORT \
      $HOST_IP:/ $MNT" || fail "mount through the NodePort chain failed"
vm "dd if=/dev/urandom of=$MNT/shared.bin bs=1M count=16 status=none conv=fsync" \
  || fail "write failed"
MD5_W=$(vm "md5sum $MNT/shared.bin" | awk '{print $1}' | tr -d '\r')
vm "mkdir -p $MNT/tree && echo 'the tier round-trips' > $MNT/tree/hello.txt && sync"
# Flush floor 3s + tick 2s + quiesce 1s ⇒ the publish lands in seconds;
# poll the bucket rather than trusting a sleep.
OBJ_SIZE=""
for _ in $(seq 1 60); do
  OBJ_SIZE=$(s3 s3api head-object --bucket "$BUCKET" --key "${PREFIX}shared.bin" \
    --query 'ContentLength' --output text 2>/dev/null)
  [ "$OBJ_SIZE" = "16777216" ] && break
  sleep 2
done
[ "$OBJ_SIZE" = "16777216" ] \
  || fail "${PREFIX}shared.bin never reached the bucket at full size (last: '$OBJ_SIZE')"
s3 s3api head-object --bucket "$BUCKET" --key "${PREFIX}tree/hello.txt" >/dev/null 2>&1 \
  || fail "${PREFIX}tree/hello.txt never reached the bucket"
pass "16 MiB + tree published through the chart-rendered tier config"

# ── 5. leg 3: hub restart under the live mount (self-recognition) ────
say "leg 3: hub pod restart — same PVC, instant epoch re-claim, mount survives"
kubectl -n "$NS" delete pod -l app=flint-lite --wait=false >/dev/null 2>&1
kubectl -n "$NS" rollout status deployment/flint-lite --timeout=180s >/dev/null 2>&1 \
  || fail "hub never came back after pod delete"
kubectl -n "$NS" logs deployment/flint-lite >/tmp/lite-tier-hub2.log 2>&1
grep -q "epoch .* held" /tmp/lite-tier-hub2.log \
  || fail "restarted hub never re-claimed the epoch"
MD5_R=$(vm "echo 3 > /proc/sys/vm/drop_caches; timeout 90 md5sum $MNT/shared.bin" \
  | awk '{print $1}' | tr -d '\r')
[ "$MD5_R" = "$MD5_W" ] \
  || fail "post-restart read '$MD5_R' != '$MD5_W'"
pass "epoch re-claimed from PVC state; reread byte-identical through the restart"

# ── 6. leg 4: DR from the bucket alone (+ live hydration) ────────────
say "leg 4: helm uninstall (PVC dies) → fresh install → import → hydrate-on-read"
vm "umount -lf $MNT" || fail "pre-DR umount failed"
helm uninstall flint-lite -n "$NS" >/dev/null 2>&1 || fail "helm uninstall failed"
for _ in $(seq 1 60); do
  kubectl -n "$NS" get pvc flint-lite-data >/dev/null 2>&1 || break
  sleep 2
done
kubectl -n "$NS" get pvc flint-lite-data >/dev/null 2>&1 \
  && fail "chart PVC survived uninstall — DR premise not met"
helm install flint-lite "$CHART" --namespace "$NS" \
  --set lite.enabled=true \
  --set lite.image.ref="$IMG" \
  --set lite.service.type=NodePort \
  --set lite.service.nodePort=$NODEPORT \
  --set lite.persistence.size=2Gi \
  --set lite.tier.enabled=true \
  --set lite.tier.bucket=$BUCKET \
  --set lite.tier.keyPrefix=$PREFIX \
  --set lite.tier.endpoint=http://minio.$NS.svc:9000 \
  --set lite.tier.region=us-east-1 \
  --set lite.tier.credentialsSecret=flint-tier-s3 \
  --set lite.tier.settings.flushFloorSecs=3 \
  --set lite.tier.settings.quiesceSecs=1 \
  --set lite.tier.settings.tickSecs=2 \
  --set lite.tier.settings.epochHeartbeatSecs=2 \
  --set lite.tier.settings.epochLeaseMisses=3 \
  >/tmp/lite-tier-helm2.log 2>&1 || { tail -5 /tmp/lite-tier-helm2.log; fail "DR reinstall failed"; }
# The fresh hub has NO prior state: it must wait out the dead holder's
# lease (2s × 3 misses), take the epoch over, and import the namespace
# as evicted stubs — all BEFORE the listener binds. The startupProbe is
# what buys this window in production; 300s bounds it here.
kubectl -n "$NS" rollout status deployment/flint-lite --timeout=300s >/dev/null 2>&1 \
  || { kubectl -n "$NS" describe pod -l app=flint-lite | tail -15; \
       fail "DR hub never became Ready (takeover/import wedged?)"; }
kubectl -n "$NS" logs deployment/flint-lite >/tmp/lite-tier-hub3.log 2>&1
grep -q "restoring from the bucket" /tmp/lite-tier-hub3.log \
  || fail "DR hub never entered import-on-start"
grep -Eq "tier import: .*stub\(s\) restored" /tmp/lite-tier-hub3.log \
  || fail "import never reported restored stubs"
vm "timeout 30 mount -t nfs4 -o minorversion=1,proto=tcp,port=$NODEPORT \
      $HOST_IP:/ $MNT" || fail "post-DR mount failed"
# The imported files are EVICTED STUBS: this reread parks on DELAY
# until the ranged-GET restore commits — live hydration, chart-wired.
MD5_DR=$(vm "timeout 120 md5sum $MNT/shared.bin" | awk '{print $1}' | tr -d '\r')
[ "$MD5_DR" = "$MD5_W" ] \
  || fail "post-DR hydrated read '$MD5_DR' != '$MD5_W' — the bucket round-trip lost bytes"
HELLO=$(vm "timeout 60 cat $MNT/tree/hello.txt" | tr -d '\r')
[ "$HELLO" = "the tier round-trips" ] || fail "tree/hello.txt came back as '$HELLO'"
pass "DR from the bucket alone: import + takeover + hydrate-on-read, byte-identical"

echo
echo "══════════════════════════════════════════════════════════════════"
echo " PASS — the CHART's tier config drove the full loop live: claim,"
echo " flush to MinIO, restart re-claim, and DR-from-bucket with"
echo " hydration, with credentials via the operator Secret. L3 proven."
echo "══════════════════════════════════════════════════════════════════"
