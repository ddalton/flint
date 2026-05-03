#!/usr/bin/env bash
#
# pNFS DS-death recall e2e — verifies the Phase A.4 wiring:
#
#   1. MDS + 2 DSes come up; a Linux NFSv4.1 client mounts and writes
#      enough to trigger LAYOUTGET (the layout pins one or more DSes
#      under the layout's owner session).
#   2. We `pkill -9` DS1.
#   3. The MDS heartbeat monitor sees DS1 stale within
#      heartbeatTimeout=5s + ~10s check-interval ≤ 30s.
#   4. The MDS log shows the full recall chain:
#        "stale data servers"            — DeviceRegistry detected death
#        "Recalling N layout(s)"         — LayoutManager produced pairs
#        "Fanning out N CB_LAYOUTRECALL" — CallbackManager fanned out
#        "CB_LAYOUTRECALL → session"     — actual CB CALL emitted
#      One of:
#        "CB_LAYOUTRECALL ← session"     — client acked
#        "fan-out: 0/N acked"            — client did not respond
#        "CB call timed out"             — client silently dropped it
#      Any of those last three is acceptable: A.4 is "MDS *initiates*
#      the recall." Whether the kernel client honors a recall on this
#      specific layout shape is a separate story (A.5 covers forced
#      revocation on timeout).
#
# This script's PASS bar is "the MDS recall chain executes through
# CB_LAYOUTRECALL emission." Client-side outcomes beyond that are
# observed and reported but not asserted, since they depend on
# kernel client behaviour outside our control.
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
DS1_PORT=20491
DS2_PORT=20492

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
  limactl shell "$LIMA_VM" -- sudo umount -lf /mnt/flint-pnfs 2>/dev/null || true
}
trap cleanup EXIT

echo "▶ pNFS DS-death recall test"
echo "  binaries:  $BIN_DIR"
echo "  configs:   $CFG_DIR"
echo

# ──────────────────────────────────────────────────────────────────────
# 0. Pre-flight
# ──────────────────────────────────────────────────────────────────────
for bin in flint-pnfs-mds flint-pnfs-ds; do
  if [ ! -x "$BIN_DIR/$bin" ]; then
    echo "✗ Missing binary: $BIN_DIR/$bin"
    echo "  Run: cd spdk-csi-driver && cargo build --release"
    exit 1
  fi
done
if ! command -v limactl >/dev/null 2>&1; then
  echo "✗ limactl not found. Install with: brew install lima"
  exit 1
fi
if ! limactl list --quiet 2>/dev/null | grep -qx "$LIMA_VM"; then
  echo "✗ Lima VM '$LIMA_VM' not running. Run: make lima-up"
  exit 1
fi

rm -rf "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"
mkdir -p "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"
chmod 0777 "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"

MDS_LOG="$LOG_DIR/flint-pnfs-mds.log"
: > "$MDS_LOG"

# ──────────────────────────────────────────────────────────────────────
# 1. Start MDS + 2 DSes (MDS uses the recall-tuned config)
# ──────────────────────────────────────────────────────────────────────
echo "▶ starting MDS (heartbeatTimeout=5s)"
PNFS_MODE=mds nohup "$BIN_DIR/flint-pnfs-mds" --config "$CFG_DIR/mds-recall.yaml" \
  >"$MDS_LOG" 2>&1 &
echo $! > "$PIDFILE_DIR/flint-pnfs-mds.pid"
sleep 1
if ! kill -0 "$(cat "$PIDFILE_DIR/flint-pnfs-mds.pid")" 2>/dev/null; then
  echo "✗ MDS died on startup. Last 30 log lines:"
  tail -30 "$MDS_LOG"
  exit 1
fi

for n in 1 2; do
  port_var=DS${n}_PORT; cfg=$CFG_DIR/ds${n}-recall.yaml
  echo "▶ starting DS $n (port ${!port_var}, heartbeatInterval=2s)"
  PNFS_MODE=ds nohup "$BIN_DIR/flint-pnfs-ds" --config "$cfg" \
    >"$LOG_DIR/flint-pnfs-ds${n}.log" 2>&1 &
  echo $! > "$PIDFILE_DIR/flint-pnfs-ds${n}.pid"
done

sleep 2
for n in 1 2; do
  if ! kill -0 "$(cat "$PIDFILE_DIR/flint-pnfs-ds${n}.pid")" 2>/dev/null; then
    echo "✗ DS $n died on startup. Last 30 log lines:"
    tail -30 "$LOG_DIR/flint-pnfs-ds${n}.log"
    exit 1
  fi
done

# Give the DSes time to register + send their first heartbeat so the
# MDS sees them as Active before we touch anything.
sleep 5
echo "✓ MDS + 2 DSes are up"
echo

# ──────────────────────────────────────────────────────────────────────
# 2. Mount + start a long-running background write that holds the
#    layout open. A short `dd` would LAYOUTRETURN on close before
#    our heartbeat detects DS1 dead — the client must still hold a
#    layout pointing at DS1 when we kill it for the recall to have
#    anything to recall. We launch a multi-GiB write in the
#    background, wait for LAYOUTGET, then kill DS1 mid-write.
# ──────────────────────────────────────────────────────────────────────
echo "▶ mount + start background write"
limactl shell "$LIMA_VM" -- sudo bash -c "
  set -eu
  mountpoint -q /mnt/flint-pnfs && umount -lf /mnt/flint-pnfs || true
  mkdir -p /mnt/flint-pnfs
  mount -t nfs4 -o minorversion=1,proto=tcp,port=${MDS_PORT} \
              ${HOST_ADDR}:/ /mnt/flint-pnfs
  # Multi-GiB streamed from /dev/zero; bs=1M with oflag=direct keeps
  # the layout live for the full duration. setsid + nohup + < /dev/null
  # detach from the shell so the SSH session can return.
  rm -f /tmp/recall-dd.pid /tmp/dd.log
  setsid nohup dd if=/dev/zero of=/mnt/flint-pnfs/recall.bin \
       bs=1M count=4096 oflag=direct status=none \
       < /dev/null > /tmp/dd.log 2>&1 &
  echo \$! > /tmp/recall-dd.pid
  disown
  sleep 1
  if ! kill -0 \$(cat /tmp/recall-dd.pid) 2>/dev/null; then
    echo 'DD_FAILED_TO_START'
    cat /tmp/dd.log
    exit 1
  fi
  echo 'BACKGROUND_DD_STARTED'
" || { echo "✗ background dd setup failed"; exit 1; }

# Wait for the MDS to actually grant a layout. The dd is running so
# this should fire within ~1-2s of it starting.
echo "▶ waiting for LAYOUTGET in MDS log…"
DEADLINE_LG=$(($(date +%s) + 10))
while [ "$(date +%s)" -lt "$DEADLINE_LG" ]; do
  if grep -q 'Generated pNFS layout' "$MDS_LOG"; then
    break
  fi
  sleep 0.5
done
if ! grep -q 'Generated pNFS layout' "$MDS_LOG"; then
  echo "✗ no LAYOUTGET observed in MDS log — recall path can't be exercised"
  tail -30 "$MDS_LOG"
  exit 1
fi
echo "✓ LAYOUTGET observed in MDS log"

# Sanity check: the issued layout must include ds-host-1, otherwise
# killing DS1 won't trigger any recall (LayoutManager filters by
# device touched). Smoke run shape gives 2 segments striping
# DS1+DS2; assert that here so failures bisect cleanly.
if ! grep -q 'Segment.*device=ds-host-1' "$MDS_LOG"; then
  echo "✗ issued layout doesn't touch ds-host-1 — kill won't recall anything"
  echo "MDS layout segments seen so far:"
  grep -E 'Segment [0-9]+: device=' "$MDS_LOG" | head
  exit 1
fi
echo "✓ layout includes ds-host-1"
echo

# ──────────────────────────────────────────────────────────────────────
# 3. Kill DS1, watch the recall chain in the MDS log
# ──────────────────────────────────────────────────────────────────────
DS1_PID="$(cat "$PIDFILE_DIR/flint-pnfs-ds1.pid")"
echo "▶ killing DS1 (pid=$DS1_PID)"
kill -9 "$DS1_PID" 2>/dev/null || true
rm -f "$PIDFILE_DIR/flint-pnfs-ds1.pid"

# Worst case: heartbeatTimeout=5s + check_interval=10s + DS heartbeat
# stale slack (up to 10s) = ~25s. Wait 30s with a periodic poll.
echo "▶ waiting up to 30s for the MDS to detect + recall…"
DEADLINE=$(($(date +%s) + 30))
SAW_STALE=false
SAW_RECALL_LIST=false
SAW_FANOUT=false
SAW_CALL_EMITTED=false

while [ "$(date +%s)" -lt "$DEADLINE" ]; do
  if ! "$SAW_STALE"        && grep -q 'stale data servers'             "$MDS_LOG"; then SAW_STALE=true;        echo "  ✓ stale DS detected"; fi
  if ! "$SAW_RECALL_LIST"  && grep -q 'Recalling [0-9]\+ layout'        "$MDS_LOG"; then SAW_RECALL_LIST=true;  echo "  ✓ LayoutManager listed recalls"; fi
  if ! "$SAW_FANOUT"       && grep -q 'Fanning out [0-9]\+ CB_LAYOUTRECALL' "$MDS_LOG"; then SAW_FANOUT=true; echo "  ✓ CallbackManager fan-out triggered"; fi
  if ! "$SAW_CALL_EMITTED" && grep -q 'CB_LAYOUTRECALL → session'        "$MDS_LOG"; then SAW_CALL_EMITTED=true; echo "  ✓ CB_LAYOUTRECALL CALL emitted"; fi

  if "$SAW_STALE" && "$SAW_RECALL_LIST" && "$SAW_FANOUT" && "$SAW_CALL_EMITTED"; then
    break
  fi
  sleep 1
done

echo
echo "▶ Recall chain assertions:"
PASS=true
"$SAW_STALE"        || { echo "  ✗ never saw 'stale data servers' in MDS log"; PASS=false; }
"$SAW_RECALL_LIST"  || { echo "  ✗ never saw 'Recalling N layout(s)' in MDS log"; PASS=false; }
"$SAW_FANOUT"       || { echo "  ✗ never saw 'Fanning out N CB_LAYOUTRECALL' in MDS log"; PASS=false; }
"$SAW_CALL_EMITTED" || { echo "  ✗ never saw 'CB_LAYOUTRECALL → session' in MDS log"; PASS=false; }

if "$PASS"; then
  echo "  ✓ all four MDS-side recall markers fired"
fi

# ──────────────────────────────────────────────────────────────────────
# 4. Optional: report whether the client acked + whether dd survived
# ──────────────────────────────────────────────────────────────────────
echo
echo "▶ Client-side outcome (informational, not asserted):"
if grep -q 'CB_LAYOUTRECALL ← session' "$MDS_LOG"; then
  echo "  • client acked the recall (CB_LAYOUTRECALL reply received)"
elif grep -q 'CB call timed out' "$MDS_LOG"; then
  echo "  • client did not reply within the 10s timeout (will be revoked in A.5)"
elif grep -q 'CB_LAYOUTRECALL to session.*failed' "$MDS_LOG"; then
  echo "  • CB_LAYOUTRECALL failed mid-flight (transport / conn-closed)"
else
  echo "  • no terminal status logged for the CB call yet"
fi

# Kill the background dd (whether it's still going or already
# errored out) and report its exit / size.
limactl shell "$LIMA_VM" -- sudo bash -c "
  set -uo pipefail
  if [ -f /tmp/recall-dd.pid ]; then
    DD_PID=\$(cat /tmp/recall-dd.pid)
    if kill -0 \"\$DD_PID\" 2>/dev/null; then
      kill -9 \"\$DD_PID\" 2>/dev/null || true
      echo '  • background dd was still running (killed)'
    else
      echo '  • background dd exited on its own (likely EIO after recall)'
      tail -5 /tmp/dd.log 2>/dev/null | sed 's/^/      /'
    fi
  fi
  if [ -f /mnt/flint-pnfs/recall.bin ]; then
    sz=\$(stat -c %s /mnt/flint-pnfs/recall.bin 2>/dev/null || echo 0)
    echo \"  • recall.bin size: \$sz bytes\"
  fi
  umount -lf /mnt/flint-pnfs 2>/dev/null || true
" || true

echo
if "$PASS"; then
  echo "✓ PASS: MDS-side CB_LAYOUTRECALL chain fires on DS death"
  exit 0
else
  echo "✗ FAIL: one or more recall markers missing — see $MDS_LOG"
  echo "Last 60 MDS log lines:"
  tail -60 "$MDS_LOG"
  exit 1
fi
