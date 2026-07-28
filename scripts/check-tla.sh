#!/usr/bin/env bash
# TLC gate for the formal model (formal/FlintReplication.tla).
#
# Two runs, BOTH required:
#   1. FlintReplication.cfg     (GateStrict=TRUE)  — every invariant and the
#      post-storm liveness property must HOLD.
#   2. FlintReplicationF36c.cfg (GateStrict=FALSE) — TLC must FIND an
#      Inv_NoSilentLoss violation. A model that cannot rediscover the bug
#      class it exists for proves nothing; this run is the model's own
#      regression test.
#
# tla2tools.jar is fetched on first use (cached; override with TLA_TOOLS_JAR).
set -euo pipefail
cd "$(dirname "$0")/../formal"

JAR=${TLA_TOOLS_JAR:-.tla2tools.jar}
if [ ! -f "$JAR" ]; then
  echo "fetching tla2tools.jar..."
  curl -fsSL -o "$JAR" \
    https://github.com/tlaplus/tlaplus/releases/latest/download/tla2tools.jar
fi

run_tlc() { # <cfg>
  java -XX:+UseParallelGC -cp "$JAR" tlc2.TLC -workers auto \
    -config "$1" FlintReplication.tla 2>&1
}

echo "== strict model (GateStrict=TRUE): invariants + liveness must hold =="
OUT=$(run_tlc FlintReplication.cfg) || { echo "$OUT" | tail -30; echo "FAIL: strict run errored"; exit 1; }
echo "$OUT" | grep -q "Model checking completed. No error has been found." \
  || { echo "$OUT" | tail -30; echo "FAIL: strict model did not verify"; exit 1; }
echo "$OUT" | grep -E "distinct states|depth" | head -2

echo "== mutation (GateStrict=FALSE): TLC must FIND the F36c loss =="
MOUT=$(run_tlc FlintReplicationF36c.cfg || true)
echo "$MOUT" | grep -q "Invariant Inv_NoSilentLoss is violated" \
  || { echo "$MOUT" | tail -30; echo "FAIL: mutation run did NOT find the loss — the model lost its teeth"; exit 1; }
echo "counterexample found (as required)"

echo "TLA GATE PASSED"
