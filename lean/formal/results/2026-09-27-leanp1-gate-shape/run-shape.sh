#!/bin/bash
# Gate-shape sizing (2026-09-27): LeanP1Holds under GateBound at three
# bounds, smallest first; each gets 4 h. Starts when gate d ends.
# LeanP1.tla md5 3f6af642. 3 workers (the deep run has 3, forge 2).
set -u
cd ~/lean-leanp1-gate-shape-2026-09-27 || exit 1
[ "$(md5sum LeanP1.tla | cut -c1-32)" = 3f6af642f7e39c19cafa413540e50cbe ] || { echo "MD5 CHANGED" > RESULTS.txt; exit 4; }
until grep -q LEANP1DONE ~/lean-leanp1-2026-09-25d/RESULTS.txt 2>/dev/null; do sleep 120; done
JAR=~/lean-gate-2026-09-18/.tla2tools.jar
ST=/mnt/nvme/tlc-leanp1-shape; mkdir -p $ST out; : > RESULTS.txt
for w in MCLeanP1GateSeq3Copies0 MCLeanP1GateSeq3Copies1 MCLeanP1GateSeqAllCopies0; do
  timeout 14400 nice -n 10 java -XX:+UseParallelGC -Xmx8g -cp $JAR tlc2.TLC -workers 3 \
    -metadir $ST/$w -config $w.cfg MCLeanP1Gate.tla > out/$w.out 2>&1; rc=$?
  got=$(grep -oE "Invariant [A-Za-z_]+ is violated|Action property [A-Za-z_]+ is violated|No error has been found|Error: [^.]*" out/$w.out | head -1)
  printf "%-28s rc=%-3s | %s | %s | %s | last: %s\n" "$w" "$rc" "$got" \
    "$(grep -oE '^[0-9,]+ states generated, [0-9,]+ distinct' out/$w.out | tail -1)" \
    "$(grep -oE 'depth of the complete state graph search is [0-9]+|Finished in [0-9a-z ]+' out/$w.out | tail -1)" \
    "$(grep Progress out/$w.out | tail -1 | grep -oE '[0-9,]+ distinct states found.*queue')" >> RESULTS.txt
  rm -rf $ST/$w
done
echo SHAPEDONE >> RESULTS.txt
