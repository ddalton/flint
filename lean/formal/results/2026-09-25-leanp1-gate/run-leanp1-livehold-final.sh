#!/bin/bash
# LeanP1LiveHolds from scratch (2026-09-28 23:00), -lncheck final. The resume
# from the 21:05 checkpoint died at once: EOFException in
# TableauDiskGraph.makeNodePtrTbl -- the liveness disk graph had been written
# past the checkpoint (the run was stopped ~21:16), so TLC cannot recover a
# liveness run from it. The old metadir is kept for the record, not used.
# -lncheck final: one liveness check at the end instead of periodic passes
# that grew 10 -> 60 min; a violation is still reported, at the end.
# No checkpoints (they cannot resume liveness); a stop means a restart.
# 20 GB, 6 workers (the user's choices). LEANP1DONE only on a verdict AND
# after SMALLDONE.
set -u
cd ~/lean-leanp1-2026-09-25d || exit 1
[ "$(md5sum LeanP1.tla | cut -c1-32)" = 3f6af642f7e39c19cafa413540e50cbe ] || { echo "MD5 CHANGED LeanP1.tla" >> RESULTS.txt; exit 4; }
JAR=~/lean-gate-2026-09-18/.tla2tools.jar
wld=LeanP1LiveHolds; exp=$(awk -F'\t' -v w=$wld '$1==w{print $2}' WORLDS-LeanP1.tsv)
ST=/mnt/nvme/tlc-leanp1d-final/$wld; rm -rf $ST; mkdir -p $ST
echo "fresh $(date -u +%FT%TZ) -lncheck final -checkpoint 0, -Xmx20g, 6 workers" >> runner-livehold.log
nice -n 10 java -XX:+UseParallelGC -Xmx20g -cp $JAR tlc2.TLC -workers 6 -lncheck final -checkpoint 0 \
  -metadir $ST -config $wld.cfg LeanP1.tla > out/$wld-final.out 2>&1
rc=$?
got=$(grep -oE "Invariant [A-Za-z_]+ is violated|Action property [A-Za-z_]+ is violated|Temporal properties were violated|No error has been found|Error: [^.]*" out/$wld-final.out | head -1)
v=MISMATCH
if [ "$exp" = HOLDS ] && [ "$got" = "No error has been found" ]; then v=OK-HOLDS
elif [ "$exp" != HOLDS ] && echo "$got" | grep -q "$exp"; then v=OK-FIRES; fi
printf "%-30s %-18s exp=%-22s rc=%-3s | %s | %s | %s (fresh, -lncheck final, 20 GB)\n" "$wld" "$v" "$exp" "$rc" "$got" \
  "$(grep -oE '^[0-9,]+ states generated, [0-9,]+ distinct' out/$wld-final.out | tail -1)" \
  "$(grep -oE 'depth of the complete state graph search is [0-9]+|Finished in [0-9a-z ]+' out/$wld-final.out | tail -1)" >> RESULTS.txt
rm -rf $ST
case "$got" in
  "No error has been found"|*violated*)
    until grep -q SMALLDONE RESULTS.txt 2>/dev/null; do sleep 60; done
    echo LEANP1DONE >> RESULTS.txt ;;
  *) echo "$wld STOPPED WITHOUT A VERDICT $(date -u +%FT%TZ)" >> RESULTS.txt ;;
esac
