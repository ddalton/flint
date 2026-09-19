#!/bin/bash
cd ~/lean-gate-2026-09-18b/lean/formal
JAR=~/lean-gate-2026-09-18/.tla2tools.jar
R=results/2026-09-18-review-h1b
for c in LeanBarrierLeaseOrphanResurrects LeanBarrierLeaseOrphanTracked LeanProbeOrphanTracked LeanProbeOrphanOutlived; do
  echo "== $c $(date -u +%H:%M:%S)"
  java -Xmx6g -XX:+UseParallelGC -cp $JAR tlc2.TLC -workers 4 -config $c.cfg -metadir states/$c LeanSubtree.tla > $R/$c.log 2>&1
  echo "rc=$?"; grep -E "distinct states found, 0 states|Error: Invariant|depth of the complete|Error: Parsing|Error: TLC" $R/$c.log | head -3
done
echo ORPHANDONE
