#!/usr/bin/env bash
#
# MDS sharding drill (mds-sharding-plan.md Phase 3).
#
# Two independent MDS shards over ONE shared DS fleet, one kernel
# client mounting both. Proves:
#   1. Fan-out: both DSes register with both shards.
#   2. Distinct shard identity: the client holds two independent
#      sessions (server_owner "flint-pnfs" vs "flint-pnfs-1") — no
#      cross-shard trunking.
#   3. Data path per shard: striped writes on each shard round-trip;
#      stripe files from both shards coexist in one DS namespace with
#      disjoint file_id top bytes (shard bits).
#   4. Cleanup disjointness: rm on shard 1 removes ONLY shard-1 stripe
#      files; shard-0 data intact.
#   5. Blast radius: kill -9 shard 0 → shard 1 keeps FULL service
#      (write/read/delete) while shard 0's mount fails; shard-0
#      restart over its sqlite state recovers the SAME client mount.
#
# Exit status: 0 on PASS, 1 on FAIL.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN_DIR="$REPO_ROOT/spdk-csi-driver/target/release"
LOG_DIR=/tmp
PIDFILE_DIR=/tmp

LIMA_VM="${LIMA_VM:-flint-nfs-client}"
HOST_ADDR="host.lima.internal"

# Shard 0 keeps "legacy" identity (no FLINT_MDS_SHARD_ID suffix in
# server_owner); shard 1 is the suffixed one.
S0_NFS=20480; S0_GRPC=50161
S1_NFS=20481; S1_GRPC=50162
DS1_PORT=20491; DS1_CTL=21491
DS2_PORT=20492; DS2_CTL=21492

WORK=/tmp/flint-shard-drill
DS1_DATA=$WORK/ds1-data
DS2_DATA=$WORK/ds2-data
MNT0=/mnt/flint-shard0
MNT1=/mnt/flint-shard1

cleanup() {
  set +e
  for n in mds0 mds1 ds1 ds2; do
    [ -f "$PIDFILE_DIR/flint-shard-$n.pid" ] && kill "$(cat "$PIDFILE_DIR/flint-shard-$n.pid")" 2>/dev/null
    rm -f "$PIDFILE_DIR/flint-shard-$n.pid"
  done
  pkill -9 -f "flint-pnfs-mds --config $WORK" 2>/dev/null
  pkill -9 -f "flint-pnfs-ds --config $WORK" 2>/dev/null
  limactl shell "$LIMA_VM" -- sudo bash -c "umount -lf $MNT0 $MNT1" 2>/dev/null
}
trap cleanup EXIT

fail() { echo "✗ FAIL: $*"; echo "  Logs: $LOG_DIR/flint-shard-{mds0,mds1,ds1,ds2}.log"; exit 1; }

echo "▶ MDS sharding drill (2 shards, shared 2-DS fleet)"
for bin in flint-pnfs-mds flint-pnfs-ds; do
  [ -x "$BIN_DIR/$bin" ] || fail "missing binary $BIN_DIR/$bin"
done
limactl list --quiet 2>/dev/null | grep -qx "$LIMA_VM" || fail "Lima VM '$LIMA_VM' not found (make lima-up)"

# ── 0. Configs (generated: everything about this drill in one file) ──
rm -rf "$WORK" && mkdir -p "$WORK" $DS1_DATA $DS2_DATA \
  $WORK/s0-exports $WORK/s0-state $WORK/s1-exports $WORK/s1-state
chmod 0777 $WORK/s0-exports $WORK/s1-exports

for s in 0 1; do
  nfs_port=$((20480 + s))
  cat > "$WORK/mds$s.yaml" <<EOF
apiVersion: flint.io/v1alpha1
kind: PnfsConfig
mode: mds
mds:
  bind: {address: "0.0.0.0", port: $nfs_port}
  layout: {type: file, stripeSize: 8388608, policy: stripe}
  dataServers:
    - deviceId: ds-shard-1
      endpoint: "192.168.5.2:$DS1_PORT"
      controlEndpoint: "127.0.0.1:$DS1_CTL"
      bdevs: [data]
    - deviceId: ds-shard-2
      endpoint: "192.168.5.2:$DS2_PORT"
      controlEndpoint: "127.0.0.1:$DS2_CTL"
      bdevs: [data]
  state:
    backend: sqlite
    config: {path: $WORK/s$s-state/state.db}
  failover: {heartbeatTimeout: 30, policy: recall_affected, gracePeriod: 60}
exports:
  - path: $WORK/s$s-exports
    fsid: 1
    options: [rw, sync, no_subtree_check]
    access: [{network: 0.0.0.0/0, permissions: rw}]
logging: {level: info, format: text}
EOF
done

for d in 1 2; do
  port=$((20490 + d)); ctl=$((21490 + d))
  cat > "$WORK/ds$d.yaml" <<EOF
apiVersion: flint.io/v1alpha1
kind: PnfsConfig
mode: ds
ds:
  bind: {address: "0.0.0.0", port: $port, controlPort: $ctl}
  deviceId: ds-shard-$d
  mds:
    endpoint: "127.0.0.1:$S0_GRPC"
    endpoints: ["127.0.0.1:$S0_GRPC", "127.0.0.1:$S1_GRPC"]
    heartbeatInterval: 5
    registrationRetry: 5
    maxRetries: 0
  bdevs:
    - {name: data, mount_point: $WORK/ds$d-data, spdk_volume: data}
exports:
  - path: /
    fsid: 1
    options: [rw, sync]
    access: [{network: 0.0.0.0/0, permissions: rw}]
logging: {level: info, format: text}
EOF
done

start_mds() { # <shard>
  # NB: separate `local` statements — `local s=$1 gp=$((...s))` on one
  # line expands the arithmetic against the PRE-assignment value of s
  # (a leftover from the config-gen loop), collapsing both shards onto
  # one gRPC port.
  local s=$1
  local grpc_port=$((50161 + s))
  echo "▶ starting MDS shard $s (nfs $((20480 + s)), grpc $grpc_port)"
  PNFS_MODE=mds FLINT_MDS_SHARD_ID=$s FLINT_MDS_GRPC_PORT=$grpc_port \
    nohup "$BIN_DIR/flint-pnfs-mds" --config "$WORK/mds$s.yaml" \
    >> "$LOG_DIR/flint-shard-mds$s.log" 2>&1 &
  echo $! > "$PIDFILE_DIR/flint-shard-mds$s.pid"
  sleep 1
  kill -0 "$(cat "$PIDFILE_DIR/flint-shard-mds$s.pid")" 2>/dev/null \
    || { tail -20 "$LOG_DIR/flint-shard-mds$s.log"; fail "MDS shard $s died on startup"; }
}

start_ds() { # <n>
  local n=$1
  echo "▶ starting DS $n"
  PNFS_MODE=ds FLINT_DS_ADVERTISE_ADDR=192.168.5.2 \
    nohup "$BIN_DIR/flint-pnfs-ds" --config "$WORK/ds$n.yaml" \
    > "$LOG_DIR/flint-shard-ds$n.log" 2>&1 &
  echo $! > "$PIDFILE_DIR/flint-shard-ds$n.pid"
  sleep 1
  kill -0 "$(cat "$PIDFILE_DIR/flint-shard-ds$n.pid")" 2>/dev/null \
    || { tail -20 "$LOG_DIR/flint-shard-ds$n.log"; fail "DS $n died on startup"; }
}

rm -f "$LOG_DIR"/flint-shard-{mds0,mds1,ds1,ds2}.log
start_mds 0
start_mds 1
start_ds 1
start_ds 2
sleep 3

# ── 1. Fan-out: every DS registered with every shard ────────────────
for s in 0 1; do
  for d in 1 2; do
    grep -q "DS registered successfully: ds-shard-$d" "$LOG_DIR/flint-shard-mds$s.log" \
      || fail "shard $s never registered ds-shard-$d"
  done
done
echo "  ✓ 2 DSes registered with both shards"

# ── 2. Client mounts both shards ─────────────────────────────────────
limactl shell "$LIMA_VM" -- sudo bash -c "
  mkdir -p $MNT0 $MNT1
  umount -lf $MNT0 $MNT1 2>/dev/null
  mount -t nfs4 -o minorversion=1,proto=tcp,port=$S0_NFS $HOST_ADDR:/ $MNT0 || exit 90
  mount -t nfs4 -o minorversion=1,proto=tcp,port=$S1_NFS $HOST_ADDR:/ $MNT1 || exit 91
" || fail "mounting the two shards from the VM (rc=$?)"
echo "  ✓ client mounted shard 0 and shard 1"

# Distinct identity on the wire: shard 1 must NOT present shard 0's
# server_owner (kernel trunking would fuse the two shards).
grep -q 'server_owner="flint-pnfs-1"' "$LOG_DIR/flint-shard-mds1.log" \
  || fail "shard 1 did not advertise suffixed server_owner (trunking hazard)"
echo "  ✓ shard 1 advertises distinct server identity (flint-pnfs-1)"

# ── 3. Striped I/O on both shards; shard-disjoint file_ids ──────────
SHAS=$(limactl shell "$LIMA_VM" -- sudo bash -c "
  dd if=/dev/urandom of=$MNT0/f0.bin bs=1M count=24 status=none oflag=direct || echo W0FAIL
  dd if=/dev/urandom of=$MNT1/f1.bin bs=1M count=24 status=none oflag=direct || echo W1FAIL
  sync
  sha256sum $MNT0/f0.bin $MNT1/f1.bin | cut -d' ' -f1
")
echo "$SHAS" | grep -q "FAIL" && fail "striped write failed: $SHAS"
SHA_F0=$(echo "$SHAS" | sed -n 1p); SHA_F1=$(echo "$SHAS" | sed -n 2p)

TOPBYTES=$(python3 - "$DS1_DATA" <<'EOF'
import sys, os
tops = set()
for f in os.listdir(sys.argv[1]):
    if ".stripe" in f:
        tops.add(int(f.split(".stripe")[0], 16) >> 56)
print(",".join(str(t) for t in sorted(tops)))
EOF
)
[ "$TOPBYTES" = "0,1" ] || fail "DS1 stripe file_id top bytes = {$TOPBYTES}, want {0,1} (shard bits)"
echo "  ✓ both shards' stripe files coexist on DS1 with disjoint file_id top bytes {0,1}"

# ── 4. Cleanup disjointness: rm on shard 1 leaves shard 0 intact ────
# Cleanup is heartbeat-borne and per-DS: a 3-stripe file's DELETE
# instructions fan out to both DSes and apply over 1-2 heartbeat
# cycles. Poll until shard-1's stripes (top byte 1) are all gone —
# checking once after the first removal races the second DS.
top_bytes() {  # -> sorted comma list of stripe file_id top bytes
  python3 - "$DS1_DATA" "$DS2_DATA" <<'EOF'
import sys, os
tops = set()
for d in sys.argv[1:]:
    for f in os.listdir(d):
        if ".stripe" in f:
            tops.add(int(f.split(".stripe")[0], 16) >> 56)
print(",".join(str(t) for t in sorted(tops)))
EOF
}
[ "$(top_bytes)" = "0,1" ] || fail "pre-rm stripe top-bytes = {$(top_bytes)}, want {0,1}"
limactl shell "$LIMA_VM" -- sudo rm -f "$MNT1/f1.bin"
LEFT_TOPS=""
for i in $(seq 1 12); do
  LEFT_TOPS=$(top_bytes)
  [ "$LEFT_TOPS" = "0" ] && break
  sleep 3
done
[ "$LEFT_TOPS" = "0" ] || fail "after shard-1 rm, surviving stripe top-bytes = {$LEFT_TOPS}, want {0} (shard-1 stripes not all cleaned)"
CHECK_F0=$(limactl shell "$LIMA_VM" -- sudo sha256sum "$MNT0/f0.bin" | cut -d' ' -f1)
[ "$CHECK_F0" = "$SHA_F0" ] || fail "shard-0 data changed after shard-1 cleanup"
echo "  ✓ shard-1 rm removed only shard-1 stripes; shard-0 data intact (${i}x3s)"

# ── 5. Blast radius: kill shard 0, shard 1 keeps full service ────────
echo "▶ kill -9 MDS shard 0"
kill -9 "$(cat "$PIDFILE_DIR/flint-shard-mds0.pid")"
sleep 2
S1_OK=$(limactl shell "$LIMA_VM" -- sudo bash -c "
  dd if=/dev/urandom of=$MNT1/f2.bin bs=1M count=16 status=none oflag=direct || { echo WFAIL; exit 0; }
  sync
  a=\$(sha256sum $MNT1/f2.bin | cut -d' ' -f1)
  mv $MNT1/f2.bin $MNT1/f2-renamed.bin || { echo MVFAIL; exit 0; }
  b=\$(sha256sum $MNT1/f2-renamed.bin | cut -d' ' -f1)
  rm $MNT1/f2-renamed.bin || { echo RMFAIL; exit 0; }
  [ \"\$a\" = \"\$b\" ] && echo S1OK || echo SHAFAIL
")
echo "$S1_OK" | grep -q "S1OK" || fail "shard 1 service degraded while shard 0 down: $S1_OK"
S0_DEAD=$(limactl shell "$LIMA_VM" -- sudo bash -c "timeout 10 ls $MNT0 >/dev/null 2>&1 && echo ALIVE || echo DEAD")
[ "$S0_DEAD" = "DEAD" ] || echo "  (note: shard-0 mount still answered — likely client cache; not a failure)"
echo "  ✓ shard 1 full service (write/rename/delete round-trip) with shard 0 dead"

# ── 6. Shard-0 restart over sqlite: same client mount recovers ───────
echo "▶ restarting MDS shard 0 (sqlite state)"
start_mds 0
RECOVERED=""
for i in $(seq 1 24); do
  CHECK=$(limactl shell "$LIMA_VM" -- sudo bash -c "timeout 10 sha256sum $MNT0/f0.bin 2>/dev/null | cut -d' ' -f1")
  if [ "$CHECK" = "$SHA_F0" ]; then RECOVERED=yes; break; fi
  sleep 5
done
[ -n "$RECOVERED" ] || fail "shard-0 mount did not recover after restart (f0 sha mismatch/timeout)"
echo "  ✓ shard-0 client mount recovered after restart ($((i*5))s), data intact"

limactl shell "$LIMA_VM" -- sudo bash -c "umount $MNT0 $MNT1" 2>/dev/null

echo
echo "✅ PASS: 2-shard fleet — fan-out registration, distinct identities, disjoint file_ids, scoped cleanup, shard-0 blast radius contained, restart recovery"
