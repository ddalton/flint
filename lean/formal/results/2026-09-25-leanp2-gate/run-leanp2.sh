#!/bin/bash
# LeanP2 gate, 2026-09-25: LeanP2.tla md5 7be0698441b34aca7a4e8a3248886eb8, 23 worlds, expectations in
# WORLDS-LeanP2.tsv written before the run. Waits for core gate B (COREBDONE) so
# it takes that run's 2 cores, never more. RECORD = no expectation, verdict kept.
set -u
cd ~/lean-leanp2-2026-09-25 || exit 1
until grep -q COREBDONE ~/lean-core-2026-09-25b/RESULTS.txt 2>/dev/null; do sleep 60; done
[ "$(md5sum LeanP2.tla | cut -c1-32)" = 7be0698441b34aca7a4e8a3248886eb8 ] || { echo "MD5 CHANGED LeanP2.tla" > RESULTS.txt; exit 4; }
JAR=~/lean-gate-2026-09-18/.tla2tools.jar
ST=/mnt/nvme/tlc-leanp2; rm -rf $ST; mkdir -p $ST out; : > RESULTS.txt
while IFS=$'\t' read -r wld exp; do
  timeout 21600 nice -n 10 java -XX:+UseParallelGC -Xmx8g -cp $JAR tlc2.TLC -workers 2 \
    -metadir $ST/$wld -config $wld.cfg LeanP2.tla > out/$wld.out 2>&1
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
done < WORLDS-LeanP2.tsv
echo LEANP2DONE >> RESULTS.txt
