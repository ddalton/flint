#!/usr/bin/env bash
#
# pNFS filesystem-semantics torture drill: fsx + fsstress from
# xfstests, run by the real kernel client against a striped mount.
#
# Why these exist in the gate: protocol suites (pynfs) validate ops in
# isolation; the P0-2 class of bug — stale stripes after rm/recreate,
# scrambled stripes after rename — only shows up when DATA integrity
# is checked ACROSS namespace operations. fsx is the canonical
# data-integrity fuzzer (random write/truncate/extend/read against a
# shadow copy); fsstress storms create/rename/link/unlink/rmdir with
# concurrent processes.
#
# Requires /opt/xfstests built in the VM (git.kernel.org xfstests-dev,
# `make`), done once by the quality-suite setup.
#
# Exit status: 0 on PASS, 1 on FAIL.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN_DIR="$REPO_ROOT/spdk-csi-driver/target/release"
CFG_DIR="$REPO_ROOT/tests/lima/pnfs"
PIDFILE_DIR="/tmp"
LOG_DIR="/tmp"

LIMA_VM="${LIMA_VM:-flint-nfs-client}"
HOST_ADDR="host.lima.internal"
MDS_PORT=20490
MNT=/mnt/flint-pnfs

FSX_OPS="${FSX_OPS:-20000}"
# 500 ops × 4 procs ≈ 7–8 min at the current ~230 ms/op (layout churn
# from return_on_close + per-open GETDEVICEINFO — P1 perf item). 1500
# needed ~23 min and tripped the 900 s timeout, which read as a hang.
STRESS_OPS="${STRESS_OPS:-500}"
STRESS_PROCS="${STRESS_PROCS:-4}"

DS1_EXPORT="/tmp/flint-pnfs-ds1"
DS2_EXPORT="/tmp/flint-pnfs-ds2"
MDS_EXPORT_DIR="/tmp/flint-pnfs-mds-exports"

cleanup() {
  set +e
  for n in mds ds1 ds2; do
    if [ -f "$PIDFILE_DIR/flint-pnfs-$n.pid" ]; then
      kill "$(cat "$PIDFILE_DIR/flint-pnfs-$n.pid")" 2>/dev/null
      rm -f "$PIDFILE_DIR/flint-pnfs-$n.pid"
    fi
  done
  pkill -9 -f "flint-pnfs-mds" 2>/dev/null || true
  pkill -9 -f "flint-pnfs-ds"  2>/dev/null || true
  limactl shell "$LIMA_VM" -- sudo umount -lf "$MNT" 2>/dev/null || true
}
trap cleanup EXIT

start_ds() {
  local n="$1" cfg="$CFG_DIR/ds$1.yaml"
  echo "▶ starting DS $n"
  PNFS_MODE=ds nohup "$BIN_DIR/flint-pnfs-ds" --config "$cfg" \
    >"$LOG_DIR/flint-pnfs-ds${n}.log" 2>&1 &
  echo $! > "$PIDFILE_DIR/flint-pnfs-ds${n}.pid"
  sleep 1
  kill -0 "$(cat "$PIDFILE_DIR/flint-pnfs-ds${n}.pid")" 2>/dev/null || {
    echo "✗ DS $n died on startup:"; tail -30 "$LOG_DIR/flint-pnfs-ds${n}.log"; exit 1;
  }
}

echo "▶ pNFS fsx + fsstress torture drill (fsx=$FSX_OPS ops, fsstress=${STRESS_PROCS}x${STRESS_OPS})"
echo

for bin in flint-pnfs-mds flint-pnfs-ds; do
  [ -x "$BIN_DIR/$bin" ] || { echo "✗ Missing binary: $BIN_DIR/$bin"; exit 1; }
done
limactl list --quiet 2>/dev/null | grep -qx "$LIMA_VM" \
  || { echo "✗ Lima VM '$LIMA_VM' not found. Run: make lima-up"; exit 1; }
limactl shell "$LIMA_VM" -- test -x /opt/xfstests/ltp/fsx \
  || { echo "✗ /opt/xfstests not built in the VM (see docs/pnfs-operator-runbook.md quality suites)"; exit 1; }

rm -rf "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"
mkdir -p "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"
chmod 0777 "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"

echo "▶ starting MDS + 2 DSes"
PNFS_MODE=mds nohup "$BIN_DIR/flint-pnfs-mds" --config "$CFG_DIR/mds.yaml" \
  >"$LOG_DIR/flint-pnfs-mds.log" 2>&1 &
echo $! > "$PIDFILE_DIR/flint-pnfs-mds.pid"
sleep 1
start_ds 1
start_ds 2
sleep 2

limactl shell "$LIMA_VM" -- sudo bash -c "
  mkdir -p $MNT
  mount -t nfs4 -o minorversion=1,proto=tcp,port=${MDS_PORT} ${HOST_ADDR}:/ $MNT
" || { echo "✗ mount failed"; exit 1; }

FAIL=""

# ── 1. fsx: buffered ─────────────────────────────────────────────────
echo "▶ fsx buffered ($FSX_OPS ops, seed 42)"
if limactl shell "$LIMA_VM" -- sudo timeout 900 \
     /opt/xfstests/ltp/fsx -N "$FSX_OPS" -S 42 "$MNT/fsx-buf.dat" \
     > /tmp/fsx-buf.out 2>&1; then
  echo "  ✓ fsx buffered clean"
else
  FAIL="$FAIL\n  - fsx (buffered) found a data-integrity failure or hung"
  tail -12 /tmp/fsx-buf.out | sed 's/^/  | /'
fi

# ── 2. fsx: O_DIRECT ─────────────────────────────────────────────────
echo "▶ fsx O_DIRECT ($FSX_OPS ops, seed 7)"
if limactl shell "$LIMA_VM" -- sudo timeout 900 \
     /opt/xfstests/ltp/fsx -N "$FSX_OPS" -S 7 -Z -r 4096 -w 4096 "$MNT/fsx-dio.dat" \
     > /tmp/fsx-dio.out 2>&1; then
  echo "  ✓ fsx O_DIRECT clean"
else
  FAIL="$FAIL\n  - fsx (O_DIRECT) found a data-integrity failure or hung"
  tail -12 /tmp/fsx-dio.out | sed 's/^/  | /'
fi

# ── 3. fsstress: concurrent namespace storm ──────────────────────────
echo "▶ fsstress (${STRESS_PROCS} procs × ${STRESS_OPS} ops, seed 42)"
if limactl shell "$LIMA_VM" -- sudo timeout 900 \
     /opt/xfstests/ltp/fsstress -d "$MNT/stress" -n "$STRESS_OPS" -p "$STRESS_PROCS" -S -s 42 \
     > /tmp/fsstress.out 2>&1; then
  echo "  ✓ fsstress clean"
else
  FAIL="$FAIL\n  - fsstress failed or hung"
  tail -12 /tmp/fsstress.out | sed 's/^/  | /'
fi

# ── 4. Fleet survived ────────────────────────────────────────────────
for n in mds ds1 ds2; do
  kill -0 "$(cat "$PIDFILE_DIR/flint-pnfs-$n.pid" 2>/dev/null)" 2>/dev/null \
    || FAIL="$FAIL\n  - $n died during the torture run"
done

echo
if [ -n "$FAIL" ]; then
  echo -e "✗ FAIL:$FAIL"
  echo "  Outputs: /tmp/fsx-buf.out /tmp/fsx-dio.out /tmp/fsstress.out"
  echo "  Logs: $LOG_DIR/flint-pnfs-{mds,ds1,ds2}.log"
  exit 1
fi
echo "✅ PASS: fsx (buffered + O_DIRECT) and fsstress clean; fleet survived"
