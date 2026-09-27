#!/usr/bin/env bash
# The rewind worlds, started ONLY if ForgeSyncNeededReachable held
# (the user's condition, 2026-09-26). Teeth and probes first. 2 workers.
cd "$(dirname "$0")"
until grep -q FORGENEEDEDDONE RESULTS.txt 2>/dev/null; do sleep 120; done
if ! grep -q "^ForgeSyncNeededReachable  *OK-HOLDS" RESULTS.txt; then
  echo "NOT STARTED: ForgeSyncNeededReachable did not hold" > RESULTS-Rewind.txt
  echo FORGEREWINDDONE >> RESULTS-Rewind.txt; exit 0
fi
JAR=~/lean-gate-2026-09-18/.tla2tools.jar
touch RESULTS-Rewind.txt
while IFS=$'\t' read -r w exp; do
  mkdir -p /mnt/nvme2/forge-rewind/$w
  java -Xmx6g -XX:+UseParallelGC -cp "$JAR" tlc2.TLC -workers 2 -metadir /mnt/nvme2/forge-rewind/$w \
    -config $w.cfg ForgeSyncRewind.tla > $w.out 2>&1; rc=$?
  inv=$(grep -o "Invariant [A-Za-z_]* is violated" $w.out | head -1)
  cnt=$(grep -o "[0-9]* states generated, [0-9]* distinct" $w.out | tail -1)
  case $exp in
    HOLDS) [ $rc = 0 ] && v=OK-HOLDS || v=MISMATCH ;;
    RECORD) v=RECORDED ;;
    *) [ "$inv" = "Invariant $exp is violated" ] && v=OK-FOUND || v=MISMATCH ;;
  esac
  printf '%-36s %-9s exp=%-24s rc=%-3s | %s | %s | %s\n' "$w" "$v" "$exp" "$rc" "$inv" "$cnt" \
    "$(grep -o 'Finished in .*' $w.out | head -1)" >> RESULTS-Rewind.txt
  rm -rf /mnt/nvme2/forge-rewind/$w
done < <(grep -v -F -f <(awk '{print $1" "}' RESULTS-Rewind.txt | sed 's/ $//') WORLDS-Rewind.tsv \
         | awk -F'\t' '$2!="HOLDS" && $2!="RECORD"'; awk -F'\t' '$1 ~ /Holds$/' WORLDS-Rewind.tsv; awk -F'\t' '$1 ~ /Strict$/' WORLDS-Rewind.tsv)
echo FORGEREWINDDONE >> RESULTS-Rewind.txt
