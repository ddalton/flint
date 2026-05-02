#!/usr/bin/env bash
#
# bench-sweep.sh — parameterised pNFS-vs-single-server benchmark.
#
# Runs the same fio workload across a sweep of (numjobs, fsync, rw)
# variants against both the single-server NFS path and the pNFS
# (MDS+2 DSes) path. Goal: figure out *why* bench.sh saw a 2.10×
# write win — is it real protocol parallelism, or a fsync
# serialization artifact in flint-nfs-server, or something else?
#
# Hypotheses being tested:
#   H1 (fsync): single-server's slow writes are stalls on a
#       serialized end_fsync. If we set --end_fsync=0 the win
#       collapses.
#   H2 (concurrency): pNFS only wins when numjobs > 1 because
#       the parallelism is across-jobs, not within-job. numjobs=1
#       should be ~tied.
#   H3 (write-only): pNFS reads tied bench.sh because reads on a
#       single host saturate loopback before per-server matters.
#
# Output:
#   * Table to stdout with bandwidth per (server, rw, numjobs, fsync).
#   * /tmp/flint-pnfs-sweep-<tag>.json per fio run for inspection.
#   * /tmp/flint-pnfs-sweep-summary.tsv with all numbers, easy to plot.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN_DIR="$REPO_ROOT/spdk-csi-driver/target/release"
CFG_DIR="$REPO_ROOT/tests/lima/pnfs"
LOG_DIR="/tmp"

LIMA_VM="${LIMA_VM:-flint-nfs-client}"
HOST_ADDR="host.lima.internal"
MDS_PORT=20490
NFS_PORT=20480
DS1_EXPORT="/tmp/flint-pnfs-ds1"
DS2_EXPORT="/tmp/flint-pnfs-ds2"
MDS_EXPORT_DIR="/tmp/flint-pnfs-mds-exports"
NFS_EXPORT_DIR="/tmp/flint-nfs-export"
VM_MOUNT="/mnt/flint-bench"

# Workload sweep dimensions.
JOBS_SWEEP=(1 4 8)
FSYNC_SWEEP=(0 1)
RW_SWEEP=(write read)
SIZE_MB=128   # per job — keep total small so sweep finishes in <15 min

SUMMARY_TSV="$LOG_DIR/flint-pnfs-sweep-summary.tsv"

cleanup() {
  set +e
  limactl shell "$LIMA_VM" -- sudo umount -lf "$VM_MOUNT" 2>/dev/null
  pkill -f flint-pnfs-mds 2>/dev/null
  pkill -f flint-pnfs-ds  2>/dev/null
  pkill -f flint-nfs-server 2>/dev/null
  rm -f /tmp/flint-pnfs-{mds,ds1,ds2}.pid /tmp/flint-nfs.pid
}
trap cleanup EXIT

# ──────────────────────────────────────────────────────────────────────
# Pre-flight
# ──────────────────────────────────────────────────────────────────────

echo "▶ pNFS bench-sweep"
echo "  jobs: ${JOBS_SWEEP[*]}, fsync: ${FSYNC_SWEEP[*]}, rw: ${RW_SWEEP[*]}"
echo "  per-job size: ${SIZE_MB} MiB"
echo

for bin in flint-pnfs-mds flint-pnfs-ds flint-nfs-server; do
  [ -x "$BIN_DIR/$bin" ] || { echo "✗ Missing: $BIN_DIR/$bin"; exit 1; }
done
limactl list --quiet 2>/dev/null | grep -qx "$LIMA_VM" \
  || { echo "✗ Lima VM '$LIMA_VM' not running (make lima-up)"; exit 1; }

limactl shell "$LIMA_VM" -- sudo mkdir -p "$VM_MOUNT" >/dev/null

# Header for the TSV.
printf 'server\trw\tjobs\tfsync\tbw_kbps\tbw_mibs\n' > "$SUMMARY_TSV"

# ──────────────────────────────────────────────────────────────────────
# Helpers
# ──────────────────────────────────────────────────────────────────────

drop_client_cache() {
  limactl shell "$LIMA_VM" \
    -- sudo bash -c 'sync && echo 3 > /proc/sys/vm/drop_caches' \
    >/dev/null 2>&1
}

# Run one fio invocation, parse JSON, return KB/s aggregate on stdout.
# $1: tag (server-rw-numjobs-fsync), $2: rw, $3: numjobs, $4: fsync
fio_run() {
  local tag="$1" rw="$2" jobs="$3" fsync="$4"
  local out="$LOG_DIR/flint-pnfs-sweep-$tag.json"

  drop_client_cache

  limactl shell "$LIMA_VM" -- bash -c "fio \
    --name=bench-$tag \
    --directory=$VM_MOUNT \
    --rw=$rw \
    --bs=1M \
    --numjobs=$jobs \
    --size=${SIZE_MB}M \
    --ioengine=libaio \
    --iodepth=16 \
    --direct=0 \
    --end_fsync=$fsync \
    --group_reporting \
    --output-format=json" > "$out" 2>>"$LOG_DIR/flint-pnfs-sweep.stderr"

  jq -r ".jobs[0].$rw.bw // 0" < "$out"
}

format_mbps() { awk -v k="$1" 'BEGIN { printf "%7.1f", k / 1024 }'; }

# Run the full variant sweep against whichever server is currently mounted.
sweep_against() {
  local server_tag="$1"   # "single" or "pnfs"
  echo
  echo "  ─── sweeping against $server_tag ───"
  printf '  %-6s %5s %5s %12s\n' rw jobs fsync 'MiB/s'

  for rw in "${RW_SWEEP[@]}"; do
    for jobs in "${JOBS_SWEEP[@]}"; do
      for fsync in "${FSYNC_SWEEP[@]}"; do
        # fsync only meaningful for writes — skip read+fsync to halve runtime.
        if [ "$rw" = "read" ] && [ "$fsync" = "1" ]; then
          continue
        fi

        local tag="$server_tag-$rw-j${jobs}-fs${fsync}"
        local kbps mibs
        kbps=$(fio_run "$tag" "$rw" "$jobs" "$fsync")
        mibs=$(format_mbps "$kbps")
        printf '  %-6s %5d %5d %12s MiB/s\n' "$rw" "$jobs" "$fsync" "$mibs"
        printf '%s\t%s\t%s\t%s\t%s\t%.1f\n' \
          "$server_tag" "$rw" "$jobs" "$fsync" "$kbps" \
          "$(awk -v k="$kbps" 'BEGIN{printf "%.1f", k/1024}')" \
          >> "$SUMMARY_TSV"
      done
    done
  done
}

# ──────────────────────────────────────────────────────────────────────
# Phase 1 — single-server NFS
# ──────────────────────────────────────────────────────────────────────

echo "═══ Phase 1: single-server NFS ═══"
rm -rf "$NFS_EXPORT_DIR"; mkdir -p "$NFS_EXPORT_DIR"; chmod 0777 "$NFS_EXPORT_DIR"

nohup "$BIN_DIR/flint-nfs-server" \
  --bind-addr "0.0.0.0" --port "$NFS_PORT" \
  --export-path "$NFS_EXPORT_DIR" --volume-id bench \
  > "$LOG_DIR/flint-nfs.log" 2>&1 &
echo $! > /tmp/flint-nfs.pid
sleep 1

limactl shell "$LIMA_VM" -- sudo mount -t nfs4 \
  -o "minorversion=1,proto=tcp,port=$NFS_PORT,nconnect=4,rsize=1048576,wsize=1048576" \
  "$HOST_ADDR:/" "$VM_MOUNT" \
  || { echo "✗ single-server mount failed"; exit 1; }

sweep_against single

limactl shell "$LIMA_VM" -- sudo umount "$VM_MOUNT" >/dev/null 2>&1
kill "$(cat /tmp/flint-nfs.pid)" 2>/dev/null
wait 2>/dev/null
rm -f /tmp/flint-nfs.pid

# ──────────────────────────────────────────────────────────────────────
# Phase 2 — pNFS
# ──────────────────────────────────────────────────────────────────────

echo
echo "═══ Phase 2: pNFS (MDS + 2 DSes) ═══"
rm -rf "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"
mkdir -p "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"
chmod 0777 "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"

PNFS_MODE=mds nohup "$BIN_DIR/flint-pnfs-mds" --config "$CFG_DIR/mds.yaml" \
  > "$LOG_DIR/flint-pnfs-mds.log" 2>&1 &
echo $! > /tmp/flint-pnfs-mds.pid
sleep 1

for n in 1 2; do
  PNFS_MODE=ds nohup "$BIN_DIR/flint-pnfs-ds" --config "$CFG_DIR/ds${n}.yaml" \
    > "$LOG_DIR/flint-pnfs-ds${n}.log" 2>&1 &
  echo $! > "/tmp/flint-pnfs-ds${n}.pid"
done
sleep 2

limactl shell "$LIMA_VM" -- sudo mount -t nfs4 \
  -o "minorversion=1,proto=tcp,port=$MDS_PORT,nconnect=4,rsize=1048576,wsize=1048576" \
  "$HOST_ADDR:/" "$VM_MOUNT" \
  || { echo "✗ pNFS mount failed"; exit 1; }

sweep_against pnfs

limactl shell "$LIMA_VM" -- sudo umount "$VM_MOUNT" >/dev/null 2>&1

# ──────────────────────────────────────────────────────────────────────
# Comparison table
# ──────────────────────────────────────────────────────────────────────

echo
echo "════════════════════════════════════════════════════════════════"
echo "  Summary: pNFS / single-server ratio (write only — reads tie)"
echo "════════════════════════════════════════════════════════════════"
printf '  %-5s %-5s %-12s %-12s %-8s\n' jobs fsync 'single MiB/s' 'pnfs MiB/s' 'ratio'

for rw in "${RW_SWEEP[@]}"; do
  for jobs in "${JOBS_SWEEP[@]}"; do
    for fsync in "${FSYNC_SWEEP[@]}"; do
      [ "$rw" = "read" ] && [ "$fsync" = "1" ] && continue

      single=$(awk -F'\t' -v rw="$rw" -v j="$jobs" -v fs="$fsync" \
        '$1=="single" && $2==rw && $3==j && $4==fs { print $5 }' "$SUMMARY_TSV")
      pnfs=$(awk -F'\t' -v rw="$rw" -v j="$jobs" -v fs="$fsync" \
        '$1=="pnfs" && $2==rw && $3==j && $4==fs { print $5 }' "$SUMMARY_TSV")

      [ -z "$single" ] || [ -z "$pnfs" ] && continue

      ratio=$(awk -v a="$pnfs" -v b="$single" \
        'BEGIN { if (b > 0) printf "%.2fx", a / b; else print "—" }')
      single_mib=$(awk -v k="$single" 'BEGIN{printf "%.1f", k/1024}')
      pnfs_mib=$(awk -v k="$pnfs" 'BEGIN{printf "%.1f", k/1024}')

      if [ "$rw" = "read" ]; then
        printf '  %-5s %-5d %-12s %-12s %-8s  %s\n' \
          "read"  "$jobs" "$single_mib" "$pnfs_mib" "$ratio" ""
      else
        printf '  %-5s %-5d %-12s %-12s %-8s  fs=%d\n' \
          "write" "$jobs" "$single_mib" "$pnfs_mib" "$ratio" "$fsync"
      fi
    done
  done
done

echo
echo "TSV: $SUMMARY_TSV"
echo "Per-run JSON: $LOG_DIR/flint-pnfs-sweep-*.json"
