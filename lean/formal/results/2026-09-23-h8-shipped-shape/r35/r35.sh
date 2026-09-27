#!/bin/bash
# r35 — H8, the ACK rows on the shipped (handles) shape. New cfgs from
# gen-cfgs.sh 2026-09-23. Controls FIRST: the probe must be violated (an ack
# written off a real install) and each mutation must violate its invariant,
# else a green holds world proves only that the stamps went quiet.
# Runs NICE 19 on 2 workers beside r34 (3-path, 8 workers, /mnt/nvme2).
set -u
cd ~/lean-io-sent/formal || exit 1
[ "$(md5sum LeanSubtree.tla | cut -c1-32)" = d2dd061afb75ee6d937bf22d20d0268b ] || { echo "MODULE MD5 CHANGED" > ~/lean-core-r3/r35/RESULTS.txt; exit 4; }
JAR=~/lean-gate-2026-09-18/.tla2tools.jar
OUT=~/lean-core-r3/r35; mkdir -p $OUT; RES=$OUT/RESULTS.txt; : > $RES
ST=/mnt/nvme/tlc-r35; rm -rf $ST; mkdir -p $ST
run() { # <cfg> <expected: "Invariant X is violated" or HOLDS> <timeout> <heap>
  local w=$1 exp=$2 to=$3 heap=$4
  timeout $to nice -n 19 java -XX:+UseParallelGC -Xmx$heap -cp $JAR tlc2.TLC -workers 2 \
    -metadir $ST/$w -config $w.cfg LeanSubtree.tla > $OUT/$w.out 2>&1
  local rc=$? got
  got=$(grep -oE "Invariant [A-Za-z_]+ is violated|No error has been found" $OUT/$w.out | head -1)
  local verdict=MISMATCH
  if [ $rc = 124 ]; then verdict=UNDECIDED-TIMEOUT
  elif [ "$exp" = HOLDS ] && [ "$got" = "No error has been found" ]; then verdict=OK-HOLDS
  elif [ "$got" = "$exp" ]; then verdict=OK-FIRES; fi
  printf "%-40s %-18s rc=%s | %s | %s | %s\n" "$w" "$verdict" "$rc" "$got" \
    "$(grep -oE '^[0-9,]+ states generated, [0-9,]+ distinct' $OUT/$w.out | tail -1)" \
    "$(grep -oE 'depth of the complete state graph search is [0-9]+|^[0-9]+\. State|Finished in [0-9a-z ]+' $OUT/$w.out | tail -1)" >> $RES
}
run LeanImmutableProbeSentinelHonored "Invariant ProbeSentinelHonored is violated" 3600 3g
run LeanImmutableSentinelQueueDropped "Invariant Inv_AckBoundaryCoherent is violated" 3600 3g
run LeanImmutableSentinelOutrankedOk  "Invariant Inv_AckImpliesCited is violated" 3600 3g
run LeanImmutableSentinelUnstamped    "Invariant Inv_BoundaryNamesItsClock is violated" 3600 3g
run LeanImmutableSentinelImpl1        HOLDS 43200 6g
echo R35DONE >> $RES
