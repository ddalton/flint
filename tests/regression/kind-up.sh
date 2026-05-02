#!/usr/bin/env bash
# kind-up.sh — bring up a kind cluster with the Flint CSI driver
# installed, suitable for running tests/regression/spdk-*.sh.
#
# What this gives you:
#   - A 3-node kind cluster (1 control plane + 2 workers) so multi-node
#     scenarios can run.
#   - The Flint CSI driver installed via the existing helm chart with
#     `spdkTarget.kindMode.enabled=true` (uses the dilipdalton/spdk-tgt-kind
#     image, which has --wait-for-rpc and minimized memory pools).
#   - Snapshot CRDs and snapshot controller installed (chart handles
#     this when crds.installSnapshotCRDs=true, the chart default).
#   - The default `flint` StorageClass and `csi-snapclass`
#     VolumeSnapshotClass.
#
# What this does NOT give you:
#   - Real NVMe devices. SPDK kindMode emulates a small malloc-backed
#     virtual disk. Backend A (RWO block) volumes work but are limited
#     in size. Latency/throughput numbers are not representative.
#   - Hugepages-backed perf. kindMode runs in --no-huge mode.
#
# Compatibility with the regression scenarios:
#   - spdk-rwo-basic.sh:        works (small volumes, ~256 MiB max recommended)
#   - spdk-rwo-snapshot.sh:     works (uses the snapshotter sidecar)
#   - spdk-rwo-clone.sh:        works
#   - spdk-rwo-expand.sh:       works
#   - spdk-rwo-multi-replica:   works (3-node cluster has 2 workers; 2 replicas fit)
#   - spdk-rwx-emptydir.sh:     works (RWX is just NFS server pod + mount)
#   - spdk-rwx-pvc.sh:          works
#
# Usage:
#   tests/regression/kind-up.sh                    # bring up cluster + install driver
#   tests/regression/kind-up.sh --recreate         # tear down first if exists
#   tests/regression/kind-down.sh                  # tear down

set -euo pipefail

CLUSTER_NAME="${CLUSTER_NAME:-flint-regression}"
KIND_NODE_IMAGE="${KIND_NODE_IMAGE:-kindest/node:v1.30.0}"
HELM_RELEASE="${HELM_RELEASE:-flint-csi}"
HELM_NAMESPACE="${HELM_NAMESPACE:-flint-system}"
CHART_DIR="$(cd "$(dirname "$0")/../.." && pwd)/flint-csi-driver-chart"
VALUES_OVERRIDE="$(cd "$(dirname "$0")" && pwd)/kind-values.yaml"

log()  { printf '  %s\n' "$*" >&2; }
step() { printf '\n▶ %s\n' "$*" >&2; }
fail() { printf '\n✗ %s\n' "$*" >&2; exit 1; }
ok()   { printf '✓ %s\n' "$*" >&2; }

# ---------------------------------------------------------------------------
# Pre-flight: required tools.
# ---------------------------------------------------------------------------

step "checking required tools"
for tool in kind kubectl helm docker jq; do
    command -v "$tool" >/dev/null 2>&1 || fail "$tool not found in PATH"
done
log "all tools present"

[ -d "$CHART_DIR" ] || fail "helm chart not found at $CHART_DIR"

# Optional --recreate flag.
if [ "${1:-}" = "--recreate" ]; then
    step "tearing down existing cluster (--recreate)"
    kind delete cluster --name "$CLUSTER_NAME" 2>/dev/null || true
fi

# ---------------------------------------------------------------------------
# Kind cluster.
# ---------------------------------------------------------------------------

if kind get clusters 2>/dev/null | grep -qx "$CLUSTER_NAME"; then
    log "kind cluster '$CLUSTER_NAME' already exists; reusing"
    log "  pass --recreate to start fresh"
else
    step "creating kind cluster '$CLUSTER_NAME' (3 nodes)"

    # Multi-node cluster. Workers carry the SPDK target DaemonSet; control
    # plane is excluded from scheduling normal workloads. The
    # `extraPortMappings` aren't needed by the regression scenarios (they
    # use ClusterIP); add only if you need to reach NFS server pods from
    # the host for ad-hoc debugging.
    cat <<EOF | kind create cluster --name "$CLUSTER_NAME" --image "$KIND_NODE_IMAGE" --config -
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
- role: control-plane
- role: worker
- role: worker
EOF
    ok "kind cluster up"
fi

kubectl config use-context "kind-$CLUSTER_NAME" >/dev/null

# ---------------------------------------------------------------------------
# Load images into kind.
#
# kind nodes can't pull from Docker Hub directly in restricted networks;
# `kind load docker-image` ships local images into the kind nodes'
# containerd. We assume the user has these images locally — pulled from
# Docker Hub or built via `make build-images`. If they aren't, kindMode
# falls back to imagePullPolicy=IfNotPresent and the kubelet pulls them.
# ---------------------------------------------------------------------------

step "loading driver images into kind"

IMAGES=(
    "dilipdalton/flint-driver:latest"
    "dilipdalton/spdk-tgt-kind:latest"
    "dilipdalton/spdk-dashboard-frontend:latest"
)

for img in "${IMAGES[@]}"; do
    if docker image inspect "$img" >/dev/null 2>&1; then
        log "loading $img"
        kind load docker-image --name "$CLUSTER_NAME" "$img" 2>/dev/null \
            || log "  (warning: kind load failed for $img — kubelet will pull instead)"
    else
        log "  $img not in local docker — kubelet will pull from registry"
    fi
done

# ---------------------------------------------------------------------------
# Helm install with kindMode overrides.
# ---------------------------------------------------------------------------

step "writing kind-mode helm values to $VALUES_OVERRIDE"

cat > "$VALUES_OVERRIDE" <<'YAML'
# Auto-generated by tests/regression/kind-up.sh.
# Overrides for running the Flint CSI driver inside a kind cluster.

spdkTarget:
  # No hugepages on kind — kindMode runs the SPDK target in --no-huge.
  hugepages:
    enabled: false
  kindMode:
    enabled: true
    spdkMemoryMB: 2048
    # Allocate a 1.5 GiB malloc-backed virtual disk so RWO PVCs have
    # somewhere to land. With sizeMB=0 the LVS is empty and CreateVolume
    # fails for any non-zero size. 1536 MiB fits inside 2048 MiB SPDK
    # memory with ~512 MiB headroom for pools/subsystems.
    virtualDisk:
      sizeMB: 1536
      path: "/var/tmp/spdk-virtual.img"
      lvsName: "lvs_kind"
    memory:
      request: "2048Mi"
      limit: "2560Mi"

# Snapshot CRDs and controller — chart default already enables these.
crds:
  installSnapshotCRDs: true

# Don't make `flint` the cluster default — the regression scripts ask
# for it by name. Setting default would override any kind built-ins
# (none of which exist by default, but be explicit).
storageClass:
  create: true
  isDefaultClass: false
  name: flint
  reclaimPolicy: Delete
  allowVolumeExpansion: true
  parameters:
    numReplicas: "1"
    autoRebuild: "false"
    thinProvision: "true"
    nfsEmptyDir: "false"
YAML

step "helm install $HELM_RELEASE → namespace $HELM_NAMESPACE"

kubectl create namespace "$HELM_NAMESPACE" --dry-run=client -o yaml | kubectl apply -f - >/dev/null

helm upgrade --install "$HELM_RELEASE" "$CHART_DIR" \
    --namespace "$HELM_NAMESPACE" \
    --values "$VALUES_OVERRIDE" \
    --wait --timeout 5m \
    || fail "helm install failed (check 'helm status -n $HELM_NAMESPACE $HELM_RELEASE')"

ok "helm install complete"

# ---------------------------------------------------------------------------
# Post-install verification — what regression scripts will check at
# preflight. Surfacing them here gives a clearer error than waiting for
# scenario 1 to fail.
# ---------------------------------------------------------------------------

step "verifying driver registration"

if ! kubectl get csidriver flint.csi.storage.io >/dev/null 2>&1; then
    fail "CSIDriver flint.csi.storage.io not registered after helm install"
fi
log "  CSIDriver registered"

if ! kubectl get storageclass flint >/dev/null 2>&1; then
    fail "StorageClass 'flint' not created"
fi
log "  StorageClass flint exists"

# Wait for all driver pods to be Ready. helm --wait already waits for
# Deployments; this ensures the DaemonSet pods on every worker are also
# Ready (helm --wait doesn't always cover DaemonSets reliably).
log "waiting for csi-controller + csi-node + spdk-tgt pods to be Ready"

for label in app=flint-csi-controller app=flint-csi-node app=spdk-tgt; do
    if ! kubectl wait --for=condition=Ready pod -l "$label" \
            -n "$HELM_NAMESPACE" --timeout=180s >/dev/null 2>&1; then
        kubectl get pods -n "$HELM_NAMESPACE" -l "$label"
        fail "pods with label '$label' did not become Ready"
    fi
done

ok "all driver pods Ready"

# ---------------------------------------------------------------------------
# Done.
# ---------------------------------------------------------------------------

cat <<EOF >&2

================================================================
✓ kind cluster '$CLUSTER_NAME' is up with the Flint CSI driver.

Run the regression suite:
  tests/regression/spdk-rwo-basic.sh

Capture baselines (first time only):
  REGRESSION_RECORD=1 tests/regression/spdk-rwo-basic.sh

Tear down:
  tests/regression/kind-down.sh

Cluster context: kind-$CLUSTER_NAME
Helm release:    $HELM_RELEASE in namespace $HELM_NAMESPACE
Storage class:   flint (provisioner: flint.csi.storage.io)
================================================================
EOF
