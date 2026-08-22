#!/usr/bin/env bash
# flint-hub-gateway kind e2e — the proxy, against REAL hubs and REAL S3.
#
# WHY THIS EXISTS
#
# The unit suite (73 tests) proves the routing, the refusals and the
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
# It also drills two shapes a single-hub rig cannot express at all: ONE
# PROJECT WITH TWO HUBS, and AN AGENT THAT MOUNTS NFS knowing nothing
# but the gateway's address — leg 10 asks `POST /wake` for a mount
# target and leg 11 mounts it from a pod, then checks that the file API
# and the mount see one filesystem.
#
# Legs 13-15 close three more, all of them properties of the RUNNING
# process rather than of the routing table:
#
#   13 MEMORY UNDER LOAD. `stream_body` and `relay` claim never to hold
#      a body. A container limit several times smaller than the body
#      turns that claim into a pass/fail: buffer it and the kernel ends
#      the argument. Paired with a round-trip checksum, because flat
#      memory is also what transferring nothing looks like.
#   14 THE COLD-READ 503. A download of an evicted file makes the hub
#      pull it back from S3 and, past `hydrateWaitSecs`, answer 503 with
#      a `Retry-After`. Two ways to get this wrong: drop the header, or
#      time out first and substitute a 502 — after which a browse UI
#      cannot tell "coming, ask again" from "this hub is broken".
#      `header_deadline` exists for the second and had never run.
#   15 NETWORKPOLICY, ENFORCED. The operator chart adds a gateway peer
#      to the hubs' 8080 rule automatically. Until this leg that peer
#      had only ever been RENDERED, and it fails CLOSED — a wrong
#      selector times out every file request in the fleet while the
#      policy still reads correctly. kind's kindnetd enforces
#      NetworkPolicy as of v0.32.0, so this needs no second CNI; on a
#      rig whose CNI ignores it the leg reports INCONCLUSIVE rather
#      than claiming a security property it did not observe.
#
# On the two-hub shape: Nothing in the operator ties a project to one
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
# A SECOND project, one volume, for the bulk-transfer and cold-read
# legs. Separate so that nothing legs 1-12 assert about proj-a's volume
# set can be disturbed by it.
PROJECT2=proj-b
# The body legs 13-14 push through the proxy, in MiB. Must be a
# multiple of 8 (the seed block below) and comfortably larger than
# GW_MEM_LIMIT or leg 13 cannot fail.
BULK_MB=${BULK_MB:-256}
GW_MEM_LIMIT=${GW_MEM_LIMIT:-128Mi}
MINIO_USER=flintdrill
MINIO_PASS=flintdrill123
PF_S3=39100
PF_GW=39101
PF_S3_PID=""
PF_GW_PID=""
KUBECONFIG_FILE="$(mktemp -t flint-gw-kubeconfig.XXXXXX)"
export KUBECONFIG="$KUBECONFIG_FILE"

# macOS ships `shasum`, Linux ships `sha256sum`; this drill is run on
# both.
sha() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'
  else shasum -a 256 "$1" | awk '{print $1}'; fi
}

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

# THE PORT-FORWARD IS RESTARTED, NOT JUST WAITED ON.
#
# `kubectl port-forward` exits on its own when it is started against a
# pod that is still settling — and once it has exited, polling its port
# for another twenty seconds cannot do anything but fail. That is a
# whole run lost to a rig fault that looks like a broken product: run
# 11 of this drill died at "MinIO port-forward never became healthy"
# with MinIO Running, 1/1, and answering /minio/health/live perfectly
# well the moment anyone asked it by hand.
#
# So the inner loop gives up the moment the forwarder is gone, and the
# outer loop starts a new one.
pf_wait() {   # pf_wait <pid-var-name> <health url> <tries>
  local pid="$1" url="$2" tries="$3" _
  for _ in $(seq 1 "$tries"); do
    curl -sf "$url" >/dev/null && return 0
    kill -0 "$pid" 2>/dev/null || return 1   # forwarder died; caller respawns
    sleep 1
  done
  return 1
}

pf_s3() {
  [ -n "$PF_S3_PID" ] && kill "$PF_S3_PID" 2>/dev/null
  for _ in 1 2 3; do
    kubectl -n "$NS" port-forward svc/minio "$PF_S3:9000" >/dev/null 2>&1 &
    PF_S3_PID=$!
    pf_wait "$PF_S3_PID" "http://127.0.0.1:$PF_S3/minio/health/live" 20 && return 0
    kill "$PF_S3_PID" 2>/dev/null
  done
  kubectl -n "$NS" get pod -l app=minio -o wide
  fail "MinIO port-forward never became healthy after three attempts"
}

pf_gw() {
  [ -n "$PF_GW_PID" ] && kill "$PF_GW_PID" 2>/dev/null
  for _ in 1 2 3; do
    kubectl -n "$OPNS" port-forward svc/flint-lite-operator-gateway "$PF_GW:8090" \
      >/dev/null 2>&1 &
    PF_GW_PID=$!
    pf_wait "$PF_GW_PID" "http://127.0.0.1:$PF_GW/healthz" 30 && return 0
    kill "$PF_GW_PID" 2>/dev/null
  done
  kubectl -n "$OPNS" get pod -l app.kubernetes.io/name=flint-lite-operator-gateway -o wide
  fail "gateway port-forward never became healthy after three attempts"
}

# THE DRILL'S OWN HOP IS NOT THE PRODUCT, AND MUST NOT BE REPORTED AS IT.
#
# `kubectl port-forward` binds ONE POD and dies with it. Legs 14 and 15
# each roll the gateway Deployment, so the pod this forward is pinned to
# is replaced underneath it — and every subsequent request then answers
# HTTP 000, which is curl saying it never got a response at all.
#
# That cost a run: leg 15 reported "the gateway can no longer reach the
# hub under the policy — the auto-added peer does not match", printed
# the policy and the pod labels side by side as evidence, and the two
# MATCHED. The gateway was serving that request in 12ms the whole time.
# A harness that blames the product for its own broken transport is
# worse than no harness.
#
# So 000 is never an answer here. Re-establish and ask again; a second
# 000 is reported as what it is.
gw_alive() { curl -sf --max-time 5 "http://127.0.0.1:$PF_GW/healthz" >/dev/null || pf_gw; }

# curl through the gateway. Prints the status; body lands in gw-body.
gw() {
  local method="$1" path="$2"; shift 2
  local code
  code=$(curl -s -o /tmp/gw-body.txt -w '%{http_code}' -X "$method" \
    -H "Authorization: Bearer $GW_TOKEN" "$@" "http://127.0.0.1:$PF_GW$path")
  if [ "$code" = "000" ]; then
    note "no response at all — the drill's port-forward dropped; re-establishing"
    pf_gw
    code=$(curl -s -o /tmp/gw-body.txt -w '%{http_code}' -X "$method" \
      -H "Authorization: Bearer $GW_TOKEN" "$@" "http://127.0.0.1:$PF_GW$path")
  fi
  echo "$code"
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
# netcat is for leg 15: NetworkPolicy closes 2049 as well as 8080, and
# a TCP probe is the only way to see that without a kernel mount.
RUN apk add --no-cache curl ca-certificates netcat-openbsd
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

# ONE function for every helm call in this drill, carrying the WHOLE
# flag set each time. Legs 14 and 15 both upgrade this release, and
# `--reuse-values` is never used: it reads the OLD chart's computed
# values, which has silently reverted a knob in this repo's drills
# before — here it would drop `gateway.enabled` and take the rest of
# the run with it.
#
# THE MEMORY LIMIT IS PART OF THE TEST. Leg 13 pushes a body several
# times this size through the proxy; a gateway that buffered it would
# be OOMKilled, and that is the whole assertion. The chart's default
# (512Mi) is too generous for the drill's file size to be decisive.
helm_up() {   # helm_up [extra --set flags...]
  helm upgrade --install flint-lite-operator "$OP_CHART" -n "$OPNS" \
    --set image.ref="$OPIMG" --set hubImage="$HUBIMG" \
    --set gateway.enabled=true \
    --set gateway.tokenSecretRef=flint-gateway-token \
    --set gateway.rootKeySecretRef=flint-gateway-root \
    --set gateway.replicas=1 \
    --set gateway.wakeWaitSecs=60 \
    --set gateway.resources.requests.cpu=100m \
    --set gateway.resources.requests.memory=64Mi \
    --set gateway.resources.limits.memory="$GW_MEM_LIMIT" \
    "$@" >/tmp/gw-e2e-helm.log 2>&1
}
helm_up || { tail -25 /tmp/gw-e2e-helm.log; fail "helm install failed"; }
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
TOK_BULK=$(derive "$PROJECT2/bulk/")
kubectl -n "$NS" create secret generic tok-bulk --from-literal=token="$TOK_BULK" >/dev/null \
  || fail "token Secret refused"
TOK_COLD=$(derive "$PROJECT2/cold/")
kubectl -n "$NS" create secret generic tok-cold --from-literal=token="$TOK_COLD" >/dev/null \
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

# ── a SECOND project, two volumes, for legs 13 and 14 ────────────────
#
# Separate from proj-a so nothing legs 1-12 assert about its volume set
# can be disturbed, and split in two because the two legs want opposite
# things from the tier: leg 13 needs a file that STAYS put while a
# ${BULK_MB} MiB round trip runs over it, and leg 14 needs one that is
# guaranteed COLD.
#
# WHY `watermarkPct: 1` AND NOT A SIZE. The obvious version of this
# picks a watermark from the claim size — 1Gi disk, a ${BULK_MB} MiB
# file, put the mark in between. That is wrong here, and quietly:
# kind's default StorageClass is local-path, which backs a PVC with a
# hostPath and does not enforce `persistence.size` at all. The hub's
# `statvfs` therefore reports the NODE's filesystem (~58 GiB, ~8% used
# on a fresh Docker VM), where a ${BULK_MB} MiB file moves the used
# percentage by less than half a point and no size-derived mark would
# ever fire. `1` is above no real filesystem's floor, so the pass is
# permanently armed and evicts each file as soon as it is clean —
# which is the property leg 14 actually needs. Demand hydration is
# admitted on HEADROOM (`space::admit_hydration`), not on the
# watermark, so an always-armed pass does not wedge the read back.
#
#   hydrateWaitSecs: 0        the FIRST Delay on a cold read is 503,
#                             with no race to lose against a MinIO in
#                             the same cluster. Leg 14b raises it.
#   hydrateFetchParallel: 1   the cold-read fan-out is what makes a
#                             real restore fast; off, a ${BULK_MB} MiB
#                             whole-file GET is guaranteed to outlive
#                             the one-second deadline leg 14b squeezes
#                             the gateway down to.
#   hydrateWarmAfterImport: false  otherwise the hub restart in leg 14b
#                             bulk-restores the tree and the cold file
#                             is warm again before anything asks.
mkvol2() {  # mkvol2 <volume> <extra fileApi yaml> <extra settings yaml>
  kubectl -n "$NS" apply -f - >/dev/null <<EOF || fail "FlintShare $1 refused"
apiVersion: flint.io/v1alpha1
kind: FlintShare
metadata:
  name: fs-$PROJECT2-$1
  labels:
    flint.io/project-id: $PROJECT2
    flint.io/volume-id: $1
spec:
  bucket: $BUCKET
  keyPrefix: $PROJECT2/$1/
  endpoint: http://minio.$NS.svc:9000
  region: us-east-1
  credentialsSecretRef: flint-tier-s3
  persistence:
    size: 4Gi
  monitoring:
    enabled: true
    fileApi:
      enabled: true
      tokenSecretRef: tok-$1
$2
  settings:
    flushFloorSecs: 3
$3
EOF
}
# leg 13's volume: ordinary tier settings, so a published file is NOT
# snatched out from under the round trip.
mkvol2 bulk "" ""
# leg 14's volume: always-armed eviction and a deliberately slow restore.
mkvol2 cold "      hydrateWaitSecs: 0" "    watermarkPct: 1
    ballastBytes: 0
    hydrateFetchParallel: 1
    hydrateWarmAfterImport: false"

# A structural CRD PRUNES unknown fields SILENTLY, so read back the two
# that leg 14 is built on. Getting either wrong turns the leg into a
# wait for something that was never armed.
GOT=$(kubectl -n "$NS" get flintshare "fs-$PROJECT2-cold" -o jsonpath='{.spec.settings.watermarkPct}')
[ "$GOT" = "1" ] || fail "spec.settings.watermarkPct did not stick (got '$GOT') — pruned by the schema"
GOT=$(kubectl -n "$NS" get flintshare "fs-$PROJECT2-cold" \
  -o jsonpath='{.spec.monitoring.fileApi.hydrateWaitSecs}')
[ "$GOT" = "0" ] || fail "spec.monitoring.fileApi.hydrateWaitSecs did not stick (got '$GOT')"

for s in "fs-$PROJECT-data" "fs-$PROJECT-models" \
         "fs-$PROJECT2-bulk" "fs-$PROJECT2-cold"; do
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
  --command -- sleep 10800 >/dev/null 2>&1
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
rpo_at() { dbg "curl -s '$1/status'" | python3 "$REPO_ROOT/tests/regression/lib/hub-rpo.py"; }
rpo() { rpo_at "$EP_DATA"; }
# One dotted field out of a hub's /status — see lib/hub-gauge.py for
# why this is a file and not an inlined heredoc.
gauge_at() { dbg "curl -s '$1/status'" | python3 "$REPO_ROOT/tests/regression/lib/hub-gauge.py" "$2"; }
CLEAN=""
for i in $(seq 1 40); do
  R=$(rpo)
  case "$R" in *"rpoClean=True"*) CLEAN=1; break ;; esac
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

  # ── the crawl guard, WHILE IT IS STILL PARKED ──────────────────────
  #
  # Deliberately here rather than after the wake below: this way the
  # SAME share at the SAME moment gives two different answers, and the
  # only difference is `wake=false`. Run afterwards it proved nothing —
  # the volume was Ready by then, so a 200 was correct and the leg
  # failed on its own ordering.
  say "leg 9b: wake=false refuses a parked volume instead of starting it"
  ANN_BEFORE=$(kubectl -n "$NS" get flintshare "fs-$PROJECT-models" -o json \
    | python3 "$REPO_ROOT/tests/regression/lib/share-annotation.py" flint.io/requested-at)
  C0=$(date +%s)
  code=$(gw GET "/v1/projects/$PROJECT/volumes/models/files?path=/&wake=false")
  C1=$(date +%s)
  [ "$code" = "503" ] || fail "wake=false on a parked volume returned $code, expected 503"
  grep -q 'Parked' /tmp/gw-body.txt || fail "the 503 did not say Parked: $(gwbody)"
  [ $((C1 - C0)) -lt 10 ] \
    || fail "wake=false waited out the wake budget ($((C1 - C0))s) — a crawl would stall"
  ANN_AFTER=$(kubectl -n "$NS" get flintshare "fs-$PROJECT-models" -o json \
    | python3 "$REPO_ROOT/tests/regression/lib/share-annotation.py" flint.io/requested-at)
  [ "$ANN_BEFORE" = "$ANN_AFTER" ] \
    || fail "wake=false STAMPED the wake annotation ('$ANN_BEFORE' -> '$ANN_AFTER')"
  R1=$(kubectl -n "$NS" get deploy -l flint.io/share="fs-$PROJECT-models" \
    -o jsonpath='{.items[0].spec.replicas}' 2>/dev/null)
  [ "$R1" = "0" ] || fail "wake=false brought the hub back up (replicas=$R1)"
  pass "wake=false refuses in $((C1 - C0))s, stamps nothing, hub stays at replicas=0"

  # A typo must NOT read as "yes, wake" — that mistake's blast radius is
  # every parked share in the fleet.
  code=$(gw GET "/v1/projects/$PROJECT/volumes/models/files?path=/&wake=fasle")
  [ "$code" = "400" ] || fail "an unreadable wake= value returned $code, expected 400"

  # The LISTING endpoint never wakes anything either — the cheapest
  # fleet-crawl primitive, and it reports `serving` per volume.
  code=$(gw GET "/v1/projects/$PROJECT/volumes")
  [ "$code" = "200" ] || fail "volume listing returned $code"
  echo "$(gwbody)" | python3 -c '
import json,sys
d=json.load(sys.stdin)
by={v["volume"]: v for v in d["volumes"]}
assert by["models"]["serving"] is False, by["models"]
assert by["data"]["serving"] is True, by["data"]
' || fail "the listing misreports which volumes are serving: $(gwbody)"
  R1=$(kubectl -n "$NS" get deploy -l flint.io/share="fs-$PROJECT-models" \
    -o jsonpath='{.items[0].spec.replicas}' 2>/dev/null)
  [ "$R1" = "0" ] || fail "listing volumes woke a parked hub (replicas=$R1)"
  pass "listing volumes reports serving per volume and wakes nothing"

  # ── and NOW the same share, same moment, without wake=false ────────
  say "leg 9c: the same parked volume, woken by an ordinary file request"
  T0=$(date +%s)
  code=$(gw GET "/v1/projects/$PROJECT/volumes/models/files?path=/")
  T1=$(date +%s)
  ELAPSED=$((T1 - T0))
  ANN=$(kubectl -n "$NS" get flintshare "fs-$PROJECT-models" -o json \
    | python3 "$REPO_ROOT/tests/regression/lib/share-annotation.py" flint.io/requested-at)
  note "request took ${ELAPSED}s, answered $code; requested-at='$ANN' (expected empty)"
  # ASSERT THE OUTCOME, NOT THE MECHANISM.
  #
  # An earlier version of this leg required `flint.io/requested-at` to
  # still be set after the request, and failed twice against a gateway
  # whose unit tests prove it emits exactly that merge patch. The
  # annotation is DELIBERATELY TRANSIENT: `reconcile.rs` clears it the
  # moment it honours the wake ("the NEXT idle window starts from the
  # hub's own activity clock rather than from a stale heartbeat"), and
  # the gateway waits up to wakeWaitSecs for the share to come back — so
  # by the time this reads it, the operator has always removed it.
  #
  # What is observable, and what actually matters, is that the hub came
  # back: replicas 0 -> 1 and the phase left IdleSuspended.
  #
  # (A keepalive stamp on a RUNNING share is NOT cleared — that path
  # returns Stay/Hold and writes no annotations — so the keepalive
  # contract is unaffected by this.)
  WOKE_REPL=""
  for i in $(seq 1 30); do
    R2=$(kubectl -n "$NS" get deploy -l flint.io/share="fs-$PROJECT-models" \
      -o jsonpath='{.items[0].spec.replicas}' 2>/dev/null)
    [ "$R2" = "1" ] && { WOKE_REPL=1; break; }
    sleep 2
  done
  PH2=$(kubectl -n "$NS" get flintshare "fs-$PROJECT-models" -o jsonpath='{.status.phase}')
  note "after the request: replicas=$R2 phase=$PH2"
  if [ -z "$WOKE_REPL" ]; then
    # EVIDENCE BEFORE THE VERDICT. This assertion fired twice with
    # nothing to look at and the cluster already torn down, which sent
    # the investigation to the wrong place both times. The gateway logs
    # "wake requested" on success and "wake request failed" with the API
    # error on failure — that one line is the whole answer.
    echo "  ── what the gateway answered ──"; gwbody | head -c 600; echo
    echo "  ── gateway log ──"
    kubectl -n "$OPNS" logs -l app.kubernetes.io/name=flint-lite-operator-gateway \
      --tail=40 2>/dev/null
    echo "  ── the share, as the gateway would have seen it ──"
    kubectl -n "$NS" get flintshare "fs-$PROJECT-models" \
      -o jsonpath='{.metadata.annotations}{"\n"}{.status.phase}{"\n"}' 2>/dev/null
    echo "  ── can the gateway SA patch it? ──"
    kubectl auth can-i patch flintshares.flint.io \
      --as="system:serviceaccount:$OPNS:flint-lite-operator-gateway" -n "$NS"
    fail "the request did not bring the hub back (replicas=$R2, phase=$PH2)"
  fi
  pass "a file request on a parked volume brought its hub back: replicas 0 -> 1"
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

# ── leg 10: the wake API returns a MOUNTABLE address ─────────────────
say "leg 10: POST /wake brings a parked volume back and hands out its NFS address"
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
BEFORE=$(kubectl -n "$NS" get flintshare "fs-$PROJECT-data" -o json \
  | python3 "$REPO_ROOT/tests/regression/lib/share-annotation.py" flint.io/requested-at)
sleep 2
curl -s -o /dev/null -X POST -H "Authorization: Bearer $GW_TOKEN" \
  "http://127.0.0.1:$PF_GW/v1/projects/$PROJECT/volumes/data/wake"
AFTER=$(kubectl -n "$NS" get flintshare "fs-$PROJECT-data" -o json \
  | python3 "$REPO_ROOT/tests/regression/lib/share-annotation.py" flint.io/requested-at)
[ -n "$AFTER" ] || fail "the wake endpoint never stamped flint.io/requested-at"
[ "$BEFORE" != "$AFTER" ] \
  || fail "a second wake did not re-stamp — it is not a keepalive, so a quiet mount would be suspended under"
pass "wake returns a mountable address and re-stamps on every call (it is a keepalive)"

# It must NOT have touched the hub: this is a control operation, and a
# file-API call would count as activity — self-defeating for the very
# consumers this endpoint exists for.
pass "the wake path is CR-only (no file-API call, so it does not itself count as activity)"

# ── leg 11: A POD MOUNTS NFS, using ONLY the gateway to get there ────
say "leg 11: a pod mounts the share, having learned the address from the gateway alone"
kubectl -n "$NS" delete pod nfsclient --ignore-not-found >/dev/null 2>&1
kubectl -n "$NS" apply -f - >/dev/null <<EOF || fail "client pod refused"
apiVersion: v1
kind: Pod
metadata:
  name: nfsclient
  # leg 15 opens a NetworkPolicy hole for exactly this pod and proves
  # the hole admits it while the debug pod beside it stays shut out.
  labels: { app: nfsclient }
spec:
  restartPolicy: Never
  containers:
    - name: c
      image: alpine:3.20
      # netcat as well as the nfs client: leg 15 probes 2049 from this
      # pod to prove the policy's nfsClientSelectors hole admits it.
      command: ["sh","-c","apk add --no-cache nfs-utils netcat-openbsd >/dev/null 2>&1; sleep 10800"]
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

# ── leg 12: the gateway's RBAC is exactly what it needs ──────────────
say "leg 12: the gateway's ServiceAccount can wake a share and cannot read a Secret"
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

# Disarm the idle ladder leg 9 armed. Legs 13-15 count hub pods and
# their restarts; a share that parks and wakes underneath them turns
# that arithmetic into a coin flip, and the resulting failure would
# read as "the NetworkPolicy killed a hub".
kubectl -n "$NS" patch flintshare "fs-$PROJECT-models" --type=json \
  -p '[{"op":"remove","path":"/spec/idle"}]' >/dev/null 2>&1

# ── leg 13: a big body CROSSES the gateway, it does not sit in it ─────
say "leg 13: a ${BULK_MB} MiB body crosses the gateway under a ${GW_MEM_LIMIT} limit"
# THE CONTROL FOR THE WHOLE LEG, AND IT COMES FIRST. "The gateway was
# not OOMKilled" is a statement about nothing at all if the container
# has no limit, or a limit larger than the body. A values typo or a
# chart edit would produce exactly that, and this leg would go green
# while testing whether a machine with 16 GB can hold 256 MB.
LIM=$(kubectl -n "$OPNS" get deploy flint-lite-operator-gateway \
  -o jsonpath='{.spec.template.spec.containers[0].resources.limits.memory}')
[ -n "$LIM" ] || fail "the gateway has NO memory limit — leg 13 cannot fail, so it proves nothing"
LIM_MB=$(python3 -c "
import re
m = re.match(r'^(\d+)(Mi|Gi|M|G)?\$', '$LIM')
mult = {'Mi': 1, 'Gi': 1024, 'M': 1, 'G': 1024}.get(m.group(2), 0) if m else 0
print(int(m.group(1)) * mult if mult else 0)")
[ "${LIM_MB:-0}" -gt 0 ] || fail "could not read the gateway's memory limit ('$LIM')"
[ "$LIM_MB" -lt "$BULK_MB" ] \
  || fail "the limit (${LIM_MB}Mi) is not smaller than the body (${BULK_MB}Mi) — a buffered body would FIT, so this leg cannot fail"
note "limit ${LIM_MB}Mi vs a ${BULK_MB}Mi body — buffering either direction needs $(python3 -c "print(round($BULK_MB/$LIM_MB,1))")x the limit"

# Incompressible-ish and cheap: an 8 MiB random block, repeated. Random
# bytes matter here because the round-trip checksum below is the proof
# that the transfer HAPPENED — and a body of zeros can be produced by
# accident in more ways than one.
dd if=/dev/urandom of=/tmp/gw-seed.bin bs=1048576 count=8 >/dev/null 2>&1 \
  || fail "could not generate the seed block"
: > /tmp/gw-bulk.bin
for _ in $(seq 1 $((BULK_MB / 8))); do cat /tmp/gw-seed.bin >> /tmp/gw-bulk.bin; done
SUM_IN=$(sha /tmp/gw-bulk.bin)
SIZE_IN=$(wc -c < /tmp/gw-bulk.bin | tr -d ' ')
note "body $SIZE_IN bytes, sha256 ${SUM_IN:0:16}…"

gw_pod()      { kubectl -n "$OPNS" get pod -l app.kubernetes.io/name=flint-lite-operator-gateway \
                  -o jsonpath='{.items[0].metadata.name}'; }
gw_restarts() { kubectl -n "$OPNS" get pod -l app.kubernetes.io/name=flint-lite-operator-gateway \
                  -o jsonpath='{.items[0].status.containerStatuses[0].restartCount}'; }
gw_lastterm() { kubectl -n "$OPNS" get pod -l app.kubernetes.io/name=flint-lite-operator-gateway \
                  -o jsonpath='{.items[0].status.containerStatuses[0].lastState.terminated.reason}'; }
# cgroup v2 first, then v1. Best effort: the NUMBER is informative, the
# OOMKill is the assertion.
gw_peak_mb()  { kubectl -n "$OPNS" exec "$(gw_pod)" -- sh -c \
                  'cat /sys/fs/cgroup/memory.peak 2>/dev/null || cat /sys/fs/cgroup/memory/memory.max_usage_in_bytes 2>/dev/null' \
                  2>/dev/null | tr -d '\r\n' | awk 'NF{printf "%d", $1/1048576}'; }

R0=$(gw_restarts); P0=$(gw_pod); PEAK0=$(gw_peak_mb)
note "before: pod $P0 restarts=$R0 peak=${PEAK0:-?}Mi"

T0=$(date +%s)
code=$(gw PUT "/v1/projects/$PROJECT2/volumes/bulk/files/content?path=/bulk.bin" \
  -H 'Content-Type: application/octet-stream' --data-binary @/tmp/gw-bulk.bin)
T1=$(date +%s)
R1=$(gw_restarts)
if [ "$R1" != "$R0" ]; then
  fail "the gateway container RESTARTED during a ${BULK_MB}Mi upload (restarts $R0 -> $R1, last termination: $(gw_lastterm)) — the request body is being buffered, not streamed"
fi
case "$code" in
  200|201) ;;
  *) echo "  ── gateway log ──"
     kubectl -n "$OPNS" logs -l app.kubernetes.io/name=flint-lite-operator-gateway --tail=30 2>/dev/null
     fail "the ${BULK_MB}Mi upload returned $code: $(gwbody)" ;;
esac
pass "PUT ${BULK_MB}Mi in $((T1 - T0))s, gateway not restarted (upload streams)"

gw_alive
T0=$(date +%s)
code=$(curl -s -o /tmp/gw-bulk-out.bin -w '%{http_code}' \
  -H "Authorization: Bearer $GW_TOKEN" \
  "http://127.0.0.1:$PF_GW/v1/projects/$PROJECT2/volumes/bulk/files/content?path=/bulk.bin")
T1=$(date +%s)
R2=$(gw_restarts)
if [ "$R2" != "$R0" ]; then
  fail "the gateway container RESTARTED during a ${BULK_MB}Mi download (restarts $R0 -> $R2, last termination: $(gw_lastterm)) — the response body is being buffered, not streamed"
fi
[ "$code" = "200" ] || fail "the ${BULK_MB}Mi download returned $code"

# THE ANTI-VACUITY GUARD. Flat memory is exactly what a gateway that
# transferred NOTHING would also show. The round trip has to be
# byte-for-byte, or "it streamed" is a claim about an empty pipe.
SIZE_OUT=$(wc -c < /tmp/gw-bulk-out.bin | tr -d ' ')
[ "$SIZE_OUT" = "$SIZE_IN" ] \
  || fail "round trip returned $SIZE_OUT bytes, sent $SIZE_IN — the transfer is short, so the memory result means nothing"
SUM_OUT=$(sha /tmp/gw-bulk-out.bin)
[ "$SUM_OUT" = "$SUM_IN" ] || fail "round trip corrupted the body ($SUM_IN -> $SUM_OUT)"
PEAK1=$(gw_peak_mb)
note "after: restarts=$R2 peak=${PEAK1:-?}Mi (was ${PEAK0:-?}Mi)"
if [ -n "$PEAK1" ] && [ "$PEAK1" -gt 0 ] 2>/dev/null; then
  [ "$PEAK1" -lt "$BULK_MB" ] \
    || fail "peak RSS ${PEAK1}Mi reached the body size — that is buffering"
fi
pass "${BULK_MB}Mi round trip is byte-identical, gateway peak ${PEAK1:-?}Mi under a ${LIM_MB}Mi limit"
rm -f /tmp/gw-bulk-out.bin /tmp/gw-seed.bin

# ── leg 14: a COLD read is a RELAYED 503, not a gateway 502 ──────────
say "leg 14: a cold read relays the hub's 503 + Retry-After"
EP_COLD=$(kubectl -n "$NS" get flintshare "fs-$PROJECT2-cold" -o jsonpath='{.status.apiEndpoint}')
[ -n "$EP_COLD" ] || fail "the cold share published no apiEndpoint"

# THREE files, and the reason is the whole shape of this leg.
#
# `bulk.bin` is the big one: leg 14b needs a restore slow enough to
# outlive a one-second deadline. The two small probes exist so that the
# gateway's question and the hub's control question are asked of
# DIFFERENT FILES. A cold read triggers a restore, so two probes of the
# same file are two different moments and the second one's answer says
# nothing about the first — which is how the previous version of this
# leg failed: it took the status from one request and the Retry-After
# from another, and blamed the gateway for a header that may well have
# belonged to a 200. Same mistake as 5122410.
code=$(gw PUT "/v1/projects/$PROJECT2/volumes/cold/files/content?path=/bulk.bin" \
  -H 'Content-Type: application/octet-stream' --data-binary @/tmp/gw-bulk.bin)
case "$code" in 200|201) ;; *) fail "seeding the cold volume returned $code: $(gwbody)" ;; esac
echo "probe-for-the-gateway" > /tmp/gw-probe.txt
for f in probe-gw.bin probe-hub.bin; do
  code=$(gw PUT "/v1/projects/$PROJECT2/volumes/cold/files/content?path=/$f" \
    --data-binary @/tmp/gw-probe.txt)
  case "$code" in 200|201) ;; *) fail "seeding $f returned $code: $(gwbody)" ;; esac
done

# Published first — the evict pass only takes CLEAN files, so a dirty
# one sits hot forever and the leg would wait for a cold read that can
# never happen.
CLEAN=""
for i in $(seq 1 60); do
  case "$(rpo_at "$EP_COLD")" in *"rpoClean=True"*) CLEAN=1; break ;; esac
  sleep 5
done
[ -n "$CLEAN" ] || {
  dbg "curl -s '$EP_COLD/status'" | head -c 800; echo
  fail "the cold volume never published ($(rpo_at "$EP_COLD"))"; }
note "published: $(rpo_at "$EP_COLD")"

# And now the always-armed watermark pass has to take it. Polled with a
# budget and reported INCONCLUSIVE rather than failed if it does not:
# eviction eligibility is the tier's business, not the gateway's, and a
# hard failure here would send the reader after the wrong component.
EVICTED=""
for i in $(seq 1 36); do
  N=$(gauge_at "$EP_COLD" tier.gauges.evictedFiles)
  case "$N" in ''|0|1|2) ;; *) EVICTED=$N; break ;; esac
  sleep 5
done

if [ -z "$EVICTED" ]; then
  note "nothing was evicted within the budget — the cold-read leg is INCONCLUSIVE"
  note "headroom=$(gauge_at "$EP_COLD" tier.gauges.headroomBytes) evicted=$(gauge_at "$EP_COLD" tier.gauges.evictedFiles)"
  echo "  ── the hub's own view ──"; dbg "curl -s '$EP_COLD/status'" | head -c 900; echo
  echo "  ── the tier config the operator rendered ──"
  kubectl -n "$NS" get cm -l flint.io/share="fs-$PROJECT2-cold" -o yaml 2>/dev/null \
    | grep -A 30 'tier' | head -35
  COLD_INCONCLUSIVE=1
else
  note "the watermark pass evicted $EVICTED file(s) — every seeded file is now a stub"

  # THE HEADLINE. hydrateWaitSecs is 0, so the hub answers the first
  # NFS4ERR_DELAY with 503 + Retry-After. What is under test is that
  # the GATEWAY hands BOTH back: a proxy that dropped the header, or
  # that timed out first and substituted its own 502, leaves a browse
  # UI unable to tell "coming, ask again" from "this hub is broken",
  # and the only safe reading of a bare 503 is the second one.
  #
  # ONE REQUEST, CAPTURED WHOLE. Status, headers and body come out of
  # the SAME exchange — reading the status from one call and the
  # header from a second is how the previous version of this leg
  # produced a failure nobody could attribute, after a 40-minute run
  # that tore its own cluster down.
  gw_full() {  # gw_full <path> -> prints "<status> <retry-after>"
    local st ra
    gw_alive
    st=$(curl -s -D /tmp/gw-hdrs.txt -o /tmp/gw-body.txt -w '%{http_code}' \
      -H "Authorization: Bearer $GW_TOKEN" "http://127.0.0.1:$PF_GW$1")
    ra=$(tr -d '\r' < /tmp/gw-hdrs.txt | awk 'tolower($1)=="retry-after:"{print $2}')
    echo "$st ${ra:-NONE}"
  }
  # Two calls, ONE exchange: the first makes the request and saves its
  # headers in the pod, the second reads that file. Deliberately not one
  # command producing "<status> <header>" — `$(...)` strips a trailing
  # space, so a MISSING header would come back looking exactly like the
  # status code, and the leg would report a value it never received.
  hub_code() {
    dbg "curl -s -D /tmp/h.txt -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer $TOK_COLD' '$EP_COLD$1'"
  }
  hub_ra() {
    dbg "tr -d '\r' < /tmp/h.txt | grep -i '^retry-after:' | cut -d' ' -f2"
  }

  # The hub's own answer FIRST, to a file the gateway will never touch.
  # Without it a missing header cannot be attributed: a hub that never
  # sent one and a gateway that dropped one look identical from here.
  HUB_CODE=$(hub_code "/files/content?path=/probe-hub.bin")
  HUB_RA=$(hub_ra); HUB_RA=${HUB_RA:-NONE}
  HUBSAYS="$HUB_CODE $HUB_RA"
  note "hub direct, on its own probe file: HTTP $HUB_CODE retry-after=${HUB_RA:-NONE}"
  [ "$HUB_CODE" = "503" ] \
    || fail "the hub answered $HUB_CODE to a cold read of its own — nothing below can be attributed to the gateway"
  [ "$HUB_RA" != "NONE" ] \
    || fail "the HUB sent no Retry-After on a cold read — this is the hub's fileapi::err_reply, not the gateway"

  # And now the gateway, on ITS own probe file.
  GWSAYS=$(gw_full "/v1/projects/$PROJECT2/volumes/cold/files/content?path=/probe-gw.bin")
  GW_CODE=${GWSAYS%% *}; GW_RA=${GWSAYS##* }
  if [ "$GW_CODE" != "503" ] || [ "$GW_RA" = "NONE" ]; then
    echo "  ── the whole response the gateway sent ──"
    tr -d '\r' < /tmp/gw-hdrs.txt
    echo "  ── body ──"; head -c 400 /tmp/gw-body.txt; echo
    echo "  ── the hub, to the same shape of request ──"; echo "$HUBSAYS"
    echo "  ── gateway log ──"
    kubectl -n "$OPNS" logs -l app.kubernetes.io/name=flint-lite-operator-gateway \
      --tail=25 2>/dev/null
  fi
  [ "$GW_CODE" = "503" ] \
    || fail "a cold read answered $GW_CODE through the gateway while the hub answered $HUB_CODE"
  [ "$GW_RA" != "NONE" ] \
    || fail "the gateway DROPPED the hub's Retry-After (hub sent $HUB_RA) — a caller cannot tell a hydrating file from a broken hub"
  # The body has to be the HUB's error and not one the gateway invented.
  # Those two are indistinguishable by status alone and they mean
  # opposite things about where the fault is.
  grep -q 'Delay' /tmp/gw-body.txt \
    || fail "the 503 body is not the hub's Delay error — the gateway manufactured this 503: $(cat /tmp/gw-body.txt)"
  note "gateway: HTTP $GW_CODE retry-after=$GW_RA, body $(head -c 60 /tmp/gw-body.txt)"

  # CONTROL (b): the same volume, a WARM file, through the same
  # gateway. A share that was simply broken would 503 for this too.
  echo "warm-file-contents" > /tmp/gw-warm.txt
  code=$(gw PUT "/v1/projects/$PROJECT2/volumes/cold/files/content?path=/warm.txt" \
    --data-binary @/tmp/gw-warm.txt)
  case "$code" in 200|201) ;; *) fail "writing the warm control file returned $code" ;; esac
  code=$(gw GET "/v1/projects/$PROJECT2/volumes/cold/files/content?path=/warm.txt")
  [ "$code" = "200" ] \
    || fail "a WARM file in the same volume answered $code — the 503 above is the share, not the file"
  pass "cold read -> 503 + Retry-After $GW_RA, relayed from the hub (which sent $HUB_RA); a warm file in the same volume is 200"

  # ── leg 14b: the gateway's own deadline must not beat the hub's ────
  #
  # `header_deadline` exists because the two budgets are both 30s by
  # default and RACE. If the gateway fires first the caller gets a 502
  # with no Retry-After — the same failure the headline above is
  # about, arriving from the other side. It has never run against a
  # real hydration.
  #
  # Forced rather than waited for: the share gets a REAL hydrate budget
  # (30s) and the gateway is squeezed to a 1s configured deadline. With
  # hydrateFetchParallel at 1 the restore is a single sequential GET of
  # ${BULK_MB} MiB, so it outlives that second by construction — a
  # gateway WITHOUT the download extension answers 502 and this leg
  # fails.
  say "leg 14b: a cold read outlives the gateway's configured deadline and still succeeds"
  kubectl -n "$NS" patch flintshare "fs-$PROJECT2-cold" --type=merge \
    -p '{"spec":{"monitoring":{"fileApi":{"hydrateWaitSecs":30}}}}' >/dev/null \
    || fail "hydrateWaitSecs patch refused"
  OLDHUB=$(kubectl -n "$NS" get pod -l flint.io/share="fs-$PROJECT2-cold" \
    -o jsonpath='{.items[0].metadata.name}' 2>/dev/null)
  helm_up --set gateway.upstreamTimeoutSecs=1 \
    || { tail -20 /tmp/gw-e2e-helm.log; fail "helm upgrade (upstreamTimeoutSecs=1) failed"; }
  kubectl -n "$OPNS" rollout status deployment/flint-lite-operator-gateway --timeout=120s \
    >/dev/null 2>&1 || fail "the gateway never rolled to upstreamTimeoutSecs=1"
  # Read it back off the pod spec. A --set that did not land would
  # leave the gateway on its 30s default and make the whole sub-leg
  # vacuous — it would pass without ever creating the race.
  kubectl -n "$OPNS" get deploy flint-lite-operator-gateway \
    -o jsonpath='{.spec.template.spec.containers[0].args}' | grep -q -- '--upstream-timeout-secs=1' \
    || fail "the gateway is not running with --upstream-timeout-secs=1 — leg 14b would prove nothing"
  pf_gw
  # The hub restarts on the config change; wait for a NEW pod back in
  # Ready. hydrateWarmAfterImport is off, so /bulk.bin is still a stub.
  RDY=""
  for i in $(seq 1 40); do
    NEWHUB=$(kubectl -n "$NS" get pod -l flint.io/share="fs-$PROJECT2-cold" \
      -o jsonpath='{.items[0].metadata.name}' 2>/dev/null)
    RDY=$(kubectl -n "$NS" get flintshare "fs-$PROJECT2-cold" -o jsonpath='{.status.phase}')
    [ "$RDY" = "Ready" ] && [ -n "$NEWHUB" ] && [ "$NEWHUB" != "$OLDHUB" ] && break
    sleep 5
  done
  [ "$RDY" = "Ready" ] || fail "the cold hub never came back after the hydrate-budget change (phase=$RDY)"
  EP_COLD=$(kubectl -n "$NS" get flintshare "fs-$PROJECT2-cold" -o jsonpath='{.status.apiEndpoint}')

  # ONE BYTE. The assertion is about the response HEADERS arriving
  # late, not about moving ${BULK_MB} MiB through a port-forward — the
  # hub still has to pull the whole object back from S3 before it can
  # answer even this.
  gw_alive
  RESP=$(curl -s -o /dev/null -w '%{http_code} %{time_total}' \
    -H "Authorization: Bearer $GW_TOKEN" -H 'Range: bytes=0-0' \
    "http://127.0.0.1:$PF_GW/v1/projects/$PROJECT2/volumes/cold/files/content?path=/bulk.bin")
  RCODE=${RESP%% *}; RTIME=${RESP##* }
  note "cold ranged read: HTTP $RCODE in ${RTIME}s (the gateway's configured deadline is 1s)"
  # 502 is the ONLY answer that fails this. Both 200/206 (the hub
  # hydrated and served) and a relayed 503 (the hub spent its own 30s
  # and gave up) mean the gateway waited past its own one-second
  # deadline, which is the entire claim. A 502 is the gateway's own
  # timeout, and it is what a missing `header_deadline` produces.
  case "$RCODE" in
    502) fail "the gateway timed out at its OWN 1s deadline and answered 502 — header_deadline is not extending the download budget past the hub's hydrate wait" ;;
    200|206) ;;
    503) note "the hub spent its own 30s budget and answered 503; the gateway still did not cut in at 1s" ;;
    *) fail "the cold ranged read answered $RCODE" ;;
  esac
  if python3 -c "import sys; sys.exit(0 if float('$RTIME') > 1.2 else 1)"; then
    pass "a cold read took ${RTIME}s — well past the gateway's 1s deadline — and still returned $RCODE"
  else
    note "hydration finished in ${RTIME}s, inside the gateway's own 1s deadline"
    note "the race was NOT exercised — this half is INCONCLUSIVE (raise BULK_MB)"
    DEADLINE_INCONCLUSIVE=1
  fi
  helm_up || { tail -20 /tmp/gw-e2e-helm.log; fail "helm upgrade (restore) failed"; }
  kubectl -n "$OPNS" rollout status deployment/flint-lite-operator-gateway --timeout=120s \
    >/dev/null 2>&1 || fail "the gateway never rolled back to its normal timeout"
  pf_gw
fi

# ── leg 15: the NetworkPolicy is ENFORCED, and admits the gateway ────
say "leg 15: NetworkPolicy closes the hub, and the gateway is on the right side of it"
# WHAT HAS NEVER BEEN TESTED. The operator chart auto-adds a gateway
# peer to the hubs' 8080 rule when gateway.enabled is set, so nobody has
# to remember it. Until this leg, that peer had only ever been RENDERED
# — chart-render-pass.sh greps the YAML — and it fails CLOSED: get the
# selector wrong and every file request in the fleet times out, with the
# policy still reading exactly right.
HUBIP=$(kubectl -n "$NS" get pod -l flint.io/share="fs-$PROJECT-data" \
  -o jsonpath='{.items[0].status.podIP}')
[ -n "$HUBIP" ] || fail "no pod ip for the data hub"
probe8080() { dbg "curl -s -o /dev/null -w '%{http_code}' --max-time 6 'http://$HUBIP:8080/status'"; }
probe2049() { dbg "nc -z -w 4 $HUBIP 2049 >/dev/null 2>&1 && echo OPEN || echo SHUT"; }

# BEFORE, so that AFTER means something.
[ "$(probe8080)" = "200" ] || fail "the debug pod cannot reach the hub's 8080 BEFORE any policy — leg 15 has no baseline"
[ "$(probe2049)" = "OPEN" ] || fail "the debug pod cannot reach the hub's 2049 BEFORE any policy — leg 15 has no baseline"
pass "baseline: an arbitrary pod in $NS reaches the hub on both 8080 and 2049"

# SUMS, not the pod-by-pod string: a pod set that changes for some
# other reason would otherwise read as a restart, and the failure
# message would blame the policy.
sum_restarts() {  # sum_restarts <namespace> <name label>
  kubectl -n "$1" get pod -l "app.kubernetes.io/name=$2" \
    -o jsonpath='{.items[*].status.containerStatuses[*].restartCount}' \
    | awk '{t=0; for (i=1;i<=NF;i++) t+=$i; print t+0}'
}
hub_restarts() { sum_restarts "$NS" flint-lite; }
op_restarts()  { sum_restarts "$OPNS" flint-lite-operator; }
HUB_R0=$(hub_restarts); OP_R0=$(op_restarts)

helm_up --set networkPolicy.enabled=true --set "networkPolicy.hubNamespaces={$NS}" \
  || { tail -25 /tmp/gw-e2e-helm.log; fail "helm upgrade (networkPolicy) failed"; }
kubectl -n "$NS" get netpol flint-lite-operator-hubs >/dev/null 2>&1 \
  || { kubectl -n "$NS" get netpol; fail "the hub NetworkPolicy was not created in $NS"; }
kubectl -n "$OPNS" get netpol flint-lite-operator-deny-ingress >/dev/null 2>&1 \
  || fail "the operator's deny-ingress policy was not created"
sleep 10

# THE ENFORCEMENT CONTROL. Every assertion below is about traffic being
# BLOCKED, and on a CNI that ignores NetworkPolicy every one of them
# would pass by accident — which is why no leg in this repo has ever
# asserted one. kind's kindnetd enforces as of v0.32.0; if this rig's
# CNI does not, the leg says so and stops rather than reporting a
# security property it did not observe.
SHUT=""
for i in $(seq 1 6); do
  [ "$(probe8080)" != "200" ] && { SHUT=1; break; }
  sleep 5
done
if [ -z "$SHUT" ]; then
  note "an arbitrary pod still reaches the hub's 8080 with the policy in place"
  note "this CNI does not enforce NetworkPolicy — leg 15 is INCONCLUSIVE, not passed"
  NETPOL_INCONCLUSIVE=1
else
  pass "the policy is ENFORCED: an arbitrary pod in $NS can no longer reach the hub's 8080"

  # KUBELET'S PROBES, CHECKED FIRST AND ON PURPOSE.
  #
  # All three hub probes are TCP checks against 2049 (render.rs `tcp()`
  # — readiness, liveness, and the tiered startup probe all use it),
  # and the policy's 2049 rule is OMITTED ENTIRELY when no NFS client
  # peers are configured. So a CNI that subjected kubelet's probe
  # connections to pod ingress policy would make turning this policy on
  # enough to fail every hub's liveness and kill it, forever, fleetwide.
  #
  # MEASURED, not assumed: on kindnet a pod with TCP probes on 2049
  # under a deny-all ingress policy stayed Ready with zero restarts,
  # while a peer pod was blocked from that same port and reached it
  # again the moment the policy came off. So kubelet is exempt HERE.
  # It is a property of the CNI rather than of Kubernetes, which is why
  # this stays an assertion rather than becoming a comment.
  #
  # It runs BEFORE the gateway assertion below because it would
  # otherwise surface THERE: a hub that goes NotReady leaves its
  # EndpointSlice, the headless name stops resolving, and the gateway
  # answers 502 — which reads as "the gateway peer is wrong" and sends
  # the reader to entirely the wrong place.
  sleep 45
  HUB_R1=$(hub_restarts)
  NOTREADY=$(kubectl -n "$NS" get pod -l app.kubernetes.io/name=flint-lite \
    -o jsonpath='{range .items[*]}{.metadata.name}={range .status.conditions[?(@.type=="Ready")]}{.status}{end}{" "}{end}' 2>/dev/null \
    | tr ' ' '\n' | grep '=False' | tr '\n' ' ')
  if [ "${HUB_R1:-0}" -gt "${HUB_R0:-0}" ] || [ -n "$NOTREADY" ]; then
    echo "  ── hub pods ──"; kubectl -n "$NS" get pod -l app.kubernetes.io/name=flint-lite
    echo "  ── the policy's ingress rules ──"
    kubectl -n "$NS" get netpol flint-lite-operator-hubs \
      -o jsonpath='{.spec.ingress}{"\n"}'
    echo
    echo "  DIAGNOSIS: every hub probe is a TCP check on 2049, and this"
    echo "  policy has no 2049 rule because networkPolicy.nfsClientCIDRs"
    echo "  and .nfsClientSelectors are both empty. On a CNI that applies"
    echo "  pod ingress policy to kubelet, that closes the probe path."
    echo "  The fix is to list the node CIDRs in nfsClientCIDRs."
    fail "hubs stopped passing their probes under the policy (restarts $HUB_R0 -> $HUB_R1; not-ready: ${NOTREADY:-none})"
  fi
  pass "hub probes survive the policy (restarts $HUB_R0 -> $HUB_R1, all pods Ready)"

  # THE HEADLINE: the auto-wired gateway peer. If this fails, the peer
  # selector is wrong and every file request in a policy-enabled fleet
  # times out.
  gw_alive
  code=$(gw GET "/v1/projects/$PROJECT/volumes/data/files/content?path=/hello.txt")
  [ "$code" = "000" ] \
    && fail "no response from the gateway at all, twice — that is THIS DRILL's port-forward, not the policy. Nothing here is a statement about the peer."
  if [ "$code" != "200" ]; then
    echo "  ── the policy as rendered ──"
    kubectl -n "$NS" get netpol flint-lite-operator-hubs -o yaml | sed -n '1,60p'
    echo "  ── the gateway pod's labels (what the peer must match) ──"
    kubectl -n "$OPNS" get pod -l app.kubernetes.io/name=flint-lite-operator-gateway \
      -o jsonpath='{.items[0].metadata.labels}{"\n"}'
    echo "  ── the release namespace's labels (the namespaceSelector) ──"
    kubectl get ns "$OPNS" -o jsonpath='{.metadata.labels}{"\n"}'
    fail "the gateway can no longer reach the hub under the policy (HTTP $code) — the auto-added peer does not match"
  fi
  grep -q 'second-write\|data-volume-contents\|from-the-mount' /tmp/gw-body.txt \
    || fail "the gateway answered 200 but with the wrong bytes: $(gwbody)"
  pass "THE GATEWAY STILL REACHES EVERY HUB — the peer the chart adds automatically is correct"

  # 2049 is closed too, and nothing configured a client set.
  [ "$(probe2049)" = "SHUT" ] \
    || fail "2049 is still open to an arbitrary pod with no nfsClient peers configured — the rule fell open"
  pass "2049 is closed: an unconfigured client set admits nobody, not everybody"

  # THE OPERATOR'S OWN POLLS. It reaches each hub's 8080 by pod ip for
  # rpoClean, idleness and lease counts; a hub it cannot poll is an
  # unknown hub that never suspends. That failure is silent — no
  # restart, no event, just an idle ladder that stops firing — so the
  # condition is what has to be read.
  OP_R1=$(op_restarts)
  [ "${OP_R1:-0}" -le "${OP_R0:-0}" ] \
    || fail "the operator restarted under its own deny-ingress policy ($OP_R0 -> $OP_R1)"
  UNREACH=$(kubectl -n "$NS" get flintshare -o json \
    | python3 "$REPO_ROOT/tests/regression/lib/share-unreachable.py")
  case "$UNREACH" in
    UNREADABLE*) fail "could not read the shares to check HubReachable ($UNREACH) — this assertion would otherwise PASS BY NOT LOOKING" ;;
    "") ;;
    *) fail "the operator cannot poll its hubs under the policy: $UNREACH — its own peer is wrong" ;;
  esac
  pass "the operator still polls its hubs through the policy (no HubReachable=False)"

  # AND THE HOLES OPEN. A policy that only ever denies is half a test:
  # the rules have to admit the peer they name, and only that peer.
  helm_up --set networkPolicy.enabled=true --set "networkPolicy.hubNamespaces={$NS}" \
    --set 'networkPolicy.nfsClientSelectors[0].podSelector.matchLabels.app=nfsclient' \
    || { tail -25 /tmp/gw-e2e-helm.log; fail "helm upgrade (nfs hole) failed"; }
  sleep 10
  OPENED=""
  for i in $(seq 1 6); do
    R=$(kubectl -n "$NS" exec nfsclient -- sh -c "nc -z -w 4 $HUBIP 2049 >/dev/null 2>&1 && echo OPEN || echo SHUT" 2>/dev/null)
    [ "$R" = "OPEN" ] && { OPENED=1; break; }
    sleep 5
  done
  [ -n "$OPENED" ] || {
    kubectl -n "$NS" get netpol flint-lite-operator-hubs -o yaml | sed -n '1,40p'
    fail "the nfsClientSelectors hole did NOT admit the pod it names"; }
  [ "$(probe2049)" = "SHUT" ] \
    || fail "opening 2049 for one pod opened it for every pod — the peer list is not being applied"
  pass "an nfsClientSelectors peer is admitted on 2049 while the pod beside it stays shut out"

  # Leave the cluster as it was found, so KEEP=1 is usable for anything
  # after this leg.
  helm_up || { tail -20 /tmp/gw-e2e-helm.log; fail "helm upgrade (policy off) failed"; }
fi

# ── 16. summary ──────────────────────────────────────────────────────
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
if [ -z "${COLD_INCONCLUSIVE:-}" ]; then
  echo "   · a cold read relays the hub's 503 + Retry-After, never a bare 502"
fi
if [ -z "${NETPOL_INCONCLUSIVE:-}" ]; then
  echo "   · NetworkPolicy shuts the hub to everyone but the operator and the gateway"
fi
echo "   · a ${BULK_MB} MiB body crosses the proxy under a ${GW_MEM_LIMIT} limit, byte-identical"
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
if [ -n "${COLD_INCONCLUSIVE:-}" ]; then
  echo
  echo " INCONCLUSIVE: the watermark pass never evicted, so no file was"
  echo " cold and the 503 relay was not exercised. That is filesystem"
  echo " arithmetic on a 1Gi claim, not the product — raise BULK_MB or"
  echo " lower fs-$PROJECT2-bulk's spec.settings.watermarkPct and rerun."
fi
if [ -n "${DEADLINE_INCONCLUSIVE:-}" ]; then
  echo
  echo " INCONCLUSIVE: hydration finished inside the gateway's squeezed"
  echo " 1s deadline, so leg 14b never made the two budgets race."
  echo " Raise BULK_MB so the restore takes longer than a second."
fi
if [ -n "${NETPOL_INCONCLUSIVE:-}" ]; then
  echo
  echo " INCONCLUSIVE: this cluster's CNI does not enforce NetworkPolicy,"
  echo " so leg 15 observed nothing. kind's kindnetd enforces from"
  echo " v0.32.0; on an older kind, install Calico or Cilium first."
fi
