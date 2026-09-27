#!/bin/bash
# r51 — repo core md5 83c17abf…, cfg md5 682816ca…: the THREE-path core world
# (LeanCoreHolds: Paths={p1,p2,p3}, Free={p3}, MaxSeq=4) with Prop_NoSilentRevert.
# Never finished before (120M states, still climbing at depth 19). Shares the box
# with r34, so 2 workers at nice 19; -continue reports every route; TLC
# checkpoints every 30 min, so a stopped run resumes with -recover.
set -u
OUT=~/lean-core-r3/r51; mkdir -p $OUT; RES=$OUT/RESULTS.txt; : > $RES
cd ~/lean-core-sandbox2/formal || exit 1
[ "$(md5sum LeanCore.tla | cut -c1-32)" = 83c17abf3583aa330124eeab411a74c7 ] || { echo "SANDBOX MD5 CHANGED" >> $RES; exit 4; }
[ "$(md5sum LeanCoreHolds.cfg | cut -c1-32)" = 682816caf529f1a103b249f9eb7cccc7 ] || { echo "CFG MD5 CHANGED" >> $RES; exit 4; }
JAR=~/lean-gate-2026-09-18/.tla2tools.jar
ST=/mnt/nvme/tlc-r51; rm -rf $ST; mkdir -p $ST
w=LeanCoreHolds
nice -n 19 java -XX:+UseParallelGC -Xmx5g -cp $JAR tlc2.TLC -workers 2 -continue \
  -metadir $ST/$w -config $w.cfg LeanCore.tla > $OUT/$w.out 2>&1
rc=$?
got=$(grep -oE "Invariant [A-Za-z_]+ is violated|Action property [A-Za-z_]+ is violated|Temporal properties were violated|No error has been found" $OUT/$w.out | sort | uniq -c | tr '\n' ';')
printf "%-20s rc=%s | %s | %s | %s\n" "$w" "$rc" "$got" \
  "$(grep -oE '^[0-9,]+ states generated, [0-9,]+ distinct' $OUT/$w.out | tail -1)" \
  "$(grep -oE 'depth of the complete state graph search is [0-9]+|Finished in [0-9a-z ]+' $OUT/$w.out | tail -1)" >> $RES
echo R51DONE >> $RES
