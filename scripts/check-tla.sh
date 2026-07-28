#!/usr/bin/env bash
# TLC gate for the formal model (formal/FlintReplication.tla).
#
# Five runs, ALL required:
#   1. FlintReplication.cfg       strict 3-leg breadth — every invariant and
#      the post-storm liveness must HOLD.
#   2. FlintReplicationDeep.cfg   strict 2-leg deep budget (torn writes,
#      hot-rejoin divergence, Scrub demotion all reachable) — must HOLD.
#   3. FlintReplicationF36c.cfg   GateStrict=FALSE  — TLC must FIND an
#      Inv_NoSilentLoss violation (the 6-write-tail loss).
#   4. FlintReplicationRejoin.cfg RejoinGuard=FALSE — TLC must FIND an
#      Inv_NoDivergentServing violation (dead-lineage phantom served).
#   5. FlintReplicationF48.cfg    FenceZombie=FALSE — TLC must FIND a
#      zombie-head violation (silent loss or split-brain divergence).
#
# A model that cannot rediscover the bug classes it exists for proves
# nothing; runs 3-5 are the model's own regression tests.
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

strict_run() { # <cfg> <label>
  echo "== $2 ($1): invariants + liveness must hold =="
  local OUT
  OUT=$(run_tlc "$1") || { echo "$OUT" | tail -30; echo "FAIL: $2 errored"; exit 1; }
  echo "$OUT" | grep -q "Model checking completed. No error has been found." \
    || { echo "$OUT" | tail -30; echo "FAIL: $2 did not verify"; exit 1; }
  echo "$OUT" | grep -E "distinct states|depth" | head -2
}

mutation_run() { # <cfg> <label> <expected-violation-regex>
  echo "== $2 ($1): TLC must FIND the loss =="
  local MOUT
  MOUT=$(run_tlc "$1" || true)
  echo "$MOUT" | grep -Eq "Invariant $3 is violated" \
    || { echo "$MOUT" | tail -30; echo "FAIL: $2 did NOT find the loss — the model lost its teeth"; exit 1; }
  echo "counterexample found (as required)"
}

strict_run FlintReplication.cfg     "strict breadth (GateStrict, RejoinGuard, FenceZombie all TRUE)"
strict_run FlintReplicationDeep.cfg "strict deep budget (scrub/divergence reachable)"

mutation_run FlintReplicationF36c.cfg   "F36c mutation (GateStrict=FALSE)"   "Inv_NoSilentLoss"
mutation_run FlintReplicationRejoin.cfg "rejoin mutation (RejoinGuard=FALSE)" "Inv_NoDivergentServing"
mutation_run FlintReplicationF48.cfg    "F48 mutation (FenceZombie=FALSE)"   "Inv_(NoSilentLoss|NoDivergentServing)"

echo "TLA GATE PASSED"
