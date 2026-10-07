#!/usr/bin/env bash
# install.sh -- Longhorn with the V2 (SPDK) data engine, per its 1.13 docs:
# vfio_pci, uio_pci_generic and nvme-tcp loaded; 1024 x 2 MiB hugepages
# (trove reserves them); open-iscsi and an NFS client (Longhorn's general
# requirements); v2-data-engine=true; the instance store added to each node
# as a block-type disk with diskDriver auto (an NVMe controller by PCI
# address takes the userspace `nvme` driver -- the docs' recommendation for
# local NVMe). No default filesystem disk is created.
#
#   VERSION=1.13.0 [CPU_CORES=2] [DISK_DRIVER=auto|aio] ./install.sh
#
# CPU_CORES sets data-engine-cpu-mask (default 2 = 0x3; Pass B uses 1, which
# Longhorn's docs warn can starve RPCs under load) and raises the v2
# Guaranteed Instance Manager CPU to cover those cores, as the docs require.
# DISK_DRIVER=aio adds the disk by its by-id link instead (the documented
# fallback when the NVMe controller cannot be isolated).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/../../lib/host.sh"
VERSION="${VERSION:-1.13.0}"
CPU_CORES="${CPU_CORES:-2}"
DISK_DRIVER="${DISK_DRIVER:-auto}"
NS=longhorn-system
case "$CPU_CORES" in 1) mask=0x1 ;; 2) mask=0x3 ;; 3) mask=0x7 ;; 4) mask=0xF ;; *) fail "CPU_CORES 1-4" ;; esac

samplers_up
require_clean_disks
alloc_cores=$(kubectl get node "$(nodes | awk '{print $1}')" -o jsonpath='{.status.allocatable.cpu}')
case "$alloc_cores" in *m) alloc_m=${alloc_cores%m} ;; *) alloc_m=$(( alloc_cores * 1000 )) ;; esac
pct=$(( (CPU_CORES * 1000 * 100 + alloc_m - 1) / alloc_m ))   # ceil(cores / allocatable)

step "host prerequisites"
for n in $(nodes); do
  hostexec "$n" '
    set -e
    for m in vfio_pci uio_pci_generic nvme_tcp; do modprobe $m; done
    rpm -q iscsi-initiator-utils nfs-utils >/dev/null 2>&1 || dnf install -y -q iscsi-initiator-utils nfs-utils
    systemctl enable --now iscsid >/dev/null 2>&1
    grep -E "^HugePages_Total" /proc/meminfo' || fail "$n: prerequisites"
done

step "helm install longhorn $VERSION (v2 engine, cpu mask $mask, guaranteed v2 CPU ${pct}%)"
pull=$(mktemp -d)
helm_private pull longhorn --repo https://charts.longhorn.io --version "$VERSION" -d "$pull"
cat > "$pull/values.yaml" <<YAML
persistence:
  defaultClass: false
defaultSettings:
  v2DataEngine: true
  createDefaultDiskLabeledNodes: true
  dataEngineCPUMask: '{"v2":"$mask"}'
  guaranteedInstanceManagerCPU: '{"v1":"12","v2":"$pct"}'
YAML
helm_private upgrade --install longhorn "$pull"/longhorn-*.tgz -n "$NS" --create-namespace \
  -f "$pull/values.yaml" --wait --timeout 20m
kubectl -n "$NS" wait --for=condition=Ready pod -l longhorn.io/component=instance-manager --timeout=900s

step "add each node's instance store as a block-type disk (driver $DISK_DRIVER)"
for n in $(nodes); do
  read -r dev bdf byid <<<"$(instance_store "$n")"
  [ -n "${bdf:-}" ] || fail "$n: no instance store"
  if [ "$DISK_DRIVER" = aio ]; then path=$byid; else path=$bdf; fi
  kubectl -n "$NS" patch nodes.longhorn.io "$n" --type merge -p "{\"spec\":{\"disks\":{\"bench-nvme\":{
    \"path\":\"$path\",\"diskType\":\"block\",\"diskDriver\":\"$DISK_DRIVER\",\"allowScheduling\":true,
    \"evictionRequested\":false,\"storageReserved\":0,\"tags\":[]}}}}"
done
for i in $(seq 1 60); do
  ready=$(kubectl -n "$NS" get nodes.longhorn.io -o json | python3 -c '
import json, sys
n = 0
for it in json.load(sys.stdin)["items"]:
    st = (it.get("status", {}).get("diskStatus") or {}).get("bench-nvme") or {}
    if any(c.get("type") == "Ready" and c.get("status") == "True" for c in st.get("conditions", [])):
        n += 1
print(n)')
  [ "$ready" = "$(nodes | wc -w | tr -d ' ')" ] && break
  sleep 10
done
kubectl -n "$NS" get nodes.longhorn.io -o json | python3 -c '
import json, sys
for it in json.load(sys.stdin)["items"]:
    st = (it.get("status", {}).get("diskStatus") or {}).get("bench-nvme") or {}
    conds = {c["type"]: c["status"] + " " + c.get("message", "") for c in st.get("conditions", [])}
    print(it["metadata"]["name"], "driver=" + str(st.get("diskDriver")), conds)'
[ "$ready" = "$(nodes | wc -w | tr -d ' ')" ] || fail "not every block disk is Ready (try DISK_DRIVER=aio, and record it)"
kubectl apply -f "$HERE/storageclasses.yaml"
