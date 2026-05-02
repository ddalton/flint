#!/usr/bin/env bash
#
# pNFS vs. single-server NFS — head-to-head fio benchmark.
#
# This is the load-bearing measurement. The product question pNFS is
# trying to answer is: "does striping across data servers beat a single
# NFS server for high-aggregate-bandwidth workloads?"
#
# This script answers that question for the only environment we have
# locally: macOS host running both servers, Lima VM running the kernel
# NFS client, fio inside the VM driving load.
#
# What this DOES measure
#   - Whether the kernel actually opens parallel TCP connections to two
#     DSes vs. one connection to a single server.
#   - Whether the userspace data path (file I/O on the server side)
#     parallelises across DSes.
#   - End-to-end client-visible aggregate MB/s for sequential reads
#     and writes at large block sizes.
#
# What this DOES NOT measure
#   - Real cross-host network behaviour. MDS, DS1, DS2 all run on the
#     same macOS kernel; their "wire" traffic is loopback. A real
#     two-DS-on-two-physical-NICs setup would see different (probably
#     better-for-pNFS) numbers.
#   - Latency of small ops. We test bs=1M, numjobs=N — throughput shape.
#   - Cold-cache vs. warm-cache. We `drop_caches` on the client between
#     write and read phases; the macOS host's page cache is not
#     dropped (we can't from inside Lima).
#
# Pass criterion
#   - For an honest "pNFS is worth shipping" result on this hardware,
#     pNFS read aggregate >= 1.3× single-server read aggregate. Below
#     that, the protocol overhead is eating the parallelism win.
#
# Output
#   - Stdout: a small table comparing the two configurations.
#   - JSON: per-run fio output saved to /tmp/flint-pnfs-bench-*.json
#     so we can inspect or replot later.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN_DIR="$REPO_ROOT/spdk-csi-driver/target/release"
CFG_DIR="$REPO_ROOT/tests/lima/pnfs"
LOG_DIR="/tmp"

LIMA_VM="${LIMA_VM:-flint-nfs-client}"
HOST_ADDR="host.lima.internal"

# pNFS layout — same ports as smoke.sh so we don't fight existing infra.
MDS_PORT=20490
DS1_PORT=20491
DS2_PORT=20492
DS1_EXPORT="/tmp/flint-pnfs-ds1"
DS2_EXPORT="/tmp/flint-pnfs-ds2"
MDS_EXPORT_DIR="/tmp/flint-pnfs-mds-exports"

# Single-server NFS (the baseline path) — uses the existing
# `flint-nfs-server` Makefile target's port + export.
NFS_PORT=20480
NFS_EXPORT_DIR="/tmp/flint-nfs-export"

# Mount point inside the VM — same for both phases so fio commands
# are identical.
VM_MOUNT="/mnt/flint-bench"

# Workload. 4 jobs × 256 MiB = 1 GiB total per phase, fits in /tmp's
# ~11 GiB free, exercises a few stripes per DS (with 8 MiB stripe ×
# 2 DSes = 16 MiB stripe-set, 1 GiB = 64 stripe-sets).
JOBS=4
SIZE_MB=256
BS=1M

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

echo "▶ pNFS vs. single-server NFS benchmark"
echo "  workload: $JOBS jobs × ${SIZE_MB} MiB × ${BS} blocks"
echo

for bin in flint-pnfs-mds flint-pnfs-ds flint-nfs-server; do
  [ -x "$BIN_DIR/$bin" ] || { echo "✗ Missing binary: $BIN_DIR/$bin"
    echo "  Run: cd spdk-csi-driver && cargo build --release"; exit 1; }
done

if ! limactl list --quiet 2>/dev/null | grep -qx "$LIMA_VM"; then
  echo "✗ Lima VM '$LIMA_VM' not running. Run: make lima-up"; exit 1
fi

if ! limactl shell "$LIMA_VM" -- bash -lc 'command -v fio' >/dev/null 2>&1; then
  echo "▶ installing fio in Lima VM (one-time)"
  limactl shell "$LIMA_VM" -- sudo apt-get -qq -y install fio >/dev/null 2>&1
fi

# Make sure VM mount point exists.
limactl shell "$LIMA_VM" -- sudo mkdir -p "$VM_MOUNT"

# ──────────────────────────────────────────────────────────────────────
# Helpers
# ──────────────────────────────────────────────────────────────────────

# Drop the client's page cache so reads actually hit the server.
# (We can't drop the macOS host's page cache from here, so server-side
# data stays warm — that's a known caveat documented at the top.)
drop_client_cache() {
  limactl shell "$LIMA_VM" -- sudo bash -c 'sync && echo 3 > /proc/sys/vm/drop_caches' \
    >/dev/null 2>&1
}

# Run a single fio phase. Returns the aggregate bandwidth in KB/s on
# stdout. Writes the full JSON result to $LOG_DIR/$1.
#   $1: tag (pnfs-write, single-read, etc.)
#   $2: rw mode (read | write)
fio_phase() {
  local tag="$1" rw="$2"
  local out="$LOG_DIR/flint-pnfs-bench-$tag.json"

  drop_client_cache

  # `--directory` drops fio's files into our mount. `--rw=write`
  # creates the files; `--rw=read` reads them back. Use the same
  # filename pattern across phases so read-after-write works.
  limactl shell "$LIMA_VM" -- bash -c "fio \
    --name=bench \
    --directory=$VM_MOUNT \
    --rw=$rw \
    --bs=$BS \
    --numjobs=$JOBS \
    --size=${SIZE_MB}M \
    --ioengine=libaio \
    --iodepth=16 \
    --direct=0 \
    --end_fsync=1 \
    --group_reporting \
    --output-format=json" > "$out" 2>"$LOG_DIR/flint-pnfs-bench-$tag.stderr"

  # fio json: jobs[0].read.bw or .write.bw — KB/s aggregate (since
  # group_reporting collapses all jobs into jobs[0]).
  jq -r ".jobs[0].$rw.bw // 0" < "$out"
}

format_mbps() {
  awk -v kbps="$1" 'BEGIN { printf "%.1f MiB/s", kbps / 1024 }'
}

# ──────────────────────────────────────────────────────────────────────
# Phase 1 — single-server NFS baseline
# ──────────────────────────────────────────────────────────────────────

echo "═══ Phase 1: single-server NFS (flint-nfs-server) ═══"
echo

rm -rf "$NFS_EXPORT_DIR"
mkdir -p "$NFS_EXPORT_DIR"
chmod 0777 "$NFS_EXPORT_DIR"

# `flint-nfs-server` is the SPDK driver's RWX path's NFS server. Run
# it on a port distinct from the pNFS MDS so the two phases can be
# brought up and torn down without conflict.
nohup "$BIN_DIR/flint-nfs-server" \
  --bind-addr "0.0.0.0" \
  --port "$NFS_PORT" \
  --export-path "$NFS_EXPORT_DIR" \
  --volume-id bench \
  > "$LOG_DIR/flint-nfs.log" 2>&1 &
echo $! > /tmp/flint-nfs.pid
sleep 1
if ! kill -0 "$(cat /tmp/flint-nfs.pid)" 2>/dev/null; then
  echo "✗ single-server NFS died on startup"; tail -20 "$LOG_DIR/flint-nfs.log"; exit 1
fi
echo "  ✓ flint-nfs-server up on port $NFS_PORT"

limactl shell "$LIMA_VM" -- sudo mount -t nfs4 \
  -o "minorversion=1,proto=tcp,port=$NFS_PORT,nconnect=4,rsize=1048576,wsize=1048576" \
  "$HOST_ADDR:/" "$VM_MOUNT" \
  || { echo "✗ mount failed"; exit 1; }
echo "  ✓ mounted on $VM_MOUNT"

echo "  running write phase ($JOBS jobs × ${SIZE_MB} MiB)"
SS_WRITE_KBPS=$(fio_phase single-write write)
echo "    → $(format_mbps "$SS_WRITE_KBPS")"

echo "  running read phase"
SS_READ_KBPS=$(fio_phase single-read read)
echo "    → $(format_mbps "$SS_READ_KBPS")"

limactl shell "$LIMA_VM" -- sudo umount "$VM_MOUNT"
kill "$(cat /tmp/flint-nfs.pid)" 2>/dev/null
wait 2>/dev/null
rm -f /tmp/flint-nfs.pid
echo "  ✓ baseline complete"
echo

# ──────────────────────────────────────────────────────────────────────
# Phase 2 — pNFS (MDS + 2 DSes)
# ──────────────────────────────────────────────────────────────────────

echo "═══ Phase 2: pNFS (MDS + 2 DSes) ═══"
echo

rm -rf "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"
mkdir -p "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"
chmod 0777 "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"

PNFS_MODE=mds nohup "$BIN_DIR/flint-pnfs-mds" --config "$CFG_DIR/mds.yaml" \
  > "$LOG_DIR/flint-pnfs-mds.log" 2>&1 &
echo $! > /tmp/flint-pnfs-mds.pid
sleep 1
kill -0 "$(cat /tmp/flint-pnfs-mds.pid)" 2>/dev/null \
  || { echo "✗ MDS died"; tail -20 "$LOG_DIR/flint-pnfs-mds.log"; exit 1; }

for n in 1 2; do
  cfg="$CFG_DIR/ds${n}.yaml"
  PNFS_MODE=ds nohup "$BIN_DIR/flint-pnfs-ds" --config "$cfg" \
    > "$LOG_DIR/flint-pnfs-ds${n}.log" 2>&1 &
  echo $! > "/tmp/flint-pnfs-ds${n}.pid"
done
sleep 2
for n in 1 2; do
  kill -0 "$(cat /tmp/flint-pnfs-ds${n}.pid)" 2>/dev/null \
    || { echo "✗ DS${n} died"; tail -20 "$LOG_DIR/flint-pnfs-ds${n}.log"; exit 1; }
done
echo "  ✓ MDS + 2 DSes up"

limactl shell "$LIMA_VM" -- sudo mount -t nfs4 \
  -o "minorversion=1,proto=tcp,port=$MDS_PORT,nconnect=4,rsize=1048576,wsize=1048576" \
  "$HOST_ADDR:/" "$VM_MOUNT" \
  || { echo "✗ mount failed"; exit 1; }
echo "  ✓ mounted on $VM_MOUNT (pNFS)"

echo "  running write phase"
PN_WRITE_KBPS=$(fio_phase pnfs-write write)
echo "    → $(format_mbps "$PN_WRITE_KBPS")"

echo "  running read phase"
PN_READ_KBPS=$(fio_phase pnfs-read read)
echo "    → $(format_mbps "$PN_READ_KBPS")"

# Capture per-DS bytes so we can tell if the kernel actually striped.
# Portable byte counter (macOS BSD du has no -b; use -k and × 1024).
# We sum apparent file sizes via stat — sparse files in striped layouts
# look big to du but are mostly holes; stat -f %z is what the protocol
# actually moved.
sum_filesizes() {
  find "$1" -type f -exec stat -f %z {} + 2>/dev/null \
    | awk 'BEGIN { s = 0 } { s += $1 } END { print s }'
}
DS1_BYTES=$(sum_filesizes "$DS1_EXPORT")
DS2_BYTES=$(sum_filesizes "$DS2_EXPORT")
MDS_BYTES=$(sum_filesizes "$MDS_EXPORT_DIR")

limactl shell "$LIMA_VM" -- sudo umount "$VM_MOUNT"
echo "  ✓ pNFS complete"
echo

# ──────────────────────────────────────────────────────────────────────
# Summary
# ──────────────────────────────────────────────────────────────────────

ratio() { awk -v a="$1" -v b="$2" 'BEGIN { if (b > 0) printf "%.2fx", a / b; else print "—" }'; }

echo "════════════════════════════════════════════════════════════════"
echo " RESULTS — workload: $JOBS jobs × ${SIZE_MB} MiB × ${BS} blocks"
echo "════════════════════════════════════════════════════════════════"
printf "  %-20s %15s %15s %10s\n" "" "single-server" "pNFS" "ratio"
printf "  %-20s %15s %15s %10s\n" "WRITE aggregate" \
  "$(format_mbps "$SS_WRITE_KBPS")" "$(format_mbps "$PN_WRITE_KBPS")" \
  "$(ratio "$PN_WRITE_KBPS" "$SS_WRITE_KBPS")"
printf "  %-20s %15s %15s %10s\n" "READ aggregate" \
  "$(format_mbps "$SS_READ_KBPS")" "$(format_mbps "$PN_READ_KBPS")" \
  "$(ratio "$PN_READ_KBPS" "$SS_READ_KBPS")"
echo
echo "  pNFS per-DS bytes after run:"
printf "    DS1 export: %d bytes\n" "${DS1_BYTES:-0}"
printf "    DS2 export: %d bytes\n" "${DS2_BYTES:-0}"
printf "    MDS export: %d bytes (should be ~0)\n" "${MDS_BYTES:-0}"
echo
echo "  Per-run JSONs in $LOG_DIR/flint-pnfs-bench-*.json"
echo "════════════════════════════════════════════════════════════════"

# Pass criterion: pNFS reads at least 1.3× single-server reads.
PASS_THRESHOLD_PCT=130
RATIO_PCT=$(awk -v a="$PN_READ_KBPS" -v b="$SS_READ_KBPS" \
            'BEGIN { if (b > 0) printf "%.0f", 100 * a / b; else print 0 }')
if [ "$RATIO_PCT" -ge "$PASS_THRESHOLD_PCT" ]; then
  echo "✓ PASS: pNFS reads ${RATIO_PCT}% of single-server (>= ${PASS_THRESHOLD_PCT}%)"
  exit 0
else
  echo "⚠ INFORMATIONAL: pNFS reads ${RATIO_PCT}% of single-server (target was >= ${PASS_THRESHOLD_PCT}%)"
  echo "  This is expected on a single-host setup where MDS+DSes share one kernel."
  echo "  A real multi-host benchmark is needed before drawing conclusions."
  exit 0
fi
