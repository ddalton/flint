#!/bin/bash
# The final batch on the H1b/H1c module: the four worlds the base-at-CAS arm
# decides, then the 130-run gate, the trace replays, and the crash world at
# OrphanTrack=TRUE (SAFETY §4.4, both arms).
cd ~/lean-gate-2026-09-18b/lean/formal
JAR=~/lean-gate-2026-09-18/.tla2tools.jar
R=results/2026-09-18-review-h1b; mkdir -p $R
run() { c=$1
  echo "== $c $(date -u +%H:%M:%S)"
  java -Xmx12g -XX:+UseParallelGC -cp $JAR tlc2.TLC -workers 8 -config $c.cfg -metadir states/$c LeanSubtree.tla > $R/$c.log 2>&1
  echo "rc=$?"; grep -E "distinct states found, 0 states|Error: Invariant|depth of the complete|Error: Parsing|Error: TLC|Assumption" $R/$c.log | head -3
}
expect() { c=$1; want=$2
  echo "== $c $(date -u +%H:%M:%S)"
  java -Xmx12g -XX:+UseParallelGC -cp $JAR tlc2.TLC -workers 8 -config $c.cfg -metadir states/$c LeanSubtree.tla > $R/$c.log 2>&1
  rc=$?; echo "rc=$rc"; grep -E "distinct states found, 0 states|Error: Invariant|depth of the complete|Error: Parsing|Error: TLC|Assumption" $R/$c.log | head -3
  if [ "$want" = hold ] && [ $rc -ne 0 ]; then echo "UNEXPECTED: $c should hold"; BAD=1; fi
  if [ "$want" = fail ] && ! grep -q "Error: Invariant" $R/$c.log; then echo "UNEXPECTED: $c should violate"; BAD=1; fi
}
BAD=0
expect LeanBarrierLeaseCollectorOffNoTombstone fail
expect LeanBarrierLeaseCollectorOff hold
expect LeanBarrierLeaseImplHolds hold
expect LeanBarrierLeaseInboxSnapshot hold
expect LeanBarrierLeaseOrphanStaleCopy fail
expect LeanBarrierLeaseOrphanConverges hold
echo "SEVENDONE bad=$BAD"
[ $BAD -eq 0 ] || { echo "stopping before the gate"; exit 1; }
echo "== gate $(date -u +%H:%M:%S)"
TLC_HEAP=12g TLC_WORKERS=8 TLA_TOOLS_JAR=$JAR bash check.sh > $R/gate.txt 2>&1; echo "gate rc=$? $(date -u +%H:%M:%S)"
grep -E "^FAIL|runs green|EXPECT" $R/gate.txt | tail -5
echo "== trace-check $(date -u +%H:%M:%S)"
TLA_TOOLS_JAR=$JAR bash trace/trace-check.sh > $R/trace-check.txt 2>&1; echo "trace-check rc=$?"; tail -3 $R/trace-check.txt
echo GATEDONE
echo "== crash1 $(date -u +%H:%M:%S)"
R2=results/2026-09-18-crash1-orphantrack; mkdir -p $R2
sed "s/OrphanTrack = FALSE/OrphanTrack = TRUE/" LeanBarrierLeaseSentinelImplCrash1.cfg > $R2/Crash1OrphanTrue.cfg
cp LeanBarrierLeaseSentinelImplCrash1.cfg $R2/Crash1OrphanFalse.cfg
for arm in Crash1OrphanFalse Crash1OrphanTrue; do
  echo "== $arm $(date -u +%H:%M:%S)"
  java -Xmx20g -XX:+UseParallelGC -cp $JAR tlc2.TLC -workers 8 -checkpoint 30 -config $R2/$arm.cfg -metadir states/$arm LeanSubtree.tla > $R2/$arm.log 2>&1
  echo "rc=$?"; grep -E "distinct states found, 0 states|Error: Invariant|depth of the complete" $R2/$arm.log | head -3
done
echo CRASH1DONE
