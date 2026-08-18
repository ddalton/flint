#!/usr/bin/env bash
# Flint-lite L4 — the tier against REAL S3 from a REAL cluster.
#
# Everything below kind-e2e parity is already proven (MinIO loop, chart
# wiring, DR, hydration). What only this run can prove:
#   - the chart's hub on a real cluster publishing to REAL S3 over the
#     real network, credentials via the operator Secret (the non-EKS
#     posture every operator without IRSA runs);
#   - real-S3 latencies for the two operator-visible costs: publish lag
#     (RPO) and cold-read hydration, at 1 GiB;
#   - the consumer story in-cloud: recipe A (static PV, in-tree nfs:)
#     from docs/flint-lite.md, verbatim;
#   - DR-from-bucket where the bucket is actual S3.
#
# PREREQS (the runner provisions these FIRST — this script creates no
# cloud resources except the bucket and in-cluster objects):
#   KUBECONFIG   points at the cluster (trove download)
#   BUCKET       S3 bucket name to create/use (versioning enabled here)
#   REGION       default us-west-1
#   IMG          hub image ref pullable by the cluster (Docker Hub)
#   SC           StorageClass for the hub PVC (e.g. flint-spdk)
#   AWS creds    in the environment for the aws CLI (bucket admin), plus
#   HUB_KEY_ID / HUB_KEY_SECRET — the LONG-LIVED scoped key the hub pod
#     gets via the Secret (mint a bucket-scoped IAM user first; the
#     rolesanywhere session key expires in ~1h and would wedge the hub).
#
# Legs:
#   1  bucket (versioned) + Secret + helm install: hub Ready, epoch
#      claimed, control object in REAL S3.
#   2  consumer pod via recipe A (static PV → ClusterIP): battery
#      (md5/sqlite/git) + 16 MiB publish + 1 GiB publish, both TIMED
#      to object-visible (the measured RPO story).
#   3  hub pod restart under the consumer's mount: re-claim, reread.
#   4  DR: uninstall (PVC dies), reinstall same bucket/prefix: import,
#      then the 1 GiB reread TIMED (real-S3 hydration), byte-identical.
#   5  the reporter/meter readout: request counts + reporter lines
#      captured for the economics record.
#
# KEEP=1 leaves everything standing. Teardown of cluster + bucket is
# the RUNNER's job (documented in the run record), not this script's.
set -uo pipefail

: "${KUBECONFIG:?set KUBECONFIG to the target cluster kubeconfig}"
: "${BUCKET:?set BUCKET}"
: "${IMG:?set IMG (hub image ref)}"
: "${SC:?set SC (StorageClass for the hub PVC)}"
: "${HUB_KEY_ID:?set HUB_KEY_ID (long-lived scoped key for the hub Secret)}"
: "${HUB_KEY_SECRET:?set HUB_KEY_SECRET}"
# Optional: session credentials (e.g. rolesanywhere) instead of a
# long-lived key — the run must then FIT the session window; refresh =
# re-create the Secret + delete the hub pod (self-recognition restart).
HUB_SESSION_TOKEN="${HUB_SESSION_TOKEN:-}"
REGION="${REGION:-us-west-1}"
NS=flint-lite
PREFIX=vol1/
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CHART="$REPO_ROOT/flint-lite-chart"
RUNLOG=/tmp/lite-l4-run.log

say()  { echo; echo "── $*"; }
pass() { echo "  ✓ $*"; }
fail() { echo "  ✗ FAIL: $*"; exit 1; }
s3cli() { aws --region "$REGION" "$@"; }
# Consumer-pod exec shorthand.
cpod() { kubectl -n "$NS" exec l4-consumer -- sh -c "$1"; }

now_ms() { python3 -c 'import time; print(int(time.time()*1000))'; }

echo "══════════════════════════════════════════════════════════════════"
echo " flint-lite L4 — real S3 ($REGION), real cluster, Secret creds"
echo "══════════════════════════════════════════════════════════════════"

for t in kubectl helm aws python3; do
  command -v "$t" >/dev/null || fail "$t not installed"
done
kubectl get nodes >/dev/null || fail "cluster unreachable via KUBECONFIG"
kubectl get sc "$SC" >/dev/null || fail "StorageClass $SC not found"

# ── leg 1: bucket + Secret + install ─────────────────────────────────
say "leg 1: bucket $BUCKET (versioned) + Secret + helm install"
if ! s3cli s3api head-bucket --bucket "$BUCKET" 2>/dev/null; then
  s3cli s3api create-bucket --bucket "$BUCKET" \
    --create-bucket-configuration LocationConstraint="$REGION" >/dev/null \
    || fail "bucket create failed"
fi
s3cli s3api put-bucket-versioning --bucket "$BUCKET" \
  --versioning-configuration Status=Enabled || fail "versioning enable failed"
kubectl create namespace "$NS" >/dev/null 2>&1
kubectl -n "$NS" delete secret flint-tier-s3 >/dev/null 2>&1
SECRET_ARGS=(--from-literal=AWS_ACCESS_KEY_ID="$HUB_KEY_ID"
             --from-literal=AWS_SECRET_ACCESS_KEY="$HUB_KEY_SECRET")
[ -n "$HUB_SESSION_TOKEN" ] \
  && SECRET_ARGS+=(--from-literal=AWS_SESSION_TOKEN="$HUB_SESSION_TOKEN")
kubectl -n "$NS" create secret generic flint-tier-s3 "${SECRET_ARGS[@]}" >/dev/null \
  || fail "creds Secret refused"

helm_install() {
  helm install flint-lite "$CHART" --namespace "$NS" \
      --set image.ref="$IMG" \
    --set persistence.storageClassName="$SC" \
    --set persistence.size=20Gi \
    --set tier.enabled=true \
    --set tier.bucket="$BUCKET" \
    --set tier.keyPrefix="$PREFIX" \
    --set tier.region="$REGION" \
    --set tier.credentialsSecret=flint-tier-s3 \
    --set tier.settings.flushFloorSecs=5 \
    --set tier.settings.quiesceSecs=2 \
    --set tier.settings.tickSecs=3 \
    --set tier.settings.epochHeartbeatSecs=5 \
    --set tier.settings.epochLeaseMisses=3 \
    >>"$RUNLOG" 2>&1
}
helm_install || { tail -5 "$RUNLOG"; fail "helm install failed"; }
kubectl -n "$NS" rollout status deployment/flint-lite --timeout=300s >/dev/null 2>&1 \
  || { kubectl -n "$NS" describe pod -l app=flint-lite | tail -20; fail "hub never Ready"; }
kubectl -n "$NS" logs deployment/flint-lite >/tmp/lite-l4-hub1.log 2>&1
grep -q "epoch .* held" /tmp/lite-l4-hub1.log || fail "no epoch claim in the hub log"
s3cli s3api list-objects-v2 --bucket "$BUCKET" --prefix "${PREFIX}.flint/" \
  --query 'KeyCount' --output text | grep -qv '^0$' \
  || fail "no ${PREFIX}.flint/ control object in real S3"
pass "hub Ready on SC=$SC; epoch claimed; control object in s3://$BUCKET/${PREFIX}.flint/"

# ── leg 2: recipe-A consumer + timed publishes ───────────────────────
say "leg 2: static-PV consumer (recipe A) + timed 16MiB / 1GiB publishes"
HUB_IP=$(kubectl -n "$NS" get svc flint-lite -o jsonpath='{.spec.clusterIP}')
[ -n "$HUB_IP" ] || fail "no ClusterIP on the hub Service"
kubectl -n "$NS" apply -f - >/dev/null <<EOF || fail "consumer manifests refused"
apiVersion: v1
kind: PersistentVolume
metadata: { name: l4-lite-shared }
spec:
  capacity: { storage: 100Gi }
  accessModes: [ReadWriteMany]
  persistentVolumeReclaimPolicy: Retain
  mountOptions: [nfsvers=4.1, proto=tcp, hard]
  nfs: { server: "$HUB_IP", path: / }
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata: { name: l4-lite-shared, namespace: $NS }
spec:
  accessModes: [ReadWriteMany]
  storageClassName: ""
  volumeName: l4-lite-shared
  resources: { requests: { storage: 100Gi } }
---
apiVersion: v1
kind: Pod
metadata: { name: l4-consumer, namespace: $NS }
spec:
  containers:
    - name: c
      image: alpine:3.20
      command: ["sh", "-c", "apk add --no-cache git sqlite >/dev/null 2>&1; sleep 86400"]
      volumeMounts: [{ name: shared, mountPath: /mnt/flint }]
  volumes:
    - name: shared
      persistentVolumeClaim: { claimName: l4-lite-shared }
EOF
kubectl -n "$NS" wait --for=condition=Ready pod/l4-consumer --timeout=180s >/dev/null \
  || fail "consumer pod never Ready (recipe-A mount failed?)"

cpod "dd if=/dev/urandom of=/mnt/flint/shared.bin bs=1M count=16 status=none conv=fsync" \
  || fail "16MiB write failed"
MD5_W=$(cpod "md5sum /mnt/flint/shared.bin" | awk '{print $1}' | tr -d '\r')
T0=$(now_ms)
SZ=""
for _ in $(seq 1 60); do
  SZ=$(s3cli s3api head-object --bucket "$BUCKET" --key "${PREFIX}shared.bin" \
    --query 'ContentLength' --output text 2>/dev/null)
  [ "$SZ" = "16777216" ] && break
  sleep 2
done
[ "$SZ" = "16777216" ] || fail "16MiB object never reached S3 (last '$SZ')"
T16=$(( $(now_ms) - T0 ))
cpod "cd /mnt/flint && mkdir -p repo && cd repo && git init -q . && echo one >f && \
      git add f && git -c user.email=a@l4 -c user.name=a commit -qm l4 && \
      sqlite3 /mnt/flint/agents.db 'CREATE TABLE t(n); INSERT INTO t VALUES(1);'" \
  || fail "git/sqlite battery failed"
cpod "dd if=/dev/urandom of=/mnt/flint/big.bin bs=1M count=1024 status=none conv=fsync" \
  || fail "1GiB write failed"
MD5_BIG=$(cpod "md5sum /mnt/flint/big.bin" | awk '{print $1}' | tr -d '\r')
T0=$(now_ms)
SZ=""
for _ in $(seq 1 150); do
  SZ=$(s3cli s3api head-object --bucket "$BUCKET" --key "${PREFIX}big.bin" \
    --query 'ContentLength' --output text 2>/dev/null)
  [ "$SZ" = "1073741824" ] && break
  sleep 2
done
[ "$SZ" = "1073741824" ] || fail "1GiB object never reached S3 (last '$SZ')"
TBIG=$(( $(now_ms) - T0 ))
pass "publishes landed in real S3 — 16MiB in ${T16}ms, 1GiB in ${TBIG}ms after write-complete (floor 5s inside both)"

# ── leg 3: hub restart under the consumer's mount ────────────────────
say "leg 3: hub pod restart under the live in-cluster mount"
kubectl -n "$NS" delete pod -l app=flint-lite --wait=false >/dev/null 2>&1
kubectl -n "$NS" rollout status deployment/flint-lite --timeout=300s >/dev/null 2>&1 \
  || fail "hub never came back"
kubectl -n "$NS" logs deployment/flint-lite >/tmp/lite-l4-hub2.log 2>&1
grep -q "epoch .* held" /tmp/lite-l4-hub2.log || fail "restarted hub never re-claimed"
MD5_R=$(cpod "timeout 120 md5sum /mnt/flint/shared.bin" | awk '{print $1}' | tr -d '\r')
[ "$MD5_R" = "$MD5_W" ] || fail "post-restart read '$MD5_R' != '$MD5_W'"
pass "re-claim + byte-identical reread through the restart"

# ── leg 4: DR from real S3 (+ timed 1GiB hydration) ──────────────────
say "leg 4: uninstall (PVC dies) → reinstall same bucket → import → timed hydrate"
kubectl -n "$NS" delete pod l4-consumer --wait=true >/dev/null 2>&1
helm uninstall flint-lite -n "$NS" >/dev/null 2>&1 || fail "helm uninstall failed"
for _ in $(seq 1 90); do
  kubectl -n "$NS" get pvc flint-lite-data >/dev/null 2>&1 || break
  sleep 2
done
kubectl -n "$NS" get pvc flint-lite-data >/dev/null 2>&1 \
  && fail "chart PVC survived uninstall"
helm_install || { tail -5 "$RUNLOG"; fail "DR reinstall failed"; }
kubectl -n "$NS" rollout status deployment/flint-lite --timeout=600s >/dev/null 2>&1 \
  || { kubectl -n "$NS" describe pod -l app=flint-lite | tail -20; fail "DR hub never Ready"; }
kubectl -n "$NS" logs deployment/flint-lite >/tmp/lite-l4-hub3.log 2>&1
grep -q "restoring from the bucket" /tmp/lite-l4-hub3.log || fail "no import-on-start"
# New hub Service = (likely) new ClusterIP: rebuild PV + consumer.
# PVC FIRST — pv-protection blocks PV finalization while the bound PVC
# exists, and kubectl delete waits by default, so pv-before-pvc wedges
# this script forever (run 3 hung here until the PVC died externally).
kubectl -n "$NS" delete pvc l4-lite-shared --timeout=60s >/dev/null 2>&1 || true
kubectl delete pv l4-lite-shared --timeout=60s >/dev/null 2>&1 || true
HUB_IP=$(kubectl -n "$NS" get svc flint-lite -o jsonpath='{.spec.clusterIP}')
kubectl -n "$NS" apply -f - >/dev/null <<EOF || fail "post-DR consumer manifests refused"
apiVersion: v1
kind: PersistentVolume
metadata: { name: l4-lite-shared }
spec:
  capacity: { storage: 100Gi }
  accessModes: [ReadWriteMany]
  persistentVolumeReclaimPolicy: Retain
  mountOptions: [nfsvers=4.1, proto=tcp, hard]
  nfs: { server: "$HUB_IP", path: / }
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata: { name: l4-lite-shared, namespace: $NS }
spec:
  accessModes: [ReadWriteMany]
  storageClassName: ""
  volumeName: l4-lite-shared
  resources: { requests: { storage: 100Gi } }
---
apiVersion: v1
kind: Pod
metadata: { name: l4-consumer, namespace: $NS }
spec:
  containers:
    - name: c
      image: alpine:3.20
      command: ["sh", "-c", "sleep 86400"]
      volumeMounts: [{ name: shared, mountPath: /mnt/flint }]
  volumes:
    - name: shared
      persistentVolumeClaim: { claimName: l4-lite-shared }
EOF
kubectl -n "$NS" wait --for=condition=Ready pod/l4-consumer --timeout=180s >/dev/null \
  || fail "post-DR consumer never Ready"
T0=$(now_ms)
MD5_DR=$(cpod "timeout 600 md5sum /mnt/flint/big.bin" | awk '{print $1}' | tr -d '\r')
THYD=$(( $(now_ms) - T0 ))
[ "$MD5_DR" = "$MD5_BIG" ] || fail "post-DR 1GiB read '$MD5_DR' != '$MD5_BIG'"
MD5_S=$(cpod "timeout 120 md5sum /mnt/flint/shared.bin" | awk '{print $1}' | tr -d '\r')
[ "$MD5_S" = "$MD5_W" ] || fail "post-DR 16MiB read mismatch"
cpod "test -f /mnt/flint/repo/f && test -f /mnt/flint/agents.db" \
  || fail "repo/db files missing after DR"
pass "DR from real S3: import + hydrate — 1 GiB cold read in ${THYD}ms, byte-identical"

# ── leg 5: the record ────────────────────────────────────────────────
say "leg 5: reporter/meter readout (the economics record)"
kubectl -n "$NS" logs deployment/flint-lite | grep "🪣" | tail -10
echo
echo "  timings: 16MiB publish ${T16}ms · 1GiB publish ${TBIG}ms · 1GiB hydrate ${THYD}ms"
echo
echo "══════════════════════════════════════════════════════════════════"
echo " PASS — real S3, real cluster, recipe-A consumers, DR + hydration."
echo " Teardown (cluster + bucket) is the runner's step — see run record."
echo "══════════════════════════════════════════════════════════════════"
