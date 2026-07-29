#!/usr/bin/env bash
# TLC gate for the formal models (formal/FlintReplication.tla — the
# replica-lifecycle / writer-set machine; formal/FlintSnapshots.tla — the
# epoch-chain / delta-copy protocol at block-content level).
#
# Forty-three runs, ALL required.
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
# Availability-envelope tranche (2026-07-29 conformance audit — the
# automatic arms the model previously idealized away while claiming
# correspondence; constants GateDeadline/StaleFloor/MonitorCurrent):
#   10a. FlintReplicationGateReal.cfg    strict — BOTH shipped arms ON
#      (the gate's 180s defer deadline + the 2-base-floor forced-stale
#      admission): InvCore (everything except Inv_NoFalseRisk, which
#      only the idealization satisfies) + post-storm liveness must HOLD
#      over the mixed insync+stale serving states.
#   10b. FlintReplicationGateRealHollow.cfg GateDeadline teeth — TLC must
#      FIND Inv_NoFalseRisk violated: the deadline arm excuses a merely
#      BLACKHOLED (recoverable) writer — the hollow risk the code
#      deliberately trades for "Never hang" (drill 2.4).
#   10c. FlintReplicationGateRealStale.cfg StaleFloor teeth — TLC must
#      FIND Inv_NoStaleServe violated: a record-Stale leg auto-admitted
#      beside the in-sync survivor, gate reads Proceed, NO risk marker
#      (only the StaleReplicaAdmitted event).
#   10d. FlintReplicationMonitorLag.cfg   MonitorCurrent=FALSE teeth —
#      TLC must FIND the one-monitor-tick silent stale-read: a
#      deconfigured-not-yet-marked leg recovers and re-enters a fresh
#      superblock:false raid content-behind (no SPDK examine belt
#      exists; the strict runs' NewestOf is a TIMING AXIOM, not code).
#
# Expansion (audit continuation):
#   10e. FlintReplicationExpandShrinkReal.cfg DeviceFloor=FALSE (the
#      shipped PV-capacity-only belt floor) — TLC must FIND
#      Inv_NoDeviceShrink violated: PV capacity lags the device after a
#      partial fan-out and a lone pre-expand leg passes the old floor —
#      the volumeMode:Block silent shrink.  DeviceFloor=TRUE in the
#      Expand strict cfg is the wave-2 fix.
#
# Maintenance (audit continuation):
#   10f. FlintReplicationMaintPark.cfg    SuppressScoped=FALSE (the
#      shipped volume-wide plan_hot_rejoin gate) + wedged roll at 3 legs
#      — TLC must FIND the StandbyAdmissionNotParked lasso: a warm
#      standby on an UNMARKED node parks forever behind another node's
#      forever-renewed mark.
#   10g. FlintReplicationMaintParkFixed.cfg SuppressScoped=TRUE (per-leg
#      marks, the design semantics — the wave-2 fix): the same world
#      must HOLD StandbyAdmissionNotParked; also the tranche's first
#      3-leg liveness coverage.
#   10h. FlintReplicationRollNoBelt.cfg   DrainBelt=FALSE (the pre-fix
#      record-level last-serving-member check) — TLC must FIND the
#      RecordBarrier silent loss, restoring that bug class's mutation
#      (the fix had erased the pre-fix world from the config space).
#   10i. FlintReplicationRollRecordBarrier3.cfg strict — the record-only
#      barrier the implementation SHIPS, at 3-leg arity (invariants).
#   10j. FlintReplicationRollRecordBarrierDeep.cfg strict — the
#      record-only barrier under the deep 2-leg budget, full liveness.
#      (10i/10j were first run green by the audit verifier; now gated.)
#
# Two-roller tranche (2026-07-29 — is the roller's lease safety-load-
# bearing?  Answer, machine-checked: NO — the belt is, and it was
# missing; constants RollerRace/RollerLeaderGate/DrainMarksBelt):
#   10k. FlintReplicationRollerRace.cfg    gate ON, shipped mutator —
#      TLC must FIND Inv_PlannedRollBoundedImpact violated at 3 legs
#      with zero failures: a deposed-but-alive roller's in-flight drain
#      lands after the new leader's drain marked a different node (F59
#      candidate; one-node-at-a-time and the barrier are planner-only
#      snapshot reads; the lease is checked before the work, not at the
#      commit).
#   10l. FlintReplicationRollerRaceUngated.cfg no leadership at all —
#      TLC must FIND the same violation (the F50 split-process shape
#      applied to the roller; the gate changes nothing the belt does
#      not already decide).
#   10m. FlintReplicationRollerRaceFixed.cfg DrainMarksBelt=TRUE with NO
#      leader gate — strict, must HOLD: exclusivity AND record
#      redundancy re-verified inside drain_for_maintenance (where the
#      rv-guarded retry makes them race-proof) carry planned-maintenance
#      safety ALONE.  Its first run beat a marks-only belt with the
#      capture→drain→roll→clear→commit erosion — the barrier had to
#      move into the mutation too.  The roller's lease buys pacing, not
#      safety: the FlintClaimsNoLeader verdict, extended to the roller.
#
# Cutover tranche (the RWX bounce — cutover.rs, the one protocol-shaped
# subsystem that had no model at all until 2026-07-29):
#   11a. FlintReplicationBounce.cfg     strict — the controller-initiated
#      ZERO-FAILURE teardown with the PROPOSED commit-time preflight ON,
#      plus admit_standbys_at_stage (the bounce's return path, which the
#      existing Admit cannot represent: it runs in the NODE process, under
#      no claim, and grows the writer set BEFORE the gate rules).  Must
#      HOLD, with the post-bounce liveness.
#   11b. FlintReplicationBounceRisk.cfg BouncePreflight=FALSE (the SHIPPED
#      planner: VolumeCutoverView carries no leg health, no serving
#      membership, no writer set) — TLC must FIND Inv_NoBounceInducedRisk
#      violated: ONE bouncer, lease honored, no race, tears down a volume
#      on a STALE data-path flag and the reassembly has to excuse an acked
#      tail that was recoverable all along.  Cutover has no DrainBelt.
#   11c. FlintReplicationBounceRace.cfg the same with a deposed-but-alive
#      second bouncer and the leader gate ON — TLC must FIND it again:
#      is_leader() is read once per tick (the single occurrence in 1548
#      lines) while the tick's work is unbounded.  Unlike the roller,
#      cutover has NO CAS anywhere to move a belt into.
#   11d. FlintReplicationBounceRaceFixed.cfg belt ON, NO leader gate at
#      all — strict.  The sharp theorem: the preflight ALONE carries
#      bounce safety; the lease buys pacing, not safety (a third instance
#      of the FlintClaimsNoLeader / RollerRaceFixed verdict).
#   11e. FlintReplicationBounceLoop.cfg  the pointless-rebounce CANARY —
#      with every individual bounce belted safe, TLC must still FIND
#      Inv_NoPointlessRebounce violated: no attempt counter, no backoff,
#      and a data-path flag only its (possibly dead) flagging node may
#      clear.  The fix is owed in CODE, not in the model.
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
  # Pinned (2026-07-29 audit): the pass/fail greps below match TLC's exact
  # output phrases, which are version-sensitive — "releases/latest" could
  # silently change them and turn every mutation run vacuous.  v1.7.4 is
  # the version this gate was validated against.
  curl -fsSL -o "$JAR" \
    https://github.com/tlaplus/tlaplus/releases/download/v1.7.4/tla2tools.jar
fi

run_tlc() { # <module> <cfg>
  # Per-cfg -metadir: TLC's default scratch dir is named by wall-clock
  # second, so two fast runs starting within the same second collide and
  # the later one aborts before checking anything.
  java -XX:+UseParallelGC -cp "$JAR" tlc2.TLC -workers auto \
    -metadir "states/${2%.cfg}" -config "$2" "$1.tla" 2>&1
}

# Pass/fail checks use bash substring/regex matching on the captured
# output — NOT `echo | grep -q` pipelines: grep -q exits at first match,
# and under `set -o pipefail` the writer's SIGPIPE turns a PASSING run
# into a spurious red (the harness SIGPIPE class that bit two chaos
# checks on runaj).  The failure-path `tail -30` pipes are safe (tail
# consumes all input).

strict_run() { # <module> <cfg> <label>
  echo "== $3 ($2): invariants must hold =="
  local OUT
  OUT=$(run_tlc "$1" "$2") || { echo "$OUT" | tail -30; echo "FAIL: $3 errored"; exit 1; }
  [[ "$OUT" == *"Model checking completed. No error has been found."* ]] \
    || { echo "$OUT" | tail -30; echo "FAIL: $3 did not verify"; exit 1; }
  awk '/distinct states|depth/ && c < 2 { print; ++c }' <<<"$OUT"
}

mutation_run() { # <module> <cfg> <label> <expected-violation-regex>
  echo "== $3 ($2): TLC must FIND the loss =="
  local MOUT PAT
  MOUT=$(run_tlc "$1" "$2" || true)
  PAT="Invariant $4 is violated"
  [[ "$MOUT" =~ $PAT ]] \
    || { echo "$MOUT" | tail -30; echo "FAIL: $3 did NOT find the loss — the model lost its teeth"; exit 1; }
  echo "counterexample found (as required)"
}

liveness_mutation_run() { # <module> <cfg> <label>
  echo "== $3 ($2): TLC must FIND the starvation lasso =="
  local MOUT
  MOUT=$(run_tlc "$1" "$2" || true)
  [[ "$MOUT" == *"Temporal properties were violated"* ]] \
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

strict_run FlintReplication FlintReplicationGateReal.cfg "availability envelope strict (GateDeadline+StaleFloor: the shipped NodeStage arms)"

mutation_run FlintReplication FlintReplicationGateRealHollow.cfg "deadline-arm teeth (GateDeadline: hollow risk excused on transient evidence)" "Inv_NoFalseRisk"
mutation_run FlintReplication FlintReplicationGateRealStale.cfg "forced-stale teeth (StaleFloor: stale leg served beside a survivor, no marker)" "Inv_NoStaleServe"
mutation_run FlintReplication FlintReplicationMonitorLag.cfg "record-currency-axiom teeth (MonitorCurrent=FALSE: the one-tick stale-read window)" "Inv_NoSilentLoss"

strict_run FlintReplication FlintReplicationMaint.cfg "maintenance strict breadth (drain+barrier+lease, rolls enabled)"
strict_run FlintReplication FlintReplicationMaintDeep.cfg "maintenance strict content depth (torn/scrub/zombie/roller-death across a roll)"

mutation_run FlintReplication FlintReplicationRollUnfenced.cfg "roll-fence mutation (MaintFence=FALSE, the csi-node roll landmine)" "Inv_PlannedRollNeverCausesOutage"
mutation_run FlintReplication FlintReplicationRollBarrier.cfg  "roll-barrier mutation (MaintBarrier=FALSE, redundancy erosion at 3 legs)" "Inv_PlannedRollBoundedImpact"

liveness_mutation_run FlintReplication FlintReplicationRollLease.cfg "roll-lease mutation (MaintLease=FALSE, the mark outlives its roller)"

strict_run FlintReplication FlintReplicationRollRecordBarrier.cfg "record-only barrier strict (the implementation's barrier; belt holds safety)"
strict_run FlintReplication FlintReplicationRollWedged.cfg "wedged-restart strict (kubelet never returns; survivor stays writable)"

strict_run FlintReplication FlintReplicationRollRecordBarrier3.cfg "record-only barrier strict, 3-leg arity (audit 10i)"
strict_run FlintReplication FlintReplicationRollRecordBarrierDeep.cfg "record-only barrier strict, deep liveness (audit 10j)"

mutation_run FlintReplication FlintReplicationRollNoBelt.cfg "drain-belt mutation (DrainBelt=FALSE: the RecordBarrier silent loss, rediscoverable again)" "Inv_NoSilentLoss"

mutation_run FlintReplication FlintReplicationRollerRace.cfg "two-roller race, gate ON (deposed roller's stale drain lands — the lease cannot close it; F59 candidate)" "Inv_PlannedRollBoundedImpact"
mutation_run FlintReplication FlintReplicationRollerRaceUngated.cfg "two-roller race, no leadership (the split-process shape on the roller)" "Inv_PlannedRollBoundedImpact"

strict_run FlintReplication FlintReplicationRollerRaceFixed.cfg "drain-mutation belts strict (exclusivity + record redundancy in the CAS; no leader gate at all)"

liveness_mutation_run FlintReplication FlintReplicationMaintPark.cfg "volume-wide-parking mutation (SuppressScoped=FALSE + wedged roll: the parked standby, F43's third door)"

strict_run FlintReplication FlintReplicationMaintParkFixed.cfg "per-leg suppression strict (the parking fix; first 3-leg liveness run)"

strict_run FlintReplication FlintReplicationExpand.cfg "expansion strict (SizeGuard+SizeHeal; the F56 theorem + no-device-shrink)"

liveness_mutation_run FlintReplication FlintReplicationExpandWedge.cfg "F56 mutation (SizeHeal=FALSE: the expand x chase size livelock)"

mutation_run FlintReplication FlintReplicationExpandGuard.cfg "size-guard mutation (SizeGuard=FALSE: silent device shrink)" "Inv_NoDeviceShrink"

mutation_run FlintReplication FlintReplicationExpandShrinkReal.cfg "shipped-floor mutation (DeviceFloor=FALSE: PV-capacity belt lags the device — Block-mode shrink)" "Inv_NoDeviceShrink"

# Cutover tranche (the RWX bounce: cutover.rs plan→bounce→verify→judge).
strict_run FlintReplication FlintReplicationBounce.cfg "bounce strict (commit-time preflight + the at-stage admission; both planner arms, two failures)"

mutation_run FlintReplication FlintReplicationBounceRisk.cfg "bounce-preflight mutation (BouncePreflight=FALSE: the shipped planner reads no leg health — the manufactured outage and its hollow risk)" "Inv_NoBounceInducedRisk"

mutation_run FlintReplication FlintReplicationBounceRace.cfg "two-bouncer race, gate ON (deposed bouncer's captured plan lands; cutover has NO CAS to belt — F59's shape without F59's remedy)" "Inv_NoBounceInducedRisk"

strict_run FlintReplication FlintReplicationBounceRaceFixed.cfg "bounce-preflight strict, no leader gate at all (the belt alone carries bounce safety)"

mutation_run FlintReplication FlintReplicationBounceLoop.cfg "pointless-rebounce canary (a flag only a dead node could clear, no attempt counter anywhere — the belt does not close churn)" "Inv_NoPointlessRebounce"

strict_run FlintClaims FlintClaims.cfg "claims strict (two processes, Lease + marker grace; F50/F53 layer)"

mutation_run FlintClaims FlintClaimsNoGrace.cfg "F50 mutation (MarkerGrace=FALSE: scrub under a live window, cold admission)" "Inv_NoColdAdmission"

strict_run FlintClaims FlintClaimsNoLeader.cfg "no-leader strict (F53 world: safety never depended on the singleton)"

strict_run FlintSnapshots FlintSnapshots.cfg "snapshots strict (full ordered walk, blobstore relink)"

mutation_run FlintSnapshots FlintSnapshotsSplit.cfg      "delta-split mutation (WalkFull=FALSE)"       "Inv_SessionFaithful"
mutation_run FlintSnapshots FlintSnapshotsOrder.cfg      "walk-order mutation (OrderedWalk=FALSE)"     "Inv_SessionFaithful"
mutation_run FlintSnapshots FlintSnapshotsBareDelete.cfg "bare-delete mutation (RelinkOnDelete=FALSE)" "Inv_SessionFaithful"

echo "TLA GATE PASSED"
