#!/bin/bash
# Gate for the three LeanSubtree patches applied 2026-09-24 (narrow over handles,
# L-123 QueueYieldsToSync, L-125 OutrankedRemovalLeavesBaseline). Expectations in
# WORLDS.tsv written BEFORE the run: check.sh's verdicts for every handles/refine
# world, r35/r36's for the sync and sentinel worlds off the gate. The 3-path
# LeanImmutableRenameHolds (30-40 h) is NOT in this batch.
set -u
cd ~/lean-patches-2026-09-24 || exit 1
for f in "LeanSubtree.tla 44eacbfd8e6198d2c7f172865650b177" "LeanRefine.tla 635c2419d3a563356e60f5505888ab6a" "LeanCore.tla 6532ddded15070912d130a948b3fdf60"; do
  set -- $f; [ "$(md5sum $1 | cut -c1-32)" = $2 ] || { echo "MD5 CHANGED $1" > RESULTS.txt; exit 4; }
done
JAR=~/lean-gate-2026-09-18/.tla2tools.jar
ST=/mnt/nvme/tlc-patches; rm -rf $ST; mkdir -p $ST out; : > RESULTS.txt
while IFS=$'\t' read -r wld mod exp; do
  timeout 21600 nice -n 10 java -XX:+UseParallelGC -Xmx16g -cp $JAR tlc2.TLC -workers 6 \
    -metadir $ST/$wld -config $wld.cfg $mod.tla > out/$wld.out 2>&1
  rc=$?
  got=$(grep -oE "Invariant [A-Za-z_]+ is violated|Action property [A-Za-z_]+ is violated|Temporal properties were violated|No error has been found|Error: [^.]*" out/$wld.out | head -1)
  v=MISMATCH
  if [ $rc = 124 ]; then v=UNDECIDED-TIMEOUT
  elif [ "$exp" = HOLDS ] && [ "$got" = "No error has been found" ]; then v=OK-HOLDS
  elif [ "$exp" != HOLDS ] && echo "$got" | grep -q "$exp"; then v=OK-FIRES; fi
  printf "%-38s %-18s exp=%-38s rc=%-3s | %s | %s | %s\n" "$wld" "$v" "$exp" "$rc" "$got" \
    "$(grep -oE '^[0-9,]+ states generated, [0-9,]+ distinct' out/$wld.out | tail -1)" \
    "$(grep -oE 'depth of the complete state graph search is [0-9]+|Finished in [0-9a-z ]+' out/$wld.out | tail -1)" >> RESULTS.txt
done < WORLDS.tsv
echo PATCHESDONE >> RESULTS.txt
