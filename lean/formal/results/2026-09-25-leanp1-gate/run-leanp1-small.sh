#!/bin/bash
# LeanP1 gate, 2026-09-25: LeanP1.tla md5 3f6af642f7e39c19cafa413540e50cbe, 28 worlds (retire age included; rerun d: the scan trigger (derived + skipped), MCLeanP1Like first), expectations in
# WORLDS-LeanP1.tsv written before the run. The box is its own (the LeanSubtree/LeanCore
# runs were stopped and their checkpoints cleared, 2026-09-25): 6 workers.
# 2026-09-28: the small worlds FIRST. LeanP1LiveHolds is left out: at -Xmx12g
# (the cap that makes room for the Lite session) it spent its time in GC and
# was stopped at its 21:05 checkpoint, to resume with -recover once its heap
# is decided. This runner ends with SMALLDONE, not LEANP1DONE.
set -u
cd ~/lean-leanp1-2026-09-25d || exit 1
[ "$(md5sum LeanP1.tla | cut -c1-32)" = 3f6af642f7e39c19cafa413540e50cbe ] || { echo "MD5 CHANGED LeanP1.tla" > RESULTS.txt; exit 4; }
JAR=~/lean-gate-2026-09-18/.tla2tools.jar
ST=/mnt/nvme/tlc-leanp1d; mkdir -p $ST out
while IFS=$'\t' read -r wld exp; do
  timeout 21600 nice -n 10 java -XX:+UseParallelGC -Xmx12g -cp $JAR tlc2.TLC -workers 6 \
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
done < <(awk -F'\t' 'NR==FNR{d[$1]=1; next} !($1 in d) && $1 != "LeanP1LiveHolds"' <(awk '{print $1}' RESULTS.txt) WORLDS-LeanP1.tsv)
echo SMALLDONE >> RESULTS.txt
