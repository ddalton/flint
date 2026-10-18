#!/usr/bin/env bash
# storm-check.sh <run-dir> -- a multi-writer run, replayed against LeanP1.tla.
#
# storm2conf.py merges the writers' traces and the agents' journals into one
# conformance trace (the order the shared state forces); ndjson2core.py and
# TraceCore.tla check it as they check a scenario: it must be ACCEPTED, with
# the model's claims holding along it. Then mutate_storm.py corrupts one
# multi-writer fact at a time, and every mutation must NOT be accepted:
# refused at conversion, a replay that stops, or a claim violated.
#
# The checker is tla-mc (`$TLAMC` or PATH) unless TRACE_CHECKER=tlc
# (TLA_TOOLS_JAR), as trace-check-core.sh. Work in $TRACE_WORK (a temp dir).
set -u
cd "$(dirname "$0")"
RUN=${1:?usage: storm-check.sh <run-dir>}
CHECKER="${TRACE_CHECKER:-tla-mc}"
TLAMC="${TLAMC:-tla-mc}"
JAR="${TLA_TOOLS_JAR:-../../../.tla2tools.jar}"
WORK="${TRACE_WORK:-$(mktemp -d)}"
FAIL=0
mkdir -p "$WORK"

verdict() { # <ndjson> <dir> -> accepted | rejected:<why> | unsafe:<what>
  local src=$1 out=$2
  rm -rf "$out"; mkdir -p "$out"
  if ! python3 ndjson2core.py "$src" "$out" > "$out/convert.txt"; then
    echo "rejected:conversion ($(tail -1 "$out/convert.txt" | cut -c1-120))"; return
  fi
  cp ../LeanP1.tla TraceCore.tla "$out/"
  if [ "$CHECKER" = tlc ]; then
    (cd "$out" && java -XX:+UseParallelGC -cp "$JAR" tlc2.TLC -workers 1 \
        -metadir states -config TraceCore.cfg TraceCoreData.tla > tlc.log 2>&1)
  else
    python3 strip-progress.py "$out" || { echo "rejected:strip"; return; }
    (cd "$out" && "$TLAMC" -workers 1 -config TraceCore.cfg TraceCoreData.tla > tlc.log 2>&1)
  fi
  if grep -q "Invariant TraceIncomplete is violated" "$out/tlc.log"; then
    echo accepted
  elif grep -qE "(Invariant|property|Property) [A-Za-z0-9_]+ is violated" "$out/tlc.log"; then
    echo "unsafe:$(grep -oE '(Invariant|property|Property) [A-Za-z0-9_]+ is violated' "$out/tlc.log" | head -1)"
  else
    # A replay that stops: the checker explored every state the trace lets
    # it reach and none followed it to the end. Name how far it got (a
    # parse or config error has no depth line, and is not a rejection).
    local d; d=$(grep -oE 'Depth [0-9]+' "$out/tlc.log" | tail -1)
    if [ -z "$d" ] || ! grep -q "No error has been found" "$out/tlc.log"; then
      echo "error:the checker did not run ($out/tlc.log)"; return
    fi
    echo "rejected:replay stops at step ${d#Depth } of $(sed -n 's/.*-> \([0-9]*\) model steps.*/\1/p' "$out/convert.txt")"
  fi
}

python3 storm2conf.py "$RUN" "$WORK/run.ndjson" || exit 2
v=$(verdict "$WORK/run.ndjson" "$WORK/run")
if [ "$v" = accepted ]; then echo "   ok: the run is accepted ($(head -1 "$WORK/run/convert.txt" | sed 's/.*-> //'))"
else echo "== the RUN is not accepted: $v ($WORK/run)"; FAIL=$((FAIL + 1)); fi

python3 mutate_storm.py "$WORK/run.ndjson" "$WORK/mut" > "$WORK/mutations.txt" || { echo "== mutate_storm.py failed"; exit 2; }
for m in "$WORK"/mut/*.ndjson; do
  n=$(basename "$m" .ndjson)
  v=$(verdict "$m" "$WORK/m-$n")
  if [ "$v" = accepted ] || [ "${v%%:*}" = error ]; then echo "== MUTATION NOT REJECTED: $n -- $v"; FAIL=$((FAIL + 1))
  else echo "   ok: $n -- $v"; fi
done
echo "work dir: $WORK"
[ "$FAIL" -eq 0 ] && echo "storm check: ok" || echo "storm check: $FAIL FAILED"
[ "$FAIL" -eq 0 ]
