#!/bin/bash
# r39 — SANDBOX (not the gate tree): LeanSubtree.tla with the one ASSUME
# conjunct `MaxNarrows = 0` dropped from `ImmutableObjects => ...`, md5
# ad0d48de0c1f4b1f73943db237d9efb4. Question: do the narrow worlds, on the
# code's shape with handles, have red controls and a firing probe? (H8: the
# narrow verb ships — verbs.rs — and was checked only on the life lease.)
set -u
OUT=~/lean-core-r3/r39; mkdir -p $OUT; RES=$OUT/RESULTS.txt; : > $RES
cd ~/lean-narrow-sandbox2/formal || exit 1
[ "$(md5sum LeanSubtree.tla | cut -c1-32)" = ad0d48de0c1f4b1f73943db237d9efb4 ] || { echo "SANDBOX MD5 CHANGED" >> $RES; exit 4; }
JAR=~/lean-gate-2026-09-18/.tla2tools.jar
ST=/mnt/nvme/tlc-r39; rm -rf $ST; mkdir -p $ST
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
# r39: sandbox2 = sandbox + the narrowRecited stamp scoped to the uploader (4 sites).
run LeanImmutableNarrowUncieFirst "Invariant Inv_NarrowNeverRecites is violated" 3600 2g
run LeanNarrowUncieFirst          "Invariant Inv_NarrowNeverRecites is violated" 3600 2g
run LeanImmutableNarrowUnlinkFirst "Invariant Inv_NarrowNeverDeletes is violated" 3600 2g
run LeanNarrowUnlinkFirst         "Invariant Inv_NarrowNeverDeletes is violated" 3600 2g
run LeanNarrowHolds               HOLDS 7200 3g
run LeanImmutableNarrowHolds      HOLDS 43200 3g
echo R39DONE >> $RES
