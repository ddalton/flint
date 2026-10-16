#!/usr/bin/env bash
# kind-nfs-only.sh — run the nfs-only Chainsaw suite against the driver built
# from THIS tree, on a throwaway kind cluster. The GitHub gate
# (.github/workflows/system-chainsaw.yml) runs exactly this script; run it
# on any Linux host with docker to reproduce a CI result.
#
# What it does:
#   1. builds the image as published (cargo zigbuild + Dockerfile.csi.prebuilt), or
#      takes one already built (BUILD=0);
#   2. starts a local registry and a 3-node kind cluster that pulls from it.
#      The chart hardcodes `imagePullPolicy: Always` on every flint image,
#      so `kind load` alone would not do: the kubelet would still go to the
#      registry. A registry the nodes can reach makes "Always" pull THIS
#      build, with no chart change;
#   3. installs the chart in nfs-only mode (no SPDK sidecar: volumes are an
#      NFS server pod over an emptyDir) plus the `flint-nfs` StorageClass;
#   4. runs tests-nfs-only/ with chainsaw-nfs-only.yaml.
#
# KRB5=1: the Kerberos suite instead (tests-nfs-krb5/). Nodes from
# krb5/kind-node.Dockerfile (rpc.gssd); krb5-operator installed from its
# release (KRB5_OPERATOR_VERSION) with a KerberosRealm FLINT.TEST; the chart
# with nfs.kerberos (server keytab, node keytabs, the flint-krb5 class, a
# krb5i floor, bob@FLINT.TEST mapped to 2000:2000) and permissions
# enforced; rpc.gssd started on each node once its keytab is there.
#
# Why nfs-only: the SPDK suites need hugepages and a ublk-capable kernel,
# which a hosted runner does not have. This mode exercises the controller,
# the node plugin, the NFS server pod and the kernel NFS client for real.
#
# Env: CLUSTER (flint-chainsaw)  REG_PORT (5001)  TAG (git short sha)
#      BUILD (1; 0 = image localhost:$REG_PORT/dilipdalton/flint-driver:$TAG
#      already exists locally)  KEEP (0; 1 = leave cluster + registry up)
#      REPORT_DIR (unset; set = write a JUnit report there)
#      KIND_NODE_IMAGE (kindest/node:v1.34.0, kind v0.30.0's default)
#      TESTS (tests-nfs-only; e.g. tests-nfs-only/rwo-pvc-migration)
#      HELM_EXTRA (extra helm flags, e.g. "--set nfs.verbose=true")
#      KRB5 (0; 1 = the Kerberos suite)  KRB5_OPERATOR_VERSION (0.4.0)
# Uses a PRIVATE kubeconfig: never touches ~/.kube/config.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
CLUSTER="${CLUSTER:-flint-chainsaw}"
REG_PORT="${REG_PORT:-5001}"
REG_NAME="${CLUSTER}-registry"
TAG="${TAG:-$(git -C "$ROOT" rev-parse --short HEAD)}"
BUILD="${BUILD:-1}"
KEEP="${KEEP:-0}"
KIND_NODE_IMAGE="${KIND_NODE_IMAGE:-kindest/node:v1.34.0}"
KRB5="${KRB5:-0}"
KRB5_OPERATOR_VERSION="${KRB5_OPERATOR_VERSION:-0.4.0}"
IMAGE="localhost:${REG_PORT}/dilipdalton/flint-driver:${TAG}"
export KUBECONFIG="${KUBECONFIG_PRIVATE:-$(mktemp -d)/kubeconfig}"

step() { printf '\n▶ %s\n' "$*" >&2; }
fail() { printf '\n✗ %s\n' "$*" >&2; exit 1; }

for t in docker kind kubectl helm chainsaw $( [ "$BUILD" = 1 ] && echo cargo-zigbuild ); do
  command -v "$t" >/dev/null || fail "$t not found in PATH"
done

cleanup() {
  rc=$?
  if [ "$rc" -ne 0 ]; then
    step "FAILED (rc=$rc): driver state for the log"
    kubectl get pods -A -o wide 2>/dev/null || true
    kubectl -n flint-system logs -l app=flint-csi-controller --all-containers --tail=200 2>/dev/null || true
    kubectl -n flint-system logs -l app=flint-csi-node --all-containers --tail=100 2>/dev/null || true
  fi
  if [ "$KEEP" = 1 ]; then
    echo "KEEP=1: cluster $CLUSTER and registry $REG_NAME left up; KUBECONFIG=$KUBECONFIG" >&2
  else
    kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
    docker rm -f "$REG_NAME" >/dev/null 2>&1 || true
  fi
  exit "$rc"
}
trap cleanup EXIT

# The node plugin mounts NFS inside a kind node, i.e. in THIS host's kernel.
step "kernel NFS client"
grep -qw nfs4 /proc/filesystems || sudo -n modprobe nfs || fail "no nfs4 in /proc/filesystems and modprobe nfs failed"
grep -qw nfs4 /proc/filesystems || fail "nfs4 still not in /proc/filesystems"

if [ "$KRB5" = 1 ]; then
  # The GSS upcall and the krb5 enctypes, in the same kernel.
  step "kernel RPCSEC_GSS (auth_rpcgss, rpcsec_gss_krb5)"
  sudo -n modprobe auth_rpcgss && sudo -n modprobe rpcsec_gss_krb5 || fail "cannot load auth_rpcgss/rpcsec_gss_krb5"
  step "kind node image with rpc.gssd"
  docker build -q --build-arg KIND_NODE_IMAGE="$KIND_NODE_IMAGE" -t flint-kind-node-krb5:latest \
    -f "$HERE/krb5/kind-node.Dockerfile" "$HERE/krb5"
  KIND_NODE_IMAGE=flint-kind-node-krb5:latest
fi

if [ "$BUILD" = 1 ]; then
  # The shipped build (scripts/stage-prebuilt.sh + publish-images.sh): musl
  # binaries from cargo-zigbuild, then Dockerfile.csi.prebuilt. Not
  # Dockerfile.csi: its context is spdk-csi-driver/ alone, which no longer
  # holds the crate's path dependencies (crates/flint-store, forge/syncer).
  step "build $IMAGE (cargo zigbuild + Dockerfile.csi.prebuilt, as published)"
  (cd "$ROOT/spdk-csi-driver" &&
    cargo zigbuild --release --target x86_64-unknown-linux-musl --bin csi-driver --bin flint-nfs-server)
  # Under target/ (gitignored), not mktemp: a snap-packaged docker sees a
  # private /tmp and reports the context "not found".
  ctx="$ROOT/spdk-csi-driver/target/kind-nfs-only-ctx"
  rm -rf "$ctx" && mkdir -p "$ctx/amd64"
  cp "$ROOT/spdk-csi-driver/target/x86_64-unknown-linux-musl/release/"{csi-driver,flint-nfs-server} "$ctx/amd64/"
  DOCKER_BUILDKIT=1 docker build --platform linux/amd64 \
    -f "$ROOT/spdk-csi-driver/docker/Dockerfile.csi.prebuilt" --build-arg BIN_DIR=. \
    -t "$IMAGE" "$ctx"
  rm -rf "$ctx"
fi
docker image inspect "$IMAGE" >/dev/null || fail "image $IMAGE not found locally"

step "registry $REG_NAME on 127.0.0.1:$REG_PORT"
docker rm -f "$REG_NAME" >/dev/null 2>&1 || true
docker run -d --restart=no --name "$REG_NAME" -p "127.0.0.1:${REG_PORT}:5000" registry:2 >/dev/null
# The registry listens a moment after the container starts: a push before
# that is "connection reset" (seen on the second, KRB5=1, run).
for _ in $(seq 30); do
  curl -fsS "http://127.0.0.1:${REG_PORT}/v2/" >/dev/null 2>&1 && break
  sleep 1
done
docker push "$IMAGE" >/dev/null

step "kind cluster $CLUSTER (1 control plane + 2 workers, $KIND_NODE_IMAGE)"
kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
kind create cluster --name "$CLUSTER" --image "$KIND_NODE_IMAGE" --kubeconfig "$KUBECONFIG" --config - <<EOF
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
containerdConfigPatches:
- |-
  [plugins."io.containerd.grpc.v1.cri".registry]
    config_path = "/etc/containerd/certs.d"
nodes:
- role: control-plane
- role: worker
- role: worker
EOF
# Each node resolves localhost:$REG_PORT to the registry container on the
# kind network (the documented kind local-registry recipe).
docker network connect kind "$REG_NAME" 2>/dev/null || true
for n in $(kind get nodes --name "$CLUSTER"); do
  docker exec "$n" mkdir -p "/etc/containerd/certs.d/localhost:${REG_PORT}"
  printf '[host."http://%s:5000"]\n' "$REG_NAME" |
    docker exec -i "$n" cp /dev/stdin "/etc/containerd/certs.d/localhost:${REG_PORT}/hosts.toml"
done

if [ "$KRB5" = 1 ]; then
  step "krb5-operator $KRB5_OPERATOR_VERSION and realm FLINT.TEST"
  ops="https://raw.githubusercontent.com/ddalton/krb5-operator/v$KRB5_OPERATOR_VERSION/deploy"
  kubectl apply --server-side -f "$ops/crds.yaml" -f "$ops/operator.yaml"
  kubectl -n krb5-operator rollout status deploy/krb5-operator --timeout=5m
  kubectl create namespace flint-system --dry-run=client -o yaml | kubectl apply -f -
  kubectl apply -f "$HERE/krb5/realm.yaml"
  kubectl -n flint-system wait kerberosrealm/flint --timeout=5m \
    --for=jsonpath='{.status.conditions[?(@.type=="Ready")].status}'=True
  # The krb5i floor, enforced permissions and attached-nodes-only are the
  # chart's defaults under Kerberos: not set here, so the suite tests them.
  HELM_EXTRA="${HELM_EXTRA:-} --set nfs.kerberos.enabled=true --set nfs.kerberos.realm=FLINT.TEST
    --set nfs.kerberos.realmRef=flint
    --set nfs.kerberos.nodeKeytab.tolerations[0].operator=Exists
    --set nfs.kerberos.nodeKeytab.restartServices[0]=rpc-gssd
    --set nfs.kerberos.idmap[0].principal=bob@FLINT.TEST --set nfs.kerberos.idmap[0].uid=2000
    --set nfs.kerberos.idmap[0].gid=2000"
fi

step "helm install flint-csi (nfs-only, image tag $TAG)"
helm upgrade --install flint-csi "$ROOT/flint-csi-driver-chart" \
  --namespace flint-system --create-namespace \
  --set images.registry="localhost:${REG_PORT}" \
  --set images.flintCsiDriver.tag="$TAG" \
  --set deployment.nodeMode=nfs-only \
  --set storageClass.parameters.nfsEmptyDir=true \
  --set dashboard.enabled=false \
  --set snapshotClass.enabled=false \
  --set crds.installSnapshotCRDs=false \
  ${HELM_EXTRA:-} \
  --wait --timeout 5m
kubectl -n flint-system wait --for=condition=Ready pod -l app=flint-csi-node --timeout=180s
kubectl -n flint-system wait --for=condition=Ready pod -l app=flint-csi-controller --timeout=180s

# The nfs-only tests name this class; the chart renders only `flint`.
kubectl apply -f - <<EOF
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: flint-nfs
provisioner: disk.csi.chert.us
parameters:
  nfsEmptyDir: "true"
  numReplicas: "1"
  thinProvision: "false"
reclaimPolicy: Delete
volumeBindingMode: WaitForFirstConsumer
allowVolumeExpansion: true
EOF

if [ "$KRB5" = 1 ]; then
  step "Kerberos keys: the NFS servers' and every node's"
  kubectl -n flint-system wait kerberosprincipal/flint-nfs-server --timeout=5m \
    --for=jsonpath='{.status.conditions[?(@.type=="Ready")].status}'=True
  kubectl wait kerberosnodekeytab --all --timeout=5m \
    --for=jsonpath='{.status.conditions[?(@.type=="Ready")].status}'=True
  # rpc-gssd's unit starts only once /etc/krb5.keytab exists: the node
  # agents start it (nfs.kerberos.nodeKeytab.restartServices), not us.
  for n in $(kind get nodes --name "$CLUSTER"); do
    docker exec "$n" klist -k /etc/krb5.keytab
    for i in $(seq 30); do docker exec "$n" systemctl is-active -q rpc-gssd && break; sleep 2; done
    docker exec "$n" systemctl is-active rpc-gssd ||
      { docker exec "$n" journalctl -u rpc-gssd --no-pager | tail -20; fail "the node agent did not start rpc-gssd on $n"; }
  done
fi

# The image under test, not a registry's: every flint pod must run $TAG.
step "pods run the image under test"
bad=$(kubectl -n flint-system get pods -o jsonpath='{range .items[*]}{range .spec.containers[*]}{.image}{"\n"}{end}{end}' |
  grep '/flint-driver:' | grep -v ":${TAG}\$" || true)
[ -z "$bad" ] || fail "flint pods run another image: $bad"

if [ "$KRB5" = 1 ]; then
  TESTS="${TESTS:-tests-nfs-krb5}"
fi
TESTS="${TESTS:-tests-nfs-only}"
step "chainsaw: $TESTS"
report=()
if [ -n "${REPORT_DIR:-}" ]; then
  mkdir -p "$REPORT_DIR"
  report=(--report-format JUNIT-TEST --report-path "$REPORT_DIR" --report-name chainsaw-nfs-only)
fi
cd "$HERE"
chainsaw test --config chainsaw-nfs-only.yaml --test-dir "$TESTS" "${report[@]}"
