#!/bin/bash
# Gate d's 20 decisive worlds were decided on tlc-rs 92c7a987 only
# (2026-09-29, RESULTS-d-2026-09-29.txt), a build before the queue-order
# fix. This re-decides each on TLC, the gate's reference checker, so no
# verdict rests on the checker under test. Small worlds: runs on the Mac.
# Usage: run-leanp1-tlc-crosscheck.sh <jar> <states dir>
set -u
cd "$(dirname "$0")/../.." || exit 1          # lean/formal
JAR=$1; ST=$2; mkdir -p "$ST"
R=results/2026-09-25-leanp1-gate/RESULTS-d-tlc-crosscheck-2026-09-29.txt
O=results/2026-09-25-leanp1-gate/out-tlc-crosscheck; mkdir -p $O
[ "$(md5 -q LeanP1.tla 2>/dev/null || md5sum LeanP1.tla | cut -c1-32)" = 3f6af642f7e39c19cafa413540e50cbe ] || { echo "MD5 CHANGED LeanP1.tla" >> $R; exit 4; }
echo "TLC $(basename "$JAR"), LeanP1.tla 3f6af642, 2 workers, -Xmx2g, 1800 s a world; started $(date '+%F %T')" >> $R
for x in DeleteOutranked OwedUnmarked AdvanceUnguarded ReaderLoses LiveWaitsOnLease GatewayBlind \
         SweepNoGrace RenameTwoCAS ProbeStalledSave ProbeSavedUnderLease ProbeSaveRefused ProbeRenamed \
         ProbeSurfaced ProbeDeleteOverridden ProbeDeleteOverTheirs ProbeShortcut ProbeReaped \
         ProbeConverged ProbeRepublish ProbeRestartAfterCas; do
  w=LeanP1$x
  grep -qE "^$w +(OK-|MISMATCH)" $R && continue
  exp=$(awk -F'\t' -v w=$w '$1==w{print $2}' results/2026-09-25-leanp1-gate/WORLDS-LeanP1.tsv)
  rm -rf "$ST/$w"; t0=$(date +%s); why=""
  java -XX:+UseParallelGC -Xmx2g -cp "$JAR" tlc2.TLC -workers 2 -checkpoint 0 \
    -metadir "$ST/$w" -config $w.cfg LeanP1.tla > $O/$w.out 2>&1 &
  pid=$!
  while kill -0 $pid 2>/dev/null; do
    sleep 5
    [ $(( $(date +%s) - t0 )) -gt 1800 ] && { why=UNDECIDED-TIMEOUT; kill $pid; break; }
  done
  wait $pid; rc=$?
  got=$(grep -oE "Invariant [A-Za-z_]+ is violated|Action property [A-Za-z_]+ is violated|Temporal propert[a-z]* [^.]*violated|No error has been found|^Error: [^.]*" $O/$w.out | head -1)
  v=MISMATCH
  if [ -n "$why" ]; then v=$why
  elif [ "$exp" = Temporal ] && echo "$got" | grep -q "Temporal"; then v=OK-FIRES
  elif [ "$exp" != HOLDS ] && echo "$got" | grep -qE "(Invariant|property) $exp is violated"; then v=OK-FIRES; fi
  printf "%-30s %-18s exp=%-22s rc=%-3s | %s | %s | %ss [TLC]\n" "$w" "$v" "$exp" "$rc" "$got" \
    "$(grep -oE '^[0-9,]+ states generated, [0-9,]+ distinct states found' $O/$w.out | tail -1)" \
    $(( $(date +%s) - t0 )) >> $R
  rm -rf "$ST/$w"
done
echo CROSSCHECKDONE >> $R
