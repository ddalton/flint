#!/usr/bin/env bash
# install.sh -- Rook (operator chart) + a CephCluster (rook-ceph-cluster
# chart, cluster-values.yaml), then wait for HEALTH_OK with one OSD per node.
#
#   VERSION=v1.21.0 ./install.sh
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/../../lib/host.sh"
VERSION="${VERSION:-v1.21.0}"
CSI_DRIVERS_VERSION="${CSI_DRIVERS_VERSION:-1.1.0}"
NS=rook-ceph

samplers_up
require_clean_disks
for n in $(nodes); do
  hostexec "$n" '[ ! -e /var/lib/rook ] || { echo "/var/lib/rook exists"; exit 1; }; modprobe rbd' ||
    fail "$n: stale /var/lib/rook or no rbd module"
done

# A helm step that dies mid-flight (2026-10-21, bench4: the API server's TLS
# answered "bad record MAC" while the operator chart posted its CRDs; repeated
# large POSTs right after all passed) leaves a FAILED release. Uninstall it
# (the CRDs are resource-policy keep and get re-adopted) and try again.
helm_retry() {  # <release> <helm upgrade args...>
  local rel=$1 i; shift
  for i in 1 2 3; do
    helm_private "$@" && return 0
    echo "helm $rel attempt $i failed; uninstalling the failed release and retrying" >&2
    helm_private -n "$NS" uninstall "$rel" --wait --timeout 10m >&2 || true
    sleep 15
  done
  return 1
}

pull=$(mktemp -d)
helm_private pull rook-ceph --repo https://charts.rook.io/release --version "$VERSION" -d "$pull"
helm_private pull rook-ceph-cluster --repo https://charts.rook.io/release --version "$VERSION" -d "$pull"
step "operator $VERSION"
helm_retry rook-ceph upgrade --install rook-ceph "$pull"/rook-ceph-[v0-9]*.tgz -n "$NS" --create-namespace --wait --timeout 15m
# Rook v1.21 no longer creates the CSI Driver CRs: without the ceph-csi-drivers
# chart (Rook's recommended values, csi-drivers-values.yaml, copied from
# rook v1.21.0 deploy/charts/ceph-csi-drivers/values.yaml) no RBD provisioner
# runs and every PVC stays Pending (bench4, 2026-10-21). Its version matches
# the ceph-csi-operator subchart Rook bundles (1.1.0).
step "ceph-csi-drivers $CSI_DRIVERS_VERSION"
helm_private pull ceph-csi-drivers --repo https://ceph.github.io/ceph-csi-operator --version "$CSI_DRIVERS_VERSION" -d "$pull"
helm_retry ceph-csi-drivers upgrade --install ceph-csi-drivers "$pull"/ceph-csi-drivers-*.tgz -n "$NS" \
  -f "$HERE/csi-drivers-values.yaml" --wait --timeout 10m
step "cluster"
helm_retry rook-ceph-cluster upgrade --install rook-ceph-cluster "$pull"/rook-ceph-cluster-*.tgz -n "$NS" \
  -f "$HERE/cluster-values.yaml" --wait --timeout 15m

want=$(nodes | wc -w | tr -d ' ')
# Expected warnings that never clear on this rig, muted so HEALTH_OK means
# "nothing else" (2026-10-21, Rook v1.21.0 / Ceph v20.2.4): the r1 pool has
# one copy by design (POOL_NO_REDUNDANCY), and Rook still issues its CSI
# clients the legacy key type (AUTH_INSECURE_*). Neither touches the data path.
MUTES="POOL_NO_REDUNDANCY AUTH_INSECURE_CLIENT_KEY_TYPE AUTH_INSECURE_KEYS_ALLOWED AUTH_INSECURE_KEYS_CREATABLE"
for i in $(seq 1 90); do
  for c in $MUTES; do kubectl -n "$NS" exec deploy/rook-ceph-tools -- ceph health mute "$c" --sticky >/dev/null 2>&1 || true; done
  health=$(kubectl -n "$NS" get cephcluster -o jsonpath='{.items[0].status.ceph.health}' 2>/dev/null || true)
  osds=$(kubectl -n "$NS" get pods -l app=rook-ceph-osd --field-selector=status.phase=Running --no-headers 2>/dev/null | wc -l | tr -d ' ')
  [ "$health" = HEALTH_OK ] && [ "$osds" = "$want" ] && break
  sleep 20
done
kubectl -n "$NS" get cephcluster,cephblockpool
[ "$health" = HEALTH_OK ] && [ "$osds" = "$want" ] || fail "cluster not healthy: health=$health osds=$osds/$want"
kubectl -n "$NS" exec deploy/rook-ceph-tools -- ceph osd tree
kubectl -n "$NS" exec deploy/rook-ceph-tools -- ceph versions
