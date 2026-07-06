#!/usr/bin/env bash
#
# pNFS placement (fleet-growth) drill — Phase 0 of
# docs/plans/pnfs-durable-ds-plan.md.
#
# Proves the per-file placement pin end-to-end against the kernel
# client:
#
#   1. Write file A while the fleet is {DS1, DS2}.
#   2. Grow the fleet: start DS3 (it registers with the MDS).
#   3. Re-mount (fresh layouts) and read A back — the checksum must
#      match and NO bytes of A may appear on DS3: A's stripe map is
#      pinned to {DS1, DS2}.
#   4. Write file B — it must stripe across all three DSes.
#
# Before Phase 0, step 3 read garbage: the stripe map was recomputed
# from the live device list, so growing the fleet re-mapped A's
# stripes.
#
# The MDS pre-registers every configured DS at boot, so the drill
# waits out mds-growth.yaml's heartbeatTimeout (25s) for the
# not-yet-started DS3 to go stale before the first mount.
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

DS1_EXPORT="/tmp/flint-pnfs-ds1"
DS2_EXPORT="/tmp/flint-pnfs-ds2"
DS3_EXPORT="/tmp/flint-pnfs-ds3"
MDS_EXPORT_DIR="/tmp/flint-pnfs-mds-exports"

cleanup() {
  set +e
  for n in mds ds1 ds2 ds3; do
    if [ -f "$PIDFILE_DIR/flint-pnfs-$n.pid" ]; then
      kill "$(cat "$PIDFILE_DIR/flint-pnfs-$n.pid")" 2>/dev/null
      rm -f "$PIDFILE_DIR/flint-pnfs-$n.pid"
    fi
  done
  pkill -9 -f "flint-pnfs-mds" 2>/dev/null || true
  pkill -9 -f "flint-pnfs-ds"  2>/dev/null || true
  limactl shell "$LIMA_VM" -- sudo umount -lf /mnt/flint-pnfs 2>/dev/null || true
}
trap cleanup EXIT

ds_bytes() { du -sk "$1" 2>/dev/null | awk '{print $1*1024}'; }

start_ds() {
  local n="$1" cfg="$CFG_DIR/ds$1.yaml"
  echo "▶ starting DS $n"
  PNFS_MODE=ds nohup "$BIN_DIR/flint-pnfs-ds" --config "$cfg" \
    >"$LOG_DIR/flint-pnfs-ds${n}.log" 2>&1 &
  echo $! > "$PIDFILE_DIR/flint-pnfs-ds${n}.pid"
  sleep 1
  if ! kill -0 "$(cat "$PIDFILE_DIR/flint-pnfs-ds${n}.pid")" 2>/dev/null; then
    echo "✗ DS $n died on startup. Last 30 log lines:"
    tail -30 "$LOG_DIR/flint-pnfs-ds${n}.log"
    exit 1
  fi
}

echo "▶ pNFS placement (fleet-growth) drill"
echo

# ── 0. Pre-flight ─────────────────────────────────────────────────────
for bin in flint-pnfs-mds flint-pnfs-ds; do
  [ -x "$BIN_DIR/$bin" ] || { echo "✗ Missing binary: $BIN_DIR/$bin"; exit 1; }
done
limactl list --quiet 2>/dev/null | grep -qx "$LIMA_VM" \
  || { echo "✗ Lima VM '$LIMA_VM' not found. Run: make lima-up"; exit 1; }

rm -rf "$DS1_EXPORT" "$DS2_EXPORT" "$DS3_EXPORT" "$MDS_EXPORT_DIR"
mkdir -p "$DS1_EXPORT" "$DS2_EXPORT" "$DS3_EXPORT" "$MDS_EXPORT_DIR"
chmod 0777 "$DS1_EXPORT" "$DS2_EXPORT" "$DS3_EXPORT" "$MDS_EXPORT_DIR"

# ── 1. MDS + DS1 + DS2 only ──────────────────────────────────────────
echo "▶ starting MDS (growth config: 3 DSes configured, DS3 absent)"
PNFS_MODE=mds nohup "$BIN_DIR/flint-pnfs-mds" --config "$CFG_DIR/mds-growth.yaml" \
  >"$LOG_DIR/flint-pnfs-mds.log" 2>&1 &
echo $! > "$PIDFILE_DIR/flint-pnfs-mds.pid"
sleep 1
kill -0 "$(cat "$PIDFILE_DIR/flint-pnfs-mds.pid")" 2>/dev/null \
  || { echo "✗ MDS died on startup:"; tail -30 "$LOG_DIR/flint-pnfs-mds.log"; exit 1; }

start_ds 1
start_ds 2

echo "▶ waiting out heartbeatTimeout so the absent DS3 goes stale (30s)"
sleep 30

# ── 2. Phase 1: write A with fleet {DS1, DS2} ────────────────────────
echo "▶ phase 1: mount, write A (24 MiB), umount"
HASH_A=$(limactl shell "$LIMA_VM" -- sudo bash -c "
  set -eu
  mountpoint -q /mnt/flint-pnfs && umount -lf /mnt/flint-pnfs || true
  mkdir -p /mnt/flint-pnfs
  mount -t nfs4 -o minorversion=1,proto=tcp,port=${MDS_PORT} ${HOST_ADDR}:/ /mnt/flint-pnfs
  dd if=/dev/urandom of=/mnt/flint-pnfs/striped-A.bin bs=1M count=24 status=none oflag=direct
  sync
  sha256sum /mnt/flint-pnfs/striped-A.bin | cut -d' ' -f1
  umount /mnt/flint-pnfs
" | tail -2 | head -1) || { echo "✗ phase 1 failed"; exit 1; }
echo "  A sha256: $HASH_A"

ds3_after_a=$(ds_bytes "$DS3_EXPORT")
if [ "${ds3_after_a:-0}" -ne 0 ]; then
  echo "✗ FAIL: DS3 has ${ds3_after_a} bytes before it was ever started?!"
  exit 1
fi

# ── 3. Grow the fleet ────────────────────────────────────────────────
start_ds 3
echo "▶ waiting for DS3 registration"
sleep 5

# ── 4. Phase 2: re-mount, read A back, write B ───────────────────────
echo "▶ phase 2: re-mount, verify A, write B (24 MiB)"
HASH_A2=$(limactl shell "$LIMA_VM" -- sudo bash -c "
  set -eu
  mkdir -p /mnt/flint-pnfs
  mount -t nfs4 -o minorversion=1,proto=tcp,port=${MDS_PORT} ${HOST_ADDR}:/ /mnt/flint-pnfs
  sha256sum /mnt/flint-pnfs/striped-A.bin | cut -d' ' -f1
" | tail -1) || { echo "✗ phase 2 mount/read failed"; exit 1; }
echo "  A sha256 after fleet growth: $HASH_A2"

ds3_after_read=$(ds_bytes "$DS3_EXPORT")

HASH_B=$(limactl shell "$LIMA_VM" -- sudo bash -c "
  set -eu
  dd if=/dev/urandom of=/mnt/flint-pnfs/striped-B.bin bs=1M count=24 status=none oflag=direct
  sync
  sha256sum /mnt/flint-pnfs/striped-B.bin | cut -d' ' -f1
  umount /mnt/flint-pnfs
" | tail -2 | head -1) || { echo "✗ phase 2 write failed"; exit 1; }
echo "  B sha256: $HASH_B"

ds3_after_b=$(ds_bytes "$DS3_EXPORT")

# ── 5. Verdict ───────────────────────────────────────────────────────
echo
echo "▶ Per-DS bytes-on-disk:"
echo "  DS1: $(ds_bytes "$DS1_EXPORT") bytes"
echo "  DS2: $(ds_bytes "$DS2_EXPORT") bytes"
echo "  DS3: ${ds3_after_b:-0} bytes (after A-read: ${ds3_after_read:-0})"
echo "  MDS: $(ds_bytes "$MDS_EXPORT_DIR") bytes"
echo
echo "▶ MDS placement pins:"
grep -o "Pinned placement for '[^']*': [0-9]* DSes" "$LOG_DIR/flint-pnfs-mds.log" | sort -u | sed 's/^/  /'
echo

FAIL=""
[ "$HASH_A" = "$HASH_A2" ] \
  || FAIL="$FAIL\n  - A's content changed after fleet growth (stripe re-map — the Phase 0 P1)"
[ "${ds3_after_read:-0}" -eq 0 ] \
  || FAIL="$FAIL\n  - reading A touched DS3, which was never in A's placement"
[ "${ds3_after_b:-0}" -gt 0 ] \
  || FAIL="$FAIL\n  - B did not stripe onto DS3 — new files must see the grown fleet"

if [ -n "$FAIL" ]; then
  echo -e "✗ FAIL:$FAIL"
  echo "  Logs: $LOG_DIR/flint-pnfs-{mds,ds1,ds2,ds3}.log"
  exit 1
fi
echo "✓ PASS: A pinned to {DS1,DS2} and intact after growth; B striped 3-wide"
exit 0
