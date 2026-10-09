#!/usr/bin/env bash
# run-queue-c.sh -- reorder of 2026-10-08 23:4xZ (user: "can you run the fixed
# flint next. I want to see if the write performance improves"). Replaces
# run-flint-epochs-b / run-flint-f74c1-b / run-flint-f74c2-b / run-locality-b
# and the rest of run-phase2-b (stopped after Longhorn r3's in-flight matrix).
# Order:
#   1. wait for Longhorn r3's run-fio to exit; uninstall Longhorn, wipe
#   2. flint-r3-f74c2   changes 1+2 (driver 1.58.0-f74c1 + spdk-tgt 1.7.0-f74c2)
#   3. flint-r3-f74c1   change 1 alone
#   4. flint-r3-epoch300  1.58.0 as released, 300 s epochs: same-cluster control
#   5. phase2.sh rook-ceph mayastor (the repeat = drift check)
#   6. flint-r3-epoch3600  change 4 alone
#   7. locality pass (run-locality-b.sh, gated on this script's line)
# Every Flint arm: r3, Phase 1 matrix, 300 s epochs unless stated; refuses to
# measure unless the images and lvstore cluster sizes are what the arm claims.
set -u
cd "/Users/ddalton/github/flint/bench"
export KUBECONFIG=$HOME/.kube/bench3.yaml
. lib/host.sh
R=results/2026-10-08-ec2-phase2
status() { echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) queue-c: $*" >> "$R/status"; }
die() { status "FAILED: $*"; exit 1; }

clean_flint() {  # <tag>
  drivers/flint/uninstall.sh > "$R/uninstall-flint-$1.log" 2>&1 &&
    CONFIRM_WIPE=yes drivers/wipe-disks.sh > "$R/wipe-flint-$1.log" 2>&1 || die "uninstall/wipe flint ($1)"
  prune_images 2>> "$R/prune-queue-c.log"
}

flint_arm() {  # <name> <expected lvstore cluster bytes> <spdk tag or ""> <epoch s> <extra helm>
  local name=$1 csz=$2 tgt=$3 ep=$4 extra=$5 D=$R/flint-r3-$1
  status "install flint arm $name (epoch $ep${tgt:+, spdk-tgt $tgt})"
  VERSION=1.58.0 HELM_EXTRA="--set crds.installSnapshotCRDs=false --set replication.orchestrators.epochIntervalSecs=$ep ${tgt:+--set images.spdkTarget.tag=$tgt} $extra" \
    drivers/flint/ec2-up.sh > "$R/install-flint-$name.log" 2>&1 || die "install flint arm $name"
  kubectl -n flint-system get pods -o jsonpath='{range .items[*]}{.metadata.name}{"\t"}{range .spec.initContainers[*]}{.image}{" "}{end}{range .spec.containers[*]}{.image}{" "}{end}{"\n"}{end}' > "$R/flint-$name-images.txt"
  local nn; nn=$(kubectl -n flint-system get pods -l app=flint-csi-node -o name | wc -l | tr -d ' ')
  if [ -n "$tgt" ]; then
    local nt; nt=$(grep flint-csi-node "$R/flint-$name-images.txt" | grep -c "spdk-tgt:$tgt")
    { [ "$nn" -ge 3 ] && [ "$nt" = "$nn" ]; } || die "arm $name: spdk-tgt $tgt on $nt of $nn node pods -- NOT measuring"
  fi
  : > "$R/flint-$name-lvstores.txt"
  for p in $(kubectl -n flint-system get pods -l app=flint-csi-node -o name); do
    kubectl -n flint-system exec "$p" -c spdk-tgt -- python3 /usr/local/scripts/rpc.py -s /var/tmp/spdk.sock bdev_lvol_get_lvstores \
      >> "$R/flint-$name-lvstores.txt" 2>&1
  done
  local ok all
  ok=$(grep -c "\"cluster_size\": $csz" "$R/flint-$name-lvstores.txt"); all=$(grep -c '"cluster_size"' "$R/flint-$name-lvstores.txt")
  { [ "$all" -ge 3 ] && [ "$ok" = "$all" ]; } || die "arm $name: lvstores at $csz on $ok of $all -- NOT measuring"
  status "flint r3 matrix (arm $name; $all lvstores at $csz)"
  ( . drivers/flint/env.sh
    SC=$SC_R3 DRIVER=flint-r3-$name CPU_PATTERNS="$CPU_PATTERNS" REACTOR_TICKS="$REACTOR_TICKS" OUT=$D \
      SIZE=100Gi FILE_SIZE=90G RUNTIME=120 RAMP=30 REPS=3 ./run-fio.sh > "$D.log" 2>&1 )
  local rc=$?
  kubectl -n flint-system logs deploy/flint-csi-controller --all-containers --timestamps > "$R/flint-$name-controller.log" 2>&1
  grep -E "\[EPOCH\]" "$R/flint-$name-controller.log" > "$R/flint-$name-epoch-events.txt"
  [ $rc = 0 ] || die "flint r3 matrix (arm $name, rc=$rc)"
  status "done flint arm $name"
  clean_flint "$name"
}

status "start (reorder: fixed Flint next)"
while pgrep -f "bench/run-fio.sh" >/dev/null; do sleep 30; done
grep -q "done longhorn-a r3" "$R/status" || die "Longhorn r3 did not finish (see longhorn-a-r3.log)"
status "uninstall longhorn"
drivers/longhorn/uninstall.sh > "$R/uninstall-longhorn-a.log" 2>&1 &&
  CONFIRM_WIPE=yes drivers/wipe-disks.sh > "$R/wipe-longhorn-a.log" 2>&1 || die "uninstall/wipe longhorn"
prune_images 2>> "$R/prune-queue-c.log"

flint_arm f74c2    131072  1.7.0-f74c2 300  "--set images.flintCsiDriver.tag=1.58.0-f74c1"
flint_arm f74c1    131072  ""          300  "--set images.flintCsiDriver.tag=1.58.0-f74c1"
flint_arm epoch300 1048576 ""          300  ""

status "phase2 rook-ceph mayastor"
DRIVERS="rook-ceph mayastor" DONE_LIST="mayastor longhorn" CEILINGS=0 OUT="$R" ./phase2.sh > "$R/phase2-c.log" 2>&1 || die "phase2 rook-ceph mayastor"

flint_arm epoch3600 1048576 ""         3600 ""
status "ALL-DONE"
