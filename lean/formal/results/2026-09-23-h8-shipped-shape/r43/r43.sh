#!/bin/bash
# r43 — SANDBOX (not the gate tree): LeanSubtree.tla with the one ASSUME
# conjunct `MaxNarrows = 0` dropped from `ImmutableObjects => ...`, md5
# 6f0b6bbc18de6758009abd7b657363eb. Question: do the narrow worlds, on the
# code's shape with handles, have red controls and a firing probe? (H8: the
# narrow verb ships — verbs.rs — and was checked only on the life lease.)
set -u
OUT=~/lean-core-r3/r43; mkdir -p $OUT; RES=$OUT/RESULTS.txt; : > $RES
cd ~/lean-syncq-sandbox/formal || exit 1
[ "$(md5sum LeanSubtree.tla | cut -c1-32)" = 6f0b6bbc18de6758009abd7b657363eb ] || { echo "SANDBOX MD5 CHANGED" >> $RES; exit 4; }
JAR=~/lean-gate-2026-09-18/.tla2tools.jar
ST=/mnt/nvme/tlc-r43; rm -rf $ST; mkdir -p $ST
run() {
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
    "$(grep -oE 'depth of the complete state graph search is [0-9]+|Finished in [0-9a-z ]+' $OUT/$w.out | tail -1)" >> $RES
}
# r43: the queued-change-behind-a-sync defect (tests queued_change_behind_a_sync). SANDBOX4 = gate module + consumeRegressed ghost + QueueYieldsToSync arm.
run LeanImmutableSyncQueueRegress "Invariant Inv_ConsumeNeverRegresses is violated" 3600 2g
run LeanImmutableSyncQueueShippedInvs HOLDS 3600 2g
run LeanImmutableSyncQueueHolds HOLDS 7200 3g
echo R43DONE >> $RES
