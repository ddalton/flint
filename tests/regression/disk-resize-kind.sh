#!/usr/bin/env bash
# Shrinking a share's disk — the one resize Kubernetes cannot do.
#
# A PVC can grow and can never shrink. Before this, a smaller
# `persistence.size` was simply refused forever, so an over-provisioned
# project stayed over-provisioned — expensive on-prem, and unfixable
# without deleting the share. `persistence.reprovisionOnShrink` makes
# the operator rebuild the disk instead: prove the bucket can restore
# the tree, drop the claim, make a new smaller one, import.
#
# The drill runs the operator and an in-cluster MinIO, because the
# whole safety argument is "ask the hub whether the BUCKET is current"
# and a drill without a real bucket would be asserting nothing.
#
# Legs:
#   1  a tiered share on a 2Gi claim reaches Ready and publishes real
#      data to the bucket (the corpus that must survive).
#   2  ANTI-VACUITY: shrink with the flag OFF is still refused — the
#      claim keeps its size and the event says ShrinkRefused. Without
#      this leg every later assertion could pass on a build that just
#      ignored the flag and rebuilt everything unconditionally.
#   3  turn the flag on and shrink: the share goes through
#      Reprovisioning, the OLD PVC is genuinely destroyed (uid changes,
#      not just the request field), and the NEW claim is 1Gi.
#   4  the share returns to Ready and the DATA COMES BACK — every file
#      byte-identical, restored from the bucket. Anti-vacuity: the same
#      comparison against a deliberately corrupted expectation must
#      report a mismatch.
#   5  a share with NO bucket is refused even with the flag on — its
#      PVC is the only copy, so a "resize" would be a delete.
#
# KEEP=1 leaves the cluster standing.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OP_CHART="$REPO_ROOT/flint-lite-operator-chart"
CARGO_DIR="$REPO_ROOT/spdk-csi-driver"

CLUSTER="${CLUSTER:-flint-reprovision}"
NS=reprov
OPNS=flint-system
HUBIMG=flint-lite-dev:local
OPIMG=flint-lite-operator-dev:local
BUCKET=flint-reprov
PREFIX=proj1/
MINIO_USER=flintdrill
MINIO_PASS=flintdrill123
PF_PORT=39100
PF_PID=""
API_TOKEN=reprov-drill-token
KUBECONFIG_FILE="$(mktemp -t flint-reprov-kubeconfig.XXXXXX)"
export KUBECONFIG="$KUBECONFIG_FILE"

say()  { echo; echo "── $*"; }
pass() { echo "  ✓ $*"; }
fail() { echo "  ✗ FAIL: $*"; exit 1; }
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
    echo "KEEP=1 — cluster left standing (kubeconfig: $KUBECONFIG_FILE)"; return
  fi
  kind delete cluster --name "$CLUSTER" >/dev/null 2>&1
  rm -f "$KUBECONFIG_FILE"
}
trap cleanup EXIT

# The share's claim name is derived: <share>-data.
claim_uid()  { kubectl -n "$NS" get pvc "$1-data" -o jsonpath='{.metadata.uid}' 2>/dev/null; }
claim_size() { kubectl -n "$NS" get pvc "$1-data" -o jsonpath='{.spec.resources.requests.storage}' 2>/dev/null; }
phase_of()   { kubectl -n "$NS" get flintshare "$1" -o jsonpath='{.status.phase}' 2>/dev/null; }
events_of()  { kubectl -n "$NS" get events --field-selector "involvedObject.name=$1" -o jsonpath='{.items[*].reason}' 2>/dev/null; }

# Wait until `phase_of` equals $2, or fail after $3 seconds.
wait_phase() {
  local name=$1 want=$2 secs=${3:-180} seen
  for _ in $(seq 1 "$secs"); do
    seen=$(phase_of "$name")
    [ "$seen" = "$want" ] && { pass "$name reached $want"; return 0; }
    sleep 1
  done
  kubectl -n "$NS" get flintshare "$name" -o yaml | tail -35
  fail "$name never reached $want (last: '${seen:-<none>}')"
}

echo "══════════════════════════════════════════════════════════════════"
echo " disk resize — grow to fit, shrink by rebuild"
echo "══════════════════════════════════════════════════════════════════"

for t in kind kubectl helm docker aws curl; do
  command -v "$t" >/dev/null || fail "$t not installed"
done
docker info >/dev/null 2>&1 || fail "docker daemon not reachable"

DARCH=$(docker info --format '{{.Architecture}}')
case "$DARCH" in
  aarch64|arm64) TRIPLE=aarch64-unknown-linux-musl; PLATFORM=linux/arm64 ;;
  x86_64|amd64)  TRIPLE=x86_64-unknown-linux-musl;  PLATFORM=linux/amd64 ;;
  *) fail "unrecognized docker VM arch: $DARCH" ;;
esac

say "building the hub and the operator ($TRIPLE)"
(cd "$CARGO_DIR" && cargo zigbuild --release --target "$TRIPLE" \
   --bin flint-pnfs-mds --bin flint-lite-operator >/tmp/reprov-build.log 2>&1) \
  || { tail -20 /tmp/reprov-build.log; fail "zigbuild failed"; }
IMGDIR=$(mktemp -d -t flint-reprov-img.XXXXXX)
cp "$CARGO_DIR/target/$TRIPLE/release/flint-pnfs-mds" "$IMGDIR/"
cp "$CARGO_DIR/target/$TRIPLE/release/flint-lite-operator" "$IMGDIR/"
cat >"$IMGDIR/Dockerfile.hub" <<'EOF'
FROM alpine:3.20
RUN apk add --no-cache curl
COPY flint-pnfs-mds /usr/local/bin/flint-pnfs-mds
EOF
cat >"$IMGDIR/Dockerfile.op" <<'EOF'
FROM alpine:3.20
RUN apk add --no-cache ca-certificates
COPY flint-lite-operator /usr/local/bin/flint-lite-operator
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/flint-lite-operator"]
EOF
docker build --platform "$PLATFORM" -f "$IMGDIR/Dockerfile.hub" -t "$HUBIMG" "$IMGDIR" \
  >/tmp/reprov-img.log 2>&1 || { tail -5 /tmp/reprov-img.log; fail "hub image build failed"; }
docker build --platform "$PLATFORM" -f "$IMGDIR/Dockerfile.op" -t "$OPIMG" "$IMGDIR" \
  >>/tmp/reprov-img.log 2>&1 || { tail -5 /tmp/reprov-img.log; fail "operator image build failed"; }
rm -rf "$IMGDIR"
pass "images built ($PLATFORM)"

say "kind cluster + MinIO + bucket $BUCKET"
kind delete cluster --name "$CLUSTER" >/dev/null 2>&1
kind create cluster --name "$CLUSTER" --wait 120s >/dev/null 2>&1
kubectl cluster-info >/dev/null 2>&1 || fail "kind cluster never came up"
for i in "$HUBIMG" "$OPIMG"; do
  kind load docker-image "$i" --name "$CLUSTER" >/dev/null 2>&1 || fail "kind load $i failed"
done
# KIND'S DEFAULT StorageClass HAS EXPANSION OFF. Without this patch the
# API server REFUSES every PVC resize, the operator reports
# ExpansionRefused (correctly), and leg 6 watches a 1Gi claim sit at 1Gi
# under a 600 MiB project — a product that works, failing a drill that
# cannot see it work. Cost this drill two runs to find.
#
# What this does and does not prove: local-path accepts the larger
# REQUEST but never resizes the backing directory, so `.status.capacity`
# stays put. That is fine here — local-path enforces no size at all, and
# what leg 6 tests is the operator's decision and its apply. A real CSI
# expansion completing end to end needs a cloud cluster and is not
# claimed by this drill.
kubectl patch storageclass standard -p '{"allowVolumeExpansion":true}' >/dev/null 2>&1 \
  || fail "could not enable volume expansion on the default StorageClass"
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
---
apiVersion: v1
kind: Service
metadata: { name: minio }
spec:
  selector: { app: minio }
  ports: [{ port: 9000, targetPort: 9000 }]
EOF
kubectl -n "$NS" rollout status deployment/minio --timeout=180s >/dev/null 2>&1 \
  || fail "MinIO never became Ready"
# Keys VERBATIM: envFrom maps them straight into the SDK's environment,
# and any other spelling makes the hub fall back to IMDS and fail with
# "bucket unreachable: dispatch failure" — which names the bucket, not
# the cause.
kubectl -n "$NS" create secret generic s3creds \
  --from-literal=AWS_ACCESS_KEY_ID=$MINIO_USER \
  --from-literal=AWS_SECRET_ACCESS_KEY=$MINIO_PASS >/dev/null \
  || fail "secret create failed"
kubectl -n "$NS" create secret generic api-token \
  --from-literal=token="$API_TOKEN" >/dev/null || fail "token secret create failed"
kubectl -n "$NS" port-forward svc/minio "$PF_PORT:9000" >/dev/null 2>&1 &
PF_PID=$!
for _ in $(seq 1 20); do
  curl -sf "http://127.0.0.1:$PF_PORT/minio/health/live" >/dev/null && break
  sleep 1
done
s3 s3 mb "s3://$BUCKET" >/dev/null 2>&1 || fail "bucket create failed"
pass "cluster, MinIO and bucket $BUCKET up"

say "installing the operator"
helm install flint-lite-operator "$OP_CHART" -n "$OPNS" --create-namespace \
  --set image.ref="$OPIMG" --set hubImage="$HUBIMG" \
  >/tmp/reprov-helm.log 2>&1 || { tail -20 /tmp/reprov-helm.log; fail "helm install failed"; }
kubectl -n "$OPNS" rollout status deployment/flint-lite-operator --timeout=180s >/dev/null 2>&1 \
  || fail "operator never became Ready"
kubectl wait --for=condition=established --timeout=60s crd/flintshares.chert.us >/dev/null 2>&1 \
  || fail "the CRD never became Established"
STAMP=$(kubectl get crd flintshares.chert.us -o jsonpath='{.metadata.annotations.chert\.us/crd-schema-version}')
[ "$STAMP" = "6" ] || fail "CRD schema stamp is '$STAMP', expected 6 (reprovision + autoExpand + Terminating + conflictWith)"
pass "operator up, CRD stamped at schema $STAMP"

# ── a share, on a deliberately over-sized claim ──────────────────────
mk_share() {  # $1 name, $2 size, $3 reprovision(true/false), $4 bucket(yes/no)
  local extra=""
  [ "$4" = "yes" ] && extra="
  bucket: $BUCKET
  keyPrefix: $PREFIX
  endpoint: http://minio.$NS.svc:9000
  region: us-east-1
  credentialsSecretRef: s3creds
  settings:
    flushFloorSecs: 3
    epochHeartbeatSecs: 2
    epochLeaseMisses: 3"
  kubectl -n "$NS" apply -f - >/dev/null <<EOF || fail "FlintShare $1 refused"
apiVersion: chert.us/v1alpha1
kind: FlintShare
metadata: { name: $1 }
spec:$extra
  persistence:
    size: $2
    reprovisionOnShrink: $3
  monitoring:
    enabled: true
    fileApi:
      enabled: true
      tokenSecretRef: api-token
EOF
}

say "leg 1: a tiered share on a 2Gi claim, publishing to the bucket"
mk_share big 2Gi false yes
wait_phase big Ready 300
[ "$(claim_size big)" = "2Gi" ] || fail "claim is $(claim_size big), expected 2Gi"
UID_BEFORE=$(claim_uid big)
[ -n "$UID_BEFORE" ] || fail "no PVC uid"

# Write a corpus through the file API and record checksums. The file
# API is the hub's own door, so this needs no NFS mount in the drill.
POD=$(kubectl -n "$NS" get pod -l chert.us/share=big -o jsonpath='{.items[0].metadata.name}')
[ -n "$POD" ] || fail "no hub pod for share big"
IP=$(kubectl -n "$NS" get pod "$POD" -o jsonpath='{.status.podIP}')
SUM1=""; SUM2=""; SUM3=""
for i in 1 2 3; do
  # Distinct, reproducible content per file.
  body=$(head -c 2000 /dev/urandom | base64 | head -c 1500)
  echo "$body" > "/tmp/reprov-f$i"
  kubectl -n "$NS" exec -i "$POD" -- sh -c \
    "curl -sf -X PUT -H 'Authorization: Bearer $API_TOKEN' --data-binary @- \
     'http://127.0.0.1:8080/files/content?path=f$i.txt'" \
    < "/tmp/reprov-f$i" >/dev/null || fail "PUT f$i.txt failed"
  eval "SUM$i=\$(shasum -a 256 < /tmp/reprov-f$i | awk '{print \$1}')"
done
pass "3 files written through the file API"

# Force a barrier and confirm the bucket really holds the data — the
# whole rebuild rests on it.
for _ in $(seq 1 40); do
  n=$(s3 s3 ls "s3://$BUCKET/$PREFIX" --recursive 2>/dev/null | grep -c "f[123].txt")
  [ "${n:-0}" -ge 3 ] && break
  sleep 3
done
[ "${n:-0}" -ge 3 ] || fail "the bucket never received the 3 objects (saw ${n:-0})"
pass "bucket holds the corpus ($n objects) — a rebuild has something to restore from"

# ── leg 2: ANTI-VACUITY — off means off ──────────────────────────────
say "leg 2: shrink with reprovisionOnShrink OFF is still refused"
kubectl -n "$NS" patch flintshare big --type=merge \
  -p '{"spec":{"persistence":{"size":"1Gi"}}}' >/dev/null || fail "patch failed"
sleep 30
[ "$(claim_size big)" = "2Gi" ] \
  || fail "the claim changed to $(claim_size big) with the flag OFF — the opt-in does nothing"
[ "$(claim_uid big)" = "$UID_BEFORE" ] || fail "the PVC was replaced with the flag OFF"
case " $(events_of big) " in
  *" ShrinkRefused "*) pass "refused, and the event says ShrinkRefused" ;;
  *) fail "no ShrinkRefused event: $(events_of big)" ;;
esac

# ── leg 3: opt in — the disk is genuinely rebuilt ────────────────────
say "leg 3: turn reprovisionOnShrink on; the claim is destroyed and remade at 1Gi"
kubectl -n "$NS" patch flintshare big --type=merge \
  -p '{"spec":{"persistence":{"reprovisionOnShrink":true}}}' >/dev/null || fail "patch failed"

SAW_REPROV=no
for _ in $(seq 1 240); do
  [ "$(phase_of big)" = "Reprovisioning" ] && { SAW_REPROV=yes; break; }
  sleep 1
done
[ "$SAW_REPROV" = yes ] || fail "the share never entered Reprovisioning"
pass "phase went to Reprovisioning"

for _ in $(seq 1 300); do
  [ "$(claim_size big)" = "1Gi" ] && break
  sleep 1
done
[ "$(claim_size big)" = "1Gi" ] || fail "claim is $(claim_size big), expected 1Gi"
UID_AFTER=$(claim_uid big)
[ -n "$UID_AFTER" ] || fail "no PVC after the rebuild"
# The decisive assertion: a NEW object, not an edited field. If the
# operator had merely patched the request, Kubernetes would have
# rejected it and the uid would be unchanged.
[ "$UID_AFTER" != "$UID_BEFORE" ] \
  || fail "same PVC uid $UID_AFTER — the claim was never actually destroyed"
pass "PVC replaced: uid $UID_BEFORE → $UID_AFTER, size 2Gi → 1Gi"
case " $(events_of big) " in
  *" ReprovisionVerified "*) pass "the rebuild was verified against the bucket first" ;;
  *) fail "no ReprovisionVerified event — the disk went without the proof: $(events_of big)" ;;
esac

# ── leg 4: the data comes back ───────────────────────────────────────
say "leg 4: the share returns to Ready and every byte comes back"
wait_phase big Ready 420
POD=$(kubectl -n "$NS" get pod -l chert.us/share=big -o jsonpath='{.items[0].metadata.name}')
[ -n "$POD" ] || fail "no hub pod after the rebuild"
ok=0; bad=0
for i in 1 2 3; do
  got=$(kubectl -n "$NS" exec "$POD" -- sh -c \
     "curl -sf -H 'Authorization: Bearer $API_TOKEN' \
      'http://127.0.0.1:8080/files/content?path=f$i.txt'" | shasum -a 256 | awk '{print $1}')
  eval "want=\$SUM$i"
  if [ "$got" = "$want" ]; then ok=$((ok+1)); else bad=$((bad+1)); echo "    f$i.txt MISMATCH"; fi
done
[ "$bad" -eq 0 ] || fail "$bad of 3 files came back wrong after the rebuild"
pass "$ok/3 files byte-identical, restored from the bucket onto a brand-new disk"

# Anti-vacuity: the same comparison MUST be able to fail.
if [ "$(echo tampered | shasum -a 256 | awk '{print $1}')" = "$SUM1" ]; then
  fail "the checksum comparison cannot distinguish anything"
fi
pass "anti-vacuity: the comparison reports a mismatch against known-wrong content"

# ── leg 5: no bucket, no rebuild ─────────────────────────────────────
say "leg 5: a share with NO bucket is refused even with the flag on"
mk_share plain 2Gi true no
wait_phase plain Ready 240
PUID=$(claim_uid plain)
kubectl -n "$NS" patch flintshare plain --type=merge \
  -p '{"spec":{"persistence":{"size":"1Gi"}}}' >/dev/null || fail "patch failed"
sleep 30
[ "$(claim_size plain)" = "2Gi" ] \
  || fail "a tier-off share's disk was rebuilt — its PVC is the ONLY copy of the data"
[ "$(claim_uid plain)" = "$PUID" ] || fail "a tier-off share's PVC was replaced"
case " $(events_of plain) " in
  *" ShrinkRefused "*) pass "refused, with ShrinkRefused — there is nothing to rebuild from" ;;
  *) fail "no ShrinkRefused event on the tier-off share: $(events_of plain)" ;;
esac

# ── leg 6: auto-expand grows a deliberately under-sized claim ────────
say "leg 6: autoExpand grows a 1Gi claim to fit the project"
kubectl -n "$NS" apply -f - >/dev/null <<EOF || fail "FlintShare grow refused"
apiVersion: chert.us/v1alpha1
kind: FlintShare
metadata: { name: grow }
spec:
  bucket: $BUCKET
  keyPrefix: grow1/
  endpoint: http://minio.$NS.svc:9000
  region: us-east-1
  credentialsSecretRef: s3creds
  settings:
    flushFloorSecs: 3
    epochHeartbeatSecs: 2
    epochLeaseMisses: 3
  persistence:
    size: 1Gi
    autoExpand:
      enabled: true
      bufferPercent: 100
      maxSize: 8Gi
  monitoring:
    enabled: true
    fileApi:
      enabled: true
      tokenSecretRef: api-token
EOF
wait_phase grow Ready 300
[ "$(claim_size grow)" = "1Gi" ] || fail "grow started at $(claim_size grow), expected 1Gi"
# Ready is read off the Deployment, so it passes even when the operator
# cannot reach the hub at all — and auto-expand's ONLY input is that
# poll. Assert the poll works, or leg 6 is testing less than it looks.
POLLED=""
for _ in $(seq 1 60); do
  POLLED=$(kubectl -n "$NS" get flintshare grow \
    -o jsonpath='{range .status.conditions[?(@.type=="HubReachable")]}{.status}{end}' 2>/dev/null)
  [ "$POLLED" = "True" ] && break
  sleep 2
done
[ "$POLLED" = "True" ] || fail "HubReachable is '$POLLED' — the operator never polled, so auto-expand has no input"
pass "HubReachable=True — the operator is actually reading /status"
GUID=$(claim_uid grow)

# Put ~600 MiB in. With a 100% buffer that wants ~1.2Gi > 1Gi, so the
# operator must raise the target. Written through the file API in
# chunks so the hub's manifest reflects real object sizes.
GPOD=$(kubectl -n "$NS" get pod -l chert.us/share=grow -o jsonpath='{.items[0].metadata.name}')
[ -n "$GPOD" ] || fail "no hub pod for share grow"
for i in 1 2 3 4 5 6; do
  kubectl -n "$NS" exec "$GPOD" -- sh -c \
    "head -c 104857600 /dev/zero | curl -sf -X PUT -H 'Authorization: Bearer $API_TOKEN' \
     --data-binary @- 'http://127.0.0.1:8080/files/content?path=blob$i.bin'" >/dev/null \
    || fail "PUT blob$i.bin failed"
done
pass "600 MiB written"

# The gauges must be REACHABLE by the operator, not merely published.
# A wrong field path deserializes cleanly to None and disables
# auto-expand silently — which is exactly how this drill first failed.
# POLLED, not sampled once. Two things have to happen first and
# neither is instant: a manifest barrier must run (that is what tallies
# the inventory) and the tier reporter must collect (60s interval). A
# single immediate read finds nothing and says so — which is how this
# assertion first failed, on a hub that was working fine.
SEEN=""
for _ in $(seq 1 90); do
  SEEN=$(kubectl -n "$NS" exec "$GPOD" -- sh -c \
    "curl -sf http://127.0.0.1:8080/status" 2>/dev/null \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["tier"]["gauges"]["logicalBytes"])' 2>/dev/null)
  [ -n "$SEEN" ] && [ "$SEEN" -gt 0 ] 2>/dev/null && break
  sleep 3
done
[ -n "$SEEN" ] && [ "$SEEN" -gt 0 ] 2>/dev/null \
  || fail "the hub never published tier.gauges.logicalBytes ('$SEEN') — auto-expand has nothing to size against"
pass "hub reports logicalBytes=$SEEN"

GREW=no
for _ in $(seq 1 180); do
  cur=$(claim_size grow)
  [ "$cur" != "1Gi" ] && { GREW=yes; break; }
  sleep 2
done
[ "$GREW" = yes ] || fail "the claim never grew past 1Gi (still $(claim_size grow))"
pass "claim grew 1Gi → $(claim_size grow)"
# Growth is an EXPANSION, not a rebuild: same PVC object throughout.
[ "$(claim_uid grow)" = "$GUID" ] \
  || fail "the PVC was replaced — auto-expand must EXPAND, never rebuild"
pass "same PVC uid $GUID — expanded in place, no data movement"
# spec is the user's and must be untouched.
SPECSZ=$(kubectl -n "$NS" get flintshare grow -o jsonpath='{.spec.persistence.size}')
[ "$SPECSZ" = "1Gi" ] || fail "the operator wrote spec.persistence.size ($SPECSZ) — it must not"
pass "spec.persistence.size still 1Gi — the target rides an annotation"
case " $(events_of grow) " in
  *" AutoExpanding "*) pass "the growth was announced" ;;
  *) fail "no AutoExpanding event: $(events_of grow)" ;;
esac

# ── leg 7: the two features do not fight ─────────────────────────────
say "leg 7: a shrink that autoExpand would undo is refused, not looped"
# The size must genuinely CHANGE. spec.persistence.size is still 1Gi
# (auto-expand never writes spec), so re-setting it to 1Gi asks for
# nothing and no shrink is ever requested — which is how this leg first
# failed, reporting a missing event for a shrink that never happened.
# 1536Mi is a real edit, below the 8Gi ceiling, so the guard must fire.
kubectl -n "$NS" patch flintshare grow --type=merge \
  -p '{"spec":{"persistence":{"reprovisionOnShrink":true,"size":"1536Mi"}}}' >/dev/null \
  || fail "patch failed"
BEFORE_UID=$(claim_uid grow)
sleep 45
[ "$(claim_uid grow)" = "$BEFORE_UID" ] \
  || fail "the disk was rebuilt — autoExpand will simply grow it back, so this is an outage for nothing"
case " $(events_of grow) " in
  *" ShrinkRefused "*) pass "refused, with the reason — no rebuild/regrow loop" ;;
  *) fail "no ShrinkRefused event: $(events_of grow)" ;;
esac
# And the escape hatch works: bring the ceiling down to the size asked
# for, and the same shrink goes through.
kubectl -n "$NS" patch flintshare grow --type=merge \
  -p '{"spec":{"persistence":{"autoExpand":{"maxSize":"1536Mi"}}}}' >/dev/null || fail "patch failed"
REBUILT=no
for _ in $(seq 1 240); do
  [ "$(claim_uid grow)" != "$BEFORE_UID" ] && { REBUILT=yes; break; }
  sleep 1
done
[ "$REBUILT" = yes ] || fail "lowering maxSize did not release the shrink"
# The uid changing only proves the claim was replaced. Wait for the NEW
# one to exist and assert its size — read too early it is still being
# recreated and reports empty, which passes while proving nothing.
NEWSZ=""
for _ in $(seq 1 120); do
  NEWSZ=$(claim_size grow)
  [ -n "$NEWSZ" ] && break
  sleep 2
done
[ "$NEWSZ" = "1536Mi" ] \
  || fail "the rebuilt claim is '$NEWSZ', expected 1536Mi — the shrink did not land"
pass "ceiling lowered → the shrink went through (uid changed, claim now $NEWSZ)"

echo
echo "REPROVISION + AUTOEXPAND: all legs passed — the disk grew, shrank, and kept its bytes"
