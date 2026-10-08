#!/usr/bin/env bash
# epoch-cow-ab.sh -- does a snapshot of a replicated volume's lvols make its
# small writes slow? (bench/results/2026-10-07-ec2-phase1/README.md, "Root
# cause of finding 1".) One r3 volume, three measurements, ONE variable:
#
#   A  epoch scheduler OFF, volume preconditioned  -> expect fast
#   B  cut one snapshot on every replica lvol, exactly the RPC the epoch
#      scheduler issues (bdev_lvol_snapshot), measure again WITHOUT
#      rewriting                                   -> expect the r3 collapse
#   C  rewrite every cluster (precondition), measure again -> expect fast
#
# Run against a cluster whose Flint has replication.orchestrators.enabled=false
# (no scheduler cutting its own epochs mid-experiment), e.g. kind:
#   HELM_EXTRA="--set replication.orchestrators.enabled=false" drivers/flint/kind-up.sh
#
#   OUT=results/<date>-epoch-cow-ab ./experiments/epoch-cow-ab.sh
#
# Env: OUT (required)  SC (flint-bench-r3)  SIZE (20Gi)  FILE_SIZE (16G)
#      RUNTIME (30)  RAMP (5)  REPS (2)
set -euo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
: "${OUT:?set OUT}"
export SC="${SC:-flint-bench-r3}" SIZE="${SIZE:-20Gi}" FILE_SIZE="${FILE_SIZE:-16G}"
export RUNTIME="${RUNTIME:-30}" RAMP="${RAMP:-5}" REPS="${REPS:-2}"
export DRIVER=cowab TESTS="lat_randwrite_4k iops_randwrite_4k seq_write_1m"
. "$HERE/drivers/flint/env.sh"; export CPU_PATTERNS REACTOR_TICKS
mkdir -p "$OUT"
log() { echo "$(date -u +%H:%M:%SZ) $*" | tee -a "$OUT/steps.log" >&2; }

if kubectl -n flint-system get deploy -o yaml | grep -q 'FLINT_EPOCH_SCHEDULER'; then
  echo "the controller runs the epoch scheduler: install with replication.orchestrators.enabled=false" >&2
  exit 1
fi

log "A: scheduler off, preconditioned"
KEEP=1 PRECONDITION=1 OUT="$OUT/A-fresh" "$HERE/run-fio.sh" > "$OUT/A-fresh.log" 2>&1

pv=$(kubectl -n flint-bench get pvc bench-cowab -o jsonpath='{.spec.volumeName}')
kubectl get pv "$pv" -o yaml > "$OUT/pv.yaml"
replicas=$(kubectl get pv "$pv" -o jsonpath='{.spec.csi.volumeAttributes.disk\.chert\.us/replicas}')
log "B: snapshot every replica lvol ($pv)"
python3 -c 'import json,sys; [print(r["node_name"], r["lvs_name"]+"/"+r["lvol_name"]) for r in json.loads(sys.argv[1])]' "$replicas" |
while read -r node lvol; do
  p=$(kubectl -n flint-system get pods -l app=flint-csi-node --field-selector "spec.nodeName=$node" -o jsonpath='{.items[0].metadata.name}')
  kubectl -n flint-system exec "$p" -c spdk-tgt -- python3 /usr/local/scripts/rpc.py -s /var/tmp/spdk.sock \
    bdev_lvol_snapshot "$lvol" "cowab-snap" | tee -a "$OUT/snapshots.log"
  log "  snapshot $lvol on $node"
done
KEEP=1 PRECONDITION=0 OUT="$OUT/B-after-snapshot" "$HERE/run-fio.sh" > "$OUT/B-after-snapshot.log" 2>&1

log "C: rewrite every cluster, measure again"
KEEP=0 PRECONDITION=1 OUT="$OUT/C-rewritten" "$HERE/run-fio.sh" > "$OUT/C-rewritten.log" 2>&1

python3 "$HERE/report.py" "$OUT/A-fresh" "$OUT/B-after-snapshot" "$OUT/C-rewritten" --csv "$OUT/summary.csv" > "$OUT/summary.md"
log "done"
