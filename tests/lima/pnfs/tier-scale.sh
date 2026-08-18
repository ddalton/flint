#!/usr/bin/env bash
#
# S3-tier SCALE drill — measurement, not chaos. Two questions the
# functional drills never ask:
#
#   A  many files — FILES small files (default 10000) through
#      capture→flush→manifest: create wall-time, settle-to-RPO-0
#      wall-time, manifest object size, then a full DR import of the
#      same population (import wall-time, stub count) with a content
#      spot-check through a kernel client.
#   B  one big file — BIGMB MiB (default 2048) through the multipart
#      publish, evict, and ranged hydration: wall-times, effective
#      throughput, and the hub's PEAK RSS at every stage (the streaming
#      claim is a measurement here, not a hope — a buffering regression
#      shows up as RSS ≈ file size).
#
# Numbers are the deliverable; hard failures fire only on pathology
# (thresholds are deliberately generous). Knobs: FILES, BIGMB, KEEP=1.
# Own MinIO (:9001) and hub port (:20494) — safe next to the chaos rig.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN_DIR="$REPO_ROOT/spdk-csi-driver/target/release"
LIMA_VM="${LIMA_VM:-flint-nfs-client}"

FILES="${FILES:-10000}"
BIGMB="${BIGMB:-2048}"

PORT=20494
MNT=/mnt/scale
MINIO_NAME=flint-scale-minio
MINIO_PORT=9001
MINIO_USER=flintscale
MINIO_PASS=flintscale123
PREFIX=vol1/

say()  { echo; echo "── $*"; }
pass() { echo "  ✓ $*"; }
fail() { echo "  ✗ FAIL: $*"; exit 1; }

vm() { limactl shell "$LIMA_VM" -- sudo sh -c "$1"; }

s3() {
  env -u AWS_PROFILE \
    AWS_ACCESS_KEY_ID=$MINIO_USER AWS_SECRET_ACCESS_KEY=$MINIO_PASS \
    AWS_DEFAULT_REGION=us-east-1 AWS_EC2_METADATA_DISABLED=true \
    aws --endpoint-url "http://127.0.0.1:$MINIO_PORT" "$@"
}

gen_cfg() { # OUT EXPORT STATEDIR BUCKET WATERMARK
  cat > "$1" <<EOF
apiVersion: flint.io/v1alpha1
kind: PnfsConfig
mode: standalone
mds:
  bind: { address: "0.0.0.0", port: $PORT }
  layout: { type: file, stripeSize: 8388608, policy: stripe }
  dataServers: []
  state:
    backend: sqlite
    config: { path: $3/state.db }
  tier:
    enabled: true
    bucket: $4
    keyPrefix: "$PREFIX"
    endpoint: "http://127.0.0.1:$MINIO_PORT"
    flushFloorSecs: 1
    quiesceSecs: 1
    tickSecs: 1
    epochHeartbeatSecs: 2
    epochLeaseMisses: 5
    wholePutMaxBytes: 4194304
    watermarkPct: $5
    reserveBytes: 33554432
    ballastBytes: 16777216
exports:
  - path: $2
    fsid: 1
    options: [rw, sync, no_subtree_check]
    access: [{ network: 0.0.0.0/0, permissions: rw }]
logging:
  level: info
  format: text
  components: { mds: info }
monitoring:
  prometheus: { enabled: false, port: 0, path: /metrics }
  health: { enabled: false, port: 0, path: /health }
  metrics: []
EOF
}

launch_hub() { # $1=tag $2=cfg
  env -u AWS_PROFILE \
    AWS_ACCESS_KEY_ID=$MINIO_USER AWS_SECRET_ACCESS_KEY=$MINIO_PASS \
    AWS_REGION=us-east-1 AWS_EC2_METADATA_DISABLED=true \
    FLINT_TIER_REPORT_SECS=30 \
    nohup "$BIN_DIR/flint-pnfs-mds" --config "$2" >>"/tmp/flint-scale-$1.log" 2>&1 &
  echo $! >"/tmp/flint-scale-$1.pid"
  disown
}
hub_pid()  { cat "/tmp/flint-scale-$1.pid" 2>/dev/null; }
stop_hub() {
  kill "$(hub_pid "$1")" 2>/dev/null
  for _ in $(seq 1 20); do kill -0 "$(hub_pid "$1")" 2>/dev/null || break; sleep 0.5; done
  kill -9 "$(hub_pid "$1")" 2>/dev/null; rm -f "/tmp/flint-scale-$1.pid"
}
wait_bound() { # $1=deadline $2=tag
  local d=$((SECONDS + $1))
  while [ $SECONDS -lt $d ]; do
    lsof -nP -iTCP:$PORT -sTCP:LISTEN >/dev/null 2>&1 && return 0
    sleep 1
  done
  tail -20 "/tmp/flint-scale-$2.log"; fail "hub '$2' never bound :$PORT"
}
mount_client() {
  vm "mountpoint -q $MNT && umount -lf $MNT; mkdir -p $MNT; \
      timeout 30 mount -t nfs4 -o minorversion=1,proto=tcp,port=$PORT \
        \$(getent hosts host.lima.internal | awk '{print \$1}'):/ $MNT" \
    || fail "client mount failed"
}
umount_client() { vm "umount -lf $MNT 2>/dev/null; true"; }

manifest_json() { # $1=bucket → stdout, or nothing
  s3 s3 cp "s3://$1/${PREFIX}.flint/manifest" - 2>/dev/null
}
settle_rpo() { # $1=bucket $2=want-entries("any" ok) $3=deadline → seconds taken
  local t0=$SECONDS d=$((SECONDS + $3))
  while :; do
    manifest_json "$1" > /tmp/scale-manifest.json 2>/dev/null
    python3 -c "
import json,sys
m=json.load(open('/tmp/scale-manifest.json'))
want='$2'
ok = m['beyond_rpo']==0 and (want=='any' or len(m['entries'])>=int(want))
sys.exit(0 if ok else 1)" 2>/dev/null && { echo $((SECONDS - t0)); return 0; }
    [ $SECONDS -gt $d ] && return 1
    sleep 5
  done
}

# Sample a hub's peak RSS (KiB) in the background; peak_rss_stop echoes it.
peak_rss_start() { # $1=pid $2=outfile
  ( local mx=0 v
    while kill -0 "$1" 2>/dev/null; do
      v=$(ps -o rss= -p "$1" 2>/dev/null | tr -d ' ')
      [ -n "$v" ] && [ "$v" -gt "$mx" ] && mx=$v
      echo "$mx" > "$2"
      sleep 0.2
    done ) &
  echo $! > "$2.pid"
  disown # no job-termination notice when peak_rss_stop kills it
}
peak_rss_stop() { # $1=outfile
  kill "$(cat "$1.pid" 2>/dev/null)" 2>/dev/null
  cat "$1" 2>/dev/null || echo 0
}

cleanup() {
  set +e
  if [ "${KEEP:-0}" = "1" ]; then echo "KEEP=1 — leaving the rig standing"; return; fi
  umount_client 2>/dev/null
  pkill -9 -f "flint-pnfs-mds --config /tmp/flint-scale" 2>/dev/null
  rm -f /tmp/flint-scale-*.pid
  docker rm -f "$MINIO_NAME" >/dev/null 2>&1
}
trap cleanup EXIT

echo "══════════════════════════════════════════════════════════════════"
echo " S3-tier SCALE drill — FILES=$FILES, BIGMB=${BIGMB}MiB"
echo "══════════════════════════════════════════════════════════════════"

[ -x "$BIN_DIR/flint-pnfs-mds" ] || fail "missing $BIN_DIR/flint-pnfs-mds"
command -v docker >/dev/null || fail "docker not found"
command -v aws >/dev/null || fail "aws CLI not found"
limactl list --quiet 2>/dev/null | grep -qx "$LIMA_VM" || fail "Lima VM not running"
lsof -nP -iTCP:$PORT -sTCP:LISTEN >/dev/null 2>&1 && fail "port $PORT already held"
AVAIL_GB=$(df -g /tmp | tail -1 | awk '{print $4}')
[ "$AVAIL_GB" -ge $(( (BIGMB / 1024) * 2 + 4 )) ] \
  || fail "need ~$(( (BIGMB/1024)*2 + 4 )) GB free on /tmp, have ${AVAIL_GB} GB"

cleanup
rm -f /tmp/flint-scale-*.log
docker run -d --name "$MINIO_NAME" -p 127.0.0.1:$MINIO_PORT:9000 \
  -e MINIO_ROOT_USER=$MINIO_USER -e MINIO_ROOT_PASSWORD=$MINIO_PASS \
  quay.io/minio/minio server /data >/dev/null || fail "MinIO failed to start"
for _ in $(seq 1 30); do
  curl -sf "http://127.0.0.1:$MINIO_PORT/minio/health/live" >/dev/null && break; sleep 1
done
pass "MinIO healthy on :$MINIO_PORT"

# ══════════════════════════════════════════════════════════════════════
say "phase A: $FILES small files — capture → flush → manifest → DR import"
BK=flint-scale-files
E=/tmp/scale-a-exp S=/tmp/scale-a-st
s3 s3 mb "s3://$BK" >/dev/null
rm -rf "$E" "$S"; mkdir -p "$E" "$S"; chmod 0777 "$E"
gen_cfg /tmp/flint-scale-a.yaml $E $S $BK 99
launch_hub a /tmp/flint-scale-a.yaml
wait_bound 30 a
mount_client

PER_DIR=100
DIRS=$(( (FILES + PER_DIR - 1) / PER_DIR ))
T0=$SECONDS
vm "cd $MNT && n=0; d=0; \
    while [ \$n -lt $FILES ]; do \
      d=\$((d+1)); mkdir -p dir\$d; i=0; \
      while [ \$i -lt $PER_DIR ] && [ \$n -lt $FILES ]; do \
        i=\$((i+1)); n=\$((n+1)); echo payload-\$n > dir\$d/f\$n.txt; \
      done; \
    done; sync" || fail "bulk create failed"
CREATE_S=$((SECONDS - T0))
pass "created $FILES files in $DIRS dirs over NFS: ${CREATE_S}s ($((FILES / (CREATE_S + 1)))/s)"

SETTLE_A_S=$(settle_rpo $BK $FILES 900) \
  || { tail -20 /tmp/flint-scale-a.log; fail "never settled to beyond_rpo=0 with $FILES entries within 900s"; }
MSIZE=$(stat -f %z /tmp/scale-manifest.json)
ENTRIES=$(python3 -c "import json;print(len(json.load(open('/tmp/scale-manifest.json'))['entries']))")
pass "settled to RPO 0: ${SETTLE_A_S}s after create; manifest carries $ENTRIES entries in $((MSIZE / 1024)) KiB"

umount_client; stop_hub a
rm -rf "$E" "$S"; mkdir -p "$E" "$S"; chmod 0777 "$E"
T0=$SECONDS
launch_hub a2 /tmp/flint-scale-a.yaml
wait_bound 600 a2
IMPORT_S=$((SECONDS - T0))
STUBS=$(find "$E" -type f ! -name '.flint*' | wc -l | tr -d ' ')
[ "$STUBS" -eq "$FILES" ] || fail "DR import materialized $STUBS files, wanted $FILES"
grep -q "tier import" /tmp/flint-scale-a2.log || fail "no import log"
pass "DR import of $FILES objects: ${IMPORT_S}s to serving ($((FILES / (IMPORT_S + 1)))/s)"

mount_client
OK=1
for n in 1 $((FILES / 3)) $((FILES / 2)) $((FILES - 1)) $FILES; do
  d=$(( (n + PER_DIR - 1) / PER_DIR ))
  GOT=$(vm "cat $MNT/dir$d/f$n.txt" | tr -d '\r')
  [ "$GOT" = "payload-$n" ] || { echo "  spot-check f$n: got '$GOT'"; OK=0; }
done
[ "$OK" = "1" ] || fail "imported content spot-check failed"
pass "spot-checked 5 imported files through the client: content exact (hydrate-on-read)"
umount_client; stop_hub a2

# ══════════════════════════════════════════════════════════════════════
say "phase B: one ${BIGMB} MiB file — multipart publish, evict, ranged hydration"
BK=flint-scale-big
E=/tmp/scale-b-exp S=/tmp/scale-b-st
s3 s3 mb "s3://$BK" >/dev/null
rm -rf "$E" "$S"; mkdir -p "$E" "$S"; chmod 0777 "$E"
gen_cfg /tmp/flint-scale-b.yaml $E $S $BK 99
launch_hub b /tmp/flint-scale-b.yaml
wait_bound 30 b
mount_client

peak_rss_start "$(hub_pid b)" /tmp/scale-rss-write
T0=$SECONDS
vm "dd if=/dev/urandom of=$MNT/big.bin bs=1M count=$BIGMB conv=fsync 2>/dev/null" \
  || fail "big write failed"
WRITE_S=$((SECONDS - T0))
BMD5=$(md5 -q "$E/big.bin")
T0=$SECONDS
PUB_S=$(settle_rpo $BK any 600) || fail "big file never settled to RPO 0 within 600s"
RSS_W=$(peak_rss_stop /tmp/scale-rss-write)
pass "write ${WRITE_S}s ($((BIGMB / (WRITE_S + 1))) MiB/s over NFS); publish settled +${PUB_S}s; hub peak RSS $((RSS_W / 1024)) MiB"
[ "$RSS_W" -lt 1048576 ] || fail "hub RSS crossed 1 GiB during the multipart publish — streaming regression"

stop_hub b
gen_cfg /tmp/flint-scale-b50.yaml $E $S $BK 50
launch_hub b2 /tmp/flint-scale-b50.yaml
wait_bound 30 b2
d=$((SECONDS + 120))
until [ "$(stat -f %z "$E/big.bin" 2>/dev/null)" = "0" ]; do
  [ $SECONDS -gt $d ] && { tail -20 /tmp/flint-scale-b2.log; fail "big file never evicted"; }
  sleep 2
done
stop_hub b2
launch_hub b3 /tmp/flint-scale-b.yaml
wait_bound 30 b3
pass "evicted to a stub; hub restarted with eviction off for the hydration measurement"

vm "sync; echo 3 > /proc/sys/vm/drop_caches" 2>/dev/null
peak_rss_start "$(hub_pid b3)" /tmp/scale-rss-hyd
T0=$SECONDS
CMD5=$(vm "md5sum $MNT/big.bin | awk '{print \$1}'" | tr -d '\r')
HYD_S=$((SECONDS - T0))
RSS_H=$(peak_rss_stop /tmp/scale-rss-hyd)
[ "$CMD5" = "$BMD5" ] || fail "hydrated bytes diverged: $CMD5 ≠ $BMD5"
pass "cold read of ${BIGMB} MiB: ${HYD_S}s end to end ($((BIGMB / (HYD_S + 1))) MiB/s incl. hydration); hub peak RSS $((RSS_H / 1024)) MiB"
[ "$RSS_H" -lt 1048576 ] || fail "hub RSS crossed 1 GiB during hydration — streaming regression"
umount_client; stop_hub b3

echo
echo "══════════════════════════════════════════════════════════════════"
echo " PASS — scale record:"
echo "   $FILES files: create ${CREATE_S}s · settle ${SETTLE_A_S}s · manifest $((MSIZE / 1024)) KiB · DR import ${IMPORT_S}s"
echo "   ${BIGMB} MiB: write ${WRITE_S}s · publish ${PUB_S}s · cold read ${HYD_S}s · peak RSS write $((RSS_W / 1024)) MiB / hydrate $((RSS_H / 1024)) MiB"
echo "══════════════════════════════════════════════════════════════════"
