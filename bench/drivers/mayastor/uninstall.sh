#!/usr/bin/env bash
# uninstall.sh -- remove Mayastor: bench PVCs, DiskPools, the release, the
# namespace, the node labels and the bench StorageClasses. wipe-disks.sh
# follows.
set -euo pipefail
. "$(cd "$(dirname "$0")/../.." && pwd)/lib/host.sh"
kubectl -n flint-bench delete pod -l app=bench-fio --wait=true --timeout=300s 2>/dev/null || true
kubectl -n flint-bench delete pvc --all --wait=true --timeout=600s 2>/dev/null || true
kubectl -n openebs delete diskpools --all --wait=true --timeout=600s 2>/dev/null || true
helm_private -n openebs uninstall openebs --wait --timeout 10m || true
kubectl delete ns openebs --wait=true --timeout=600s --ignore-not-found
for n in $(nodes); do kubectl label node "$n" openebs.io/engine- >/dev/null 2>&1 || true; done
kubectl delete -f "$(dirname "$0")/storageclasses.yaml" --ignore-not-found
