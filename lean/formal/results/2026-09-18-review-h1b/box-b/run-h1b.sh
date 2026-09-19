#!/bin/bash
cd ~/lean-gate-2026-09-18b/lean/formal
JAR=~/lean-gate-2026-09-18/.tla2tools.jar
R=results/2026-09-18-review-h1b; mkdir -p $R
run() { c=$1; heap=$2; w=$3
  echo "== $c $(date -u +%H:%M:%S)"
  java -Xmx$heap -XX:+UseParallelGC -cp $JAR tlc2.TLC -workers $w -config $c.cfg -metadir states/$c LeanSubtree.tla > $R/$c.log 2>&1
  echo "rc=$?"; grep -E "distinct states found, 0 states|Error: Invariant|depth of the complete|Error: Parsing|Error: TLC|Assumption" $R/$c.log | head -3
}
for c in LeanBarrierLeaseOrphanResurrects LeanBarrierLeaseOrphanTracked LeanProbeOrphanTracked LeanProbeOrphanOutlived LeanBarrierLeaseOrphanStaleCopy LeanBarrierLeaseQueueTombstoneOverHitl LeanBarrierLeaseLeakResurrects LeanBarrierLeaseLeakHolds LeanBarrierLeaseQueueHolds LeanBarrierLeaseStragglerGCFenced; do run $c 8g 8; done
for c in LeanBarrierLeaseCollectorOff LeanBarrierLeaseImplHolds LeanBarrierLeaseInboxSnapshot LeanBarrierLeaseImplThreeWriters; do run $c 12g 8; done
echo H1BDONE
