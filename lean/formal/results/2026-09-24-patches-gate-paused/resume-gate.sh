#!/bin/bash
# Resume the patches gate (paused 2026-09-25 09:01 UTC after 13 of 44 worlds).
# 1. LeanImmutableRepairOverridesUI resumes from its checkpoint of 08:39:29
#    (SOUNDNESS FINGERPRINT: "Recovery completed" ~391.1M examined, ~103.9M
#    queue; Progress(22) at 08:39:29: 391,084,564 distinct, queue 103,915,687).
#    It had NOT fired at depth 23 / 427M distinct: since L-119's fix it must find
#    its own route (it fired through L-119's at depth 18 on 2026-09-20).
# 2. Then the 30 worlds after it in WORLDS.tsv, as run-patches.sh would.
# Appends to RESULTS.txt; writes PATCHESDONE at the end (r54.sh waits for it).
set -u
cd ~/lean-patches-2026-09-24 || exit 1
[ "$(md5sum LeanSubtree.tla | cut -c1-32)" = 44eacbfd8e6198d2c7f172865650b177 ] || { echo "MD5 CHANGED LeanSubtree.tla" >> RESULTS.txt; exit 4; }
[ "$(md5sum LeanRefine.tla | cut -c1-32)" = 635c2419d3a563356e60f5505888ab6a ] || { echo "MD5 CHANGED LeanRefine.tla" >> RESULTS.txt; exit 4; }
[ "$(md5sum LeanCore.tla | cut -c1-32)" = 6532ddded15070912d130a948b3fdf60 ] || { echo "MD5 CHANGED LeanCore.tla" >> RESULTS.txt; exit 4; }
JAR=~/lean-gate-2026-09-18/.tla2tools.jar
ST=/mnt/nvme/tlc-patches
score() { # <world> <expectation> <rc> <out>
  local wld=$1 exp=$2 rc=$3 out=$4 got v=MISMATCH
  got=$(grep -oE "Invariant [A-Za-z_]+ is violated|Action property [A-Za-z_]+ is violated|Temporal properties were violated|No error has been found|Error: [^.]*" $out | head -1)
  if [ $rc = 124 ]; then v=UNDECIDED-TIMEOUT
  elif [ "$exp" = HOLDS ] && [ "$got" = "No error has been found" ]; then v=OK-HOLDS
  elif [ "$exp" != HOLDS ] && echo "$got" | grep -q "$exp"; then v=OK-FIRES; fi
  printf "%-38s %-18s exp=%-38s rc=%-3s | %s | %s | %s\n" "$wld" "$v" "$exp" "$rc" "$got" \
    "$(grep -oE '^[0-9,]+ states generated, [0-9,]+ distinct' $out | tail -1)" \
    "$(grep -oE 'depth of the complete state graph search is [0-9]+|Finished in [0-9a-z ]+' $out | tail -1)" >> RESULTS.txt
}
w=LeanImmutableRepairOverridesUI
timeout 21600 nice -n 10 java -XX:+UseParallelGC -Xmx16g -cp $JAR tlc2.TLC -workers 6 \
  -metadir $ST/$w -recover $ST/$w/26-09-25-04-09-07 -config $w.cfg LeanSubtree.tla > out/$w-resumed.out 2>&1
rc=$?
score $w "Invariant Inv_HITLDurable" $rc out/$w-resumed.out
grep -E "^Recovery completed" out/$w-resumed.out | head -1 >> RESULTS.txt
awk -F'\t' -v w=$w 'f{print} $1==w{f=1}' WORLDS.tsv | while IFS=$'\t' read -r wld mod exp; do
  timeout 21600 nice -n 10 java -XX:+UseParallelGC -Xmx16g -cp $JAR tlc2.TLC -workers 6 \
    -metadir $ST/$wld -config $wld.cfg $mod.tla > out/$wld.out 2>&1
  score $wld "$exp" $? out/$wld.out
done
echo PATCHESDONE >> RESULTS.txt
