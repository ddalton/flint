#!/bin/bash
# Deep LeanP1Holds on EC2 (2026-10-03, user-approved, 12 h cap, one-time spot).
# tlc-rs 2aa87d67, the COMPILED checker (-codegen; on the box it matched the
# interpreter's counts exactly and ran 1.36x faster). Module md5 94da7541.
# Phases: build; 10 min each at 8, 16 and 32 workers (fresh each time); then
# the full run at the best. No checkpoints: on spot the disk dies with the
# instance. Everything but *.bin goes to s3://$BUCKET/out/ every 2 min.
set -u
W=/opt/payload; O=/data/out; mkdir -p $O; export HOME=/root
log() { echo "$(date -u +%FT%TZ) $*" | tee -a $O/runner.log; }
sync_out() { aws s3 cp $O/ s3://$BUCKET/out/ --recursive --quiet; }
( while true; do sleep 120; { date -u +%FT%TZ; grep -E 'MemTotal|MemAvailable' /proc/meminfo; df -h /data | tail -1; uptime; } >> $O/sys.log; sync_out; done ) &
log "start $(uname -m) $(nproc) cpus"
[ "$(md5sum $W/model/LeanP1.tla | cut -c1-32)" = 94da754147dd4a821bc32d81d5f923b9 ] || { log "MD5 CHANGED"; sync_out; exit 4; }
source $HOME/.cargo/env
cd $W/tlc-rs && cargo build --release > $O/build.log 2>&1 || { log BUILD-FAILED; sync_out; exit 1; }
T=$W/tlc-rs/target/release/tlc-rs
cd $W/model && $T -codegen $W/gen -config LeanP1Holds.cfg LeanP1.tla >> $O/build.log 2>&1 || { log CODEGEN-FAILED; sync_out; exit 1; }
(cd $W/gen && CARGO_TARGET_DIR=$W/gen-target cargo build --release >> $O/build.log 2>&1) || { log GEN-BUILD-FAILED; sync_out; exit 1; }
G=$W/gen-target/release/tlcgen-leanp1
log "built"
# SMOKE: the compiled checker on a world with a known count (TLC: 3,510,269
# distinct), on this CPU, before anything long. Abort on any mismatch.
cd $W/model && $T -codegen $W/gen-smoke -config LeanP1FetchHolds.cfg LeanP1.tla >> $O/build.log 2>&1 \
  && (cd $W/gen-smoke && CARGO_TARGET_DIR=$W/gen-smoke-target cargo build --release >> $O/build.log 2>&1) \
  || { log SMOKE-BUILD-FAILED; sync_out; exit 1; }
t0=$(date +%s)
$W/gen-smoke-target/release/tlcgen-leanp1 -workers 16 -metadir /data/md-smoke -config LeanP1FetchHolds.cfg LeanP1.tla > $O/smoke.out 2>&1
rm -rf /data/md-smoke
if grep -q "No error has been found" $O/smoke.out && grep -q " 3510269 distinct states found" $O/smoke.out; then
  log "SMOKE OK $(( $(date +%s) - t0 ))s | $(grep -oE '[0-9]+ states generated, [0-9]+ distinct states found. Depth [0-9]+' $O/smoke.out | tail -1)"
else
  log "SMOKE FAILED | $(tail -3 $O/smoke.out | tr '\n' ' ')"; sync_out; exit 2
fi
sync_out
FLAGS="-fpmem 48000 -queue-mem 80000 -checkpoint 0"
best=8; bestn=0
for w in 8 16 32; do
  rm -rf /data/md-w$w
  timeout 600 $G -workers $w $FLAGS -metadir /data/md-w$w -config LeanP1Holds.cfg LeanP1.tla > $O/w$w.out 2>&1
  rc=$?
  n=$(grep '^progress:' $O/w$w.out | tail -1 | grep -oE '[0-9]+ distinct' | grep -oE '[0-9]+')
  log "w$w rc=$rc 600s | $(grep '^progress:' $O/w$w.out | tail -1)"
  rm -rf /data/md-w$w
  [ "${n:-0}" -gt "$bestn" ] && { bestn=$n; best=$w; }
done
log "full run at $best workers"
sync_out
$G -workers $best $FLAGS -metadir /data/md-full -config LeanP1Holds.cfg LeanP1.tla > $O/full.out 2>&1
rc=$?
log "full rc=$rc | $(grep -E 'No error has been found|is violated|^Error' $O/full.out | head -1) | $(grep -E 'distinct states found' $O/full.out | tail -1)"
echo DONE >> $O/runner.log
sync_out
