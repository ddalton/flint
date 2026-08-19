#!/usr/bin/env bash
# flint-lite OPERATOR kind e2e — the FlintShare control plane, on a real
# API server, with a real kernel client.
#
# The unit suite proves the decisions; this proves the things only a
# cluster can refuse: that the API server ACCEPTS the generated CRD
# (structural schema, CEL rules), that the RBAC is complete, that
# server-side apply converges, that the finalizer honours reclaim, and
# that adoption of a chart release does not double-mount a claim.
#
# Legs:
#   1  operator chart installs; the CRD is Established and stamped
#   2  a FlintShare becomes Ready — children exist with the right
#      ownership (three owned; the PVC deliberately NOT owned) — and a
#      Lima kernel client mounts it and runs the agent battery
#   3  identity immutability is refused BY THE API SERVER (CEL), and an
#      unknown settings key is refused too (the whole point of a schema)
#   4  a settings edit rolls the hub exactly once and the new config is
#      live in the pod
#   5  conflict: a second share on a nested prefix, in another
#      namespace, is Failed with a Conflict condition and NO Deployment;
#      deleting the winner promotes it
#   6  the operator repairs a hand-mangled CRD schema on restart
#   7  reclaim: Retain keeps the PVC when the share is deleted, and the
#      three owned children are garbage collected; Delete removes it
#   8  adoption: a share adopts a live helm release IN PLACE (one
#      Deployment, same PVC, same data), and a differently-named share
#      is fenced with AdoptionBlocked instead of double-mounting
#
# Images are built from the WORKING TREE, so this always tests the code
# you are sitting on. KEEP=1 leaves the cluster standing.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OP_CHART="$REPO_ROOT/flint-lite-operator-chart"
LITE_CHART="$REPO_ROOT/flint-lite-chart"
CARGO_DIR="$REPO_ROOT/spdk-csi-driver"

CLUSTER="${CLUSTER:-flint-op-e2e}"
OPNS=flint-system
NS=workspaces
NS2=team-b
CHARTNS=legacy
HUBIMG=flint-lite-dev:local
OPIMG=flint-lite-operator-dev:local
NODEPORT=32050
LIMA_VM="${LIMA_VM:-flint-nfs-client}"
MNT=/mnt/op-kind
KUBECONFIG_FILE="$(mktemp -t flint-op-kubeconfig.XXXXXX)"
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
  # Umount FIRST: a dead server under a live mount D-states umount.
  vm "umount -lf $MNT 2>/dev/null" 2>/dev/null
  kind delete cluster --name "$CLUSTER" >/dev/null 2>&1
  rm -f "$KUBECONFIG_FILE"
}
trap cleanup EXIT

echo "══════════════════════════════════════════════════════════════════"
echo " flint-lite operator kind e2e — FlintShare on a real API server"
echo "══════════════════════════════════════════════════════════════════"

for t in kind kubectl helm docker limactl cargo; do
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

# ── 0. build both images from the working tree ───────────────────────
say "building flint-pnfs-mds + flint-lite-operator ($TRIPLE)"
(cd "$CARGO_DIR" && cargo zigbuild --release --target "$TRIPLE" \
   --bin flint-pnfs-mds --bin flint-lite-operator >/tmp/op-e2e-build.log 2>&1) \
  || { tail -20 /tmp/op-e2e-build.log; fail "zigbuild failed"; }
IMGDIR=$(mktemp -d -t flint-op-img.XXXXXX)
cp "$CARGO_DIR/target/$TRIPLE/release/flint-pnfs-mds" "$IMGDIR/"
cp "$CARGO_DIR/target/$TRIPLE/release/flint-lite-operator" "$IMGDIR/"
cat >"$IMGDIR/Dockerfile.hub" <<'EOF'
FROM alpine:3.20
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
  >/tmp/op-e2e-img.log 2>&1 || { tail -5 /tmp/op-e2e-img.log; fail "hub image build failed"; }
docker build --platform "$PLATFORM" -f "$IMGDIR/Dockerfile.op" -t "$OPIMG" "$IMGDIR" \
  >>/tmp/op-e2e-img.log 2>&1 || { tail -5 /tmp/op-e2e-img.log; fail "operator image build failed"; }
rm -rf "$IMGDIR"
pass "images $HUBIMG and $OPIMG built ($PLATFORM)"

say "creating kind cluster '$CLUSTER' (hostPort $NODEPORT → NodePort)"
KIND_CFG=$(mktemp -t flint-op-kind.XXXXXX.yaml)
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
for i in "$HUBIMG" "$OPIMG"; do
  kind load docker-image "$i" --name "$CLUSTER" >/dev/null 2>&1 || fail "kind load $i failed"
done
pass "cluster up, images loaded"

# ── 1. the operator installs and its CRD is served ───────────────────
say "leg 1: helm install the operator; CRD Established and stamped"
helm install flint-lite-operator "$OP_CHART" -n "$OPNS" --create-namespace \
  --set image.ref="$OPIMG" --set hubImage="$HUBIMG" \
  >/tmp/op-e2e-helm.log 2>&1 || { tail -20 /tmp/op-e2e-helm.log; fail "helm install failed"; }
kubectl -n "$OPNS" rollout status deployment/flint-lite-operator --timeout=120s >/dev/null 2>&1 \
  || { kubectl -n "$OPNS" describe pod -l app.kubernetes.io/name=flint-lite-operator | tail -20; \
       fail "operator never became Ready"; }
kubectl wait --for=condition=established --timeout=60s crd/flintshares.flint.io >/dev/null 2>&1 \
  || fail "the CRD never became Established — the API server refused the schema"
STAMP=$(kubectl get crd flintshares.flint.io -o jsonpath='{.metadata.annotations.flint\.io/crd-schema-version}')
[ -n "$STAMP" ] || fail "CRD carries no flint.io/crd-schema-version annotation"
# The operator must have claimed field ownership of the CRD — i.e. it
# really applied its own copy, rather than passively accepting the
# chart's. --show-managed-fields is load-bearing: kubectl STRIPS
# managedFields from get output by default, so without it this check
# fails no matter what the operator did.
# jsonpath, not a grep of -o json: kubectl pretty-prints JSON with a
# space after the colon, so '"manager":"x"' never matches.
managers() { kubectl get crd flintshares.flint.io --show-managed-fields \
  -o jsonpath='{.metadata.managedFields[*].manager}' 2>/dev/null; }
for i in $(seq 1 30); do
  case " $(managers) " in *" flint-lite-operator "*) break ;; esac
  sleep 2
done
case " $(managers) " in
  *" flint-lite-operator "*) ;;
  *) kubectl -n "$OPNS" logs deployment/flint-lite-operator --tail=30
     fail "the operator never applied the CRD itself (managers: $(managers))" ;;
esac
pass "operator Ready; CRD established at schema version $STAMP, applied by the operator"

# ── 2. a share, end to end ───────────────────────────────────────────
say "leg 2: a FlintShare becomes Ready; ownership is right; a kernel client mounts it"
kubectl create namespace "$NS" >/dev/null
kubectl apply -f - >/dev/null <<EOF || fail "applying the FlintShare failed"
apiVersion: flint.io/v1alpha1
kind: FlintShare
metadata:
  name: tenant-a
  namespace: $NS
spec:
  persistence:
    size: 2Gi
  service:
    type: NodePort
    nodePort: $NODEPORT
EOF
for i in $(seq 1 60); do
  PHASE=$(kubectl -n "$NS" get flintshare tenant-a -o jsonpath='{.status.phase}' 2>/dev/null)
  [ "$PHASE" = "Ready" ] && break
  sleep 2
done
[ "${PHASE:-}" = "Ready" ] || {
  kubectl -n "$NS" get flintshare tenant-a -o yaml | tail -30
  kubectl -n "$NS" get pods
  kubectl -n "$OPNS" logs deployment/flint-lite-operator --tail=40
  fail "share never reached Ready (last phase: ${PHASE:-<none>})"
}
ADDR=$(kubectl -n "$NS" get flintshare tenant-a -o jsonpath='{.status.address}')
[ "$ADDR" = "tenant-a.$NS.svc.cluster.local:2049" ] || fail "unexpected status.address '$ADDR'"

for kind_ in deployment service configmap; do
  OWNER=$(kubectl -n "$NS" get "$kind_" \
    "$( [ "$kind_" = configmap ] && echo tenant-a-config || echo tenant-a )" \
    -o jsonpath='{.metadata.ownerReferences[0].kind}' 2>/dev/null)
  [ "$OWNER" = "FlintShare" ] || fail "$kind_ is not owned by the FlintShare (got '$OWNER')"
done
# The invariant that keeps Retain honest: no ownerReference at all.
PVC_OWNER=$(kubectl -n "$NS" get pvc tenant-a-data -o jsonpath='{.metadata.ownerReferences}')
[ -z "$PVC_OWNER" ] || fail "the PVC carries an ownerReference — owner GC would ignore reclaim: Retain"
pass "Ready at $ADDR; three children owned, PVC deliberately unowned"

HOST_IP=$(vm "getent hosts host.lima.internal | awk '{print \$1}'" | tr -d '\r')
[ -n "$HOST_IP" ] || fail "could not resolve host.lima.internal in the VM"
vm "mountpoint -q $MNT && umount -lf $MNT; mkdir -p $MNT; \
    timeout 30 mount -t nfs4 -o minorversion=1,proto=tcp,port=$NODEPORT $HOST_IP:/ $MNT" \
  || fail "mount through the NodePort chain failed"
vm "dd if=/dev/urandom of=$MNT/shared.bin bs=1M count=8 status=none conv=fsync" || fail "write failed"
MD5_W=$(vm "md5sum $MNT/shared.bin" | awk '{print $1}' | tr -d '\r')
vm "echo 3 > /proc/sys/vm/drop_caches"
MD5_R=$(vm "md5sum $MNT/shared.bin" | awk '{print $1}' | tr -d '\r')
[ "$MD5_W" = "$MD5_R" ] || fail "cold reread $MD5_R != written $MD5_W"
if vm "command -v git >/dev/null"; then
  vm "cd $MNT && rm -rf repo && mkdir repo && cd repo && timeout 120 git init -q && \
      echo one >f && timeout 120 git add f && \
      timeout 120 git -c user.email=a@op -c user.name=a commit -qm op-e2e" \
    || fail "git battery failed"
fi
pass "kernel client mounted an operator-created share; 8 MiB byte-identical; git ok"

# ── 3. the schema does its job ───────────────────────────────────────
say "leg 3: the API server refuses an identity change and an unknown knob"
if kubectl -n "$NS" patch flintshare tenant-a --type=merge \
     -p '{"spec":{"keyPrefix":"somewhere-else/"}}' >/tmp/op-e2e-cel.log 2>&1; then
  fail "keyPrefix was mutable — the CEL immutability rule is not enforced"
fi
grep -qi "immutable" /tmp/op-e2e-cel.log \
  || { cat /tmp/op-e2e-cel.log; fail "refusal did not name immutability"; }
# An unknown knob must not be silently pruned into oblivion: with a
# typed schema it is refused outright.
# The share carries a bucket so the ONLY thing wrong with it is the
# misspelling — otherwise the CEL rule "settings needs a bucket" would
# refuse it and the test would pass without testing anything.
if kubectl apply --server-side --dry-run=server -f - >/tmp/op-e2e-typo.log 2>&1 <<EOF
apiVersion: flint.io/v1alpha1
kind: FlintShare
metadata:
  name: typo
  namespace: $NS
spec:
  bucket: some-bucket
  keyPrefix: typo/
  persistence: { size: 1Gi }
  settings:
    watermarkPCT: 90
EOF
then
  fail "a misspelled knob (watermarkPCT) was ACCEPTED — the schema is not doing its job"
fi
# Server-side apply says "field not declared in schema"; client-side
# validation says "unknown field". Either is the schema doing its job;
# neither is the CEL rules or a coincidence.
grep -qiE "field not declared in schema|unknown field" /tmp/op-e2e-typo.log \
  || { cat /tmp/op-e2e-typo.log; fail "the typo was refused, but not for being an undeclared field"; }
# ... and the correctly-spelled one is accepted, so the refusal above
# is about the SPELLING and not about settings being unusable.
kubectl apply --server-side --dry-run=server -f - >/dev/null 2>&1 <<EOF || fail "a VALID settings block was refused"
apiVersion: flint.io/v1alpha1
kind: FlintShare
metadata:
  name: typo
  namespace: $NS
spec:
  bucket: some-bucket
  keyPrefix: typo/
  persistence: { size: 1Gi }
  settings:
    watermarkPct: 90
EOF
pass "identity is immutable and a misspelled knob is refused, both by the API server"

# ── 4. a settings edit reaches the running hub ───────────────────────
say "leg 4: an edit rolls the hub exactly once and the new config is live"
GEN_BEFORE=$(kubectl -n "$NS" get deployment tenant-a -o jsonpath='{.metadata.generation}')
kubectl -n "$NS" patch flintshare tenant-a --type=merge -p '{"spec":{"logLevel":"debug"}}' >/dev/null
for i in $(seq 1 45); do
  LIVE=$(kubectl -n "$NS" get cm tenant-a-config -o jsonpath='{.data.mds\.yaml}' 2>/dev/null | grep -c "level: debug")
  [ "${LIVE:-0}" -ge 1 ] && break
  sleep 2
done
[ "${LIVE:-0}" -ge 1 ] || fail "the ConfigMap never picked up logLevel: debug"
kubectl -n "$NS" rollout status deployment/tenant-a --timeout=180s >/dev/null 2>&1 \
  || fail "the hub never rolled after the settings edit"
GEN_AFTER=$(kubectl -n "$NS" get deployment tenant-a -o jsonpath='{.metadata.generation}')
[ "$GEN_AFTER" -gt "$GEN_BEFORE" ] || fail "the Deployment never changed — nothing would have restarted the hub"
POD_CFG=$(kubectl -n "$NS" exec deployment/tenant-a -- sh -c "grep -c 'level: debug' /etc/flint/mds.yaml" 2>/dev/null | tr -d ' \r')
[ "${POD_CFG:-0}" -ge 1 ] || fail "the RUNNING pod does not have the new config"
pass "edit → ConfigMap → one roll → live in the pod (generation $GEN_BEFORE → $GEN_AFTER)"

# ── 5. fleet uniqueness ──────────────────────────────────────────────
say "leg 5: a second share on the same bucket subtree is refused, across namespaces"
kubectl create namespace "$NS2" >/dev/null
kubectl apply -f - >/dev/null <<EOF || fail "applying the winner failed"
apiVersion: flint.io/v1alpha1
kind: FlintShare
metadata: { name: owner, namespace: $NS }
spec:
  bucket: shared-bucket
  keyPrefix: tenant-x/
  persistence: { size: 1Gi }
EOF
sleep 3   # creationTimestamp has 1s granularity; make the winner unambiguous
kubectl apply -f - >/dev/null <<EOF || fail "applying the loser failed"
apiVersion: flint.io/v1alpha1
kind: FlintShare
metadata: { name: intruder, namespace: $NS2 }
spec:
  bucket: shared-bucket
  keyPrefix: tenant-x/nested/
  persistence: { size: 1Gi }
EOF
for i in $(seq 1 45); do
  P=$(kubectl -n "$NS2" get flintshare intruder -o jsonpath='{.status.phase}' 2>/dev/null)
  [ "$P" = "Failed" ] && break
  sleep 2
done
[ "${P:-}" = "Failed" ] || { kubectl -n "$NS2" get flintshare intruder -o yaml | tail -25; \
  fail "a nested-prefix duplicate in another namespace was NOT refused (phase ${P:-<none>})"; }
WINNER=$(kubectl -n "$NS2" get flintshare intruder \
  -o jsonpath='{.status.conditions[?(@.type=="Conflict")].message}')
echo "$WINNER" | grep -q "$NS/owner" || fail "the Conflict condition does not name the winner: $WINNER"
kubectl -n "$NS2" get deployment intruder >/dev/null 2>&1 \
  && fail "the loser got a Deployment — that is the hub that takes the prefix over"
pass "loser Failed with a Conflict naming $NS/owner, and no Deployment at all"

kubectl -n "$NS" delete flintshare owner --wait=true >/dev/null 2>&1
for i in $(seq 1 45); do
  P=$(kubectl -n "$NS2" get flintshare intruder -o jsonpath='{.status.phase}' 2>/dev/null)
  [ "$P" != "Failed" ] && break
  sleep 2
done
[ "${P:-}" != "Failed" ] || fail "deleting the winner did not promote the survivor (still $P)"
kubectl -n "$NS2" delete flintshare intruder --wait=true >/dev/null 2>&1
pass "deleting the winner promoted the survivor"

# ── 6. the operator repairs its own CRD ──────────────────────────────
say "leg 6: a hand-mangled CRD schema is repaired on operator restart"
# `logLevel`, not `settings`: the API server REFUSES to remove a
# property a CEL rule references ("undefined field 'settings'" — the
# rules pin their own fields, which is a good property and an
# inconvenient one here). logLevel is pinned by nothing.
knob_type() { kubectl get crd flintshares.flint.io \
  -o jsonpath='{.spec.versions[0].schema.openAPIV3Schema.properties.spec.properties.logLevel.type}' 2>/dev/null; }
[ "$(knob_type)" = "string" ] || fail "spec.logLevel is not in the served schema to begin with"
kubectl patch crd flintshares.flint.io --type=json \
  -p '[{"op":"remove","path":"/spec/versions/0/schema/openAPIV3Schema/properties/spec/properties/logLevel"}]' \
  >/dev/null || fail "could not mangle the CRD for the test"
[ -z "$(knob_type)" ] || fail "the mangle did not take"
kubectl -n "$OPNS" delete pod -l app.kubernetes.io/name=flint-lite-operator --wait=false >/dev/null
kubectl -n "$OPNS" rollout status deployment/flint-lite-operator --timeout=120s >/dev/null 2>&1 \
  || fail "operator never came back"
for i in $(seq 1 30); do
  [ "$(knob_type)" = "string" ] && break
  sleep 2
done
[ "$(knob_type)" = "string" ] \
  || fail "the operator did NOT restore the stripped schema — that field would be pruned on every apply, silently"
pass "stripped property (spec.logLevel) restored by the operator at startup"

# ── 7. reclaim ───────────────────────────────────────────────────────
say "leg 7: reclaim Retain keeps the PVC; Delete removes it"
vm "umount -lf $MNT 2>/dev/null" 2>/dev/null
kubectl -n "$NS" delete flintshare tenant-a --wait=true >/dev/null 2>&1 \
  || fail "deleting the share hung (a finalizer that never releases)"
for i in $(seq 1 30); do
  kubectl -n "$NS" get deployment tenant-a >/dev/null 2>&1 || break
  sleep 2
done
kubectl -n "$NS" get deployment tenant-a >/dev/null 2>&1 \
  && fail "the Deployment survived the CR — owner GC did not run"
PVC_PHASE=$(kubectl -n "$NS" get pvc tenant-a-data -o jsonpath='{.status.phase}' 2>/dev/null)
[ "$PVC_PHASE" = "Bound" ] \
  || fail "reclaim: Retain LOST the PVC (phase '${PVC_PHASE:-gone}') — this is the data-loss case"
pass "children collected, PVC still Bound (the default keeps your data)"

kubectl apply -f - >/dev/null <<EOF || fail "applying the reclaim: Delete share failed"
apiVersion: flint.io/v1alpha1
kind: FlintShare
metadata: { name: ephemeral, namespace: $NS }
spec:
  reclaim: Delete
  persistence: { size: 1Gi }
EOF
for i in $(seq 1 45); do
  kubectl -n "$NS" get pvc ephemeral-data >/dev/null 2>&1 && break
  sleep 2
done
kubectl -n "$NS" get pvc ephemeral-data >/dev/null 2>&1 || fail "the PVC was never created"
kubectl -n "$NS" delete flintshare ephemeral --wait=true >/dev/null 2>&1
for i in $(seq 1 45); do
  kubectl -n "$NS" get pvc ephemeral-data >/dev/null 2>&1 || break
  sleep 2
done
kubectl -n "$NS" get pvc ephemeral-data >/dev/null 2>&1 \
  && fail "reclaim: Delete left the PVC behind"
pass "reclaim: Delete removed the claim"

# ── 8. adoption ──────────────────────────────────────────────────────
say "leg 8: adopting a live helm release in place"
helm install flint-lite "$LITE_CHART" -n "$CHARTNS" --create-namespace \
  --set image.ref="$HUBIMG" --set persistence.size=1Gi \
  >/tmp/op-e2e-lite.log 2>&1 || { tail -10 /tmp/op-e2e-lite.log; fail "lite chart install failed"; }
kubectl -n "$CHARTNS" rollout status deployment/flint-lite --timeout=120s >/dev/null 2>&1 \
  || fail "the chart's hub never became Ready"
kubectl -n "$CHARTNS" exec deployment/flint-lite -- \
  sh -c 'echo adopted-data > /data/exports/marker.txt' || fail "could not write the marker"
PVC_UID=$(kubectl -n "$CHARTNS" get pvc flint-lite-data -o jsonpath='{.metadata.uid}')

# A differently-named share must be FENCED, not allowed to create a
# second Deployment on the same RWO claim.
kubectl apply -f - >/dev/null <<EOF || fail "applying the blocked share failed"
apiVersion: flint.io/v1alpha1
kind: FlintShare
metadata: { name: tenant-z, namespace: $CHARTNS }
spec:
  existingClaim: flint-lite-data
  persistence: { size: 1Gi }
EOF
for i in $(seq 1 30); do
  B=$(kubectl -n "$CHARTNS" get flintshare tenant-z \
      -o jsonpath='{.status.conditions[?(@.type=="AdoptionBlocked")].status}' 2>/dev/null)
  [ "$B" = "True" ] && break
  sleep 2
done
[ "${B:-}" = "True" ] || { kubectl -n "$CHARTNS" get flintshare tenant-z -o yaml | tail -25; \
  fail "a second Deployment on a live claim was NOT fenced"; }
kubectl -n "$CHARTNS" get deployment tenant-z >/dev/null 2>&1 \
  && fail "the fenced share created a Deployment anyway — two sqlite writers on one state.db"
kubectl -n "$CHARTNS" delete flintshare tenant-z --wait=true >/dev/null 2>&1
pass "a differently-named share was fenced with AdoptionBlocked, and created nothing"

# In-place adoption: the CR is NAMED like the release, so the operator
# applies over the very objects helm made.
kubectl apply -f - >/dev/null <<EOF || fail "applying the adopting share failed"
apiVersion: flint.io/v1alpha1
kind: FlintShare
metadata: { name: flint-lite, namespace: $CHARTNS }
spec:
  existingClaim: flint-lite-data
  persistence: { size: 1Gi }
EOF
for i in $(seq 1 60); do
  P=$(kubectl -n "$CHARTNS" get flintshare flint-lite -o jsonpath='{.status.phase}' 2>/dev/null)
  [ "$P" = "Ready" ] && break
  sleep 2
done
[ "${P:-}" = "Ready" ] || { kubectl -n "$CHARTNS" get flintshare flint-lite -o yaml | tail -30; \
  kubectl -n "$CHARTNS" get pods; fail "the adopting share never reached Ready (phase ${P:-<none>})"; }
DEPS=$(kubectl -n "$CHARTNS" get deployments -o name | wc -l | tr -d ' ')
[ "$DEPS" = "1" ] || fail "$DEPS Deployments in $CHARTNS — adoption must never create a second one"
PVC_UID2=$(kubectl -n "$CHARTNS" get pvc flint-lite-data -o jsonpath='{.metadata.uid}')
[ "$PVC_UID" = "$PVC_UID2" ] || fail "the claim was replaced ($PVC_UID → $PVC_UID2)"
MARKER=$(kubectl -n "$CHARTNS" exec deployment/flint-lite -- sh -c 'cat /data/exports/marker.txt' 2>/dev/null | tr -d '\r')
[ "$MARKER" = "adopted-data" ] || fail "the adopted hub does not see the pre-adoption data ('$MARKER')"
pass "adopted in place: one Deployment, same PVC ($PVC_UID2), data intact"

echo
echo "══════════════════════════════════════════════════════════════════"
echo " PASS — the operator installed its own CRD, served a kernel client"
echo " through a FlintShare, refused an identity change and a typo'd"
echo " knob at admission, rolled a hub on a settings edit, refused a"
echo " duplicate bucket subtree across namespaces, repaired a mangled"
echo " schema, kept a Retain PVC through deletion, and adopted a live"
echo " helm release without ever double-mounting its claim."
echo "══════════════════════════════════════════════════════════════════"
