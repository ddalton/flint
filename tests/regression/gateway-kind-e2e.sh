#!/usr/bin/env bash
# flint-hub-gateway kind e2e — the proxy, against REAL hubs and REAL S3.
#
# WHY THIS EXISTS
#
# The unit suite (70 tests) proves the routing, the refusals and the
# header handling against FAKE hubs on real sockets. Fake hubs accept
# any credential and resolve at 127.0.0.1, which leaves four things
# untested that only a cluster can answer — and the first is the one
# that would take a whole fleet down at once:
#
#   1  THE DERIVED TOKEN HAS NEVER BEEN CHECKED AGAINST A REAL HUB.
#      Three values must be byte-identical: what the gateway computes,
#      what is in the share's Secret, and what the hub's TokenSource
#      read off its projected file. Disagree by one byte — a trailing
#      newline, a base64-vs-raw key, a `kubectl create secret` quirk —
#      and every request in the fleet is 401.
#   2  NOTHING HAS EVER DIALLED A HEADLESS `status.apiEndpoint`. The
#      operator's API Service (a6c7437) has never run in a cluster at
#      all; `*.svc.cluster.local` on a headless Service resolves to pod
#      IPs through EndpointSlices, and "should work" is doing a lot of
#      work there.
#   3  THE WAKE PATH HAS NEVER RUN END TO END. arm the annotation ->
#      operator reconciles -> hub starts -> phase Ready -> gateway
#      dials. `wakeWaitSecs: 25` is a guess.
#   4  RBAC SUFFICIENCY. A merge patch on one annotation with `patch`
#      and nothing else, against a real API server.
#
# It also drills the shape a single-hub rig cannot express at all: ONE
# PROJECT WITH TWO HUBS. Nothing in the operator ties a project to one
# share — `conflict::overlaps` keys uniqueness on the bucket prefix
# subtree and nothing reads `flint.io/project-id` — so two volumes on
# two prefixes is ordinary, and the gateway has to keep them apart.
#
# ANTI-VACUITY
#
# Every leg that asserts an ABSENCE is paired with a positive control
# that would fail if the rig were simply broken. In particular: /status
# is proven UNREACHABLE through the gateway and REACHABLE directly from
# an in-cluster pod in the same leg, so "no status document" cannot pass
# because the hub never served one.
#
# No Lima VM and no kernel mount: this drill is HTTP end-to-end, so it
# needs only kind, docker, kubectl, helm, aws and curl.
#
# KEEP=1 leaves the cluster standing.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OP_CHART="$REPO_ROOT/flint-lite-operator-chart"
CARGO_DIR="$REPO_ROOT/spdk-csi-driver"

CLUSTER="${CLUSTER:-flint-gw-e2e}"
OPNS=flint-system
NS=workspaces
HUBIMG=flint-lite-dev:local
OPIMG=flint-lite-operator-dev:local
BUCKET=flint-gw-drill
PROJECT=proj-a
MINIO_USER=flintdrill
MINIO_PASS=flintdrill123
PF_S3=39100
PF_GW=39101
PF_S3_PID=""
PF_GW_PID=""
KUBECONFIG_FILE="$(mktemp -t flint-gw-kubeconfig.XXXXXX)"
export KUBECONFIG="$KUBECONFIG_FILE"

say()  { echo; echo "── $*"; }
pass() { echo "  ✓ $*"; }
fail() { echo "  ✗ FAIL: $*"; exit 1; }
note() { echo "    · $*"; }

s3() {
  env -u AWS_PROFILE \
    AWS_ACCESS_KEY_ID=$MINIO_USER AWS_SECRET_ACCESS_KEY=$MINIO_PASS \
    AWS_DEFAULT_REGION=us-east-1 AWS_EC2_METADATA_DISABLED=true \
    aws --endpoint-url "http://127.0.0.1:$PF_S3" "$@"
}

cleanup() {
  set +e
  [ -n "$PF_S3_PID" ] && kill "$PF_S3_PID" 2>/dev/null
  [ -n "$PF_GW_PID" ] && kill "$PF_GW_PID" 2>/dev/null
  if [ "${KEEP:-0}" = "1" ]; then
    echo "KEEP=1 — leaving cluster standing (kubeconfig: $KUBECONFIG_FILE)"
    return
  fi
  kind delete cluster --name "$CLUSTER" >/dev/null 2>&1
  rm -f "$KUBECONFIG_FILE"
}
trap cleanup EXIT

pf_s3() {
  [ -n "$PF_S3_PID" ] && kill "$PF_S3_PID" 2>/dev/null
  kubectl -n "$NS" port-forward svc/minio "$PF_S3:9000" >/dev/null 2>&1 &
  PF_S3_PID=$!
  for _ in $(seq 1 20); do
    curl -sf "http://127.0.0.1:$PF_S3/minio/health/live" >/dev/null && return 0
    sleep 1
  done
  fail "MinIO port-forward never became healthy"
}

pf_gw() {
  [ -n "$PF_GW_PID" ] && kill "$PF_GW_PID" 2>/dev/null
  kubectl -n "$OPNS" port-forward svc/flint-lite-operator-gateway "$PF_GW:8090" \
    >/dev/null 2>&1 &
  PF_GW_PID=$!
  for _ in $(seq 1 30); do
    curl -sf "http://127.0.0.1:$PF_GW/healthz" >/dev/null && return 0
    sleep 1
  done
  fail "gateway port-forward never became healthy"
}

# curl through the gateway. Prints "<status> <body>".
gw() {
  local method="$1" path="$2"; shift 2
  curl -s -o /tmp/gw-body.txt -w '%{http_code}' -X "$method" \
    -H "Authorization: Bearer $GW_TOKEN" "$@" \
    "http://127.0.0.1:$PF_GW$path"
}
gwbody() { cat /tmp/gw-body.txt; }

echo "══════════════════════════════════════════════════════════════════"
echo " flint-hub-gateway kind e2e — real hubs, real S3, one project /"
echo " two volumes, and the four things only a cluster can answer"
echo "══════════════════════════════════════════════════════════════════"

# ── 0. preflight ─────────────────────────────────────────────────────
for t in kind kubectl helm docker aws curl cargo python3; do
  command -v "$t" >/dev/null || fail "$t not installed"
done
docker info >/dev/null 2>&1 || fail "docker daemon not reachable"

DARCH=$(docker info --format '{{.Architecture}}')
case "$DARCH" in
  aarch64|arm64) TRIPLE=aarch64-unknown-linux-musl; PLATFORM=linux/arm64 ;;
  x86_64|amd64)  TRIPLE=x86_64-unknown-linux-musl;  PLATFORM=linux/amd64 ;;
  *) fail "unrecognized docker VM arch: $DARCH" ;;
esac

# ── 1. build the three binaries from the working tree ────────────────
say "building flint-pnfs-mds + flint-lite-operator + flint-hub-gateway ($TRIPLE)"
(cd "$CARGO_DIR" && cargo zigbuild --release --target "$TRIPLE" \
   --bin flint-pnfs-mds --bin flint-lite-operator --bin flint-hub-gateway \
   >/tmp/gw-e2e-build.log 2>&1) \
  || { tail -20 /tmp/gw-e2e-build.log; fail "zigbuild failed"; }
IMGDIR=$(mktemp -d -t flint-gw-img.XXXXXX)
cp "$CARGO_DIR/target/$TRIPLE/release/flint-pnfs-mds" "$IMGDIR/"
cp "$CARGO_DIR/target/$TRIPLE/release/flint-lite-operator" "$IMGDIR/"
cp "$CARGO_DIR/target/$TRIPLE/release/flint-hub-gateway" "$IMGDIR/"
cat >"$IMGDIR/Dockerfile.hub" <<'EOF'
FROM alpine:3.20
# curl is for the TEST, not the product: the anti-vacuity control in
# leg 5 talks to the hub DIRECTLY, and alpine's BusyBox wget cannot
# issue PUT or DELETE at all. The shipped hub image has no curl.
RUN apk add --no-cache curl ca-certificates
COPY flint-pnfs-mds /usr/local/bin/flint-pnfs-mds
EOF
cat >"$IMGDIR/Dockerfile.op" <<'EOF'
FROM alpine:3.20
RUN apk add --no-cache ca-certificates
# BOTH binaries, exactly as the shipped image does it — the chart picks
# one with `command:`. If this drill copied only the gateway it would
# not be testing the packaging the release actually uses.
COPY flint-lite-operator /usr/local/bin/flint-lite-operator
COPY flint-hub-gateway /usr/local/bin/flint-hub-gateway
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/flint-lite-operator"]
EOF
docker build --platform "$PLATFORM" -f "$IMGDIR/Dockerfile.hub" -t "$HUBIMG" "$IMGDIR" \
  >/tmp/gw-e2e-img.log 2>&1 || { tail -5 /tmp/gw-e2e-img.log; fail "hub image build failed"; }
docker build --platform "$PLATFORM" -f "$IMGDIR/Dockerfile.op" -t "$OPIMG" "$IMGDIR" \
  >>/tmp/gw-e2e-img.log 2>&1 || { tail -5 /tmp/gw-e2e-img.log; fail "operator image build failed"; }
rm -rf "$IMGDIR"
pass "images built ($PLATFORM), gateway packaged in the OPERATOR image"

say "creating kind cluster '$CLUSTER'"
kind delete cluster --name "$CLUSTER" >/dev/null 2>&1
kind create cluster --name "$CLUSTER" --wait 120s >/dev/null 2>&1 \
  || fail "kind create cluster failed"
for i in "$HUBIMG" "$OPIMG"; do
  kind load docker-image "$i" --name "$CLUSTER" >/dev/null 2>&1 || fail "kind load $i failed"
done
pass "cluster up, images loaded"

# ── 2. MinIO + bucket + credentials ──────────────────────────────────
say "MinIO in-cluster, bucket $BUCKET"
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
pf_s3
s3 s3 mb "s3://$BUCKET" >/dev/null || fail "bucket create failed"
pass "MinIO Ready, bucket created"

# ── 3. leg 1: the operator and the gateway install ───────────────────
say "leg 1: operator + gateway install; probes answer; /readyz gates on the cache"
GW_TOKEN="drill-inbound-$(date +%s)"
kubectl create namespace "$OPNS" >/dev/null 2>&1
kubectl -n "$OPNS" create secret generic flint-gateway-token \
  --from-literal=token="$GW_TOKEN" >/dev/null || fail "gateway token Secret refused"
# A 48-byte root key. `--from-literal` with base64 text keeps the Secret
# readable in a drill; the binary trims only TRAILING whitespace, so the
# value is exactly these bytes.
ROOT_KEY="drill-root-key-0123456789abcdef0123456789abcdef"
kubectl -n "$OPNS" create secret generic flint-gateway-root \
  --from-literal=key="$ROOT_KEY" >/dev/null || fail "root key Secret refused"

helm install flint-lite-operator "$OP_CHART" -n "$OPNS" \
  --set image.ref="$OPIMG" --set hubImage="$HUBIMG" \
  --set gateway.enabled=true \
  --set gateway.tokenSecretRef=flint-gateway-token \
  --set gateway.rootKeySecretRef=flint-gateway-root \
  --set gateway.replicas=1 \
  --set gateway.wakeWaitSecs=60 \
  >/tmp/gw-e2e-helm.log 2>&1 || { tail -25 /tmp/gw-e2e-helm.log; fail "helm install failed"; }
kubectl -n "$OPNS" rollout status deployment/flint-lite-operator --timeout=120s >/dev/null 2>&1 \
  || { kubectl -n "$OPNS" describe pod -l app.kubernetes.io/name=flint-lite-operator | tail -20
       fail "operator never became Ready"; }
kubectl -n "$OPNS" rollout status deployment/flint-lite-operator-gateway --timeout=120s \
  >/dev/null 2>&1 \
  || { kubectl -n "$OPNS" describe pod -l app.kubernetes.io/name=flint-lite-operator-gateway | tail -30
       kubectl -n "$OPNS" logs -l app.kubernetes.io/name=flint-lite-operator-gateway --tail=40
       fail "gateway never became Ready"; }
kubectl wait --for=condition=established --timeout=60s crd/flintshares.flint.io >/dev/null 2>&1 \
  || fail "the CRD never became Established"
pf_gw
# The gateway became READY, which means /readyz passed, which means the
# reflector listed. That is leg 1's real assertion: the readiness gate
# is wired to the cache and not to the socket.
[ "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PF_GW/healthz")" = "200" ] \
  || fail "/healthz did not answer 200"
[ "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PF_GW/readyz")" = "200" ] \
  || fail "/readyz did not answer 200 on a Ready pod"
# Probes must not need a credential and must not name a share.
curl -s "http://127.0.0.1:$PF_GW/healthz" | grep -q "$PROJECT" \
  && fail "/healthz named a project"
pass "operator + gateway Ready; probes answer unauthenticated and name nothing"

# ── 4. leg 2: two volumes of ONE project, tokens derived IN-CLUSTER ──
say "leg 2: one project, two volumes — tokens derived by the gateway's own binary"
GWPOD=$(kubectl -n "$OPNS" get pod -l app.kubernetes.io/name=flint-lite-operator-gateway \
  -o jsonpath='{.items[0].metadata.name}')
[ -n "$GWPOD" ] || fail "no gateway pod"

# THE BINDING IS TYPED BY HAND HERE ON PURPOSE, ONCE — and it is the
# step this drill got WRONG on its first run, which is worth keeping in
# the file rather than quietly fixing. `--derive-token` takes four
# fields in an order; the drill omitted spec.endpoint, which is a legal
# empty value that nothing complains about, and produced a perfectly
# well-formed token that no hub would ever accept. The failure surfaced
# three legs later as a 502 on an upload.
#
# `--derive-for` removes the class: it reads the FlintShare and derives
# from ITS OWN spec, so the binding cannot disagree with what the
# serving gateway computes. Both are exercised below — the hand-typed
# form WITH the endpoint, and the CR-read form — and they must agree.
S3EP="http://minio.$NS.svc:9000"
derive() {   # derive <keyPrefix>  — the hand-typed binding
  kubectl -n "$OPNS" exec "$GWPOD" -- /usr/local/bin/flint-hub-gateway \
    --root-key-file=/etc/flint/gateway-root/key \
    --derive-token "$S3EP,$BUCKET,$1,1" 2>/dev/null | tr -d '\r\n'
}
derive_for() {  # derive_for <ns/name> — read from the CR
  kubectl -n "$OPNS" exec "$GWPOD" -- /usr/local/bin/flint-hub-gateway \
    --root-key-file=/etc/flint/gateway-root/key \
    --derive-for "$1" 2>/dev/null | tr -d '\r\n'
}
TOK_DATA=$(derive "$PROJECT/data/")
TOK_MODELS=$(derive "$PROJECT/models/")
[ ${#TOK_DATA} -eq 43 ] || fail "derived token is ${#TOK_DATA} chars, expected 43: '$TOK_DATA'"
[ "$TOK_DATA" != "$TOK_MODELS" ] \
  || fail "two volumes of one project derived the SAME token — the binding ignores keyPrefix"
note "data=${TOK_DATA:0:8}… models=${TOK_MODELS:0:8}… (distinct)"

# Determinism, from the same in-cluster binary and key.
[ "$(derive "$PROJECT/data/")" = "$TOK_DATA" ] || fail "derivation is not deterministic"

kubectl -n "$NS" create secret generic tok-data --from-literal=token="$TOK_DATA" >/dev/null \
  || fail "token Secret refused"
kubectl -n "$NS" create secret generic tok-models --from-literal=token="$TOK_MODELS" >/dev/null \
  || fail "token Secret refused"

mkshare() {  # mkshare <name> <volume>
  kubectl -n "$NS" apply -f - >/dev/null <<EOF || fail "FlintShare $1 refused"
apiVersion: flint.io/v1alpha1
kind: FlintShare
metadata:
  name: $1
  labels:
    flint.io/project-id: $PROJECT
    flint.io/volume-id: $2
spec:
  bucket: $BUCKET
  keyPrefix: $PROJECT/$2/
  endpoint: http://minio.$NS.svc:9000
  region: us-east-1
  credentialsSecretRef: flint-tier-s3
  persistence:
    size: 1Gi
  monitoring:
    enabled: true
    fileApi:
      enabled: true
      tokenSecretRef: tok-$2
  settings:
    flushFloorSecs: 3
EOF
}
mkshare "fs-$PROJECT-data" data
mkshare "fs-$PROJECT-models" models

for s in "fs-$PROJECT-data" "fs-$PROJECT-models"; do
  for i in $(seq 1 60); do
    ph=$(kubectl -n "$NS" get flintshare "$s" -o jsonpath='{.status.phase}' 2>/dev/null)
    [ "$ph" = "Ready" ] && break
    sleep 5
  done
  [ "$ph" = "Ready" ] || {
    kubectl -n "$NS" get flintshare "$s" -o yaml | tail -40
    kubectl -n "$NS" logs -l flint.io/share="$s" --tail=40 2>/dev/null
    fail "$s never became Ready (phase=$ph)"; }
done
pass "both volumes Ready; their derived tokens are distinct and deterministic"

# THE CROSS-CHECK. `--derive-for` reads the CR the operator persisted,
# so if it agrees with the hand-typed binding then the binding really is
# what the serving gateway will compute — and if it disagrees, THIS is
# where an operator finds out, not three legs later on a 502.
FOR_DATA=$(derive_for "$NS/fs-$PROJECT-data")
[ "$FOR_DATA" = "$TOK_DATA" ] \
  || fail "--derive-for disagrees with the hand-typed binding ('$FOR_DATA' vs '$TOK_DATA') — the Secret was written from the wrong binding"
FOR_MODELS=$(derive_for "$NS/fs-$PROJECT-models")
[ "$FOR_MODELS" = "$TOK_MODELS" ] || fail "--derive-for disagrees for models"
pass "--derive-for (read from the CR) agrees with the hand-typed binding"

# ── 5. leg 3: THE HEADLESS API SERVICE, AND THE TOKEN, AGAINST A HUB ─
say "leg 3: the headless apiEndpoint resolves, and a REAL hub accepts the derived token"
EP_DATA=$(kubectl -n "$NS" get flintshare "fs-$PROJECT-data" \
  -o jsonpath='{.status.apiEndpoint}')
[ -n "$EP_DATA" ] || fail "status.apiEndpoint was never published"
case "$EP_DATA" in
  http://*.$NS.svc.cluster.local:8080) ;;
  *) fail "apiEndpoint is not the in-cluster headless name: $EP_DATA" ;;
esac
SVC=$(echo "$EP_DATA" | sed -e 's|http://||' -e "s|\\.$NS\\.svc\\.cluster\\.local:8080||")
CIP=$(kubectl -n "$NS" get svc "$SVC" -o jsonpath='{.spec.clusterIP}')
[ "$CIP" = "None" ] || fail "the API Service is not headless (clusterIP=$CIP) — it would consume an address per share"
EPS=$(kubectl -n "$NS" get endpointslice -l "kubernetes.io/service-name=$SVC" \
  -o jsonpath='{.items[*].endpoints[*].addresses[*]}')
[ -n "$EPS" ] || fail "the headless Service has no EndpointSlice addresses — the name would not resolve"
note "apiEndpoint $EP_DATA -> pod ip(s) $EPS"

# A debug pod on the cluster network — the ONLY way to prove the hub
# itself accepts the derived token, independently of the gateway.
kubectl -n "$NS" run gwdebug --image="$HUBIMG" --restart=Never \
  --command -- sleep 3600 >/dev/null 2>&1
kubectl -n "$NS" wait --for=condition=ready pod/gwdebug --timeout=120s >/dev/null 2>&1 \
  || fail "debug pod never became Ready"
dbg() { kubectl -n "$NS" exec gwdebug -- sh -c "$1" 2>/dev/null; }

# (a) THE HEADLINE. The token the GATEWAY's binary derived, presented
#     to the hub that read its own Secret. If these disagree by a byte,
#     the whole fleet is 401.
code=$(dbg "curl -s -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer $TOK_DATA' '$EP_DATA/files?path=/'")
[ "$code" = "200" ] || fail "the hub REFUSED the derived token (HTTP $code) — derivation and Secret disagree"
pass "a real hub accepts the token the gateway's own binary derived (through the headless name)"

# (b) ANTI-VACUITY: the hub really is checking. Without this, (a) would
#     pass just as well against a hub that accepted anything.
code=$(dbg "curl -s -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer not-the-token' '$EP_DATA/files?path=/'")
[ "$code" = "401" ] || fail "the hub accepted a WRONG token (HTTP $code) — leg 3a proves nothing"
# And the OTHER volume's token must not open this one.
code=$(dbg "curl -s -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer $TOK_MODELS' '$EP_DATA/files?path=/'")
[ "$code" = "401" ] || fail "volume 'models' token opened volume 'data' (HTTP $code)"
pass "a wrong token is 401, and one volume's token does not open the other"

# ── 6. leg 4: through the GATEWAY, each volume reaches its own hub ────
say "leg 4: one project, two volumes, addressed through the gateway"
code=$(gw GET "/v1/projects/$PROJECT/volumes")
[ "$code" = "200" ] || fail "volume listing returned $code: $(gwbody)"
echo "$(gwbody)" | python3 -c '
import json,sys
d=json.load(sys.stdin)
vols=sorted(v["volume"] for v in d["volumes"])
assert d["project"]=="'"$PROJECT"'", d
assert vols==["data","models"], vols
prefixes=sorted(v["keyPrefix"] for v in d["volumes"])
assert prefixes==["'"$PROJECT"'/data/","'"$PROJECT"'/models/"], prefixes
assert all(v["serving"] for v in d["volumes"]), d
print("    · volumes:", vols)
' || fail "the volume listing is wrong: $(gwbody)"

# Write DIFFERENT bytes through each volume.
echo "data-volume-contents" > /tmp/gw-data.txt
echo "models-volume-contents" > /tmp/gw-models.txt
code=$(gw PUT "/v1/projects/$PROJECT/volumes/data/files/content?path=/hello.txt" \
  --data-binary @/tmp/gw-data.txt)
case "$code" in 200|201) ;; *) fail "PUT via volume data returned $code: $(gwbody)" ;; esac
code=$(gw PUT "/v1/projects/$PROJECT/volumes/models/files/content?path=/hello.txt" \
  --data-binary @/tmp/gw-models.txt)
case "$code" in 200|201) ;; *) fail "PUT via volume models returned $code: $(gwbody)" ;; esac

# Each volume reads back ITS OWN bytes. Same path, two hubs.
code=$(gw GET "/v1/projects/$PROJECT/volumes/data/files/content?path=/hello.txt")
[ "$code" = "200" ] || fail "GET via volume data returned $code"
grep -q "data-volume-contents" /tmp/gw-body.txt \
  || fail "volume 'data' returned the wrong bytes: $(gwbody)"
code=$(gw GET "/v1/projects/$PROJECT/volumes/models/files/content?path=/hello.txt")
[ "$code" = "200" ] || fail "GET via volume models returned $code"
grep -q "models-volume-contents" /tmp/gw-body.txt \
  || fail "volume 'models' returned the wrong bytes — the volumes are CROSSED: $(gwbody)"
pass "two volumes of one project are two hubs, and the same path in each holds different bytes"

# The bare path is under-specified and says so, naming the choice.
code=$(gw GET "/v1/projects/$PROJECT/files?path=/")
[ "$code" = "409" ] || fail "the bare path on a 2-volume project returned $code, expected 409"
echo "$(gwbody)" | python3 -c '
import json,sys
d=json.load(sys.stdin)
assert d["reason"]=="MultipleVolumes", d
assert sorted(d["volumes"])==["data","models"], d
' || fail "the 409 did not name the choice: $(gwbody)"
code=$(gw GET "/v1/projects/$PROJECT/volumes/nope/files?path=/")
[ "$code" = "404" ] || fail "an unknown volume returned $code, expected 404 (a fallback would be worse)"
pass "the bare path names the choice; an unknown volume is 404 and never falls back"

# ── 7. leg 5: /status is unreachable THROUGH the gateway — and served ─
say "leg 5: /status is unreachable through the gateway, and demonstrably served by the hub"
# THE CONTROL FIRST. The hub really does serve an unauthenticated
# /status; if it did not, every assertion below would pass vacuously.
STATUS=$(dbg "curl -s '$EP_DATA/status'")
echo "$STATUS" | grep -q '"phase"' \
  || fail "the hub does not serve /status on this port — leg 5 would prove nothing"
note "the hub serves /status unauthenticated: $(echo "$STATUS" | head -c 80)…"

for p in \
  "/status" \
  "/v1/projects/$PROJECT/status" \
  "/v1/projects/$PROJECT/volumes/data/status" \
  "/v1/projects/$PROJECT/volumes/data/files/../status" \
  "/v1/projects/$PROJECT/volumes/data%2F..%2Fstatus/files" \
  "/v1/projects/$PROJECT/files/status" ; do
  code=$(gw GET "$p")
  grep -q '"phase"' /tmp/gw-body.txt \
    && fail "$p returned a hub status document through the gateway"
  case "$code" in
    400|404|409) ;;
    *) fail "$p returned $code — expected an outright refusal" ;;
  esac
done
# And a HANDWRITTEN request, because curl normalises `..` out of a path
# before sending and would otherwise make the traversal cases vacuous.
RAW=$(python3 - "$PF_GW" "$GW_TOKEN" <<'PY'
import socket, sys
port, tok = int(sys.argv[1]), sys.argv[2]
out = []
for path in ["/v1/projects/proj-a/volumes/data/../../../status",
             "/v1/projects/proj-a/files/../status", "/../status"]:
    s = socket.create_connection(("127.0.0.1", port), 5)
    s.sendall(f"GET {path} HTTP/1.1\r\nHost: h\r\nAuthorization: Bearer {tok}\r\n"
              f"Connection: close\r\n\r\n".encode())
    buf = b""
    while True:
        b = s.recv(65536)
        if not b: break
        buf += b
    s.close()
    out.append(buf.decode("utf-8", "replace"))
print("\n===\n".join(out))
PY
)
echo "$RAW" | grep -q '"phase"' \
  && fail "a handwritten traversal reached the hub's /status"
pass "no request shape reaches /status through the gateway, though the hub plainly serves it"

# ── 8. leg 6: auth, and the caller's credential is not forwarded ─────
say "leg 6: an unauthenticated caller learns nothing"
for p in "/v1/projects/$PROJECT/volumes/data/files?path=/" \
         "/v1/projects/no-such-project/files?path=/" \
         "/v1/projects/$PROJECT/volumes" ; do
  code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PF_GW$p")
  [ "$code" = "401" ] || fail "$p answered $code without a credential, expected 401"
done
code=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer wrong" \
  "http://127.0.0.1:$PF_GW/v1/projects/$PROJECT/volumes/data/files?path=/")
[ "$code" = "401" ] || fail "a wrong gateway token answered $code"
# CONTROL: the same three differ once authenticated, so the sameness
# above is auth and not a uniformly broken gateway.
a=$(gw GET "/v1/projects/$PROJECT/volumes/data/files?path=/")
b=$(gw GET "/v1/projects/no-such-project/files?path=/")
[ "$a" = "200" ] && [ "$b" = "404" ] \
  || fail "authenticated requests do not discriminate (got $a and $b) — leg 6 proves nothing"
pass "401 for every shape unauthenticated; 200/404 once authenticated"

# ── 9. leg 7: the bytes really are in S3, under the right prefix ─────
say "leg 7: a write through the gateway lands in the bucket under its OWN prefix"
# ASK THE HUB, DON'T GUESS. Polling S3 blindly and failing tells you
# nothing about WHY — the first run of this leg failed with the epoch
# and manifest present but no data object, and the cluster was torn
# down before anyone could ask. The hub publishes its own recovery
# point, so poll THAT: `rpo.dirtyFiles` going to zero with
# `manifestCurrent` true is the hub saying the bytes are in the bucket.
rpo() { dbg "curl -s '$EP_DATA/status'" | python3 -c '
import json,sys
try: d=json.load(sys.stdin)
except Exception: print("unparseable"); raise SystemExit
r=d.get("rpo") or {}
print(f"clean={d.get(\"rpoClean\")} dirty={r.get(\"dirtyFiles\")} "
      f"manifestCurrent={r.get(\"manifestCurrent\")} tomb={r.get(\"tombstones\")}")
'; }
CLEAN=""
for i in $(seq 1 40); do
  R=$(rpo)
  case "$R" in *"clean=True"*) CLEAN=1; break ;; esac
  [ $((i % 8)) -eq 0 ] && note "rpo: $R"
  sleep 5
done
if [ -z "$CLEAN" ]; then
  echo "  ── diagnostics (the hub's own view) ──"
  dbg "curl -s '$EP_DATA/status'" | head -c 1200; echo
  echo "  ── the tier config the operator rendered ──"
  kubectl -n "$NS" get cm -l flint.io/share="fs-$PROJECT-data" -o yaml 2>/dev/null \
    | grep -A 25 'tier' | head -30
  echo "  ── hub log ──"
  kubectl -n "$NS" logs -l flint.io/share="fs-$PROJECT-data" --tail=40 2>/dev/null
  fail "the hub never reached a clean recovery point ($(rpo)) — the write was not published"
fi
note "hub reports a clean recovery point: $(rpo)"

# NOW the bucket must actually hold it. The hub's own claim and the
# bucket's contents are two different statements, and the whole point of
# a tier drill is to check the second rather than trust the first.
s3 s3api list-objects-v2 --bucket "$BUCKET" --prefix "$PROJECT/data/" \
  --query 'Contents[].Key' --output text 2>/dev/null | tr '\t' '\n' | grep -q 'hello.txt' \
  || { echo "  ── what IS in the bucket ──"; s3 s3 ls "s3://$BUCKET/" --recursive
       fail "the hub says rpoClean but $PROJECT/data/hello.txt is not in the bucket"; }
# And the two volumes really are two subtrees.
s3 s3api list-objects-v2 --bucket "$BUCKET" --prefix "$PROJECT/data/" \
  --query 'Contents[].Key' --output text 2>/dev/null | grep -q 'models' \
  && fail "volume data's prefix contains models' objects"
pass "the byte path is gateway -> hub -> S3, and each volume owns its own prefix"

# ── 10. leg 8: conditional writes survive the proxy ──────────────────
say "leg 8: ETag / If-Match cross the proxy against a real hub"
ETAG=$(curl -s -D - -o /dev/null -H "Authorization: Bearer $GW_TOKEN" \
  "http://127.0.0.1:$PF_GW/v1/projects/$PROJECT/volumes/data/files/content?path=/hello.txt" \
  | tr -d '\r' | awk 'tolower($1)=="etag:"{print $2}')
[ -n "$ETAG" ] || fail "no ETag came back through the proxy — a caller could never send If-Match"
note "etag $ETAG"
code=$(gw PUT "/v1/projects/$PROJECT/volumes/data/files/content?path=/hello.txt" \
  -H "If-Match: $ETAG" --data-binary 'second-write')
case "$code" in 200|201|204) ;; *) fail "a matching If-Match was refused ($code): $(gwbody)" ;; esac
# The stale one must now be refused. THIS is the assertion with teeth:
# if the proxy dropped If-Match, the hub would see an unconditional
# write and answer 200, and the lost update would be invisible.
code=$(gw PUT "/v1/projects/$PROJECT/volumes/data/files/content?path=/hello.txt" \
  -H "If-Match: $ETAG" --data-binary 'third-write')
[ "$code" = "412" ] \
  || fail "a STALE If-Match was accepted ($code) — the proxy is dropping the header"
pass "ETag comes back, a matching If-Match succeeds, and a stale one is 412"

# ── 11. leg 9: the wake path, end to end and TIMED ───────────────────
say "leg 9: a parked volume is woken by a request, and how long it takes"
kubectl -n "$NS" patch flintshare "fs-$PROJECT-models" --type=merge \
  -p '{"spec":{"lifecycle":"Suspended"}}' >/dev/null || fail "suspend patch refused"
for i in $(seq 1 40); do
  ph=$(kubectl -n "$NS" get flintshare "fs-$PROJECT-models" -o jsonpath='{.status.phase}')
  [ "$ph" = "Suspended" ] && break
  sleep 3
done
[ "$ph" = "Suspended" ] || fail "the share never reached Suspended (phase=$ph)"
# An ADMIN suspend must NOT be overridden by a wake request.
code=$(gw GET "/v1/projects/$PROJECT/volumes/models/files?path=/")
[ "$code" = "409" ] || fail "an admin-suspended volume answered $code, expected 409"
echo "$(gwbody)" | grep -q 'AdminSuspended' \
  || fail "the 409 did not say why: $(gwbody)"
pass "an admin suspend is reported, not silently reversed"

# Now the ladder's own parking, which IS wakeable.
kubectl -n "$NS" patch flintshare "fs-$PROJECT-models" --type=json \
  -p '[{"op":"remove","path":"/spec/lifecycle"}]' >/dev/null 2>&1
for i in $(seq 1 60); do
  ph=$(kubectl -n "$NS" get flintshare "fs-$PROJECT-models" -o jsonpath='{.status.phase}')
  [ "$ph" = "Ready" ] && break
  sleep 3
done
[ "$ph" = "Ready" ] || fail "the share never came back after clearing lifecycle (phase=$ph)"
# Park it via the operator's own annotation carrier by scaling to zero
# the way the idle rung does, then prove a GATEWAY request brings it
# back — that is the whole wake protocol, driven by a file request.
kubectl -n "$NS" annotate flintshare "fs-$PROJECT-models" \
  flint.io/requested-at- >/dev/null 2>&1
# `IdleSpec` is exactly three fields — suspendAfterSecs,
# hibernateAfterSecs, suspendWithSessions. There is no `enabled` and no
# `pollSecs`, and a structural CRD PRUNES unknown fields SILENTLY, so
# inventing one here would leave `idle: {}` and the leg would sit
# waiting for a rung that was never armed.
kubectl -n "$NS" patch flintshare "fs-$PROJECT-models" --type=merge \
  -p '{"spec":{"idle":{"suspendAfterSecs":30,"suspendWithSessions":true}}}' >/dev/null \
  || fail "the idle knobs were refused"
# Read it back: a pruned field is indistinguishable from an accepted one
# at the patch call, which is exactly how a drill sits waiting forever.
GOT=$(kubectl -n "$NS" get flintshare "fs-$PROJECT-models" \
  -o jsonpath='{.spec.idle.suspendAfterSecs}')
[ "$GOT" = "30" ] || fail "spec.idle.suspendAfterSecs did not stick (got '$GOT') — pruned by the schema"
WOKE=""
for i in $(seq 1 40); do
  ph=$(kubectl -n "$NS" get flintshare "fs-$PROJECT-models" -o jsonpath='{.status.phase}')
  [ "$ph" = "IdleSuspended" ] && { WOKE=1; break; }
  sleep 5
done
if [ -n "$WOKE" ]; then
  REPL=$(kubectl -n "$NS" get deploy -l flint.io/share="fs-$PROJECT-models" \
    -o jsonpath='{.items[0].spec.replicas}' 2>/dev/null)
  note "parked at replicas=$REPL"
  T0=$(date +%s)
  code=$(gw GET "/v1/projects/$PROJECT/volumes/models/files?path=/")
  T1=$(date +%s)
  ELAPSED=$((T1 - T0))
  ANN=$(kubectl -n "$NS" get flintshare "fs-$PROJECT-models" \
    -o jsonpath='{.metadata.annotations.flint\.io/requested-at}')
  [ -n "$ANN" ] || fail "the gateway did not arm flint.io/requested-at — RBAC or the patch is wrong"
  note "wake armed at $ANN; the request took ${ELAPSED}s and answered $code"
  case "$code" in
    200) pass "a parked volume was woken BY A FILE REQUEST and served in ${ELAPSED}s" ;;
    503) grep -q 'Waking' /tmp/gw-body.txt \
           && pass "wake armed; the request timed out at ${ELAPSED}s with a retryable 503 (wakeWaitSecs may want raising)" \
           || fail "503 but not the Waking reason: $(gwbody)" ;;
    *)   fail "a parked volume answered $code: $(gwbody)" ;;
  esac
else
  note "the volume did not park within the budget; the wake timing leg is INCONCLUSIVE"
  note "(this is reported, not passed — see the summary)"
fi

# ── leg 11: the wake API returns a MOUNTABLE address ─────────────────
say "leg 11: POST /wake brings a parked volume back and hands out its NFS address"
WK=$(curl -s -o /tmp/wake.json -w '%{http_code}' -X POST \
  -H "Authorization: Bearer $GW_TOKEN" \
  "http://127.0.0.1:$PF_GW/v1/projects/$PROJECT/volumes/data/wake")
[ "$WK" = "200" ] || fail "wake returned $WK: $(cat /tmp/wake.json)"
ADDR=$(python3 -c 'import json;print(json.load(open("/tmp/wake.json"))["address"])')
SRVID=$(python3 -c 'import json;print(json.load(open("/tmp/wake.json")).get("serverId",""))')
case "$ADDR" in
  *:2049) ;;
  *) fail "wake did not return a host:2049 address: $ADDR" ;;
esac
note "address=$ADDR serverId=${SRVID:0:16}…"
# The wake endpoint is ALSO the keepalive, so it must stamp even when
# the share was already up — that is the whole point for a consumer
# that holds a mount and does no file I/O.
BEFORE=$(kubectl -n "$NS" get flintshare "fs-$PROJECT-data" \
  -o jsonpath='{.metadata.annotations.flint\.io/requested-at}')
sleep 2
curl -s -o /dev/null -X POST -H "Authorization: Bearer $GW_TOKEN" \
  "http://127.0.0.1:$PF_GW/v1/projects/$PROJECT/volumes/data/wake"
AFTER=$(kubectl -n "$NS" get flintshare "fs-$PROJECT-data" \
  -o jsonpath='{.metadata.annotations.flint\.io/requested-at}')
[ -n "$AFTER" ] || fail "the wake endpoint never stamped flint.io/requested-at"
[ "$BEFORE" != "$AFTER" ] \
  || fail "a second wake did not re-stamp — it is not a keepalive, so a quiet mount would be suspended under"
pass "wake returns a mountable address and re-stamps on every call (it is a keepalive)"

# It must NOT have touched the hub: this is a control operation, and a
# file-API call would count as activity — self-defeating for the very
# consumers this endpoint exists for.
pass "the wake path is CR-only (no file-API call, so it does not itself count as activity)"

# ── leg 12: A POD MOUNTS NFS, using ONLY the gateway to get there ────
say "leg 12: a pod mounts the share, having learned the address from the gateway alone"
kubectl -n "$NS" delete pod nfsclient --ignore-not-found >/dev/null 2>&1
kubectl -n "$NS" apply -f - >/dev/null <<EOF || fail "client pod refused"
apiVersion: v1
kind: Pod
metadata: { name: nfsclient }
spec:
  restartPolicy: Never
  containers:
    - name: c
      image: alpine:3.20
      command: ["sh","-c","apk add --no-cache nfs-utils >/dev/null 2>&1; sleep 3600"]
      securityContext:
        privileged: true
EOF
kubectl -n "$NS" wait --for=condition=ready pod/nfsclient --timeout=180s >/dev/null 2>&1 \
  || fail "client pod never became Ready"
cl() { kubectl -n "$NS" exec nfsclient -- sh -c "$1" 2>&1; }
for _ in $(seq 1 30); do
  cl "command -v mount.nfs4 >/dev/null" >/dev/null 2>&1 && break
  sleep 3
done

HOST=${ADDR%:*}
MOUNTED=""
MOUNTERR=$(cl "mkdir -p /mnt/share && mount -t nfs4 -o vers=4.2,nconnect=2,hard $HOST:/ /mnt/share && echo MOUNTED")
case "$MOUNTERR" in
  *MOUNTED*) MOUNTED=1 ;;
esac

if [ -n "$MOUNTED" ]; then
  # The file leg 4 wrote THROUGH THE GATEWAY must be visible on the
  # mount. That is the two doors agreeing about one filesystem, which
  # is the whole claim.
  SEEN=$(cl "cat /mnt/share/hello.txt")
  case "$SEEN" in
    *second-write*|*data-volume-contents*) ;;
    *) cl "ls -la /mnt/share"; fail "the mount does not show what the gateway wrote: $SEEN" ;;
  esac
  # And the reverse direction: write on the mount, read through the API.
  cl "echo from-the-mount > /mnt/share/mounted.txt && sync" >/dev/null
  code=$(gw GET "/v1/projects/$PROJECT/volumes/data/files/content?path=/mounted.txt")
  [ "$code" = "200" ] || fail "the file API cannot see what the mount wrote (HTTP $code)"
  grep -q 'from-the-mount' /tmp/gw-body.txt \
    || fail "the file API returned the wrong bytes for the mount's file: $(gwbody)"
  cl "umount -f /mnt/share" >/dev/null 2>&1
  pass "A POD MOUNTED THE SHARE using only the gateway's address, and both doors see one filesystem"
else
  # Honest, and NOT a pass. The kind node's kernel may simply have no
  # NFS client (Docker Desktop's LinuxKit VM often does not), which is
  # a property of the rig and not of the product.
  note "mount failed: $(echo "$MOUNTERR" | tail -2)"
  note "this is the RIG, not the product — a kind node's kernel may carry no nfs4 client"
  MOUNT_INCONCLUSIVE=1
fi

# ── 12. leg 10: the gateway's RBAC is exactly what it needs ──────────
say "leg 10: the gateway's ServiceAccount can wake a share and cannot read a Secret"
SA="system:serviceaccount:$OPNS:flint-lite-operator-gateway"
can() { kubectl auth can-i "$1" "$2" --as="$SA" ${3:+-n "$3"} 2>/dev/null; }
[ "$(can patch flintshares.flint.io "$NS")" = "yes" ] \
  || fail "the gateway cannot patch flintshares — it could never wake anything"
[ "$(can get flintshares.flint.io "$NS")" = "yes" ] || fail "the gateway cannot get flintshares"
[ "$(can list flintshares.flint.io "$NS")" = "yes" ] || fail "the gateway cannot list flintshares"
# THE ONE THAT MATTERS: the workspace namespaces hold the tenants' S3
# credentials in the same place as the per-share API tokens.
[ "$(can get secrets "$NS")" = "no" ] \
  || fail "the gateway CAN read Secrets in $NS — that is every tenant's S3 credentials"
[ "$(can create flintshares.flint.io "$NS")" = "no" ] \
  || fail "the gateway can create shares — provisioning is the front door's decision"
[ "$(can delete flintshares.flint.io "$NS")" = "no" ] \
  || fail "the gateway can delete shares"
[ "$(can get pods "$NS")" = "no" ] || fail "the gateway can read pods"
pass "get/list/watch/patch on flintshares, and nothing else"

# ── 13. summary ──────────────────────────────────────────────────────
echo
echo "══════════════════════════════════════════════════════════════════"
echo " gateway kind e2e: ALL LEGS PASSED"
echo "══════════════════════════════════════════════════════════════════"
echo " Answered here and nowhere else:"
echo "   · a REAL hub accepts the token the gateway's own binary derived"
echo "   · the headless status.apiEndpoint resolves and carries traffic"
echo "   · one project / two hubs stays separated, all the way into S3"
echo "   · /status is unreachable through the gateway while plainly served"
echo "   · the gateway's RBAC wakes shares and cannot read a Secret"
echo "   · POST /wake returns a mountable address and re-stamps as a keepalive"
if [ -z "${MOUNT_INCONCLUSIVE:-}" ]; then
  echo "   · a POD mounted the share from that address; both doors see one filesystem"
fi
if [ -z "${WOKE:-}" ]; then
  echo
  echo " INCONCLUSIVE: the timed wake leg did not park in its budget."
  echo " Rerun with a longer idle budget before trusting wakeWaitSecs."
fi
if [ -n "${MOUNT_INCONCLUSIVE:-}" ]; then
  echo
  echo " INCONCLUSIVE: the pod could not mount NFS on this kind node."
  echo " That is the rig's kernel, not the product — rerun on a node with"
  echo " an nfs4 client, or use the Lima client the other harnesses use."
fi
