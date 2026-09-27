#!/usr/bin/env bash
# Trace validation against LeanP1.tla (TraceCore.tla): every trace in
# traces-core/ must be a behaviour of the model, with the model's safety
# claims holding along it, and every mutation and control must NOT be.
# (Earlier: LeanCore.tla, then LeanP2.tla — ../results/2026-09-25-tracecore-on-*/.)
# trace-check.sh is the same check against LeanSubtree.tla, on traces/.
#
#   trace-check-core.sh                 the traces, the mutations, the controls
#   trace-check-core.sh <file.ndjson>   check one
#
# traces-core/ is the CURRENT code's output (P2 + P1-lite). Regenerate with:
#   FLINT_SYNC_CONFORMANCE_DIR=$PWD/lean/formal/trace/traces-core \
#     cargo test --manifest-path lean/syncer/Cargo.toml --lib conformance_
set -u
cd "$(dirname "$0")"
JAR="${TLA_TOOLS_JAR:-../../../.tla2tools.jar}"
JAR="$(cd "$(dirname "$JAR")" && pwd)/$(basename "$JAR")"
WORK="${TRACE_WORK:-$(mktemp -d)}"
FAIL=0

check() { # <ndjson> <expect: accept|reject|unsafe> [label] [--set=K=V ...]
  local src=$1 want=$2 name out
  name=${3:-$(basename "$src" .ndjson)}
  shift 2; [ $# -gt 0 ] && shift
  out="$WORK/$name"
  rm -rf "$out"; mkdir -p "$out"
  if ! python3 ndjson2core.py "$src" "$out" "$@" > "$out/convert.txt"; then
    cat "$out/convert.txt"
    if [ "$want" = reject ]; then echo "   ok (rejected at conversion)"; return; fi
    FAIL=$((FAIL + 1)); return
  fi
  cp ../LeanP1.tla TraceCore.tla "$out/"
  (cd "$out" && java -XX:+UseParallelGC -cp "$JAR" tlc2.TLC -workers 1 \
      -metadir states -config TraceCore.cfg TraceCoreData.tla > tlc.log 2>&1)
  local got
  if grep -q "Invariant TraceIncomplete is violated" "$out/tlc.log"; then got=accept
  elif grep -q "Model checking completed. No error has been found" "$out/tlc.log"; then got=reject
  elif grep -qE "Invariant (TypeOK|Inv_[A-Za-z]+) is violated|Action property Prop_[A-Za-z]+ is violated" "$out/tlc.log"; then
    # The run the code performed entered a state the core calls unsafe.
    got="UNSAFE: $(grep -m1 -oE "(Invariant|Action property) [A-Za-z_]+ is violated" "$out/tlc.log")"
  else
    echo "== $name: TLC ERROR"; grep -m5 -E "^Error|Exception|error" "$out/tlc.log"; FAIL=$((FAIL + 1)); return
  fi
  local reached
  # TLC wraps long values across lines: join each print before reading it.
  reached=$(python3 -c 'import re,sys; t=open(sys.argv[1]).read(); m=re.findall(r"<<\s*\"TRACE-REACHED\",.*?>>\n", t, re.S); print(re.sub(r"\s+"," ",m[-1]).strip() if m else "")' "$out/tlc.log")
  if [ "$got" = "$want" ] || { [ "$want" = unsafe ] && [ "${got#UNSAFE}" != "$got" ]; }; then
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
  for f in traces-core/*.ndjson; do check "$f" accept; done
  # MUTATIONS: one fact of a real trace corrupted — must reject.
  # A mutation whose anchor is missing stops the generator; every one after
  # it would go unchecked, so that is a failure, not a shorter list.
  if ! python3 mutate_core.py traces-core "$WORK/mutations" > "$WORK/mutate.txt" 2>&1; then
    echo "== MUTATIONS: the generator failed"; tail -1 "$WORK/mutate.txt"; FAIL=$((FAIL + 1))
  fi
  for f in "$WORK"/mutations/*.ndjson; do check "$f" reject; done
  # CONTROLS: a rule turned off must change the verdict on a trace that
  # exercises it.  Not every rule can be exercised by a trace: the ones
  # that only RESTRICT the model (SweepUnderLease, GatewaySweepGrace) or
  # guard a step no scenario takes (GatewayJudgesRead: no save is refused;
  # GatewayIgnoresLease: no save lands while a writer holds the lease;
  # CollectorSparesCited: no install retires a handle it still cites;
  # ContentConverges: no scenario restarts between a CAS and step 7) are
  # the model checker's to show (gen-leanp1.sh).
  check traces-core/both_edit_one_file.ndjson reject control-no-r7 --set=CommitSurfacesForeign=FALSE
  check traces-core/commit_withholds_a_swept_upload.ndjson reject control-commit-blind --set=CommitVerifiesUploads=FALSE
  # The delete-override record: with it off, the model records nothing where the code does.
  check traces-core/agent_edit_over_a_ui_delete.ndjson reject control-delete-override-unrecorded --set=CommitRecordsDeleteOverride=FALSE
  # M3: with the delete outranked, the model cannot follow the code's delete over B's edit.
  check traces-core/delete_over_a_peers_edit.ndjson reject control-delete-outranked --set=DeleteWinsPreserved=FALSE
  # The cheap path without the merge's mark: the model's shortcut skips what
  # is owed, and the check stops UNSAFE (Inv_ShortcutSound).
  check traces-core/ui_save_lands_inside_a_barrier.ndjson unsafe control-advance-unguarded --set=CommitAdvanceGuarded=FALSE
  # M1: without the retire age the model collects at the commit, and cannot
  # follow the code's later reap of what the saves retired.
  check traces-core/superseded_ui_write_swept.ndjson reject control-retire-age-off --set=RetireAge=FALSE
  # A rename in two CASes cannot follow the code's one: its first CAS cites
  # one handle at both names, and the check stops UNSAFE right there.
  check traces-core/ui_rename.ndjson unsafe control-rename-two-cas --set=RenameAtomic=FALSE
fi
echo "work dir: $WORK"
[ "$FAIL" -eq 0 ] && echo "trace check (core): ok" || { echo "trace check (core): $FAIL FAILED"; exit 1; }
