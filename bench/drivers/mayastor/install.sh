#!/usr/bin/env bash
# install.sh -- OpenEBS Replicated PV Mayastor, per its docs: nvme_tcp loaded,
# storage nodes labelled openebs.io/engine=mayastor, 2 GiB of 2 MiB
# hugepages (trove reserves them), one DiskPool per node on the instance
# store by its stable by-id link (the documented best practice, aio).
# Engines that are not on the block path (LVM, ZFS local PV) and the log
# stack (Loki, Alloy) are off: their CPU would land in the measurements.
#
#   VERSION=4.6.2 [CPU_CORES=2] ./install.sh
#
# CPU_CORES sets io_engine.cpuCount (chart default 2; Pass B uses 1).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/../../lib/host.sh"
VERSION="${VERSION:-4.6.2}"
CPU_CORES="${CPU_CORES:-2}"
NS=openebs

samplers_up
require_clean_disks
for n in $(nodes); do
  hostexec "$n" "modprobe nvme_tcp && grep -q '^nvme_tcp ' /proc/modules" || fail "$n: nvme_tcp"
  kubectl label node "$n" openebs.io/engine=mayastor --overwrite >/dev/null
done

step "helm install openebs $VERSION (Mayastor io_engine.cpuCount=$CPU_CORES)"
pull=$(mktemp -d)
helm_private pull openebs --repo https://openebs.github.io/openebs --version "$VERSION" -d "$pull"
helm_private upgrade --install openebs "$pull"/openebs-*.tgz -n "$NS" --create-namespace \
  --set engines.local.lvm.enabled=false \
  --set engines.local.zfs.enabled=false \
  --set loki.enabled=false --set alloy.enabled=false \
  --set mayastor.loki.enabled=false --set mayastor.alloy.enabled=false \
  --set mayastor.io_engine.cpuCount="$CPU_CORES" \
  --wait --timeout 20m
kubectl -n "$NS" wait --for=condition=Ready pod -l app=io-engine --timeout=600s

step "one DiskPool per node on its instance store"
for n in $(nodes); do
  read -r dev bdf byid <<<"$(instance_store "$n")"
  [ -n "${byid:-}" ] || fail "$n: no instance store by-id link"
  kubectl apply -f - <<YAML
apiVersion: openebs.io/v1beta3
kind: DiskPool
metadata: {name: pool-$n, namespace: $NS}
spec:
  node: $n
  disks: ["aio://$byid"]
YAML
done
for i in $(seq 1 60); do
  online=$(kubectl -n "$NS" get diskpools -o jsonpath='{range .items[*]}{.status.pool_status}{"\n"}{end}' | grep -c '^Online$' || true)
  [ "$online" = "$(nodes | wc -w | tr -d ' ')" ] && break
  sleep 10
done
kubectl -n "$NS" get diskpools
[ "$online" = "$(nodes | wc -w | tr -d ' ')" ] || fail "not every DiskPool is Online"
kubectl apply -f "$HERE/storageclasses.yaml"
