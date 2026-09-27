#!/bin/bash
# The gated ForgeSync worlds on the SHIPPED baseline (2026-09-27), in the
# Forge slot (2 workers): starts when ForgeSyncRewindHolds has a verdict,
# and takes the slot from ForgeSyncRewindStrict (deferred: a baseline
# count, not a claim). Mutations first. ForgeSync.tla md5 1c201e9d.
set -u
cd ~/forgesync-shipped-2026-09-27 || exit 1
[ "$(md5sum ForgeSync.tla | cut -c1-32)" = 1c201e9deafafec8de2e7d640e6e1429 ] || { echo "MD5 CHANGED" > RESULTS.txt; exit 4; }
R=~/forge-needed-2026-09-26
until grep -q "^ForgeSyncRewindHolds " $R/RESULTS-Rewind.txt 2>/dev/null || grep -q FORGEREWINDDONE $R/RESULTS-Rewind.txt 2>/dev/null; do sleep 120; done
for p in $(pgrep -f "^bash ./run-rewind.sh") $(pgrep -f "^/bin/bash ./run-rewind.sh") $(pgrep -f "^java .*ForgeSyncRewindStrict"); do kill $p; done
grep -q FORGEREWINDDONE $R/RESULTS-Rewind.txt || printf "ForgeSyncRewindStrict DEFERRED 2026-09-27: its slot went to the shipped ForgeSync gate\nFORGEREWINDDONE\n" >> $R/RESULTS-Rewind.txt
JAR=~/lean-gate-2026-09-18/.tla2tools.jar
ST=/mnt/nvme2/forgesync-shipped; mkdir -p $ST out; : > RESULTS.txt
while IFS=$'\t' read -r w exp; do
  timeout 21600 nice -n 5 java -XX:+UseParallelGC -Xmx6g -cp $JAR tlc2.TLC -workers 2 \
    -metadir $ST/$w -config $w.cfg ForgeSync.tla > out/$w.out 2>&1; rc=$?
  got=$(grep -oE "Invariant [A-Za-z_]+ is violated|Temporal properties were violated|No error has been found|Error: [^.]*" out/$w.out | head -1)
  v=MISMATCH
  if [ $rc = 124 ]; then v=UNDECIDED-TIMEOUT
  elif [ "$exp" = HOLDS ] && [ "$got" = "No error has been found" ]; then v=OK-HOLDS
  elif [ "$exp" != HOLDS ] && echo "$got" | grep -qE "Invariant $exp is violated"; then v=OK-FIRES; fi
  printf "%-34s %-18s exp=%-40s rc=%-3s | %s | %s | %s\n" "$w" "$v" "$exp" "$rc" "$got" \
    "$(grep -oE '^[0-9,]+ states generated, [0-9,]+ distinct' out/$w.out | tail -1)" \
    "$(grep -oE 'Finished in [0-9a-z ]+' out/$w.out | tail -1)" >> RESULTS.txt
  rm -rf $ST/$w
done < <(awk -F'\t' '$2!="HOLDS"' WORLDS-ForgeSyncShipped.tsv; awk -F'\t' '$2=="HOLDS"' WORLDS-ForgeSyncShipped.tsv)
echo FORGESHIPPEDDONE >> RESULTS.txt
