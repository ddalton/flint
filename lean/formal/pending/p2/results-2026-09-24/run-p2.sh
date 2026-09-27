#!/bin/bash
# p2 run 2 — LeanCoreP2 md5 5bc68fae…, every world in WORLDS.tsv scored
# against the expectation written BEFORE the run.
set -u
cd ~/lean-p2-2026-09-24 || exit 1
[ "$(md5sum LeanCoreP2.tla | cut -c1-32)" = c82cbb811a316222a2fd57495cbe03e5 ] || { echo "MD5 CHANGED" > RESULTS.txt; exit 4; }
JAR=~/lean-gate-2026-09-18/.tla2tools.jar
ST=/mnt/nvme/tlc-p2; rm -rf $ST; mkdir -p $ST out; : > RESULTS.txt
order="P2RenameTwoCAS P2SweepNoGrace P2GatewayBlind P2CommitBlind P2VerifyOff P2SweepFree P2ProbeStalledSave P2ProbeSavedUnderLease P2ProbeRenamed P2ProbeSurfaced P2LiveWaitsOnLease P2ForeignFlat P2Holds P2LiveHolds"
for wld in $order; do
  exp=$(awk -F'\t' -v w=$wld '$1==w{print $2}' WORLDS.tsv)
  timeout 21600 nice -n 19 java -XX:+UseParallelGC -Xmx4g -cp $JAR tlc2.TLC -workers 2 \
    -metadir $ST/$wld -config $wld.cfg LeanCoreP2.tla > out/$wld.out 2>&1
  rc=$?
  got=$(grep -oE "Invariant [A-Za-z_]+ is violated|Action property [A-Za-z_]+ is violated|Temporal properties were violated|No error has been found|Error: [^.]*" out/$wld.out | head -1)
  v=MISMATCH
  if [ $rc = 124 ]; then v=UNDECIDED-TIMEOUT
  elif [ "$exp" = HOLDS ] && [ "$got" = "No error has been found" ]; then v=OK-HOLDS
  elif [ "$exp" = "?" ]; then v=RECORDED
  elif [ "$exp" != HOLDS ] && echo "$got" | grep -q "$exp"; then v=OK-FIRES; fi
  printf "%-24s %-18s exp=%-22s rc=%-3s | %s | %s | %s\n" "$wld" "$v" "$exp" "$rc" "$got" \
    "$(grep -oE '^[0-9,]+ states generated, [0-9,]+ distinct' out/$wld.out | tail -1)" \
    "$(grep -oE 'depth of the complete state graph search is [0-9]+|Finished in [0-9a-z ]+' out/$wld.out | tail -1)" >> RESULTS.txt
done
echo P2DONE >> RESULTS.txt
