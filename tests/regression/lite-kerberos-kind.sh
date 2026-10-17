#!/usr/bin/env bash
# Kerberos for a FlintShare hub, on kind (docs/plans/flint-lite-kerberos-plan.md,
# L1 checked as L4 describes): krb5-operator and a realm, node keytabs and
# rpc.gssd, the lite operator from this tree with kerberos.enabled, then
#   1. a Kerberos share: its KerberosPrincipal (owned by the share), keytab
#      Secret, the hub's security env, status.kerberos;
#   2. a client pod on a static PV, sec=krb5i by status.kerberos.serverName:
#      it mounts, writes and reads back, and /proc/mounts says sec=krb5i;
#   3. the same hub by sec=sys: refused by the krb5i floor;
#   4. a setting the hub would refuse (a lower-case realm): KerberosInvalid,
#      no Deployment;
#   5. a sec=sys share turned Kerberos whose keytab never comes: its running
#      hub is scaled to zero (KerberosKeytabPending), never left on sec=sys;
#   6. Kerberos turned off again on the first share: its principal goes.
#
# Needs: docker, kind, kubectl, helm, cargo-zigbuild, sudo -n modprobe.
# Env: CLUSTER (flint-lite-krb5), KEEP (0; 1 keeps the cluster),
#      KRB5_OPERATOR_VERSION (0.4.0), KIND_CTX_DIR (docker build contexts;
#      a snap docker sees only $HOME), CARGO_TARGET_DIR.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
HERE="$ROOT/tests/system"
CLUSTER="${CLUSTER:-flint-lite-krb5}"
KEEP="${KEEP:-0}"
KRB5_OPERATOR_VERSION="${KRB5_OPERATOR_VERSION:-0.4.0}"
KIND_NODE_IMAGE="${KIND_NODE_IMAGE:-kindest/node:v1.34.0}"
TAG="lite-krb5-$(git -C "$ROOT" rev-parse --short HEAD)"
OPIMG="flint-lite-operator:$TAG"
HUBIMG="flint-pnfs:$TAG"
NS=flint-system
WORK="$(mktemp -d "${KIND_CTX_DIR:-$HOME/tmp}/lite-krb5.XXXXXX")"
export KUBECONFIG="$WORK/kubeconfig"

step() { echo; echo "▶ $*"; }
fail() { echo "FAIL: $*" >&2; diagnose; exit 1; }
pass() { echo "PASS: $*"; }
diagnose() {
  kubectl get flintshare -A -o wide 2>/dev/null || true
  kubectl -n "$NS" get flintshare -o jsonpath='{range .items[*]}{.metadata.name}{": "}{.status.conditions}{"\n"}{end}' 2>/dev/null || true
  kubectl -n "$NS" get kerberosprincipal,deploy,pods 2>/dev/null || true
  kubectl -n "$NS" logs deploy/flint-lite-operator --tail=40 2>/dev/null || true
}
cleanup() {
  if [ "$KEEP" = 1 ]; then echo "kept: kind cluster $CLUSTER, KUBECONFIG=$KUBECONFIG"; return; fi
  kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

cond() { # share type -> "status reason"
  kubectl -n "$NS" get flintshare "$1" -o jsonpath="{.status.conditions[?(@.type==\"$2\")].status} {.status.conditions[?(@.type==\"$2\")].reason}"
}
wait_cond() { # share type want-status want-reason
  for _ in $(seq 90); do [ "$(cond "$1" "$2")" = "$3 $4" ] && return 0; sleep 2; done
  fail "$1: condition $2 is '$(cond "$1" "$2")', want '$3 $4'"
}

step "kernel NFS client and RPCSEC_GSS"
sudo -n modprobe nfs && sudo -n modprobe auth_rpcgss && sudo -n modprobe rpcsec_gss_krb5 ||
  fail "cannot load nfs/auth_rpcgss/rpcsec_gss_krb5"

step "kind node image with rpc.gssd"
docker build -q --build-arg KIND_NODE_IMAGE="$KIND_NODE_IMAGE" -t flint-kind-node-krb5:latest \
  -f "$HERE/krb5/kind-node.Dockerfile" "$HERE/krb5" >/dev/null

step "build the operator and the hub ($TAG)"
(cd "$ROOT/spdk-csi-driver" &&
  cargo zigbuild --release --target x86_64-unknown-linux-musl \
    --bin flint-lite-operator --bin flint-pnfs-mds --bin flint-pnfs-ds)
rel="${CARGO_TARGET_DIR:-$ROOT/spdk-csi-driver/target}/x86_64-unknown-linux-musl/release"
mkdir -p "$WORK/op" "$WORK/hub/amd64"
cp "$rel/flint-lite-operator" "$WORK/op/"
cat > "$WORK/op/Dockerfile" <<'EOF'
FROM cgr.dev/chainguard/static:latest
COPY flint-lite-operator /usr/local/bin/flint-lite-operator
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/flint-lite-operator"]
EOF
docker build -q -t "$OPIMG" "$WORK/op" >/dev/null
cp "$rel/"{flint-pnfs-mds,flint-pnfs-ds} "$WORK/hub/amd64/"
DOCKER_BUILDKIT=1 docker build -q --platform linux/amd64 \
  -f "$ROOT/spdk-csi-driver/docker/Dockerfile.pnfs.prebuilt" --build-arg BIN_DIR=. \
  -t "$HUBIMG" "$WORK/hub" >/dev/null

step "kind cluster $CLUSTER (1 control plane + 1 worker)"
kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
kind create cluster --name "$CLUSTER" --image flint-kind-node-krb5:latest --kubeconfig "$KUBECONFIG" --config - <<EOF
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
- role: control-plane
- role: worker
EOF
kind load docker-image --name "$CLUSTER" "$OPIMG" "$HUBIMG" >/dev/null
WORKER="$CLUSTER-worker"

step "krb5-operator $KRB5_OPERATOR_VERSION, realm FLINT.TEST, node keytabs"
ops="https://raw.githubusercontent.com/ddalton/krb5-operator/v$KRB5_OPERATOR_VERSION/deploy"
kubectl apply --server-side -f "$ops/crds.yaml" -f "$ops/operator.yaml" >/dev/null
kubectl -n krb5-operator rollout status deploy/krb5-operator --timeout=5m
kubectl create namespace "$NS"
kubectl apply -f "$HERE/krb5/realm.yaml"
kubectl -n "$NS" wait kerberosrealm/flint --timeout=5m \
  --for=jsonpath='{.status.conditions[?(@.type=="Ready")].status}'=True
kubectl apply -f - <<EOF
apiVersion: krb5.ddalton.dev/v1alpha1
kind: KerberosNodeKeytab
metadata: {name: lite-clients}
spec:
  realmRef: flint
  realmNamespace: $NS
  services: [host, nfs]
  tolerations: [{operator: Exists}]
  keytabPath: /etc/krb5.keytab
  restartServices: [rpc-gssd]
  deletionPolicy: Delete
EOF
kubectl wait kerberosnodekeytab/lite-clients --timeout=5m \
  --for=jsonpath='{.status.conditions[?(@.type=="Ready")].status}'=True
for _ in $(seq 30); do docker exec "$WORKER" systemctl is-active -q rpc-gssd && break; sleep 2; done
docker exec "$WORKER" systemctl is-active -q rpc-gssd || fail "rpc-gssd is not running on $WORKER"

step "the lite operator, kerberos.enabled"
helm install flint-lite-operator "$ROOT/flint-lite-operator-chart" -n "$NS" \
  --set image.ref="$OPIMG" --set image.pullPolicy=Never --set hubImage="$HUBIMG" \
  --set hubImagePullPolicy=Never --set replicas=1 --set kerberos.enabled=true >/dev/null
kubectl -n "$NS" rollout status deploy/flint-lite-operator --timeout=3m

share() { # name [kerberos yaml, indented 6]
  { printf '%s\n' "apiVersion: chert.us/v1alpha1" "kind: FlintShare" "metadata: {name: $1, namespace: $NS}" \
      "spec:" "  persistence: {size: 1Gi}"
    if [ -n "${2:-}" ]; then printf '%s\n' "  security:" "    kerberos:" "$2"; fi
  } | kubectl apply -f -
}
KRB='      realm: FLINT.TEST
      realmRef: flint
      idmap: [{principal: bob@FLINT.TEST, uid: 2000, gid: 2000}]'

step "1. a Kerberos share: principal, keytab, hub env, status"
share ws1 "$KRB"
wait_cond ws1 Ready True Serving
owner=$(kubectl -n "$NS" get kerberosprincipal ws1-krb5 -o jsonpath='{.metadata.ownerReferences[0].kind}/{.metadata.ownerReferences[0].name}')
[ "$owner" = FlintShare/ws1 ] || fail "KerberosPrincipal ws1-krb5 is owned by '$owner'"
princ=$(kubectl -n "$NS" get kerberosprincipal ws1-krb5 -o jsonpath='{.spec.principals[*]}')
[ "$princ" = "nfs/ws1.$NS.svc.cluster.local@FLINT.TEST" ] || fail "principals: $princ"
kubectl -n "$NS" get secret ws1-krb5 >/dev/null || fail "no keytab Secret ws1-krb5"
envs=$(kubectl -n "$NS" get deploy ws1 -o jsonpath='{range .spec.template.spec.containers[0].env[*]}{.name}={.value}{"\n"}{end}')
for want in KRB5_KTNAME=/etc/flint-krb5/krb5.keytab FLINT_NFS_MIN_SEC=krb5i FLINT_NFS_ENFORCE_PERMISSIONS=enforce \
            FLINT_NFS_REQUIRE_MACH_CRED=true "FLINT_NFS_GSS_PRINCIPALS=host/*@FLINT.TEST,nfs/*@FLINT.TEST,bob@FLINT.TEST"; do
  echo "$envs" | grep -qxF "$want" || fail "the hub lacks $want"
done
! echo "$envs" | grep -q '^FLINT_NFS_ATTACHED_NODES_ONLY=' || fail "a standalone hub must not get attached-nodes-only"
SERVER=$(kubectl -n "$NS" get flintshare ws1 -o jsonpath='{.status.kerberos.serverName}')
[ "$SERVER" = "ws1.$NS.svc.cluster.local" ] || fail "status.kerberos.serverName: '$SERVER'"
kubectl -n "$NS" logs deploy/ws1 | grep -E 'floor|Kerberos|KRB5' | head -5
pass "the share's principal, keytab and hub security"

step "2. a client pod mounts it with sec=krb5i by its server name"
IP=$(kubectl -n "$NS" get svc ws1 -o jsonpath='{.spec.clusterIP}')
docker exec "$WORKER" sh -c "echo '$IP $SERVER # flint-lite-krb5' >> /etc/hosts"
pv() { # name sec
  kubectl apply -f - <<EOF
apiVersion: v1
kind: PersistentVolume
metadata: {name: $1}
spec:
  capacity: {storage: 1Gi}
  accessModes: [ReadWriteMany]
  persistentVolumeReclaimPolicy: Retain
  storageClassName: ""
  mountOptions: [nfsvers=4.1, proto=tcp, hard, sec=$2]
  nfs: {server: $SERVER, path: /}
  claimRef: {namespace: default, name: $1}
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata: {name: $1, namespace: default}
spec:
  accessModes: [ReadWriteMany]
  storageClassName: ""
  volumeName: $1
  resources: {requests: {storage: 1Gi}}
---
apiVersion: v1
kind: Pod
metadata: {name: $1, namespace: default}
spec:
  nodeName: $WORKER
  containers:
  - name: c
    image: busybox
    command: [sh, -c, "echo from-krb5 > /data/f && sleep 3600"]
    volumeMounts: [{name: d, mountPath: /data}]
  volumes: [{name: d, persistentVolumeClaim: {claimName: $1}}]
EOF
}
pv krb5i krb5i
kubectl -n default wait pod/krb5i --for=condition=Ready --timeout=3m || fail "the sec=krb5i pod did not start"
m=$(kubectl -n default exec krb5i -- grep ' /data ' /proc/mounts)
echo "$m"
case "$m" in *sec=krb5i*) ;; *) fail "not a sec=krb5i mount" ;; esac
[ "$(kubectl -n default exec krb5i -- cat /data/f)" = from-krb5 ] || fail "read back"
pass "mounted, written and read under krb5i"

step "3. the same hub by sec=sys: refused by the floor"
pv sys sys
sleep 45
[ "$(kubectl -n default get pod sys -o jsonpath='{.status.phase}')" != Running ] || fail "a sec=sys mount of a Kerberos hub succeeded"
kubectl -n default get events --field-selector involvedObject.name=sys -o jsonpath='{range .items[*]}{.reason}: {.message}{"\n"}{end}' |
  grep -m1 FailedMount || fail "no FailedMount event for the sec=sys pod"
pass "sec=sys refused"

step "4. a setting the hub would refuse: KerberosInvalid, no hub"
share ws2 '      realm: flint.test
      realmRef: flint'
wait_cond ws2 Kerberos False KerberosInvalid
! kubectl -n "$NS" get deploy ws2 >/dev/null 2>&1 || fail "ws2 has a Deployment"
pass "refused before any hub ran"

step "5. a sec=sys share turned Kerberos without a keytab: its hub stops"
share ws3
wait_cond ws3 Ready True Serving
share ws3 '      realm: FLINT.TEST
      realmRef: no-such-realm'
wait_cond ws3 Kerberos False KerberosKeytabPending
for _ in $(seq 30); do [ "$(kubectl -n "$NS" get deploy ws3 -o jsonpath='{.spec.replicas}')" = 0 ] && break; sleep 2; done
[ "$(kubectl -n "$NS" get deploy ws3 -o jsonpath='{.spec.replicas}')" = 0 ] || fail "the sec=sys hub of ws3 still runs"
pass "a share that asked for Kerberos is not served as sec=sys"

step "6. Kerberos turned off: its principal goes"
kubectl -n default delete pod krb5i sys --wait=false >/dev/null
kubectl -n "$NS" patch flintshare ws1 --type=json -p '[{"op":"remove","path":"/spec/security"}]'
for _ in $(seq 60); do kubectl -n "$NS" get kerberosprincipal ws1-krb5 >/dev/null 2>&1 || break; sleep 2; done
! kubectl -n "$NS" get kerberosprincipal ws1-krb5 >/dev/null 2>&1 || fail "KerberosPrincipal ws1-krb5 outlived spec.security.kerberos"
pass "the principal is deleted with the setting"

echo; echo "PASS: all"
