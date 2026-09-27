#!/usr/bin/env bash
# ForgeSyncNeeded's six worlds, the teeth first. 2 workers: gate d has 6.
cd "$(dirname "$0")"
JAR=~/lean-gate-2026-09-18/.tla2tools.jar
: > RESULTS.txt
while IFS=$'\t' read -r w exp; do
  mkdir -p states/$w
  java -Xmx6g -XX:+UseParallelGC -cp "$JAR" tlc2.TLC -workers 2 -metadir states/$w \
    -config $w.cfg ForgeSyncNeeded.tla > $w.out 2>&1; rc=$?
  inv=$(grep -o "Invariant [A-Za-z_]* is violated" $w.out | head -1)
  cnt=$(grep -o "[0-9]* states generated, [0-9]* distinct" $w.out | tail -1)
  if [ "$exp" = HOLDS ]; then
    [ $rc = 0 ] && v=OK-HOLDS || v=MISMATCH
  else
    [ "$inv" = "Invariant $exp is violated" ] && v=OK-FOUND || v=MISMATCH
  fi
  printf '%-36s %-9s exp=%-24s rc=%-3s | %s | %s | %s\n' "$w" "$v" "$exp" "$rc" "$inv" "$cnt" \
    "$(grep -o 'Finished in .*' $w.out | head -1)" >> RESULTS.txt
  rm -rf states/$w
done < <(awk -F'\t' '$2!="HOLDS"' WORLDS.tsv; awk -F'\t' '$2=="HOLDS"' WORLDS.tsv)
echo FORGENEEDEDDONE >> RESULTS.txt
