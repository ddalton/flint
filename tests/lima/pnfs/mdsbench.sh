#!/usr/bin/env bash
#
# MDS metadata-performance bench (docs/plans/mds-performance-plan.md).
#
# Measures the MDS's metadata-op throughput and CPU cost under the
# workloads where the MDS — not the data path — is the bottleneck.
# The data path scales with DSes (ADR 0004: 6x at N=4, MDS 0% CPU);
# the per-open protocol cost does not. This harness exists so every
# optimization tier in the plan lands with a before/after number.
#
# Workloads (kernel client in the lima VM, P procs each):
#   W1 create   — create + 4KiB write + close + unlink, N_W1 files/proc
#   W2 opencl   — open existing + read 4KiB + close, N_W2 cycles/proc
#                 (isolates per-open layout churn: LAYOUTGET/RETURN/
#                  GETDEVICEINFO per cycle while return_on_close=true)
#   W3 stat     — stat() over the pool, N_W3 calls/proc (pure
#                 LOOKUP/GETATTR dispatch cost)
#   W4 mixed    — fsstress -n N_W4 -p P (cross-check vs fsx drill)
#
# Reported per workload:
#   ops/s        client-visible throughput (wall clock)
#   cpu_ms/op    MDS process CPU consumed per op (host `ps -o cputime`)
#   log_KiB/op   MDS log bytes written per op (logging overhead proxy)
#
# Env knobs (all optional):
#   P=8 N_W1=250 N_W2=500 N_W3=2000 N_W4=500
#   MDS_ENV="RUST_LOG=warn FLINT_NFS_MAX_INFLIGHT=64"   # A/B variants
#   LABEL=baseline                                      # result tag
#
# Results append to /tmp/mdsbench-results.tsv (one row per workload)
# so runs diff cleanly across variants.
#
# Exit status: 0 on completed run (numbers are the deliverable).

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN_DIR="$REPO_ROOT/spdk-csi-driver/target/release"
CFG_DIR="$REPO_ROOT/tests/lima/pnfs"
LOG_DIR=/tmp

LIMA_VM="${LIMA_VM:-flint-nfs-client}"
HOST_ADDR="host.lima.internal"
MDS_PORT=20490
MNT=/mnt/flint-pnfs

P="${P:-8}"
N_W1="${N_W1:-250}"
N_W2="${N_W2:-500}"
N_W3="${N_W3:-2000}"
N_W4="${N_W4:-500}"
MDS_ENV="${MDS_ENV:-}"
LABEL="${LABEL:-baseline}"
RESULTS=/tmp/mdsbench-results.tsv

DS1_EXPORT="/tmp/flint-pnfs-ds1"
DS2_EXPORT="/tmp/flint-pnfs-ds2"
MDS_EXPORT_DIR="/tmp/flint-pnfs-mds-exports"

cleanup() {
  set +e
  limactl shell "$LIMA_VM" -- sudo bash -c "pkill -9 -f mdsbench-worker; umount -lf $MNT" 2>/dev/null
  pkill -9 -f "flint-pnfs-mds" 2>/dev/null || true
  pkill -9 -f "flint-pnfs-ds"  2>/dev/null || true
}
trap cleanup EXIT

for bin in flint-pnfs-mds flint-pnfs-ds; do
  [ -x "$BIN_DIR/$bin" ] || { echo "✗ Missing binary: $BIN_DIR/$bin"; exit 1; }
done
limactl list --quiet 2>/dev/null | grep -qx "$LIMA_VM" \
  || { echo "✗ Lima VM '$LIMA_VM' not found"; exit 1; }

rm -rf "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"
mkdir -p "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"
chmod 0777 "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"

echo "▶ mdsbench [$LABEL]  P=$P  MDS_ENV='$MDS_ENV'"
echo "▶ starting MDS + 2 DSes"
env $MDS_ENV PNFS_MODE=mds nohup "$BIN_DIR/flint-pnfs-mds" --config "$CFG_DIR/mds.yaml" \
  >"$LOG_DIR/flint-pnfs-mds.log" 2>&1 &
MDS_PID=$!
sleep 1
for n in 1 2; do
  PNFS_MODE=ds nohup "$BIN_DIR/flint-pnfs-ds" --config "$CFG_DIR/ds$n.yaml" \
    >"$LOG_DIR/flint-pnfs-ds$n.log" 2>&1 &
done
sleep 3
kill -0 "$MDS_PID" || { echo "✗ MDS died on startup"; tail -20 $LOG_DIR/flint-pnfs-mds.log; exit 1; }

limactl shell "$LIMA_VM" -- sudo bash -c "
  umount -lf $MNT 2>/dev/null; rm -rf $MNT
  mkdir -p $MNT
  mount -t nfs4 -o minorversion=1,proto=tcp,port=${MDS_PORT} ${HOST_ADDR}:/ $MNT
" || { echo "✗ mount failed"; exit 1; }

# Push the worker (tight loops in python — client-side fork overhead
# would otherwise mask server-side gains).
limactl shell "$LIMA_VM" -- sudo tee /tmp/mdsbench-worker.py >/dev/null <<'PYEOF'
import os, sys, time
mode, root, procid, n = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
d = f"{root}/w{procid}"
buf = b"x" * 4096
if mode == "create":
    os.makedirs(d, exist_ok=True)
    for i in range(n):
        fd = os.open(f"{d}/f{i}", os.O_CREAT | os.O_WRONLY, 0o644)
        os.write(fd, buf); os.close(fd)
    for i in range(n):
        os.unlink(f"{d}/f{i}")
elif mode == "pool":
    os.makedirs(d, exist_ok=True)
    for i in range(n):
        fd = os.open(f"{d}/p{i}", os.O_CREAT | os.O_WRONLY, 0o644)
        os.write(fd, buf); os.close(fd)
elif mode == "opencl":
    pool = 50
    for i in range(n):
        fd = os.open(f"{d}/p{i % pool}", os.O_RDONLY)
        os.read(fd, 4096); os.close(fd)
elif mode == "stat":
    pool = 50
    for i in range(n):
        os.stat(f"{d}/p{i % pool}")
PYEOF

mds_cpu_s() {  # cumulative CPU seconds of the MDS process
  ps -o cputime= -p "$MDS_PID" | awk -F'[:.]' '{ if (NF>=3) print $1*60+$2"."$3; else print $1*60+$2 }' | head -1
}
log_bytes() { stat -f %z "$LOG_DIR/flint-pnfs-mds.log"; }

run_workload() {  # name mode per_proc_ops total_ops
  local name=$1 mode=$2 nops=$3 total=$4
  local c0 c1 b0 b1 t0 t1
  c0=$(mds_cpu_s); b0=$(log_bytes); t0=$(date +%s.%N 2>/dev/null || date +%s)
  limactl shell "$LIMA_VM" -- sudo bash -c "
    for p in \$(seq 1 $P); do
      python3 /tmp/mdsbench-worker.py $mode $MNT/bench \$p $nops &
    done
    wait
  " || { echo "  ✗ $name workload failed"; return 1; }
  t1=$(date +%s.%N 2>/dev/null || date +%s)
  c1=$(mds_cpu_s); b1=$(log_bytes)
  python3 - "$LABEL" "$name" "$total" "$t0" "$t1" "$c0" "$c1" "$b0" "$b1" "$RESULTS" <<'PYEOF'
import sys
label, name, total, t0, t1, c0, c1, b0, b1, out = sys.argv[1:]
total, wall, cpu, logb = int(total), float(t1)-float(t0), float(c1)-float(c0), int(b1)-int(b0)
ops = total / wall if wall > 0 else 0
row = f"{label}\t{name}\t{total}\t{wall:.1f}\t{ops:.0f}\t{cpu*1000/total:.2f}\t{logb/1024/total:.2f}"
print(f"  {name:8s} {total} ops in {wall:.1f}s = {ops:.0f} ops/s   cpu {cpu*1000/total:.2f} ms/op   log {logb/1024/total:.2f} KiB/op")
open(out, "a").write(row + "\n")
PYEOF
}

echo
run_workload w1-create create "$N_W1" $((N_W1 * P * 2))   # create+unlink both count
limactl shell "$LIMA_VM" -- sudo bash -c "
  for p in \$(seq 1 $P); do python3 /tmp/mdsbench-worker.py pool $MNT/bench \$p 50 & done; wait"
run_workload w2-opencl opencl "$N_W2" $((N_W2 * P))
run_workload w3-stat   stat   "$N_W3" $((N_W3 * P))

# W4: fsstress (only if built in the VM)
if limactl shell "$LIMA_VM" -- test -x /opt/xfstests/ltp/fsstress; then
  c0=$(mds_cpu_s); b0=$(log_bytes); t0=$(date +%s)
  limactl shell "$LIMA_VM" -- sudo bash -c \
    "mkdir -p $MNT/bench/stress && timeout 1800 /opt/xfstests/ltp/fsstress -d $MNT/bench/stress -n $N_W4 -p $P -s 42 >/dev/null 2>&1"
  t1=$(date +%s); c1=$(mds_cpu_s); b1=$(log_bytes)
  total=$((N_W4 * P))
  python3 - "$LABEL" w4-mixed "$total" "$t0" "$t1" "$c0" "$c1" "$b0" "$b1" "$RESULTS" <<'PYEOF'
import sys
label, name, total, t0, t1, c0, c1, b0, b1, out = sys.argv[1:]
total, wall, cpu, logb = int(total), float(t1)-float(t0), float(c1)-float(c0), int(b1)-int(b0)
ops = total / wall if wall > 0 else 0
row = f"{label}\t{name}\t{total}\t{wall:.1f}\t{ops:.0f}\t{cpu*1000/total:.2f}\t{logb/1024/total:.2f}"
print(f"  {name:8s} {total} ops in {wall:.1f}s = {ops:.0f} ops/s   cpu {cpu*1000/total:.2f} ms/op   log {logb/1024/total:.2f} KiB/op")
open(out, "a").write(row + "\n")
PYEOF
fi

echo
echo "✅ mdsbench [$LABEL] complete — results appended to $RESULTS"
column -t "$RESULTS" 2>/dev/null | tail -8
