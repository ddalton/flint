#!/usr/bin/env bash
# The two fixes that the pNFS suite and unit tests cover but no cluster had:
#
#   F1  every lite hub must advertise a UNIQUE NFS server identity.
#       `StateManager::new("")` gave them all `flint-nfs` /
#       `flint-nfs-standalone`, so an agent mounting two workspaces hands
#       the kernel one identity at two addresses.
#
#   F2  a hub that could not read the bucket's manifest must NEVER publish
#       a barrier. It serves an EMPTY export, and one barrier from that
#       replaces a real manifest with one naming no files — losing every
#       directory, symlink and mode, which live ONLY in the manifest.
#
# Run it TWICE. HUB_IMAGE selects the build under test, so the same legs
# run against the unfixed 1.35.0 and must FAIL there:
#
#   HUB_IMAGE=<locally built>            -> expect PASS
#   HUB_IMAGE=dilipdalton/flint-pnfs:1.35.0 -> expect BOTH legs to fail
#
# kind + in-cluster MinIO. Free. Neither leg needs real S3, real NVMe or
# node-failure behaviour, so a paid cluster buys nothing here.
set -uo pipefail

CLUSTER="${CLUSTER:-flint-fence-drill}"
NS="${NS:-fence}"
OPNS="${OPNS:-flint-system}"
BUCKET="${BUCKET:-fence-drill}"
MINIO_USER=minioadmin
MINIO_PASS=minioadmin123
PF_S3="${PF_S3:-39701}"
CARGO_DIR="$(cd "$(dirname "$0")/../../spdk-csi-driver" && pwd)"
EXPECT="${EXPECT:-pass}"          # pass | fail  (fail = running the control)
KEEP="${KEEP:-}"

PASSES=0; FAILURES=()
say()  { echo; echo "── $*"; }
pass() { echo "  ✓ $*"; PASSES=$((PASSES+1)); }
fail() { echo "  ✗ FAIL: $*"; exit 1; }
bad()  { echo "  ✗ FAIL: $*"; FAILURES+=("$*"); }
note() { echo "    · $*" >&2; }
s3()   { AWS_ACCESS_KEY_ID=$MINIO_USER AWS_SECRET_ACCESS_KEY=$MINIO_PASS \
         AWS_DEFAULT_REGION=us-east-1 aws --endpoint-url "http://127.0.0.1:$PF_S3" "$@" 2>&1; }

PF_PID=""
pf_s3() {
  [ -n "$PF_PID" ] && kill "$PF_PID" 2>/dev/null
  kubectl -n "$NS" port-forward svc/minio "$PF_S3:9000" >/dev/null 2>&1 &
  PF_PID=$!
  for _ in $(seq 1 30); do
    s3 s3 ls >/dev/null 2>&1 && return 0
    sleep 1
  done
  return 1
}
cleanup() {
  [ -n "$PF_PID" ] && kill "$PF_PID" 2>/dev/null
  [ -z "$KEEP" ] && kind delete cluster --name "$CLUSTER" >/dev/null 2>&1
  return 0
}
trap cleanup EXIT

echo "══════════════════════════════════════════════════════════════════"
echo " fence + identity drill    (expecting these legs to $EXPECT)"
echo "══════════════════════════════════════════════════════════════════"

case "$(uname -m)" in
  arm64|aarch64) TRIPLE=aarch64-unknown-linux-musl; PLATFORM=linux/arm64 ;;
  *)             TRIPLE=x86_64-unknown-linux-musl;  PLATFORM=linux/amd64 ;;
esac
OPIMG=flint-lite-operator:fence-drill
LOCAL_HUB=flint-pnfs:fence-drill
HUB_IMAGE="${HUB_IMAGE:-$LOCAL_HUB}"

say "building the operator (and the hub, unless HUB_IMAGE overrides it)"
(cd "$CARGO_DIR" && cargo zigbuild --release --target "$TRIPLE" \
   --bin flint-pnfs-mds --bin flint-lite-operator --bin flint-hub-gateway \
   >/tmp/fence-build.log 2>&1) || { tail -20 /tmp/fence-build.log; fail "zigbuild failed"; }
IMGDIR=$(mktemp -d -t flint-fence-img.XXXXXX)
for b in flint-pnfs-mds flint-lite-operator flint-hub-gateway; do
  cp "$CARGO_DIR/target/$TRIPLE/release/$b" "$IMGDIR/"
done
cat >"$IMGDIR/Dockerfile.hub" <<'EOF'
FROM alpine:3.20
RUN apk add --no-cache curl ca-certificates
COPY flint-pnfs-mds /usr/local/bin/flint-pnfs-mds
EOF
cat >"$IMGDIR/Dockerfile.op" <<'EOF'
FROM alpine:3.20
RUN apk add --no-cache ca-certificates
COPY flint-lite-operator /usr/local/bin/flint-lite-operator
COPY flint-hub-gateway /usr/local/bin/flint-hub-gateway
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/flint-lite-operator"]
EOF
docker build --platform "$PLATFORM" -f "$IMGDIR/Dockerfile.hub" -t "$LOCAL_HUB" "$IMGDIR" \
  >/tmp/fence-img.log 2>&1 || { tail -5 /tmp/fence-img.log; fail "hub image build failed"; }
docker build --platform "$PLATFORM" -f "$IMGDIR/Dockerfile.op" -t "$OPIMG" "$IMGDIR" \
  >>/tmp/fence-img.log 2>&1 || { tail -5 /tmp/fence-img.log; fail "op image build failed"; }
rm -rf "$IMGDIR"
note "hub under test: $HUB_IMAGE"

say "kind cluster '$CLUSTER'"
kind delete cluster --name "$CLUSTER" >/dev/null 2>&1
kind create cluster --name "$CLUSTER" --wait 120s >/dev/null 2>&1 || fail "kind create failed"
kind load docker-image "$OPIMG" --name "$CLUSTER" >/dev/null 2>&1 || fail "kind load op failed"
if [ "$HUB_IMAGE" = "$LOCAL_HUB" ]; then
  kind load docker-image "$LOCAL_HUB" --name "$CLUSTER" >/dev/null 2>&1 || fail "kind load hub failed"
  PULL=IfNotPresent
else
  PULL=IfNotPresent
  docker pull --platform "$PLATFORM" "$HUB_IMAGE" >/dev/null 2>&1 \
    || fail "cannot pull the control image $HUB_IMAGE for $PLATFORM"
  kind load docker-image "$HUB_IMAGE" --name "$CLUSTER" >/dev/null 2>&1 \
    || fail "kind load $HUB_IMAGE failed"
fi
pass "cluster up, images loaded"

say "MinIO + bucket"
kubectl create namespace "$NS" >/dev/null 2>&1
kubectl -n "$NS" apply -f - >/dev/null <<EOF || fail "MinIO refused"
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
kubectl -n "$NS" rollout status deployment/minio --timeout=180s >/dev/null 2>&1 \
  || fail "MinIO never Ready"
pf_s3 || fail "port-forward to MinIO failed"
s3 s3 mb "s3://$BUCKET" >/dev/null 2>&1
pass "MinIO Ready, bucket $BUCKET"

say "operator (hub image: $HUB_IMAGE)"
kubectl create namespace "$OPNS" >/dev/null 2>&1
kubectl apply -f "$CARGO_DIR/../flint-lite-operator-chart/crds/flintshares.yaml" >/dev/null 2>&1 \
  || fail "CRD apply failed"
kubectl -n "$OPNS" create serviceaccount flint-lite-operator >/dev/null 2>&1
kubectl create clusterrolebinding flint-lite-operator-fence \
  --clusterrole=cluster-admin --serviceaccount="$OPNS:flint-lite-operator" >/dev/null 2>&1
kubectl -n "$OPNS" apply -f - >/dev/null <<EOF || fail "operator refused"
apiVersion: apps/v1
kind: Deployment
metadata: { name: flint-lite-operator }
spec:
  replicas: 1
  selector: { matchLabels: { app: flint-lite-operator } }
  template:
    metadata: { labels: { app: flint-lite-operator } }
    spec:
      serviceAccountName: flint-lite-operator
      containers:
        - name: operator
          image: $OPIMG
          imagePullPolicy: IfNotPresent
          args:
            - "--hub-image=$HUB_IMAGE"
            - "--hub-image-pull-policy=$PULL"
            - "--election=disabled"
            - "--manage-crd=false"
          env:
            - { name: RUST_LOG, value: "info" }
            - { name: POD_NAMESPACE, valueFrom: { fieldRef: { fieldPath: metadata.namespace } } }
            - { name: POD_NAME, valueFrom: { fieldRef: { fieldPath: metadata.name } } }
EOF
kubectl -n "$OPNS" rollout status deployment/flint-lite-operator --timeout=180s >/dev/null 2>&1 \
  || fail "operator never Ready"
pass "operator Ready"

mkshare() {  # mkshare <name> <prefix>
  kubectl -n "$NS" create secret generic flint-s3 \
    --from-literal=AWS_ACCESS_KEY_ID=$MINIO_USER \
    --from-literal=AWS_SECRET_ACCESS_KEY=$MINIO_PASS \
    --dry-run=client -o yaml 2>/dev/null | kubectl apply -f - >/dev/null 2>&1
  kubectl apply -f - >/dev/null <<EOF || fail "share $1 refused"
apiVersion: chert.us/v1alpha1
kind: FlintShare
metadata: { name: $1, namespace: $NS, labels: { chert.us/project-id: $1 } }
spec:
  bucket: $BUCKET
  region: us-east-1
  endpoint: http://minio.$NS.svc:9000
  keyPrefix: $2
  credentialsSecretRef: flint-s3
  persistence: { size: 1Gi }
  settings: { flushFloorSecs: 2 }
  monitoring: { enabled: true }
EOF
  for _ in $(seq 1 60); do
    [ "$(kubectl -n "$NS" get flintshare "$1" -o jsonpath='{.status.phase}' 2>/dev/null)" = "Ready" ] && return 0
    sleep 5
  done
  return 1
}
hubpod() { kubectl -n "$NS" get pods -l chert.us/share="$1" \
  -o jsonpath='{.items[0].metadata.name}' 2>/dev/null; }

# ══ F1: two hubs, two identities ═════════════════════════════════════
say "F1: two lite hubs must advertise DISTINCT NFS server identities"
mkshare fs-one one/ || fail "fs-one never Ready"
mkshare fs-two two/ || fail "fs-two never Ready"
ID1=$(kubectl -n "$NS" logs "$(hubpod fs-one)" 2>/dev/null \
      | sed -n 's/.*server_owner=\([^, ]*\).*/\1/p' | head -1)
ID2=$(kubectl -n "$NS" logs "$(hubpod fs-two)" 2>/dev/null \
      | sed -n 's/.*server_owner=\([^, ]*\).*/\1/p' | head -1)
note "fs-one server_owner=${ID1:-<none>}"
note "fs-two server_owner=${ID2:-<none>}"
if [ -z "$ID1" ] || [ -z "$ID2" ]; then
  bad "could not read server_owner from both hub logs — no oracle"
elif [ "$ID1" = "$ID2" ]; then
  bad "BOTH hubs advertise '$ID1' — a client mounting both is told they are ONE server"
else
  pass "distinct identities ($ID1 vs $ID2) — two workspaces cannot alias each other's client state"
fi

# ══ F2: a refused import must not publish a barrier ══════════════════
say "F2: a hub that cannot read the manifest must NEVER publish a barrier"
POD=$(hubpod fs-one)
# Shape that lives ONLY in the manifest: a directory and a symlink.
kubectl -n "$NS" exec "$POD" -- sh -c \
  'mkdir -p /data/exports/keepdir && ln -sf /data/exports/real.txt /data/exports/link \
   && printf hello > /data/exports/real.txt' >/dev/null 2>&1 \
  || note "could not seed via exec (continuing; the manifest may still exist)"
sleep 12   # let a flush barrier land
MKEY="one/.flint/manifest"
BEFORE=$(s3 s3api head-object --bucket "$BUCKET" --key "$MKEY" --query ContentLength --output text 2>/dev/null)
note "manifest before: ${BEFORE:-<absent>} bytes"
[ -n "$BEFORE" ] && [ "$BEFORE" != "None" ] || bad "no manifest was ever published — F2 has nothing to protect"

# Corrupt it, then restart the hub so the import refuses.
printf 'NOT-A-MANIFEST' > /tmp/fence-garbage
s3 s3 cp /tmp/fence-garbage "s3://$BUCKET/$MKEY" >/dev/null 2>&1 || bad "could not corrupt the manifest"
GARBAGE=$(s3 s3api head-object --bucket "$BUCKET" --key "$MKEY" --query ContentLength --output text 2>/dev/null)
note "manifest corrupted to: ${GARBAGE:-?} bytes"

kubectl -n "$NS" delete pod "$POD" --wait=true >/dev/null 2>&1
for _ in $(seq 1 40); do
  P=$(hubpod fs-one); [ -n "$P" ] && \
    [ "$(kubectl -n "$NS" get pod "$P" -o jsonpath='{.status.phase}' 2>/dev/null)" = "Running" ] && break
  sleep 5
done
POD2=$(hubpod fs-one)
note "restarted hub: ${POD2:-<none>}"
sleep 25    # several flush ticks — the window the barrier would land in

AFTER=$(s3 s3api head-object --bucket "$BUCKET" --key "$MKEY" --query ContentLength --output text 2>/dev/null)
note "manifest after: ${AFTER:-<absent>} bytes"
REFUSED=$(kubectl -n "$NS" logs "$POD2" 2>/dev/null | grep -c 'import REFUSED' || true)
FENCED=$(kubectl -n "$NS" logs "$POD2" 2>/dev/null | grep -c 'publishing FENCED' || true)
note "log: import REFUSED x$REFUSED, publishing FENCED x$FENCED"

if [ "${REFUSED:-0}" -eq 0 ]; then
  bad "the hub did not refuse the import — the corruption did not take, so this leg proves nothing"
elif [ "$AFTER" = "$GARBAGE" ]; then
  pass "the corrupt manifest was left ALONE (${AFTER} bytes) — no barrier was published over it"
else
  bad "the manifest was REWRITTEN (${GARBAGE} -> ${AFTER} bytes) by a hub serving an empty export — this is the data-loss path"
fi

echo
echo "══════════════════════════════════════════════════════════════════"
echo " summary — $PASSES passed, ${#FAILURES[@]} failed   (expected: $EXPECT)"
echo "══════════════════════════════════════════════════════════════════"
echo " hub under test        : $HUB_IMAGE"
echo " F1 identities         : ${ID1:-?} / ${ID2:-?}"
echo " F2 manifest bytes     : before=${BEFORE:-?} corrupted=${GARBAGE:-?} after=${AFTER:-?}"
echo
# bash 3.2 (macOS) treats "${ARR[@]}" on an EMPTY array as unbound under
# `set -u`, so this line aborted the script BEFORE its `exit 0` and turned
# a clean 5-passed/0-failed run into FIXED_EXIT=1. The evidence was in the
# log and the exit code said the opposite — exactly the failure mode where
# the instrument reports on itself.
for f in ${FAILURES[@]+"${FAILURES[@]}"}; do echo "  ✗ $f"; done
if [ "$EXPECT" = fail ]; then
  if [ ${#FAILURES[@]} -eq 0 ]; then
    echo "CONTROL DID NOT REPRODUCE — the legs passed on the UNFIXED build, so they prove nothing."
    exit 1
  fi
  echo "CONTROL REPRODUCED (${#FAILURES[@]} leg(s) failed on the unfixed build) — the legs can detect the bugs."
  exit 0
fi
[ ${#FAILURES[@]} -eq 0 ] && { echo "ALL LEGS PASSED."; exit 0; }
exit 1
