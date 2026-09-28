#!/bin/bash
# LeanP1LiveHolds, resumed (2026-09-28). The first attempt ran at -Xmx12g and
# spent its time in GC (8 GC threads ~77 CPU-min each, 366K -> 44K ds/min,
# the liveness pass 10 min -> 60 min); it was stopped after its 21:05
# checkpoint. The user chose 20 GB. No timeout: the gate runner's 6 h `timeout` would kill it and
# then delete the checkpoint. The metadir is deleted only on a verdict.
# Ends with LEANP1DONE (the deep run waits for it) only on a verdict.
set -u
cd ~/lean-leanp1-2026-09-25d || exit 1
[ "$(md5sum LeanP1.tla | cut -c1-32)" = 3f6af642f7e39c19cafa413540e50cbe ] || { echo "MD5 CHANGED LeanP1.tla" >> RESULTS.txt; exit 4; }
# 22:55: started NOW, beside the small worlds (the user's call, to shorten
# the gate): it is the long pole. The small-worlds runner was moved to
# tlc-rs, whose seen set spills past 1 GB, so the two fit beside 20 GB.
JAR=~/lean-gate-2026-09-18/.tla2tools.jar
wld=LeanP1LiveHolds; exp=$(awk -F'\t' -v w=$wld '$1==w{print $2}' WORLDS-LeanP1.tsv)
ST=/mnt/nvme/tlc-leanp1d/$wld
CK=$(ls -d $ST/*/ | head -1); CK=${CK%/}
[ -e "$CK/queue.chkpt" ] || { echo "$wld NO CHECKPOINT in $ST" >> RESULTS.txt; exit 5; }
echo "recover $(date -u +%FT%TZ) from $CK, -Xmx20g, 6 workers" >> runner-livehold.log
nice -n 10 java -XX:+UseParallelGC -Xmx20g -cp $JAR tlc2.TLC -workers 6 \
  -metadir $ST -recover $CK -config $wld.cfg LeanP1.tla > out/$wld-recovered.out 2>&1
rc=$?
got=$(grep -oE "Invariant [A-Za-z_]+ is violated|Action property [A-Za-z_]+ is violated|Temporal properties were violated|No error has been found|Error: [^.]*" out/$wld-recovered.out | head -1)
v=MISMATCH
if [ "$exp" = HOLDS ] && [ "$got" = "No error has been found" ]; then v=OK-HOLDS
elif [ "$exp" != HOLDS ] && echo "$got" | grep -q "$exp"; then v=OK-FIRES; fi
printf "%-30s %-18s exp=%-22s rc=%-3s | %s | %s | %s (recovered from the 21:05 checkpoint, 20 GB)\n" "$wld" "$v" "$exp" "$rc" "$got" \
  "$(grep -oE '^[0-9,]+ states generated, [0-9,]+ distinct' out/$wld-recovered.out | tail -1)" \
  "$(grep -oE 'depth of the complete state graph search is [0-9]+|Finished in [0-9a-z ]+' out/$wld-recovered.out | tail -1)" >> RESULTS.txt
# Only a verdict ends the gate: without one (OOM, a kill) the checkpoint is
# kept and LEANP1DONE is NOT written, so the deep run does not start early.
case "$got" in
  "No error has been found"|*violated*) rm -rf $ST
    # The gate is done when BOTH halves are: this world and the small ones.
    until grep -q SMALLDONE RESULTS.txt 2>/dev/null; do sleep 60; done
    echo LEANP1DONE >> RESULTS.txt ;;
  *) echo "$wld STOPPED WITHOUT A VERDICT $(date -u +%FT%TZ); checkpoint kept in $ST" >> RESULTS.txt ;;
esac
