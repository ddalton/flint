#!/usr/bin/env bash
#
# Step 9 RIG GATE — Linux-client NFS4ERR_DELAY retry behavior across a
# multi-minute simulated hydration (S3 tier design review, gate (b)
# re-scoped per A5).
#
# The A5 hydration posture answers concurrent I/O on a hydrating file
# with NFS4ERR_DELAY (slot released immediately) instead of holding the
# RPC. This drill measures what the REAL kernel client does with that
# across minutes:
#
#   read leg   `sha256sum` of a cold file whose READs answer DELAY for
#              N seconds. Measures: app-visible elapsed vs N (the
#              overshoot = the client's backoff granularity), zero
#              app-visible errors, content integrity, and — from the
#              server log, which stamps every DELAY answer with attempt
#              number + seconds-since-first-touch — the client's exact
#              retry cadence.
#   warm leg   (all) reads of a WARM file every 5 s DURING the park:
#              the mount must stay responsive (max latency recorded).
#   write leg  (all) buffered writes + fsync into a second cold file
#              whose WRITEs answer DELAY: measures the writeback
#              engine's retry behavior and that fsync eventually
#              succeeds with exit 0.
#   dmesg      cleared before, harvested after: must not contain
#              "server not responding" / task-timeout noise.
#
# Usage: delay-retry-gate.sh <delay-secs> [read|all]
# One server process per invocation (the injector env is read once).
# Exit 0 = leg data collected and integrity held; the go/no-go verdict
# is made from the printed numbers.

set -uo pipefail

DELAY="${1:?usage: delay-retry-gate.sh <delay-secs> [read|all]}"
LEGS="${2:-read}"

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN_DIR="$REPO_ROOT/spdk-csi-driver/target/release"
CFG_DIR="$REPO_ROOT/tests/lima/pnfs"
LIMA_VM="${LIMA_VM:-flint-nfs-client}"
MDS_PORT=20490
EXPORT_DIR=/tmp/flint-lite-export
MNT=/mnt/lite-gate
MDS_LOG=/tmp/flint-lite-gate-mds.log
PIDFILE=/tmp/flint-lite-gate-mds.pid
OUT=/tmp/flint-gate-cold.out

vm() { limactl shell "$LIMA_VM" -- sudo sh -c "$1"; }
say() { echo "▶ $*"; }
pass() { echo "✓ $*"; }
fail() { echo "✗ $*"; exit 1; }

cleanup() {
  set +e
  # Umount FIRST: killing the server under a live mount D-states umount.
  vm "umount -lf $MNT 2>/dev/null; true" 2>/dev/null
  [ -f "$PIDFILE" ] && kill "$(cat "$PIDFILE")" 2>/dev/null
  rm -f "$PIDFILE"
}
trap cleanup EXIT

say "DELAY-retry gate: hydration=${DELAY}s legs=$LEGS"

[ -x "$BIN_DIR/flint-pnfs-mds" ] || fail "build first: cargo build --release"
limactl list --quiet 2>/dev/null | grep -qx "$LIMA_VM" || fail "lima VM not running"

cleanup
rm -rf "$EXPORT_DIR"; mkdir -p "$EXPORT_DIR"; chmod 0777 "$EXPORT_DIR"
if lsof -iTCP:$MDS_PORT -sTCP:LISTEN >/dev/null 2>&1; then
  fail "port $MDS_PORT already held ($(lsof -iTCP:$MDS_PORT -sTCP:LISTEN | tail -1 | awk '{print $1, $2}')) — kill the leftover rig first"
fi

# ── server: standalone hub with the DELAY injector armed ──────────────
say "starting standalone hub with FLINT_TEST_HYDRATION_DELAY_SECS=$DELAY"
FLINT_TEST_HYDRATION_DELAY_SECS="$DELAY" nohup "$BIN_DIR/flint-pnfs-mds" \
  --config "$CFG_DIR/lite.yaml" >"$MDS_LOG" 2>&1 &
echo $! >"$PIDFILE"
UP=""
for _ in $(seq 1 10); do
  sleep 1
  kill -0 "$(cat "$PIDFILE")" 2>/dev/null || { tail -20 "$MDS_LOG"; fail "hub died"; }
  grep -q "STANDALONE" "$MDS_LOG" && { UP=1; break; }
done
[ -n "$UP" ] || { tail -20 "$MDS_LOG"; fail "no standalone banner after 10s"; }

# ── fixtures, server-side (client caches stay cold) ───────────────────
dd if=/dev/urandom of="$EXPORT_DIR/warm.bin" bs=1m count=8 status=none 2>/dev/null \
  || dd if=/dev/urandom of="$EXPORT_DIR/warm.bin" bs=1M count=8 status=none
dd if=/dev/urandom of="$EXPORT_DIR/r.cold.bin" bs=1m count=16 status=none 2>/dev/null \
  || dd if=/dev/urandom of="$EXPORT_DIR/r.cold.bin" bs=1M count=16 status=none
dd if=/dev/zero of="$EXPORT_DIR/w.cold.bin" bs=1024 count=1 status=none 2>/dev/null || true
chmod 0666 "$EXPORT_DIR"/*.bin
SRV_SHA=$(shasum -a 256 "$EXPORT_DIR/r.cold.bin" | awk '{print $1}')
pass "fixtures ready (r.cold.bin sha256 ${SRV_SHA:0:12}…)"

# ── mount ─────────────────────────────────────────────────────────────
HOST_IP=$(vm "getent hosts host.lima.internal | awk '{print \$1}'" | tr -d '\r')
[ -n "$HOST_IP" ] || fail "cannot resolve host.lima.internal"
vm "mountpoint -q $MNT && umount -lf $MNT; mkdir -p $MNT; \
    timeout 30 mount -t nfs4 -o minorversion=1,proto=tcp,port=$MDS_PORT \
      $HOST_IP:/ $MNT" || fail "mount failed"
pass "mounted at $MNT"
vm "dmesg -c > /dev/null 2>&1 || true"

# ── read leg (background) ─────────────────────────────────────────────
say "read leg: sha256sum of the cold file (expect ~${DELAY}s park)"
(vm "start=\$(date +%s.%N); \
     sha=\$(sha256sum $MNT/r.cold.bin | awk '{print \$1}'); rc=\$?; \
     end=\$(date +%s.%N); \
     echo COLD_RC=\$rc; echo COLD_SHA=\$sha; \
     echo COLD_ELAPSED=\$(awk -v a=\$start -v b=\$end 'BEGIN{printf \"%.2f\", b-a}')" \
  >"$OUT" 2>&1) &
READ_PID=$!

# ── warm leg: mount responsiveness DURING the park ────────────────────
WARM_MAX=0
if [ "$LEGS" = "all" ]; then
  say "warm leg: reading warm.bin every 5s during the park"
  END=$((SECONDS + DELAY - 5))
  while [ $SECONDS -lt $END ]; do
    W=$(vm "start=\$(date +%s.%N); cat $MNT/warm.bin >/dev/null; \
            end=\$(date +%s.%N); \
            awk -v a=\$start -v b=\$end 'BEGIN{printf \"%.3f\", b-a}'" | tr -d '\r')
    WARM_MAX=$(awk -v m="$WARM_MAX" -v w="${W:-999}" 'BEGIN{print (w>m)?w:m}')
    # A cold-cache first read of 8 MiB has real transfer time; the
    # gate cares that it stays interactive, not instant.
    sleep 5
  done
  pass "warm leg done, max warm-read latency ${WARM_MAX}s"
fi

# ── write leg: fsync into a DELAY-parked file ─────────────────────────
WRITE_ELAPSED=""
WRITE_RC=""
if [ "$LEGS" = "all" ]; then
  say "write leg: dd + fsync into w.cold.bin (its own ${DELAY}s park)"
  WOUT=$(vm "start=\$(date +%s.%N); \
             dd if=/dev/zero of=$MNT/w.cold.bin bs=64k count=16 \
                conv=notrunc,fsync status=none 2>&1; rc=\$?; \
             end=\$(date +%s.%N); \
             echo WRITE_RC=\$rc \
                  WRITE_ELAPSED=\$(awk -v a=\$start -v b=\$end 'BEGIN{printf \"%.2f\", b-a}')" \
        | tr -d '\r')
  WRITE_RC=$(echo "$WOUT" | grep -o 'WRITE_RC=[0-9]*' | cut -d= -f2)
  WRITE_ELAPSED=$(echo "$WOUT" | grep -o 'WRITE_ELAPSED=[0-9.]*' | cut -d= -f2)
  pass "write leg: rc=$WRITE_RC elapsed=${WRITE_ELAPSED}s"
fi

# ── join the read leg ─────────────────────────────────────────────────
say "waiting for the cold read (budget: DELAY + 120s)"
GUARD=$((DELAY + 120))
SPENT=0
while kill -0 $READ_PID 2>/dev/null; do
  sleep 2; SPENT=$((SPENT + 2))
  [ $SPENT -lt $GUARD ] || { kill $READ_PID 2>/dev/null; fail "cold read exceeded budget — that IS a gate answer (bad)"; }
done
wait $READ_PID 2>/dev/null
COLD_RC=$(grep -o 'COLD_RC=[0-9]*' "$OUT" | cut -d= -f2)
COLD_SHA=$(grep -o 'COLD_SHA=[0-9a-f]*' "$OUT" | cut -d= -f2)
COLD_ELAPSED=$(grep -o 'COLD_ELAPSED=[0-9.]*' "$OUT" | cut -d= -f2)

# ── harvest ───────────────────────────────────────────────────────────
DMESG=$(vm "dmesg 2>/dev/null | grep -Ei 'nfs|rpc' | tail -20" | tr -d '\r')
READ_ATTEMPTS=$(grep -c "TEST hydration DELAY: READ attempt" "$MDS_LOG" || true)
WRITE_ATTEMPTS=$(grep -c "TEST hydration DELAY: WRITE attempt" "$MDS_LOG" || true)
# Retry cadence: distribution of gaps between consecutive DELAY
# answers for r.cold.bin (the raw list is thousands of entries).
GAPSTATS=$(grep "TEST hydration DELAY: READ attempt" "$MDS_LOG" \
  | grep -o 'at +[0-9.]*s' | grep -o '[0-9.]*' \
  | awk 'NR>1{g=$1-p; n++; s+=g; if(g>max)max=g; if(min==""||g<min)min=g} {p=$1} \
         END{if(n) printf "n=%d min=%.3fs mean=%.3fs max=%.3fs", n, min, s/n, max}')
MAXGAP=$(echo "$GAPSTATS" | grep -o 'max=[0-9.]*' | cut -d= -f2)

echo
echo "════════ GATE DATA (hydration=${DELAY}s) ════════"
echo "cold read : rc=$COLD_RC elapsed=${COLD_ELAPSED}s (overshoot $(awk -v e="$COLD_ELAPSED" -v d="$DELAY" 'BEGIN{printf "%.2f", e-d}')s)"
echo "integrity : server=$SRV_SHA"
echo "            client=$COLD_SHA"
[ "$SRV_SHA" = "$COLD_SHA" ] && echo "            MATCH" || echo "            *** MISMATCH ***"
echo "read DELAY answers: $READ_ATTEMPTS   retry-gap distribution: ${GAPSTATS:-n/a}"
echo "max retry gap: ${MAXGAP:-n/a}s"
if [ "$LEGS" = "all" ]; then
  echo "warm read max latency during park: ${WARM_MAX}s"
  echo "write leg : rc=$WRITE_RC elapsed=${WRITE_ELAPSED}s   WRITE DELAY answers: $WRITE_ATTEMPTS"
fi
echo "client dmesg (nfs/rpc tail):"
echo "${DMESG:-  (quiet)}"
echo "═════════════════════════════════════════════════"

[ "$COLD_RC" = "0" ] || fail "cold read returned an app-visible error"
[ "$SRV_SHA" = "$COLD_SHA" ] || fail "content mismatch after the park"
pass "leg complete — integrity held, no app-visible error"
