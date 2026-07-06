#!/usr/bin/env bash
#
# mds-restart-load.sh — durable-DS plan Phase 3 gate: MDS killed and
# restarted UNDER LOAD.
#
# What it asserts (the Phase 3 "Done when" verbatim):
#   1. DSes re-register within ONE heartbeat of the restarted MDS
#      coming up — via the heartbeat-NACK → immediate re-register
#      path, not the 3-strike transport-failure path.
#   2. ZERO stale-device detections / layout recalls for healthy DSes
#      (the boot grace holds the sweep while they re-introduce
#      themselves).
#   3. A client writer running THROUGH the kill+restart completes with
#      no I/O errors (hard mount blocks, reclaims through grace,
#      resumes), and bytes written after recovery round-trip a
#      fresh-mount readback.
#
# Exit 0 = PASS.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN_DIR="$REPO_ROOT/spdk-csi-driver/target/release"
CFG_DIR="$REPO_ROOT/tests/lima/pnfs"
PIDFILE_DIR="/tmp"
LOG_DIR="/tmp"
STATE_DIR="/tmp/flint-pnfs-restart-state"

LIMA_VM="${LIMA_VM:-flint-nfs-client}"
HOST_ADDR="host.lima.internal"
MDS_PORT=20490

DS1_EXPORT="/tmp/flint-pnfs-ds1"
DS2_EXPORT="/tmp/flint-pnfs-ds2"
MDS_EXPORT_DIR="/tmp/flint-pnfs-mds-exports"
MNT=/mnt/flint-pnfs-load

# ds1/ds2.yaml heartbeat every 10s; "within one heartbeat" plus dial
# and log-flush margin.
DS_HEARTBEAT_S=10
REREG_DEADLINE_S=15

cleanup() {
  set +e
  for n in mds ds1 ds2; do
    [ -f "$PIDFILE_DIR/flint-pnfs-$n.pid" ] && kill "$(cat "$PIDFILE_DIR/flint-pnfs-$n.pid")" 2>/dev/null
    rm -f "$PIDFILE_DIR/flint-pnfs-$n.pid"
  done
  pkill -9 -f "flint-pnfs-mds" 2>/dev/null || true
  pkill -9 -f "flint-pnfs-ds"  2>/dev/null || true
  limactl shell "$LIMA_VM" -- sudo umount -lf "$MNT" 2>/dev/null || true
}
trap cleanup EXIT

step() { printf '\n▶ %s\n' "$*"; }
ok()   { printf '  ✓ %s\n' "$*"; }
fail() { printf '\n✗ %s\n' "$*" >&2; exit 1; }

# ── pre-flight ────────────────────────────────────────────────────────
for bin in flint-pnfs-mds flint-pnfs-ds; do
  [ -x "$BIN_DIR/$bin" ] || fail "missing $BIN_DIR/$bin (cargo build --release)"
done
limactl list --quiet 2>/dev/null | grep -qx "$LIMA_VM" || fail "Lima VM '$LIMA_VM' not running (make lima-up)"

rm -rf "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR" "$STATE_DIR"
mkdir -p "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR" "$STATE_DIR"
chmod 0777 "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR" "$STATE_DIR"

MDS_LOG1="$LOG_DIR/flint-pnfs-mds-load1.log"
MDS_LOG2="$LOG_DIR/flint-pnfs-mds-load2.log"
: > "$MDS_LOG1"; : > "$MDS_LOG2"

start_mds() { # <logfile>
  PNFS_MODE=mds nohup "$BIN_DIR/flint-pnfs-mds" --config "$CFG_DIR/mds-restart-dynamic.yaml" \
    >>"$1" 2>&1 &
  echo $! > "$PIDFILE_DIR/flint-pnfs-mds.pid"
}

# ── 1. stack up ───────────────────────────────────────────────────────
step "starting MDS (sqlite) + 2 DSes"
start_mds "$MDS_LOG1"
sleep 1
kill -0 "$(cat "$PIDFILE_DIR/flint-pnfs-mds.pid")" 2>/dev/null || { tail -20 "$MDS_LOG1"; fail "MDS died on startup"; }
for n in 1 2; do
  # The MDS config is dynamic-only (no configured endpoints to
  # override the DS-reported bind address), so each DS must advertise
  # the VM-reachable host address itself — same mechanism as the
  # chart's per-pod Service DNS.
  PNFS_MODE=ds FLINT_DS_ADVERTISE_ADDR=192.168.5.2 \
    nohup "$BIN_DIR/flint-pnfs-ds" --config "$CFG_DIR/ds${n}.yaml" \
    >"$LOG_DIR/flint-pnfs-ds${n}-load.log" 2>&1 &
  echo $! > "$PIDFILE_DIR/flint-pnfs-ds${n}.pid"
done
sleep 3
for n in 1 2; do
  kill -0 "$(cat "$PIDFILE_DIR/flint-pnfs-ds${n}.pid")" 2>/dev/null || fail "DS $n died on startup"
done
n_boot=$(grep -c "DS registered successfully" "$MDS_LOG1" 2>/dev/null || true)
[ "${n_boot:-0}" -ge 2 ] || fail "expected 2 dynamic registrations at boot, saw ${n_boot:-0}"
ok "MDS + 2 DSes up (dynamic registration only)"

# ── 2. mount + start the load writer ─────────────────────────────────
step "mount + background writer (4 MiB appends, ~60s of load)"
limactl shell "$LIMA_VM" -- sudo bash -c "
  set -eu
  mountpoint -q $MNT && umount -lf $MNT || true
  mkdir -p $MNT
  mount -t nfs4 -o minorversion=1,proto=tcp,port=${MDS_PORT} ${HOST_ADDR}:/ $MNT
  rm -f /tmp/pnfs-load-status $MNT/load.bin
" || fail "mount failed"

# The writer mixes metadata ops (open/close per dd) with striped data
# writes. hard-mount semantics: it BLOCKS through the MDS outage and
# resumes after reclaim; any -EIO fails the loop and the drill.
limactl shell "$LIMA_VM" -- sudo bash -c "
  nohup bash -c '
    for i in \$(seq 1 60); do
      dd if=/dev/zero of=$MNT/load.bin bs=1M count=4 conv=notrunc oflag=append 2>/dev/null \
        || { echo FAIL > /tmp/pnfs-load-status; exit 1; }
      sleep 0.2
    done
    echo OK > /tmp/pnfs-load-status
  ' >/dev/null 2>&1 &
" || fail "could not start writer"
sleep 5
ok "writer running"

# ── 3. kill -9 the MDS mid-load, restart over the same state.db ─────
step "kill -9 MDS mid-load, restart over same state.db"
kill -9 "$(cat "$PIDFILE_DIR/flint-pnfs-mds.pid")" 2>/dev/null
sleep 2
start_mds "$MDS_LOG2"
T_RESTART=$(date +%s)
sleep 1
kill -0 "$(cat "$PIDFILE_DIR/flint-pnfs-mds.pid")" 2>/dev/null || { tail -20 "$MDS_LOG2"; fail "restarted MDS died"; }
grep -q "MDS instance counter: 2" "$MDS_LOG2" || { grep "instance counter" "$MDS_LOG2"; fail "expected instance counter=2"; }
ok "MDS restarted (instance 2) at $(date -u +%H:%M:%S)"

# ── 4. Phase 3 assertion: both DSes re-register within one heartbeat ─
step "waiting for DS re-registration (deadline ${REREG_DEADLINE_S}s ≈ one ${DS_HEARTBEAT_S}s heartbeat)"
deadline=$(( T_RESTART + REREG_DEADLINE_S ))
while :; do
  # grep -c prints the count even when it exits 1 on zero matches —
  # don't `|| echo 0` (that appends a second line and breaks -ge).
  n_reg=$(grep -c "DS registered successfully" "$MDS_LOG2" 2>/dev/null || true)
  [ "${n_reg:-0}" -ge 2 ] && break
  [ "$(date +%s)" -gt "$deadline" ] && {
    grep -E "register|Heartbeat" "$MDS_LOG2" | tail -10
    fail "only $n_reg/2 DSes re-registered within ${REREG_DEADLINE_S}s"
  }
  sleep 0.5
done
T_REG=$(date +%s)
ok "both DSes re-registered ${T_REG}-${T_RESTART}=$(( T_REG - T_RESTART ))s after restart"

# The fast path must be the NACK route, not the 3-strike transport route.
grep -q "re-registering now" "$LOG_DIR/flint-pnfs-ds1-load.log" "$LOG_DIR/flint-pnfs-ds2-load.log" \
  || fail "DS logs show no NACK→immediate re-register ('re-registering now')"
ok "re-registration went through the heartbeat-NACK fast path"

# ── 5. Phase 3 assertion: zero staleness/recalls for healthy DSes ────
step "letting the writer finish, then checking for spurious recalls"
# The writer's per-dd OPENs hit NFS4ERR_GRACE on the restarted MDS
# until the 90s grace period expires ("clients reclaim through
# grace"), THEN the remaining appends run — total legitimate stall is
# grace + remaining-writes, so budget generously before calling it an
# error.
for _ in $(seq 1 240); do
  st=$(limactl shell "$LIMA_VM" -- sudo cat /tmp/pnfs-load-status 2>/dev/null)
  [ -n "$st" ] && break
  sleep 1
done
[ "$st" = "OK" ] || fail "writer did not finish cleanly (status: '${st:-none}') — I/O error during restart window"
ok "writer completed 60×4 MiB appends with zero errors through the restart"

if grep -qE "stale data servers|Detected .* stale" "$MDS_LOG2"; then
  grep -E "stale|Recall" "$MDS_LOG2" | head -5
  fail "restarted MDS marked healthy DSes stale"
fi
if grep -qiE "CB_LAYOUTRECALL|fan.out.recall|Recalling" "$MDS_LOG2"; then
  grep -iE "recall" "$MDS_LOG2" | head -5
  fail "restarted MDS fired recalls for healthy DSes"
fi
ok "zero stale-detections, zero recalls (boot grace held)"

# ── 6. integrity: fresh-mount readback of post-restart bytes ─────────
step "fresh-mount integrity readback"
H1=$(limactl shell "$LIMA_VM" -- sudo bash -c "sha256sum $MNT/load.bin | awk '{print \$1}'")
limactl shell "$LIMA_VM" -- sudo bash -c "umount -lf $MNT && mount -t nfs4 -o minorversion=1,proto=tcp,port=${MDS_PORT} ${HOST_ADDR}:/ $MNT"
H2=$(limactl shell "$LIMA_VM" -- sudo bash -c "sha256sum $MNT/load.bin | awk '{print \$1}'")
[ -n "$H1" ] && [ "$H1" = "$H2" ] || fail "readback hash mismatch: '$H1' vs '$H2'"
SIZE=$(limactl shell "$LIMA_VM" -- sudo stat -c %s "$MNT/load.bin")
[ "$SIZE" -eq $(( 60 * 4 * 1048576 )) ] || fail "load.bin is $SIZE bytes, expected $(( 60*4*1048576 ))"
ok "240 MiB, hash stable across remount ($H1)"

printf '\n✅ PASS: MDS kill -9 under load — one-heartbeat re-registration, zero recalls, error-free I/O\n'
