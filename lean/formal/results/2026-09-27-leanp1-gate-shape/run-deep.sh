#!/bin/bash
# LeanP1Holds at its FULL bounds, the opt-in deep run (2026-09-27): no time
# limit, checkpoints KEPT (TLC writes one every 30 min into the metadir).
# Starts when gate d ends. To resume after a stop or a reboot, run this
# script again: it recovers from the metadir when one is there.
set -u
cd ~/lean-leanp1-deep-2026-09-27 || exit 1
[ "$(md5sum LeanP1.tla | cut -c1-32)" = 3f6af642f7e39c19cafa413540e50cbe ] || { echo "MD5 CHANGED" > RESULTS.txt; exit 4; }
until grep -q LEANP1DONE ~/lean-leanp1-2026-09-25d/RESULTS.txt 2>/dev/null; do sleep 120; done
JAR=~/lean-gate-2026-09-18/.tla2tools.jar
ST=/mnt/nvme/tlc-leanp1-deep/LeanP1Holds; mkdir -p $ST
REC=; [ -e $ST/queue.chkpt ] && REC="-recover $ST"
echo "start $(date -u +%FT%TZ) ${REC:-fresh}" >> runs.log
nice -n 10 java -XX:+UseParallelGC -Xmx12g -cp $JAR tlc2.TLC -workers 3 \
  -metadir $ST $REC -config LeanP1Holds.cfg LeanP1.tla >> out-LeanP1Holds.out 2>&1; rc=$?
got=$(grep -oE "Invariant [A-Za-z_]+ is violated|Action property [A-Za-z_]+ is violated|No error has been found|Error: [^.]*" out-LeanP1Holds.out | tail -1)
printf "LeanP1Holds rc=%s | %s | %s | %s\n" "$rc" "$got" \
  "$(grep -oE '^[0-9,]+ states generated, [0-9,]+ distinct' out-LeanP1Holds.out | tail -1)" \
  "$(grep -oE 'Finished in [0-9a-z ]+' out-LeanP1Holds.out | tail -1)" >> RESULTS.txt
echo DEEPDONE >> RESULTS.txt
