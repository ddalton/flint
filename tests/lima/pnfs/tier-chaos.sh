#!/usr/bin/env bash
#
# S3-tier CHAOS drill — hostile-world hardening on the MinIO rig.
#
# tier-drill.sh is the happy-path smoke of the assembled tier; this
# script is the hostile world. Each phase gets its OWN bucket, export
# tree, and state dir — no cross-phase contamination.
#
#   A  split-brain    — a second hub on the same bucket/prefix WAITS
#                       while the holder lives, takes over (epoch+1)
#                       once it is killed, and a foreign epoch
#                       overwrite deposes the incumbent (self-fence).
#   B  store outage   — a short MinIO outage is survived (writes keep
#                       ACKing — S3 down ≠ filesystem down); a long
#                       one self-fences after the lease window, and
#                       the restart self-recognizes + flushes the
#                       durable dirty backlog.
#   C  foreign hands  — the A6 412 split, end to end: a foreign
#                       overwrite of a LIVE file loses to local truth
#                       (publish lane, local-wins); a foreign
#                       overwrite of an EVICTED file's object is
#                       adopted (hydration lane, S3-wins) and served
#                       to the client.
#   D  space pressure — a real 256 MB volume: truthful client df,
#                       NOSPC admission before hard-full, and the
#                       watermark evict → hydrate loop against
#                       genuine fullness.
#   E  crash loop     — kill -9 the hub at random pipeline moments
#                       (multipart flushes in flight), N iterations;
#                       every ACKed byte must verify after every
#                       restart, the world must settle to
#                       beyond_rpo=0 with ZERO leftover MPUs, and a
#                       final DR rebuild must be byte-identical.
#   G  two writers   — a second netns client (own co_ownerid): concurrent
#                       writer batteries + close-to-open handoffs both
#                       ways + cross-client verification, all under
#                       constant evict/hydrate churn.
#   F  endurance      — sqlite+git battery under CONSTANT
#                       evict/hydrate churn (watermark always
#                       exceeded); ends with git fsck --strict,
#                       sqlite integrity_check, and a zero-tolerance
#                       log sweep.
#
# Knobs: CRASH_ITERS (default 5), ENDURE_SECS (default 60),
# PHASES ("a b c d e f"), KEEP=1 leaves the rig standing on failure.
# Runtime ≈ 8 minutes with defaults.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN_DIR="$REPO_ROOT/spdk-csi-driver/target/release"
LIMA_VM="${LIMA_VM:-flint-nfs-client}"

CRASH_ITERS="${CRASH_ITERS:-5}"
ENDURE_SECS="${ENDURE_SECS:-60}"
PHASES="${PHASES:-a b c d e f g}"

PORT=20492
PORT_B=20493
MNT=/mnt/chaos
MINIO_NAME=flint-tier-minio
MINIO_PORT=9000
MINIO_USER=flintdrill
MINIO_PASS=flintdrill123
PREFIX=vol1/
DMG=/tmp/flint-chaos.dmg
VOLNAME=flintchaos
VOL=/Volumes/$VOLNAME

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

# gen_cfg OUT PORT EXPORT STATEDIR BUCKET WATERMARK HB MISSES WHOLEMAX FLOOR TICK
gen_cfg() {
  cat > "$1" <<EOF
apiVersion: flint.io/v1alpha1
kind: PnfsConfig
mode: standalone
mds:
  bind: { address: "0.0.0.0", port: $2 }
  layout: { type: file, stripeSize: 8388608, policy: stripe }
  dataServers: []
  state:
    backend: sqlite
    config: { path: $4/state.db }
  tier:
    enabled: true
    bucket: $5
    keyPrefix: "$PREFIX"
    endpoint: "http://127.0.0.1:$MINIO_PORT"
    flushFloorSecs: ${10}
    quiesceSecs: 1
    tickSecs: ${11}
    epochHeartbeatSecs: $7
    epochLeaseMisses: $8
    wholePutMaxBytes: $9
    watermarkPct: $6
    reserveBytes: 33554432
    ballastBytes: 16777216
exports:
  - path: $3
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

launch_hub() { # $1=tag $2=cfg — no bind wait (a claim may lawfully block)
  env -u AWS_PROFILE \
    AWS_ACCESS_KEY_ID=$MINIO_USER AWS_SECRET_ACCESS_KEY=$MINIO_PASS \
    AWS_REGION=us-east-1 AWS_EC2_METADATA_DISABLED=true \
    FLINT_TIER_REPORT_SECS=5 \
    nohup "$BIN_DIR/flint-pnfs-mds" --config "$2" >>"/tmp/flint-chaos-$1.log" 2>&1 &
  echo $! >"/tmp/flint-chaos-$1.pid"
  disown
}

hub_pid()   { cat "/tmp/flint-chaos-$1.pid" 2>/dev/null; }
hub_alive() { kill -0 "$(hub_pid "$1")" 2>/dev/null; }
kill_hub()  { kill -9 "$(hub_pid "$1")" 2>/dev/null; rm -f "/tmp/flint-chaos-$1.pid"; }
stop_hub()  {
  kill "$(hub_pid "$1")" 2>/dev/null
  for _ in $(seq 1 20); do hub_alive "$1" || break; sleep 0.5; done
  kill -9 "$(hub_pid "$1")" 2>/dev/null; rm -f "/tmp/flint-chaos-$1.pid"
}

wait_bound() { # $1=port $2=deadline-secs $3=tag(for log tail)
  local d=$((SECONDS + $2))
  while [ $SECONDS -lt $d ]; do
    lsof -nP -iTCP:"$1" -sTCP:LISTEN >/dev/null 2>&1 && return 0
    sleep 1
  done
  tail -20 "/tmp/flint-chaos-$3.log"; fail "hub '$3' never bound :$1"
}

wait_key() { # $1=bucket $2=key $3=deadline-secs
  local d=$((SECONDS + $3))
  until s3 s3api head-object --bucket "$1" --key "$2" >/dev/null 2>&1; do
    [ $SECONDS -gt $d ] && return 1
    sleep 2
  done
}

mount_client() {
  vm "mountpoint -q $MNT && umount -lf $MNT; mkdir -p $MNT; \
      timeout 30 mount -t nfs4 -o minorversion=1,proto=tcp,port=$PORT \
        \$(getent hosts host.lima.internal | awk '{print \$1}'):/ $MNT" \
    || fail "client mount failed"
}
umount_client() { vm "umount -lf $MNT 2>/dev/null; true"; }

# Integrity sweep: patterns that must NEVER appear, phase-agnostic.
sweep_log() { # $1=log $2=phase-name
  if grep -nE "CRC mismatch|ChecksumMismatch|could not reset|panicked|DIVERGE" "$1"; then
    fail "$2: integrity-pattern found in $1 (above)"
  fi
}

fresh_world() { # $1=export $2=statedir
  rm -rf "$1" "$2"; mkdir -p "$1" "$2"; chmod 0777 "$1"
}

cleanup() {
  set +e
  if [ "${KEEP:-0}" = "1" ]; then
    echo "KEEP=1 — leaving the rig standing"
    return
  fi
  umount_client 2>/dev/null
  vm "umount -lf /mnt/chaos-b 2>/dev/null; ip netns del chaosb 2>/dev/null; \
      ip link del chaosb0 2>/dev/null; true" 2>/dev/null
  pkill -9 -f "flint-pnfs-mds --config /tmp/flint-chaos" 2>/dev/null
  rm -f /tmp/flint-chaos-*.pid
  docker rm -f "$MINIO_NAME" >/dev/null 2>&1
  hdiutil detach "$VOL" -force >/dev/null 2>&1
  rm -f "$DMG"
}
trap cleanup EXIT

echo "══════════════════════════════════════════════════════════════════"
echo " S3-tier CHAOS drill — phases: $PHASES (crash×$CRASH_ITERS, endure ${ENDURE_SECS}s)"
echo "══════════════════════════════════════════════════════════════════"

[ -x "$BIN_DIR/flint-pnfs-mds" ] || fail "missing $BIN_DIR/flint-pnfs-mds"
command -v docker >/dev/null || fail "docker not found"
command -v aws >/dev/null || fail "aws CLI not found"
limactl list --quiet 2>/dev/null | grep -qx "$LIMA_VM" || fail "Lima VM not running"
lsof -nP -iTCP:$PORT -sTCP:LISTEN >/dev/null 2>&1 && fail "port $PORT already held"

cleanup
rm -f /tmp/flint-chaos-*.log
docker run -d --name "$MINIO_NAME" -p 127.0.0.1:$MINIO_PORT:9000 \
  -e MINIO_ROOT_USER=$MINIO_USER -e MINIO_ROOT_PASSWORD=$MINIO_PASS \
  quay.io/minio/minio server /data >/dev/null || fail "MinIO failed to start"
for _ in $(seq 1 30); do
  curl -sf "http://127.0.0.1:$MINIO_PORT/minio/health/live" >/dev/null && break; sleep 1
done
pass "MinIO healthy"

# ══════════════════════════════════════════════════════════════════════
phase_a() {
  say "phase A: split-brain — two hubs, one bucket"
  local BK=flint-chaos-a EA=/tmp/chaos-a-exp-a SA=/tmp/chaos-a-st-a
  local EB=/tmp/chaos-a-exp-b SB=/tmp/chaos-a-st-b
  s3 s3 mb "s3://$BK" >/dev/null
  fresh_world $EA $SA; fresh_world $EB $SB

  gen_cfg /tmp/flint-chaos-a1.yaml $PORT $EA $SA $BK 99 2 3 67108864 2 2
  launch_hub a1 /tmp/flint-chaos-a1.yaml
  wait_bound $PORT 30 a1
  grep -q "epoch 1 held" /tmp/flint-chaos-a1.log || fail "A never held epoch 1"
  pass "hub A holds epoch 1"

  gen_cfg /tmp/flint-chaos-a2.yaml $PORT_B $EB $SB $BK 99 2 3 67108864 2 2
  launch_hub a2 /tmp/flint-chaos-a2.yaml
  sleep 8
  hub_alive a2 || { tail -20 /tmp/flint-chaos-a2.log; fail "hub B died instead of waiting"; }
  lsof -nP -iTCP:$PORT_B -sTCP:LISTEN >/dev/null 2>&1 \
    && fail "hub B bound its listener while A held the epoch — SPLIT BRAIN"
  grep -qE "tier: epoch [0-9]+ held" /tmp/flint-chaos-a2.log \
    && fail "hub B claims to hold while A lives"
  grep -q "watching its lease" /tmp/flint-chaos-a2.log \
    || fail "hub B never logged the lease watch"
  pass "hub B waits: no listener, no lease, while the holder heartbeats"

  kill_hub a1
  local d=$((SECONDS + 30))
  until grep -q "tier: epoch 2 held" /tmp/flint-chaos-a2.log; do
    [ $SECONDS -gt $d ] && { tail -20 /tmp/flint-chaos-a2.log; fail "B never took over"; }
    sleep 1
  done
  wait_bound $PORT_B 15 a2
  pass "B judged the dead holder by the store's clock and took epoch 2"

  # The old holder returns: it must WAIT (B is live), never resume.
  launch_hub a1b /tmp/flint-chaos-a1.yaml
  sleep 8
  hub_alive a1b || { tail -20 /tmp/flint-chaos-a1b.log; fail "returned A died instead of waiting"; }
  lsof -nP -iTCP:$PORT -sTCP:LISTEN >/dev/null 2>&1 \
    && fail "returned hub A bound while B holds — SPLIT BRAIN"
  pass "the deposed incumbent waits behind the live holder"
  kill_hub a1b

  # Foreign epoch overwrite ⇒ the incumbent's next CAS renew 412s ⇒
  # self-fence + exit.
  echo garbage-epoch > /tmp/chaos-epoch-garbage
  s3 s3 cp /tmp/chaos-epoch-garbage "s3://$BK/${PREFIX}.flint/epoch" >/dev/null
  d=$((SECONDS + 15))
  while hub_alive a2; do
    [ $SECONDS -gt $d ] && fail "B survived a foreign epoch overwrite"
    sleep 1
  done
  grep -q "DEPOSED" /tmp/flint-chaos-a2.log || fail "B exited without logging DEPOSED"
  pass "foreign epoch overwrite ⇒ 412 on renew ⇒ self-fence ⇒ exit"
  sweep_log /tmp/flint-chaos-a1.log "phase A"; sweep_log /tmp/flint-chaos-a2.log "phase A"
}

# ══════════════════════════════════════════════════════════════════════
phase_b() {
  say "phase B: store outage — short survives, long self-fences + resumes"
  local BK=flint-chaos-b E=/tmp/chaos-b-exp S=/tmp/chaos-b-st
  s3 s3 mb "s3://$BK" >/dev/null
  fresh_world $E $S
  gen_cfg /tmp/flint-chaos-b.yaml $PORT $E $S $BK 99 2 5 67108864 2 2
  launch_hub b /tmp/flint-chaos-b.yaml
  wait_bound $PORT 30 b
  mount_client
  vm "dd if=/dev/urandom of=$MNT/pre.bin bs=1M count=2 conv=fsync 2>/dev/null"
  wait_key $BK "${PREFIX}pre.bin" 40 || fail "pre.bin never published"
  pass "baseline file published"

  docker stop -t 2 "$MINIO_NAME" >/dev/null
  vm "echo written-during-outage > $MNT/during-short.txt && sync" \
    || fail "a write FAILED during the S3 outage — S3 down must not take the filesystem down"
  sleep 3
  docker start "$MINIO_NAME" >/dev/null
  for _ in $(seq 1 30); do
    curl -sf "http://127.0.0.1:$MINIO_PORT/minio/health/live" >/dev/null && break; sleep 1
  done
  hub_alive b || fail "hub died on a SHORT outage (inside the lease window)"
  grep -q "self-fencing" /tmp/flint-chaos-b.log && fail "short outage self-fenced"
  wait_key $BK "${PREFIX}during-short.txt" 40 || fail "outage-written file never flushed"
  pass "short outage: writes kept ACKing, hub survived, backlog flushed on return"

  docker stop -t 2 "$MINIO_NAME" >/dev/null
  vm "echo written-during-long-outage > $MNT/during-long.txt && sync" \
    || fail "a write FAILED during the long outage"
  local d=$((SECONDS + 40))
  while hub_alive b; do
    [ $SECONDS -gt $d ] && fail "hub survived a full lease window without the store"
    sleep 1
  done
  grep -q "self-fencing" /tmp/flint-chaos-b.log \
    || fail "hub exited without the lease-window self-fence log"
  pass "long outage: a full lease window of failed renews ⇒ self-fence ⇒ exit"

  docker start "$MINIO_NAME" >/dev/null
  for _ in $(seq 1 30); do
    curl -sf "http://127.0.0.1:$MINIO_PORT/minio/health/live" >/dev/null && break; sleep 1
  done
  launch_hub b2 /tmp/flint-chaos-b.yaml
  wait_bound $PORT 40 b2
  wait_key $BK "${PREFIX}during-long.txt" 60 \
    || fail "the durable dirty backlog did not flush after recovery"
  vm "md5sum $MNT/pre.bin >/dev/null" || fail "client mount did not survive the bounce"
  pass "restart self-recognized, durable backlog flushed, client mount alive"
  umount_client; stop_hub b2
  sweep_log /tmp/flint-chaos-b.log "phase B"; sweep_log /tmp/flint-chaos-b2.log "phase B"
}

# ══════════════════════════════════════════════════════════════════════
phase_c() {
  say "phase C: foreign hands in the bucket — the A6 412 split, live"
  local BK=flint-chaos-c E=/tmp/chaos-c-exp S=/tmp/chaos-c-st
  s3 s3 mb "s3://$BK" >/dev/null
  fresh_world $E $S
  gen_cfg /tmp/flint-chaos-c.yaml $PORT $E $S $BK 99 2 3 67108864 2 2
  launch_hub c /tmp/flint-chaos-c.yaml
  wait_bound $PORT 30 c
  mount_client

  # C1 — publish lane, LOCAL WINS on a live file.
  vm "echo 'local truth v1' > $MNT/contested.txt && sync"
  wait_key $BK "${PREFIX}contested.txt" 40 || fail "contested.txt never published"
  echo "FOREIGN OVERWRITE" > /tmp/chaos-foreign.txt
  s3 s3 cp /tmp/chaos-foreign.txt "s3://$BK/${PREFIX}contested.txt" >/dev/null
  vm "echo 'local truth v2' >> $MNT/contested.txt && sync"
  local d=$((SECONDS + 60)) want got
  want=$(md5 -q "$E/contested.txt")
  while :; do
    s3 s3 cp "s3://$BK/${PREFIX}contested.txt" /tmp/chaos-got.txt >/dev/null 2>&1
    got=$(md5 -q /tmp/chaos-got.txt 2>/dev/null)
    [ "$got" = "$want" ] && break
    [ $SECONDS -gt $d ] && { tail -20 /tmp/flint-chaos-c.log; fail "publish lane never re-won the key from the foreign overwrite"; }
    sleep 2
  done
  pass "C1 publish lane: foreign overwrite of a LIVE file lost to local truth (guarded re-publish)"

  # C3 — foreign DELETE of a LIVE file's object: the next local
  # mutation re-publishes local truth (create-flavor; the base is
  # gone and arbitration says retry-from-base). MUST run on this
  # no-eviction hub: under churn the file would evict first, and a
  # foreign delete under an EVICTED file is the C4 park-forever
  # posture — an append would (correctly) hang the drill.
  vm "echo 'phoenix v1' > $MNT/phoenix.txt && sync"
  wait_key $BK "${PREFIX}phoenix.txt" 40 || fail "phoenix.txt never published"
  s3 s3 rm "s3://$BK/${PREFIX}phoenix.txt" >/dev/null
  vm "echo 'phoenix v2' >> $MNT/phoenix.txt && sync"
  local d15=$((SECONDS + 60)) pwant pgot
  pwant=$(md5 -q "$E/phoenix.txt")
  while :; do
    s3 s3 cp "s3://$BK/${PREFIX}phoenix.txt" /tmp/chaos-phx.txt >/dev/null 2>&1 \
      && pgot=$(md5 -q /tmp/chaos-phx.txt 2>/dev/null) && [ "$pgot" = "$pwant" ] && break
    [ $SECONDS -gt $d15 ] && { tail -20 /tmp/flint-chaos-c.log; fail "object never re-published after a foreign DELETE"; }
    sleep 2
  done
  pass "C3 publish lane: foreign DELETE of a live file's object ⇒ local truth re-published"

  # C2 — hydration lane, S3 WINS on an evicted file's object.
  vm "dd if=/dev/urandom of=$MNT/adopted.bin bs=1M count=1 conv=fsync 2>/dev/null"
  wait_key $BK "${PREFIX}adopted.bin" 40 || fail "adopted.bin never published"
  stop_hub c
  gen_cfg /tmp/flint-chaos-c2.yaml $PORT $E $S $BK 50 2 3 67108864 2 2
  launch_hub c2 /tmp/flint-chaos-c2.yaml
  wait_bound $PORT 30 c2
  d=$((SECONDS + 60))
  until [ "$(stat -f %z "$E/adopted.bin" 2>/dev/null)" = "0" ]; do
    [ $SECONDS -gt $d ] && { tail -20 /tmp/flint-chaos-c2.log; fail "adopted.bin never evicted"; }
    sleep 2
  done

  dd if=/dev/urandom of=/tmp/chaos-adopt-src.bin bs=1k count=700 2>/dev/null
  local FMD5; FMD5=$(md5 -q /tmp/chaos-adopt-src.bin)
  s3 s3 cp /tmp/chaos-adopt-src.bin "s3://$BK/${PREFIX}adopted.bin" >/dev/null
  # The FIRST read triggers DELAY → 412 → adopt → restore; its own
  # view may mix the open-time logical size with the adopted bytes
  # (standard close-to-open: attributes are from open time). Absorb
  # it, then assert the SERVER's file became the foreign truth, then
  # assert a FRESH open serves it end to end.
  vm "sync; echo 3 > /proc/sys/vm/drop_caches" 2>/dev/null
  vm "md5sum $MNT/adopted.bin >/dev/null 2>&1; true"
  local d2=$((SECONDS + 45))
  until grep -q "S3-wins: adopting" /tmp/flint-chaos-c2.log; do
    [ $SECONDS -gt $d2 ] && { tail -20 /tmp/flint-chaos-c2.log; fail "the server never adopted the foreign object (S3-wins)"; }
    sleep 2
  done
  vm "sync; echo 3 > /proc/sys/vm/drop_caches" 2>/dev/null
  local CMD5; CMD5=$(vm "md5sum $MNT/adopted.bin | awk '{print \$1}'" | tr -d '\r')
  [ "$CMD5" = "$FMD5" ] \
    || fail "a fresh client open served $CMD5, wanted the FOREIGN $FMD5 (S3-wins)"
  pass "C2 hydration lane: 412 mid-restore ⇒ the bucket's CURRENT object adopted and served (S3-wins)"

  # C4 — foreign DELETE under an EVICTED file: the bytes exist
  # NOWHERE. The honest posture is refusal-never-loss: reads PARK
  # (DELAY-retry) rather than serving zeros; recovery is bucket
  # versioning (A9's recommendation) or operator action.
  vm "dd if=/dev/urandom of=$MNT/orphaned.bin bs=1k count=600 conv=fsync 2>/dev/null"
  wait_key $BK "${PREFIX}orphaned.bin" 40 || fail "orphaned.bin never published"
  local d16=$((SECONDS + 60))
  until [ "$(stat -f %z "$E/orphaned.bin" 2>/dev/null)" = "0" ]; do
    [ $SECONDS -gt $d16 ] && fail "orphaned.bin never evicted"
    sleep 2
  done
  s3 s3 rm "s3://$BK/${PREFIX}orphaned.bin" >/dev/null
  vm "sync; echo 3 > /proc/sys/vm/drop_caches" 2>/dev/null
  vm "timeout 8 cat $MNT/orphaned.bin > /tmp/chaos-orphan-read.out 2>/dev/null"
  local RC=$?
  [ $RC -eq 124 ] || fail "read of the orphaned stub returned rc=$RC — it must PARK, never serve"
  vm "[ ! -s /tmp/chaos-orphan-read.out ]" \
    || fail "the orphaned stub served BYTES — refusal-never-loss violated"
  hub_alive c2 || fail "hub died on the orphaned stub"
  pass "C4: foreign DELETE under an evicted file ⇒ reads park forever, ZERO wrong bytes served"

  umount_client; stop_hub c2
  sweep_log /tmp/flint-chaos-c.log "phase C"; sweep_log /tmp/flint-chaos-c2.log "phase C"
}

# ══════════════════════════════════════════════════════════════════════
phase_d() {
  say "phase D: real space pressure — a 256 MB volume"
  local BK=flint-chaos-d
  s3 s3 mb "s3://$BK" >/dev/null
  hdiutil detach "$VOL" -force >/dev/null 2>&1; rm -f "$DMG"
  hdiutil create -size 256m -fs "HFS+" -volname $VOLNAME "$DMG" >/dev/null \
    || fail "hdiutil create failed"
  hdiutil attach "$DMG" >/dev/null || fail "hdiutil attach failed"
  local E=$VOL/export S=$VOL/state
  mkdir -p "$E" "$S"; chmod 0777 "$E"
  gen_cfg /tmp/flint-chaos-d.yaml $PORT $E $S $BK 60 2 3 67108864 1 1
  launch_hub d /tmp/flint-chaos-d.yaml
  wait_bound $PORT 30 d
  mount_client

  [ "$(stat -f %z "$S/flint-ballast.bin" 2>/dev/null)" = "16777216" ] \
    || fail "ballast not preallocated next to state.db"
  local TOT; TOT=$(vm "df -k $MNT | tail -1 | awk '{print \$2}'" | tr -d '\r')
  [ "$TOT" -gt 100000 ] && [ "$TOT" -lt 300000 ] \
    || fail "client df total is ${TOT}K — SPACE_* attrs are not serving the real volume"
  pass "ballast armed; client df sees the REAL 256 MB volume (${TOT}K), not 8 EiB"

  vm "dd if=/dev/zero of=$MNT/huge.bin bs=1M count=300 conv=fsync" 2>/tmp/chaos-dd.err
  local RC=$?
  [ $RC -ne 0 ] || fail "a 300 MB write onto a 256 MB volume SUCCEEDED?"
  grep -qi "space" /tmp/chaos-dd.err \
    || fail "dd failed without a no-space errno: $(tail -2 /tmp/chaos-dd.err)"
  hub_alive d || fail "hub died under NOSPC pressure (F55 shape)"
  vm "rm -f $MNT/huge.bin"
  pass "NOSPC admission refused the overrun before hard-full; hub healthy"

  vm "for i in \$(seq 1 10); do dd if=/dev/urandom of=$MNT/fill\$i.bin bs=1M count=16 conv=fsync 2>/dev/null; done" \
    || fail "fill writes failed"
  local MD5F; MD5F=$(vm "md5sum $MNT/fill3.bin | awk '{print \$1}'" | tr -d '\r')
  local d2=$((SECONDS + 120)) STUBS=0
  while :; do
    STUBS=$(find "$E" -maxdepth 1 -name 'fill*.bin' -size 0 2>/dev/null | wc -l | tr -d ' ')
    [ "$STUBS" -ge 1 ] && break
    [ $SECONDS -gt $d2 ] && { tail -20 /tmp/flint-chaos-d.log; fail "watermark pass never evicted on the full volume"; }
    sleep 2
  done
  grep -q "watermark pass evicted" /tmp/flint-chaos-d.log \
    || fail "no watermark-pass log despite stubs"
  pass "watermark eviction freed real space ($STUBS stub(s) so far)"

  vm "sync; echo 3 > /proc/sys/vm/drop_caches" 2>/dev/null
  local MD5B; MD5B=$(vm "md5sum $MNT/fill3.bin | awk '{print \$1}'" | tr -d '\r')
  [ "$MD5B" = "$MD5F" ] || fail "fill3.bin changed across evict/hydrate: $MD5B ≠ $MD5F"
  pass "evict → hydrate round trip byte-identical on the pressured volume"
  umount_client; stop_hub d
  sweep_log /tmp/flint-chaos-d.log "phase D"
  hdiutil detach "$VOL" -force >/dev/null 2>&1; rm -f "$DMG"
}

# ══════════════════════════════════════════════════════════════════════
phase_e() {
  say "phase E: crash loop — kill -9 ×$CRASH_ITERS at random pipeline moments"
  local BK=flint-chaos-e E=/tmp/chaos-e-exp S=/tmp/chaos-e-st
  s3 s3 mb "s3://$BK" >/dev/null
  fresh_world $E $S
  # 4 MiB whole-put ceiling ⇒ the 8 MiB batch file takes the MULTIPART
  # path: kills land on in-flight MPUs, intents, and Complete windows.
  gen_cfg /tmp/flint-chaos-e.yaml $PORT $E $S $BK 99 2 5 4194304 1 1
  launch_hub e /tmp/flint-chaos-e.yaml
  wait_bound $PORT 30 e
  mount_client

  for i in $(seq 1 "$CRASH_ITERS"); do
    vm "dd if=/dev/urandom of=$MNT/big_$i.bin bs=1M count=8 conv=fsync 2>/dev/null && \
        echo batch-$i > $MNT/small_$i.txt && \
        echo line-$i >> $MNT/rolling.txt && \
        { [ -f $MNT/small_$((i-1)).txt ] && mv $MNT/small_$((i-1)).txt $MNT/moved_$((i-1)).txt; true; } && \
        { [ -f $MNT/big_$((i-2)).bin ] && rm $MNT/big_$((i-2)).bin; true; } && \
        sync && cd $MNT && find . -type f -exec md5sum {} + > /tmp/chaos-expect.txt" \
      || fail "iteration $i batch failed"
    sleep $((RANDOM % 4))
    kill_hub e
    launch_hub e /tmp/flint-chaos-e.yaml
    wait_bound $PORT 40 e
    vm "cd $MNT && md5sum --quiet -c /tmp/chaos-expect.txt" \
      || fail "iteration $i: ACKed content diverged after kill -9 + restart"
    echo "  · iteration $i: killed mid-pipeline, restarted, all ACKed bytes verified"
  done
  pass "$CRASH_ITERS kill -9 cycles: every ACKed byte survived every crash"

  # Settle: the manifest's own RPO statement must reach zero.
  local d=$((SECONDS + 120))
  while :; do
    s3 s3 cp "s3://$BK/${PREFIX}.flint/manifest" /tmp/chaos-manifest.json >/dev/null 2>&1
    python3 -c "
import json,sys
m=json.load(open('/tmp/chaos-manifest.json'))
sys.exit(0 if m['beyond_rpo']==0 else 1)" 2>/dev/null && break
    [ $SECONDS -gt $d ] && { tail -20 /tmp/flint-chaos-e.log; fail "world never settled to beyond_rpo=0"; }
    sleep 3
  done
  # A LIVE re-publish can still hold an MPU at settle time (its file
  # already has a gen row, so beyond_rpo=0 does not wait for it) —
  # poll until quiet; only what PERSISTS is an orphan.
  local MPUS OUT d3=$((SECONDS + 45))
  while :; do
    OUT=$(s3 s3api list-multipart-uploads --bucket "$BK" --output json 2>/dev/null)
    if [ -z "$OUT" ]; then MPUS=0; else
      MPUS=$(printf '%s' "$OUT" | python3 -c \
        "import json,sys; print(len(json.load(sys.stdin).get('Uploads') or []))")
    fi
    [ "$MPUS" = "0" ] && break
    [ $SECONDS -gt $d3 ] && {
      printf '%s' "$OUT" | python3 -c \
        "import json,sys; [print(' ', u['Key'], u.get('Initiated')) for u in json.load(sys.stdin).get('Uploads') or []]"
      fail "$MPUS orphaned MPU(s) persisted after the crash loop — the abort-sweep missed"
    }
    sleep 3
  done
  pass "settled to beyond_rpo=0 with ZERO surviving multipart uploads"

  umount_client; stop_hub e
  rm -rf "$E" "$S"; mkdir -p "$E" "$S"; chmod 0777 "$E"
  launch_hub e2 /tmp/flint-chaos-e.yaml
  wait_bound $PORT 40 e2
  grep -q "tier import" /tmp/flint-chaos-e2.log || fail "DR import never ran"
  mount_client
  vm "cd $MNT && md5sum --quiet -c /tmp/chaos-expect.txt" \
    || fail "post-DR content diverged from the last ACKed state"
  pass "DR after the crash loop: every byte rebuilt from the bucket, byte-identical"
  umount_client; stop_hub e2
  sweep_log /tmp/flint-chaos-e.log "phase E"; sweep_log /tmp/flint-chaos-e2.log "phase E"
}

# ══════════════════════════════════════════════════════════════════════
phase_f() {
  say "phase F: endurance — sqlite+git under constant evict/hydrate churn (${ENDURE_SECS}s)"
  local BK=flint-chaos-f E=/tmp/chaos-f-exp S=/tmp/chaos-f-st
  s3 s3 mb "s3://$BK" >/dev/null
  fresh_world $E $S
  gen_cfg /tmp/flint-chaos-f.yaml $PORT $E $S $BK 50 2 3 67108864 1 1
  launch_hub f /tmp/flint-chaos-f.yaml
  wait_bound $PORT 30 f
  mount_client

  vm "cd $MNT && mkdir -p repo && cd repo && git init -q && \
      git config user.email d@d && git config user.name d"
  vm "END=\$((\$(date +%s) + $ENDURE_SECS)); i=0; \
      cd $MNT && \
      while [ \$(date +%s) -lt \$END ]; do \
        i=\$((i+1)); \
        sqlite3 churn.db 'create table if not exists t(i integer, v text); insert into t values ('\$i', hex(randomblob(64)));' || exit 1; \
        ( cd repo && echo rev-\$i > f.txt && git add f.txt && git commit -qm c\$i ) || exit 1; \
        dd if=/dev/urandom of=blob.bin bs=1M count=2 conv=fsync 2>/dev/null || exit 1; \
        sleep 0.3; \
      done; echo \$i > /tmp/chaos-endure-iters" \
    || fail "endurance battery aborted mid-run"
  local ITERS; ITERS=$(vm "cat /tmp/chaos-endure-iters" | tr -d '\r')
  vm "sync; echo 3 > /proc/sys/vm/drop_caches" 2>/dev/null
  vm "cd $MNT/repo && git fsck --strict >/dev/null 2>&1" \
    || fail "git fsck failed after churn"
  vm "cd $MNT && sqlite3 churn.db 'pragma integrity_check;' | grep -q ok" \
    || fail "sqlite integrity_check failed after churn"
  local COUNT; COUNT=$(vm "cd $MNT && sqlite3 churn.db 'select count(*) from t;'" | tr -d '\r')
  [ "$COUNT" = "$ITERS" ] || fail "sqlite rows $COUNT ≠ $ITERS iterations"
  grep -qE "hydrate.*attempt .* failed" /tmp/flint-chaos-f.log \
    && fail "hydration failures during endurance (store was healthy)"
  grep -q "self-fencing\|DEPOSED" /tmp/flint-chaos-f.log \
    && fail "epoch trouble during endurance"
  local EV; EV=$(grep -c "watermark pass evicted" /tmp/flint-chaos-f.log)
  pass "$ITERS battery iterations under churn ($EV evict passes): git fsck + sqlite integrity clean"
  umount_client; stop_hub f
  sweep_log /tmp/flint-chaos-f.log "phase F"
}

# ══════════════════════════════════════════════════════════════════════
phase_g() {
  say "phase G: concurrent writers — TWO distinct clients under churn"
  local BK=flint-chaos-g E=/tmp/chaos-g-exp S=/tmp/chaos-g-st
  local NETNS=chaosb VH=chaosb0 VN=chaosb1
  local NS_NET=10.99.79.0/30 NS_GW=10.99.79.1 NS_IP=10.99.79.2
  local MNT_B=/mnt/chaos-b ITERS=20
  s3 s3 mb "s3://$BK" >/dev/null
  fresh_world $E $S
  gen_cfg /tmp/flint-chaos-g.yaml $PORT $E $S $BK 50 2 3 67108864 1 1
  launch_hub g /tmp/flint-chaos-g.yaml
  wait_bound $PORT 30 g
  mount_client

  # Client B = separate netns + UTS hostname (its own nfs_net and
  # co_ownerid — the lite-drill harness; the only honest second
  # client one VM allows).
  local HOST_IP
  HOST_IP=$(vm "getent hosts host.lima.internal | awk '{print \$1}'" | tr -d '\r')
  vm "ip netns del $NETNS 2>/dev/null; ip link del $VH 2>/dev/null; true"
  vm "ip netns add $NETNS && \
      ip link add $VH type veth peer name $VN && \
      ip link set $VN netns $NETNS && \
      ip addr add $NS_GW/30 dev $VH && ip link set $VH up && \
      ip netns exec $NETNS ip addr add $NS_IP/30 dev $VN && \
      ip netns exec $NETNS ip link set $VN up && \
      ip netns exec $NETNS ip link set lo up && \
      ip netns exec $NETNS ip route add default via $NS_GW && \
      sysctl -qw net.ipv4.ip_forward=1 && \
      { iptables -C FORWARD -s $NS_NET -j ACCEPT 2>/dev/null || iptables -I FORWARD -s $NS_NET -j ACCEPT; } && \
      { iptables -C FORWARD -d $NS_NET -j ACCEPT 2>/dev/null || iptables -I FORWARD -d $NS_NET -j ACCEPT; } && \
      { iptables -t nat -C POSTROUTING -s $NS_NET -j MASQUERADE 2>/dev/null || \
        iptables -t nat -A POSTROUTING -s $NS_NET -j MASQUERADE; }" \
    || fail "netns plumbing failed"
  vm "mkdir -p $MNT_B"
  vm "nsenter --net=/var/run/netns/$NETNS unshare --uts sh -c ' \
        hostname chaos-cluster-b && \
        timeout 30 mount -t nfs4 -o minorversion=1,proto=tcp,port=$PORT \
          $HOST_IP:/ $MNT_B'" || fail "client B mount failed"
  pass "two distinct kernel clients mounted (root netns + $NETNS)"

  # Both write CONCURRENTLY for $ITERS iterations while the watermark
  # pass evicts everything clean behind them.
  vm "cd $MNT && i=0; while [ \$i -lt $ITERS ]; do i=\$((i+1)); \
        sqlite3 a.db 'create table if not exists t(i integer); insert into t values ('\$i');' || exit 1; \
        dd if=/dev/urandom of=blob-a.bin bs=1M count=1 conv=fsync 2>/dev/null || exit 1; \
      done" >/tmp/chaos-ga.out 2>&1 &
  local GA=$!
  vm "nsenter --net=/var/run/netns/$NETNS sh -c 'cd $MNT_B && \
        mkdir -p repo-b && cd repo-b && git init -q 2>/dev/null; \
        git config user.email b@b; git config user.name b; \
        i=0; while [ \$i -lt $ITERS ]; do i=\$((i+1)); \
          echo rev-\$i > g.txt && git add g.txt && git commit -qm c\$i || exit 1; \
          dd if=/dev/urandom of=../blob-b.bin bs=1M count=1 conv=fsync 2>/dev/null || exit 1; \
        done'" >/tmp/chaos-gb.out 2>&1 &
  local GB=$!
  wait $GA; local RA=$?
  wait $GB; local RB=$?
  [ $RA -eq 0 ] || { cat /tmp/chaos-ga.out; fail "writer A aborted"; }
  [ $RB -eq 0 ] || { cat /tmp/chaos-gb.out; fail "writer B aborted"; }
  pass "both writers completed $ITERS iterations each, concurrently, under churn"

  # Close-to-open handoffs A→B and B→A under churn.
  local i tok
  for i in 1 2 3; do
    vm "echo handoff-a\$RANDOM-$i > $MNT/token.txt && sync" >/dev/null
    tok=$(vm "cat $MNT/token.txt" | tr -d '\r')
    got=$(vm "nsenter --net=/var/run/netns/$NETNS cat $MNT_B/token.txt" | tr -d '\r')
    [ "$tok" = "$got" ] || fail "A→B handoff $i: B read '$got', wanted '$tok'"
    vm "nsenter --net=/var/run/netns/$NETNS sh -c 'echo handoff-b-$i > $MNT_B/token.txt && sync'" >/dev/null
    tok=$(vm "nsenter --net=/var/run/netns/$NETNS cat $MNT_B/token.txt" | tr -d '\r')
    got=$(vm "cat $MNT/token.txt" | tr -d '\r')
    [ "$tok" = "$got" ] || fail "B→A handoff $i: A read '$got', wanted '$tok'"
  done
  pass "close-to-open held in BOTH directions ×3 under churn"

  # Cross-client integrity: each side verifies the OTHER's work.
  vm "nsenter --net=/var/run/netns/$NETNS sh -c 'cd $MNT_B && sqlite3 a.db \"select count(*) from t;\"'" \
    | tr -d '\r' | grep -qx "$ITERS" || fail "B's view of A's sqlite rows wrong"
  vm "cd $MNT/repo-b && git log --oneline | wc -l" | tr -d '\r' | grep -qx "$ITERS" \
    || fail "A's view of B's git history wrong"
  local BA BB
  BA=$(vm "md5sum $MNT/blob-b.bin | awk '{print \$1}'" | tr -d '\r')
  BB=$(vm "nsenter --net=/var/run/netns/$NETNS md5sum $MNT_B/blob-b.bin | awk '{print \$1}'" | tr -d '\r')
  [ "$BA" = "$BB" ] || fail "blob-b differs across clients: $BA ≠ $BB"
  local EV; EV=$(grep -c "watermark pass evicted" /tmp/flint-chaos-g.log)
  pass "cross-client verification clean ($EV evict passes ran underneath)"

  vm "umount -lf $MNT_B 2>/dev/null; ip netns del $NETNS 2>/dev/null; \
      ip link del $VH 2>/dev/null; true"
  umount_client; stop_hub g
  sweep_log /tmp/flint-chaos-g.log "phase G"
}

for ph in $PHASES; do
  case "$ph" in
    a) phase_a;; b) phase_b;; c) phase_c;;
    d) phase_d;; e) phase_e;; f) phase_f;; g) phase_g;;
    *) fail "unknown phase '$ph'";;
  esac
done

echo
echo "══════════════════════════════════════════════════════════════════"
echo " PASS — split-brain, outage, foreign hands, space pressure,"
echo " kill -9 crash loops, and endurance churn all held"
echo "══════════════════════════════════════════════════════════════════"
