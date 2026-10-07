#!/usr/bin/env bash
# uninstall.sh -- remove Flint completely so the next driver gets clean disks:
# bench PVCs first (so volumes are deleted by the driver that made them),
# then the release, its namespace and the CSIDriver. wipe-disks.sh follows.
set -euo pipefail
. "$(cd "$(dirname "$0")/../.." && pwd)/lib/host.sh"
kubectl -n flint-bench delete pod -l app=bench-fio --wait=true --timeout=300s 2>/dev/null || true
kubectl -n flint-bench delete pvc --all --wait=true --timeout=600s 2>/dev/null || true
helm -n flint-system uninstall flint-csi --wait --timeout 10m || true
kubectl delete ns flint-system --wait=true --timeout=600s --ignore-not-found
# trove's preinstalled Flint adds flint-nfs / flint-pnfs, and releases before
# 1.57 registered the driver as flint.csi.storage.io.
kubectl delete storageclass flint-bench-r1 flint-bench-r3 flint-spdk flint-nfs flint-pnfs --ignore-not-found
kubectl delete csidriver disk.csi.chert.us flint.csi.storage.io --ignore-not-found
