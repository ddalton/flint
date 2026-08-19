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
#   3  the mount's nconnect is REAL: >=2 established connections, and
#      the option is on the mount rather than silently dropped
#   4  identity immutability is refused BY THE API SERVER (CEL), and an
#      unknown settings key is refused too (the whole point of a schema)
#   5  advertiseAddress: a bare host and unbracketed IPv6 are refused at
#      admission; a valid one lands in status.address VERBATIM without
#      touching the Service or the live mount; clearing it reverts
#   6  a settings edit rolls the hub exactly once and the new config is
#      live in the pod
#   7  conflict: a second share on a nested prefix, in another
#      namespace, is Failed with a Conflict condition and NO Deployment;
#      deleting the winner promotes it
#   8  the operator repairs a hand-mangled CRD schema on restart
#   9  the hub's own HTTP surface: /status answers on the pod IP and
#      reports rpoClean as null (never true) with no bucket, and the
#      file API round-trips bytes with the kernel mount in BOTH
#      directions while refusing an unauthenticated caller and a
#      symlink out of the export
#  10  suspendWithSessions holds a share awake while a client is
#      mounted, even after the hub's own clock says idle — the signal
#      that tells "everyone went home" from "everyone is blocked"
#  11  the idle ladder: a quiet share suspends to replicas 0 while
#      KEEPING its PVC, the suspend survives later reconciles (the
#      annotation carrier), stamping the ladder's wake input brings it
#      back, and an admin's spec.lifecycle: Suspended outranks it
#  12  reclaim: Retain keeps the PVC when the share is deleted, and the
#      three owned children are garbage collected; Delete removes it
#  13  adoption: a share adopts a live helm release IN PLACE (one
#      Deployment, same PVC, same data), and a differently-named share
#      is fenced with AdoptionBlocked instead of double-mounting
#
# Legs 9-11 run BEFORE reclaim on purpose: they all drive tenant-a and
# its live kernel mount, and leg 12 is the leg that deletes them.
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
# The client MUST ask for trunking explicitly: without nconnect>=2 the
# kernel silently opens ONE connection and every trunk is refused with
# no error anywhere. Asserted in leg 3 rather than assumed.
NCONNECT=4
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
# curl is for the TEST, not the product: leg 9 drives the file API from
# inside the pod, and alpine's wget is BusyBox's — it cannot issue PUT
# or DELETE at all. The shipped hub image has no curl and needs none.
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
    timeout 30 mount -t nfs4 -o minorversion=1,proto=tcp,nconnect=$NCONNECT,port=$NODEPORT $HOST_IP:/ $MNT" \
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

# ── 3. the trunking the mount asked for ──────────────────────────────
say "leg 3: nconnect actually opened more than one connection"
# The scar this exists for: a mount WITHOUT nconnect>=2 gets exactly one
# TCP connection, every trunk is silently refused, and nothing logs it —
# on either side. The mount succeeds, I/O works, and the parallelism you
# configured is simply absent. So the transport count is asserted from
# the client, where `ss` is the only honest witness.
#
# Sampled MID-FLIGHT: an idle NFSv4.1 client tears extra connections
# down, so counting after the I/O settles measures the teardown, not the
# mount. The reader below runs in the background and the census happens
# while it is still going.
vm "nohup sh -c 'dd if=$MNT/shared.bin of=/dev/null bs=1M count=8 iflag=direct' \
    >/dev/null 2>&1 &" || true
CONNS=0
for i in $(seq 1 20); do
  CONNS=$(vm "ss -tn state established \"( dport = :$NODEPORT )\" 2>/dev/null | grep -c ':'" \
          | tr -d '\r' | tail -1)
  [ "${CONNS:-0}" -ge 2 ] && break
  vm "dd if=$MNT/shared.bin of=/dev/null bs=1M count=4 iflag=direct >/dev/null 2>&1" || true
done
[ "${CONNS:-0}" -ge 2 ] || {
  vm "ss -tn | head -20"
  vm "findmnt -no OPTIONS $MNT"
  fail "nconnect=$NCONNECT produced ${CONNS:-0} connection(s) — the kernel refused the trunks silently"
}
# And the option must actually be ON the mount, not merely tolerated:
# a kernel that dropped it would still show one connection above and
# could pass a >=2 check by coincidence on a busy link.
OPTS=$(vm "findmnt -no OPTIONS $MNT" | tr -d '\r')
echo "$OPTS" | grep -q "nconnect=$NCONNECT" \
  || fail "the mount does not carry nconnect=$NCONNECT (options: $OPTS)"
pass "nconnect=$NCONNECT is on the mount and $CONNS connections are established"

# ── 4. the schema does its job ───────────────────────────────────────
say "leg 4: the API server refuses an identity change and an unknown knob"
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

# ── 5. the advertised address ────────────────────────────────────────
say "leg 5: advertiseAddress is what status.address reports, verbatim"
# The ONLY way a consumer in another cluster gets a mountable address.
# Everything the operator can derive is in-cluster-only except a
# LoadBalancer's ingress, and NodePort is the trap: it resolves to the
# .svc DNS name, so a foreign client reads status.address, mounts it,
# and fails on a name it cannot resolve.

# Malformed is refused BY THE API SERVER, not discovered as a failed
# mount in somebody else's cluster. A bare host is the dangerous shape:
# an NFS client given one silently uses 2049, which is exactly wrong
# for the port-per-project layout this exists to serve.
if kubectl -n "$NS" patch flintshare tenant-a --type=merge \
     -p '{"spec":{"service":{"advertiseAddress":"hub.example.internal"}}}' \
     >/tmp/op-e2e-adv.log 2>&1; then
  fail "advertiseAddress without a port was ACCEPTED — a client would silently use 2049"
fi
grep -qi "host:port" /tmp/op-e2e-adv.log \
  || { cat /tmp/op-e2e-adv.log; fail "the refusal did not say what shape is wanted"; }
# Unbracketed IPv6 is refused too: its colons make the port ambiguous.
if kubectl -n "$NS" patch flintshare tenant-a --type=merge \
     -p '{"spec":{"service":{"advertiseAddress":"2001:db8::1:2049"}}}' >/dev/null 2>&1; then
  fail "unbracketed IPv6 was accepted — host and port are not separable"
fi
pass "a bare host and unbracketed IPv6 are both refused at admission"

# The positive case: verbatim into status.address.
ADV="hub-a.corp.internal:2149"
kubectl -n "$NS" patch flintshare tenant-a --type=merge \
  -p "{\"spec\":{\"service\":{\"advertiseAddress\":\"$ADV\"}}}" >/dev/null \
  || fail "a valid advertiseAddress was refused"
for i in $(seq 1 30); do
  A=$(kubectl -n "$NS" get flintshare tenant-a -o jsonpath='{.status.address}' 2>/dev/null)
  [ "$A" = "$ADV" ] && break
  sleep 2
done
[ "$A" = "$ADV" ] || fail "status.address is '$A', not the advertised '$ADV'"
# The Service itself must be untouched — this changes what is
# ADVERTISED, not what is created, so the in-cluster path keeps working.
SVC_TYPE=$(kubectl -n "$NS" get svc tenant-a -o jsonpath='{.spec.type}')
[ "$SVC_TYPE" = "NodePort" ] || fail "the Service changed type to '$SVC_TYPE' — advertising is not provisioning"
vm "timeout 30 stat $MNT/shared.bin" >/dev/null 2>&1 \
  || fail "the existing in-cluster mount broke when the advertised address changed"
pass "status.address is '$ADV' verbatim; Service and live mount untouched"

# And it reverts, so a share that moves back in-cluster is not stranded
# advertising an address nobody serves any more.
kubectl -n "$NS" patch flintshare tenant-a --type=json \
  -p '[{"op":"remove","path":"/spec/service/advertiseAddress"}]' >/dev/null \
  || fail "could not clear advertiseAddress"
for i in $(seq 1 30); do
  A=$(kubectl -n "$NS" get flintshare tenant-a -o jsonpath='{.status.address}' 2>/dev/null)
  [ "$A" = "tenant-a.$NS.svc.cluster.local:2049" ] && break
  sleep 2
done
[ "$A" = "tenant-a.$NS.svc.cluster.local:2049" ] \
  || fail "clearing advertiseAddress left status.address at '$A'"
pass "clearing it reverts to the derived in-cluster address"

# ── 6. a settings edit reaches the running hub ───────────────────────
say "leg 6: an edit rolls the hub exactly once and the new config is live"
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

# ── 7. fleet uniqueness ──────────────────────────────────────────────
say "leg 7: a second share on the same bucket subtree is refused, across namespaces"
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

# ── 8. the operator repairs its own CRD ──────────────────────────────
say "leg 8: a hand-mangled CRD schema is repaired on operator restart"
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

# ── 9. the hub's HTTP surface: status, and files without a mount ─────
say "leg 9: /status answers, and the file API round-trips without any mount"
TOKEN=$(head -c 24 /dev/urandom | base64 | tr -d '=+/' | head -c 24)
kubectl -n "$NS" create secret generic api-token --from-literal=token="$TOKEN" >/dev/null
kubectl -n "$NS" patch flintshare tenant-a --type=merge -p "{
  \"spec\": { \"monitoring\": { \"enabled\": true, \"port\": 8080,
    \"fileApi\": { \"enabled\": true, \"tokenSecretRef\": \"api-token\" } } } }" >/dev/null \
  || fail "enabling the file API was refused"
kubectl -n "$NS" rollout status deployment/tenant-a --timeout=180s >/dev/null 2>&1 \
  || fail "the hub never rolled with the file API on"

# Everything below runs INSIDE the cluster, against the pod IP. The
# status port is deliberately not on the Service, so this is the only
# way to reach it — which is the property being asserted.
HUBPOD=$(kubectl -n "$NS" get pods -l flint.io/share=tenant-a \
  --field-selector=status.phase=Running -o jsonpath='{.items[0].metadata.name}')
[ -n "$HUBPOD" ] || fail "no hub pod found for tenant-a"
kexec() { kubectl -n "$NS" exec "$HUBPOD" -- sh -c "$1"; }
# curl, never BusyBox wget: wget cannot issue PUT, and the status-code
# assertions below want the code itself rather than scraped headers.
# `-f` turns a non-2xx into a non-zero exit for the calls that must
# succeed; the calls that ASSERT a code omit it and read %{http_code}.
CURL="curl -sS --max-time 30"

for i in $(seq 1 30); do
  PH=$(kexec "$CURL http://127.0.0.1:8080/status 2>/dev/null" | tr -d '\r' \
       | sed -n 's/.*"phase":"\([a-zA-Z]*\)".*/\1/p')
  [ "$PH" = "serving" ] && break
  sleep 2
done
[ "${PH:-}" = "serving" ] || fail "/status never reported phase serving (got '${PH:-<none>}')"

# rpoClean must be NULL for a tier-off share — never true. A controller
# reading absence as "clean" would delete the only copy of the data.
RPO=$(kexec "$CURL http://127.0.0.1:8080/status" | tr -d '\r' \
      | sed -n 's/.*"rpoClean":\([a-z]*\).*/\1/p')
[ "$RPO" = "null" ] || fail "rpoClean is '$RPO' on a tier-off share — it MUST be null"
pass "/status serves on the pod IP; rpoClean is null (not true) with no bucket"

# The gRPC control plane must NOT be listening. It binds 0.0.0.0, is
# unauthenticated unless FLINT_PNFS_CONTROL_TOKEN is set (nothing sets
# it), and carries DeleteVolume — on a hub whose PVC may be the only
# copy of the data. A standalone hub has no data servers to register
# and no CSI driver in front of it, so every verb on that port is
# either meaningless or destructive.
#
# Read the listening sockets straight out of /proc rather than probing
# with a tool the image may not have: ports are hex, state 0A = LISTEN.
# 2049 = 0x0801, 8080 = 0x1F90, 50051 = 0xC383.
LISTENING=$(kexec "cat /proc/net/tcp /proc/net/tcp6 2>/dev/null" \
            | awk '$4 == "0A" { print $2 }' | sed 's/.*://' | sort -u | tr '\n' ' ')
# Anti-vacuity FIRST: if the ports we KNOW are up are missing, the
# parse is broken and the absence of 50051 below would prove nothing.
case " $LISTENING " in
  *" 0801 "*) ;;
  *) fail "cannot see 2049 in the pod's listening sockets ('$LISTENING') — the /proc parse is broken, so this leg is not testing anything" ;;
esac
case " $LISTENING " in
  *" 1F90 "*) ;;
  *) fail "cannot see 8080 in the pod's listening sockets ('$LISTENING') — the /proc parse is broken" ;;
esac
case " $LISTENING " in
  *" C383 "*) fail "the gRPC control plane (50051) IS LISTENING on a standalone hub — DeleteVolume is reachable by any pod in the cluster" ;;
esac
pass "50051 is not listening on a standalone hub (2049 and 8080 are, so the check can see)"

# The file API refuses without the token, and works with it.
AUTH="Authorization: Bearer $TOKEN"
CODE=$(kexec "$CURL -o /dev/null -w '%{http_code}' 'http://127.0.0.1:8080/files?path=/'")
[ "$CODE" = "401" ] || fail "the file API served a listing WITHOUT a token (HTTP $CODE)"

kexec "$CURL -f -X POST -H '$AUTH' -H 'Content-Type: application/json' \
       -d '{\"path\":\"/api-made\"}' \
       -o /dev/null http://127.0.0.1:8080/files/folder" || fail "POST /files/folder failed"
# The patch above RESTARTED the hub while the kernel client still held
# state, so the server is in its RFC-mandated 90s grace window: OPEN
# answers NFS4ERR_GRACE and every WRITE is refused, while reads serve
# throughout. Browsing works and saving does not — the asymmetry that
# is near-impossible to diagnose from outside — so the API is required
# to say when to come back rather than failing opaquely.
#
# Grace is skipped entirely when nothing survived into this incarnation
# (a hub woken from hibernation comes back on a fresh PVC), so both
# answers are legitimate here and the leg accepts either. What it does
# NOT accept is a 503 that leaves the caller guessing.
kexec "printf '%s' 'written via HTTP, read via NFS' > /tmp/up.bin" \
  || fail "could not stage the upload inside the pod"
PUTURL='http://127.0.0.1:8080/files/content?path=/api-made/note.txt'
put_note() {
  kexec "$CURL -o /dev/null -D /tmp/put.hdr -w '%{http_code}' \
         -X PUT -H '$AUTH' --data-binary @/tmp/up.bin '$PUTURL'"
}
CODE=$(put_note)
case "$CODE" in
  20*) pass "PUT accepted at once (HTTP $CODE) — nothing survived the roll, so grace ended early" ;;
  503)
    RETRY=$(kexec "sed -n 's/^[Rr]etry-[Aa]fter: *\([0-9][0-9]*\).*/\1/p' /tmp/put.hdr" | tr -d '\r')
    [ -n "$RETRY" ] \
      || { kexec "cat /tmp/put.hdr"; fail "503 in grace carried no numeric Retry-After — every caller is left guessing"; }
    pass "writes refused during the post-restart grace window: 503, Retry-After ${RETRY}s (reads served throughout)"
    for i in $(seq 1 40); do
      sleep 5
      CODE=$(put_note)
      case "$CODE" in 20*) break ;; esac
    done
    case "$CODE" in
      20*) pass "the same write lands once grace lapses (HTTP $CODE)" ;;
      *) fail "the hub never left grace — PUT still HTTP $CODE after 200s" ;;
    esac
    ;;
  *) fail "PUT /files/content: HTTP $CODE" ;;
esac

# The bytes must be visible to the KERNEL CLIENT that mounted this same
# share — the whole point of the API is that it is the same filesystem.
# The patch above rolled the server under this live mount, so every NFS
# call here is bounded: a hard mount BLOCKS instead of failing, and an
# unbounded read against a server that never came back hangs the run
# forever instead of reporting the bug.
SEEN=$(vm "timeout 60 cat $MNT/api-made/note.txt" | tr -d '\r')
[ "$SEEN" = "written via HTTP, read via NFS" ] \
  || fail "an HTTP upload is not visible to the NFS mount ('$SEEN') — or the mount never recovered the roll"

# And the reverse direction.
vm "timeout 60 sh -c \"echo -n 'written via NFS' > $MNT/api-made/from-nfs.txt\"" \
  || fail "NFS write failed"
GOT=$(kexec "$CURL -f -H '$AUTH' \
      'http://127.0.0.1:8080/files/content?path=/api-made/from-nfs.txt'" | tr -d '\r')
[ "$GOT" = "written via NFS" ] || fail "an NFS write is not visible to the API ('$GOT')"

# A symlink is DATA, never a path the SERVER resolves. The server holds
# its state database and its cloud credentials outside the export, so
# following one is the credential-theft hole fixed in 1b05a14.
#
# The target has to be a file that exists ONLY inside the hub pod. An
# absolute symlink on an NFS mount is resolved by the CLIENT against
# the CLIENT's root — that is correct NFS behaviour, not a leak — so a
# link to something both sides have (/etc/hostname) reads fine on the
# client and proves nothing either way. /etc/flint/mds.yaml is the
# hub's mounted ConfigMap: if the SERVER dereferenced, the client would
# receive the hub's own config; because the server returns the link as
# data, the client resolves it locally and finds nothing.
vm "timeout 60 ln -sf /etc/flint/mds.yaml $MNT/api-made/escape.txt"
CODE=$(kexec "$CURL -o /dev/null -w '%{http_code}' -H '$AUTH' \
       'http://127.0.0.1:8080/files/content?path=/api-made/escape.txt'")
[ "$CODE" = "409" ] || fail "the API followed a symlink out of the export (HTTP $CODE)"
# READLINK must hand back the target as DATA — that is the server
# refusing to resolve it, stated positively.
TGT=$(vm "timeout 60 readlink $MNT/api-made/escape.txt" | tr -d '\r')
[ "$TGT" = "/etc/flint/mds.yaml" ] \
  || fail "READLINK did not return the link target as data (got '$TGT')"
# And the read must not yield the hub's config. Asserted on CONTENT,
# not on an exit code: a non-zero exit is also what a wedged mount and
# a timeout produce, and neither of those proves anything.
OUT=$(vm "timeout 60 cat $MNT/api-made/escape.txt 2>/dev/null" | tr -d '\r')
echo "$OUT" | grep -q "level:" \
  && fail "the NFS server DEREFERENCED the symlink and served the hub's own config — 1b05a14 has regressed"
[ -z "$OUT" ] \
  || fail "reading the symlink returned content the client should not have got: $OUT"
pass "file API: 401 unauthenticated, HTTP↔NFS round trip both ways, symlink refused 409 and never dereferenced server-side"

# ── 10. the sessions guard ───────────────────────────────────────────
say "leg 10: a mounted-but-idle share is held awake by suspendWithSessions"
# The partition case: agents hold mounts and go quiet. The hub's own
# activity clock DELIBERATELY ignores bare SEQUENCE, GETATTR and ACCESS
# (see nfs/activity.rs) so that a mounted hub CAN idle — which means the
# clock alone cannot tell "everyone went home" from "everyone is blocked
# and still mounted". `suspendWithSessions: false` is what tells them
# apart, and until it was wired it was a knob nothing honoured.
#
# The mount stays UP and idle for this leg. That is the whole point.
kubectl -n "$NS" patch flintshare tenant-a --type=merge \
  -p '{"spec":{"idle":{"suspendAfterSecs":20,"suspendWithSessions":false}}}' >/dev/null \
  || fail "spec.idle.suspendWithSessions was refused"

# Re-resolve the pod. `spec.idle` is operator-only and should not roll
# the hub, but a stale HUBPOD would fail this leg for a reason that has
# nothing to do with what it tests.
kubectl -n "$NS" rollout status deployment/tenant-a --timeout=120s >/dev/null 2>&1 \
  || fail "the hub is not settled after arming the ladder"
HUBPOD=$(kubectl -n "$NS" get pods -l flint.io/share=tenant-a \
  --field-selector=status.phase=Running -o jsonpath='{.items[0].metadata.name}')
[ -n "$HUBPOD" ] || fail "no running hub pod for tenant-a"

# Wait until the HUB itself says it is past the threshold. Without this
# the leg would pass on a hub that simply had not gone quiet yet — the
# guard would never be the thing under test.
IDLE=0; LEASES=0
for i in $(seq 1 30); do
  ST=$(kexec "$CURL http://127.0.0.1:8080/status" | tr -d '\r')
  IDLE=$(echo "$ST"   | sed -n 's/.*"idleSecs":\([0-9]*\).*/\1/p')
  LEASES=$(echo "$ST" | sed -n 's/.*"activeLeases":\([0-9]*\).*/\1/p')
  [ "${IDLE:-0}" -ge 25 ] && break
  sleep 5
done
[ "${IDLE:-0}" -ge 25 ] \
  || fail "the hub never reported itself idle past the threshold (idleSecs ${IDLE:-<none>}) — the guard would not be what is under test"
[ "${LEASES:-0}" -ge 1 ] \
  || fail "the hub reports ${LEASES:-<none>} active leases while a client is mounted — the signal the guard reads is broken"
pass "hub is idle ${IDLE}s (past the 20s threshold) and still holds $LEASES lease(s)"

# Both other signals now say "suspend". Only the guard is holding it.
sleep 30
PHASE=$(kubectl -n "$NS" get flintshare tenant-a -o jsonpath='{.status.phase}')
[ "$PHASE" = "Ready" ] || {
  kubectl -n "$NS" get flintshare tenant-a -o yaml | tail -30
  fail "a share with a live mount was suspended anyway (phase $PHASE) — the mounts it stranded would hang on heal"
}
REPL=$(kubectl -n "$NS" get deployment tenant-a -o jsonpath='{.spec.replicas}')
[ "$REPL" = "1" ] || fail "replicas=$REPL on a share that must stay up"
HELD=$(kubectl -n "$NS" get flintshare tenant-a \
  -o jsonpath='{.status.conditions[?(@.type=="IdleEligible")].message}')
echo "$HELD" | grep -qi "lease" \
  || fail "the share stayed up but not because of the lease guard (reported: $HELD)"
pass "held awake by the lease guard, and the condition says so: $HELD"

# Disarm, so the ladder leg below starts from the same state it always
# has. The guard releasing when clients go away is proven there.
kubectl -n "$NS" patch flintshare tenant-a --type=merge -p '{"spec":{"idle":null}}' >/dev/null \
  || fail "could not disarm the ladder"

# ── 11. the idle ladder ──────────────────────────────────────────────
say "leg 11: an idle share suspends, an annotation wakes it, an admin suspend outranks it"
# UNMOUNT BEFORE ARMING. The leg above leaves the hub idle well past
# this threshold, so arming the ladder while the mount is still up
# suspends the share on the very next reconcile — and a hard NFS mount
# whose server just went to zero replicas D-states the umount that was
# supposed to come next. Arm only once nothing is mounted.
vm "timeout 60 sh -c 'mountpoint -q $MNT && umount -lf $MNT'" || true
vm "findmnt -no TARGET $MNT 2>/dev/null" | grep -q . \
  && fail "the mount is still present — suspending under it would wedge the umount"

kubectl -n "$NS" patch flintshare tenant-a --type=merge \
  -p '{"spec":{"idle":{"suspendAfterSecs":20}}}' >/dev/null \
  || fail "spec.idle was refused"
# CEL must refuse hibernation on a share with no bucket: that PVC is
# the only copy of the data.
if kubectl -n "$NS" patch flintshare tenant-a --type=merge \
     -p '{"spec":{"idle":{"suspendAfterSecs":20,"hibernateAfterSecs":60}}}' \
     >/tmp/op-e2e-hib.log 2>&1; then
  fail "hibernateAfterSecs was accepted on a share with NO bucket"
fi
grep -qi "only copy" /tmp/op-e2e-hib.log \
  || { cat /tmp/op-e2e-hib.log; fail "the refusal did not explain why"; }
pass "hibernateAfterSecs is refused without a bucket, and says why"

# Nothing is mounted (asserted above, before the ladder was armed), so
# the only thing left to wait for is the ladder itself.
for i in $(seq 1 45); do
  P=$(kubectl -n "$NS" get flintshare tenant-a -o jsonpath='{.status.phase}' 2>/dev/null)
  [ "$P" = "IdleSuspended" ] && break
  sleep 2
done
[ "${P:-}" = "IdleSuspended" ] || {
  kubectl -n "$NS" get flintshare tenant-a -o yaml | tail -40
  kubectl -n "$OPNS" logs deployment/flint-lite-operator --tail=40
  fail "the share never idle-suspended (phase ${P:-<none>})"
}
REPL=$(kubectl -n "$NS" get deployment tenant-a -o jsonpath='{.spec.replicas}')
[ "$REPL" = "0" ] || fail "IdleSuspended but replicas=$REPL"
# Suspend KEEPS the disk. That is the whole difference from hibernate.
kubectl -n "$NS" get pvc tenant-a-data >/dev/null 2>&1 \
  || fail "idle-suspend deleted the PVC — it must only scale to zero"
pass "idle-suspended: replicas 0, PVC kept, phase distinguishable from an admin suspend"

# The carrier must be an ANNOTATION, and it must SURVIVE the next
# reconcile — a suspend the renderer does not read is undone in seconds.
ST=$(kubectl -n "$NS" get flintshare tenant-a -o jsonpath='{.metadata.annotations.flint\.io/idle-state}')
[ "$ST" = "Suspended" ] || fail "the idle state is not on the CR as an annotation (got '$ST')"
sleep 20
REPL=$(kubectl -n "$NS" get deployment tenant-a -o jsonpath='{.spec.replicas}')
[ "$REPL" = "0" ] || fail "the suspend was UNDONE by a later reconcile (replicas back to $REPL)"
pass "the suspend survives repeated reconciles — the annotation carrier holds"

# `flint.io/requested-at` is the LADDER'S INPUT: whatever wants this
# share awake stamps it, and the level-triggered reconcile does the
# rest. Asserted here as the operator's input and deliberately NOT as
# any particular caller's contract — how a front door reaches a share
# is a separate decision, and pinning it in a committed test now makes
# changing it a test rewrite later.
kubectl -n "$NS" annotate flintshare tenant-a \
  "flint.io/requested-at=$(date -u +%FT%TZ)" --overwrite >/dev/null
for i in $(seq 1 60); do
  P=$(kubectl -n "$NS" get flintshare tenant-a -o jsonpath='{.status.phase}' 2>/dev/null)
  [ "$P" = "Ready" ] && break
  sleep 2
done
[ "${P:-}" = "Ready" ] || {
  kubectl -n "$NS" get flintshare tenant-a -o yaml | tail -30
  fail "touching flint.io/requested-at did not wake the share (phase ${P:-<none>})"
}
pass "one annotation woke it back to Ready"

# An admin's suspend outranks the ladder, and reports DIFFERENTLY — a
# front door has to tell "will wake on request" from "someone said no".
kubectl -n "$NS" patch flintshare tenant-a --type=merge \
  -p '{"spec":{"lifecycle":"Suspended"}}' >/dev/null
for i in $(seq 1 30); do
  P=$(kubectl -n "$NS" get flintshare tenant-a -o jsonpath='{.status.phase}' 2>/dev/null)
  [ "$P" = "Suspended" ] && break
  sleep 2
done
[ "${P:-}" = "Suspended" ] || fail "an admin suspend did not take (phase ${P:-<none>})"
kubectl -n "$NS" annotate flintshare tenant-a \
  "flint.io/requested-at=$(date -u +%FT%TZ)" --overwrite >/dev/null
sleep 15
P=$(kubectl -n "$NS" get flintshare tenant-a -o jsonpath='{.status.phase}')
[ "$P" = "Suspended" ] || fail "a wake request overrode an ADMIN suspend (phase $P)"
REPL=$(kubectl -n "$NS" get deployment tenant-a -o jsonpath='{.spec.replicas}')
[ "$REPL" = "0" ] || fail "an admin-suspended share was scaled back up by a wake request"
pass "spec.lifecycle: Suspended outranks the ladder; a wake request does not override it"

# ── 12. reclaim ──────────────────────────────────────────────────────
say "leg 12: reclaim Retain keeps the PVC; Delete removes it"
# tenant-a arrives here admin-suspended at replicas 0, left that way by
# leg 11. That is deliberate: cleanup must not need a running hub to
# reach — a share you suspended BECAUSE it was misbehaving is exactly
# the one you then delete, and a finalizer that waits on a pod that is
# never coming back wedges the namespace.
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

# ── 13. adoption ─────────────────────────────────────────────────────
say "leg 13: adopting a live helm release in place"
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
# Adoption rolls the Deployment (the operator's config checksum differs
# from helm's), and `strategy: Recreate` on an RWO claim means there is
# a window with the old pod Terminating and the new one Pending. Wait
# it out and name the pod explicitly: `exec deployment/...` resolves
# through the selector and will happily pick a pod that cannot exec,
# which is indistinguishable from missing data once stderr is dropped.
kubectl -n "$CHARTNS" rollout status deployment/flint-lite --timeout=180s >/dev/null 2>&1 \
  || fail "the adopted Deployment never finished rolling"
for i in $(seq 1 30); do
  ADOPTED_POD=$(kubectl -n "$CHARTNS" get pods -l app.kubernetes.io/name=flint-lite \
    --field-selector=status.phase=Running \
    -o jsonpath='{.items[?(@.status.containerStatuses[0].ready==true)].metadata.name}' 2>/dev/null \
    | awk '{print $1}')
  [ -n "$ADOPTED_POD" ] && break
  sleep 2
done
[ -n "${ADOPTED_POD:-}" ] || { kubectl -n "$CHARTNS" get pods; \
  fail "no ready hub pod in $CHARTNS after adoption"; }
# Last attempt keeps stderr: if this fails, the reason should be in the
# log rather than inferred from an empty string.
MARKER=""
for i in $(seq 1 10); do
  MARKER=$(kubectl -n "$CHARTNS" exec "$ADOPTED_POD" -- sh -c 'cat /data/exports/marker.txt' 2>/dev/null | tr -d '\r')
  [ "$MARKER" = "adopted-data" ] && break
  sleep 2
done
if [ "$MARKER" != "adopted-data" ]; then
  echo "--- last exec, with stderr ---"
  kubectl -n "$CHARTNS" exec "$ADOPTED_POD" -- sh -c 'ls -la /data/exports/; cat /data/exports/marker.txt' || true
  fail "the adopted hub does not see the pre-adoption data ('$MARKER')"
fi
pass "adopted in place: one Deployment, same PVC ($PVC_UID2), data intact"

echo
echo "══════════════════════════════════════════════════════════════════"
echo " PASS — the operator installed its own CRD, served a kernel client"
echo " through a FlintShare, refused an identity change and a typo'd"
echo " knob at admission, rolled a hub on a settings edit, refused a"
echo " duplicate bucket subtree across namespaces, repaired a mangled"
echo " schema, served files over HTTP to the same tree a kernel client"
echo " had mounted, held a mounted share awake through its own idle"
echo " window, suspended an unmounted one and woke it with a single"
echo " annotation, kept a Retain PVC through deletion, and adopted a"
echo " live helm release without ever double-mounting its claim."
echo "══════════════════════════════════════════════════════════════════"
