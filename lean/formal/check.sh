#!/usr/bin/env bash
# TLC gate for the LEAN formal models (lean/formal/LeanSubtree.tla — the
# checkout/publish subtree protocol: barrier x HITL x lease/takeover over
# the bucket substrate; model BEFORE code, the FlintExtents posture).
#
# DELIBERATELY SEPARATE from scripts/check-tla.sh (flint's 196-run gate):
# lean is a separate system.  Same harness discipline, its own runs.
#
# Fifty-five runs, ALL required:
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

# ---- tranche 3, product 2: gated citation x version GC x the backstop -----
strict_run $M LeanGatedHolds.cfg "gated advance: cited versions live, the reaper never takes live bytes, boundaries are all-or-nothing"
strict_run $M LeanGatedCrash.cfg "gated advance UNDER POD REPLACEMENT: the citation's own crash matrix, which belonged to no product until now"
mutation_run $M LeanProbeGatedCrash.cfg "probe: a pod replacement is REACHABLE in the gated world -- without it the crash run is green over nothing" \
  "Invariant ProbeGatedCrashReachable is violated"
mutation_run $M LeanProbeGatedRestart.cfg "probe (U41): a RESTART is reachable in the gated world — Inv_NoResurrection was listed as checked there while MaxRestarts=0 disabled its only writer" \
  "Invariant ProbeGatedRestartReachable is violated"
mutation_run $M LeanGatedReapsCurrent.cfg "the shipped reaper rule (keep only the cited version) DELETES a HITL write that landed between the lane and the citation — it was current, acked, and about to be read" \
  "Invariant Inv_NoUncitedGC is violated"
mutation_run $M LeanGatedBackstop.cfg "the noncurrent-retention BACKSTOP reaps a cited version — gated staging makes the cited generation noncurrent, so lifecycle runs a clock against live cited data (D8's inversion; the abandoned-mid-stage endgame)" \
  "Invariant Inv_CitedVersionLives is violated"
mutation_run $M LeanGatedSplitCitation.cfg "a citation split across two CASes lets a reader see half a logical change" \
  "Invariant Inv_BoundaryAtomic is violated"
mutation_run $M LeanGatedInflightHitl.cfg "citing over an inbox entry still in flight names bytes that PREDATE the user's write" \
  "Invariant Inv_HITLDurable is violated"
mutation_run $M LeanProbeCitationInstalled.cfg "probe: ONE CAS installs >= 2 paths from a pending set that survived a lane pass" \
  "Invariant ProbeCitationInstalled is violated"
mutation_run $M LeanProbeWithheldDelete.cfg "probe: a delete is actually withheld from the manifest until a citation" \
  "Invariant ProbeWithheldDelete is violated"
mutation_run $M LeanProbeGatedGC.cfg "probe (U15): the withheld delete actually LANDS at a citation — ProbeWithheldDelete counts the WITHHOLDING, so the gated run could hold over dels={} forever" \
  "Invariant ProbeGatedGC is violated"
mutation_run $M LeanProbeForcedCite.cfg "probe: a citation actually fires mid-change (the lag/backlog caps' shape)" \
  "Invariant ProbeForcedCite is violated"
mutation_run $M LeanProbeRawUncited.cfg "probe (REQUIRED-REACHABLE): a raw reader sees uncited bytes — §3 residual 11 proven present, not assumed away" \
  "Invariant ProbeRawReaderSeesUncited is violated"

# ---- tranche 3, product 1: the boundary VERB x barrier x inbox ------------
strict_run $M LeanSentinelHolds.cfg "boundary verb: consume/honor/ack/retire against the barrier, the inbox and a restart"
strict_run $M LeanSentinelRestart.cfg "boundary verb across a RESTART: the pending file outlives it, the in-memory honored flag does not"
strict_run $M LeanSentinelDeposal.cfg "boundary verb across a FENCE: the deposal arm the draft's cfgs never had"
mutation_run $M LeanSentinelOrphan.cfg "a consume that CLOBBERS the standing pending record instead of folding into it orphans the first agent's nonce forever" \
  "Invariant Inv_NoNonceOrphan is violated"
mutation_run $M LeanSentinelAckEarly.cfg "acking from persisted state: pending-and-no-matching-ack is the SAME observable state for crash-before-CAS as for crash-after-step-7, so it asserts publication of writes that never uploaded (the crash matrix the review retracted)" \
  "Invariant Inv_AckImpliesCited is violated"
mutation_run $M LeanSentinelFencedAck.cfg "success-ack-after-fence: a deposed incarnation telling a waiting agent its boundary landed" \
  "Invariant Inv_NoFencedOkAck is violated"
mutation_run $M LeanSentinelFastPathUnguarded.cfg "the skip-on-no-diff fast path WITHOUT its citation-repair and manifest-unchanged guards acks a boundary the manifest does not carry -- section 10.1's deliberate deviation from section 2.1, machine-checked instead of argued" \
  "Invariant Inv_AckBoundaryCoherent is violated"
mutation_run $M LeanSentinelStaleMergeBase.cfg "a restart between the manifest CAS and step 7 leaves our OWN install looking foreign at the next merge, and delete/modify then drops the agent's delete from the boundary it is about to be acked for (found in shipped code)" \
  "Invariant Inv_AckImpliesCited is violated"
mutation_run $M LeanProbeSentinelHonored.cfg "probe: an ack was written off a REAL barrier install" \
  "Invariant ProbeSentinelHonored is violated"
mutation_run $M LeanProbeRefusedAck.cfg "probe: the refusal fires -- deposal answers a waiting agent" \
  "Invariant ProbeRefusedAck is violated"
mutation_run $M LeanProbeAckAfterCrash.cfg "probe: an ack was written for a pending record that SURVIVED a restart" \
  "Invariant ProbeAckAfterCrash is violated"
mutation_run $M LeanProbeCoalescedAck.cfg "probe: two touches actually coalesced into one pending record (without this the orphan mutation checks a world with only one live nonce)" \
  "Invariant ProbeCoalescedAck is violated"
mutation_run $M LeanProbeFastPathHonor.cfg "probe: a pending sentinel was honored by the skip-on-no-diff pass rather than a full barrier" \
  "Invariant ProbeFastPathHonor is violated"

# ---- C6: the sentinel over the CITATION lane ------------------------------
# The matrix gap the verified review named: the two products never ran in
# one world, so an ack written off a citation-lane honor was never checked
# at all.
strict_run $M LeanSentinelGatedHolds.cfg "the boundary verb over the GATED citation lane: an ok ack never claims a path the citation dropped"
mutation_run $M LeanSentinelGatedOkOverDrop.cfg "the shipped gated honor: status ok whatever the citation dropped, with no ack field that could express the exception (found in shipped code)" \
  "Invariant Inv_AckImpliesCited is violated"
mutation_run $M LeanProbeDeclaredDrop.cfg "probe: a gated citation actually dropped a path the agent had DECLARED (without it the honesty rule holds vacuously)" \
  "Invariant ProbeDeclaredDrop is violated"
mutation_run $M LeanProbePartialAck.cfg "probe: the partial ack fires -- the agent is answered rather than left waiting" \
  "Invariant ProbePartialAck is violated"
mutation_run $M LeanSentinelGatedNoRepair.cfg "the citation lane WITHOUT the repair the fused barrier has: an ok ack names a manifest that does not cite a HITL write this workspace already integrated into its own tree (found in shipped code -- C2)" \
  "Invariant Inv_AckBoundaryCoherent is violated"
mutation_run $M LeanSentinelGatedStaleStage.cfg "the stage and the withheld-delete set both reach the citation and merge order decides: the boundary an ok ack names cites a file the agent deleted before declaring (found in a fix two hours old)" \
  "Invariant Inv_AckImpliesCited is violated"

# ---- the ack's PROVENANCE: one boundary, one clock ------------------------
strict_run $M LeanSentinelClockHolds.cfg "the ack and the manifest name the SAME clock -- the agent reads the ack, the fleet reads the stamp"
mutation_run $M LeanSentinelClockUnstamped.cfg "the barrier installs through an UNSTAMPED CAS: the bucket reports the default clock while the ack tells the agent otherwise (found by the bucket drill, twice in one session)" \
  "Invariant Inv_BoundaryNamesItsClock is violated"
mutation_run $M LeanSentinelGatedClockUnstamped.cfg "the same, over the CITATION lane -- the installer where the second half of the shipped defect actually lived (leg B11a caught it after the cadence half was fixed)" \
  "Invariant Inv_BoundaryNamesItsClock is violated"

# ---- the PAIR the plan predicted and the matrix never ran -----------------
strict_run $M LeanScopedGatedHolds.cfg "SyncScope x GatedCitation: two products that both advance instBase, run together for the first time (the pair 10.3 named)"
mutation_run $M LeanProbeScopedGated.cfg "probe: the scoped deferral is REACHABLE in the gated world -- without it the pair run is green over nothing" \
  "Invariant ProbeScopedDeferral is violated"
mutation_run $M LeanScopedGatedWholeBase.cfg "whole-instBase advance, with a citation lane also advancing it: the out-of-scope foreign entry is still lost forever" \
  "Invariant Inv_NoForeignLost is violated"

# ---- chunk GC (LeanChunkGC.tla) -------------------------------------------
# Chunks are SHARED between generations, which is what makes LeanSubtree's GC
# reasoning not carry over: there, every generation object had exactly one
# referent. Written BEFORE the reaper, and it refuted the design's own §8.1
# ordering rule on the first run.
C=LeanChunkGC
strict_run $C LeanChunkGC.cfg "chunk GC is safe iff ALL FOUR arms hold: refs read AT the delete, a grace, the grace outliving the publish, and adoption REWRITING what it adopts"
mutation_run $C LeanChunkGCStaleRefs.cfg "the reference set is carried from a snapshot taken before a CAS the delete follows -- the rule §8.1 actually named (list-before-refs) does NOT save it" \
  "Invariant Inv_LiveComplete is violated"
mutation_run $C LeanChunkGCRefsFirst.cfg "refs snapshotted before the listing: the other order, equally unsafe -- which is the point, the ordering was never the load-bearing property" \
  "Invariant Inv_LiveComplete is violated"
mutation_run $C LeanChunkGCNoGrace.cfg "no grace: a chunk written and not yet referenced is collected out from under its own publish" \
  "Invariant Inv_LiveComplete is violated"
mutation_run $C LeanChunkGCRacyGrace.cfg "a grace that does NOT outlive the publish: the chunk ages while its publisher is still writing" \
  "Invariant Inv_LiveComplete is violated"
mutation_run $C LeanChunkGCAdoptSkips.cfg "adoption SKIPS the rewrite: a crashed publish leaves an aged orphan, a later publish adopts it by content address and references it without touching it, and the sweep collects it as the orphan it still looks like" \
  "Invariant Inv_LiveComplete is violated"
mutation_run $C LeanChunkGCProbeCollect.cfg "probe: the reaper actually deletes something -- without it every run above is green over a GC that never fired" \
  "Invariant Probe_Collected is violated"
mutation_run $C LeanChunkGCProbeAdopt.cfg "probe: a publisher actually references a chunk it did not upload (adoption is REACHABLE, and only a crash makes it so)" \
  "Invariant Probe_Adopted is violated"
mutation_run $C LeanChunkGCSlowReader.cfg "the reader does NOT revalidate: its generation is swept out from under it and it reads a hole -- a reader safe for `Retain` PUBLISHES rather than for a duration, which is the wrong unit when a checkout runs minutes and the floor is seconds" \
  "Invariant Inv_NoTornRead is violated"
mutation_run $C LeanChunkGCProbeRestart.cfg "probe: a reader actually restarts onto a newer generation -- without it the strict run is green over a reader that never raced a sweep" \
  "Invariant Probe_Restarted is violated"

echo
echo "lean formal gate: $PASS/75 runs green"
