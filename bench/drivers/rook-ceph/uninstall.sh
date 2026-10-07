#!/usr/bin/env bash
# uninstall.sh -- remove Rook-Ceph: bench PVCs, the cluster release (deletes
# the CephCluster), the operator, the namespace, then each host's
# /var/lib/rook and any ceph LVM/device-mapper left on the instance store.
# wipe-disks.sh follows.
set -euo pipefail
. "$(cd "$(dirname "$0")/../.." && pwd)/lib/host.sh"
NS=rook-ceph
kubectl -n flint-bench delete pod -l app=bench-fio --wait=true --timeout=300s 2>/dev/null || true
kubectl -n flint-bench delete pvc --all --wait=true --timeout=600s 2>/dev/null || true
helm_private -n "$NS" uninstall rook-ceph-cluster --wait --timeout 15m || true
for i in $(seq 1 60); do kubectl -n "$NS" get cephcluster --no-headers 2>/dev/null | grep -q . || break; sleep 10; done
helm_private -n "$NS" uninstall rook-ceph --wait --timeout 10m || true
kubectl delete ns "$NS" --wait=true --timeout=600s --ignore-not-found
kubectl delete storageclass ceph-bench-r1 ceph-bench-r3 --ignore-not-found
samplers_up
for n in $(nodes); do
  hostexec "$n" '
    for v in $(vgs --noheadings -o vg_name 2>/dev/null | grep -E "^ *ceph-"); do vgremove -f "$v"; done
    for m in $(dmsetup ls 2>/dev/null | awk "/^ceph/{print \$1}"); do dmsetup remove "$m"; done
    rm -rf /var/lib/rook'
done
