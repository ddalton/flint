#!/usr/bin/env bash
#
# S3-tier drill — the assembled tier, end to end, against MinIO.
#
# Every L2 layer is drill-proven in isolation (in-process drills, the
# 11-leg real-S3 store gate, the step-9 DELAY rig). This script is the
# first time the ASSEMBLED start_tier() path — S3Store::connect →
# bootstrap → epoch claim → import → flush loop → watermark evict →
# hydrate → DR — runs as a whole under a real kernel NFS client.
#
#   phase 1  capture → flush → manifest: write through the kernel
#            mount (pattern file, tree, symlink, chmod, sqlite, git),
#            verify objects + posix stamps land in the bucket, the
#            manifest records the tree (RPO readable from the bucket
#            alone), the git tmp-write+rename storm produced ZERO
#            foreign/412 noise, REMOVE tombstones delete the object,
#            and the A12 reporter spoke.
#   phase 2  restart + evict + hydrate: restart the hub under the
#            LIVE mount (epoch self-recognition), watermark now armed
#            ⇒ clean files evict to 0-byte stubs; the client's stat
#            still sees logical sizes; reads DELAY → hydrate → serve
#            byte-identical.
#   phase 3  DR: destroy the "PVC" (export tree AND state.db), restart
#            the hub against the surviving bucket ⇒ import-refresh
#            rebuilds dirs/symlinks/stubs; a fresh mount reads every
#            byte back (sqlite queries, git fsck); the tombstoned file
#            did NOT resurrect.
#
# Topology (lite-drill conventions): MinIO + hub on the macOS host,
# the Lima VM is the kernel client. Configs are generated per phase
# (the watermark is the eviction lever: 99 = never on this 91%-full
# disk, 50 = always). KEEP=1 leaves the rig standing. Cleanup umounts
# BEFORE killing the server (a dead server under a live mount D-states
# umount).
#
# Exit status: 0 on PASS, 1 on FAIL.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN_DIR="$REPO_ROOT/spdk-csi-driver/target/release"
LOG_DIR=/tmp
LIMA_VM="${LIMA_VM:-flint-nfs-client}"

MDS_PORT=20491
EXPORT_DIR=/tmp/flint-tier-export
STATE_DIR=/tmp/flint-tier-state
CFG=/tmp/flint-tier-drill.yaml
MDS_LOG="$LOG_DIR/flint-tier-mds.log"
PIDFILE="$LOG_DIR/flint-tier-mds.pid"
SCRATCH=/tmp/flint-tier-scratch

MINIO_NAME=flint-tier-minio
MINIO_PORT=9000
MINIO_USER=flintdrill
MINIO_PASS=flintdrill123
BUCKET=flint-tier-drill
PREFIX=vol1/

MNT=/mnt/tier

say()  { echo; echo "── $*"; }
pass() { echo "  ✓ $*"; }
skip() { echo "  △ SKIP: $*"; }
fail() { echo "  ✗ FAIL: $*"; exit 1; }

vm() { limactl shell "$LIMA_VM" -- sudo sh -c "$1"; }

# aws CLI pinned to the drill's MinIO — never the ambient AWS world.
# (AWS_PROFILE must be UNSET, not empty: an empty value is a lookup
# for a profile named "".)
s3() {
  env -u AWS_PROFILE \
    AWS_ACCESS_KEY_ID=$MINIO_USER AWS_SECRET_ACCESS_KEY=$MINIO_PASS \
    AWS_DEFAULT_REGION=us-east-1 AWS_EC2_METADATA_DISABLED=true \
    aws --endpoint-url "http://127.0.0.1:$MINIO_PORT" "$@"
}

start_server() { # $1 = watermarkPct, $2 = hydrateWarmAfterImport (default false)
  cat > "$CFG" <<EOF
apiVersion: chert.us/v1alpha1
kind: PnfsConfig
mode: standalone

mds:
  bind:
    address: "0.0.0.0"
    port: $MDS_PORT

  layout:
    type: file
    stripeSize: 8388608
    policy: stripe

  dataServers: []

  state:
    backend: sqlite
    config:
      path: $STATE_DIR/state.db

  tier:
    enabled: true
    bucket: $BUCKET
    keyPrefix: "$PREFIX"
    endpoint: "http://127.0.0.1:$MINIO_PORT"
    flushFloorSecs: 2
    quiesceSecs: 1
    tickSecs: 2
    epochHeartbeatSecs: 2
    epochLeaseMisses: 3
    watermarkPct: $1
    reserveBytes: 67108864
    ballastBytes: 16777216
    hydrateWarmAfterImport: ${2:-false}

exports:
  - path: $EXPORT_DIR
    fsid: 1
    options: [rw, sync, no_subtree_check]
    access:
      - network: 0.0.0.0/0
        permissions: rw

logging:
  level: info
  format: text
  components:
    mds: info

monitoring:
  prometheus:
    enabled: false
    port: 0
    path: /metrics
  health:
    enabled: false
    port: 0
    path: /health
  metrics: []
EOF
  env -u AWS_PROFILE \
    AWS_ACCESS_KEY_ID=$MINIO_USER AWS_SECRET_ACCESS_KEY=$MINIO_PASS \
    AWS_REGION=us-east-1 AWS_EC2_METADATA_DISABLED=true \
    FLINT_TIER_REPORT_SECS=5 \
    nohup "$BIN_DIR/flint-pnfs-mds" --config "$CFG" >>"$MDS_LOG" 2>&1 &
  echo $! >"$PIDFILE"
  disown
  # Startup includes bucket bootstrap + epoch claim: allow retries.
  for _ in $(seq 1 30); do
    kill -0 "$(cat "$PIDFILE")" 2>/dev/null \
      || { tail -30 "$MDS_LOG"; fail "hub died on startup"; }
    lsof -nP -iTCP:$MDS_PORT -sTCP:LISTEN >/dev/null 2>&1 && return 0
    sleep 1
  done
  tail -30 "$MDS_LOG"; fail "hub never bound :$MDS_PORT"
}

stop_server() {
  [ -f "$PIDFILE" ] && kill "$(cat "$PIDFILE")" 2>/dev/null
  for _ in $(seq 1 20); do
    kill -0 "$(cat "$PIDFILE" 2>/dev/null)" 2>/dev/null || break
    sleep 0.5
  done
  kill -9 "$(cat "$PIDFILE" 2>/dev/null)" 2>/dev/null
  rm -f "$PIDFILE"
}

cleanup() {
  set +e
  if [ "${KEEP:-0}" = "1" ]; then
    echo "KEEP=1 — leaving MinIO, server and mount standing"
    return
  fi
  vm "umount -lf $MNT 2>/dev/null" 2>/dev/null
  stop_server
  pkill -9 -f "flint-pnfs-mds --config $CFG" 2>/dev/null
  docker rm -f "$MINIO_NAME" >/dev/null 2>&1
}
trap cleanup EXIT

echo "══════════════════════════════════════════════════════════════════"
echo " S3-tier drill — the assembled tier vs MinIO, one kernel client"
echo "══════════════════════════════════════════════════════════════════"

# ── pre-flight ────────────────────────────────────────────────────────
[ -x "$BIN_DIR/flint-pnfs-mds" ] \
  || fail "missing $BIN_DIR/flint-pnfs-mds — run 'make build-pnfs'"
command -v docker >/dev/null || fail "docker not found"
command -v aws >/dev/null || fail "aws CLI not found"
command -v limactl >/dev/null || fail "limactl not found"
limactl list --quiet 2>/dev/null | grep -qx "$LIMA_VM" \
  || fail "Lima VM '$LIMA_VM' not running"
lsof -nP -iTCP:$MDS_PORT -sTCP:LISTEN >/dev/null 2>&1 \
  && fail "port $MDS_PORT already held — a leftover server?"

# Fresh world.
cleanup
rm -rf "$EXPORT_DIR" "$STATE_DIR" "$SCRATCH"
mkdir -p "$EXPORT_DIR" "$STATE_DIR" "$SCRATCH"
chmod 0777 "$EXPORT_DIR"
: > "$MDS_LOG"

# ── MinIO up + bucket ─────────────────────────────────────────────────
say "starting MinIO"
docker rm -f "$MINIO_NAME" >/dev/null 2>&1
docker run -d --name "$MINIO_NAME" -p 127.0.0.1:$MINIO_PORT:9000 \
  -e MINIO_ROOT_USER=$MINIO_USER -e MINIO_ROOT_PASSWORD=$MINIO_PASS \
  quay.io/minio/minio server /data >/dev/null \
  || fail "MinIO container failed to start"
for _ in $(seq 1 30); do
  curl -sf "http://127.0.0.1:$MINIO_PORT/minio/health/live" >/dev/null && break
  sleep 1
done
curl -sf "http://127.0.0.1:$MINIO_PORT/minio/health/live" >/dev/null \
  || fail "MinIO never became healthy"
s3 s3 mb "s3://$BUCKET" >/dev/null || fail "bucket create failed"
pass "MinIO healthy, bucket $BUCKET created"

# ══════════════════════════════════════════════════════════════════════
# Phase 1 — capture → flush → manifest (watermark 99: no eviction)
# ══════════════════════════════════════════════════════════════════════
say "phase 1: starting the hub (tier on, watermark 99)"
start_server 99
grep -q "STANDALONE" "$MDS_LOG" || fail "no standalone banner"
grep -q "epoch .* held" "$MDS_LOG" || fail "epoch never claimed — $(tail -5 "$MDS_LOG")"
pass "hub up: standalone posture, bucket bootstrap accepted, epoch held"

HOST_IP=$(vm "getent hosts host.lima.internal | awk '{print \$1}'" | tr -d '\r')
[ -n "$HOST_IP" ] || fail "cannot resolve host.lima.internal in the VM"
vm "mountpoint -q $MNT && umount -lf $MNT; mkdir -p $MNT; \
    timeout 30 mount -t nfs4 -o minorversion=1,proto=tcp,port=$MDS_PORT \
      $HOST_IP:/ $MNT" || fail "client mount failed"
pass "kernel client mounted at $MNT"

say "phase 1: workload through the kernel mount"
vm "dd if=/dev/urandom of=$MNT/model.bin bs=1M count=16 conv=fsync 2>/dev/null && \
    md5sum $MNT/model.bin | awk '{print \$1}' > /tmp/tier-md5-model" \
  || fail "pattern write failed"
vm "mkdir -p $MNT/data/nested && \
    echo 'the notes' > $MNT/data/nested/notes.txt && \
    chmod 640 $MNT/data/nested/notes.txt && \
    ln -s data/nested/notes.txt $MNT/latest && \
    echo 'doomed' > $MNT/victim.txt" || fail "tree build failed"
vm "cd $MNT && sqlite3 tier.db 'create table t(v text); insert into t values(\"alpha\"),(\"beta\"); select count(*) from t;' | grep -q 2" \
  || fail "sqlite workload failed"
vm "cd $MNT && rm -rf repo && mkdir repo && cd repo && \
    git init -q && git config user.email d@d && git config user.name d && \
    for i in 1 2 3; do echo commit-\$i > f.txt; git add f.txt; git commit -qm c\$i; done && \
    git log --oneline | wc -l | grep -q 3" || fail "git workload failed"
pass "workload done: 16 MiB pattern, tree+symlink+chmod, sqlite, git ×3 commits"

say "phase 1: waiting for the flush barrier"
DEADLINE=$((SECONDS + 90))
until s3 s3api head-object --bucket "$BUCKET" --key "${PREFIX}model.bin" >/dev/null 2>&1; do
  [ $SECONDS -gt $DEADLINE ] && { tail -20 "$MDS_LOG"; fail "model.bin never published"; }
  sleep 2
done
SIZE=$(s3 s3api head-object --bucket "$BUCKET" --key "${PREFIX}model.bin" \
  --query ContentLength --output text)
[ "$SIZE" = "16777216" ] || fail "published size $SIZE ≠ 16777216"
pass "model.bin published (16 MiB)"

DEADLINE=$((SECONDS + 60))
until s3 s3api head-object --bucket "$BUCKET" --key "${PREFIX}data/nested/notes.txt" \
        >/dev/null 2>&1; do
  [ $SECONDS -gt $DEADLINE ] && { tail -20 "$MDS_LOG"; fail "notes.txt never published"; }
  sleep 2
done
MODE=$(s3 s3api head-object --bucket "$BUCKET" --key "${PREFIX}data/nested/notes.txt" \
  --query 'Metadata."flint-mode"' --output text 2>/dev/null)
case "$MODE" in *640) pass "posix stamps ride the object (flint-mode=$MODE)";;
  *) fail "notes.txt flint-mode stamp is '$MODE', wanted *640";; esac

DEADLINE=$((SECONDS + 60))
until s3 s3 cp "s3://$BUCKET/${PREFIX}.flint/manifest" "$SCRATCH/manifest.json" >/dev/null 2>&1 \
      && python3 - "$SCRATCH/manifest.json" <<'PY' >/dev/null 2>&1
import json, sys
m = json.load(open(sys.argv[1]))
paths = {e["path"]: e for e in m["entries"]}
assert m["seq"] >= 1
assert "model.bin" in paths and paths["model.bin"]["etag"]
assert paths["data"]["type"] == "dir"
assert paths["latest"]["type"] == "symlink"
assert paths["latest"]["target"] == "data/nested/notes.txt"
assert "victim.txt" in paths
assert oct(paths["data/nested/notes.txt"]["mode"] & 0o777) == "0o640"
PY
do
  [ $SECONDS -gt $DEADLINE ] && { tail -20 "$MDS_LOG"; fail "manifest never settled"; }
  sleep 2
done
pass "manifest: seq, files w/ etag, dir, symlink+target, 0640 mode — RPO readable from the bucket alone"

# The git idiom is the false-412 proof workload (A7).
if grep -qi "FOREIGN bucket state\|found FOREIGN" "$MDS_LOG"; then
  fail "git storm produced foreign/412 noise — A7 regression"
fi
pass "zero foreign-overwrite noise across the git tmp-write+rename storm"

grep -q "tier last" "$MDS_LOG" \
  || fail "the A12 reporter never spoke during activity"
pass "A12 reporter spoke: $(grep 'tier last' "$MDS_LOG" | tail -1 | sed 's/.*tier last/tier last/' | cut -c1-90)…"

say "phase 1: REMOVE → tombstone → bucket delete"
vm "rm $MNT/victim.txt" || fail "remove failed"
DEADLINE=$((SECONDS + 30))
while s3 s3api head-object --bucket "$BUCKET" --key "${PREFIX}victim.txt" >/dev/null 2>&1; do
  [ $SECONDS -gt $DEADLINE ] && fail "victim.txt object never deleted after REMOVE"
  sleep 2
done
pass "tombstone consumed: the bucket object is gone"

# The warm leg's size oracle: every non-empty regular file's size,
# captured while the tree is FULLY LOCAL (nothing evicted yet). After
# the phase-4 warm fill, each of these paths must hold its full size
# again — "non-zero" would certify fill-STARTED, not fill-COMPLETED
# (restores pwrite in place, so a file is non-zero at its first chunk;
# chaos-K exploits exactly that window), and legitimately-empty files
# are exempt by construction.
SIZE_ORACLE="$LOG_DIR/warm-size-oracle"
(cd "$EXPORT_DIR" && find . -type f ! -size 0 -exec stat -f "%z %N" {} \; | sort -k2) \
  > "$SIZE_ORACLE"
[ -s "$SIZE_ORACLE" ] || fail "size oracle came up empty — capture point moved?"

# ══════════════════════════════════════════════════════════════════════
# Phase 2 — restart under the live mount, evict, hydrate
# ══════════════════════════════════════════════════════════════════════
say "phase 2: restarting the hub UNDER the live mount (watermark 50 ⇒ evict)"
stop_server
start_server 50
CLAIMS=$(grep -c "epoch .* held" "$MDS_LOG")
[ "$CLAIMS" -ge 2 ] \
  || fail "second epoch claim not logged (saw $CLAIMS) — self-recognition never ran?"
pass "hub restarted; epoch re-claimed (self-recognition path)"

DEADLINE=$((SECONDS + 60))
until [ "$(stat -f %z "$EXPORT_DIR/model.bin" 2>/dev/null)" = "0" ]; do
  [ $SECONDS -gt $DEADLINE ] && { tail -20 "$MDS_LOG"; fail "model.bin never evicted"; }
  sleep 2
done
pass "model.bin evicted server-side (0-byte stub; bucket holds the bytes)"

CSIZE=$(vm "stat -c %s $MNT/model.bin" | tr -d '\r')
[ "$CSIZE" = "16777216" ] \
  || fail "client stat of the evicted file says $CSIZE — GETATTR must serve the LOGICAL size"
pass "client stat sees the logical 16 MiB through the marker"

# Force the read through the wire — the client cached these bytes in
# phase 1.
vm "sync; echo 3 > /proc/sys/vm/drop_caches" 2>/dev/null
T0=$SECONDS
MD5_NOW=$(vm "md5sum $MNT/model.bin | awk '{print \$1}'" | tr -d '\r')
MD5_ORIG=$(vm "cat /tmp/tier-md5-model" | tr -d '\r')
[ "$MD5_NOW" = "$MD5_ORIG" ] \
  || fail "hydrated read differs: $MD5_NOW ≠ $MD5_ORIG"
pass "evicted read DELAY→hydrate→served byte-identical in $((SECONDS - T0))s"

# ══════════════════════════════════════════════════════════════════════
# Phase 3 — DR: destroy the PVC, rebuild from the bucket alone
# ══════════════════════════════════════════════════════════════════════
say "phase 3: destroying the PVC (export tree AND state.db)"
vm "umount -lf $MNT" || fail "umount failed"
stop_server
rm -rf "$EXPORT_DIR" "$STATE_DIR"
mkdir -p "$EXPORT_DIR" "$STATE_DIR"
chmod 0777 "$EXPORT_DIR"
pass "local world erased — only the bucket survives"

say "phase 3: restarting the hub against the surviving bucket"
start_server 99
grep -q "tier import" "$MDS_LOG" \
  || { tail -30 "$MDS_LOG"; fail "import-refresh never ran on the fresh state"; }
[ -L "$EXPORT_DIR/latest" ] || fail "symlink not restored"
[ "$(stat -f %z "$EXPORT_DIR/model.bin" 2>/dev/null)" = "0" ] \
  || fail "model.bin not restored as a stub"
[ ! -e "$EXPORT_DIR/victim.txt" ] || fail "the TOMBSTONED victim.txt RESURRECTED"
NMODE=$(stat -f %Lp "$EXPORT_DIR/data/nested/notes.txt" 2>/dev/null)
[ "$NMODE" = "640" ] || fail "notes.txt mode restored as $NMODE, wanted 640"
pass "tree rebuilt: dirs, symlink, 0640 mode, stubs; victim stayed dead"

vm "timeout 30 mount -t nfs4 -o minorversion=1,proto=tcp,port=$MDS_PORT \
      $HOST_IP:/ $MNT" || fail "post-DR mount failed"
MD5_DR=$(vm "md5sum $MNT/model.bin | awk '{print \$1}'" | tr -d '\r')
[ "$MD5_DR" = "$MD5_ORIG" ] || fail "post-DR read differs: $MD5_DR ≠ $MD5_ORIG"
pass "post-DR model.bin byte-identical through hydration"

vm "cd $MNT && sqlite3 tier.db 'select count(*) from t;' | grep -q 2" \
  || fail "post-DR sqlite query failed"
vm "cd $MNT/repo && git fsck --strict >/dev/null 2>&1 && git log --oneline | wc -l | grep -q 3" \
  || fail "post-DR git fsck/log failed"
pass "sqlite queries and git fsck pass on the restored tree"

# ══════════════════════════════════════════════════════════════════════
# Phase 4 — warm DR: destroy the PVC AGAIN, rebuild with the warm fill
# (a fresh destroy is load-bearing: only fresh state triggers the
# import, and only an import that ran triggers the fill)
# ══════════════════════════════════════════════════════════════════════
say "phase 4: destroying the PVC a second time (warm-fill DR)"
vm "umount -lf $MNT" || fail "phase-4 umount failed"
stop_server
rm -rf "$EXPORT_DIR" "$STATE_DIR"
mkdir -p "$EXPORT_DIR" "$STATE_DIR"
chmod 0777 "$EXPORT_DIR"

say "phase 4: restarting with hydrateWarmAfterImport=true"
start_server 99 true
grep -q "tier import" "$MDS_LOG" \
  || { tail -30 "$MDS_LOG"; fail "phase-4 import-refresh never ran"; }

# The truthful done-signal is the driver's own report line — never a
# non-zero-size poll (in-place restores are non-zero at chunk one).
# NO CLIENT MOUNT EXISTS YET: everything below is the fill's own work.
say "phase 4: waiting for the warm fill's report line"
for _ in $(seq 1 120); do
  grep -q "tier warm fill done" "$MDS_LOG" && break
  kill -0 "$(cat "$PIDFILE")" 2>/dev/null || { tail -30 "$MDS_LOG"; fail "hub died mid-fill"; }
  sleep 1
done
grep -q "tier warm fill done" "$MDS_LOG" \
  || { tail -30 "$MDS_LOG"; fail "warm fill never reported within 120s"; }
FILL_LINE=$(grep "tier warm fill done" "$MDS_LOG" | tail -1)
echo "   $FILL_LINE"
F_RESTORED=$(echo "$FILL_LINE" | sed -E 's/.*done: ([0-9]+) restored.*/\1/')
F_CAND=$(echo "$FILL_LINE" | sed -E 's/.* ([0-9]+) candidates.*/\1/')
F_COLD=$(echo "$FILL_LINE" | sed -E 's/.* ([0-9]+) still cold.*/\1/')
F_SPACE=$(echo "$FILL_LINE" | sed -E 's/.* ([0-9]+) stopped for space.*/\1/')
[ "$F_CAND" -gt 0 ] || fail "warm fill saw 0 candidates — the import restored no stubs?"
[ "$F_COLD" = "0" ] && [ "$F_SPACE" = "0" ] \
  || fail "warm fill left files cold (still_cold=$F_COLD stopped_for_space=$F_SPACE)"
[ "$F_RESTORED" = "$F_CAND" ] \
  || fail "warm fill restored $F_RESTORED of $F_CAND candidates"
pass "warm fill restored all $F_RESTORED stubs with ZERO client reads"

# Size EQUALITY against the phase-1 oracle (full sizes, not non-zero),
# checked SERVER-SIDE before any mount exists.
while read -r WANT_SZ REL; do
  GOT_SZ=$(stat -f %z "$EXPORT_DIR/$REL" 2>/dev/null) \
    || fail "warm-restored tree is missing $REL"
  [ "$GOT_SZ" = "$WANT_SZ" ] \
    || fail "$REL is $GOT_SZ bytes after the fill, wanted $WANT_SZ (partial restore?)"
done < "$SIZE_ORACLE"
MD5_WARM=$(md5 -q "$EXPORT_DIR/model.bin")
[ "$MD5_WARM" = "$MD5_ORIG" ] \
  || fail "warm-restored model.bin differs server-side: $MD5_WARM ≠ $MD5_ORIG"
pass "every oracle file back at full size; model.bin byte-identical server-side"

# Only now does a client appear — reads must be plain local serves.
vm "timeout 30 mount -t nfs4 -o minorversion=1,proto=tcp,port=$MDS_PORT \
      $HOST_IP:/ $MNT" || fail "phase-4 mount failed"
MD5_W2=$(vm "md5sum $MNT/model.bin | awk '{print \$1}'" | tr -d '\r')
[ "$MD5_W2" = "$MD5_ORIG" ] || fail "phase-4 client read differs: $MD5_W2 ≠ $MD5_ORIG"
vm "cd $MNT && sqlite3 tier.db 'select count(*) from t;' | grep -q 2" \
  || fail "phase-4 sqlite query failed"
vm "cd $MNT/repo && git fsck --strict >/dev/null 2>&1 && git log --oneline | wc -l | grep -q 3" \
  || fail "phase-4 git fsck/log failed"
pass "client verify on the pre-warmed tree: md5, sqlite, git all pass"

echo
echo "══════════════════════════════════════════════════════════════════"
echo " PASS — capture→flush→manifest, tombstones, restart, evict,"
echo " hydrate-under-a-kernel-client, DR-from-the-bucket, and the"
echo " WARM FILL (eager DR restore, zero client reads) all held"
echo "══════════════════════════════════════════════════════════════════"
