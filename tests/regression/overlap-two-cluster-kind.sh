#!/usr/bin/env bash
# Bucket-subtree overlap: what the fence withholds, and what nothing fences.
#
# The operator refuses two shares on one bucket subtree. Two things about
# that refusal had never been drilled against a real bucket:
#
#   1. WHAT THE REFUSAL DOES TO A RUNNING HUB. Scaling a loser to zero is
#      an ORDINARY termination — SIGTERM runs the hub's graceful shutdown,
#      which drains, ticks the flush orchestrator one last time, writes a
#      manifest barrier and releases the epoch. For a loser those publishes
#      land in the WINNER's subtree. The operator kind e2e proves the pod
#      is killed; it CANNOT prove bytes were withheld, because its bucket
#      is fictional. This drill proves it, at the bucket.
#
#   2. WHAT HAPPENS ACROSS TWO API SERVERS. `conflict::admit` reads ONE
#      reflector. Two clusters on one bucket never contend at the operator,
#      and for NESTED prefixes they never contend at the store either:
#      `epoch_key()` is `<prefix>.flint/epoch`, so `n/` and `n/inner/` mint
#      DIFFERENT objects. Two live leases, no fence anywhere, no error.
#
# The oracle throughout is the EPOCH CELL, not object counts. A graceful
# shutdown CAS-writes `released: true` and rewrites the manifest; a SIGKILL
# cannot. That makes "was the graceful path taken?" a deterministic
# question about bucket state rather than a race with a flush timer.
#
# Legs:
#   1  a tiered share publishes for real; a nested share in the same
#      cluster is refused, gets no Deployment, claims no epoch, and
#      carries a machine-readable status.conflictWith redirect.
#   2  KNOWN LIMITATION, asserted as it currently behaves: a share
#      demoted WHILE RUNNING is stopped, but its shutdown is GRACEFUL,
#      so it still runs its epilogue and releases its epoch inside the
#      winner's subtree. No operator can prevent this; S12 is the fix.
#   3  two clusters, NESTED prefixes: both hubs reach Ready and BOTH
#      hold a live epoch at once. CONTROL: two clusters on the SAME
#      prefix — exactly one serves.
#   4  the outer hub does NOT ingest the inner share's control objects.
#      ANTI-VACUITY: it DOES ingest an ordinary file the inner wrote.
#   5  KNOWN BUG, asserted as it currently behaves: a new project on a
#      REUSED prefix silently serves the previous project's data.
#      CONTROL: a project on a fresh prefix serves nothing.
#
# One MinIO, shared by both clusters over the docker network, so the two
# API servers are genuinely independent while the bucket is genuinely one.
#
# KEEP=1 leaves both clusters and MinIO standing.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OP_CHART="$REPO_ROOT/flint-lite-operator-chart"
CARGO_DIR="$REPO_ROOT/spdk-csi-driver"

CA="${CA:-flint-ov-a}"
CB="${CB:-flint-ov-b}"
NS=overlap
OPNS=flint-system
HUBIMG=flint-lite-dev:local
OPIMG=flint-lite-operator-dev:local
MINIO_CT=flint-ov-minio
BUCKET=flint-overlap
MINIO_USER=flintdrill
MINIO_PASS=flintdrill123
MINIO_HOSTPORT=39200
TOKEN=overlap-drill-token
KUBECONFIG_FILE="$(mktemp -t flint-ov-kubeconfig.XXXXXX)"
export KUBECONFIG="$KUBECONFIG_FILE"

say()  { echo; echo "── $*"; }
pass() { echo "  ✓ $*"; }
fail() { echo "  ✗ FAIL: $*"; exit 1; }
note() { echo "    $*"; }

ka() { kubectl --context "kind-$CA" "$@"; }
kb() { kubectl --context "kind-$CB" "$@"; }

s3() {
  env -u AWS_PROFILE \
    AWS_ACCESS_KEY_ID=$MINIO_USER AWS_SECRET_ACCESS_KEY=$MINIO_PASS \
    AWS_DEFAULT_REGION=us-east-1 AWS_EC2_METADATA_DISABLED=true \
    aws --endpoint-url "http://127.0.0.1:$MINIO_HOSTPORT" "$@"
}
# The epoch cell as text, and its `released` mark. A clean shutdown sets
# it; a SIGKILL cannot.
epoch_body() { s3 s3 cp "s3://$BUCKET/$1.flint/epoch" - 2>/dev/null; }
released_of() {
  local b; b=$(epoch_body "$1")
  case "$b" in
    *'"released":true'*) echo true ;;
    "")                  echo missing ;;
    *)                   echo false ;;
  esac
}
etag_of() { s3 s3api head-object --bucket "$BUCKET" --key "$1" \
              --query ETag --output text 2>/dev/null; }

cleanup() {
  set +e
  if [ "${KEEP:-0}" = "1" ]; then
    echo "KEEP=1 — clusters and MinIO left standing (kubeconfig: $KUBECONFIG_FILE)"; return
  fi
  kind delete cluster --name "$CA" >/dev/null 2>&1
  kind delete cluster --name "$CB" >/dev/null 2>&1
  docker rm -f "$MINIO_CT" >/dev/null 2>&1
  rm -f "$KUBECONFIG_FILE"
}
trap cleanup EXIT

echo "══════════════════════════════════════════════════════════════════"
echo " bucket-subtree overlap — one bucket, two clusters, a real fence"
echo "══════════════════════════════════════════════════════════════════"

for t in kind kubectl helm docker aws; do
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
   --bin flint-pnfs-mds --bin flint-lite-operator >/tmp/ov-build.log 2>&1) \
  || { tail -20 /tmp/ov-build.log; fail "zigbuild failed"; }
IMGDIR=$(mktemp -d -t flint-ov-img.XXXXXX)
cp "$CARGO_DIR/target/$TRIPLE/release/flint-pnfs-mds"     "$IMGDIR/"
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
  >/tmp/ov-img.log 2>&1 || { tail -5 /tmp/ov-img.log; fail "hub image build failed"; }
docker build --platform "$PLATFORM" -f "$IMGDIR/Dockerfile.op" -t "$OPIMG" "$IMGDIR" \
  >>/tmp/ov-img.log 2>&1 || { tail -5 /tmp/ov-img.log; fail "operator image build failed"; }
rm -rf "$IMGDIR"
pass "images built ($PLATFORM)"

say "two kind clusters and ONE MinIO on the shared docker network"
kind delete cluster --name "$CA" >/dev/null 2>&1
kind delete cluster --name "$CB" >/dev/null 2>&1
docker rm -f "$MINIO_CT" >/dev/null 2>&1
kind create cluster --name "$CA" --wait 120s >/dev/null 2>&1 || fail "cluster $CA never came up"
kind create cluster --name "$CB" --wait 120s >/dev/null 2>&1 || fail "cluster $CB never came up"
# MinIO lives OUTSIDE both clusters, on the network kind put them on, so
# neither API server has any view of the other's shares while both hubs
# write to one bucket. That is the condition being tested.
docker run -d --name "$MINIO_CT" --network kind \
  -p "$MINIO_HOSTPORT:9000" \
  -e "MINIO_ROOT_USER=$MINIO_USER" -e "MINIO_ROOT_PASSWORD=$MINIO_PASS" \
  quay.io/minio/minio server /data >/dev/null 2>&1 \
  || fail "could not start MinIO"
MINIO_IP=$(docker inspect -f '{{.NetworkSettings.Networks.kind.IPAddress}}' "$MINIO_CT")
[ -n "$MINIO_IP" ] || fail "MinIO has no address on the kind network"
EP="http://$MINIO_IP:9000"
for _ in $(seq 1 40); do
  curl -sf "http://127.0.0.1:$MINIO_HOSTPORT/minio/health/live" >/dev/null && break
  sleep 1
done
curl -sf "http://127.0.0.1:$MINIO_HOSTPORT/minio/health/live" >/dev/null \
  || fail "MinIO never became live"
s3 s3 mb "s3://$BUCKET" >/dev/null 2>&1 || fail "bucket create failed"
# Versioning ON: the CRD requires it, and the tier's conditional writes
# depend on it.
s3 s3api put-bucket-versioning --bucket "$BUCKET" \
  --versioning-configuration Status=Enabled >/dev/null 2>&1 \
  || fail "could not enable bucket versioning"
pass "clusters $CA and $CB up; MinIO at $EP (host port $MINIO_HOSTPORT), bucket $BUCKET"

say "operator + credentials in both clusters"
for C in "$CA" "$CB"; do
  for i in "$HUBIMG" "$OPIMG"; do
    kind load docker-image "$i" --name "$C" >/dev/null 2>&1 || fail "kind load $i into $C failed"
  done
  kubectl --context "kind-$C" create namespace "$NS" >/dev/null 2>&1
  # Keys VERBATIM — any other spelling makes the hub fall back to IMDS
  # and fail with "bucket unreachable: dispatch failure", which names the
  # bucket rather than the cause.
  kubectl --context "kind-$C" -n "$NS" create secret generic s3creds \
    --from-literal=AWS_ACCESS_KEY_ID=$MINIO_USER \
    --from-literal=AWS_SECRET_ACCESS_KEY=$MINIO_PASS >/dev/null \
    || fail "s3creds in $C failed"
  kubectl --context "kind-$C" -n "$NS" create secret generic api-token \
    --from-literal=token="$TOKEN" >/dev/null || fail "api-token in $C failed"
  helm --kube-context "kind-$C" install flint-lite-operator "$OP_CHART" \
    -n "$OPNS" --create-namespace \
    --set image.ref="$OPIMG" --set hubImage="$HUBIMG" \
    >/tmp/ov-helm-$C.log 2>&1 || { tail -20 /tmp/ov-helm-$C.log; fail "operator install in $C failed"; }
  kubectl --context "kind-$C" -n "$OPNS" rollout status deployment/flint-lite-operator \
    --timeout=180s >/dev/null 2>&1 || fail "operator in $C never became Ready"
done
STAMP=$(ka get crd flintshares.flint.io \
  -o jsonpath='{.metadata.annotations.flint\.io/crd-schema-version}')
[ -n "$STAMP" ] || fail "the CRD carries no schema-version annotation"
pass "both operators Ready; CRD stamped at schema $STAMP"

# ── helpers that need the cluster ────────────────────────────────────
mk_share() {  # $1 ctx-fn, $2 name, $3 prefix, $4 endpoint
  local kc=$1 name=$2 prefix=$3 ep=$4
  $kc apply -f - >/dev/null <<EOF || fail "applying $name failed"
apiVersion: flint.io/v1alpha1
kind: FlintShare
metadata: { name: $name, namespace: $NS }
spec:
  bucket: $BUCKET
  keyPrefix: $prefix
  endpoint: $ep
  region: us-east-1
  credentialsSecretRef: s3creds
  settings:
    flushFloorSecs: 3
  persistence: { size: 1Gi }
  monitoring:
    enabled: true
    port: 8080
    fileApi: { enabled: true, tokenSecretRef: api-token }
EOF
}
phase_in() { $1 -n "$NS" get flintshare "$2" -o jsonpath='{.status.phase}' 2>/dev/null; }
wait_phase_in() {  # $1 ctx-fn, $2 name, $3 want, $4 secs
  local kc=$1 name=$2 want=$3 secs=${4:-180} seen
  for _ in $(seq 1 "$secs"); do
    seen=$(phase_in "$kc" "$name")
    [ "$seen" = "$want" ] && { pass "$name reached $want"; return 0; }
    sleep 1
  done
  $kc -n "$NS" get flintshare "$name" -o yaml | tail -25
  local pod
  pod=$($kc -n "$NS" get pods -l "flint.io/share=$name" -o name 2>/dev/null | head -1)
  if [ -n "$pod" ]; then
    echo "    --- $pod ---"
    $kc -n "$NS" logs "$pod" --tail=40 2>&1 | sed 's/^/    /'
  else
    echo "    (no pod at all for $name)"
  fi
  fail "$name never reached $want (last: '${seen:-<none>}')"
}
hubpod_in() {  # resolve EVERY time: a rolled hub invalidates a captured name
  $1 -n "$NS" get pods -l "flint.io/share=$2" --field-selector=status.phase=Running \
    -o jsonpath='{.items[?(@.status.containerStatuses[0].ready==true)].metadata.name}' \
    2>/dev/null | awk '{print $1}'
}
put_file() {  # $1 ctx-fn, $2 share, $3 path, $4 content
  local pod; pod=$(hubpod_in "$1" "$2")
  [ -n "$pod" ] || fail "no ready hub pod for $2"
  printf '%s' "$4" | $1 -n "$NS" exec -i "$pod" -- sh -c \
    "curl -sf -X PUT -H 'Authorization: Bearer $TOKEN' --data-binary @- \
     'http://127.0.0.1:8080/files/content?path=$3'" >/dev/null \
    || fail "PUT $3 into $2 failed"
}
get_file() {  # $1 ctx-fn, $2 share, $3 path -> body on stdout, empty if absent
  local pod; pod=$(hubpod_in "$1" "$2")
  [ -n "$pod" ] || return 1
  $1 -n "$NS" exec "$pod" -- sh -c \
    "curl -sf -H 'Authorization: Bearer $TOKEN' \
     'http://127.0.0.1:8080/files/content?path=$3'" 2>/dev/null | tr -d '\r'
}
# The epoch release is GATED ON rpoClean (`server.rs:1311`): a hub with
# unpublished work deliberately leaves the cell HELD, because a released
# cell would let the next claimant serve exactly that gap. So every leg
# whose oracle is "did it release?" must first establish that a graceful
# shutdown WOULD have released — otherwise the assertion passes on a hub
# that was never going to release anyway.
wait_rpo_clean() {  # $1 ctx-fn, $2 share, $3 secs
  local kc=$1 name=$2 secs=${3:-90} pod v
  for _ in $(seq 1 "$secs"); do
    pod=$(hubpod_in "$kc" "$name")
    if [ -n "$pod" ]; then
      v=$($kc -n "$NS" exec "$pod" -- sh -c \
            'curl -sf --max-time 5 http://127.0.0.1:8080/status' 2>/dev/null \
          | tr -d '\r' | sed -n 's/.*"rpoClean":\([a-z]*\).*/\1/p')
      [ "$v" = "true" ] && return 0
    fi
    sleep 2
  done
  note "last rpoClean for $name: '${v:-<none>}'"
  return 1
}

wait_object() {  # $1 key, $2 secs
  for _ in $(seq 1 "${2:-60}"); do
    s3 s3api head-object --bucket "$BUCKET" --key "$1" >/dev/null 2>&1 && return 0
    sleep 2
  done
  return 1
}
reasons_in() { $1 -n "$NS" get events --field-selector "involvedObject.name=$2" \
                 -o jsonpath='{.items[*].reason}' 2>/dev/null; }

# ── leg 1 ────────────────────────────────────────────────────────────
say "leg 1: a tiered share publishes for real; a nested share is refused"
mk_share ka owner "t/" "$EP"
wait_phase_in ka owner Ready 180
put_file ka owner "hello.txt" "owner-data-1"
wait_object "t/hello.txt" 60 || fail "the winner never published t/hello.txt — the tier path is not working, so nothing below would mean anything"
pass "the winner published t/hello.txt to the real bucket"
wait_object "t/.flint/epoch" 30 || fail "the winner never wrote an epoch cell"

sleep 3   # creationTimestamp is 1s-granular; make the winner unambiguous
mk_share ka nested "t/nested/" "$EP"
wait_phase_in ka nested Failed 120
ka -n "$NS" get deployment nested >/dev/null 2>&1 \
  && fail "the loser got a Deployment — that is the hub that takes the prefix over"
CWNAME=$(ka -n "$NS" get flintshare nested -o jsonpath='{.status.conflictWith.name}')
CWREL=$(ka -n "$NS" get flintshare nested -o jsonpath='{.status.conflictWith.relation}')
CWSUB=$(ka -n "$NS" get flintshare nested -o jsonpath='{.status.conflictWith.subPath}')
CWADDR=$(ka -n "$NS" get flintshare nested -o jsonpath='{.status.conflictWith.address}')
[ "$CWNAME" = "owner" ]    || fail "conflictWith.name is '$CWNAME'"
[ "$CWREL" = "Ancestor" ]  || fail "conflictWith.relation is '$CWREL'"
[ "$CWSUB" = "nested/" ]   || fail "conflictWith.subPath is '$CWSUB'"
# Same namespace, so the address IS disclosed here — the withholding case
# is cross-namespace and is asserted in the operator kind e2e.
[ -n "$CWADDR" ] || fail "conflictWith.address is empty for a SAME-namespace winner"
s3 s3api head-object --bucket "$BUCKET" --key "t/nested/.flint/epoch" >/dev/null 2>&1 \
  && fail "the refused share still claimed an epoch under the winner's subtree"
pass "loser Failed, no Deployment, no epoch; conflictWith redirects to $CWNAME + $CWSUB at $CWADDR"
ka -n "$NS" delete flintshare nested --wait=true >/dev/null 2>&1

# ── leg 2 — KNOWN LIMITATION: the demoted hub still publishes ────────
# The operator scales a conflict loser to zero. That is an ORDINARY
# termination, so SIGTERM runs the hub's graceful shutdown — drain, a
# final flush tick, a manifest barrier, an epoch release — and for a
# loser those publishes land in the WINNER's subtree.
#
# A force-delete was tried here and REVERTED. It does not SIGKILL:
# `--grace-period=0 --force` removes the pod object from the API without
# waiting for the kubelet, which then stops the container through its
# normal path, SIGTERM first. Measured on this rig it bought ~2 SECONDS
# (fenced 0s vs ungated 2s from Failed to pod-gone), because a clean hub
# finishes its epilogue in ~370ms and releases either way. Not worth a
# cluster-wide pods/delete grant, and it invited the belief that the
# hole was closed.
#
# So this leg asserts the limitation AS IT CURRENTLY IS, the same way
# leg 6 does for prefix reuse. When S12 lands — the hub probing its
# ancestor chain before publishing — this leg FAILS, and that is the
# signal to rewrite it as a refusal.
say "leg 2: KNOWN LIMITATION — a demoted hub still runs its epilogue"
mk_share ka alpha "p/" "http://10.255.255.1:9000"
sleep 3
mk_share ka beta "p/sub/" "$EP"
wait_phase_in ka beta Ready 240
put_file ka beta "b.txt" "beta-data"
wait_object "p/sub/b.txt" 90 || fail "beta never published — it must be capable of publishing for this leg to mean anything"
wait_rpo_clean ka beta 90 \
  || fail "beta never reached rpoClean — a dirty hub holds its epoch anyway, so the release below would not be the epilogue's doing"
[ "$(released_of p/sub/)" = "false" ] || fail "beta's epoch says released while it is running"
pass "beta is Ready and clean, publishing into p/sub/ — inside alpha's p/"

ka -n "$NS" patch flintshare alpha --type=merge \
  -p "{\"spec\":{\"endpoint\":\"$EP\"}}" >/dev/null || fail "converging the endpoint was refused"
wait_phase_in ka beta Failed 180
for _ in $(seq 1 40); do
  L=$(ka -n "$NS" get pods -l flint.io/share=beta -o name 2>/dev/null | grep -c .)
  [ "${L:-1}" = "0" ] && break
  sleep 2
done
[ "${L:-1}" = "0" ] || fail "the losing hub pod is still running"
W_REPL=$(ka -n "$NS" get deployment alpha -o jsonpath='{.spec.replicas}' 2>/dev/null)
[ "${W_REPL:-0}" = "1" ] \
  || fail "the WINNER was scaled down too (replicas=${W_REPL:-<none>}) — demotion is not conditional on losing"
pass "the losing hub was stopped; the winner untouched"

REL=""
for _ in $(seq 1 40); do
  REL=$(released_of p/sub/); [ "$REL" = "true" ] && break; sleep 2
done
if [ "$REL" = "true" ]; then
  pass "REPRODUCED: the demoted hub ran its shutdown epilogue and released its epoch inside the winner's subtree"
  note "expected — an operator can neither compel a SIGKILL nor fence the store. S12 (ancestor-chain probe in the HUB) is the fix."
else
  fail "the demoted hub did NOT complete its epilogue (released=$REL). If S12 landed, this leg is stale and should assert the refusal instead."
fi

# ── leg 3 — TWO CLUSTERS ─────────────────────────────────────────────
say "leg 3: nested prefixes in TWO clusters — two live epochs, no fence"
mk_share ka outer "n/" "$EP"
wait_phase_in ka outer Ready 180
mk_share kb inner "n/inner/" "$EP"
wait_phase_in kb inner Ready 180
# Neither operator can see the other's share, and the store cannot help:
# the two prefixes mint DIFFERENT epoch keys.
[ "$(phase_in ka outer)" = "Ready" ] || fail "outer left Ready while inner came up"
E_OUT=$(released_of n/); E_IN=$(released_of n/inner/)
[ "$E_OUT" = "false" ] || fail "the outer hub holds no epoch (released=$E_OUT)"
[ "$E_IN"  = "false" ] || fail "the inner hub holds no epoch (released=$E_IN)"
pass "BOTH hubs serve and BOTH hold a live epoch — nested prefixes never contend, anywhere"

# CONTROL: the same two clusters on the SAME prefix. Here the store DOES
# fence, and exactly one hub serves. Without this the leg above would not
# distinguish "nesting defeats the fence" from "there is no fence at all".
mk_share kb rival "t/" "$EP"
SERVING=0
for _ in $(seq 1 90); do
  [ "$(phase_in kb rival)" = "Ready" ] && { SERVING=1; break; }
  sleep 2
done
[ "$SERVING" = "0" ] \
  || fail "a second cluster's hub on the SAME prefix reached Ready while the first holds the epoch — the store-side lease is not fencing at all"
RP=$(phase_in kb rival)
pass "on the SAME prefix the rival never serves (phase $RP) — the epoch cell fences it, as designed"
kb -n "$NS" delete flintshare rival --wait=true >/dev/null 2>&1

# ── leg 4 — .flint AT ANY DEPTH ──────────────────────────────────────
say "leg 4: the outer hub does not ingest the inner share's control objects"
put_file kb inner "payload.txt" "inner-payload"
wait_object "n/inner/payload.txt" 60 || fail "the inner hub never published payload.txt"
wait_object "n/inner/.flint/epoch" 30 || fail "the inner hub has no control objects to be confused by"
# Force the outer hub to import from scratch: import runs when tier state
# is FRESH, so it needs a new claim, not a restart.
ka -n "$NS" delete flintshare outer --wait=true >/dev/null 2>&1
ka -n "$NS" delete pvc outer-data --wait=true >/dev/null 2>&1
mk_share ka outer "n/" "$EP"
wait_phase_in ka outer Ready 240
OPOD=""
for _ in $(seq 1 60); do OPOD=$(hubpod_in ka outer); [ -n "$OPOD" ] && break; sleep 2; done
[ -n "$OPOD" ] || fail "no ready outer hub pod"
LISTING=$(ka -n "$NS" exec "$OPOD" -- sh -c 'ls -R /data/exports 2>/dev/null' | tr '\n' ' ')
note "outer export: $LISTING"
# ANTI-VACUITY: the import must actually have run.
case "$LISTING" in
  *payload.txt*) ;;
  *) fail "the outer hub did not import the inner hub's ordinary file — import did not run, so the assertion below is vacuous" ;;
esac
case "$LISTING" in
  *.flint*) fail "the outer hub INGESTED the inner share's control namespace — a client write there republishes over another hub's live epoch cell" ;;
esac
pass "ordinary file imported, .flint/ refused at depth — the outer hub cannot shadow the inner's control objects"

# ── leg 5 — KNOWN BUG: PREFIX REUSE ──────────────────────────────────
say "leg 5: KNOWN BUG — a new project on a reused prefix serves the old project's data"
# Asserted AS IT CURRENTLY BEHAVES, deliberately. When identity lands in
# <prefix>.flint/ and the importer refuses a foreign owner, this leg
# FAILS and is the signal to update it.
mk_share ka proja "reuse/" "$EP"
wait_phase_in ka proja Ready 180
put_file ka proja "secret.txt" "project-A-private-data"
wait_object "reuse/secret.txt" 60 || fail "project A never published its marker"
ka -n "$NS" delete flintshare proja --wait=true >/dev/null 2>&1
ka -n "$NS" delete pvc proja-data --wait=true >/dev/null 2>&1
s3 s3api head-object --bucket "$BUCKET" --key "reuse/secret.txt" >/dev/null 2>&1 \
  || fail "deleting the CR removed the bucket data — that is not the behaviour under test"
note "project A is gone; its bytes are still in the bucket (the operator never touches it)"

mk_share ka projb "reuse/" "$EP"
wait_phase_in ka projb Ready 240
LEAKED=""
for _ in $(seq 1 30); do
  LEAKED=$(get_file ka projb "secret.txt"); [ -n "$LEAKED" ] && break; sleep 2
done
# CONTROL: a project on a FRESH prefix sees nothing, so the leak above is
# the reuse and not the drill handing it the file.
mk_share ka projc "fresh/" "$EP"
wait_phase_in ka projc Ready 180
CLEAN=$(get_file ka projc "secret.txt")
[ -z "$CLEAN" ] || fail "a project on a FRESH prefix also served secret.txt — the control is broken"
if [ "$LEAKED" = "project-A-private-data" ]; then
  pass "REPRODUCED: project B on the reused prefix serves project A's bytes verbatim (control on a fresh prefix serves nothing)"
  note "expected — the bucket carries no owner identity and importOnStart defaults true"
else
  fail "project B did NOT adopt the old data (got '${LEAKED:-<empty>}'). If identity landed in .flint/, this leg is now stale and should assert the refusal instead."
fi

echo
echo "══════════════════════════════════════════════════════════════════"
echo " PASS — a nested share is refused with a machine-readable redirect;"
echo " a share demoted while RUNNING is stopped without touching the"
echo " winner, but STILL runs its epilogue into the winner's subtree"
echo " (the known limitation an operator cannot close); two clusters on"
echo " NESTED prefixes hold two live epochs at once while the SAME"
echo " prefix is fenced to one; an outer hub imports an inner share's"
echo " data but never its control namespace; and prefix reuse silently"
echo " adopts the previous project's bytes."
echo "══════════════════════════════════════════════════════════════════"
