#!/usr/bin/env bash
# TLC gate for the LEAN formal models (lean/formal/LeanSubtree.tla — the
# checkout/publish subtree protocol: barrier x HITL x lease/takeover over
# the bucket substrate; model BEFORE code, the FlintExtents posture).
#
# DELIBERATELY SEPARATE from scripts/check-tla.sh (flint's 196-run gate):
# lean is a separate system.  Same harness discipline, its own runs.
#
# A hundred and eighty-two runs, ALL required (asserted at the bottom, not just printed —
# this prose count had drifted to "fifty-five", then to "eighty-three";
# and EXPECT itself was left at 92 over 69 real runs when gated mode's
# 23 runs were removed, so the gate at that commit failed its own count.
# The recipe in the README is the census; EXPECT is what is checked):
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

# ---- the journal -----------------------------------------------------------
# This gate is a hundred and eighty-two TLC runs and takes the better part
# of an hour. On a memory-constrained laptop the OS has killed it three
# times out of four — at runs 109, 62 and 89 of the sequence, with
# nothing else of ours running — and each kill cost the whole hour and
# proved nothing. So every green run is journalled, and GATE_RESUME=1
# lets a later attempt skip what has already been proved.
#
# A skip is sound ONLY if nothing that decides the run has changed, so:
#   * the journal is keyed on a fingerprint of this script and EVERY .tla
#     module — any edit to any of them throws the whole journal away;
#   * each entry additionally carries the hash of its OWN cfg, so an
#     edited cfg re-runs even when the modules did not move.
# Both are required: the modules and the cfg together decide the verdict.
#
# And a resumed gate SAYS SO on its last lines. A replayed run is
# evidence from an earlier process, not from this one, and a log that
# reads "117/117 green" without that distinction would be a gate telling
# a small lie about how much work it just did.
JOURNAL=states/.gate-journal
sha() { shasum -a 256 "$@" | shasum -a 256 | cut -d' ' -f1; }
FP=$(sha check.sh *.tla)
REPLAYED=0
RANNOW=0
if [ "${GATE_RESUME:-}" = "1" ] && [ -f "$JOURNAL" ] &&
   [ "$(head -1 "$JOURNAL")" = "#fingerprint $FP" ]; then
  echo "gate: RESUMING — $(($(wc -l < "$JOURNAL") - 1)) run(s) already proved at this fingerprint"
  echo "gate: (a replayed run is evidence from an earlier process; unset GATE_RESUME for a fresh one)"
else
  # No resume, or the fingerprint moved: start the journal over. Writing
  # it is unconditional — a run that is killed has to leave something a
  # later attempt can stand on, and that is the whole point.
  printf '#fingerprint %s\n' "$FP" > "$JOURNAL"
fi

# Has this cfg already been proved at this fingerprint, with its own
# bytes unchanged?
journalled() { # <cfg>
  [ "${GATE_RESUME:-}" = "1" ] || return 1
  grep -qxF "$(sha "$1") $1" "$JOURNAL"
}
record() { printf '%s %s\n' "$(sha "$1")" "$1" >> "$JOURNAL"; }

# Most cfgs fingerprint through `StrictView` (the ghost-state reduction,
# LeanSubtree.tla). A view that drops a field something still reads is
# silently unsound, so before any run: the census must pass, and its own
# positive controls — edits that make the view unsound — must each fail it.
python3 view-census.py --selftest LeanSubtree.tla || { echo "FAIL: the view census has lost its teeth"; exit 1; }
python3 view-census.py LeanSubtree.tla || { echo "FAIL: StrictView is not sound for this module"; exit 1; }
# And every constant the module declares must be known to BOTH cfg
# generators — this one and `trace/ndjson2tla.py`. A constant added to
# only one leaves the other's runs refused by TLC, in a gate you did not
# happen to run: that broke trace-check.sh (which is in CI) twice on
# 2026-09-15 and was found days later by a replay, not by a gate.
python3 constants-census.py --selftest || { echo "FAIL: the constants census has lost its teeth"; exit 1; }
python3 constants-census.py || { echo "FAIL: a cfg generator does not know every constant"; exit 1; }

# TLC_HEAP and TLC_WORKERS exist because this gate has to be runnable on a
# memory-constrained laptop: TLC sizes its heap from system RAM and takes
# every core, and on an 8 GiB box that is what the OS kills — twice on
# 2026-09-15, at runs 109 and 62, with nothing else of ours running. Unset
# = the old behaviour exactly, so a big box is unaffected.
run_tlc() { # <module> <cfg>
  # Per-cfg -metadir: TLC's default scratch dir is named by wall-clock
  # second — parallel or fast-successive runs collide without this.
  java -XX:+UseParallelGC ${TLC_HEAP:+-Xmx$TLC_HEAP} -cp "$JAR" tlc2.TLC \
    -workers "${TLC_WORKERS:-auto}" \
    -metadir "states/${2%.cfg}" -config "$2" "$1.tla" 2>&1
}

strict_run() { # <module> <cfg> <label>
  echo "== strict: $3 [$2]"
  if journalled "$2"; then
    PASS=$((PASS + 1)); REPLAYED=$((REPLAYED + 1))
    echo "   ok (REPLAYED from the journal — not run in this process)"
    return
  fi
  local out rc=0
  out=$(run_tlc "$1" "$2") || rc=$?
  if [ "$rc" -ne 0 ]; then
    # The invariant's name is at the TOP of a violation; the tail is the
    # trace. `tail -40` alone lost the name with the 2026-09-13 box.
    printf '%s\n' "$out" | grep -m2 '^Error:'
    printf '%s\n' "$out" | tail -40
    echo "FAIL: $3 — strict run errored or violated"
    exit 1
  fi
  PASS=$((PASS + 1)); RANNOW=$((RANNOW + 1))
  record "$2"
  echo "   ok"
}

mutation_run() { # <module> <cfg> <label> <required-violation-substring>
  echo "== must-fail: $3 [$2]"
  if journalled "$2"; then
    PASS=$((PASS + 1)); REPLAYED=$((REPLAYED + 1))
    echo "   found: $4 (REPLAYED from the journal — not run in this process)"
    return
  fi
  local out rc=0
  out=$(run_tlc "$1" "$2") || rc=$?
  if [ "$rc" -eq 0 ]; then
    printf '%s\n' "$out" | tail -20
    echo "FAIL: $3 — did NOT find its counterexample (a green mutation proves nothing)"
    exit 1
  fi
  case "$out" in
    *"$4"*) PASS=$((PASS + 1)); RANNOW=$((RANNOW + 1)); record "$2"; echo "   found: $4" ;;
    *)
      printf '%s\n' "$out" | grep -m2 '^Error:'
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
mutation_run $M LeanRematerialize.cfg "re-checkout over a live tree resurrects an unpublished delete (the no-resurrection claim is an ACTION property since 2026-09-19: a restart creates no local file)" \
  "Action property"

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

# ---- tranche 4: the NARROW verb (scoped-read design §4) --------------------
strict_run $M LeanNarrowHolds.cfg "narrow: a narrow is an UNWATCH — the dropped path keeps its object and is never re-cited"
mutation_run $M LeanNarrowUnlinkFirst.cfg "unlink-then-uncite: the surviving citation makes the path delete-eligible and the GC publishes it" \
  "Invariant Inv_NarrowNeverDeletes is violated"
mutation_run $M LeanNarrowUncieFirst.cfg "uncite-then-unlink: the surviving file reads as a local ADD and the barrier re-cites everything just dropped" \
  "Invariant Inv_NarrowNeverRecites is violated"
mutation_run $M LeanProbeNarrow.cfg "probe: the narrow verb actually fires" \
  "Invariant ProbeNarrow is violated"

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

# ---- the ack's PROVENANCE: one boundary, one clock ------------------------
strict_run $M LeanSentinelClockHolds.cfg "the ack and the manifest name the SAME clock -- the agent reads the ack, the fleet reads the stamp"
mutation_run $M LeanSentinelClockUnstamped.cfg "the barrier installs through an UNSTAMPED CAS: the bucket reports the default clock while the ack tells the agent otherwise (found by the bucket drill, twice in one session)" \
  "Invariant Inv_BoundaryNamesItsClock is violated"
# ---- the core model and the refinement (LeanCore.tla, LeanRefine.tla) -----
# The shipped shape once, with no arms, and state-based invariants; then the
# proof that the history model's shipped worlds refine it.  The queue world
# first (606,916 states, depth 40, a minute), then the probe world
# (1,937,311 states, depth 36, half an hour on two workers); the rename
# world follows once its size is known (the box runs it first).
# The core's own worlds first: the shipped shape with one rule removed at a
# time.  Two of them are NOT here -- `…AnsweredSkipped`,
# `…RenameLeavesEntry` and `…RepairOverridesUI` need a third name and tens of millions of states, so
# `run-io.sh` decides them on the box; the same two rules are refuted here on
# the history model (`LeanImmutableAnsweredRecordSkipped`, and
# `LeanImmutableRenameLeavesEntry` once the box has sized it).  Each names the invariant the rule holds up, and the probes say the
# world REACHED the step the rule is about (a mutation that never reaches
# its rule is green for the wrong reason).
# The two-path shape: 48,995,156 states, depth 34, twenty-odd minutes on
# of the box's workers.  The three-path world (LeanCoreHolds.cfg) is the
# same claims with room for a third name and runs on the box -- 120 million
# states and still climbing at depth 19, which is not a gate a laptop runs.
strict_run LeanCore LeanCoreHoldsSmall.cfg "the core model: the shipped shape, no arms -- every citation names a live handle, one handle is never cited under two names, an acked handle that is gone is accounted for by the state, and the commit section is one writer's -- and (2026-09-23) a published version is never silently reverted: replaced only by an edit derived from it, a recorded override, or a move"
mutation_run LeanCore LeanCoreRepairRecites.cfg "core rule R2 refuted: a repair re-cites a handle the document cites at another path this install keeps" \
  "Invariant Inv_OneName is violated"
mutation_run LeanCore LeanCoreCommitBlind.cfg "core rule R7 refuted: the commit publishes over a version the tree never integrated with no record naming it" \
  "Invariant Inv_AckedNamed is violated"
mutation_run LeanCore LeanCoreCommitBlindReverts.cfg "core rule R7 refuted, read by the no-lost-update claim: the commit replaces a published version it never took in THERE with one not derived from it, and no record names it (2026-09-23, after L-123)" \
  "Action property Prop_NoSilentRevert is violated"
mutation_run LeanCore LeanCoreSweepTakesNamed.cfg "core rule R4c refuted: the sweep takes a handle an unconsumed entry names" \
  "Invariant Inv_AckedNamed is violated"
mutation_run LeanCore LeanCoreForeignFlat.cfg "core: theirs-or-mine judged by ONE FLAT SET per writer instead of per path -- a peer that had the seed at a rename's source publishes its own file over the renamed destination, surfaces nothing because the version it published over was familiar (just not at that path), and the handle is retired with no record. This is the approximation LeanSubtree still makes, so the refinement worlds map onto it: the debt, priced" \
  "Invariant Inv_AckedNamed is violated"
mutation_run LeanCore LeanCorePendingDropped.cfg "core: an adoption declined for the MOVE ALONE, at the destination of a rename still waiting at its source, refuted as gone -- the entry leaves the cell, the peer's record uncites the source, and the collector takes the handle (L-119, the rename world's tenth box run)" \
  "Invariant Inv_AckedNamed is violated"
mutation_run LeanCore LeanCoreCitesBlind.cfg "core rule R4a refuted: the commit cites its own uploads without re-reading them and a sweep has taken one" \
  "Invariant Inv_CitationsLive is violated"
mutation_run LeanCore LeanCoreSweepLeaseFree.cfg "core rule R4b refuted: a lease-free sweep lands between a commit's re-read and its CAS" \
  "Invariant Inv_CitationsLive is violated"
mutation_run LeanCore LeanCoreRetirePerPath.cfg "core rule R3 refuted: retirement forgets that a rename's destination cites the source's handle, and the collector takes it" \
  "Invariant Inv_CitationsLive is violated"
mutation_run LeanCore LeanCoreProbeRenamed.cfg "probe: the core's worlds actually perform a rename (the document cites a handle minted at another path)" \
  "Invariant ProbeRenamed is violated"
mutation_run LeanCore LeanCoreProbeGone.cfg "probe: a handle is actually collected or swept (minted is not a subset of live)" \
  "Invariant ProbeGone is violated"
mutation_run LeanCore LeanCoreProbeRefused.cfg "probe: a removal is actually refused, and the record is durable" \
  "Invariant ProbeRefused is violated"
mutation_run LeanCore LeanCoreProbePending.cfg "probe: the SHAPE L-119 is about is reachable here -- an entry in the cell whose handle the document cites at the source of a rename still waiting there (it fires at depth 3, on the rename itself; the evidence that the RULE fires is the history model's ProbePendingKept, a counter only the declining CAS writes)" \
  "Invariant ProbePending is violated"
mutation_run LeanCore LeanCoreProbeRemovalOutranked.cfg "probe: a declared removal is actually outranked at the CAS, the arm L-125's step 7 takes (or the holds world proves nothing about it)" \
  "Invariant ProbeRemovalOutranked is violated"
strict_run LeanCore LeanCoreWithheldHolds.cfg "L-126's fix: at one path and three barriers, a withheld upload re-published next barrier records the version it never integrated"
mutation_run LeanCore LeanCoreWithheldReverts.cfg "L-126 (the shape before the fix): step 7 moves a parked path's merge base to theirs, and the re-upload replaces an acked UI write with no record" \
  "Action property Prop_NoSilentRevert is violated"
strict_run LeanRefine LeanRefineQueue.cfg "refinement: every step of LeanSubtree's queue world (IMPL + handles, one path, three barriers, a UI write) is a LeanCore step or a stutter, and the core's three state-based invariants hold through the mapping"
strict_run LeanRefine LeanRefineProbe.cfg "refinement: every step of LeanSubtree's probe world (IMPL + handles, two paths, two barriers, no UI write, the commit re-reading every upload it cites, the sweep under the lease) is a LeanCore step or a stutter, and the core's three state-based invariants hold through the mapping"

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
mutation_run $C LeanChunkGCSlowReader.cfg "the reader does NOT revalidate: its generation is swept out from under it and it reads a hole -- a reader safe for 'Retain' PUBLISHES rather than for a duration, which is the wrong unit when a checkout runs minutes and the floor is seconds" \
  "Invariant Inv_NoTornRead is violated"
mutation_run $C LeanChunkGCProbeRestart.cfg "probe: a reader actually restarts onto a newer generation -- without it the strict run is green over a reader that never raced a sweep" \
  "Invariant Probe_Restarted is violated"

# ---- the chunked MERGE (LeanChunkMerge.tla) -------------------------------
# The entry-level merge is LeanSubtree's and is not re-derived. This checks the
# level chunking adds: a writer that 412s must merge, and reusing the other
# writer's chunk list to make that O(changed) too is the tempting shortcut.
M2=LeanChunkMerge
strict_run $M2 LeanChunkMerge.cfg "merge at ENTRY level and re-chunk: the published key set is exactly the whole-document merge's, over every base/add/delete combination"
mutation_run $M2 LeanChunkMergeSplice.cfg "splice the chunk LISTS instead -- keep theirs where my change did not touch, substitute mine where it did: base {}, A adds {1}, B adds {1,2}; A's chunk {1} and B's chunk {1,2} cover the same range under different boundaries, so substituting drops B's key 2 entirely" \
  "Invariant Inv_ChunkedMergeMatches is violated"
mutation_run $M2 LeanChunkMergeProbeDiverge.cfg "probe: the two writers' chunk BOUNDARIES actually diverge -- without it the strict run is green over inputs where splicing could not have gone wrong" \
  "Invariant Probe_BoundariesDiverged is violated"
mutation_run $M2 LeanChunkMergeProbeBoth.cfg "probe: both writers actually wrote (a merge with one idle side proves nothing)" \
  "Invariant Probe_BothWrote is violated"

echo
# ---- tranche 5: DECLARED removals (delete/rename design, phase E) ---------
strict_run $M LeanRemovalHolds.cfg "declared removals + rename: one generation, no hole, every acked write tracked"
strict_run $M LeanRemovalCrashHolds.cfg "declared removals + rename under crash + restart: the late inbox drop and the intent journal"
mutation_run $M LeanRemovalOverreaches.cfg "H4 (review 2026-09-18): a declared removal as it shipped removes any clean version -- a UI write made after the delete was asked for is consumed and then removed with it" \
  "Invariant Inv_RemovalNamesItsVersion is violated"
mutation_run $M LeanRemovalViaWalk.cfg "mutation: a declared removal routed through the walk lands a rename in two generations" \
  "Invariant Inv_RenameAtomic is violated"
mutation_run $M LeanEarlyInboxDropLosesHitl.cfg "mutation: the early inbox drop loses a consumed write to a pod replacement" \
  "Invariant Inv_HITLTracked is violated"
mutation_run $M LeanEarlyInboxDropLosesRename.cfg "mutation: the early inbox drop loses a rename's destination to a pod replacement" \
  "Invariant Inv_HITLTracked is violated"
mutation_run $M LeanRenameNoDestinationGuard.cfg "mutation: a rename that does not wait for its destination leaves a HOLE under that crash" \
  "Invariant Inv_RenameNoHole is violated"
mutation_run $M LeanProbeRemoval.cfg "probe: a declared removal is actually performed" \
  "Invariant ProbeRemovalApplied is violated"
mutation_run $M LeanProbeRemovalRefused.cfg "probe: a declared removal of a dirty path is actually refused" \
  "Invariant ProbeRemovalRefused is violated"
mutation_run $M LeanProbeRename.cfg "probe: a rename's removal is actually performed" \
  "Invariant ProbeRenameApplied is violated"

echo
# ---- tranche 6: the PER-BARRIER lease (writer-lease design §4-§5) --------
# The cell is held for one barrier's commit section, with a FIFO ticket;
# both writers run from the start and the takeover is the generic deposal.
# Every pre-existing run above keeps BarrierLease=FALSE (state spaces
# preserved by construction, verified by distinct-state count).
strict_run $M LeanBarrierLeaseHolds.cfg "barrier lease, two live writers: HITL + crash + restart, every invariant + commit exclusion"
# LeanBarrierLeaseSentinel (the sentinel when the honoring barrier had to WAIT
# for the cell) left the gate on 2026-09-15: it is box-scale (62M states to
# its depth-19 stop) and was the gate's standing red. Its successor is
# LeanBarrierLeaseSentinelImpl — the same world on the code's shape (tranche
# 7), with Inv_AckBoundaryCoherent refined — which runs on the TLC box, like
# LeanBarrierLeaseSameBytesDeep. README, tranche 7.
# A THIRD WRITER in the gate (COVERAGE.md, 2026-09-15: every invariant read
# 0 in the "3 writers" column because this world was opt-in).  One path, a
# UI write, the queue and the ticket with a third party in every exchange.
strict_run $M LeanBarrierLeaseImplThreeWriters.cfg "barrier lease on the code's shape, THREE writers: the queue, the ticket and the pull-only boundary with a third party in every exchange"
strict_run $M LeanBarrierLeaseDeposal.cfg "barrier lease, takeover world: A freezes INSIDE its commit section, B deposes it with rotation, the thawed A abandons"
strict_run $M LeanBarrierLeaseEpochOnly.cfg "barrier lease, rotation OFF: per-request epoch validation alone fences the deposed holder"
strict_run $M LeanBarrierLeaseRotationOnly.cfg "barrier lease, epoch-check OFF: rotation alone fences the deposed holder's CAS"
strict_run $M LeanBarrierLeaseAdoptVerified.cfg "barrier lease: an ADOPTED entry re-verified under the lease survives the other writer's GC (the control for the adopt race)"
strict_run $M LeanBarrierLeaseHitlOverAnyVerified.cfg "barrier lease: a UI write may overwrite ANY current object and is still never lost -- the uploader's commit re-reads its citation (so the gateway's untracked-object escapes carry no safety -- which is why the writer heartbeat could go)"
strict_run $M LeanBarrierLeaseSameBytesVerified.cfg "barrier lease, identical bytes share an etag: every citation the commit adds is re-verified under the lease (the control for finding 13)"
strict_run $M LeanBarrierLeaseSyncOverlayHolds.cfg "barrier lease: a sync keeps its merge base for a path an older inbox entry hid (SyncKeepsHiddenBase, sync.rs step 5) -- the control for the overlay finding"
strict_run $M LeanBarrierLeaseLive.cfg "LIVENESS (FairSpec): with the ticket, every queued writer eventually holds the cell"
strict_run $M LeanBarrierLeaseLiveCrash.cfg "LIVENESS with a crash: a dead holder is deposed, a dead handoff is skipped, the survivor never starves"
# The epoch discipline itself, refuted (COVERAGE.md found both of these
# invariants checked in nine strict worlds with NO mutation behind them).
mutation_run $M LeanBarrierLeaseEpochReused.cfg "barrier lease, a claim REUSES the cell's epoch: two writers hold the commit section at one epoch and neither reads as deposed" \
  "Invariant Inv_CommitExclusive is violated"
mutation_run $M LeanBarrierLeaseClaimUnstamped.cfg "barrier lease, a claim does not STAMP its epoch on the claimant: the cell names a holder whose own epoch is older, and every later fence compares against the wrong one" \
  "Invariant Inv_CellHeldByHolder is violated"
mutation_run $M LeanBarrierLeaseNoRotate.cfg "barrier lease, both fences off: the thawed straggler's manifest CAS lands" \
  "Invariant Inv_NoStragglerInstall is violated"
mutation_run $M LeanBarrierLeaseGCUnconditional.cfg "FINDING: the shipped HEAD-then-unconditional-DELETE GC -- under the barrier lease the other writer's supersede lands between the two and its citation dangles (the life lease covered that window; a conditional delete is the fix)" \
  "Invariant Inv_NoDangling is violated"
mutation_run $M LeanBarrierLeaseAdoptBlind.cfg "FINDING: an adopted entry cited BLIND -- a restart between CAS and baseline makes the next barrier adopt its own upload, the other writer uncites the path and its GC deletes the object before the adopter's CAS (verify adopted entries under the lease is the fix)" \
  "Invariant Inv_NoDangling is violated"
mutation_run $M LeanBarrierLeaseSameBytesUnverified.cfg "FINDING 13 (live drill runcv A3): an upload of the SAME bytes carries the etag the other writer's GC recognises -- the GC deletes the landed upload and its commit cites nothing (the adopt verification alone does not cover it; re-verify every citation is the fix)" \
  "Invariant Inv_NoDangling is violated"
mutation_run $M LeanBarrierLeaseSameBytesOverride.cfg "FINDING 13, second route (model-found): B's identical-bytes upload is overwritten by A's new bytes If-Match the same etag, A commits, and B's commit cites its generation over A's -- nothing dangles, A's committed bytes are cited by nothing" \
  "Invariant Inv_NoStaleOverride is violated"
mutation_run $M LeanBarrierLeaseHitlOverAnyUnverified.cfg "the control for the strict run above: the same world without the commit's re-read loses the UI write (finding 4)" \
  "Invariant Inv_HITLDurable is violated"
mutation_run $M LeanBarrierLeaseSentinelOutrankedOk.cfg "FINDING (2026-09-14 box run, depth 20): the merge keeps another writer's entry over the agent's delete, and the ack says ok for a seq that still cites the path" \
  "Invariant Inv_AckImpliesCited is violated"
mutation_run $M LeanProbeOutrankedPartial.cfg "probe: the partial ack for an outranked delete is actually written (the strict run is not green over an ack never reached)" \
  "Invariant ProbePartialAck is violated"
mutation_run $M LeanBarrierLeaseHitlOverUncited.cfg "FINDING: the gateway overwrote whatever object was current -- a UI write over another writer's UNCITED upload, consumed and cited by a third party, is re-cited over by the uploader's commit and lost (the tracked-only overwrite rule is the fix)" \
  "Invariant Inv_HITLDurable is violated"
mutation_run $M LeanBarrierLeaseSyncOverlayStale.cfg "FINDING (D4 refuted with two writers): the sync's remote truth reads a queued foreign entry as newer than a manifest a second LIVE writer already moved past, verifies the path unchanged against the overlay and advances the merge base to the manifest -- the silent, permanent loss" \
  "Invariant Inv_NoForeignLost is violated"
mutation_run $M LeanBarrierLeaseRandomArbitration.cfg "L5, the ticket is load-bearing: release names nobody and one writer claims forever (a fair lasso under WF)" \
  "Temporal properties were violated"
mutation_run $M LeanBarrierLeaseDeadHandoffWedge.cfg "no dead-handoff skip: a crashed waiter named by the release wedges the cell for the survivor forever" \
  "Temporal properties were violated"
mutation_run $M LeanProbeWritersInterleave.cfg "probe (REQUIRED-REACHABLE): an upload lands while the other writer is in its commit section -- two writers really overlap" \
  "Invariant ProbeWritersInterleave is violated"
mutation_run $M LeanProbeHandoff.cfg "probe: a claim by the syncer the release NAMED actually happens" \
  "Invariant ProbeHandoffFired is violated"
mutation_run $M LeanProbeEnqueued.cfg "probe: a writer actually queues for the cell" \
  "Invariant ProbeEnqueued is violated"
mutation_run $M LeanProbeDeposalMidCommit.cfg "probe: a quiet holder in its commit section is deposed by the other writer" \
  "Invariant ProbeDeposalMidCommit is violated"
mutation_run $M LeanProbeFenceAbandoned.cfg "probe: a fenced holder abandons its barrier and KEEPS RUNNING (a fence is not a death)" \
  "Invariant ProbeFenceAbandoned is violated"
mutation_run $M LeanProbeDeadHandoffSkipped.cfg "probe: a quiet handoff is actually skipped and the cell taken by a survivor" \
  "Invariant ProbeDeadHandoffSkipped is violated"
mutation_run $M LeanProbeAdoptWithheld.cfg "probe: the adopt-verification actually withholds an entry whose object is gone (without it the control is green over a race never reached)" \
  "Invariant ProbeAdoptWithheld is violated"
mutation_run $M LeanProbeUploadWithheld.cfg "probe: the upload re-verification actually withholds an entry whose LANDED upload a GC took (finding 13's control is not green over a race never reached)" \
  "Invariant ProbeUploadWithheld is violated"

echo
# ---- tranche 7: MODEL THE IMPLEMENTATION (2026-09-15) ---------------------
# The writer-local queue (deletions included), the pull-only boundary and
# the commit that installs nothing — the code's shape since v1.52.0. Every
# run above keeps all three off, and their distinct-state counts are
# unchanged (README).
mutation_run $M LeanBarrierLeaseQueueTombstoneOverHitl.cfg "FINDING (modelling the queue, 19 steps): a queued deletion applied after the same consume adopted a UI write that re-created the path removes it from the tree, and the window clear drops its inbox entry -- acked, tracked by nothing" \
  "Invariant Inv_HITLTracked is violated"
strict_run $M LeanBarrierLeaseQueueHolds.cfg "the code's shape with the fix: a queued deletion applies only while the key is absent"
strict_run $M LeanBarrierLeaseImplHolds.cfg "the tranche-6 breadth world (two paths, HITL, crash, restart) on the code's shape"
# IS LEAKING SAFE?  On a store that does not enforce If-Match on DELETE the
# syncer stops collecting (`conformance.rs`, L-27): the object is LEFT.
# The world below is that store (ConditionalGC=FALSE) plus the give-way
# (CollectorOff=TRUE), in the breadth world, on the shipped baseline rule.
#
# NINE of the ten invariants hold, exhaustively (6.5M distinct states,
# depth 39) -- including Inv_HITLDurable, Inv_NoResurrection and
# Inv_NoDangling, which is the claim the mitigation needed.
#
# Inv_HITLTracked is in this set now.  It used to fail here, and was
# carried as a must-fail so the gap was recorded rather than unasked.  The
# clause was wrong, not the code: "legitimately superseded" was written as
# `objects[p] = 0` -- physical destruction -- which a collector that gives
# way never performs.  Retirement now follows the collector's DECISION, so
# a leak retires the write exactly as a collection does.  The three
# mutations that require this invariant to FAIL were re-run first and all
# three still find their counterexample.
strict_run $M LeanBarrierLeaseCollectorOff.cfg "the collector GIVES WAY on a store without a conditional DELETE: nine invariants hold, exhaustively"
mutation_run $M LeanBarrierLeaseCollectorOffNoTombstone.cfg "H1c/H1d/H1e: a UI write adopted while pending, cited and then knowingly deleted by the writer that cited it (the adopter after a restart, a citer that restarted before its window clear, or plainly another writer), is re-cited by the adopter's repair -- without a tombstone in the document nothing tells it" \
  "Invariant Inv_NoDeleteResurrected is violated"
mutation_run $M LeanProbeCollectorLeaked.cfg "probe: the collector actually gave way on a path it would have collected (or the run above proves nothing)" \
  "Invariant ProbeCollectorLeaked is violated"
# IS THE SNAPSHOT READ SAFE?  `barrier.rs` step 1 reads the cell ONCE and
# the consume integrates THAT snapshot, so a peer's window clear can drop
# an entry in between and this writer still adopts it. The replay of
# churn/p47.txt forced the model to have that window at all (W4 phase 2):
# before InboxSnapshot the model read `inbox` at the instant of the
# consume and could not take a step the code took. It holds.
strict_run $M LeanBarrierLeaseInboxSnapshot.cfg "the barrier consumes the cell it read at its FIRST step, and a peer's window clear in between changes nothing"
mutation_run $M LeanProbeStaleInboxAdopt.cfg "probe: a consume actually adopted an entry the cell no longer holds (or the run above proves nothing)" \
  "Invariant ProbeStaleInboxAdopt is violated"
mutation_run $M LeanBarrierLeaseQueueDropped.cfg "known-bad for the ack's third refinement: the merge base moves past a peer's change and nothing queues it -- the queue exemption must not excuse a document ahead by NOTHING queued" \
  "Invariant Inv_AckBoundaryCoherent is violated"
mutation_run $M LeanProbePullOnly.cfg "probe: a pull-only boundary runs (no claim, no CAS)" \
  "Invariant ProbePullOnly is violated"
mutation_run $M LeanProbeEmptyInstall.cfg "probe: a commit whose merge equals theirs installs nothing" \
  "Invariant ProbeEmptyInstall is violated"
mutation_run $M LeanProbeTombstoneApplied.cfg "probe: a queued deletion removes a clean copy from the other writer's tree" \
  "Invariant ProbeTombstoneApplied is violated"
mutation_run $M LeanProbeTombstoneSuperseded.cfg "probe: the fix fires -- a queued deletion is superseded by an object at the key" \
  "Invariant ProbeTombstoneSuperseded is violated"

# FINDING 10 (shipped 2026-09-16 as untracked.rs), as convergence: a pod
# replaced between its upload and its commit leaves bytes at a cited key that
# nothing tracks, once no syncer can move. The sweep tracks such an object
# through the inbox.
mutation_run $M LeanBarrierLeaseOrphanDiverges.cfg "FINDING 10: a writer lost between its upload and its commit leaves an untracked upload at a cited key when every syncer is quiet" \
  "Invariant Inv_QuiescentConverged is violated"
# Review 2026-09-18, H1b: the sweep's entry outlived the commit that cited its
# object and, after the uploader's own delete, re-cited the retired generation
# through the other writer's consume.  The known-bad run is kept; the fix
# (the entry carries the citation it was judged against) is in IMPL.
mutation_run $M LeanBarrierLeaseOrphanResurrects.cfg "H1b as shipped (the tombstone off too -- with it on, H1e closes this route as well): the sweep's entry outlives the citation it was judged against and a published delete is resurrected through it" \
  "Invariant Inv_NoDeleteResurrected is violated"
strict_run $M LeanBarrierLeaseOrphanTracked.cfg "finding 10's sweep with H1b's fix: a live writer tracks the orphan through the inbox, at ANY time (no grace), and every invariant holds"
mutation_run $M LeanProbeOrphanTracked.cfg "probe: the orphan is actually tracked" \
  "Invariant ProbeOrphanTracked is violated"
mutation_run $M LeanProbeOrphanOutlived.cfg "probe: a sweep entry whose citation moved on is actually dropped" \
  "Invariant ProbeOrphanOutlived is violated"
mutation_run $M LeanBarrierLeaseOrphanStaleCopy.cfg "H1b, convergence (four barriers): a leaked generation the tree never integrated supersedes the tombstone and a clean stale copy stays forever" \
  "Invariant Inv_TreesConverged is violated"
strict_run $M LeanBarrierLeaseOrphanConverges.cfg "H1b's leak rule: a peer's published delete reaches every live tree, and every IMPL invariant holds (four barriers)"
mutation_run $M LeanBarrierLeaseSupersedeDropsBase.cfg "H1f (review 2026-09-18): a queued deletion superseded by a re-cite is settled without restoring the merge base; the path is deleted again before the install and the clean copy of the retired generation stays forever" \
  "Invariant Inv_TreesConverged is violated"
mutation_run $M LeanProbeBaseRestored.cfg "probe: superseding a deletion actually restores the merge base" \
  "Invariant ProbeBaseRestored is violated"
mutation_run $M LeanProbeLeakApplied.cfg "probe: a tombstone is actually applied over a leaked generation" \
  "Invariant ProbeLeakApplied is violated"

# ---- review 2026-09-18: C2 and H1 ------------------------------------------
# C2: THE FENCE IS A COUNT, NOT A CLOCK.  The code read the cell before the
# CAS and then every 200 deletes; the DELETE carries no epoch.  A holder
# deposed AFTER its CAS landed still deletes, and takes the etag the
# successor just re-cited (same bytes, or an adoption).  The module fenced
# every GCDelete atomically, so it could not see it.
mutation_run $M LeanBarrierLeaseStragglerGC.cfg "C2 (review 2026-09-18): a holder deposed after its CAS landed runs its deletes unfenced and takes an etag the successor's commit has just re-cited -- the citation dangles" \
  "Invariant Inv_NoDangling is violated"
strict_run $M LeanBarrierLeaseStragglerGCFenced.cfg "C2's fix: the collector observes the cell before every delete (a renew by time in the code), and the thawed straggler is fenced there"
mutation_run $M LeanBarrierLeaseStragglerLoadsSuccessor.cfg "H2 (review 2026-09-18): the commit read the cell BEFORE its HEAD fan-out and window; a holder deposed in between loads its successor's rotated document and its CAS lands -- the rotation fences nothing" \
  "Invariant Inv_NoStragglerInstall is violated"
mutation_run $M LeanProbeStragglerGCFenced.cfg "probe: the thawed straggler actually reaches its GC and is fenced THERE (or the run above proves nothing)" \
  "Invariant ProbeFenceAbandoned is violated"
# H1: COLLECTOR-OFF IS NOT A COST.  On a store without a conditional DELETE
# the peer's leaked object supersedes its own tombstone and the citation
# repair re-cites the deleted generation; the two writers flap forever.
mutation_run $M LeanBarrierLeaseLeakResurrects.cfg "H1 (review 2026-09-18): on a collector-off store a peer's published delete is superseded by its own leaked object and RE-CITED by the other writer's repair -- the delete is resurrected with no write" \
  "Invariant Inv_NoDeleteResurrected is violated"
strict_run $M LeanBarrierLeaseLeakHolds.cfg "H1's fix: the tombstone carries the generation the delete retired and applies over the leak; every invariant holds on the code's shape, and the tree converges"
strict_run $M LeanBarrierLeaseLeakRetiredOnly.cfg "H1 with the leak rule, the tombstone and the restored base off: the retired-etag rule alone closes the resurrection (each later rule subsumed it, so the pair above no longer isolates it)"
strict_run $M LeanBarrierLeaseLeakRestoreOnly.cfg "H1f's restored base with H1's, H1b's and H1e's rules off: the base being honest, there is no repair for the resurrection to ride -- it closes that half alone"
mutation_run $M LeanBarrierLeaseLeakFlaps.cfg "H1f's restored base with H1's and H1b's rules off, under the convergence invariant: the retired generation the collector left supersedes the deletion at every consume and the next install queues it again -- the leak flaps and the tree never converges" \
  "Invariant Inv_TreesConverged is violated"
strict_run $M LeanBarrierLeaseLeakRuleConverges.cfg "one arm from LeakFlaps: with the leak rule back on the deletion applies over the leak and the tree converges"
mutation_run $M LeanBarrierLeaseLeakSkippedGeneration.cfg "one arm from LeakHolds (the leak rule off): a writer that never installed the generation the collector left holds a tombstone naming an older one, the retired-etag rule cannot recognise the leak, and it supersedes the deletion forever" \
  "Invariant Inv_TreesConverged is violated"
mutation_run $M LeanProbeTombstoneOverLeak.cfg "probe: a queued deletion is actually applied over a leaked object (or the run above proves nothing)" \
  "Invariant ProbeTombstoneApplied is violated"

# ---- review 2026-09-18: H10 ------------------------------------------------
# THE ACK READS THE WRONG BARRIER.  The barrier that carried a declaration is
# not always the one whose report the ack reads: a restart after step 7 (the
# crash world at OrphanTrack=TRUE, depth 25), an ack write that failed, or an
# honor that failed before the floor's cadence barrier published it.  When a
# peer deleted a declared path in between, the later barrier merged the
# delete and the ack said `ok` while the tree still held the declared bytes.
# The first sentinel world on the code's shape in the gate (H8).
mutation_run $M LeanBarrierLeaseAckAfterRestart.cfg "H10 (review 2026-09-18): a restart between the install and the ack; a peer deletes the declared path; the re-run's pull-only is acked ok while the tree still holds the declared bytes" \
  "Invariant Inv_AckImpliesCited is violated"
strict_run $M LeanBarrierLeaseAckCarried.cfg "H10's fix: each carrying install journals the declared paths with its CAS, and the ack reports one whose deletion by another writer waits in the queue as dropped -- every sentinel invariant holds on the code's shape with a restart"
mutation_run $M LeanProbeCarrierAck.cfg "probe: the journalled carrier alone actually makes an ack partial (or the run above proves nothing)" \
  "Invariant ProbeCarrierAck is violated"

# ---- immutable object handles (design 2026-09-19, the structural fix for H6)
# Every write lands at a handle nobody else writes; the manifest cites
# handles; a cited handle is never overwritten and is collected only once no
# document cites it (docs/plans/flint-lean-immutable-objects-design.md).  The
# arm is NOT in IMPL — it is the design, not the code — and this block is its
# evidence: the design's rules each refuted by one mutation, the known-bad
# shapes of the same-key classes run again under handles and holding, and
# the new steps shown to fire.
strict_run $M LeanImmutableHolds.cfg "immutable handles: the breadth world (two paths, a UI write, a crash, a restart) on the code's shape, every handles invariant"
strict_run $M LeanImmutableQueueHolds.cfg "immutable handles: the queue world (one path, three barriers, a UI write)"
strict_run $M LeanImmutableRenameHolds.cfg "immutable handles: a rename is a citation move -- the removal world under the barrier lease, rename atomicity and no-hole included"
# H8 (2026-09-23): the narrow verb ships (verbs.rs) and had been checked on
# the life lease only.  Narrowing is ONE writer's scope, so both claims are
# the narrowing writer's: it never classifies a path it narrowed as a
# delete, and never re-uploads one (a peer that still watches the path may).
strict_run $M LeanImmutableNarrowHolds.cfg "narrow over handles: a narrow is an UNWATCH for the writer that narrowed -- it never deletes or re-cites the dropped path"
mutation_run $M LeanImmutableNarrowUnlinkFirst.cfg "narrow over handles, unlink-then-uncite: the narrowing writer's next walk reads the dropped path as a delete" \
  "Invariant Inv_NarrowNeverDeletes is violated"
mutation_run $M LeanImmutableNarrowUncieFirst.cfg "narrow over handles, uncite-then-unlink: the surviving file reads as a local ADD and the narrowing writer re-cites it" \
  "Invariant Inv_NarrowNeverRecites is violated"
mutation_run $M LeanImmutableProbeNarrow.cfg "probe: the narrow verb actually fires over handles" \
  "Invariant ProbeNarrow is violated"
# L-123 (2026-09-23): a sync that moves a path's merge base past a queued
# peer change must prune it, or the next consume adopts the OLDER version
# over the newer one -- a peer's v3 reverted for everyone.
strict_run $M LeanImmutableSyncQueueHolds.cfg "L-123's fix: a sync prunes the writer-local queue where it moved the merge base, and no consume ever adopts an older version than the tree holds"
mutation_run $M LeanImmutableSyncQueueRegress.cfg "L-123 (the shape before the fix): sync never reads the queue, and a consume adopts a queued version the sync already overtook" \
  "Invariant Inv_ConsumeNeverRegresses is violated"
strict_run $M LeanImmutableSyncQueueShippedInvs.cfg "L-123's blind spot: on the unfixed shape every invariant that existed before it HOLDS -- only Inv_ConsumeNeverRegresses sees the revert"
mutation_run $M LeanImmutableRepairRecites.cfg "handles rule 2 refuted: an adopter that never saw the removal re-cites the moved handle at its old name -- one handle under two names (the rename world's fourth box run, depth 18)" \
  "Invariant Inv_OneName is violated"
mutation_run $M LeanImmutableRepairOverridesUI.cfg "a citation repair refuted as an upload's equal: it re-cited an older adoption over a later UI write the document cited, published by a peer, with no record (the rename world's eighth box run, depth 18)" \
  "Invariant Inv_HITLDurable is violated"
mutation_run $M LeanImmutablePendingAdoptionDropped.cfg "a pending adoption refuted as gone: an adoption declined for the MOVE ALONE, at the destination of a rename still waiting at its source, left the cell with the rest of the consumed -- the peer that applied the answered record uncited the source, and the collector took the handle (the rename world's tenth box run, L-119)" \
  "Invariant Inv_HITLDurable is violated"
mutation_run $M LeanImmutableProbePending.cfg "probe: a barrier actually declines an adoption for the move alone and leaves its entry in the cell (L-119's rule, reached)" \
  "Invariant ProbePendingKept is violated"
mutation_run $M LeanImmutableAnsweredRecordSkipped.cfg "an answered removal record refuted as final: a tree that adopted the source's write before the rename keeps it at the old name beside the destination's and cites one handle twice (the core model's first run; L-117 is the same rule's one-barrier lag)" \
  "Invariant Inv_OneName is violated"
mutation_run $M LeanImmutableRetirePerPath.cfg "handles rule 1 refuted: retirement judged path by path forgets that the rename's destination cites the source's handle, and the collector takes it" \
  "Invariant Inv_NoDangling is violated"
mutation_run $M LeanImmutableCasCitesBlind.cfg "handles rule 2 refuted: the commit cites its own uploads without re-reading them and the other writer's sweep has taken one (no grace is modelled: the grace is not what keeps this safe)" \
  "Invariant Inv_NoDangling is violated"
strict_run $M LeanImmutableCasVerifies.cfg "handles rule 2: the commit re-reads every upload it cites (its own step, before the CAS) and withholds what the sweep took"
mutation_run $M LeanImmutableSweepLeaseFree.cfg "handles rule 3 refuted: a LEASE-FREE sweep lands between a writer's re-read and its CAS, and the installed document cites a collected handle" \
  "Invariant Inv_NoDangling is violated"
mutation_run $M LeanImmutableSweepTakesTracked.cfg "handles rule 4 refuted: a sweep that does not spare what an inbox entry names takes an acked UI write before any barrier consumes it" \
  "Invariant Inv_HITLDurable is violated"
strict_run $M LeanImmutableGCUnconditional.cfg "retired class (F1/L-10): the HEAD-then-DELETE collector's world under handles, the mutation left on and inert -- every handles invariant holds"
strict_run $M LeanImmutableAdoptBlind.cfg "retired class (F2/L-11): the blind adopt's restart world under handles, the mutation left on and inert"
strict_run $M LeanImmutableHitlOverAny.cfg "retired class (F8/L-16): the gateway overwriting any current object, under handles there is no object to overwrite"
mutation_run $M LeanImmutableCasOverridesUI.cfg "handles rule 5 refuted: a commit cites its upload over an acked UI write a peer published and it never integrated -- no slot, no 412, no record; the first HitlOverAny run under handles, depth 18" \
  "Invariant Inv_HITLDurable is violated"
mutation_run $M LeanImmutableCasOverridesPeer.cfg "handles rule 5 refuted (peer): the same commit over a peer's own publish, with no record" \
  "Invariant Inv_NoStaleOverride is violated"
strict_run $M LeanImmutableLeakHolds.cfg "retired class (H1/L-105, L-27, L-102): the collector-off store's leak world under handles with both leak rules off; the tree converges and no delete is resurrected"
strict_run $M LeanImmutableStragglerGC.cfg "retired class (C2/L-104): a holder deposed after its CAS runs its deletes unfenced; a retired handle is never re-cited, so nothing dangles"
strict_run $M LeanImmutableOrphanCollected.cfg "retired class (finding 10 / H6): a writer's pod replaced between its upload and its commit leaves a handle nothing cites; it is collected, never adopted path by path"
mutation_run $M LeanImmutableProbeSwept.cfg "probe: the sweep actually collected a lost writer's handle (or the run above proves nothing)" \
  "Invariant ProbeSwept is violated"
mutation_run $M LeanImmutableProbeGC.cfg "probe: a retired handle is actually collected under handles" \
  "Invariant ProbeGC is violated"
mutation_run $M LeanImmutableProbeUploadWithheld.cfg "probe: the commit's re-read actually withheld a swept upload" \
  "Invariant ProbeUploadWithheld is violated"
mutation_run $M LeanImmutableProbeHITLCited.cfg "probe: a UI write is actually cited under handles" \
  "Invariant ProbeHITLCited is violated"
mutation_run $M LeanImmutableProbeRename.cfg "probe: a rename is actually performed under handles" \
  "Invariant ProbeRenameApplied is violated"

# The expected total is ASSERTED, not printed: a hardcoded denominator
# that drifts below the real run count turns "83/79 green" into a line
# nobody reads as wrong. It had drifted to 79 against 79 real runs
# before this tranche; the prose count at the top of this file had
# drifted further still, to "Fifty-five".
# The coverage matrix is part of the gate: a cfg added without regenerating
# it leaves a hole nobody can see (COVERAGE.md, lean/SAFETY.md).
if ! python3 "$(dirname "$0")/coverage.py" --check; then
  echo "FAIL: lean/formal/COVERAGE.md is stale — run lean/formal/coverage.py"
  exit 1
fi

EXPECT=182
echo
if [ "$PASS" -ne "$EXPECT" ]; then
  echo "lean formal gate: $PASS runs green but $EXPECT were declared — a run was"
  echo "added or removed without updating EXPECT, so the gate is no longer"
  echo "counting what it claims to count."
  exit 1
fi
echo "lean formal gate: $PASS/$EXPECT runs green"
if [ "$REPLAYED" -gt 0 ]; then
  echo "lean formal gate: NOT A FRESH RUN — $REPLAYED of those were REPLAYED from the"
  echo "journal (proved by an earlier process at this same fingerprint) and only"
  echo "$RANNOW ran here. Evidence for the record wants GATE_RESUME unset."
fi
