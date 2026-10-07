#!/usr/bin/env bash
# uninstall.sh -- remove Longhorn: bench PVCs, then the documented uninstall
# (deleting-confirmation-flag, helm uninstall), the namespace and the bench
# StorageClasses. The `nvme` disk driver leaves the controller unbound from
# the kernel; wipe-disks.sh rebinds it.
set -euo pipefail
. "$(cd "$(dirname "$0")/../.." && pwd)/lib/host.sh"
kubectl -n flint-bench delete pod -l app=bench-fio --wait=true --timeout=300s 2>/dev/null || true
kubectl -n flint-bench delete pvc --all --wait=true --timeout=600s 2>/dev/null || true
kubectl -n longhorn-system patch settings.longhorn.io deleting-confirmation-flag --type merge -p '{"value":"true"}' || true
helm_private -n longhorn-system uninstall longhorn --wait --timeout 15m || true
kubectl delete ns longhorn-system --wait=true --timeout=600s --ignore-not-found
kubectl delete -f "$(dirname "$0")/storageclasses.yaml" --ignore-not-found
