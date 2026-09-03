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
#   H  zombie hub     — SIGSTOP the holder past its lease (the GC-pause
#                       split brain): a successor takes over while the
#                       incumbent is UNCONSCIOUS; on SIGCONT the zombie
#                       wakes with a dirty backlog and a stale epoch —
#                       it must self-fence and exit with NOTHING of its
#                       backlog leaking into the bucket.
#   I  neighbor prefix — two tenants (prefixes) on ONE bucket: a claim's
#                       MPU abort-sweep must fence its OWN prefix and
#                       never the neighbor's in-flight upload (the
#                       bucket-wide-list + code-filter blast radius);
#                       DR import must stay inside the tenant's prefix.
#   J  versioned recovery — C4's park-forever posture is RECOVERABLE
#                       when A9's advice was taken: on a versioned
#                       bucket, removing the foreign DELETE's marker
#                       unparks the SAME blocked reader with the
#                       original bytes — the operator runbook, drilled.
#   K  restart storm  — incarnations killed at 0.05–1.5 s lifetimes
#                       (some die before the claim completes) must
#                       still converge the durable backlog; then a
#                       kill -9 landed MID-HYDRATION (caught by size,
#                       partial bytes on disk): the reconciler must
#                       disambiguate via the durable hydrating flag,
#                       truncate back, and the parked reader completes.
#   L  degraded network — S3 fails SOFT through toxiproxy (brew install
#                       toxiproxy): publishes under 60% mid-stream
#                       connection cuts, hydration with EVERY
#                       connection cut after 16 MiB, 750 ms latency
#                       both ways (no spurious self-fence), and a full
#                       25 s stall that must lift without a wedge.
#
# Knobs: CRASH_ITERS (default 5), ENDURE_SECS (default 60),
# PHASES ("a b c d e f g h i j k l"), KEEP=1 leaves the rig standing on
# failure. Runtime ≈ 17 minutes with defaults.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN_DIR="$REPO_ROOT/spdk-csi-driver/target/release"
LIMA_VM="${LIMA_VM:-flint-nfs-client}"

CRASH_ITERS="${CRASH_ITERS:-5}"
ENDURE_SECS="${ENDURE_SECS:-60}"
PHASES="${PHASES:-a b c d e f g h i j k l}"

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

# gen_cfg OUT PORT EXPORT STATEDIR BUCKET WATERMARK HB MISSES WHOLEMAX FLOOR TICK [KEYPREFIX] [ENDPOINT]
gen_cfg() {
  local PFX="${12:-$PREFIX}"
  local EP="${13:-http://127.0.0.1:$MINIO_PORT}"
  cat > "$1" <<EOF
apiVersion: chert.us/v1alpha1
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
    keyPrefix: "$PFX"
    endpoint: "$EP"
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

mount_client() { # [port] — defaults to $PORT
  local P="${1:-$PORT}"
  vm "mountpoint -q $MNT && umount -lf $MNT; mkdir -p $MNT; \
      timeout 30 mount -t nfs4 -o minorversion=1,proto=tcp,port=$P \
        \$(getent hosts host.lima.internal | awk '{print \$1}'):/ $MNT" \
    || fail "client mount failed"
}
umount_client() { vm "umount -lf $MNT 2>/dev/null; true"; }

mpu_count() { # $1=bucket $2=key-prefix filter (client-side: MinIO's
              # server-side MPU prefix filter is blind — the very bug
              # phase I guards)
  local OUT
  OUT=$(s3 s3api list-multipart-uploads --bucket "$1" --output json 2>/dev/null)
  [ -z "$OUT" ] && { echo 0; return; }
  printf '%s' "$OUT" | KEYPFX="$2" python3 -c '
import json, os, sys
ups = json.load(sys.stdin).get("Uploads") or []
print(len([u for u in ups if u["Key"].startswith(os.environ["KEYPFX"])]))'
}

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
  pkill -9 -f toxiproxy-server 2>/dev/null
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

# ══════════════════════════════════════════════════════════════════════
phase_h() {
  say "phase H: zombie hub — SIGSTOP past the lease, takeover, SIGCONT"
  local BK=flint-chaos-h EA=/tmp/chaos-h-exp-a SA=/tmp/chaos-h-st-a
  local EB=/tmp/chaos-h-exp-b SB=/tmp/chaos-h-st-b
  s3 s3 mb "s3://$BK" >/dev/null
  fresh_world $EA $SA; fresh_world $EB $SB

  # 4 MiB whole-put ceiling: the zombie's 8 MiB backlog file would take
  # the MULTIPART path if its stale publish ever ran. Flush floor 3 s:
  # the freeze must land inside it, and an umount sits between the
  # backlog write and the SIGSTOP.
  gen_cfg /tmp/flint-chaos-h1.yaml $PORT $EA $SA $BK 99 2 3 4194304 3 1
  launch_hub h1 /tmp/flint-chaos-h1.yaml
  wait_bound $PORT 30 h1
  mount_client
  vm "dd if=/dev/urandom of=$MNT/baseline.bin bs=1M count=2 conv=fsync 2>/dev/null"
  wait_key $BK "${PREFIX}baseline.bin" 40 || fail "baseline never published"
  local BMD5; BMD5=$(vm "md5sum $MNT/baseline.bin | awk '{print \$1}'" | tr -d '\r')

  # ACK a dirty backlog, then freeze INSIDE the flush floor (3 s):
  # the zombie sleeps holding durable dirty bits it never published.
  # Umount BEFORE the freeze — a client umount against a hub that will
  # die without a successor on its port wedges in D-state forever (the
  # kernel retries the dead port), and the client has no role in the
  # freeze/wake drama.
  vm "dd if=/dev/urandom of=$MNT/sting.bin bs=1M count=8 conv=fsync 2>/dev/null && \
      echo zombie > $MNT/zsmall.txt && sync"
  umount_client
  kill -STOP "$(hub_pid h1)" || fail "SIGSTOP failed"
  ps -o state= -p "$(hub_pid h1)" | grep -q '^ *T' \
    || fail "hub h1 is not in stopped state after SIGSTOP"
  s3 s3api head-object --bucket $BK --key "${PREFIX}sting.bin" >/dev/null 2>&1 \
    && fail "sting.bin reached the bucket BEFORE the freeze — timing lost the flush-floor race"
  pass "holder frozen with an unpublished 8 MiB backlog (T state)"

  # A successor on a FRESH world judges the frozen holder by the
  # store's clock, takes epoch 2, imports the bucket, and serves.
  gen_cfg /tmp/flint-chaos-h2.yaml $PORT_B $EB $SB $BK 99 2 3 4194304 2 2
  launch_hub h2 /tmp/flint-chaos-h2.yaml
  local d=$((SECONDS + 40))
  until grep -q "tier epoch: TAKEOVER" /tmp/flint-chaos-h2.log; do
    [ $SECONDS -gt $d ] && { tail -20 /tmp/flint-chaos-h2.log; fail "successor never took over the frozen holder"; }
    sleep 1
  done
  grep -q "watching its lease" /tmp/flint-chaos-h2.log \
    || fail "successor skipped the lease watch — it must judge, not seize"
  wait_bound $PORT_B 30 h2
  pass "successor judged the unconscious holder dead and took epoch 2"

  # The moment of truth: the zombie wakes believing it holds the epoch,
  # with a flush tick and a heartbeat both overdue. The heartbeat's
  # single CAS must fence the guard before the flusher's multi-step
  # publish reaches the store — NOTHING of the backlog may land.
  kill -CONT "$(hub_pid h1)" || fail "SIGCONT failed"
  d=$((SECONDS + 25))
  while hub_alive h1; do
    [ $SECONDS -gt $d ] && { tail -20 /tmp/flint-chaos-h1.log; fail "the zombie survived waking with a stale epoch"; }
    sleep 1
  done
  grep -q "DEPOSED" /tmp/flint-chaos-h1.log \
    || fail "the zombie exited without logging DEPOSED"
  s3 s3api head-object --bucket $BK --key "${PREFIX}sting.bin" >/dev/null 2>&1 \
    && fail "the ZOMBIE'S stale publish reached the bucket — fencing lost the wake-up race"
  s3 s3api head-object --bucket $BK --key "${PREFIX}zsmall.txt" >/dev/null 2>&1 \
    && fail "the zombie's small-file publish reached the bucket"
  [ "$(mpu_count $BK "$PREFIX")" = "0" ] \
    || fail "the zombie left a multipart upload behind"
  rm -f /tmp/flint-chaos-h1.pid
  pass "woken zombie: DEPOSED ⇒ exit; ZERO stale bytes reached the bucket"
  # (sting.bin is ACKed-but-unflushed at the freeze: it survives on the
  #  OLD holder's disk only — that is the documented failover RPO, and
  #  leaking it into the successor's bucket would be the actual bug.)

  mount_client $PORT_B
  vm "sync; echo 3 > /proc/sys/vm/drop_caches" 2>/dev/null
  local CMD5; CMD5=$(vm "md5sum $MNT/baseline.bin | awk '{print \$1}'" | tr -d '\r')
  [ "$CMD5" = "$BMD5" ] || fail "successor served $CMD5 for baseline, wanted $BMD5"
  vm "echo after-takeover > $MNT/after.txt && sync"
  wait_key $BK "${PREFIX}after.txt" 40 || fail "successor never published a new write"
  pass "successor serves the imported world and publishes fresh writes"
  umount_client; stop_hub h2
  sweep_log /tmp/flint-chaos-h1.log "phase H"; sweep_log /tmp/flint-chaos-h2.log "phase H"
}

# ══════════════════════════════════════════════════════════════════════
phase_i() {
  say "phase I: neighbor prefix — the sweep's blast radius on a shared bucket"
  local BK=flint-chaos-i EA=/tmp/chaos-i-exp-a SA=/tmp/chaos-i-st-a
  local EB=/tmp/chaos-i-exp-b SB=/tmp/chaos-i-st-b
  local PA=teama/ PB=teamb/
  s3 s3 mb "s3://$BK" >/dev/null
  fresh_world $EA $SA; fresh_world $EB $SB

  gen_cfg /tmp/flint-chaos-i1.yaml $PORT   $EA $SA $BK 99 2 3 4194304 1 1 $PA
  gen_cfg /tmp/flint-chaos-i2.yaml $PORT_B $EB $SB $BK 99 2 3 4194304 1 1 $PB
  launch_hub i1 /tmp/flint-chaos-i1.yaml
  launch_hub i2 /tmp/flint-chaos-i2.yaml
  wait_bound $PORT 30 i1; wait_bound $PORT_B 30 i2
  pass "two tenants hold independent epochs on ONE bucket"

  mount_client
  vm "echo tenant-a-anchor > $MNT/anchor-a.txt && sync"
  wait_key $BK "${PA}anchor-a.txt" 40 || fail "tenant A's anchor never published"
  umount_client

  # Plant two in-flight assemblies: an ORPHAN under A's prefix (a dead
  # incarnation's leftover — the sweep's rightful prey) and a LIVE one
  # under B's prefix (the neighbor mid-flush). The bug-2 fix lists the
  # bucket WIDE and filters in code — this is the blast-radius test.
  local UPA UPB
  UPA=$(s3 s3api create-multipart-upload --bucket $BK --key "${PA}orphan-a.bin" \
          --query UploadId --output text) || fail "create-multipart A failed"
  UPB=$(s3 s3api create-multipart-upload --bucket $BK --key "${PB}inflight-b.bin" \
          --query UploadId --output text) || fail "create-multipart B failed"
  [ -n "$UPA" ] && [ -n "$UPB" ] || fail "empty UploadId from create-multipart"

  kill_hub i1
  launch_hub i1r /tmp/flint-chaos-i1.yaml
  wait_bound $PORT 40 i1r
  local d=$((SECONDS + 30))
  until [ "$(mpu_count $BK $PA)" = "0" ]; do
    [ $SECONDS -gt $d ] && { tail -20 /tmp/flint-chaos-i1r.log; fail "the claim sweep never aborted tenant A's own orphan"; }
    sleep 1
  done
  [ "$(mpu_count $BK $PB)" = "1" ] \
    || fail "the sweep CROSSED the prefix boundary and killed the neighbor's in-flight upload"
  grep -q "fenced in-flight assembly .* on ${PA}orphan-a.bin" /tmp/flint-chaos-i1r.log \
    || fail "no fence log for tenant A's orphan"
  grep -q "${PB}inflight-b.bin" /tmp/flint-chaos-i1r.log \
    && fail "the neighbor's upload appears in tenant A's fence log"
  pass "claim sweep fenced its OWN orphan and spared the neighbor's upload"

  # Live variant: tenant B flushes a real multipart while tenant A
  # restarts (its sweep may run mid-flight); B's flush must land
  # byte-identical with no NoSuchUpload and no epoch disturbance.
  mount_client $PORT_B
  vm "dd if=/dev/urandom of=$MNT/live.bin bs=1M count=8 conv=fsync 2>/dev/null"
  kill_hub i1r
  launch_hub i1s /tmp/flint-chaos-i1.yaml
  wait_bound $PORT 40 i1s
  wait_key $BK "${PB}live.bin" 60 || fail "tenant B's live flush never landed"
  s3 s3 cp "s3://$BK/${PB}live.bin" /tmp/chaos-i-live.bin >/dev/null 2>&1
  [ "$(md5 -q /tmp/chaos-i-live.bin)" = "$(md5 -q "$EB/live.bin")" ] \
    || fail "tenant B's published bytes diverged across A's restart"
  grep -q "NoSuchUpload" /tmp/flint-chaos-i2.log \
    && fail "tenant B hit NoSuchUpload — its assembly was fenced by the neighbor"
  grep -qE "DEPOSED|self-fencing" /tmp/flint-chaos-i2.log \
    && fail "tenant B's epoch was disturbed by the neighbor's restart"
  umount_client
  pass "neighbor's live flush landed byte-identical across A's restart"

  # DR import must stay inside the tenant's prefix: rebuild A from the
  # shared bucket and assert it materializes ONLY teama/ content.
  stop_hub i1s
  fresh_world $EA $SA
  launch_hub i1d /tmp/flint-chaos-i1.yaml
  wait_bound $PORT 40 i1d
  [ -e "$EA/anchor-a.txt" ] || fail "DR import lost the tenant's own file"
  [ ! -e "$EA/live.bin" ] || fail "DR import swallowed the NEIGHBOR's object"
  pass "DR import rebuilt tenant A without crossing into the neighbor's prefix"

  s3 s3api abort-multipart-upload --bucket $BK --key "${PB}inflight-b.bin" \
    --upload-id "$UPB" >/dev/null 2>&1
  stop_hub i1d; stop_hub i2
  sweep_log /tmp/flint-chaos-i1.log "phase I"; sweep_log /tmp/flint-chaos-i1r.log "phase I"
  sweep_log /tmp/flint-chaos-i1s.log "phase I"; sweep_log /tmp/flint-chaos-i1d.log "phase I"
  sweep_log /tmp/flint-chaos-i2.log "phase I"
}

# ══════════════════════════════════════════════════════════════════════
phase_j() {
  say "phase J: versioned bucket — the C4 orphan is RECOVERABLE"
  local BK=flint-chaos-j E=/tmp/chaos-j-exp S=/tmp/chaos-j-st
  s3 s3 mb "s3://$BK" >/dev/null
  s3 s3api put-bucket-versioning --bucket $BK \
    --versioning-configuration Status=Enabled >/dev/null \
    || fail "put-bucket-versioning failed"
  s3 s3api get-bucket-versioning --bucket $BK | grep -q Enabled \
    || fail "MinIO did not enable versioning"
  fresh_world $E $S
  gen_cfg /tmp/flint-chaos-j.yaml $PORT $E $S $BK 50 2 3 67108864 1 1
  launch_hub j /tmp/flint-chaos-j.yaml
  wait_bound $PORT 30 j
  mount_client

  vm "dd if=/dev/urandom of=/tmp/chaos-j-src.bin bs=1k count=900 2>/dev/null"
  local VMD5; VMD5=$(vm "md5sum /tmp/chaos-j-src.bin | awk '{print \$1}'" | tr -d '\r')
  vm "cp /tmp/chaos-j-src.bin $MNT/victim.bin && sync"
  wait_key $BK "${PREFIX}victim.bin" 40 || fail "victim.bin never published"
  local d=$((SECONDS + 90))
  until [ "$(stat -f %z "$E/victim.bin" 2>/dev/null)" = "0" ]; do
    [ $SECONDS -gt $d ] && fail "victim.bin never evicted"
    sleep 2
  done

  # The C4 wound: a foreign DELETE under the evicted file. On a
  # versioned bucket this writes a DELETE MARKER — the bytes still
  # exist one version down.
  s3 s3 rm "s3://$BK/${PREFIX}victim.bin" >/dev/null
  vm "sync; echo 3 > /proc/sys/vm/drop_caches" 2>/dev/null
  vm "rm -f /tmp/chaos-j-rc /tmp/chaos-j-read.out; \
      nohup sh -c 'cat $MNT/victim.bin > /tmp/chaos-j-read.out 2>/dev/null; \
                   echo \$? > /tmp/chaos-j-rc' >/dev/null 2>&1 &"
  sleep 6
  vm "[ ! -f /tmp/chaos-j-rc ]" \
    || fail "the read COMPLETED against a deleted object (rc=$(vm "cat /tmp/chaos-j-rc"))"
  vm "[ ! -s /tmp/chaos-j-read.out ]" \
    || fail "bytes served from a deleted object — refusal-never-loss violated"
  pass "reader parked on the orphaned stub: no bytes, no error, still waiting"

  # The operator runbook (A9): remove the delete marker; the prior
  # version — same etag the row recorded — becomes current again.
  local MARKER
  MARKER=$(s3 s3api list-object-versions --bucket $BK \
             --prefix "${PREFIX}victim.bin" --output json 2>/dev/null \
           | python3 -c '
import json, sys
d = json.load(sys.stdin)
ms = [m["VersionId"] for m in d.get("DeleteMarkers") or [] if m.get("IsLatest")]
print(ms[0] if ms else "")')
  [ -n "$MARKER" ] || fail "no delete marker found on the versioned bucket"
  s3 s3api delete-object --bucket $BK --key "${PREFIX}victim.bin" \
    --version-id "$MARKER" >/dev/null || fail "delete-marker removal failed"

  # The SAME parked reader must now unpark (hydration retry backoff
  # caps at 30 s) and deliver the ORIGINAL bytes.
  d=$((SECONDS + 90))
  until vm "[ -f /tmp/chaos-j-rc ]"; do
    [ $SECONDS -gt $d ] && { tail -20 /tmp/flint-chaos-j.log; fail "reader never unparked after the marker removal"; }
    sleep 3
  done
  [ "$(vm "cat /tmp/chaos-j-rc" | tr -d '\r')" = "0" ] || fail "unparked read failed"
  local RMD5; RMD5=$(vm "md5sum /tmp/chaos-j-read.out | awk '{print \$1}'" | tr -d '\r')
  [ "$RMD5" = "$VMD5" ] || fail "unparked reader got $RMD5, wanted $VMD5"
  hub_alive j || fail "hub died during the recovery"
  pass "delete-marker removal unparked the SAME reader with the original bytes"
  umount_client; stop_hub j
  sweep_log /tmp/flint-chaos-j.log "phase J"
}

# ══════════════════════════════════════════════════════════════════════
phase_k() {
  say "phase K: restart storm + a kill landed MID-HYDRATION"
  local BK=flint-chaos-k E=/tmp/chaos-k-exp S=/tmp/chaos-k-st
  s3 s3 mb "s3://$BK" >/dev/null
  fresh_world $E $S
  gen_cfg /tmp/flint-chaos-k.yaml $PORT $E $S $BK 99 2 5 4194304 1 1
  launch_hub k /tmp/flint-chaos-k.yaml
  wait_bound $PORT 30 k
  mount_client

  # A dirty backlog (multipart-sized + small), then a storm of
  # incarnations too short-lived to finish anything: some die before
  # the claim completes, some mid-reconcile, some mid-flush.
  vm "for i in 1 2 3; do dd if=/dev/urandom of=$MNT/storm_\$i.bin bs=1M count=8 conv=fsync 2>/dev/null || exit 1; done; \
      for i in \$(seq 1 12); do echo storm-\$i > $MNT/s\$i.txt || exit 1; done; sync; \
      cd $MNT && find . -type f -exec md5sum {} + > /tmp/chaos-k-expect.txt" \
    || fail "backlog batch failed"
  kill_hub k
  local L
  for L in 0.05 0.3 0.9 1.5 0.05 0.6 1.2 0.1; do
    launch_hub k /tmp/flint-chaos-k.yaml
    sleep "$L"
    kill_hub k
  done
  launch_hub k /tmp/flint-chaos-k.yaml
  wait_bound $PORT 40 k
  vm "cd $MNT && md5sum --quiet -c /tmp/chaos-k-expect.txt" \
    || fail "ACKed content diverged after the restart storm"
  local d=$((SECONDS + 120))
  while :; do
    s3 s3 cp "s3://$BK/${PREFIX}.flint/manifest" /tmp/chaos-k-manifest.json >/dev/null 2>&1
    python3 -c "
import json,sys
m=json.load(open('/tmp/chaos-k-manifest.json'))
sys.exit(0 if m['beyond_rpo']==0 else 1)" 2>/dev/null && break
    [ $SECONDS -gt $d ] && { tail -20 /tmp/flint-chaos-k.log; fail "storm world never settled to beyond_rpo=0"; }
    sleep 3
  done
  d=$((SECONDS + 45))
  until [ "$(mpu_count $BK "$PREFIX")" = "0" ]; do
    [ $SECONDS -gt $d ] && fail "orphaned MPU(s) persisted after the storm"
    sleep 3
  done
  pass "8 sub-2s incarnations: backlog converged to RPO 0, zero orphans"

  # Kill -9 landed MID-HYDRATION, caught by watching the stub GROW:
  # the in-place restore fills the file from 0, so 0 < size < full IS
  # the restore window. The durable hydrating flag must disambiguate
  # at the next startup (partial ⇒ truncate back to the stub).
  local HYD_MB=512 HYD_BYTES=$((512 * 1048576))
  vm "dd if=/dev/urandom of=$MNT/hyd.bin bs=1M count=$HYD_MB conv=fsync 2>/dev/null" \
    || fail "hydration-target write failed"
  local HMD5; HMD5=$(md5 -q "$E/hyd.bin")
  d=$((SECONDS + 180))
  while :; do
    s3 s3 cp "s3://$BK/${PREFIX}.flint/manifest" /tmp/chaos-k-manifest.json >/dev/null 2>&1
    python3 -c "
import json,sys
m=json.load(open('/tmp/chaos-k-manifest.json'))
sys.exit(0 if m['beyond_rpo']==0 else 1)" 2>/dev/null && break
    [ $SECONDS -gt $d ] && fail "hyd.bin never settled to RPO 0"
    sleep 3
  done
  stop_hub k
  gen_cfg /tmp/flint-chaos-k50.yaml $PORT $E $S $BK 50 2 5 4194304 1 1
  launch_hub k2 /tmp/flint-chaos-k50.yaml
  wait_bound $PORT 30 k2
  d=$((SECONDS + 120))
  until [ "$(stat -f %z "$E/hyd.bin" 2>/dev/null)" = "0" ]; do
    [ $SECONDS -gt $d ] && { tail -20 /tmp/flint-chaos-k2.log; fail "hyd.bin never evicted"; }
    sleep 2
  done
  stop_hub k2
  launch_hub k3 /tmp/flint-chaos-k.yaml
  wait_bound $PORT 30 k3
  vm "sync; echo 3 > /proc/sys/vm/drop_caches" 2>/dev/null
  vm "rm -f /tmp/chaos-k-rc; \
      nohup sh -c 'cat $MNT/hyd.bin > /dev/null 2>&1; \
                   echo \$? > /tmp/chaos-k-rc' >/dev/null 2>&1 &"
  d=$((SECONDS + 30))
  local SZ=0
  while :; do
    SZ=$(stat -f %z "$E/hyd.bin" 2>/dev/null || echo 0)
    [ "$SZ" -gt 0 ] && [ "$SZ" -lt "$HYD_BYTES" ] && break
    [ $SECONDS -gt $d ] && fail "never observed the restore window (size stayed $SZ)"
    sleep 0.02
  done
  kill -9 "$(hub_pid k3)"; rm -f /tmp/flint-chaos-k3.pid
  pass "kill -9 landed mid-restore ($SZ of $HYD_BYTES bytes on disk)"

  launch_hub k4 /tmp/flint-chaos-k.yaml
  wait_bound $PORT 40 k4
  grep -q "crashed hydration of .*hyd.bin" /tmp/flint-chaos-k4.log \
    || { tail -20 /tmp/flint-chaos-k4.log; fail "reconciler never disambiguated the crashed restore"; }
  d=$((SECONDS + 120))
  until vm "[ -f /tmp/chaos-k-rc ]"; do
    [ $SECONDS -gt $d ] && { tail -20 /tmp/flint-chaos-k4.log; fail "parked reader never completed after the crash"; }
    sleep 3
  done
  [ "$(vm "cat /tmp/chaos-k-rc" | tr -d '\r')" = "0" ] || fail "post-crash read failed"
  vm "sync; echo 3 > /proc/sys/vm/drop_caches" 2>/dev/null
  local CMD5; CMD5=$(vm "md5sum $MNT/hyd.bin | awk '{print \$1}'" | tr -d '\r')
  [ "$CMD5" = "$HMD5" ] || fail "post-crash hydration diverged: $CMD5 ≠ $HMD5"
  pass "reconciler truncated the partial back; re-hydration served the exact bytes"
  umount_client; stop_hub k4
  sweep_log /tmp/flint-chaos-k.log "phase K"; sweep_log /tmp/flint-chaos-k2.log "phase K"
  sweep_log /tmp/flint-chaos-k3.log "phase K"; sweep_log /tmp/flint-chaos-k4.log "phase K"
}

# ══════════════════════════════════════════════════════════════════════
TOXI_API=http://127.0.0.1:8474
TOXI_PORT=29002
toxic_add() { # $1=json
  curl -s -X POST "$TOXI_API/proxies/s3deg/toxics" -d "$1" | grep -q '"name"' \
    || fail "toxic creation failed: $1"
}
toxic_del() { curl -s -X DELETE "$TOXI_API/proxies/s3deg/toxics/$1" >/dev/null; }

phase_l() {
  say "phase L: degraded network — cuts, latency, and a full stall"
  command -v toxiproxy-server >/dev/null \
    || fail "toxiproxy-server not found (brew install toxiproxy)"
  local BK=flint-chaos-l E=/tmp/chaos-l-exp S=/tmp/chaos-l-st
  s3 s3 mb "s3://$BK" >/dev/null
  fresh_world $E $S

  pkill -9 -f toxiproxy-server 2>/dev/null; sleep 0.3
  nohup toxiproxy-server >/tmp/flint-chaos-toxi.log 2>&1 &
  disown
  local i
  for i in $(seq 1 20); do curl -sf "$TOXI_API/version" >/dev/null && break; sleep 0.5; done
  curl -sf "$TOXI_API/version" >/dev/null || fail "toxiproxy API never came up"
  curl -s -X DELETE "$TOXI_API/proxies/s3deg" >/dev/null 2>&1
  curl -s -X POST "$TOXI_API/proxies" \
    -d "{\"name\":\"s3deg\",\"listen\":\"127.0.0.1:$TOXI_PORT\",\"upstream\":\"127.0.0.1:$MINIO_PORT\"}" \
    | grep -q '"name"' || fail "toxiproxy proxy creation failed"

  gen_cfg /tmp/flint-chaos-l.yaml $PORT $E $S $BK 99 2 5 4194304 1 1 \
    "$PREFIX" "http://127.0.0.1:$TOXI_PORT"
  launch_hub l /tmp/flint-chaos-l.yaml
  wait_bound $PORT 30 l
  mount_client
  vm "dd if=/dev/urandom of=$MNT/pre.bin bs=1M count=2 conv=fsync 2>/dev/null"
  wait_key $BK "${PREFIX}pre.bin" 40 || fail "baseline through the proxy never published"
  pass "baseline publish through the clean proxy"

  # L1 — multipart publishes while 60% of upstream connections DIE
  # after 4 MiB: parts fail mid-body, the SDK and the outer tick both
  # retry, everything must still land byte-identical.
  toxic_add '{"name":"cut_up","type":"limit_data","stream":"upstream","toxicity":0.6,"attributes":{"bytes":4194304}}'
  vm "for i in 1 2 3 4 5 6; do \
        dd if=/dev/urandom of=$MNT/cut_\$i.bin bs=1M count=8 conv=fsync 2>/dev/null || exit 1; \
      done; sync" || fail "writes under cuts failed on the CLIENT side (S3 faults must not surface)"
  local d=$((SECONDS + 240)) n want got
  while :; do
    n=0
    for i in 1 2 3 4 5 6; do
      s3 s3api head-object --bucket $BK --key "${PREFIX}cut_$i.bin" >/dev/null 2>&1 && n=$((n+1))
    done
    [ "$n" = "6" ] && break
    [ $SECONDS -gt $d ] && { tail -20 /tmp/flint-chaos-l.log; fail "only $n/6 files landed under mid-stream cuts"; }
    sleep 3
  done
  toxic_del cut_up
  for i in 2 5; do
    s3 s3 cp "s3://$BK/${PREFIX}cut_$i.bin" /tmp/chaos-l-got.bin >/dev/null 2>&1
    want=$(md5 -q "$E/cut_$i.bin"); got=$(md5 -q /tmp/chaos-l-got.bin)
    [ "$want" = "$got" ] || fail "cut_$i.bin landed CORRUPT under cuts: $got ≠ $want"
  done
  grep -q "DEPOSED" /tmp/flint-chaos-l.log && fail "connection cuts deposed the epoch"
  pass "L1: 6×8 MiB multiparts converged byte-identical through 60% mid-stream cuts"

  # L2 — hydration with EVERY downstream connection cut after 16 MiB:
  # the ranged-GET loop must reconnect its way through 128 MiB.
  vm "dd if=/dev/urandom of=$MNT/hyd.bin bs=1M count=128 conv=fsync 2>/dev/null"
  local HMD5; HMD5=$(md5 -q "$E/hyd.bin")
  d=$((SECONDS + 180))
  while :; do
    s3 s3 cp "s3://$BK/${PREFIX}.flint/manifest" /tmp/chaos-l-manifest.json >/dev/null 2>&1
    python3 -c "
import json,sys
m=json.load(open('/tmp/chaos-l-manifest.json'))
sys.exit(0 if m['beyond_rpo']==0 else 1)" 2>/dev/null && break
    [ $SECONDS -gt $d ] && fail "hyd.bin never settled pre-eviction"
    sleep 3
  done
  stop_hub l
  gen_cfg /tmp/flint-chaos-l50.yaml $PORT $E $S $BK 50 2 5 4194304 1 1 \
    "$PREFIX" "http://127.0.0.1:$TOXI_PORT"
  launch_hub lw /tmp/flint-chaos-l50.yaml
  wait_bound $PORT 30 lw
  d=$((SECONDS + 120))
  until [ "$(stat -f %z "$E/hyd.bin" 2>/dev/null)" = "0" ]; do
    [ $SECONDS -gt $d ] && fail "hyd.bin never evicted"
    sleep 2
  done
  stop_hub lw
  launch_hub lr /tmp/flint-chaos-l.yaml
  wait_bound $PORT 30 lr
  toxic_add '{"name":"cut_down","type":"limit_data","stream":"downstream","toxicity":1.0,"attributes":{"bytes":16777216}}'
  vm "sync; echo 3 > /proc/sys/vm/drop_caches" 2>/dev/null
  local CMD5
  CMD5=$(vm "timeout 240 md5sum $MNT/hyd.bin | awk '{print \$1}'" | tr -d '\r')
  toxic_del cut_down
  [ "$CMD5" = "$HMD5" ] \
    || { tail -20 /tmp/flint-chaos-lr.log; fail "hydration under cuts served '$CMD5', wanted $HMD5"; }
  local RETRIES
  RETRIES=$(grep -c "retrying the chunk" /tmp/flint-chaos-lr.log)
  [ "$RETRIES" -ge 1 ] \
    || fail "hydration succeeded but the chunk-retry path never fired — the fault didn't bite"
  pass "L2: 128 MiB hydrated byte-identical with EVERY connection cut at 16 MiB ($RETRIES chunk retries)"

  # L3 — 750 ms latency each way: everything slows, nothing fences.
  toxic_add '{"name":"lat_up","type":"latency","stream":"upstream","toxicity":1.0,"attributes":{"latency":750,"jitter":250}}'
  toxic_add '{"name":"lat_down","type":"latency","stream":"downstream","toxicity":1.0,"attributes":{"latency":750,"jitter":250}}'
  vm "echo slow-but-alive > $MNT/lat.txt && sync"
  wait_key $BK "${PREFIX}lat.txt" 120 || fail "publish never landed under 750 ms latency"
  grep -qE "DEPOSED|self-fencing" /tmp/flint-chaos-lr.log \
    && fail "LATENCY deposed the epoch — renew treats slow as dead"
  toxic_del lat_up; toxic_del lat_down
  pass "L3: 750 ms RTT inflation: publishes land, the epoch holds"

  # L4 — a FULL stall (data stopped, connections held open), then
  # lifted. The wildcard leg: whichever way the hub rides it (blocked
  # calls resume, or a lease-window self-fence + restart), the backlog
  # must converge after the network returns — a permanent wedge is the
  # only failure.
  toxic_add '{"name":"stall_up","type":"timeout","stream":"upstream","toxicity":1.0,"attributes":{"timeout":0}}'
  toxic_add '{"name":"stall_down","type":"timeout","stream":"downstream","toxicity":1.0,"attributes":{"timeout":0}}'
  vm "echo survived-the-stall > $MNT/stall.txt && sync" \
    || fail "a write FAILED during the stall — a stalled store must not take the filesystem down"
  sleep 25
  toxic_del stall_up; toxic_del stall_down
  local OUTCOME
  if hub_alive lr; then
    if wait_key $BK "${PREFIX}stall.txt" 120; then
      OUTCOME="hub rode out the stall in place"
    elif hub_alive lr; then
      fail "hub alive but the backlog never converged after the stall lifted — WEDGED"
    else
      OUTCOME=""
    fi
  fi
  if [ -z "${OUTCOME:-}" ]; then
    grep -qE "DEPOSED|self-fencing" /tmp/flint-chaos-lr.log \
      || fail "hub died during the stall without a fence log"
    launch_hub l2 /tmp/flint-chaos-l.yaml
    wait_bound $PORT 40 l2
    wait_key $BK "${PREFIX}stall.txt" 120 \
      || fail "backlog never converged after the post-stall restart"
    OUTCOME="hub self-fenced (a stall IS a dead store); restart flushed the backlog"
  fi
  pass "L4: 25 s full stall lifted — $OUTCOME"

  d=$((SECONDS + 60))
  until [ "$(mpu_count $BK "$PREFIX")" = "0" ]; do
    [ $SECONDS -gt $d ] && fail "MPU orphans persisted after the degraded-network run"
    sleep 3
  done
  umount_client
  if hub_alive lr; then stop_hub lr; fi
  if [ -f /tmp/flint-chaos-l2.pid ]; then stop_hub l2; fi
  curl -s -X DELETE "$TOXI_API/proxies/s3deg" >/dev/null 2>&1
  pkill -9 -f toxiproxy-server 2>/dev/null
  sweep_log /tmp/flint-chaos-l.log "phase L"; sweep_log /tmp/flint-chaos-lw.log "phase L"
  sweep_log /tmp/flint-chaos-lr.log "phase L"
  if [ -f /tmp/flint-chaos-l2.log ]; then sweep_log /tmp/flint-chaos-l2.log "phase L"; fi
}

for ph in $PHASES; do
  case "$ph" in
    a) phase_a;; b) phase_b;; c) phase_c;;
    d) phase_d;; e) phase_e;; f) phase_f;; g) phase_g;;
    h) phase_h;; i) phase_i;; j) phase_j;; k) phase_k;;
    l) phase_l;;
    *) fail "unknown phase '$ph'";;
  esac
done

echo
echo "══════════════════════════════════════════════════════════════════"
echo " PASS — split-brain, outage, foreign hands, space pressure,"
echo " crash loops, endurance, two writers, the zombie hub, neighbor"
echo " prefixes, versioned recovery, restart storms, and the degraded"
echo " network all held"
echo "══════════════════════════════════════════════════════════════════"
