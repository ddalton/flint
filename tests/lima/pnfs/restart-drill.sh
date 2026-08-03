#!/usr/bin/env bash
#
# F67 restart drill — striped data must survive a full server restart,
# and a DESTROYED binding must fail loud, never as zeros.
#
# Leg 1: fresh boot (memory state backend), write 256 MiB through the
#         mount, md5 it server-served (VM cache dropped + remount).
# Leg 2: kill MDS + both DSes, restart, remount, drop cache, md5 again.
#         PASS = identical md5. (Pre-F67 this leg read 256 MiB of ZEROS:
#         the restarted MDS re-minted the file_id and the DS hole path
#         zero-filled every READ — proven on the x86 rig 2026-08-03.)
# Leg 3: strip the stub's binding xattr AND restart (total binding
#         loss). PASS = the read FAILS (EIO), the MDS logs the F67
#         refusal, and no zeros are ever returned.
#
# See docs/plans/f67-durable-placement-binding.md.
#
# Exit status: 0 on PASS, 1 on FAIL.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN_DIR="$REPO_ROOT/spdk-csi-driver/target/release"
CFG_DIR="$REPO_ROOT/tests/lima/pnfs"

LIMA_VM="${LIMA_VM:-flint-nfs-client}"
HOST_ADDR="host.lima.internal"
MDS_PORT=20490
MNT=/mnt/flint-pnfs-restart
DRILL_FILE=f67-truth.bin
DRILL_MIB=256

DS1_EXPORT=/tmp/flint-pnfs-ds1
DS2_EXPORT=/tmp/flint-pnfs-ds2
MDS_EXPORT_DIR=/tmp/flint-pnfs-mds-exports
MDS_LOG=/tmp/f67-mds.log

ts() { date +%H:%M:%S; }
say()  { printf "[%s] %s\n" "$(ts)" "$*"; }
pass() { printf "[%s] ✓ %s\n" "$(ts)" "$*"; }
fail() { printf "[%s] ✗ FAIL: %s\n" "$(ts)" "$*"; cleanup; exit 1; }

vm() { limactl shell "$LIMA_VM" -- sudo sh -c "$1"; }

cleanup() {
  vm "umount -lf $MNT 2>/dev/null" >/dev/null 2>&1 || true
  pkill -9 -f flint-pnfs-mds 2>/dev/null || true
  pkill -9 -f flint-pnfs-ds 2>/dev/null || true
}

start_servers() {
  pkill -9 -f flint-pnfs-mds 2>/dev/null || true
  pkill -9 -f flint-pnfs-ds 2>/dev/null || true
  sleep 0.5
  PNFS_MODE=mds nohup "$BIN_DIR/flint-pnfs-mds" --config "$CFG_DIR/mds.yaml" \
    >"$MDS_LOG" 2>&1 &
  MDS_PID=$!
  sleep 1
  kill -0 $MDS_PID 2>/dev/null || fail "MDS died on start: $(tail -3 $MDS_LOG)"
  DS_PIDS=()
  for n in 1 2; do
    PNFS_MODE=ds nohup "$BIN_DIR/flint-pnfs-ds" --config "$CFG_DIR/ds$n.yaml" \
      >/tmp/f67-ds$n.log 2>&1 &
    DS_PIDS+=($!)
  done
  sleep 2
  for p in "${DS_PIDS[@]}"; do
    kill -0 "$p" 2>/dev/null || fail "a DS died on start"
  done
}

remount() {
  vm "umount -lf $MNT 2>/dev/null; mkdir -p $MNT
      mount -t nfs4 -o minorversion=1,proto=tcp,port=$MDS_PORT $HOST_ADDR:/ $MNT" \
    || fail "mount failed"
}

drop_vm_cache() {
  local before after
  before=$(vm "free -m | awk '/Mem:/{print \$6}'" 2>/dev/null | tr -d '\r')
  vm "sync; echo 3 > /proc/sys/vm/drop_caches" >/dev/null 2>&1
  after=$(vm "free -m | awk '/Mem:/{print \$6}'" 2>/dev/null | tr -d '\r')
  [ -n "$before" ] && [ -n "$after" ] || fail "cannot read VM cache size"
}

ds_cpu_cs() {
  local total=0 t
  for p in "${DS_PIDS[@]}"; do
    t=$(ps -o cputime= -p "$p" 2>/dev/null | tr -d ' ' | awk -F'[:.]' \
      '{ if (NF==4) print ($1*3600+$2*60+$3)*100+$4;
         else if (NF==3) print ($1*60+$2)*100+$3;
         else print 0 }')
    total=$((total + ${t:-0}))
  done
  echo "$total"
}

md5_via_mount() {
  vm "cd $MNT && timeout 90 md5sum $DRILL_FILE 2>/dev/null" | awk '{print $1}' | tr -d '\r'
}

echo "══════════════════════════════════════════════════════════════════"
echo " F67 restart drill — binding survival + loud orphan refusal"
echo "══════════════════════════════════════════════════════════════════"

[ -x "$BIN_DIR/flint-pnfs-mds" ] || fail "binaries missing — run 'make build-pnfs'"

# Fresh world.
cleanup
rm -rf "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"
mkdir -p "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"
chmod 0777 "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"

# ── leg 1: write + same-boot server-served md5 ──────────────────────────
say "leg 1: boot, write ${DRILL_MIB} MiB, same-boot md5"
# The zero-fill fingerprint. Leg 1 and leg 2 must both differ from it —
# without this, a rig that zero-fills BOTH legs "passes" on zeros==zeros
# (the exact shape that voided two copy-tax gates).
ZERO_MD5=$(python3 -c "
import hashlib
h = hashlib.md5()
z = bytes(1024 * 1024)
for _ in range($DRILL_MIB):
    h.update(z)
print(h.hexdigest())")
start_servers
remount
vm "cd $MNT && dd if=/dev/urandom of=$DRILL_FILE bs=1M count=$DRILL_MIB 2>/dev/null" \
  || fail "write failed"
remount
drop_vm_cache
C0=$(ds_cpu_cs)
MD5_TRUTH=$(md5_via_mount)
C1=$(ds_cpu_cs)
[ -n "$MD5_TRUTH" ] || fail "same-boot md5 read failed"
[ "$MD5_TRUTH" != "$ZERO_MD5" ] || fail "same-boot read is ALL ZEROS — rig is on the hole path"
# The DSes must have participated at all; the zeros check above is the
# load-bearing honesty gate (cpu cost per byte is platform-dependent).
[ $((C1 - C0)) -ge 3 ] || fail "same-boot read moved DS cpu only $((C1-C0))cs — not server-served"
pass "leg 1: same-boot md5 $MD5_TRUTH (ds-cpu +$((C1-C0))cs)"

STUB="$MDS_EXPORT_DIR/$DRILL_FILE"
[ -f "$STUB" ] || fail "stub $STUB not found"
xattr -p user.flint.placement "$STUB" >/dev/null 2>&1 \
  || fail "stub carries no binding xattr — leg 1 should have written it"
pass "leg 1: stub binding xattr present"

# ── leg 2: full restart, binding intact ─────────────────────────────────
say "leg 2: restart every server (memory backend: records gone, xattr survives)"
start_servers
remount
drop_vm_cache
C0=$(ds_cpu_cs)
MD5_AFTER=$(md5_via_mount)
C1=$(ds_cpu_cs)
[ -n "$MD5_AFTER" ] || fail "post-restart md5 read failed (should succeed via xattr recovery)"
[ "$MD5_AFTER" = "$MD5_TRUTH" ] \
  || fail "POST-RESTART DATA DIFFERS: $MD5_AFTER != $MD5_TRUTH — F67 regression (zeros?)"
[ "$MD5_AFTER" != "$ZERO_MD5" ] || fail "post-restart read is ALL ZEROS — F67 corruption"
[ $((C1 - C0)) -ge 3 ] || fail "post-restart read moved DS cpu only $((C1-C0))cs — not server-served"
grep -q "F67: recovered placement" "$MDS_LOG" \
  || fail "MDS log shows no F67 recovery line — did the binding actually drive this?"
pass "leg 2: md5 identical across restart, recovered from stub binding (ds-cpu +$((C1-C0))cs)"

# ── leg 3: destroyed binding must fail LOUD, never as zeros ─────────────
say "leg 3: strip the binding xattr, restart — total binding loss"
xattr -d user.flint.placement "$STUB" || fail "could not strip xattr"
start_servers
remount
drop_vm_cache
MD5_ORPHAN=$(md5_via_mount || true)
if [ -n "$MD5_ORPHAN" ]; then
  [ "$MD5_ORPHAN" = "$ZERO_MD5" ] && fail "orphaned read returned ZEROS — the exact F67 corruption"
  fail "orphaned read SUCCEEDED ($MD5_ORPHAN) — expected loud EIO"
fi
grep -q "F67" "$MDS_LOG" \
  || fail "orphaned read failed but the MDS log has no F67 refusal line"
pass "leg 3: orphaned read refused loudly (no zeros, F67 logged)"

cleanup
echo "══════════════════════════════════════════════════════════════════"
echo " PASS — binding survives restart; orphan fails loud, never zeros"
echo "══════════════════════════════════════════════════════════════════"
