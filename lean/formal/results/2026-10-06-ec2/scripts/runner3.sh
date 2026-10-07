#!/bin/bash
# Restart 2026-10-06 ~14:35Z with tlc-rs patched (LevelWriter::push: write + drop outside the disk lock; profile:
# 176/192 workers waited on that lock once a level spilled, box 90% idle). Run 2 (unpatched) kept as .run2.out;
# its per-depth counts are the control for run 3 (depths 26 and 27 spilled). Same flags as run 2.
export HOME=/root AWS_DEFAULT_REGION=us-west-1; source /root/.cargo/env; ulimit -n 1048576
BK=flint-tlc-lean-20261006; O=/data/out; LF=/opt/flint/lean/formal; W=LeanP1Size3L4W3; TR=/opt/flint/formal/tlc-rs
log() { echo "$(date -u +%FT%TZ) $*" | tee -a $O/RESULTS.txt; }
( while true; do aws s3 cp $O/ s3://$BK/out/ --recursive --quiet; sleep 60; done ) &
( while true; do P=$(pgrep tlcgen); echo "$(date -u +%T) $(free -g | awk '/Mem/{print "used",$3,"avail",$7}') $(df -BG --output=used /data | tail -1) maps ${P:+$(wc -l < /proc/$P/maps)}" >> $O/mem.log; sleep 60; done ) &
rm -rf /data/st/a
# control: interpreter old vs patched on L1W3 with the queue forced to disk
python3 - <<PY
import re
s=open("$LF/LeanP1Holds1p3b.cfg").read()
for k,v in (("Writers","{A, B, C}"),("MaxMint",2),("MaxUI",0),("MaxBarriers",1),("MaxRestarts",0),("MaxSyncs",0),("MaxCopies",1)):
    s,n=re.subn(rf"^  {k} = .*$",f"  {k} = {v}",s,flags=re.M); assert n==1,k
open("$LF/LeanP1Size3L1W3.cfg","w").write(s)
PY
[ -f /data/tlc-rs.old ] || cp $TR/target/release/tlc-rs /data/tlc-rs.old
cp /opt/store.rs $TR/src/store.rs && (cd $TR && touch src/store.rs && cargo build --release) > $O/tlcrs-build3.log 2>&1 || { log "patched tlc-rs build FAILED"; exit 1; }
for b in old new; do B=/data/tlc-rs.old; [ $b = new ] && B=$TR/target/release/tlc-rs
  (cd $LF && $B -workers 64 -checkpoint 0 -metadir /data/st/ctl-$b -queue-mem 1 -config LeanP1Size3L1W3.cfg LeanP1.tla > $O/ctl-L1W3-$b.out 2>&1); rm -rf /data/st/ctl-$b
done
co=$(grep -oE '[0-9]+ states generated, [0-9]+ distinct states found. Depth [0-9]+' $O/ctl-L1W3-old.out); cn=$(grep -oE '[0-9]+ states generated, [0-9]+ distinct states found. Depth [0-9]+' $O/ctl-L1W3-new.out)
dsk=$(grep -oE '[0-9]+ on disk' $O/ctl-L1W3-new.out | sort -n | tail -1)
log "control L1W3 (Mac ladder: 1954072 gen, 449595 distinct, depth 33) -queue-mem 1: old [$co] new [$cn] (max $dsk)"
[ -n "$co" ] && [ "$co" = "$cn" ] && [ "$dsk" != "0 on disk" ] || { log "CONTROL MISMATCH or no spill — not running"; echo DONE > $O/DONE; aws s3 cp $O/ s3://$BK/out/ --recursive --quiet; shutdown -h +5; exit 1; }
rm -rf /data/gen /data/gt
if (cd $LF && $TR/target/release/tlc-rs -codegen /data/gen -config $W.cfg LeanP1.tla && cd /data/gen && CARGO_TARGET_DIR=/data/gt cargo build --release) > $O/A.build3.log 2>&1; then
  BIN=$(ls /data/gt/release/tlcgen-* | grep -v '\.d$' | head -1); t0=$(date +%s); log "patched checker built; run 3 starting"
  (cd $LF && timeout 18000 $BIN -workers 192 -checkpoint 0 -metadir /data/st/a -fpmem 200000 -queue-mem 150000 -config $W.cfg LeanP1.tla > $O/$W.out 2>&1); rc=$?
  log "$W run 3 rc=$rc $(( $(date +%s)-t0 ))s (124 = 5 h cap) | $(grep -oE 'No error has been found|Invariant [A-Za-z_]+ is violated|Action property [A-Za-z_]+ is violated|Error:.*|memory allocation.*' $O/$W.out | head -1) | $(grep -E 'states generated|^progress' $O/$W.out | tail -1 | cut -c1-170)"
  rm -rf /data/st/a
else log "patched codegen build FAILED (A.build3.log)"; fi
log DONE; echo DONE > $O/DONE; aws s3 cp $O/ s3://$BK/out/ --recursive --quiet
shutdown -h +5
