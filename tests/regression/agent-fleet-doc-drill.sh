#!/usr/bin/env bash
# agent-fleet doc drill — does docs/flint-lite-for-agent-fleets.md WORK?
#
# WHY THIS EXISTS
#
# The guide tells a team building an agentic harness to do a specific
# sequence of things. Every other drill in this tree tests the product;
# this one tests the DOCUMENT — it runs the guide's own commands and
# asserts the guide's own claims, so a wrong instruction fails here
# rather than in a stranger's cluster.
#
# The shape it drills is the one the guide describes, which is NOT the
# shape gateway-kind-e2e.sh drills:
#
#   * READS come through a MOUNT, and the mount is made by KUBELET from
#     an in-tree `nfs:` PersistentVolume into an ORDINARY pod. The
#     gateway drill hand-mounts from a `privileged: true` pod with
#     nfs-utils installed at runtime, which proves the server speaks
#     NFS and proves nothing at all about whether the guide's PV works.
#     Kubelet mounting on the node's behalf is a different code path
#     with a different prerequisite (`mount.nfs4` ON THE NODE) and a
#     different failure mode (the pod hangs in ContainerCreating).
#   * WRITES come through the REST file API.
#   * The point of the mount is HYDRATION: files live in S3 and the PVC
#     is a cache, so the read a real agent makes is a read of something
#     that is not on local disk yet.
#
# WHAT THE GUIDE CLAIMS, AND WHAT WOULD MAKE EACH CLAIM FALSE
#
#   L1  The AWS_* secret key names are load-bearing.
#       FALSE IF: a share with `accessKeyId` comes up fine, i.e. the
#       warning is folklore. Drilled with a real wrong-named Secret.
#   L2  The guide's FlintShare YAML is valid and reaches Ready.
#       FALSE IF: a field is misspelled. A structural CRD PRUNES
#       unknown fields SILENTLY, so every field is read back.
#   L3  The guide's PV/PVC mounts into an unprivileged pod.
#       FALSE IF: kubelet cannot mount, or the pod needs privilege.
#   L4  The mount needs NO credentials, and identity is asserted by the
#       client (AUTH_SYS, no squash).
#       FALSE IF: files created by uid 1000 land owned by root, or a
#       non-root pod cannot read at all.
#   L5  A REST write is visible on an already-established mount.
#       This is the two-door coherence claim, and it is the one most
#       likely to be quietly wrong: the kernel caches attributes.
#   L6  An evicted file reads back through the MOUNT transparently and
#       byte-identically.
#       FALSE IF: the reader gets a body of zeros, an error, or the
#       file was never actually evicted (the vacuity trap — asserted
#       against the hub's own /status, not assumed).
#   L7  `suspendWithSessions: false` keeps a mounted share up.
#       FALSE IF: it suspends anyway. Paired with a POSITIVE CONTROL on
#       a second share that omits the knob and MUST suspend — without
#       that control, "still Ready" is also what a broken idle ladder
#       looks like.
#
# DEVIATION FROM THE GUIDE, DELIBERATE AND RECORDED
#   This drill installs the LOCAL chart so it tests the working tree,
#   not whatever was last released. The guide installs 0.2.6 from
#   oci://registry-1.docker.io, which IS published as of 2026-08-22 and
#   renders the same gateway objects. To drill the published artifact
#   instead:
#
#     CHART_SRC=oci://registry-1.docker.io/dilipdalton/flint-lite-operator \
#       ./tests/regression/agent-fleet-doc-drill.sh
#
#   HISTORY, because it is the failure this drill nearly shipped into:
#   1.35.0 published its IMAGES but not its CHARTS, so for a day the
#   registry's newest operator chart was 0.2.5 — which has no
#   gateway.yaml at all. `--set gateway.enabled=true` against it renders
#   NOTHING and helm does not warn, because helm never errors on an
#   unknown --set key. A reader following the guide would have got a
#   clean install and no gateway.
#
# KEEP=1 leaves the cluster standing.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CARGO_DIR="$REPO_ROOT/spdk-csi-driver"
GUIDE="$REPO_ROOT/docs/flint-lite-for-agent-fleets.md"

# ── MODE ─────────────────────────────────────────────────────────────
# kind    (default) self-contained: builds images from the working tree,
#         creates a kind cluster, runs MinIO in it. Free, ~12 min.
# cluster runs against the CURRENT KUBECONFIG and REAL S3, using the
#         PUBLISHED chart and PUBLISHED images. This is the only mode
#         that tests what a reader actually installs, and the only one
#         that can answer four questions kind physically cannot:
#
#           * do the SHIPPED artifacts work? (kind loads locally-built
#             images and a local chart — it tests the working tree)
#           * does a real node image ship `mount.nfs4`?
#           * does the ClusterIP mount work from a node that is NOT the
#             hub's node? kind here is single-node, so every mount it
#             has ever made was node-local.
#           * what does hydration from REAL S3 actually cost? The kind
#             number is in-cluster MinIO and is not a rate.
#
#   MODE=cluster BUCKET=my-bucket REGION=us-west-1 \
#     AWS_PROFILE=... ./tests/regression/agent-fleet-doc-drill.sh
MODE="${MODE:-kind}"

CLUSTER="${CLUSTER:-flint-doc-drill}"
OPNS=flint-system
NS=workspaces
PROJECT=proj-a
PF_S3=39200
PF_GW=39201

if [ "$MODE" = cluster ]; then
  # The published artifacts, on purpose. Overridable for a pre-release.
  HUBIMG="${HUBIMG:-dilipdalton/flint-pnfs:1.35.0}"
  OPIMG="${OPIMG:-dilipdalton/flint-lite-operator:1.35.0}"
  OP_CHART="${CHART_SRC:-oci://registry-1.docker.io/dilipdalton/flint-lite-operator}"
  CHART_VER="${CHART_VER:-0.2.6}"
  BUCKET="${BUCKET:?MODE=cluster needs BUCKET=<an existing bucket, versioning ON>}"
  REGION="${REGION:-us-west-1}"
  S3_ENDPOINT=""                 # real S3: no endpoint override
  STORAGE_CLASS="${STORAGE_CLASS:-}"   # "" = the cluster default
  # A real PVC ENFORCES persistence.size, so the watermark is a real
  # watermark here — unlike kind's local-path hostPath, where statvfs
  # reports the node's whole filesystem and no size-derived watermark
  # can ever fire. That means this mode can use an honest PVC size and
  # a file big enough to make hydration a measurement rather than a
  # round trip.
  COLD_MB="${COLD_MB:-1024}"
  PVC_SIZE="${PVC_SIZE:-4Gi}"
else
  HUBIMG=flint-lite-dev:local
  OPIMG=flint-lite-operator-dev:local
  OP_CHART="${CHART_SRC:-$REPO_ROOT/flint-lite-operator-chart}"
  CHART_VER=""
  BUCKET=flint-doc-drill
  REGION=us-east-1
  S3_ENDPOINT="http://minio.$NS.svc:9000"
  STORAGE_CLASS=""
  COLD_MB="${COLD_MB:-8}"
  PVC_SIZE="${PVC_SIZE:-4Gi}"
fi
MINIO_USER=flintdrill
MINIO_PASS=flintdrill123
# What goes INTO the share's credentials Secret. Kind: MinIO's root
# user. Cluster: a real key pair, which the caller must supply — the
# drill will not fall back to ambient/admin credentials, because a hub
# quietly running as the operator's own identity would prove nothing
# about the guide's `credentialsSecretRef` path.
if [ "$MODE" = cluster ]; then
  HUB_AK="${DRILL_AK:?MODE=cluster needs DRILL_AK (an access key id for the bucket)}"
  HUB_SK="${DRILL_SK:?MODE=cluster needs DRILL_SK (its secret access key)}"
else
  HUB_AK="$MINIO_USER"
  HUB_SK="$MINIO_PASS"
fi
PF_S3_PID=""
PF_GW_PID=""
if [ "$MODE" = cluster ]; then
  # The caller's context, deliberately. Print it before touching
  # anything: a drill that installs an operator into the wrong cluster
  # is the worst possible outcome of a typo.
  KUBECONFIG_FILE=""
else
  KUBECONFIG_FILE="$(mktemp -t flint-doc-kubeconfig.XXXXXX)"
  export KUBECONFIG="$KUBECONFIG_FILE"
fi

# HELM NEEDS A WRITABLE CACHE, AND THE FAILURE NAMES THE WRONG THING.
#
# Pulling an OCI chart writes to $HELM_CACHE_HOME (default
# ~/Library/Caches/helm on macOS). Where that is not writable — a
# sandbox, a CI runner, a locked-down laptop — helm reports
# `failed to download "oci://..." at version "X"`, which reads exactly
# like the chart is missing from the registry. It is not; it is a local
# permission error. Point it somewhere we know we can write.
export HELM_CACHE_HOME="${HELM_CACHE_HOME:-${TMPDIR:-/tmp}/flint-drill-helm-cache}"
mkdir -p "$HELM_CACHE_HOME" 2>/dev/null

PASSES=0
FAILURES=()
say()  { echo; echo "── $*"; }
pass() { echo "  ✓ $*"; PASSES=$((PASSES+1)); }
# `fail` is for a rig that cannot continue (no cluster, no images).
fail() { echo "  ✗ FAIL: $*"; exit 1; }
# `bad` is for a LEG that failed. The legs are independent claims about
# the guide, so one wrong instruction should not hide the next four —
# run 4 lost L7 entirely because L6 exited. Recorded, reported at the
# end, and the drill still exits non-zero.
bad()  { echo "  ✗ FAIL: $*"; FAILURES+=("$*"); }
# STDERR on purpose: this is called from inside functions whose
# stdout is captured by command substitution (gw, derive_for). On
# stdout a single retry message becomes part of the captured value.
note() { echo "    · $*" >&2; }

sha() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'
  else shasum -a 256 "$1" | awk '{print $1}'; fi
}

# THE DRILL'S OWN TRANSPORT IS NOT THE PRODUCT.
#
# `kubectl port-forward` dies on its own, and an `aws` call through a
# dead one fails in a way that reads exactly like "the bucket is empty".
# That is how run 4 reported "the cold volume never published" — the
# publish was fine; the forward had gone. `gw()` already learned this
# lesson; this is the same fix for the S3 side.
_s3_raw() {
  if [ "$MODE" = cluster ]; then
    # Real S3, real credentials, whatever profile the caller exported.
    AWS_DEFAULT_REGION="$REGION" aws "$@" 2>&1
  else
    env -u AWS_PROFILE \
      AWS_ACCESS_KEY_ID=$MINIO_USER AWS_SECRET_ACCESS_KEY=$MINIO_PASS \
      AWS_DEFAULT_REGION="$REGION" AWS_EC2_METADATA_DISABLED=true \
      aws --endpoint-url "http://127.0.0.1:$PF_S3" "$@" 2>&1
  fi
}
s3() {
  local out rc
  out=$(_s3_raw "$@"); rc=$?
  if [ "$MODE" != cluster ] && [ $rc -ne 0 ] && printf '%s' "$out" | grep -qiE 'could not connect|connection refused|EndpointConnectionError|Connection aborted'; then
    note "the drill's S3 port-forward dropped; re-establishing"
    pf_s3
    out=$(_s3_raw "$@"); rc=$?
  fi
  printf '%s\n' "$out"
  return $rc
}

# UNMOUNT BEFORE TEARING THE CLUSTER DOWN.
#
# A `hard` mount whose server disappears puts its clients in
# uninterruptible sleep, and those processes cannot be killed — the
# kernel is waiting on I/O that will never complete. `kind delete
# cluster` kills the hub pods first, so tearing down with mounts live
# wedges the NODE CONTAINER ITSELF: `docker rm -f` answers "tried to
# kill container, but did not receive an exit event" and the only way
# out is restarting Docker. That happened on this drill's second run and
# cost a Docker Desktop restart.
#
# So: consumers first, server second. Same order the guide tells readers
# to use, for the same reason.
cleanup() {
  set +e
  [ -n "$PF_S3_PID" ] && kill "$PF_S3_PID" 2>/dev/null
  [ -n "$PF_GW_PID" ] && kill "$PF_GW_PID" 2>/dev/null
  if [ "${KEEP:-0}" = "1" ]; then
    echo "KEEP=1 — cluster standing (kubeconfig: $KUBECONFIG_FILE)"
    echo "NOTE: delete the agent pods BEFORE deleting the cluster, or the"
    echo "      node container wedges on uninterruptible NFS I/O:"
    echo "      kubectl -n $NS delete pod --all --force --grace-period=0"
    return
  fi
  # Consumers first, always — see the comment above.
  timeout 90 kubectl -n "$NS" delete pod --all --force --grace-period=0 >/dev/null 2>&1
  if [ "$MODE" = cluster ]; then
    # A cluster we did not create is not ours to delete. Remove only what
    # this drill added, and leave the objects' bucket data alone — the
    # operator never touches a bucket and neither do we.
    timeout 120 kubectl -n "$NS" delete flintshare --all >/dev/null 2>&1
    # PVCs BEFORE PVs. A PV carries the `kubernetes.io/pv-protection`
    # finalizer and will sit in Terminating FOREVER while any PVC is
    # still bound to it — after which `kubectl apply` of the same PV name
    # is refused and the next run dies at "PV ... refused" with nothing
    # obviously wrong. Note also that the HUB's own PVCs deliberately
    # outlive their FlintShare (no ownerRef, reclaim: Retain), so
    # deleting the shares does not take them with it.
    timeout 180 kubectl -n "$NS" delete pvc --all --timeout=120s >/dev/null 2>&1
    timeout 60 kubectl delete pv proj-a-data proj-a-cold proj-a-held proj-a-free \
      dnsname-pv secnull-pv --ignore-not-found >/dev/null 2>&1
    timeout 60 helm uninstall flint-lite-operator -n "$OPNS" >/dev/null 2>&1
    timeout 60 kubectl -n "$OPNS" delete secret flint-gateway-token flint-gateway-root \
      --ignore-not-found >/dev/null 2>&1
    timeout 60 kubectl -n "$NS" delete secret flint-s3 flint-s3-wrong \
      tok-data tok-cold tok-held tok-free --ignore-not-found >/dev/null 2>&1
    echo "cluster mode: drill objects removed; the CLUSTER and the BUCKET are untouched."
    echo "  bucket data under $PROJECT/ is left in place — delete it yourself if you want it gone."
    return
  fi
  timeout 60 docker exec "${CLUSTER}-control-plane" sh -c \
    'for m in $(mount | grep nfs4 | cut -d" " -f3); do umount -l -f "$m"; done' \
    >/dev/null 2>&1
  kind delete cluster --name "$CLUSTER" >/dev/null 2>&1
  rm -f "$KUBECONFIG_FILE"
}
trap cleanup EXIT

# A port-forward exits on its own; polling its port afterwards can only
# fail. Give up the moment the forwarder is gone and let the caller
# respawn. (This cost run 11 of the gateway drill a whole cycle.)
pf_wait() {
  local pid="$1" url="$2" tries="$3" _
  for _ in $(seq 1 "$tries"); do
    curl -sf "$url" >/dev/null && return 0
    kill -0 "$pid" 2>/dev/null || return 1
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
  fail "MinIO port-forward never became healthy"
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
  fail "gateway port-forward never became healthy"
}
# HTTP 000 is curl saying it got no response at all. It is never an
# answer about the product; re-establish and ask again.
gw() {
  local method="$1" path="$2"; shift 2
  local code
  code=$(curl -s -o /tmp/doc-body.txt -w '%{http_code}' -X "$method" \
    -H "Authorization: Bearer $GW_TOKEN" "$@" "http://127.0.0.1:$PF_GW$path")
  if [ "$code" = "000" ]; then
    note "no response at all — the drill's port-forward dropped; re-establishing"
    pf_gw
    code=$(curl -s -o /tmp/doc-body.txt -w '%{http_code}' -X "$method" \
      -H "Authorization: Bearer $GW_TOKEN" "$@" "http://127.0.0.1:$PF_GW$path")
  fi
  echo "$code"
}

echo "══════════════════════════════════════════════════════════════════"
echo " agent-fleet doc drill — running the guide's own commands"
echo " $GUIDE"
echo "══════════════════════════════════════════════════════════════════"
[ -f "$GUIDE" ] || fail "the guide does not exist at $GUIDE"

# ── 0. the target ────────────────────────────────────────────────────
if [ "$MODE" = cluster ]; then
  say "cluster mode — running against the CURRENT kubeconfig and REAL S3"
  CTX=$(kubectl config current-context 2>/dev/null)
  [ -n "$CTX" ] || fail "no current kubectl context"
  SRV=$(kubectl config view --minify -o jsonpath='{.clusters[0].cluster.server}' 2>/dev/null)
  NODES=$(kubectl get nodes --no-headers 2>/dev/null | wc -l | tr -d ' ')
  [ "$NODES" -ge 1 ] || fail "cannot list nodes — is this kubeconfig live?"
  note "context : $CTX"
  note "server  : $SRV"
  note "nodes   : $NODES"
  note "bucket  : s3://$BUCKET  region $REGION"
  note "chart   : $OP_CHART ${CHART_VER:+(version $CHART_VER)}"
  note "images  : $OPIMG / $HUBIMG"
  # THE BUCKET MUST EXIST AND BE OURS TO WRITE. Finding out later costs
  # a whole provisioning cycle, and finding out by writing into someone
  # else's prefix is worse than costing time.
  s3 s3 ls "s3://$BUCKET/" >/dev/null 2>&1 \
    || fail "cannot list s3://$BUCKET — check the bucket name, the region and AWS_PROFILE"
  EXISTING=$(s3 s3 ls "s3://$BUCKET/$PROJECT/" --recursive 2>/dev/null | grep -c . || true)
  if [ "${EXISTING:-0}" -gt 0 ]; then
    fail "s3://$BUCKET/$PROJECT/ ALREADY HAS $EXISTING object(s) — refusing to adopt another tenant's prefix. Use a different BUCKET, or clear that prefix deliberately."
  fi
  VERS=$(s3 s3api get-bucket-versioning --bucket "$BUCKET" 2>/dev/null | grep -o '"Status": *"[A-Za-z]*"' | head -1)
  case "$VERS" in
    *Enabled*) note "versioning: Enabled" ;;
    *) note "⚠ versioning is NOT enabled ($VERS) — the tier wants it on" ;;
  esac
  # A real cluster needs a working default StorageClass for the hub PVC.
  DEFSC=$(kubectl get sc -o jsonpath='{range .items[*]}{.metadata.name}{" "}{.metadata.annotations.storageclass\.kubernetes\.io/is-default-class}{"\n"}{end}' 2>/dev/null | awk '$2=="true"{print $1}' | head -1)
  if [ -n "$STORAGE_CLASS" ]; then
    note "storageClass: $STORAGE_CLASS (explicit)"
  elif [ -n "$DEFSC" ]; then
    note "storageClass: $DEFSC (cluster default)"
  else
    fail "no default StorageClass and STORAGE_CLASS unset — the hub's PVC would stay Pending forever"
  fi
  pass "target confirmed"
else
# ── 0. build + cluster ───────────────────────────────────────────────
case "$(uname -m)" in
  arm64|aarch64) TRIPLE=aarch64-unknown-linux-musl; PLATFORM=linux/arm64 ;;
  *)             TRIPLE=x86_64-unknown-linux-musl;  PLATFORM=linux/amd64 ;;
esac

say "building flint-pnfs-mds + flint-lite-operator + flint-hub-gateway ($TRIPLE)"
(cd "$CARGO_DIR" && cargo zigbuild --release --target "$TRIPLE" \
   --bin flint-pnfs-mds --bin flint-lite-operator --bin flint-hub-gateway \
   >/tmp/doc-drill-build.log 2>&1) \
  || { tail -20 /tmp/doc-drill-build.log; fail "zigbuild failed"; }
IMGDIR=$(mktemp -d -t flint-doc-img.XXXXXX)
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
docker build --platform "$PLATFORM" -f "$IMGDIR/Dockerfile.hub" -t "$HUBIMG" "$IMGDIR" \
  >/tmp/doc-drill-img.log 2>&1 || { tail -5 /tmp/doc-drill-img.log; fail "hub image build failed"; }
docker build --platform "$PLATFORM" -f "$IMGDIR/Dockerfile.op" -t "$OPIMG" "$IMGDIR" \
  >>/tmp/doc-drill-img.log 2>&1 || { tail -5 /tmp/doc-drill-img.log; fail "op image build failed"; }
rm -rf "$IMGDIR"
pass "images built ($PLATFORM)"

say "creating kind cluster '$CLUSTER'"
kind delete cluster --name "$CLUSTER" >/dev/null 2>&1
kind create cluster --name "$CLUSTER" --wait 120s >/dev/null 2>&1 \
  || fail "kind create cluster failed"
for i in "$HUBIMG" "$OPIMG"; do
  kind load docker-image "$i" --name "$CLUSTER" >/dev/null 2>&1 || fail "kind load $i failed"
done
pass "cluster up, images loaded"

# THE NODE PREREQUISITE, CHECKED BEFORE ANYTHING DEPENDS ON IT.
#
# An in-tree `nfs:` PV is mounted BY KUBELET, on the node, using the
# node's own `mount.nfs4`. If it is missing the pod does not fail — it
# sits in ContainerCreating with the reason buried in an event, which
# is exactly the failure a guide should warn about. Find out here, so
# L3 can report the difference between "the guide is wrong" and "this
# node has no NFS client".
NODE=$(kubectl get nodes -o jsonpath='{.items[0].metadata.name}')
if docker exec "$NODE" sh -c 'command -v mount.nfs4 >/dev/null 2>&1'; then
  NODE_HAS_NFS=1
  pass "the kind node has mount.nfs4 — kubelet can mount an nfs: PV"
else
  NODE_HAS_NFS=0
  note "the kind node has NO mount.nfs4"
  note "installing nfs-common on the node so L3 tests the GUIDE, not the rig"
  docker exec "$NODE" sh -c 'apt-get update -qq && apt-get install -y -qq nfs-common' \
    >/tmp/doc-drill-nfs.log 2>&1
  if docker exec "$NODE" sh -c 'command -v mount.nfs4 >/dev/null 2>&1'; then
    NODE_HAS_NFS=1
    pass "nfs-common installed on the node (THE GUIDE MUST SAY THIS IS A PREREQUISITE)"
  else
    tail -5 /tmp/doc-drill-nfs.log
    note "could not install an NFS client; L3/L4/L5/L6 will report INCONCLUSIVE"
  fi
fi

# ── MinIO ────────────────────────────────────────────────────────────
say "MinIO in-cluster, bucket $BUCKET (stands in for S3)"
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
kubectl -n "$NS" rollout status deployment/minio --timeout=180s >/dev/null 2>&1 \
  || fail "MinIO never became Ready"
pf_s3
s3 s3 mb "s3://$BUCKET" >/dev/null || fail "bucket create failed"
pass "MinIO Ready, bucket created"
fi

# ── operator + gateway, per the guide ────────────────────────────────
say "installing the operator and the gateway (guide steps 1 and 4)"
# Both modes need the workspace namespace. In kind it is created with
# MinIO; in cluster mode nothing has made it yet, and every share, Secret
# and PVC below lands in it.
kubectl create namespace "$NS" >/dev/null 2>&1
GW_TOKEN="doc-drill-inbound-$(date +%s)"
GW_ROOT="doc-drill-root-key-at-least-32-bytes-long-yes"
kubectl create namespace "$OPNS" >/dev/null 2>&1
# Idempotent: a re-run after a failed run must not trip over its own
# leftovers. `create --dry-run=client | apply` is the standard trick and
# it also updates the value, which is what we want for a rotating token.
mksecret() {  # mksecret <ns> <name> <key=value>
  kubectl -n "$1" create secret generic "$2" --from-literal="$3" \
    --dry-run=client -o yaml 2>/dev/null | kubectl apply -f - >/dev/null 2>&1
}
mksecret "$OPNS" flint-gateway-token "token=$GW_TOKEN" || fail "gateway token Secret refused"
mksecret "$OPNS" flint-gateway-root  "key=$GW_ROOT"    || fail "gateway root Secret refused"
helm install flint-lite-operator "$OP_CHART" ${CHART_VER:+--version "$CHART_VER"} -n "$OPNS" \
  --set image.ref="$OPIMG" --set hubImage="$HUBIMG" \
  --set replicas=1 --set gateway.replicas=1 \
  --set gateway.enabled=true \
  --set gateway.tokenSecretRef=flint-gateway-token \
  --set gateway.rootKeySecretRef=flint-gateway-root \
  >/tmp/doc-drill-helm.log 2>&1 || {
    tail -20 /tmp/doc-drill-helm.log
    if grep -q 'failed to download' /tmp/doc-drill-helm.log; then
      note "NOTE: 'failed to download' from an OCI chart is USUALLY a local cache"
      note "permission problem, not a missing chart. HELM_CACHE_HOME=$HELM_CACHE_HOME"
      note "Check by hand:  helm show chart $OP_CHART ${CHART_VER:+--version $CHART_VER}"
    fi
    fail "helm install failed"; }
kubectl -n "$OPNS" rollout status deployment/flint-lite-operator --timeout=180s >/dev/null 2>&1 \
  || fail "operator never became Ready"
kubectl -n "$OPNS" rollout status deployment/flint-lite-operator-gateway --timeout=180s >/dev/null 2>&1 \
  || fail "gateway never became Ready"
pf_gw
pass "operator + gateway Ready"

# ══ L1 ═══════════════════════════════════════════════════════════════
say "L1: the guide says the AWS_* secret key names are load-bearing"
# The claim is that naming a key `accessKeyId` leaves the SDK with NO
# credentials, so the hub cannot reach the bucket. If a share built on
# such a Secret comes up Ready anyway, the warning is folklore and the
# guide should not carry it.
kubectl -n "$NS" create secret generic flint-s3-wrong \
  --from-literal=accessKeyId="$HUB_AK" \
  --from-literal=secretAccessKey="$HUB_SK" \
  --dry-run=client -o yaml 2>/dev/null | kubectl apply -f - >/dev/null \
  || fail "wrong-name Secret refused"
kubectl -n "$NS" apply -f - >/dev/null <<EOF || fail "control share refused"
apiVersion: flint.io/v1alpha1
kind: FlintShare
metadata: { name: fs-wrongkeys, namespace: $NS }
spec:
  bucket: $BUCKET
  keyPrefix: wrongkeys/
  endpoint: http://minio.$NS.svc:9000
  region: us-east-1
  credentialsSecretRef: flint-s3-wrong
  persistence: { size: 1Gi }
  monitoring: { enabled: true }
EOF
WRONG_READY=""
for _ in $(seq 1 24); do
  ph=$(kubectl -n "$NS" get flintshare fs-wrongkeys -o jsonpath='{.status.phase}' 2>/dev/null)
  [ "$ph" = "Ready" ] && { WRONG_READY=1; break; }
  sleep 5
done
if [ -n "$WRONG_READY" ]; then
  fail "a share with accessKeyId/secretAccessKey reached Ready — the guide's warning is WRONG"
fi
WLOG=$(kubectl -n "$NS" logs -l flint.io/share=fs-wrongkeys --tail=40 2>/dev/null)
note "phase after 120s: ${ph:-<none>}"
case "$WLOG" in
  *unreachable*|*dispatch*|*credential*|*Credential*)
    pass "wrong key names ⇒ never Ready, and the log names the bucket (guide's warning holds)" ;;
  *)
    note "log tail: $(echo "$WLOG" | tail -3)"
    pass "wrong key names ⇒ never Ready (phase=${ph:-<none>}); log wording differs from the guide" ;;
esac
kubectl -n "$NS" delete flintshare fs-wrongkeys --ignore-not-found >/dev/null 2>&1

# ══ L2 ═══════════════════════════════════════════════════════════════
say "L2: the guide's FlintShare applies verbatim and reaches Ready"
kubectl -n "$NS" create secret generic flint-s3 \
  --from-literal=AWS_ACCESS_KEY_ID="$HUB_AK" \
  --from-literal=AWS_SECRET_ACCESS_KEY="$HUB_SK" \
  --dry-run=client -o yaml 2>/dev/null | kubectl apply -f - >/dev/null \
  || fail "creds Secret refused"

# The share's file-API token, derived by the binary exactly as the
# guide's step-4 recipe says. Derived AFTER the share exists, via
# --derive-for, so this also proves the recipe's ordering works.
# `endpoint` is omitted entirely for real S3 (absent = AWS), and it is
# part of the token binding — so getting this wrong does not just break
# the tier, it invalidates every derived token.
EP_LINE=""
[ -n "$S3_ENDPOINT" ] && EP_LINE="
  endpoint: $S3_ENDPOINT"
# storageClassName lives UNDER persistence, and "" means "the cluster
# default" to the operator — so it is omitted rather than set empty.
SC_LINE=""
[ -n "$STORAGE_CLASS" ] && SC_LINE="
    storageClassName: $STORAGE_CLASS"

mkshare() {  # mkshare <name> <volume-id> <extra spec yaml>
  kubectl -n "$NS" apply -f - >/dev/null <<EOF || fail "FlintShare $1 refused"
apiVersion: flint.io/v1alpha1
kind: FlintShare
metadata:
  name: $1
  namespace: $NS
  labels:
    flint.io/project-id: $PROJECT
    flint.io/volume-id: $2
spec:
  bucket: $BUCKET
  keyPrefix: $PROJECT/$2/${EP_LINE}
  region: $REGION
  credentialsSecretRef: flint-s3
  persistence:
    size: $PVC_SIZE${SC_LINE}
  monitoring:
    enabled: true
    fileApi:
      enabled: true
      tokenSecretRef: tok-$2
$3
EOF
}

# The main volume: ordinary settings, for the mount + coherence legs.
mkshare "fs-$PROJECT-data" data "  settings:
    flushFloorSecs: 3"
# The hydration volume: eviction ALWAYS armed, so a read through the
# mount is a read of something genuinely not on local disk. kind's
# local-path SC backs a PVC with a HOSTPATH and does not enforce
# persistence.size, so the hub's statvfs reports the NODE's filesystem
# and no size-derived watermark could ever fire — hence watermarkPct 1.
mkshare "fs-$PROJECT-cold" cold "  settings:
    flushFloorSecs: 3
    watermarkPct: 1
    ballastBytes: 0
    hydrateFetchParallel: 1
    hydrateWarmAfterImport: false"
# L7's pair is created BY L7. The Docker VM backing kind is commonly
# ~4 GiB, and four hubs plus their agents plus MinIO plus the operator
# and gateway is enough to make the node flaky — which shows up as
# `kind create cluster failed` on the NEXT run, not as an honest OOM
# here. Peak concurrency is a rig property worth keeping low.

# A structural CRD PRUNES unknown fields SILENTLY — a misspelling in
# the guide would vanish rather than error, and every leg built on it
# would wait for something that was never armed. Read them back.
chk() {  # chk <share> <jsonpath> <expected>
  local got; got=$(kubectl -n "$NS" get flintshare "$1" -o jsonpath="$2" 2>/dev/null)
  [ "$got" = "$3" ] || fail "$1 $2 did not stick (got '$got', wanted '$3') — pruned by the schema"
}
chk "fs-$PROJECT-cold" '{.spec.settings.watermarkPct}' 1
chk "fs-$PROJECT-cold" '{.spec.settings.hydrateFetchParallel}' 1
pass "every field in the guide's YAML survived admission (nothing silently pruned)"

# THE GUIDE'S ORDERING, AND WHY IT MATTERS.
#
# The guide says: create the share, then `--derive-for` it, then write
# the token Secret. That ordering is only safe because the Secret is a
# projected VOLUME: until it exists the hub pod sits in
# ContainerCreating ("secret not found") and retries, then mounts and
# starts the moment the Secret appears. So the share does NOT reach
# Ready until the token exists — which means anything that waits for
# Ready BEFORE writing the token deadlocks. Derive first, then wait.
derive_for() {
  kubectl -n "$OPNS" exec deploy/flint-lite-operator-gateway -- \
    /usr/local/bin/flint-hub-gateway --root-key-file=/etc/flint/gateway-root/key \
    --derive-for "$1" 2>/dev/null | tr -d '\r\n'
}
for v in data cold; do
  T=$(derive_for "$NS/fs-$PROJECT-$v")
  [ -n "$T" ] || fail "--derive-for produced nothing for $v (is the CR readable by the gateway SA?)"
  mksecret "$NS" "tok-$v" "token=$T" || fail "token Secret tok-$v refused"
done
pass "--derive-for produced a token for every share, straight from its CR"

# Prove the bootstrap claim rather than assuming it: a share whose token
# Secret did not exist when it was created must nonetheless reach Ready
# once the Secret lands.
for s in data cold; do
  ph=""
  for _ in $(seq 1 60); do
    ph=$(kubectl -n "$NS" get flintshare "fs-$PROJECT-$s" -o jsonpath='{.status.phase}' 2>/dev/null)
    [ "$ph" = "Ready" ] && break
    sleep 5
  done
  [ "$ph" = "Ready" ] || {
    kubectl -n "$NS" get flintshare "fs-$PROJECT-$s" -o yaml | tail -30
    kubectl -n "$NS" describe pod -l flint.io/share="fs-$PROJECT-$s" | sed -n '/Events:/,$p' | tail -10
    fail "fs-$PROJECT-$s never became Ready (phase=$ph)"; }
done
ADDR=$(kubectl -n "$NS" get flintshare "fs-$PROJECT-data" -o jsonpath='{.status.address}')
[ -n "$ADDR" ] || fail "status.address is empty — the guide shows it in the ADDRESS column"
pass "both shares Ready (token Secret written AFTER the share — the guide's order works)"

code=$(gw GET "/v1/projects/$PROJECT/volumes")
[ "$code" = "200" ] || fail "the gateway cannot list this project's volumes (HTTP $code): $(cat /tmp/doc-body.txt)"
pass "the guide's --derive-for recipe produces tokens every hub accepts"

# ══ L3 ═══════════════════════════════════════════════════════════════
say "L3: the guide's PV + PVC mount into an UNPRIVILEGED pod (kubelet does the mount)"
# THE ADDRESS IS THE WHOLE LEG.
#
# An in-tree `nfs:` volume is resolved BY KUBELET, ON THE NODE. Nodes do
# not generally use cluster DNS, so a `*.svc.cluster.local` server name
# does not fail — it HANGS, with the pod in ContainerCreating and no
# error event, while mount.nfs retries on the node forever. The guide
# says to use the Service ClusterIP instead, because kube-proxy programs
# that on every node.
#
# L3a proves the guidance is NECESSARY (the DNS name really does hang).
# L3b proves it is SUFFICIENT (the ClusterIP really does mount).
# Without L3a the guide is repeating folklore; without L3b it is wrong.
DNSNAME=${ADDR%:*}
CIP=$(kubectl -n "$NS" get svc "fs-$PROJECT-data" -o jsonpath='{.spec.clusterIP}')
[ -n "$CIP" ] || fail "the share's Service has no ClusterIP"
note "DNS name: $DNSNAME"
note "ClusterIP: $CIP"

mkpv() {  # mkpv <name> <server> <extra mount option or empty>
  local extra=""
  [ -n "$3" ] && extra="
    - $3"
  kubectl apply -f - >/dev/null <<EOF || fail "PV $1 refused"
apiVersion: v1
kind: PersistentVolume
metadata: { name: $1 }
spec:
  capacity: { storage: 100Gi }
  accessModes: [ReadWriteMany]
  persistentVolumeReclaimPolicy: Retain
  mountOptions:
    - nfsvers=4.1
    - proto=tcp
    - hard
    - nconnect=4
    - noatime$extra
  nfs:
    server: $2
    path: /
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata: { name: $1, namespace: $NS }
spec:
  accessModes: [ReadWriteMany]
  storageClassName: ""
  volumeName: $1
  resources: { requests: { storage: 100Gi } }
EOF
}
mkagent() {  # mkagent <pod> <claim>
  kubectl -n "$NS" apply -f - >/dev/null <<EOF || fail "pod $1 refused"
apiVersion: v1
kind: Pod
metadata: { name: $1, namespace: $NS }
spec:
  restartPolicy: Never
  securityContext:
    runAsUser: 1000
    runAsGroup: 1000
    fsGroup: 1000
  containers:
    - name: c
      image: alpine:3.20
      command: ["sh","-c","sleep 10800"]
      volumeMounts: [{ name: ws, mountPath: /workspace }]
  volumes:
    - name: ws
      persistentVolumeClaim: { claimName: $2 }
EOF
}

# ── L3a: the DNS name must NOT work, or the guide's warning is folklore
mkpv dnsname-pv "$DNSNAME" "sec=sys"
mkagent agent-dns dnsname-pv
if kubectl -n "$NS" wait --for=condition=ready pod/agent-dns --timeout=90s >/dev/null 2>&1; then
  note "the pod mounted a .svc.cluster.local server name — this node DOES resolve cluster DNS"
  note "the guide's ClusterIP advice is then belt-and-braces here, but still correct for"
  note "the general case; NOT reporting the warning as proven on this rig"
  DNS_HANGS=0
else
  ST=$(kubectl -n "$NS" get pod agent-dns -o jsonpath='{.status.phase}' 2>/dev/null)
  EV=$(kubectl -n "$NS" describe pod agent-dns 2>/dev/null | grep -c "FailedMount")
  note "after 90s: phase=$ST, FailedMount events=$EV"
  DNS_HANGS=1
  pass "a .svc.cluster.local server name does NOT mount (phase=$ST) — the guide's warning is real"
fi
kubectl -n "$NS" delete pod agent-dns --force --grace-period=0 >/dev/null 2>&1 &

# ── L3b: the ClusterIP must work
mkpv proj-a-data "$CIP" "sec=sys"
mkagent agent proj-a-data
MOUNT_OK=""
kubectl -n "$NS" wait --for=condition=ready pod/agent --timeout=180s >/dev/null 2>&1 && MOUNT_OK=1
if [ -z "$MOUNT_OK" ]; then
  kubectl -n "$NS" describe pod agent | sed -n '/Events:/,$p' | tail -12
  if [ "$NODE_HAS_NFS" = "0" ]; then
    note "the node has no NFS client — RIG, not the guide"
    MOUNT_INCONCLUSIVE=1
  else
    fail "an unprivileged pod could NOT mount the guide's ClusterIP PV"
  fi
else
  pass "an unprivileged, non-root pod mounted the ClusterIP PV — kubelet did the mount"
  # A successful kubelet mount IS the proof that the node carries an NFS
  # client; nothing else could have satisfied an in-tree `nfs:` volume.
  # Cluster mode never runs the kind-only probe, so without this the
  # summary reported "NOT AVAILABLE" on a node that had just mounted.
  NODE_HAS_NFS=1
fi
ag() { kubectl -n "$NS" exec agent -- sh -c "$1" 2>&1; }

# ══ L4 ═══════════════════════════════════════════════════════════════
if [ -n "$MOUNT_OK" ]; then
say "L4: sec=sys is load-bearing — without it every client is root"
  FLAV=$(ag 'mount | grep /workspace | grep -o "sec=[a-z]*" | head -1')
  [ "$FLAV" = "sec=sys" ] || fail "the guide asked for sec=sys and the mount negotiated '$FLAV'"
  note "negotiated flavour: $FLAV"
  WHO=$(ag 'id -u' | tr -d '\r')
  ROOTDIR=$(ag 'ls -ldn /workspace')
  note "the agent is uid $WHO; export root is: $ROOTDIR"
  ag 'touch /workspace/from-agent.txt' >/dev/null
  OWNER=$(ag 'stat -c "%u:%g" /workspace/from-agent.txt' | tr -d '\r')
  [ "$OWNER" = "1000:1000" ] \
    || fail "with sec=sys a uid-1000 agent's file should be owned 1000:1000, got '$OWNER'"
  pass "with sec=sys a uid-1000 agent's file is owned 1000:1000 — identity round-trips"
  ROOTDIR_WRITABLE=1

  # THE CONTROL. The guide claims that OMITTING sec=sys silently gives
  # every client root. Same server, same pod spec, one option removed —
  # so a difference here is attributable to that option and nothing else.
  mkpv secnull-pv "$CIP" ""
  mkagent agent-secnull secnull-pv
  if kubectl -n "$NS" wait --for=condition=ready pod/agent-secnull --timeout=150s >/dev/null 2>&1; then
    NFLAV=$(kubectl -n "$NS" exec agent-secnull -- sh -c 'mount | grep /workspace | grep -o "sec=[a-z]*" | head -1' 2>/dev/null | tr -d '\r')
    kubectl -n "$NS" exec agent-secnull -- sh -c 'touch /workspace/no-sec.txt' >/dev/null 2>&1
    NOWNER=$(kubectl -n "$NS" exec agent-secnull -- sh -c 'stat -c "%u:%g" /workspace/no-sec.txt' 2>/dev/null | tr -d '\r')
    note "without sec=sys: flavour=$NFLAV, a uid-1000 agent's file is owned $NOWNER"
    if [ "$NFLAV" = "sec=null" ] && [ "$NOWNER" = "0:0" ]; then
      pass "CONTROL: omitting sec=sys ⇒ sec=null and every file lands root-owned (guide's warning proven)"
      SECNULL_PROVEN=1
    else
      note "the control did not reproduce sec=null (flavour=$NFLAV owner=$NOWNER)"
      note "the guide's sec=sys advice stands, but this rig did not demonstrate the failure"
    fi
    kubectl -n "$NS" delete pod agent-secnull --force --grace-period=0 >/dev/null 2>&1 &
  else
    note "the sec=null control pod never became Ready; not reporting the warning as proven"
  fi
fi

# ══ L5 ═══════════════════════════════════════════════════════════════
if [ -n "$MOUNT_OK" ]; then
say "L5: a REST write is visible on an ALREADY-ESTABLISHED mount"
  # This is the two-door coherence claim. The mount is already up and
  # the kernel is caching attributes, so this is the realistic shape:
  # the harness writes over REST while an agent has the tree mounted.
  BODY="rest-write-$(date +%s)"
  printf '%s\n' "$BODY" >/tmp/doc-rest.txt
  code=$(gw PUT "/v1/projects/$PROJECT/volumes/data/files/content?path=/rest-written.txt" \
    -H 'Content-Type: application/octet-stream' --data-binary @/tmp/doc-rest.txt)
  case "$code" in
    200|201|204) ;;
    *) fail "the REST write failed (HTTP $code): $(cat /tmp/doc-body.txt)" ;;
  esac
  SAW=""
  T0=$(date +%s)
  for _ in $(seq 1 60); do
    OUT=$(ag "cat /workspace/rest-written.txt 2>/dev/null")
    case "$OUT" in *"$BODY"*) SAW=1; break ;; esac
    sleep 1
  done
  T1=$(date +%s)
  if [ -n "$SAW" ]; then
    pass "the mount saw the REST write after $((T1-T0))s (a NEW file needs a dir revalidation)"
    REST_VISIBLE_SECS=$((T1-T0))
  else
    ag 'ls -la /workspace'
    fail "the mount NEVER saw a file written over REST — the two doors do not agree"
  fi

  # And the harder direction: OVERWRITE a file the agent has already
  # read. A new file only needs the directory cache to expire; an
  # overwrite needs the FILE's attribute cache to expire, which is the
  # case a guide is most likely to get wrong.
  printf 'v1\n' >/tmp/doc-v1.txt
  gw PUT "/v1/projects/$PROJECT/volumes/data/files/content?path=/versioned.txt" \
     -H 'Content-Type: application/octet-stream' --data-binary @/tmp/doc-v1.txt >/dev/null
  for _ in $(seq 1 30); do
    [ "$(ag "cat /workspace/versioned.txt 2>/dev/null" | grep -c v1)" -gt 0 ] && break
    sleep 1
  done
  ag "cat /workspace/versioned.txt" >/dev/null    # agent has now cached it
  printf 'v2-overwritten\n' >/tmp/doc-v2.txt
  gw PUT "/v1/projects/$PROJECT/volumes/data/files/content?path=/versioned.txt" \
     -H 'Content-Type: application/octet-stream' --data-binary @/tmp/doc-v2.txt >/dev/null
  SAW2=""; T0=$(date +%s)
  for _ in $(seq 1 90); do
    [ "$(ag "cat /workspace/versioned.txt 2>/dev/null" | grep -c v2-overwritten)" -gt 0 ] && { SAW2=1; break; }
    sleep 1
  done
  T1=$(date +%s)
  if [ -n "$SAW2" ]; then
    pass "an OVERWRITE over REST reached the mounted agent after $((T1-T0))s"
    OVERWRITE_SECS=$((T1-T0))
  else
    fail "an overwrite over REST NEVER reached the mounted agent (stale for >90s)"
  fi
fi

# ══ L6 ═══════════════════════════════════════════════════════════════
say "L6: an evicted file hydrates from S3 through the MOUNT, byte-identical (${COLD_MB} MiB)"
# Seed through REST, let it publish, force eviction, then read it back
# on a mount — the guide's actual read path.
# SEED FROM INSIDE THE CLUSTER, NOT THROUGH THE PORT-FORWARD.
#
# A gigabyte pushed from a laptop through `kubectl port-forward` is slow
# and fragile: the forward is a userspace relay through the API server,
# and it dropped mid-upload on run 4 — giving HTTP 000 and a leg that
# blamed the gateway for the drill's own transport. Generating the bytes
# in-cluster and PUTting them to the gateway's Service removes the hop.
COLDSUM=""
if [ "$MODE" = cluster ]; then
  kubectl -n "$NS" delete pod seeder --force --grace-period=0 >/dev/null 2>&1
  kubectl -n "$NS" run seeder --image=debian:12-slim --restart=Never --command -- \
    sh -c "apt-get update -qq >/dev/null 2>&1; apt-get install -y -qq curl >/dev/null 2>&1;
      dd if=/dev/urandom of=/tmp/p.bin bs=1M count=$COLD_MB 2>/dev/null;
      S=\$(sha256sum /tmp/p.bin | cut -d' ' -f1);
      C=\$(curl -s -o /dev/null -w '%{http_code}' -X PUT \
        -H 'Authorization: Bearer $GW_TOKEN' -H 'Content-Type: application/octet-stream' -H 'Expect:' \
        -T /tmp/p.bin \
        'http://flint-lite-operator-gateway.$OPNS.svc:8090/v1/projects/$PROJECT/volumes/cold/files/content?path=/cold.bin');
      curl -s -o /dev/null -X PUT -H 'Authorization: Bearer $GW_TOKEN' \
        -H 'Content-Type: application/octet-stream' -H 'Expect:' \
        --data-binary \"\$S\" \
        'http://flint-lite-operator-gateway.$OPNS.svc:8090/v1/projects/$PROJECT/volumes/data/files/content?path=/cold.sha256';
      echo RESULT code=\$C sum=\$S" >/dev/null 2>&1
  for _ in $(seq 1 90); do
    sph=$(kubectl -n "$NS" get pod seeder -o jsonpath='{.status.phase}' 2>/dev/null)
    case "$sph" in Succeeded|Failed) break ;; esac
    sleep 5
  done
  RES=$(kubectl -n "$NS" logs seeder 2>/dev/null | grep RESULT | tail -1)
  note "in-cluster seeder: ${RES:-<no log line>}"
  SEEDCODE=$(printf '%s' "$RES" | sed -n 's/.*code=\([0-9]*\).*/\1/p')
  COLDSUM=$(printf '%s' "$RES" | sed -n 's/.*sum=\([0-9a-f]*\).*/\1/p')
  # DO NOT DEPEND ON `kubectl logs`. Run 5 uploaded a full gigabyte
  # correctly and still failed the leg, because the log line could not be
  # read back — leaving `wanted ''` in a checksum comparison. The seeder
  # also PUTs its digest as an object on the NORMAL volume (which has an
  # ordinary watermark, so it stays local and cannot 503 on us), and that
  # is the authoritative source here.
  if [ -z "$COLDSUM" ]; then
    for _ in 1 2 3; do
      if [ "$(gw GET "/v1/projects/$PROJECT/volumes/data/files/content?path=/cold.sha256")" = "200" ]; then
        COLDSUM=$(tr -d '\r\n ' < /tmp/doc-body.txt); [ -n "$COLDSUM" ] && break
      fi
      sleep 4
    done
    [ -n "$COLDSUM" ] && note "checksum recovered over REST: ${COLDSUM:0:16}…"
  fi
  # If the digest object arrived, the upload itself plainly succeeded —
  # so do not also fail on a log line we could not read.
  [ -n "$COLDSUM" ] && [ -z "$SEEDCODE" ] && SEEDCODE=200
  kubectl -n "$NS" delete pod seeder --force --grace-period=0 >/dev/null 2>&1 &
  case "$SEEDCODE" in
    200|201|204) ;;
    *) bad "in-cluster seeding of ${COLD_MB} MiB failed (HTTP ${SEEDCODE:-none})" ;;
  esac
  [ -n "$COLDSUM" ] || bad "the seeder reported no checksum — nothing to compare a hydration against"
else
  dd if=/dev/urandom of=/tmp/doc-cold.bin bs=1M count="$COLD_MB" 2>/dev/null
  COLDSUM=$(sha /tmp/doc-cold.bin)
  code=$(gw PUT "/v1/projects/$PROJECT/volumes/cold/files/content?path=/cold.bin" \
    -H 'Content-Type: application/octet-stream' -H 'Expect:' -T /tmp/doc-cold.bin)
  case "$code" in 200|201|204) ;; *) bad "seeding the cold volume with ${COLD_MB} MiB failed (HTTP $code)" ;; esac
fi

# THE ANTI-VACUITY GUARD. "the read returned the right bytes" is also
# what a file that never left local disk looks like. Prove it was
# actually published to the bucket AND actually evicted before reading.
# Distinguish "the bucket is empty" from "the drill cannot reach the
# bucket". Those look identical in `aws s3 ls` output and only one of
# them is a product statement.
PUBLISHED=""
S3_REACHABLE=""
for _ in $(seq 1 60); do
  OUT=$(s3 s3 ls "s3://$BUCKET/$PROJECT/cold/" --recursive); RC=$?
  if [ $RC -eq 0 ]; then
    S3_REACHABLE=1
    [ "$(printf '%s' "$OUT" | grep -c .)" -gt 0 ] && { PUBLISHED=1; break; }
  fi
  sleep 3
done
if [ -z "$S3_REACHABLE" ]; then
  note "last aws output: $(printf '%s' "$OUT" | tail -2)"
  bad "the drill could not reach MinIO at all — this is the RIG, not the product"
elif [ -z "$PUBLISHED" ]; then
  note "hub tier status:"
  kubectl -n "$NS" exec deploy/"fs-$PROJECT-cold" -- \
    curl -sf "http://127.0.0.1:8080/status" 2>/dev/null | head -c 600
  echo
  note "whole bucket: $(s3 s3 ls "s3://$BUCKET/" --recursive | head -5)"
  bad "the cold volume never published to the bucket — nothing to hydrate FROM"
else
  note "bucket holds $(printf '%s' "$OUT" | grep -c .) object(s) under $PROJECT/cold/"
fi

# THE GAUGES ARE NESTED UNDER `tier.gauges`, NOT `tier`.
#
# `lite_operator/hubstatus.rs` says so in as many words. Run 6 of this
# drill read `tier.evictedFiles`, got None on every poll, timed out, read
# the file anyway and reported "8 MiB hydrated in 0s" — which is not a
# hydration, it is what a local-disk read looks like. A guard with a
# wrong path is worse than no guard: it makes a vacuous leg look
# careful.
evicted_now() {
  kubectl -n "$NS" exec deploy/"fs-$PROJECT-cold" -- \
    curl -sf "http://127.0.0.1:8080/status" 2>/dev/null \
    | python3 "$REPO_ROOT/tests/regression/lib/hub-gauge.py" tier.gauges.evictedFiles 2>/dev/null
}
EVICTED=""
for _ in $(seq 1 40); do
  EV=$(evicted_now | tr -d '\r\n')
  case "$EV" in ''|0|EMPTY|None|UNREADABLE*) ;; *) EVICTED="$EV"; break ;; esac
  sleep 3
done
if [ -n "$EVICTED" ]; then
  note "the hub reports $EVICTED evicted file(s) — the read below is a REAL hydration"
  # Belt and braces: an evicted file is a STUB on local disk. Logical
  # size stays truthful, allocated blocks go to zero. If both agree, the
  # bytes really are only in S3.
  BLOCKS=$(kubectl -n "$NS" exec deploy/"fs-$PROJECT-cold" -- \
    stat -c '%s %b' /data/exports/cold.bin 2>/dev/null | tr -d '\r')
  note "on the hub's disk: 'size blocks' = ${BLOCKS:-unavailable}"
else
  bad "eviction never confirmed via tier.gauges.evictedFiles — a hydration test that cannot prove the file left local disk measures NOTHING"
  note "hub tier gauges:"
  kubectl -n "$NS" exec deploy/"fs-$PROJECT-cold" -- \
    curl -sf "http://127.0.0.1:8080/status" 2>/dev/null | head -c 400; echo
fi

if [ -n "$MOUNT_OK" ] && [ -n "$PUBLISHED" ] && [ -n "$EVICTED" ]; then
  COLD_CIP=$(kubectl -n "$NS" get svc "fs-$PROJECT-cold" -o jsonpath='{.spec.clusterIP}')
  [ -n "$COLD_CIP" ] || fail "the cold share's Service has no ClusterIP"
  mkpv proj-a-cold "$COLD_CIP" "sec=sys"
  mkagent agent-cold proj-a-cold
  if kubectl -n "$NS" wait --for=condition=ready pod/agent-cold --timeout=180s >/dev/null 2>&1; then
    # MILLISECONDS. 8 MiB from an in-cluster object store lands well
    # inside one second, and `date +%s` renders that as "0s" — which
    # reads exactly like the read never happened. Against real S3 this
    # is seconds-to-minutes, so the same leg must be able to express
    # both ends.
    T0=$(python3 -c 'import time;print(int(time.time()*1000))')
    GOTSUM=$(kubectl -n "$NS" exec agent-cold -- sh -c \
      'sha256sum /workspace/cold.bin 2>/dev/null | cut -d" " -f1' 2>/dev/null | tr -d '\r\n')
    T1=$(python3 -c 'import time;print(int(time.time()*1000))')
    if [ "$GOTSUM" = "$COLDSUM" ]; then
      HYDRATE_MS=$((T1-T0))
      RATE=$(python3 -c "print('%.0f' % (${COLD_MB}/(max(${HYDRATE_MS},1)/1000.0)))" 2>/dev/null)
      pass "an agent HYDRATED ${COLD_MB} MiB from S3 through the mount in ${HYDRATE_MS}ms (~${RATE} MiB/s) — byte-identical to what REST wrote"
    else
      kubectl -n "$NS" exec agent-cold -- sh -c 'ls -la /workspace' 2>&1 | head -5
      bad "the hydrated read does NOT match (got '$GOTSUM', wanted '$COLDSUM')"
    fi
  else
    note "cold agent pod never became Ready"
    kubectl -n "$NS" describe pod agent-cold | sed -n '/Events:/,$p' | tail -8
  fi
else
  note "no mount available — L6 skipped"
fi

# ══ L8 (cluster mode only) ═══════════════════════════════════════════
if [ "$MODE" = cluster ] && [ -n "$MOUNT_OK" ]; then
say "L8: the ClusterIP mount works from a node that is NOT the hub's node"
# kind here is single-node, so every mount the kind runs ever made was
# node-local — the packet never crossed a node boundary and kube-proxy's
# ClusterIP path was never really exercised. The guide tells people to
# put a ClusterIP in a PV precisely because kube-proxy programs it on
# EVERY node; that claim is only tested when the client is somewhere
# else.
HUBNODE=$(kubectl -n "$NS" get pod -l flint.io/share="fs-$PROJECT-data" \
  -o jsonpath='{.items[0].spec.nodeName}' 2>/dev/null)
NODECOUNT=$(kubectl get nodes --no-headers 2>/dev/null | wc -l | tr -d ' ')
OTHER=$(kubectl get nodes --no-headers -o custom-columns=N:.metadata.name 2>/dev/null \
  | grep -v "^${HUBNODE}$" | head -1)
note "hub is on: ${HUBNODE:-unknown}   cluster has $NODECOUNT node(s)"
if [ -z "$OTHER" ]; then
  note "only one schedulable node — this leg needs two and is INCONCLUSIVE, not passed"
  L8_INCONCLUSIVE=1
else
  note "pinning a client to: $OTHER"
  kubectl -n "$NS" apply -f - >/dev/null <<EOF || bad "cross-node pod refused"
apiVersion: v1
kind: Pod
metadata: { name: agent-crossnode, namespace: $NS }
spec:
  restartPolicy: Never
  nodeName: $OTHER
  securityContext: { runAsUser: 1000, runAsGroup: 1000, fsGroup: 1000 }
  containers:
    - name: c
      image: alpine:3.20
      command: ["sh","-c","sleep 3600"]
      volumeMounts: [{ name: ws, mountPath: /workspace }]
  volumes:
    - name: ws
      persistentVolumeClaim: { claimName: proj-a-data }
EOF
  if kubectl -n "$NS" wait --for=condition=ready pod/agent-crossnode --timeout=240s >/dev/null 2>&1; then
    # Prove it is really the same filesystem, not an empty mount that
    # merely succeeded: read the file the REST door wrote in L5.
    XSEEN=$(kubectl -n "$NS" exec agent-crossnode -- sh -c 'cat /workspace/rest-written.txt 2>&1' | tr -d '\r')
    XFLAV=$(kubectl -n "$NS" exec agent-crossnode -- sh -c 'mount | grep /workspace | grep -o "sec=[a-z]*" | head -1' 2>/dev/null | tr -d '\r')
    case "$XSEEN" in
      *rest-write-*)
        pass "a pod on $OTHER mounted the hub on $HUBNODE via ClusterIP ($XFLAV) and sees the same filesystem" ;;
      *)
        kubectl -n "$NS" exec agent-crossnode -- sh -c 'ls -la /workspace' 2>&1 | head -5
        bad "the cross-node mount came up but does not show the REST-written file: $XSEEN" ;;
    esac
  else
    kubectl -n "$NS" describe pod agent-crossnode | sed -n '/Events:/,$p' | tail -10
    bad "a pod on a DIFFERENT node could not mount the ClusterIP — the guide's same-cluster advice does not hold here"
  fi
  kubectl -n "$NS" delete pod agent-crossnode --force --grace-period=0 >/dev/null 2>&1 &
fi
fi

# ══ L7 ═══════════════════════════════════════════════════════════════
say "L7: suspendWithSessions:false holds a mounted share up — with a control that must suspend"
# Mount BOTH shares, then wait past suspendAfterSecs. `held` must stay
# up; `free` must go down. Without the `free` control, "still Ready" is
# indistinguishable from an idle ladder that never fires at all.
# Free the earlier legs' agents first — their work is done and their
# mounts are pure load from here on. Consumers before servers, as ever.
kubectl -n "$NS" delete pod agent agent-cold --force --grace-period=0 >/dev/null 2>&1
mkshare "fs-$PROJECT-held" held "  idle:
    suspendAfterSecs: 30
    suspendWithSessions: false
  settings:
    flushFloorSecs: 3"
mkshare "fs-$PROJECT-free" free "  idle:
    suspendAfterSecs: 30
  settings:
    flushFloorSecs: 3"
chk "fs-$PROJECT-held" '{.spec.idle.suspendWithSessions}' false
chk "fs-$PROJECT-held" '{.spec.idle.suspendAfterSecs}' 30
chk "fs-$PROJECT-free" '{.spec.idle.suspendAfterSecs}' 30
for v in held free; do
  T=$(derive_for "$NS/fs-$PROJECT-$v")
  [ -n "$T" ] || { bad "--derive-for produced nothing for $v"; T=placeholder; }
  mksecret "$NS" "tok-$v" "token=$T"
done
L7_READY=1
for v in held free; do
  ph=""
  for _ in $(seq 1 48); do
    ph=$(kubectl -n "$NS" get flintshare "fs-$PROJECT-$v" -o jsonpath='{.status.phase}' 2>/dev/null)
    [ "$ph" = "Ready" ] && break
    sleep 5
  done
  [ "$ph" = "Ready" ] || { note "fs-$PROJECT-$v never became Ready (phase=$ph)"; L7_READY=""; }
done
if [ -n "$L7_READY" ]; then
  for v in held free; do
    VCIP=$(kubectl -n "$NS" get svc "fs-$PROJECT-$v" -o jsonpath='{.spec.clusterIP}')
    [ -n "$VCIP" ] || { bad "share $v has no ClusterIP"; L7_READY=""; break; }
    mkpv "proj-a-$v" "$VCIP" "sec=sys"
    mkagent "agent-$v" "proj-a-$v"
  done
fi
if [ -n "$L7_READY" ] \
   && kubectl -n "$NS" wait --for=condition=ready pod/agent-held --timeout=180s >/dev/null 2>&1 \
   && kubectl -n "$NS" wait --for=condition=ready pod/agent-free --timeout=180s >/dev/null 2>&1; then
  note "both shares mounted; going quiet for 150s (suspendAfterSecs=30)"
  # Deliberately touch NOTHING: this is the agent-thinking-in-memory
  # shape the guide warns about.
  sleep 150
  HELD_PH=$(kubectl -n "$NS" get flintshare "fs-$PROJECT-held" -o jsonpath='{.status.phase}')
  FREE_PH=$(kubectl -n "$NS" get flintshare "fs-$PROJECT-free" -o jsonpath='{.status.phase}')
  HELD_REP=$(kubectl -n "$NS" get deploy "fs-$PROJECT-held" -o jsonpath='{.spec.replicas}' 2>/dev/null)
  FREE_REP=$(kubectl -n "$NS" get deploy "fs-$PROJECT-free" -o jsonpath='{.spec.replicas}' 2>/dev/null)
  note "held: phase=$HELD_PH replicas=$HELD_REP   free: phase=$FREE_PH replicas=$FREE_REP"
  if [ "$FREE_PH" != "IdleSuspended" ] && [ "$FREE_REP" != "0" ]; then
    note "the CONTROL never suspended — the ladder did not fire at all in this window"
    note "so 'held stayed up' proves NOTHING here. Reporting INCONCLUSIVE rather than a pass."
    L7_INCONCLUSIVE=1
  elif [ "$HELD_REP" = "0" ] || [ "$HELD_PH" = "IdleSuspended" ]; then
    bad "suspendWithSessions:false did NOT hold the share up — it suspended under a live mount"
  else
    pass "the control suspended (free: $FREE_PH/$FREE_REP) and the protected share stayed up (held: $HELD_PH/$HELD_REP)"
  fi
else
  note "could not mount both shares; L7 skipped"
  L7_INCONCLUSIVE=1
fi

# ══ summary ══════════════════════════════════════════════════════════
echo
echo "══════════════════════════════════════════════════════════════════"
echo " doc drill summary — $PASSES checks passed   [MODE=$MODE]"
echo "══════════════════════════════════════════════════════════════════"
echo " node ships mount.nfs4               : $([ "${NODE_HAS_NFS:-0}" = "1" ] && echo 'yes (a kubelet mount proves it)' || echo 'NOT AVAILABLE')"
echo " .svc.cluster.local server HANGS     : ${DNS_HANGS:-?}  (1 = the guide's warning is proven)"
echo " omitting sec=sys ⇒ everyone is root : ${SECNULL_PROVEN:-?}  (1 = proven by control)"
echo " unprivileged pod mounted the PV     : $([ -n "${MOUNT_OK:-}" ] && echo yes || echo NO)"
echo " export root writable by uid 1000    : ${ROOTDIR_WRITABLE:-?}"
echo " REST->mount new file visible after  : ${REST_VISIBLE_SECS:-?}s"
echo " REST->mount overwrite visible after : ${OVERWRITE_SECS:-?}s"
if [ "$MODE" = cluster ]; then
  echo " ${COLD_MB} MiB hydrate via mount (REAL S3) : ${HYDRATE_MS:-?}ms — THIS IS A RATE"
  echo " cross-node ClusterIP mount          : $([ -n "${L8_INCONCLUSIVE:-}" ] && echo 'INCONCLUSIVE (one node)' || echo 'tested')"
else
  echo " ${COLD_MB} MiB hydrate via mount        : ${HYDRATE_MS:-?}ms (in-cluster MinIO — NOT a rate)"
fi
echo " suspend-with-sessions leg           : $([ -n "${L7_INCONCLUSIVE:-}" ] && echo 'INCONCLUSIVE' || echo 'conclusive')"
echo
echo "These numbers are what the guide should carry. Anything marked ?"
echo "was not measured and must not be written down as if it were."
echo
if [ ${#FAILURES[@]} -eq 0 ]; then
  echo "ALL LEGS PASSED."
else
  echo "${#FAILURES[@]} LEG(S) FAILED:"
  for f in "${FAILURES[@]}"; do echo "  ✗ $f"; done
  exit 1
fi
