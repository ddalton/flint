#!/usr/bin/env bash
# Trace validation: every committed syncer trace must be a behaviour of
# LeanSubtree.tla (TraceLean.tla), and every trace mutation must NOT be.
#
#   trace-check.sh                 check lean/formal/trace/traces/*.ndjson
#   trace-check.sh <file.ndjson>   check one
#
# Accepted = TLC violates TraceIncomplete (it followed the trace to the end).
# Rejected = TLC exhausts without reaching the end; the last TRACE-REACHED
# line names the step it could not take.  Mutations (mutate.py: one fact of
# a real trace corrupted) and controls (a model correction turned back off)
# must be rejected: a checker that accepts a trace with a wrong etag, or a
# model without the correction a trace forced, would accept anything.
#
# Regenerate traces with:
#   FLINT_SYNC_CONFORMANCE_DIR=$PWD/lean/formal/trace/traces \
#     cargo test --manifest-path lean/syncer/Cargo.toml --lib conformance_
set -u
cd "$(dirname "$0")"
JAR="${TLA_TOOLS_JAR:-../../../.tla2tools.jar}"
JAR="$(cd "$(dirname "$JAR")" && pwd)/$(basename "$JAR")"
WORK="${TRACE_WORK:-$(mktemp -d)}"
FAIL=0

check() { # <ndjson> <expect: accept|reject> [label] [--set=K=V ...]
  local src=$1 want=$2 name out
  name=${3:-$(basename "$src" .ndjson)}
  shift 2; [ $# -gt 0 ] && shift
  out="$WORK/$name"
  rm -rf "$out"; mkdir -p "$out"
  if ! python3 ndjson2tla.py "$src" "$out" "$@" > "$out/convert.txt"; then
    cat "$out/convert.txt"
    if [ "$want" = reject ]; then echo "   ok (rejected at conversion)"; return; fi
    FAIL=$((FAIL + 1)); return
  fi
  cp ../LeanSubtree.tla TraceLean.tla "$out/"
  (cd "$out" && java -XX:+UseParallelGC -cp "$JAR" tlc2.TLC -workers 1 \
      -metadir states -config TraceLean.cfg TraceLean.tla > tlc.log 2>&1)
  local got
  if grep -q "Invariant TraceIncomplete is violated" "$out/tlc.log"; then got=accept
  elif grep -q "Model checking completed. No error has been found" "$out/tlc.log"; then got=reject
  else
    echo "== $name: TLC ERROR"; grep -m5 -E "^Error|Exception|error" "$out/tlc.log"; FAIL=$((FAIL + 1)); return
  fi
  local steps reached
  steps=$(grep -c . "$out/MAP.txt" 2>/dev/null)
  # TLC wraps long values across lines: join each print before reading it.
  reached=$(python3 -c 'import re,sys; t=open(sys.argv[1]).read(); m=re.findall(r"<<\s*\"TRACE-REACHED\",.*?>>\n", t, re.S); print(re.sub(r"\s+"," ",m[-1]).strip() if m else "")' "$out/tlc.log")
  if [ "$got" = "$want" ]; then
    echo "== $name: $got (as required)${reached:+ — last: ${reached:0:160}}"
  else
    echo "== $name: $got, but $want was required"
    echo "   $reached"
    FAIL=$((FAIL + 1))
  fi
}

if [ $# -gt 0 ]; then
  for f in "$@"; do check "$f" accept; done
else
  for f in traces/*.ndjson; do check "$f" accept; done
  # MUTATIONS: one fact of a real trace corrupted (mutate.py) — must reject.
  python3 mutate.py traces "$WORK/mutations" > "$WORK/mutate.txt"
  for f in "$WORK"/mutations/*.ndjson; do check "$f" reject; done
  # CONTROLS: each model correction trace validation forced, turned back off —
  # the trace that forced it must be rejected again, or the correction is not
  # what made it pass.
  check traces/edit_and_delete_cross.ndjson reject control-commit-token --set=CommitLoadsCurrent=FALSE
  check traces/both_edit_one_file.ndjson reject control-412-parks --set=Upload412Preserves=FALSE
  check traces/outranked_delete_publish.ndjson reject control-two-scan-declared --set=DeclaredConfirmsAbsence=FALSE
  check traces/ui_write_over_a_queued_delete.ndjson reject control-tombstone-unfixed --set=TombstoneHeadsKey=FALSE
  check traces/edit_and_delete_cross.ndjson reject control-shared-inbox --set=WriterQueue=FALSE --set=EmptyInstall=FALSE
fi
echo "work dir: $WORK"
[ "$FAIL" -eq 0 ] && echo "trace check: ok" || { echo "trace check: $FAIL FAILED"; exit 1; }
