#!/bin/bash
# 2026-10-07 tlc-rs speedup benchmark (user-approved): three builds of one snapshot (8feba0c7) — base, s1 (step-property
# vars compare), s2 (s1 + step properties checked before a successor is built) — on LeanP1Size3L3W3 (1,421,211,248
# distinct, depth 44). Per round base, s1, s2: run to the end of level 30 (TLCRS_LEVEL_PROFILE), then kill; 3 rounds.
# Then s2 to completion (must be 1,421,211,248 distinct, No error). Results to s3 every 60 s.
export HOME=/root AWS_DEFAULT_REGION=us-west-1; source /root/.cargo/env; ulimit -n 1048576
BK=flint-tlc-bench-20261007; O=/data/out; mkdir -p $O /data/st
log() { echo "$(date -u +%FT%TZ) $*" | tee -a $O/RESULTS.txt; }
( while true; do aws s3 cp $O/ s3://$BK/out/ --recursive --quiet; sleep 60; done ) &
log "start; nproc $(nproc); mem $(free -g | awk '/Mem/{print $2}') GB; max_map_count $(sysctl -n vm.max_map_count)"
python3 - <<PY
import re
s=open("/opt/flint/lean/formal/LeanP1Holds1p3b.cfg").read()
for k,v in (("Writers","{A, B, C}"),("MaxMint",3),("MaxUI",1),("MaxBarriers",2),("MaxRestarts",0),("MaxSyncs",0),("MaxCopies",1)):
    s,n=re.subn(rf"^  {k} = .*$",f"  {k} = {v}",s,flags=re.M); assert n==1,k
open("/opt/flint/lean/formal/L3W3.cfg","w").write(s)
PY
for a in base s1 s2; do
  T=/opt/flint/$a/formal/tlc-rs
  (cd $T && CARGO_TARGET_DIR=/data/tt-$a cargo build --release) > $O/build-$a.log 2>&1 || { log "$a tlc-rs build FAILED"; continue; }
  (cd /opt/flint/lean/formal && /data/tt-$a/release/tlc-rs -codegen /data/gen-$a -config L3W3.cfg LeanP1.tla && cd /data/gen-$a && CARGO_TARGET_DIR=/data/gt-$a cargo build --release) >> $O/build-$a.log 2>&1 || { log "$a checker build FAILED"; continue; }
  log "$a built: $(ls /data/gt-$a/release/tlcgen-* | grep -v '\.d$')"
done
run() { # arm tag stop_level(0 = to completion)
  local a=$1 tag=$2 stop=$3 BIN=$(ls /data/gt-$1/release/tlcgen-* | grep -v '\.d$' | head -1) t0=$(date +%s.%N)
  rm -rf /data/st/x; cd /opt/flint/lean/formal
  TLCRS_LEVEL_PROFILE=1 $BIN -workers 192 -checkpoint 0 -metadir /data/st/x -fpmem 32000 -queue-mem 150000 -config L3W3.cfg LeanP1.tla > $O/$tag.out 2> $O/$tag.err &
  local pid=$!
  if [ $stop -gt 0 ]; then
    while kill -0 $pid 2>/dev/null && ! grep -q "^level $stop:" $O/$tag.err; do sleep 0.2; done; kill $pid 2>/dev/null
  fi
  wait $pid 2>/dev/null; local t1=$(date +%s.%N)
  local cpu=$(awk -v s=$stop '/^level /{l=$2+0; if (s==0 || l<=s) w+=$6} END{printf "%.1f", w/1000}' $O/$tag.err)
  local busy=$(awk -v s=$stop '/^level /{l=$2+0; if (l==s || (s==0)) b=$NF} END{print b}' $O/$tag.err)
  log "$tag: levels 0..$stop wall-sum ${cpu}s | process $(echo "$t1 - $t0" | bc)s | level-$stop busy $busy | $(grep -oE 'No error has been found|is violated|[0-9]+ distinct states found. Depth [0-9]+' $O/$tag.out | tr '\n' ' ')"
  rm -rf /data/st/x
}
dnf -y install bc >/dev/null 2>&1
for r in 1 2 3; do for a in base s1 s2; do run $a $a-r$r 30; done; done
run s2 s2-full 0
log DONE; echo DONE > $O/DONE; aws s3 cp $O/ s3://$BK/out/ --recursive --quiet
shutdown -h +5
