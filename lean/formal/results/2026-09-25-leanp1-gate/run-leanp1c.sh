#!/bin/bash
# LeanP1 gate, 2026-09-25: LeanP1.tla md5 e9f47335bb22f9f3d9e2ae3eb423e019, 28 worlds (retire age included; rerun c: consume and parked paths mark behind; MCLeanP1Like first), expectations in
# WORLDS-LeanP1.tsv written before the run. The box is its own (the LeanSubtree/LeanCore
# runs were stopped and their checkpoints cleared, 2026-09-25): 6 workers.
set -u
cd ~/lean-leanp1-2026-09-25c || exit 1
[ "$(md5sum LeanP1.tla | cut -c1-32)" = e9f47335bb22f9f3d9e2ae3eb423e019 ] || { echo "MD5 CHANGED LeanP1.tla" > RESULTS.txt; exit 4; }
JAR=~/lean-gate-2026-09-18/.tla2tools.jar
ST=/mnt/nvme/tlc-leanp1c; rm -rf $ST; mkdir -p $ST out; : > RESULTS.txt
while IFS=$'\t' read -r wld exp; do
  timeout 21600 nice -n 10 java -XX:+UseParallelGC -Xmx24g -cp $JAR tlc2.TLC -workers 6 \
    -metadir $ST/$wld -config $wld.cfg $( [ -f $wld.tla ] && echo $wld.tla || echo LeanP1.tla ) > out/$wld.out 2>&1
  rc=$?
  got=$(grep -oE "Invariant [A-Za-z_]+ is violated|Action property [A-Za-z_]+ is violated|Temporal properties were violated|No error has been found|Error: [^.]*" out/$wld.out | head -1)
  v=MISMATCH
  if [ $rc = 124 ]; then v=UNDECIDED-TIMEOUT
  elif [ "$exp" = RECORD ]; then v=RECORDED
  elif [ "$exp" = HOLDS ] && [ "$got" = "No error has been found" ]; then v=OK-HOLDS
  elif [ "$exp" != HOLDS ] && echo "$got" | grep -q "$exp"; then v=OK-FIRES; fi
  printf "%-30s %-18s exp=%-22s rc=%-3s | %s | %s | %s\n" "$wld" "$v" "$exp" "$rc" "$got" \
    "$(grep -oE '^[0-9,]+ states generated, [0-9,]+ distinct' out/$wld.out | tail -1)" \
    "$(grep -oE 'depth of the complete state graph search is [0-9]+|Finished in [0-9a-z ]+' out/$wld.out | tail -1)" >> RESULTS.txt
  rm -rf $ST/$wld
done < WORLDS-LeanP1.tsv
echo LEANP1DONE >> RESULTS.txt
