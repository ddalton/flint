#!/bin/bash
# Queued 2026-10-07 ~15:15Z: after the base/s1/s2 benchmark writes DONE, keep the box (cancel its +5 shutdown,
# re-arm +80) and benchmark s3 (s2 with the seen-check moved into one seen.insert in the successor callback, so
# one shard lock per successor as in base) against s1: s3,s1,s3,s1,s3 to level 30, then s3 to completion.
export HOME=/root AWS_DEFAULT_REGION=us-west-1; source /root/.cargo/env; ulimit -n 1048576
BK=flint-tlc-bench-20261007; O=/data/out
# wait for the main runner to finish, including its own 'shutdown -h +5', then replace that shutdown
# user 15:40Z: keep the benchmark lean — the s2 full run is not needed (s2 is dropped): stop it when it starts
while [ ! -f $O/DONE ] || systemctl is-active --quiet flint-bench; do
  [ -f $O/s2-full.err ] && pgrep -f gt-s2/release/tlcgen >/dev/null && { pkill -f gt-s2/release/tlcgen; echo "$(date -u +%FT%TZ) s2-full stopped by hand (not needed)" >> $O/RESULTS.txt; }
  sleep 5
done
sleep 2; shutdown -c; shutdown -h +50
log() { echo "$(date -u +%FT%TZ) $*" | tee -a $O/RESULTS.txt; }
rm -f $O/DONE
mkdir -p /opt/flint/s3 && tar -xzf /opt/s3.tgz -C /opt/flint/s3
( while true; do aws s3 cp $O/ s3://$BK/out/ --recursive --quiet; sleep 60; done ) &
T=/opt/flint/s3/formal/tlc-rs
(cd $T && CARGO_TARGET_DIR=/data/tt-s3 cargo build --release) > $O/build-s3.log 2>&1 && (cd /opt/flint/lean/formal && /data/tt-s3/release/tlc-rs -codegen /data/gen-s3 -config L3W3.cfg LeanP1.tla && cd /data/gen-s3 && CARGO_TARGET_DIR=/data/gt-s3 cargo build --release) >> $O/build-s3.log 2>&1 || { log "s3 build FAILED"; echo DONE > $O/DONE; aws s3 cp $O/ s3://$BK/out/ --recursive --quiet; shutdown -h +5; exit 1; }
log "s3 built (box kept: shutdown re-armed +80)"
run() {
  local a=$1 tag=$2 stop=$3 BIN=$(ls /data/gt-$1/release/tlcgen-* | grep -v '\.d$' | head -1) t0=$(date +%s.%N)
  rm -rf /data/st/x; cd /opt/flint/lean/formal
  TLCRS_LEVEL_PROFILE=1 $BIN -workers 192 -checkpoint 0 -metadir /data/st/x -fpmem 32000 -queue-mem 150000 -config L3W3.cfg LeanP1.tla > $O/$tag.out 2> $O/$tag.err &
  local pid=$!
  if [ $stop -gt 0 ]; then
    while kill -0 $pid 2>/dev/null && ! grep -q "^level $stop:" $O/$tag.err; do sleep 0.2; done; kill $pid 2>/dev/null
  fi
  wait $pid 2>/dev/null; local t1=$(date +%s.%N)
  local w=$(awk -v s=$stop '/^level /{l=$2+0; if (s==0 || l<=s) w+=$6} END{printf "%.1f", w/1000}' $O/$tag.err)
  log "$tag: levels 0..$stop wall-sum ${w}s | process $(echo "$t1 - $t0" | bc)s | $(grep -oE 'No error has been found|is violated|[0-9]+ distinct states found. Depth [0-9]+' $O/$tag.out | tr '\n' ' ')"
  rm -rf /data/st/x
}
run s3 s3-r1 30; run s1 s1-r4 30
run s3 s3-full 0
log DONE2; echo DONE > $O/DONE; aws s3 cp $O/ s3://$BK/out/ --recursive --quiet
shutdown -h +5
