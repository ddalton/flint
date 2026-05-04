#!/usr/bin/env bash
#
# pNFS cross-host bench — produces the headline scaling number.
#
# Topology: 1 control + N workers (4 recommended). One worker is the
# MDS, $|DS_NODES| workers are DSes (one each), one worker is the
# client. See README.md for full topology / NIC / disk requirements.
#
# Workflow:
#   1. apply Namespace + MDS Deployment + DS Deployments + client.
#   2. wait for everything Ready and DSes registered with MDS.
#   3. for each (bs, rw, jobs) cell:
#        a. drop client cache
#        b. run fio inside the client pod, mounting the MDS Service
#        c. capture aggregate MiB/s
#   4. dump TSV + markdown summary.
#   5. tear down.
#
# Exit 0 on completion regardless of pass/fail of the perf threshold —
# this is exploratory, the script's job is to produce the numbers, not
# gate on them. The README's "pass criterion" is documentary.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
HERE="$REPO_ROOT/tests/k8s/pnfs-bench"
RESULTS_DIR="$HERE/results"
mkdir -p "$RESULTS_DIR"

# ─── Required env ────────────────────────────────────────────────────
: "${KUBECONFIG:?KUBECONFIG must point at the cluster}"
: "${PNFS_IMAGE:?PNFS_IMAGE required (must be pullable from cluster)}"
: "${MDS_NODE:?MDS_NODE required}"
: "${DS_NODES:?DS_NODES required (space-separated worker names)}"
: "${CLIENT_NODE:?CLIENT_NODE required}"
NS="${NAMESPACE:-pnfs-bench}"

# ─── Sweep dimensions ─────────────────────────────────────────────────
SIZE_MB="${SIZE_MB_OVERRIDE:-256}"           # per-fio-job size
JOBS_VALUES="${JOBS_VALUES_OVERRIDE:-4}"     # parallelism (single value
                                              # by default; comma-list to
                                              # sweep)
BS_VALUES="${BS_VALUES_OVERRIDE:-4K 1M}"
MODES="${MODES_OVERRIDE:-write read}"
NCONNECT="${NCONNECT_OVERRIDE:-16}"          # Linux NFSv4.1 supports
                                              # up to 16 TCP conns per
                                              # mount

DS_COUNT=$(echo "$DS_NODES" | wc -w | tr -d ' ')
TS=$(date +%Y-%m-%d-%H%M%S)
RESULTS_TSV="$RESULTS_DIR/cross-host-results-$TS.tsv"
LOG="$RESULTS_DIR/cross-host-$TS.log"
: > "$RESULTS_TSV"
: > "$LOG"

log() { printf '%s  %s\n' "$(date +%H:%M:%S)" "$*" | tee -a "$LOG"; }

cleanup() {
  set +e
  log "▶ teardown: deleting namespace $NS"
  kubectl delete namespace "$NS" --wait=false --ignore-not-found=true >>"$LOG" 2>&1
}
trap cleanup EXIT

log "▶ pNFS cross-host bench"
log "  KUBECONFIG : $KUBECONFIG"
log "  image      : $PNFS_IMAGE"
log "  MDS        : $MDS_NODE"
log "  DSes (N=$DS_COUNT): $DS_NODES"
log "  client     : $CLIENT_NODE"
log "  workload   : $JOBS_VALUES jobs × ${SIZE_MB}MiB × bs={$BS_VALUES} × {$MODES}"
log "  results    : $RESULTS_TSV"

# ─── Pre-flight ───────────────────────────────────────────────────────
if ! kubectl version --request-timeout=5s >/dev/null 2>&1; then
  log "✗ kubectl can't reach the cluster. Check KUBECONFIG."
  exit 1
fi
for node in "$MDS_NODE" $DS_NODES "$CLIENT_NODE"; do
  if ! kubectl get node "$node" >/dev/null 2>&1; then
    log "✗ node '$node' not found in cluster"
    exit 1
  fi
done
log "✓ cluster reachable, all nodes exist"

# ─── Apply ────────────────────────────────────────────────────────────
log "▶ applying manifests"
"$HERE/manifests.sh" | kubectl apply -f - >>"$LOG" 2>&1

# Wait for MDS + DSes + client to be Ready.
log "▶ waiting for MDS Ready"
kubectl -n "$NS" wait --for=condition=available deploy/pnfs-mds --timeout=120s >>"$LOG" 2>&1 || {
  log "✗ MDS didn't become Ready"; kubectl -n "$NS" get pods -o wide; exit 1; }

DS_DEPLOYS=()
for i in $(seq 1 "$DS_COUNT"); do DS_DEPLOYS+=("deploy/pnfs-ds$i"); done
log "▶ waiting for ${#DS_DEPLOYS[@]} DS Ready"
for d in "${DS_DEPLOYS[@]}"; do
  kubectl -n "$NS" wait --for=condition=available "$d" --timeout=120s >>"$LOG" 2>&1 || {
    log "✗ $d didn't become Ready"; kubectl -n "$NS" get pods -o wide; exit 1; }
done

log "▶ waiting for client Ready"
kubectl -n "$NS" wait --for=condition=available deploy/pnfs-bench-client --timeout=300s >>"$LOG" 2>&1 || {
  log "✗ client didn't become Ready"; exit 1; }
CLIENT_POD=$(kubectl -n "$NS" get pod -l app=pnfs-bench-client -o jsonpath='{.items[0].metadata.name}')
log "✓ client pod: $CLIENT_POD"

# Give DSes a heartbeat window to register with MDS.
log "▶ giving DSes 10s to register with MDS"
sleep 10
REGISTERED=$(kubectl -n "$NS" logs deploy/pnfs-mds | grep -c "DS registered successfully" || true)
log "  registered devices in MDS log: $REGISTERED (expected: $DS_COUNT)"
if [ "$REGISTERED" -lt "$DS_COUNT" ]; then
  log "  ⚠ fewer DSes registered than expected — bench will run anyway, results may be skewed"
fi

# ─── Mount ────────────────────────────────────────────────────────────
MDS_IP=$(kubectl -n "$NS" get svc pnfs-mds -o jsonpath='{.spec.clusterIP}')
log "▶ MDS Service ClusterIP: $MDS_IP"

mount_cmd=$(cat <<EOM
set -eux
mountpoint -q /mnt/pnfs && umount -lf /mnt/pnfs || true
mkdir -p /mnt/pnfs
mount -t nfs4 -o minorversion=1,port=2049,nconnect=$NCONNECT,rsize=1048576,wsize=1048576 \
  $MDS_IP:/ /mnt/pnfs
ls /mnt/pnfs
EOM
)
log "▶ mounting NFSv4.1 from client → MDS Service"
kubectl -n "$NS" exec "$CLIENT_POD" -- bash -lc "$mount_cmd" >>"$LOG" 2>&1 || {
  log "✗ mount failed"; tail -30 "$LOG"; exit 1; }
log "✓ mount succeeded"

# ─── Helpers ──────────────────────────────────────────────────────────
drop_cache() {
  # Skip sync — it hangs in D state on pNFS mounts. Just drop clean
  # pages; fio's end_fsync=1 handles dirty data.
  kubectl -n "$NS" exec "$CLIENT_POD" -- bash -lc \
    'echo 3 > /proc/sys/vm/drop_caches 2>/dev/null' >/dev/null 2>&1 || true
}

# Run fio inside the client pod and emit aggregate MiB/s on stdout.
fio_phase() {
  local rw="$1" bs="$2" jobs="$3"
  local tag="N${DS_COUNT}-${rw}-${bs}-j${jobs}"
  local out_json="/results/${tag}.json"
  drop_cache
  local cmd=$(cat <<EOM
fio \
  --name=bench --directory=/mnt/pnfs \
  --rw=$rw --bs=$bs --numjobs=$jobs --size=${SIZE_MB}M \
  --ioengine=libaio --iodepth=16 --direct=0 --end_fsync=1 \
  --group_reporting --output-format=json > $out_json 2>/dev/null || true
jq -r ".jobs[0].$rw.bw // 0" < $out_json
EOM
)
  local kbps
  kbps=$(kubectl -n "$NS" exec "$CLIENT_POD" -- bash -lc "$cmd" 2>>"$LOG" | tr -d '\r')
  awk -v k="$kbps" 'BEGIN { printf "%.1f", k/1024 }'
}

# ─── Sweep ────────────────────────────────────────────────────────────
log "▶ sweep starting"
printf 'n_ds\tbs\trw\tjobs\tmibs\n' > "$RESULTS_TSV"
for jobs in $JOBS_VALUES; do
  for bs in $BS_VALUES; do
    for rw in $MODES; do
      log "  N=$DS_COUNT bs=$bs rw=$rw jobs=$jobs"
      mibs=$(fio_phase "$rw" "$bs" "$jobs")
      log "    → $mibs MiB/s"
      printf '%s\t%s\t%s\t%s\t%s\n' "$DS_COUNT" "$bs" "$rw" "$jobs" "$mibs" >> "$RESULTS_TSV"
    done
  done
done

# Per-DS allocation sanity. Bytes-on-disk per DS should be ~equal for
# striped writes; gross imbalance means stripe alignment is off.
log "▶ per-DS allocation sanity"
for i in $(seq 1 "$DS_COUNT"); do
  pod=$(kubectl -n "$NS" get pod -l app=pnfs-ds,ds=ds$i -o jsonpath='{.items[0].metadata.name}')
  bytes=$(kubectl -n "$NS" exec "$pod" -- bash -lc 'find /var/lib/flint-pnfs/exports -type f -exec stat -c %s {} + 2>/dev/null | awk "{s+=\$1} END {print s+0}"')
  log "  DS$i ($pod): $bytes bytes"
done

# ─── Summary ──────────────────────────────────────────────────────────
log ""
log "════════════════════════════════════════════════════════════════"
log "RESULTS — N=$DS_COUNT DS workers, $JOBS_VALUES jobs × ${SIZE_MB} MiB"
log "════════════════════════════════════════════════════════════════"
{
  printf '\n| %-7s | %-3s | %-7s |\n' "bs" "rw" "MiB/s"
  printf '| %-7s | %-3s | %-7s |\n' "-------" "---" "-------"
  awk -v ndc="$DS_COUNT" -F'\t' 'NR>1 && $1==ndc { printf "| %-7s | %-3s | %7s |\n", $2, $3, $5 }' "$RESULTS_TSV"
  printf '\n'
} | tee -a "$LOG"

log "▶ TSV:  $RESULTS_TSV"
log "▶ Log:  $LOG"
log ""
log "Compare against single-host baseline:"
log "  tests/lima/pnfs/nconnect-results-2026-05-03.tsv"
log "  (loopback-bound; cross-host should exceed by 1.8×+ on writes."
log "   Documented thresholds in tests/k8s/pnfs-bench/README.md.)"
exit 0
