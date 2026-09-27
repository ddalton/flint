#!/usr/bin/env bash
# Trace validation against LeanCore.tla (TraceCore.tla): every trace in
# traces-core/ must be a behaviour of the core, with the core's safety
# claims holding along it, and every mutation and control must NOT be.
# trace-check.sh is the same check against LeanSubtree.tla, on traces/.
#
#   trace-check-core.sh                 the traces, the mutations, the controls
#   trace-check-core.sh <file.ndjson>   check one
#
# traces-core/ is the CURRENT code's output (handles). Regenerate with:
#   FLINT_SYNC_CONFORMANCE_DIR=$PWD/lean/formal/trace/traces-core \
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
  if ! python3 ndjson2core.py "$src" "$out" "$@" > "$out/convert.txt"; then
    cat "$out/convert.txt"
    if [ "$want" = reject ]; then echo "   ok (rejected at conversion)"; return; fi
    FAIL=$((FAIL + 1)); return
  fi
  cp ../LeanCore.tla TraceCore.tla "$out/"
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
  for f in traces-core/*.ndjson; do check "$f" accept; done
  # MUTATIONS: one fact of a real trace corrupted — must reject.
  python3 mutate_core.py traces-core "$WORK/mutations" > "$WORK/mutate.txt"
  for f in "$WORK"/mutations/*.ndjson; do check "$f" reject; done
  # CONTROLS: a design rule turned off must change the verdict on a trace
  # that exercises it. Swept 2026-09-24 over all 11 rules x 10 traces:
  # nine are exercised (the last four by the scenarios added for them the
  # same day). SweepUnderLease and SweepSparesNamed only RESTRICT the
  # model, so turning one off cannot reject a real trace (the model
  # checker shows they are needed, not this).
  check traces-core/both_edit_one_file.ndjson reject control-no-r7 --set=CommitSurfacesForeign=FALSE
  check traces-core/ui_rename.ndjson reject control-collector-takes-cited --set=CollectorSparesCited=FALSE
  check traces-core/pending_adoption_behind_a_superseded_removal.ndjson reject control-repair-ignores-moves --set=RepairRespectsMoves=FALSE
  check traces-core/ui_delete_refused_dirty.ndjson reject control-answered-records-inert --set=AnsweredRecordsApply=FALSE
  check traces-core/pending_adoption_behind_a_superseded_removal.ndjson reject control-pending-not-kept --set=PendingAdoptionStays=FALSE
  check traces-core/repair_yields_to_a_later_ui_write.ndjson reject control-repair-beats-later-ui --set=RepairYieldsToLaterUI=FALSE
  check traces-core/commit_withholds_a_swept_upload.ndjson reject control-commit-blind --set=CommitVerifiesUploads=FALSE
  check traces-core/r7_is_per_path_across_a_rename.ndjson reject control-foreign-flat --set=ForeignPerPath=FALSE
  check traces-core/rename_of_a_pending_ui_write_onto_a_dirty_destination.ndjson reject control-rename-leaves-entry --set=RenameMovesEntry=FALSE
  # L-126: with the old step 7, the core's re-upload records nothing where the code's R7 does.
  check traces-core/withheld_republished_over_a_ui_save.ndjson reject control-l126-parked-base-moves --set=ParkedKeepsMergeBase=FALSE
  # The delete-override record: with it off, the core records nothing where the code does.
  check traces-core/ui_write_over_a_queued_delete.ndjson reject control-delete-override-unrecorded --set=CommitRecordsDeleteOverride=FALSE
  # L-125: the core's old step 7 cannot follow the fixed code's refused delete.
  check traces-core/ui_delete_refused_dirty.ndjson reject control-l125-baseline-kept --set=OutrankedRemovalLeavesBaseline=FALSE
fi
echo "work dir: $WORK"
[ "$FAIL" -eq 0 ] && echo "trace check (core): ok" || { echo "trace check (core): $FAIL FAILED"; exit 1; }
