#!/usr/bin/env bash
# TLC gate for the LEAN formal models (lean/formal/LeanSubtree.tla — the
# checkout/publish subtree protocol: barrier x HITL x lease/takeover over
# the bucket substrate; model BEFORE code, the FlintExtents posture).
#
# DELIBERATELY SEPARATE from scripts/check-tla.sh (flint's 196-run gate):
# lean is a separate system.  Same harness discipline, its own runs.
#
# Twenty-four runs, ALL required:
#   - strict runs must complete with every listed invariant green;
#   - mutation runs must FIND their designated counterexample — a model
#     that cannot rediscover its bug classes proves nothing;
#   - probe runs must be VIOLATED — each probe names an ACTION via a
#     ghost only that action writes (non-vacuity: probe the action,
#     never the situation).
#
# Regenerate the cfg matrix with ./gen-cfgs.sh.
set -u
cd "$(dirname "$0")"

JAR="${TLA_TOOLS_JAR:-../../.tla2tools.jar}"
if [ ! -f "$JAR" ]; then
  echo "fetching tla2tools.jar (v1.7.4)..."
  curl -fsSL -o "$JAR" \
    https://github.com/tlaplus/tlaplus/releases/download/v1.7.4/tla2tools.jar
fi

mkdir -p states
PASS=0

run_tlc() { # <module> <cfg>
  # Per-cfg -metadir: TLC's default scratch dir is named by wall-clock
  # second — parallel or fast-successive runs collide without this.
  java -XX:+UseParallelGC -cp "$JAR" tlc2.TLC -workers auto \
    -metadir "states/${2%.cfg}" -config "$2" "$1.tla" 2>&1
}

strict_run() { # <module> <cfg> <label>
  echo "== strict: $3 [$2]"
  local out rc=0
  out=$(run_tlc "$1" "$2") || rc=$?
  if [ "$rc" -ne 0 ]; then
    printf '%s\n' "$out" | tail -40
    echo "FAIL: $3 — strict run errored or violated"
    exit 1
  fi
  PASS=$((PASS + 1))
  echo "   ok"
}

mutation_run() { # <module> <cfg> <label> <required-violation-substring>
  echo "== must-fail: $3 [$2]"
  local out rc=0
  out=$(run_tlc "$1" "$2") || rc=$?
  if [ "$rc" -eq 0 ]; then
    printf '%s\n' "$out" | tail -20
    echo "FAIL: $3 — did NOT find its counterexample (a green mutation proves nothing)"
    exit 1
  fi
  case "$out" in
    *"$4"*) PASS=$((PASS + 1)); echo "   found: $4" ;;
    *)
      printf '%s\n' "$out" | tail -40
      echo "FAIL: $3 — failed for a reason other than: $4"
      exit 1 ;;
  esac
}

M=LeanSubtree

# ---- strict ----------------------------------------------------------------
strict_run $M LeanSubtree.cfg          "core breadth (crash+restart+HITL, all arms on)"
strict_run $M LeanSubtreeTakeover.cfg  "stall/takeover world: rotation + per-request epoch hold"
strict_run $M LeanNoWindowHolds.cfg    "window OFF: inbox + guards still carry safety (the window is availability/defense-in-depth at whole-PUT atomicity)"
strict_run $M LeanEpochOnlyHolds.cfg   "rotation OFF, epoch-check ON: per-request validation alone fences the straggler (redundancy A/B)"

# ---- mutations (the review's confirmed defects, pinned permanently) --------
mutation_run $M LeanAmputation.cfg   "HITL amputation: direct bump + whole-rewrite 412/re-seed/overwrite" \
  "Invariant Inv_HITLDurable is violated"
mutation_run $M LeanDirectMergeInsufficient.cfg "merge WITHOUT the inbox: preservation is one barrier deep (delete-after-absorption) — the inbox is load-bearing" \
  "Invariant Inv_HITLDurable is violated"
mutation_run $M LeanLocalWins.cfg    "the inherited LOCAL-WINS 412 arbitration destroys a user upload" \
  "Invariant Inv_HITLDurable is violated"
mutation_run $M LeanGCUnguarded.cfg  "unguarded GC delete destroys a HITL re-create" \
  "Invariant Inv_HITLDurable is violated"
mutation_run $M LeanDanglingOrder.cfg "v1 order (upload->delete->CAS) dangles the standing manifest" \
  "Invariant Inv_NoDangling is violated"
mutation_run $M LeanNoRotate.cfg     "no takeover rotation: the deposed straggler's manifest CAS lands" \
  "Invariant Inv_NoStragglerInstall is violated"
mutation_run $M LeanNoEpochCheck.cfg "rotation alone: the deposed straggler's data PUT lands" \
  "Invariant Inv_NoDeposedPut is violated"
mutation_run $M LeanRematerialize.cfg "re-checkout over a live tree resurrects an unpublished delete" \
  "Invariant Inv_NoResurrection is violated"

# ---- probes (non-vacuity: TLC must violate each) ---------------------------
mutation_run $M LeanProbeBarrier.cfg          "probe: a full 7-step barrier completes" \
  "Invariant ProbeBarrierDone is violated"
mutation_run $M LeanProbeHITLCited.cfg        "probe: an acked HITL write becomes manifest-cited" \
  "Invariant ProbeHITLCited is violated"
mutation_run $M LeanProbeTakeover.cfg         "probe: the takeover fires" \
  "Invariant ProbeTakeover is violated"
mutation_run $M LeanProbeStragglerAttempt.cfg "probe: a deposed CAS attempt is exercised (and fenced)" \
  "Invariant ProbeStragglerAttempt is violated"
mutation_run $M LeanProbePark.cfg             "probe: the 412-park/conflict-surface arm fires" \
  "Invariant ProbePark is violated"
mutation_run $M LeanProbeGC.cfg               "probe: the GC delete fires" \
  "Invariant ProbeGC is violated"
mutation_run $M LeanProbeRefusal.cfg          "probe: a HITL write is refused while the window is open" \
  "Invariant ProbeRefusal is violated"
mutation_run $M LeanProbeAdoptOwn.cfg         "probe: the own-crashed-PUT 412 adoption fires after restart" \
  "Invariant ProbeAdoptOwn is violated"

# ---- tranche 2: the sync verb x barrier product ----------------------------
strict_run $M LeanSyncHolds.cfg "sync verb: scan-first + locally-dirty-wins holds against the barrier/HITL product"
mutation_run $M LeanSyncStaleDirt.cfg "sync judging dirt from the LAST BARRIER's snapshot destroys un-scanned live work" \
  "Invariant Inv_SyncNeverDestroysDirty is violated"
mutation_run $M LeanProbeSyncApplied.cfg  "probe: sync actually applies a remote change" \
  "Invariant ProbeSyncApplied is violated"
mutation_run $M LeanProbeSyncConflict.cfg "probe: sync actually surfaces a dirty-path conflict" \
  "Invariant ProbeSyncConflict is violated"

# ---- tranche 3, product 4: the SCOPED sync verb x the merge base (D4) ------
strict_run $M LeanScopedSyncHolds.cfg "scoped sync: the merge base advances ONLY for paths applied or verified in scope"
mutation_run $M LeanScopedSyncWholeBase.cfg "a scoped sync advancing the WHOLE merge base loses every out-of-scope foreign entry from the inbox flow, permanently" \
  "Invariant Inv_NoForeignLost is violated"
mutation_run $M LeanProbeScopedDeferral.cfg "probe: a scoped sync actually defers an out-of-scope remote change" \
  "Invariant ProbeScopedDeferral is violated"

echo
echo "lean formal gate: $PASS/27 runs green"
