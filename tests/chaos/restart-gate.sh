#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# restart-gate.sh -- the RESTART GATE: run before releasing a flint-csi-driver
# chart, on a real cluster with the release candidate installed.
#
#   KUBECONFIG=~/.kube/<cluster>.yaml AWS_REGION=<region> tests/chaos/restart-gate.sh <version>
#
# WHY THIS EXISTS. On 2026-08-02 the consumer's kernel NVMe initiator got a
# 20 s fail-fast bound (560c1d1e). Every unit test passed, and so did every
# gate we had, because none of them restarts a real spdk-tgt under a mounted
# volume. These drills had last run on 2026-07-21. When they ran again on
# 2026-10-22 (bench5), a raid-host target kill (1.9) and a kill under an RWX
# NFS server (3.3a) both ended in a read-only filesystem: a restart takes
# 20-125 s, so the bound turned it into EIO. The regression shipped in every
# release for almost three months. This gate is how a change to the restart
# path meets real restarts before it ships.
#
# RUN IT when a release touches: the node agent, nvme_recovery / kernel
# connect policy, NodeStage/NodeUnstage/NodePublish, the raid or export
# recovery paths, the spdk-tgt image, or chart values for the csi-node DS.
# A release that touches none of these may be waived (see scripts/release.sh).
#
# Needs: a 3-node cluster (EC2 spot: 3 x i4i.2xlarge, all three storage nodes,
# control-plane taint removed, AWS_PROFILE able to send SSM commands), Flint
# installed with the candidate images and the chaos SCs (storageclasses in
# bench/results/*/chaos/storageclasses.yaml: flint, flint-r3). About 45 min.
#
# Does, in order (each drill's own verdict decides; no stage is skipped):
#   1. kernel-deadline-test.sh on one node: same-node controller waits out a
#      target outage, the ctrl_loss_tmo backstop fails it, delete_controller
#      releases a blocked umount
#   2. RWO r3: the consumer controller carries the same-node policy; 2.2a,
#      1.9, 1.9 again (kubelet restart backoff), 2.7, 2.11 (every csi-node pod)
#   3. RWX r1: 3.1, 3.2, 3.3a, 3.8, 3.3a again
# Writes $OUT (default ./restart-gate-<version>) and, on PASS, the gate record
# tests/chaos/gate-records/flint-csi-driver-chart-<version>.txt that
# scripts/release.sh requires before it pushes that chart.
# Prints "RESTART-GATE PASS|FAIL".
# ---------------------------------------------------------------------------
set -u
VERSION=${1:?usage: restart-gate.sh <chart version>}
HERE=$(cd "$(dirname "$0")" && pwd)
NS=flint-system
OUT=${OUT:-$PWD/restart-gate-$VERSION}; mkdir -p "$OUT"
export RESULTS=$OUT/results.csv ARTIFACTS=$OUT/artifacts BASELINE_DIR=$OUT/baseline
mkdir -p "$ARTIFACTS" "$BASELINE_DIR"
log() { echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) $*" | tee -a "$OUT/gate.log"; }
fails=0; fail() { log "CHECK FAILED: $*"; fails=$((fails+1)); }

verdict_of_last_row() {
  python3 -c 'import csv,sys; r=list(csv.reader(open(sys.argv[1]))); print(r[-1][13] if len(r) > 1 and len(r[-1]) > 13 else "NONE")' "$RESULTS" 2>/dev/null || echo NONE
}
drill() {  # <label> <command...>
  local label=$1 before after v; shift
  log "drill $label"
  before=$(wc -l < "$RESULTS" 2>/dev/null || echo 0)
  ( cd "$HERE" && "$@" ) >> "$OUT/drills.log" 2>&1
  after=$(wc -l < "$RESULTS" 2>/dev/null || echo 0)
  if [ "$after" -gt "$before" ]; then v=$(verdict_of_last_row); else v=NO-ROW; fi
  log "done $label -> $v | $(tail -1 "$RESULTS" | cut -c1-180)"
  [ "$v" = PASS ] || fail "drill $label verdict $v"
}
lost_writes_in_last_drill() {  # the newest drill's db check found acked writes still absent on a by-value re-query
  local art; art=$(ls -td "$ARTIFACTS"/*/ 2>/dev/null | head -1)
  grep -q 'LOST ACKED WRITES' "${art}db-verdict.txt" 2>/dev/null
}
rwo_drill() {  # <label> <command...>: a drill on the RWO harness. A lost acked write keeps its
  # evidence: the legs are compared before anything else touches the volume, and the drills
  # after it get a fresh harness (the compare stops the workload).
  drill "$@"
  lost_writes_in_last_drill || return 0
  local pv; pv=$(kubectl -n flint-chaos get pvc data-pg-0 -o jsonpath='{.spec.volumeName}')
  log "lost acked writes in $1: comparing the legs of $pv before anything else touches it"
  ( cd "$HERE" && ./leg-compare.sh "$pv" "$OUT/leg-compare-$1" ) >> "$OUT/drills.log" 2>&1
  log "leg compare $1: $(tail -1 "$OUT/leg-compare-$1/diff.txt" 2>/dev/null) ($OUT/leg-compare-$1)"
  log "RWO: a fresh harness for the drills after $1"
  ( cd "$HERE" && SC=flint-r3 MODE=RWO ./deploy-harness.sh down && SC=flint-r3 MODE=RWO ./deploy-harness.sh up \
      && ./baseline.sh capture ) >> "$OUT/drills.log" 2>&1 || fail "RWO harness redeploy after $1"
}
controllers() {  # node ctrl traddr fio cl rd, for every flint kernel controller
  for p in $(kubectl -n $NS get pods -l app=flint-csi-node -o name); do
    n=$(kubectl -n $NS get "$p" -o jsonpath='{.spec.nodeName}')
    kubectl -n $NS exec "$p" -c flint-csi-driver -- sh -c 'for c in /sys/class/nvme/nvme*; do [ -f $c/subsysnqn ] && grep -q com.flint $c/subsysnqn && echo "$(basename $c) $(sed -n "s/.*traddr=\([^,]*\).*/\1/p" $c/address) fio=$(cat $c/fast_io_fail_tmo) cl=$(cat $c/ctrl_loss_tmo) rd=$(cat $c/reconnect_delay)"; done' 2>/dev/null < /dev/null | sed "s/^/$n /"
  done
}
policy_ok() {  # same-node: fast_io_fail off, ctrl_loss 300, reconnect 2; remote: fast_io_fail 60
  local out; out=$(controllers); echo "$out" > "$OUT/policy-$1.txt"
  log "kernel controllers ($1): $(echo "$out" | tr '\n' ';')"
  [ -n "$out" ] || { fail "policy $1: no flint kernel controllers"; return; }
  echo "$out" | awk '$3=="127.0.0.1" && !(($4=="fio=off" || $4=="fio=-1") && $5=="cl=300" && $6=="rd=2") {bad=1} $3!="127.0.0.1" && $4!="fio=60" {bad=1} END {exit bad}' \
    || fail "policy $1: a controller does not carry its policy (policy-$1.txt)"
}

log "restart gate for flint-csi-driver-chart $VERSION on $(kubectl config current-context 2>/dev/null)"
kubectl -n $NS get pods -l app=flint-csi-node -o jsonpath='{range .items[*]}{.spec.nodeName}{" "}{.status.containerStatuses[?(@.name=="flint-csi-driver")].imageID}{" "}{.status.initContainerStatuses[?(@.name=="spdk-tgt")].imageID}{"\n"}{end}' > "$OUT/images.txt"
cat "$OUT/images.txt" | tee -a "$OUT/gate.log"
[ "$(wc -l < "$OUT/images.txt" | tr -d ' ')" -ge 3 ] || fail "fewer than 3 csi-node pods"
( cd "$HERE" && bash -c '. ./lib.sh; for n in $(worker_nodes); do echo "$n $(instance_id_for_node $n)"; done' ) > "$OUT/nodes-instances.txt" 2>&1
[ "$(grep -c ' i-' "$OUT/nodes-instances.txt")" -ge 3 ] || fail "node->instance map incomplete (kill drills need SSM): nodes-instances.txt"

# ---- 1
KN=$(kubectl get nodes -o jsonpath='{.items[0].metadata.name}')
"$HERE/kernel-deadline-test.sh" "$KN" > "$OUT/kernel-deadline-test.txt" 2>&1
grep -E '^(SDL-KERNEL|connected)' "$OUT/kernel-deadline-test.txt" | while read -r l; do log "$l"; done
grep -q '^SDL-KERNEL PASS' "$OUT/kernel-deadline-test.txt" || fail "kernel deadline test"

# ---- 2
log "RWO: harness up (flint-r3)"
if ( cd "$HERE" && SC=flint-r3 MODE=RWO ./deploy-harness.sh up ) >> "$OUT/drills.log" 2>&1; then
  ( cd "$HERE" && ./baseline.sh capture ) >> "$OUT/drills.log" 2>&1
  policy_ok rwo
  rwo_drill 2.2a         ./drills/phase2.sh 2.2a
  rwo_drill 1.9          env PHASE_LABEL=1r3 ./drills/phase1.sh 1.9
  rwo_drill 1.9-again    env PHASE_LABEL=1r3 ./drills/phase1.sh 1.9
  rwo_drill 2.7          ./drills/phase2.sh 2.7
  pv=$(kubectl -n flint-chaos get pvc data-pg-0 -o jsonpath='{.spec.volumeName}')
  s=""
  for i in $(seq 1 30); do
    s=$(kubectl get pv "$pv" -o jsonpath='{.metadata.annotations.disk\.chert\.us/replica-sync-state}' | jq -c '[.replicas[].sync_state]' 2>/dev/null)
    [ "$s" = '["in_sync","in_sync","in_sync"]' ] && break; sleep 10
  done
  [ "$s" = '["in_sync","in_sync","in_sync"]' ] && rwo_drill 2.11 ./drills/phase2.sh 2.11 || fail "legs not in_sync 5 min after 2.7: $s"
else
  fail "RWO harness up (drills.log)"
fi
( cd "$HERE" && SC=flint-r3 MODE=RWO ./deploy-harness.sh down ) >> "$OUT/drills.log" 2>&1

# ---- 3
log "RWX: harness up (flint, WITNESS=1, SCALE=100)"
if ( cd "$HERE" && SCALE=100 SC=flint MODE=RWX WITNESS=1 ./deploy-harness.sh up ) >> "$OUT/drills.log" 2>&1; then
  policy_ok rwx
  drill 3.1        ./drills/phase3.sh 3.1
  drill 3.2        ./drills/phase3.sh 3.2
  drill 3.3a       ./drills/phase3.sh 3.3a
  drill 3.8        ./drills/phase3.sh 3.8
  drill 3.3a-again ./drills/phase3.sh 3.3a
else
  fail "RWX harness up (drills.log)"
fi
( cd "$HERE" && SC=flint MODE=RWX ./deploy-harness.sh down ) >> "$OUT/drills.log" 2>&1

if [ $fails = 0 ]; then
  log "RESTART-GATE PASS"
  mkdir -p "$HERE/gate-records"
  { echo "RESTART-GATE PASS flint-csi-driver-chart $VERSION $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "cluster: $(kubectl config current-context 2>/dev/null); nodes: $(kubectl get nodes --no-headers 2>/dev/null | awk '{print $1}' | tr '\n' ' ')"
    echo "images (node driver tgt):"; sed 's/^/  /' "$OUT/images.txt"
    echo "results:"; grep -E 'SDL-KERNEL|Z done |kernel controllers' "$OUT/gate.log" | sed 's/^/  /'
  } > "$HERE/gate-records/flint-csi-driver-chart-$VERSION.txt"
  echo "gate record: $HERE/gate-records/flint-csi-driver-chart-$VERSION.txt (commit it with the release)"
else
  log "RESTART-GATE FAIL ($fails failing checks)"
fi
