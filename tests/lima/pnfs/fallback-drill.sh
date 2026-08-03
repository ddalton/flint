#!/usr/bin/env bash
#
# pNFS bounded-DELAY fallback drill — the fix for the DELAY livelock
# (docs/pnfs-operator-runbook.md, "the DELAY livelock").
#
# Kernel-verified background: a files-layout client whose DS I/O fails
# falls back to READ-through-MDS and, if the MDS answers NFS4ERR_DELAY,
# retries the identical MDS READ every ~100 ms FOREVER — the loop never
# re-drives the layout path, holds page locks, and survives DS
# recovery. The bounded escalation answers the fallback with:
#   - PROXY (F66)  while the registry thinks the fleet is healthy: the
#                  MDS applies the I/O to the stripes via DsControl. A
#                  dead-but-not-yet-Offline DS makes the proxy attempt
#                  fail → NFS4ERR_DELAY until the missed heartbeat
#                  flips the registry, after which the bounded ladder
#                  below owns the file. (Pre-F66 this arm answered
#                  instant NFS4ERR_IO, which also EIO'd HEALTHY
#                  clients' truncate-window stragglers — the fsx
#                  failure. The ceiling, not the ambiguity window, is
#                  what bounds the wait.)
#   - NFS4ERR_DELAY while a pinned DS is down within the ceiling;
#   - NFS4ERR_IO   once a pinned DS's outage exceeds the ceiling (and
#                  when the proxy is off/unconfigured).
#
# Phases (ceiling=60 s via FLINT_PNFS_FALLBACK_DELAY_CEILING_SECS,
# mds.yaml heartbeatTimeout=30 s):
#   1. Write F striped over {DS1,DS2}; remount (cold client).
#   2. kill -9 DS1 at T0.
#   3. T0+2   read → PARKED     (registry still Active → Proxy; DS1 is
#                                dead so the proxy fails → DELAY. The
#                                dd is killed by its timeout — F66: the
#                                ambiguity window parks, it no longer
#                                EIOs.)
#   4. T0+45  read → HANG       (DS1 Offline, outage 45s < 60s → Delay;
#                                dd killed by timeout — this read's page
#                                I/O keeps looping in the VM kernel: the
#                                trap, deliberately armed).
#   5. T0+75  read → FAST EIO   (outage > ceiling → FailFast; the step-4
#                                loop got IO on its next retry and died —
#                                the ceiling SPRINGS an in-flight trap).
#   6. restart DS1; poll-read until the checksum matches (client's 120 s
#      device/layout marks expire → LAYOUTGET → DS1 → data). Before the
#      fix this NEVER converged without a node-level unstick.
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
CEILING_SECS=60

DS1_EXPORT="/tmp/flint-pnfs-ds1"
DS2_EXPORT="/tmp/flint-pnfs-ds2"
MDS_EXPORT_DIR="/tmp/flint-pnfs-mds-exports"
MNT=/mnt/flint-pnfs

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

# Remote read of F with a bound. Prints the remote dd exit code
# ("124"/"137"/"143" family = killed by timeout = HANG).
timed_read() {
  local bound="$1"
  limactl shell "$LIMA_VM" -- sudo sh -c \
    "timeout $bound dd if=$MNT/F of=/dev/null bs=1M 2>/dev/null; echo \$?" | tail -1
}

echo "▶ pNFS bounded-DELAY fallback drill (ceiling=${CEILING_SECS}s)"
echo

# ── 0. Pre-flight ─────────────────────────────────────────────────────
for bin in flint-pnfs-mds flint-pnfs-ds; do
  [ -x "$BIN_DIR/$bin" ] || { echo "✗ Missing binary: $BIN_DIR/$bin"; exit 1; }
done
limactl list --quiet 2>/dev/null | grep -qx "$LIMA_VM" \
  || { echo "✗ Lima VM '$LIMA_VM' not found. Run: make lima-up"; exit 1; }

rm -rf "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"
mkdir -p "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"
chmod 0777 "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"

echo "▶ starting MDS (fallback ceiling ${CEILING_SECS}s)"
PNFS_MODE=mds FLINT_PNFS_FALLBACK_DELAY_CEILING_SECS=$CEILING_SECS \
  nohup "$BIN_DIR/flint-pnfs-mds" --config "$CFG_DIR/mds.yaml" \
  >"$LOG_DIR/flint-pnfs-mds.log" 2>&1 &
echo $! > "$PIDFILE_DIR/flint-pnfs-mds.pid"
sleep 1
kill -0 "$(cat "$PIDFILE_DIR/flint-pnfs-mds.pid")" 2>/dev/null \
  || { echo "✗ MDS died on startup:"; tail -30 "$LOG_DIR/flint-pnfs-mds.log"; exit 1; }
start_ds 1
start_ds 2
sleep 2

# ── 1. Write F, remember its sha, remount for a cold client ─────────
echo "▶ writing F (24 MiB, striped) and remounting"
HASH_F=$(limactl shell "$LIMA_VM" -- sudo bash -c "
  set -eu
  mkdir -p $MNT
  mount -t nfs4 -o minorversion=1,proto=tcp,port=${MDS_PORT} ${HOST_ADDR}:/ $MNT
  dd if=/dev/urandom of=$MNT/F bs=1M count=24 status=none oflag=direct
  sync
  sha256sum $MNT/F | cut -d' ' -f1
  umount $MNT
  mount -t nfs4 -o minorversion=1,proto=tcp,port=${MDS_PORT} ${HOST_ADDR}:/ $MNT
" | tail -1) || { echo "✗ write phase failed"; exit 1; }
echo "  F sha256: $HASH_F"

# ── 2. Kill DS1 ──────────────────────────────────────────────────────
echo "▶ kill -9 DS1 (T0)"
kill -9 "$(cat "$PIDFILE_DIR/flint-pnfs-ds1.pid")"
rm -f "$PIDFILE_DIR/flint-pnfs-ds1.pid"
T0=$SECONDS

FAIL=""

# ── 3. Ambiguity window: registry still Active → Proxy → parked ──────
# F66: the healthy-registry answer is the fallback PROXY. DS1 is dead,
# so the proxy attempt fails and the MDS answers DELAY — the read PARKS
# (dd dies by its own timeout) instead of the pre-F66 instant EIO. The
# missed heartbeat flips DS1 Offline within 30 s and the bounded ladder
# (steps 4-5) takes over; the CEILING is what bounds the wait now, not
# the ambiguity window. An instant EIO here would be the F66 regression
# — it is what turned healthy clients' truncate-window stragglers into
# msync EIO (the fsx failure).
sleep 2
echo "▶ T0+2s read (registry still thinks DS1 healthy → proxy attempt parks)"
rc=$(timed_read 20)
if [ "$rc" = "0" ]; then FAIL="$FAIL\n  - T0+2 read SUCCEEDED (should park: DS1 is dead)";
elif [ "$rc" -ge 124 ] 2>/dev/null; then echo "  ✓ parked (rc=$rc — proxy failed → DELAY, awaiting the registry flip)";
else FAIL="$FAIL\n  - T0+2 read got fast rc=$rc (want PARKED — instant EIO here is the pre-F66 straggler-killer)"; fi

# ── 4. Outage window: Offline + under ceiling → Delay (parks) ────────
# Wait for the MDS's stale sweep to mark DS1 Offline (heartbeatTimeout
# 30 s), then read IMMEDIATELY: outage clock anchors at DS1's LAST
# heartbeat (≈ T0), so the Delay window closes ~T0+55..60.
echo "▶ waiting for the MDS to mark ds-host-1 Offline"
until grep -q "ds-host-1 heartbeat timeout" "$LOG_DIR/flint-pnfs-mds.log"; do
  [ $(( SECONDS - T0 )) -gt 50 ] && { echo "✗ sweep never marked DS1 Offline"; exit 1; }
  sleep 1
done
echo "▶ T0+$(( SECONDS - T0 ))s read (DS1 Offline, outage < ceiling → expect PARKED)"
rc=$(timed_read 10)
if [ "$rc" -ge 124 ] 2>/dev/null; then echo "  ✓ parked (dd killed at 10s; its page I/O keeps looping in the VM kernel — trap armed)";
else FAIL="$FAIL\n  - Offline-window read returned rc=$rc (want Delay-parked hang: outage under ceiling)"; fi

# ── 5. Past ceiling → FailFast, and it springs the armed trap ────────
sleep $(( 70 - (SECONDS - T0) ))
echo "▶ T0+$(( SECONDS - T0 ))s read (outage > ceiling → expect fast EIO; step-4 loop sprung)"
rc=$(timed_read 20)
if [ "$rc" = "0" ]; then FAIL="$FAIL\n  - past-ceiling read SUCCEEDED (should EIO: DS1 still dead)";
elif [ "$rc" -ge 124 ] 2>/dev/null; then FAIL="$FAIL\n  - past-ceiling read HUNG — ceiling did not spring the fallback loop (the livelock)";
else echo "  ✓ fast EIO (rc=$rc)"; fi

# ── 6. Restart DS1 → the SAME live client must converge ─────────────
# No remount: since each DS presents its own server identity
# (flint-pnfs-ds-<id>, the P0-5 fix), the client keeps a separate
# lease per DS and re-establishes it cleanly against the restarted
# process — even on this same-IP rig, where the old shared identity
# made trunking detection churn EXCHANGE_ID forever.
echo "▶ restarting DS1; polling the SAME client until the checksum matches"
start_ds 1
DEADLINE=$(( SECONDS + 160 ))
RECOVERED=""
while [ $SECONDS -lt $DEADLINE ]; do
  h=$(limactl shell "$LIMA_VM" -- sudo sh -c \
      "timeout 30 sha256sum $MNT/F 2>/dev/null | cut -d' ' -f1" 2>/dev/null | tail -1)
  if [ "$h" = "$HASH_F" ]; then RECOVERED="yes"; break; fi
  sleep 5
done
if [ -n "$RECOVERED" ]; then
  echo "  ✓ recovered: checksum matches ($(( SECONDS - T0 ))s after T0) — no unstick, no reboot"
else
  FAIL="$FAIL\n  - read never recovered after DS1 restart + remount"
fi

# ── 7. The MDS must have used both branches ──────────────────────────
grep -q "failed fast" "$LOG_DIR/flint-pnfs-mds.log" \
  || FAIL="$FAIL\n  - MDS log has no 'failed fast' line (FailFast branch never fired)"
grep -q "NFS4ERR_DELAY (pinned DS down" "$LOG_DIR/flint-pnfs-mds.log" \
  || FAIL="$FAIL\n  - MDS log has no bounded-DELAY line (Delay branch never fired)"

echo
if [ -n "$FAIL" ]; then
  echo -e "✗ FAIL:$FAIL"
  echo "  Logs: $LOG_DIR/flint-pnfs-{mds,ds1,ds2}.log"
  exit 1
fi
echo "✅ PASS: bounded-DELAY fallback — parked through the ambiguity window (F66 proxy attempt → DELAY), parked under the ceiling, sprung past it, self-recovered after DS restart"
