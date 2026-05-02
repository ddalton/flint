#!/usr/bin/env bash
#
# Run the pynfs `pnfs` test set against the running flint MDS.
#
# Pynfs has 8 pNFS-specific tests (GETLAYOUT1, GETDINFO1, GETDLIST1,
# LAYOUTRET1/2/3, LAYOUTCOMMIT1, plus CSID7) plus 17 flex-files
# (FFLO*) tests. They run as a normal NFSv4.1 client against the MDS
# endpoint — no special pynfs setup needed beyond what the standalone
# NFS suite uses.
#
# Brings up MDS + 2 DSes (same configs as smoke.sh), mounts no NFS
# from the VM (pynfs talks RPC directly), runs the pnfs flag set, and
# saves JSON results to /tmp/flint-pnfs-pynfs-results.json.
#
# Exit status: 0 if pynfs ran (pass/fail breakdown is in the JSON).

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN_DIR="$REPO_ROOT/spdk-csi-driver/target/release"
CFG_DIR="$REPO_ROOT/tests/lima/pnfs"
PIDFILE_DIR="/tmp"
LOG_DIR="/tmp"

LIMA_VM="${LIMA_VM:-flint-nfs-client}"
HOST_ADDR="host.lima.internal"
MDS_PORT=20490

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
}
trap cleanup EXIT

echo "▶ pNFS conformance test (pynfs)"

# Pre-flight (same as smoke.sh).
for bin in flint-pnfs-mds flint-pnfs-ds; do
  if [ ! -x "$BIN_DIR/$bin" ]; then
    echo "✗ Missing binary: $BIN_DIR/$bin"; exit 1
  fi
done
if ! command -v limactl >/dev/null 2>&1; then
  echo "✗ limactl not found"; exit 1
fi

# Reset export trees so the test directory rebuild starts from clean.
rm -rf "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"
mkdir -p "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR/tmp"
chmod -R 0777 "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"

# Start MDS + 2 DSes.
nohup "$BIN_DIR/flint-pnfs-mds" --config "$CFG_DIR/mds.yaml" \
  >"$LOG_DIR/flint-pnfs-mds.log" 2>&1 &
echo $! > "$PIDFILE_DIR/flint-pnfs-mds.pid"
sleep 1
for n in 1 2; do
  cfg=$CFG_DIR/ds${n}.yaml
  nohup "$BIN_DIR/flint-pnfs-ds" --config "$cfg" \
    >"$LOG_DIR/flint-pnfs-ds${n}.log" 2>&1 &
  echo $! > "$PIDFILE_DIR/flint-pnfs-ds${n}.pid"
done
sleep 2

for n in mds ds1 ds2; do
  if ! kill -0 "$(cat "$PIDFILE_DIR/flint-pnfs-$n.pid")" 2>/dev/null; then
    echo "✗ $n died on startup. Last 30 log lines of /tmp/flint-pnfs-$n.log:"
    tail -30 "$LOG_DIR/flint-pnfs-$n.log"; exit 1
  fi
done
echo "✓ MDS + 2 DSes are up"

# Run pynfs `pnfs` flag set against the MDS.
# The /tmp path on the export root is the test tree --maketree builds.
limactl shell "$LIMA_VM" -- bash -lc "
  cd /opt/pynfs/nfs4.1 && \
  timeout 600 python3 ./testserver.py \
    ${HOST_ADDR}:${MDS_PORT}/tmp \
    --maketree --nocleanup \
    --json=/tmp/flint-pnfs-pynfs.json \
    pnfs 2>&1 | tail -20
"

# Pull the JSON back to the host and report the headline.
limactl cp "$LIMA_VM:/tmp/flint-pnfs-pynfs.json" /tmp/flint-pnfs-pynfs-results.json 2>&1 | tail -1

if [ -f /tmp/flint-pnfs-pynfs-results.json ]; then
  python3 - <<'PY'
import json, sys
r = json.load(open('/tmp/flint-pnfs-pynfs-results.json'))
total = r.get('tests', 0)
fail  = r.get('failures', 0)
skip  = r.get('skipped', 0)
print()
print(f"pNFS pynfs results: PASS={total-fail-skip}  FAIL={fail}  SKIP={skip}  TOTAL={total}")
print("(JSON saved to /tmp/flint-pnfs-pynfs-results.json)")
PY
fi
