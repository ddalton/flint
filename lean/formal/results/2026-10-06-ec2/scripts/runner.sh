#!/bin/bash
# 2026-10-06 lean (user-approved, "just do A"): the three-syncer three-barrier rung LeanP1Size3L4W3 on tlc-rs,
# compiled, on a spot r8g.48xlarge (192 vCPU, 1.5 TB): 192 workers, seen set 200 GB, level budget 250 GB, cap 5 h.
# Tree = git archive a466e463. 10-05 c8g run of the same world was OOM-killed at 983,406,036 distinct, depth 26.
export HOME=/root AWS_DEFAULT_REGION=us-west-1; source /root/.cargo/env; ulimit -n 1048576
BK=flint-tlc-lean-20261006; O=/data/out; LF=/opt/flint/lean/formal; mkdir -p $O /data/st
log() { echo "$(date -u +%FT%TZ) $*" | tee -a $O/RESULTS.txt; }
( while true; do aws s3 cp $O/ s3://$BK/out/ --recursive --quiet; sleep 60; done ) &
( while true; do echo "$(date -u +%T) $(free -g | awk '/Mem/{print "used",$3,"avail",$7}') $(df -BG --output=used /data | tail -1)" >> $O/mem.log; sleep 60; done ) &
log "start; LeanP1.tla md5 $(md5sum $LF/LeanP1.tla | cut -c1-8); nproc $(nproc); mem $(free -g | awk '/Mem/{print $2}') GB; disk avail $(df -BG --output=avail /data | tail -1)"
cd /opt/flint/formal/tlc-rs && cargo build --release > $O/tlcrs-build.log 2>&1 || log "tlc-rs build FAILED"
T=/opt/flint/formal/tlc-rs/target/release/tlc-rs; W=LeanP1Size3L4W3
python3 - <<PY
import re
s=open("$LF/LeanP1Holds1p3b.cfg").read()
for k,v in (("Writers","{A, B, C}"),("MaxMint",3),("MaxUI",1),("MaxBarriers",3),("MaxRestarts",0),("MaxSyncs",0),("MaxCopies",1)):
    s,n=re.subn(rf"^  {k} = .*$",f"  {k} = {v}",s,flags=re.M); assert n==1,k
open("$LF/$W.cfg","w").write(s)
PY
cp $LF/$W.cfg $O/
if [ -x $T ] && (cd $LF && $T -codegen /data/gen -config $W.cfg LeanP1.tla && cd /data/gen && CARGO_TARGET_DIR=/data/gt cargo build --release) > $O/A.build.log 2>&1; then
  BIN=$(ls /data/gt/release/tlcgen-* | grep -v '\.d$' | head -1); t0=$(date +%s); log "checker built; running"
  (cd $LF && timeout 18000 $BIN -workers 192 -checkpoint 0 -metadir /data/st/a -fpmem 200000 -queue-mem 250000 -config $W.cfg LeanP1.tla > $O/$W.out 2>&1); rc=$?
  log "$W rc=$rc $(( $(date +%s)-t0 ))s (124 = 5 h cap, 137 = killed) | $(grep -oE 'No error has been found|Invariant [A-Za-z_]+ is violated|Error:.*' $O/$W.out | head -1) | $(grep -E 'states generated|^progress' $O/$W.out | tail -1 | cut -c1-170)"
  rm -rf /data/st/a
else log "codegen build FAILED (A.build.log)"; fi
log DONE; echo DONE > $O/DONE; aws s3 cp $O/ s3://$BK/out/ --recursive --quiet
shutdown -h +5
