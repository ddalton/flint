#!/usr/bin/env bash
# TLC gate for the formal models (formal/FlintReplication.tla — the
# replica-lifecycle / writer-set machine; formal/FlintSnapshots.tla — the
# epoch-chain / delta-copy protocol at block-content level).
#
# Twenty-five runs, ALL required.
#
# FlintReplication:
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
#   5b. FlintReplicationF43.cfg   ClaimArb=FALSE — TLC must FIND the
#      admission-starvation lasso (the F43 parked standby).
#   5c. FlintReplicationResurrect.cfg EvidenceStrict=FALSE — TLC must FIND
#      an Inv_NoFalseRisk violation (hollow surfaced risk).
#   5d. FlintReplicationP4.cfg    SpecNoP4 — TLC must FIND the unbounded
#      write-stall lasso (pre-P4 detection).
#
# Maintenance tranche (the csi-node roll landmine):
#   5e. FlintReplicationMaint.cfg        strict 3-leg breadth — drain+
#      barrier+lease ON, rolls enabled: all invariants (incl. planned-
#      roll-never-causes-outage, fence-holds) and all liveness (incl.
#      maintenance-eventually-lifts) must HOLD.
#   5e'. FlintReplicationMaintDeep.cfg   strict 2-leg content depth —
#      torn writes, scrub, zombies and roller death reachable across a
#      roll campaign; its first run found the dead-leg counterexample
#      that forced the per-leg, death-escaped statement of the lifts
#      property.  Must HOLD.
#   5f. FlintReplicationRollUnfenced.cfg MaintFence=FALSE (today's world) —
#      TLC must FIND Inv_PlannedRollNeverCausesOutage violated: a routine
#      DS roll with ZERO real failures drives serving to {}.
#   5g. FlintReplicationRollBarrier.cfg  MaintBarrier=FALSE (pod-ready is
#      not readmitted) — TLC must FIND Inv_PlannedRollBoundedImpact
#      violated at 3 legs: with the last-serving-member belt stopping the
#      direct outage, the barrier's necessity is redundancy EROSION (two
#      legs out of service under planned maintenance alone).
#   5h. FlintReplicationRollLease.cfg    MaintLease=FALSE — TLC must FIND
#      the MaintenanceEventuallyLifts lasso: a dead roller's suppression
#      mark parks the drained leg forever.
#   5i. FlintReplicationRollRecordBarrier.cfg BarrierRaidAware=FALSE — the
#      barrier the IMPLEMENTATION has (record-only). Strict: must HOLD.
#      Its first run (no ground-truth belt) found a REAL silent-loss
#      composition — drain armed on a record lagging the raid prunes the
#      sole serving leg from the writer set — which forced the
#      unconditional last-serving-member belt (probe-first in code).
#   5j. FlintReplicationRollWedged.cfg   SpecWedgedKubelet (the roll's pod
#      never comes back) — strict: every invariant + writability on the
#      survivor must HOLD (a wedged restart degrades one leg, nothing
#      else; the parked mark is the honest operational state).
#
# Expansion tranche (the F56 size dimension):
#   5n. FlintReplicationExpand.cfg      strict 2-leg (SizeGuard + SizeHeal
#      ON, maintenance off) — every core invariant, Inv_NoDeviceShrink,
#      ExpansionCompletes (the F56 theorem) and AdmissionNotStarved must
#      HOLD.  This property is the module's first per-leg progress
#      obligation, and its first runs found: the ghost-epoch model bug
#      (EpochCut cutting acked instead of held content), the missing
#      ReleaseAdmission (a deferral wedging the claim), the WF
#      acquire/release trap for same-class claimants (ExpandLeg now SF —
#      the persistent-retrier abstraction), and CANDIDATE F57 (a standby
#      whose node dies parks forever: no demotion, no replacement —
#      escaped honestly in the property, fix owed in code).
#   5o. FlintReplicationExpandWedge.cfg SizeHeal=FALSE (the shipped
#      pre-F56 code) — TLC must FIND the ExpansionCompletes lasso: leg
#      lost mid-fan-out, survivors grown, device grown, the leg returns
#      as a live content-warm size-old standby — admission size-guard
#      defers it forever, the C2 belt refuses the expand retry, the
#      retention pin holds the full-build escape shut.
#   5p. FlintReplicationExpandGuard.cfg SizeGuard=FALSE (pre-F43-#8) —
#      TLC must FIND Inv_NoDeviceShrink violated: the pre-expand leg
#      admitted under the grown device — the silent shrink.
#
# FlintClaims (the multi-process claims/window layer — the F50/F53 axis):
#   5k. FlintClaims.cfg          strict — Lease + marker grace ON, two
#      processes, deaths + spurious leadership moves: Inv_NoColdAdmission
#      and both liveness properties (window resolves; eventually serves,
#      incl. the owner-dies-mid-window recovery story) must HOLD.
#   5l. FlintClaimsNoGrace.cfg   MarkerGrace=FALSE (pre-F50), Lease still
#      ON — TLC must FIND the cold-admission loss: a deposed-but-alive
#      leader's in-flight dispatch scrubs the new leader's young window
#      and the blind flip commits a cold leg. Proves grace and Lease are
#      complementary layers (the Lease gates ticks, not in-flight ops).
#   5m. FlintClaimsNoLeader.cfg  LeaderGate=FALSE (the F53 world, grace
#      ON) — strict, must HOLD: safety never depended on the process
#      singleton (the record CAS + grace carry it); the Lease buys
#      ownership determinism and churn-freedom, not safety.
#
# FlintSnapshots:
#   6. FlintSnapshots.cfg           strict — every completed copy session
#      delivers exactly the cut (Inv_SessionFaithful) — must HOLD.
#   7. FlintSnapshotsSplit.cfg      WalkFull=FALSE — the delta-split bug;
#      TLC must FIND the loss.
#   8. FlintSnapshotsOrder.cfg      OrderedWalk=FALSE — walk-order bug
#      (what chain.reverse() enforces); TLC must FIND the loss.
#   9. FlintSnapshotsBareDelete.cfg RelinkOnDelete=FALSE — the finding-#1
#      class (bare snapshot delete); TLC must FIND the loss.
#
# A model that cannot rediscover the bug classes it exists for proves
# nothing; the mutation runs are the models' own regression tests.
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

run_tlc() { # <module> <cfg>
  # Per-cfg -metadir: TLC's default scratch dir is named by wall-clock
  # second, so two fast runs starting within the same second collide and
  # the later one aborts before checking anything.
  java -XX:+UseParallelGC -cp "$JAR" tlc2.TLC -workers auto \
    -metadir "states/${2%.cfg}" -config "$2" "$1.tla" 2>&1
}

strict_run() { # <module> <cfg> <label>
  echo "== $3 ($2): invariants must hold =="
  local OUT
  OUT=$(run_tlc "$1" "$2") || { echo "$OUT" | tail -30; echo "FAIL: $3 errored"; exit 1; }
  echo "$OUT" | grep -q "Model checking completed. No error has been found." \
    || { echo "$OUT" | tail -30; echo "FAIL: $3 did not verify"; exit 1; }
  echo "$OUT" | grep -E "distinct states|depth" | head -2
}

mutation_run() { # <module> <cfg> <label> <expected-violation-regex>
  echo "== $3 ($2): TLC must FIND the loss =="
  local MOUT
  MOUT=$(run_tlc "$1" "$2" || true)
  echo "$MOUT" | grep -Eq "Invariant $4 is violated" \
    || { echo "$MOUT" | tail -30; echo "FAIL: $3 did NOT find the loss — the model lost its teeth"; exit 1; }
  echo "counterexample found (as required)"
}

liveness_mutation_run() { # <module> <cfg> <label>
  echo "== $3 ($2): TLC must FIND the starvation lasso =="
  local MOUT
  MOUT=$(run_tlc "$1" "$2" || true)
  echo "$MOUT" | grep -q "Temporal properties were violated" \
    || { echo "$MOUT" | tail -30; echo "FAIL: $3 did NOT find the starvation — the model lost its teeth"; exit 1; }
  echo "temporal counterexample found (as required)"
}

strict_run FlintReplication FlintReplication.cfg     "replication strict breadth (all guards TRUE)"
strict_run FlintReplication FlintReplicationDeep.cfg "replication strict deep budget (scrub/divergence reachable)"

mutation_run FlintReplication FlintReplicationF36c.cfg   "F36c mutation (GateStrict=FALSE)"    "Inv_NoSilentLoss"
mutation_run FlintReplication FlintReplicationRejoin.cfg "rejoin mutation (RejoinGuard=FALSE)"  "Inv_NoDivergentServing"
mutation_run FlintReplication FlintReplicationF48.cfg    "F48 mutation (FenceZombie=FALSE)"    "Inv_(NoSilentLoss|NoDivergentServing)"

liveness_mutation_run FlintReplication FlintReplicationF43.cfg "F43 mutation (ClaimArb=FALSE, admission starvation)"

mutation_run FlintReplication FlintReplicationResurrect.cfg "resurrection mutation (EvidenceStrict=FALSE, hollow risk)" "Inv_NoFalseRisk"

liveness_mutation_run FlintReplication FlintReplicationP4.cfg "P4 mutation (SpecNoP4: unbounded detection, write stall)"

strict_run FlintReplication FlintReplicationMaint.cfg "maintenance strict breadth (drain+barrier+lease, rolls enabled)"
strict_run FlintReplication FlintReplicationMaintDeep.cfg "maintenance strict content depth (torn/scrub/zombie/roller-death across a roll)"

mutation_run FlintReplication FlintReplicationRollUnfenced.cfg "roll-fence mutation (MaintFence=FALSE, the csi-node roll landmine)" "Inv_PlannedRollNeverCausesOutage"
mutation_run FlintReplication FlintReplicationRollBarrier.cfg  "roll-barrier mutation (MaintBarrier=FALSE, redundancy erosion at 3 legs)" "Inv_PlannedRollBoundedImpact"

liveness_mutation_run FlintReplication FlintReplicationRollLease.cfg "roll-lease mutation (MaintLease=FALSE, the mark outlives its roller)"

strict_run FlintReplication FlintReplicationRollRecordBarrier.cfg "record-only barrier strict (the implementation's barrier; belt holds safety)"
strict_run FlintReplication FlintReplicationRollWedged.cfg "wedged-restart strict (kubelet never returns; survivor stays writable)"

strict_run FlintReplication FlintReplicationExpand.cfg "expansion strict (SizeGuard+SizeHeal; the F56 theorem + no-device-shrink)"

liveness_mutation_run FlintReplication FlintReplicationExpandWedge.cfg "F56 mutation (SizeHeal=FALSE: the expand x chase size livelock)"

mutation_run FlintReplication FlintReplicationExpandGuard.cfg "size-guard mutation (SizeGuard=FALSE: silent device shrink)" "Inv_NoDeviceShrink"

strict_run FlintClaims FlintClaims.cfg "claims strict (two processes, Lease + marker grace; F50/F53 layer)"

mutation_run FlintClaims FlintClaimsNoGrace.cfg "F50 mutation (MarkerGrace=FALSE: scrub under a live window, cold admission)" "Inv_NoColdAdmission"

strict_run FlintClaims FlintClaimsNoLeader.cfg "no-leader strict (F53 world: safety never depended on the singleton)"

strict_run FlintSnapshots FlintSnapshots.cfg "snapshots strict (full ordered walk, blobstore relink)"

mutation_run FlintSnapshots FlintSnapshotsSplit.cfg      "delta-split mutation (WalkFull=FALSE)"       "Inv_SessionFaithful"
mutation_run FlintSnapshots FlintSnapshotsOrder.cfg      "walk-order mutation (OrderedWalk=FALSE)"     "Inv_SessionFaithful"
mutation_run FlintSnapshots FlintSnapshotsBareDelete.cfg "bare-delete mutation (RelinkOnDelete=FALSE)" "Inv_SessionFaithful"

echo "TLA GATE PASSED"
