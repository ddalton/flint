#!/bin/bash
# r55 — resume r53 (3-PATH LeanCoreHolds + Prop_NoSilentRevert, core md5 83c17abf…,
# cfg md5 682816ca…), paused 2026-09-25 09:01 UTC for the night.
# Its last checkpoint: 2026-09-25 08:34:45, into the SAME dir r53 recovered from.
# SOUNDNESS FINGERPRINT: "Recovery completed" must report ~1,464.9M states
# examined and ~323.4M on queue (Progress(26) at 08:34:45: 1,464,866,439
# distinct, queue 323,417,068). Anything else: STOP, do not trust the run.
# DO NOT re-run r51.sh: it rm -rf's this metadir.
set -u
OUT=~/lean-core-r3/r51; RES=$OUT/RESULTS-r55.txt; : > $RES
cd ~/lean-core-sandbox2/formal || exit 1
[ "$(md5sum LeanCore.tla | cut -c1-32)" = 83c17abf3583aa330124eeab411a74c7 ] || { echo "SANDBOX MD5 CHANGED" >> $RES; exit 4; }
[ "$(md5sum LeanCoreHolds.cfg | cut -c1-32)" = 682816caf529f1a103b249f9eb7cccc7 ] || { echo "CFG MD5 CHANGED" >> $RES; exit 4; }
JAR=~/lean-gate-2026-09-18/.tla2tools.jar
ST=/mnt/nvme/tlc-r51
w=LeanCoreHolds
nice -n 19 java -XX:+UseParallelGC -Xmx5g -cp $JAR tlc2.TLC -workers 2 -continue \
  -metadir $ST/$w -recover $ST/$w/26-09-23-23-36-55 -config $w.cfg LeanCore.tla > $OUT/$w-r55.out 2>&1
rc=$?
got=$(grep -oE "Invariant [A-Za-z_]+ is violated|Action property [A-Za-z_]+ is violated|Temporal properties were violated|No error has been found" $OUT/$w-r55.out | sort | uniq -c | tr '\n' ';')
printf "%-20s rc=%s | %s | %s | %s\n" "$w" "$rc" "$got" \
  "$(grep -oE '^[0-9,]+ states generated, [0-9,]+ distinct' $OUT/$w-r55.out | tail -1)" \
  "$(grep -oE 'depth of the complete state graph search is [0-9]+|Finished in [0-9a-z ]+' $OUT/$w-r55.out | tail -1)" >> $RES
grep -E "^Recovery completed" $OUT/$w-r55.out | head -1 >> $RES
echo R55DONE >> $RES
