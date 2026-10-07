#!/usr/bin/env bash
# install.sh -- Rook (operator chart) + a CephCluster (rook-ceph-cluster
# chart, cluster-values.yaml), then wait for HEALTH_OK with one OSD per node.
#
#   VERSION=v1.21.0 ./install.sh
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/../../lib/host.sh"
VERSION="${VERSION:-v1.21.0}"
NS=rook-ceph

samplers_up
require_clean_disks
for n in $(nodes); do
  hostexec "$n" '[ ! -e /var/lib/rook ] || { echo "/var/lib/rook exists"; exit 1; }; modprobe rbd' ||
    fail "$n: stale /var/lib/rook or no rbd module"
done

pull=$(mktemp -d)
helm_private pull rook-ceph --repo https://charts.rook.io/release --version "$VERSION" -d "$pull"
helm_private pull rook-ceph-cluster --repo https://charts.rook.io/release --version "$VERSION" -d "$pull"
step "operator $VERSION"
helm_private upgrade --install rook-ceph "$pull"/rook-ceph-v*.tgz -n "$NS" --create-namespace --wait --timeout 15m
step "cluster"
helm_private upgrade --install rook-ceph-cluster "$pull"/rook-ceph-cluster-*.tgz -n "$NS" \
  -f "$HERE/cluster-values.yaml" --wait --timeout 15m

want=$(nodes | wc -w | tr -d ' ')
for i in $(seq 1 90); do
  health=$(kubectl -n "$NS" get cephcluster -o jsonpath='{.items[0].status.ceph.health}' 2>/dev/null || true)
  osds=$(kubectl -n "$NS" get pods -l app=rook-ceph-osd --field-selector=status.phase=Running --no-headers 2>/dev/null | wc -l | tr -d ' ')
  [ "$health" = HEALTH_OK ] && [ "$osds" = "$want" ] && break
  sleep 20
done
kubectl -n "$NS" get cephcluster,cephblockpool
[ "$health" = HEALTH_OK ] && [ "$osds" = "$want" ] || fail "cluster not healthy: health=$health osds=$osds/$want"
kubectl -n "$NS" exec deploy/rook-ceph-tools -- ceph osd tree
kubectl -n "$NS" exec deploy/rook-ceph-tools -- ceph versions
