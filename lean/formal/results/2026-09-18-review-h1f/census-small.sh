#!/bin/bash
# The census worlds the gate does not print counts for (it keeps only a
# failing run's output): the seven worlds of the README census, the two
# small sweep/straggler worlds.  Small heap, two workers, niced: it runs
# beside the gate.
cd ~/lean-gate-2026-09-18d/lean/formal
JAR=~/lean-gate-2026-09-18/.tla2tools.jar
R=results/2026-09-18-review-h1f/census
for c in LeanSubtreeTakeover LeanEpochOnlyHolds LeanBarrierLeaseDeposal LeanBarrierLeaseSameBytesVerified LeanRemovalHolds LeanBarrierLeaseQueueHolds LeanBarrierLeaseOrphanTracked LeanBarrierLeaseStragglerGCFenced; do
  echo "== $c $(date -u +%H:%M:%S)"
  nice -n 10 java -Xmx3g -XX:+UseParallelGC -cp $JAR tlc2.TLC -workers 2 -config $c.cfg -metadir states/census-$c LeanSubtree.tla > $R/$c.log 2>&1
  echo "rc=$?"; grep -E "distinct states found, 0 states|Error: Invariant|depth of the complete" $R/$c.log | head -2
done
echo CENSUSSMALLDONE
