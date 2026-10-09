#!/usr/bin/env bash
# run-locality-b.sh -- appended 2026-10-08 (user: "add a locality pass for
# Mayastor and Longhorn"). Their default r1 runs put the only replica on a
# different node from fio (Mayastor: pool-scheduler choice; Longhorn:
# replica on bench3-cp-1, engine on bench3-aws-1), while Flint's r1 replica is
# local, so the r1 comparison measured placement as much as data path. This
# pass reruns r1 with the replica pinned to the fio node:
#   Mayastor: DiskPools labelled bench-pool=<node> (install.sh POOL_LABELS=1),
#             StorageClass poolAffinityTopologyLabel "bench-pool: $FIO_NODE"
#             (upstream has no "local" parameter and ignores CSI topology).
#   Longhorn: dataLocality strict-local.
# fio pinned to FIO_NODE (nodeSelector). r3 is not repeated: on 3 nodes every
# node already holds a replica. A watcher samples placement every 60 s during
# the matrix; the arm is marked NOT LOCAL if any sample disagrees.
#
# run-locality-c.sh: same pass, gated on run-queue-c.sh (the 23:4xZ reorder)
# ending ALL-DONE.
set -u
cd "/Users/ddalton/github/flint/bench"
export KUBECONFIG=$HOME/.kube/bench3.yaml
. lib/host.sh
R=results/2026-10-08-ec2-phase2
M="relaunch after longhorn disk-pressure fix"
FIO_NODE=bench3-aws-1
status() { echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) locality: $*" >> "$R/status"; }
since() { sed -n "/$M/,\$p" "$R/status"; }

until grep -q -E "queue-c: (ALL-DONE|FAILED)" "$R/status"; do sleep 60; done
if ! grep -q "queue-c: ALL-DONE" "$R/status"; then
  status "queue-c did not finish ALL-DONE -- locality pass SKIPPED"; exit 1
fi

if kubectl get ns flint-system >/dev/null 2>&1; then
  status "uninstall flint (end of the Flint chain)"
  drivers/flint/uninstall.sh > "$R/uninstall-flint-end.log" 2>&1 &&
    CONFIRM_WIPE=yes drivers/wipe-disks.sh > "$R/wipe-flint-end.log" 2>&1 ||
    { status "FAILED uninstall/wipe flint"; exit 1; }
fi
prune_images 2>> "$R/prune-locality.log"
fio_ip=$(kubectl get node $FIO_NODE -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')

# placement <driver> -> one line: "<utc> replica=<node(s)> target=<node|ip> local=yes|no"
placement() {
  local t; t=$(date -u +%H:%M:%SZ)
  case "$1" in
    mayastor)
      # thick r1 volume: the pool holding it shows ~SIZE used; target = the
      # NVMe-oF address the fio node's kernel connected to.
      local used tgt
      used=$(kubectl -n openebs get diskpools -o json | python3 -c '
import json,sys
for p in json.load(sys.stdin)["items"]:
    u=int((p.get("status") or {}).get("used") or 0)
    if u > 50*2**30: print(p["spec"]["node"])' | tr '\n' ',' )
      tgt=$(hostexec $FIO_NODE 'for c in /sys/class/nvme/nvme*; do [ "$(cat $c/transport)" = tcp ] && cat $c/address; done' 2>/dev/null | sed -n 's/.*traddr=\([^,]*\).*/\1/p' | sort -u | tr '\n' ',')
      # local = the fio node's io-engine, by node IP or (if not hostNetwork) pod IP
      local eng_ip; eng_ip=$(kubectl -n openebs get pod -l app=io-engine --field-selector spec.nodeName=$FIO_NODE -o jsonpath='{.items[0].status.podIP}')
      local ok=no; [ "$used" = "$FIO_NODE," ] && { [ "$tgt" = "$fio_ip," ] || [ "$tgt" = "$eng_ip," ]; } && ok=yes
      echo "$t replica=$used target=$tgt local=$ok" ;;
    longhorn)
      local rep eng
      rep=$(kubectl -n longhorn-system get replicas.longhorn.io -o jsonpath='{range .items[*]}{.spec.nodeID}{","}{end}')
      eng=$(kubectl -n longhorn-system get engines.longhorn.io -o jsonpath='{range .items[*]}{.spec.nodeID}{","}{end}')
      local ok=no; [ "$rep" = "$FIO_NODE," ] && [ "$eng" = "$FIO_NODE," ] && ok=yes
      echo "$t replica=$rep engine=$eng local=$ok" ;;
  esac
}

for d in mayastor longhorn; do
  label=$d-local-r1; D=$R/$label; P=$R/$label-placement.txt
  status "install $d (locality pass)"
  if [ $d = mayastor ]; then
    POOL_LABELS=1 VERSION=4.6.2 drivers/mayastor/install.sh > "$R/install-$label.log" 2>&1 &&
      drivers/mayastor/storageclass-local.sh $FIO_NODE | kubectl apply -f - >> "$R/install-$label.log" 2>&1
    SC=mayastor-bench-r1-local
  else
    VERSION=1.13.0 drivers/longhorn/install.sh > "$R/install-$label.log" 2>&1 &&
      kubectl apply -f drivers/longhorn/storageclass-local.yaml >> "$R/install-$label.log" 2>&1
    SC=longhorn-v2-bench-r1-local
  fi
  [ $? = 0 ] || { status "FAILED install $d (locality)"; exit 1; }
  status "run $label ($SC, fio on $FIO_NODE)"
  ( . drivers/$d/env.sh
    SC=$SC DRIVER=$label FIO_NODE=$FIO_NODE CPU_PATTERNS="$CPU_PATTERNS" REACTOR_TICKS="${REACTOR_TICKS:-}" OUT=$D \
      ./run-fio.sh > "$D.log" 2>&1 ) &
  fio_pid=$!
  : > "$P"
  while kill -0 $fio_pid 2>/dev/null; do
    if kubectl -n flint-bench get pvc -o jsonpath='{.items[*].status.phase}' 2>/dev/null | grep -q Bound; then
      placement $d >> "$P" 2>/dev/null
    fi
    sleep 60
  done
  wait $fio_pid; rc=$?
  n=$(grep -c . "$P"); bad=$(grep -c "local=no" "$P")
  [ $rc = 0 ] || { status "FAILED $label matrix (rc=$rc)"; exit 1; }
  if [ "$n" -gt 0 ] && [ "$bad" = 0 ]; then status "done $label (placement local in $n of $n samples)"
  else status "WARNING $label placement NOT LOCAL in $bad of $n samples -- see $(basename $P)"; fi
  status "uninstall $d (locality)"
  { drivers/$d/uninstall.sh && kubectl delete sc $SC --ignore-not-found &&
    CONFIRM_WIPE=yes drivers/wipe-disks.sh; } > "$R/uninstall-$label.log" 2>&1 ||
    { status "FAILED uninstall/wipe $d (locality)"; exit 1; }
  prune_images 2>> "$R/prune-locality.log"
done
status "ALL-DONE"
