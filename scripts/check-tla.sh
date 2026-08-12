#!/usr/bin/env bash
# TLC gate for the formal models (formal/FlintReplication.tla — the
# replica-lifecycle / writer-set machine; formal/FlintSnapshots.tla — the
# epoch-chain / delta-copy protocol at block-content level).
#
# One hundred and thirty-two runs, ALL required.
#
# (Counted as invocations — `grep -c '^strict_run \|^mutation_run \|^liveness_mutation_run '`
# with the trailing spaces, so the three function DEFINITIONS don't inflate
# the count; that miscount is how this header once briefly read wrong.
#
# The tally is PER MODULE, not per tranche, because the per-tranche
# version drifted silently: it summed to 114 while the file held 116, and
# nothing could notice — a tranche breakdown has no command that
# regenerates it.  This one does:
#
#   awk '/^(strict_run|mutation_run|liveness_mutation_run)[ ]/ {print $2}' \
#     scripts/check-tla.sh | sort | uniq -c | sort -rn
#
#   71 FlintReplication   15 FlintExtents   13 FlintComposition
#   11 FlintExtentsProbe   7 FlintTruncate   5 FlintAdmission
#    4 FlintSnapshots      3 FlintClaims     3 FlintA2Probe)
#
# FlintTruncate.tla — the pNFS truncate gate; the tranche is documented at the
# bottom of this file, next to its runs.
#
# FlintExtents.tla — the block-layout extent allocator (tranche 1: grant/
# fence/reuse core, modelling code that DOES NOT EXIST YET — the module
# comes before the allocator, deliberately); documented at the bottom of
# this file, next to its runs.
#
# FlintComposition.tla — the block-tier serving-composition machine
# (replication failover: serving-target record + epoch, model-before-code
# like FlintExtents was); documented at the bottom of this file, next to
# its runs.
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
#   12a. FlintReplicationBouncePod.cfg   the POD LAYER, brought in after
#      the first tranche abstracted it away: the bare flint-nfs-<vol> pod
#      has TWO independent creators (execute_cutover's recreate and
#      rwx_nfs.rs's liveness reconciler) and nothing mutually excludes
#      them.  With the bouncer IDEALIZED (it recreates only after the
#      unstage it waited for), TLC must still FIND
#      Inv_BounceNotSilentlyDefeated violated: the reconciler recreates
#      the pod inside the detach wait, kubelet reuses the staged volume,
#      no NodeStage runs, and the warm standby stays parked.  Eight
#      states, no partition/zombie/second failure/leader change.
#   12b. FlintReplicationBouncePodFixed.cfg ReconcilerBelt=TRUE (hold off
#      while a window is open — one creator at a time): strict, and the
#      volume must still converge.
#   12c. FlintReplicationBounceTimeout.cfg the SECOND door, with the
#      reconciler already belted: DetachWaitHonored=FALSE is the shipped
#      timeout path where await_detached returning false only WARNS and
#      execute_cutover recreates anyway.  TLC must FIND the same
#      violation — the bouncer defeats its own wait, so the reconciler
#      fix ALONE is insufficient.
#   12d. FlintReplicationBouncePlanner.cfg plan_cutover applies NEITHER
#      of plan_hot_rejoin's admission filters — TLC must FIND
#      Inv_NoDoomedBounce violated (3 legs, pre-fix volume-wide
#      suppression): a full teardown whose only purpose the stage
#      admission is guaranteed to refuse.  Checked on its OWN ghost, not
#      the shared churn canary — an A/B showed the canary fires with the
#      filter ON as well as OFF, so it cannot test this fix.
#   12f. FlintReplicationBounceStarve.cfg   THE BELT'S OWN LIVENESS, and the
#      one gap in this model a CODE review had to find instead: because
#      BouncePreflight is a GUARD, a blocked bounce is merely a disabled
#      action, and nothing here asked whether the remediation it blocks ever
#      happens.  WriterLimbo makes the missing world reachable (a flapping
#      node costs no failure budget, so a writer can stay neither answering
#      nor verifiably gone forever — under a budget it always resolves, which
#      is precisely why the model was blind).  TLC must FIND the
#      RemediationNotStarved lasso.
#   12g. FlintReplicationBounceBounded.cfg RefusalBounded=TRUE (the shipped
#      bound): strict, must HOLD — the remediation is never starved by its
#      own belt, and buying that liveness costs no safety.
#   12e. FlintReplicationBouncePlannerScoped.cfg the same with
#      SuppressScoped=TRUE (the shipped per-leg marks) and the planner
#      still unfiltered — strict, must HOLD.  The wave-2 per-leg fix
#      already closed this door, so plan_cutover needs no suppression
#      filter: a tranche result that says DO NOT write the code.
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
# Raid-composition lifetime tranche (F62, found LIVE on runao 2026-07-30 in
# the very roll the F61 fix enabled).  The module had only `serving`, whose
# own comment admitted the conflation — "{} = down" folding together "the
# members left" and "the composition does not exist".  Those have different
# LIFETIMES: the lvols live for the life of the PV, the volume is staged for
# as long as a consumer wants it, and the raid lives exactly as long as ONE
# spdk-tgt process on the node hosting the consumer — the most-restarted
# component in the system.  Nothing else in the model had that lifetime, so
# nothing else could express the composition being gone while every leg is
# healthy, on disk, and recorded in_sync.  Three destroyers, and the
# discriminator is whether kubelet still believes the volume STAGED:
#   consumer pod deleted -> NodeUnstage -> bdev_raid_delete: staged clears,
#     so the next attach re-creates it.  PAIRED.
#   node destroyed -> consumer relocates -> NodeStage on the new host:
#     staged clears.  PAIRED (and the host is mobile).
#   csi-node tgt dies, node and consumer stay put: no RPC, staged untouched,
#     NodeStage never called again.  UNPAIRED — and that is all of F62.
#   13a. FlintReplicationRaidLifetime.cfg  RaidLifetimeArm=TRUE on the
#      F61-FIXED world — TLC must FIND Inv_PlannedRollNeverCausesOutage
#      violated in 4 states.  The single most uncomfortable run here: the
#      cfg it mutates is green and blessed the F61 fix.  F61's livelock was
#      LOAD-BEARING — the only thing keeping the un-implemented local half
#      from being exercised.
#   13b. FlintReplicationRaidLost.cfg      the same world, liveness — TLC
#      must FIND RaidEventuallyReassembled violated: Assemble is the only
#      creator and kubelet stages only what it believes unstaged, so the
#      one creator is disabled while the one thing it creates is missing.
#      Nothing but a liveness claim could have caught this; the state trips
#      no safety invariant forever-after and every other property is
#      satisfied by a volume that is merely quiet.
#   13c. FlintReplicationRaidFenceAB.cfg   the A/B bug side — TLC must FIND
#      Inv_MaintFenceStrict violated.  Exists because the first draft of
#      that invariant was conditioned on the very arm it evaluates, making
#      it vacuous on the bug side: a tooth that could never fail.
#   13d. FlintReplicationRaidRefuse.cfg    fix B strict — refuse the
#      local-consumer node and SURFACE it (maintSkipped).  Prevention, not
#      repair; the campaign still converges; and the fence recovers FULL
#      strength, with no LocalLegs carve-out.
#   13e. FlintReplicationRaidReconcile.cfg repair A2 strict — the agent
#      re-creates the composition on boot from the record.  NOT a
#      superblock: flint passes "superblock": false on purpose (the
#      phantom-assembly class, and the 1 MiB payload shift that silently
#      formatted restored snapshots on 2026-06-12).  Deliberately does NOT
#      carry the outage invariant — a repair cannot prevent the outage, and
#      that asymmetry is why fix B is needed as well.
#   13f/g. FlintReplicationRaidSeenBlind/Fixed.cfg  the trigger A/B.  The
#      repair path is not missing, it is UNREACHABLE: CollapseEvent::Lost
#      needs data_path_raid_seen.contains(pv) and that HashSet dies with
#      the process that took the composition.  Rehydrate it from the
#      STAGED set — not from live SPDK, which reads empty in exactly the
#      situation that matters — and the shipped bounce-and-restage chain
#      suffices.  That is what makes A1 the first thing to code.
#   13h. FlintReplicationA2Naive.cfg  THE A2 TRANCHE.  A2 modelled as a
#      MECHANISM instead of as a relaxed guard on somebody else's action.
#      Through 13e this arm did exactly one thing — drop Assemble's ~staged
#      conjunct — which answers "would a repair of this SHAPE restore the
#      volume?" and was then cited as "A2 is modelled green".  It never was:
#      A2's hazard is a SECOND CREATOR, and while raidHost was a SCALAR the
#      state "two compositions exist" could not be written down, so TLC was
#      structurally unable to refute the one property A2 is most likely to
#      break.  Third instance of the pod layer's lesson — the abstraction
#      was the bug — and the creators here are NodeStage and the agent's own
#      boot pass.  raidHosts is now a SET; every other creator assigns a
#      singleton, so a cardinality violation is attributable to A2.
#      TLC must FIND, in 4 states, A2 assembling on a node the consumer has
#      LEFT: the VA still names it (VaCanLag), nothing is assembled there,
#      so the guard the implementation actually has is satisfied.  Not an
#      invented pessimism — node_agent.rs:3219 gives the ublk reaper's
#      reason for existing as cleaning up "the local disk a STALE VA made us
#      rebuild after the consumer moved away".  On that single-replica path
#      staleness leaks a disk and the reaper takes it; over a raid on shared
#      lvols the same trigger is a SECOND WRITER, and no reaper undoes a
#      write.
#   13i. FlintReplicationA2SoleOwner.cfg  the belt everyone reaches for
#      first, REFUTED.  "Refuse to assemble if any other host already holds
#      one" is check-then-act: A2 defeats it by going FIRST, and the
#      legitimate NodeStage — which has no such belt and should not need one
#      — supplies the second host afterwards.  TLC must still FIND the
#      violation.  Kept as a run so the refutation is standing evidence
#      rather than a paragraph of reasoning in a design note.
#   13j. FlintReplicationA2Staging.cfg  ⚠️ RELABELLED 2026-07-30 — NOT a
#      test of A2.  It was headed THE DELIVERABLE and cited for the
#      local-staging belt.  AgentBootReconcile fires ZERO times in it, over
#      the COMPLETE state graph (7,375,538 generated / 1,261,953 distinct /
#      0 left on queue).  Verified directly: ProbeA2Fires == a2Created = {}
#      HOLDS, and a2Created has exactly one writer.
#        WHY: A2 answers a CLASS-3 destroyer, which by definition leaves
#      kubelet's `staged` belief SET, and the belt requires exactly that.
#      This cfg pins UncontrolledTgtDeath = FALSE, so the only reachable
#      destroyers are the ones that CLEAR staging.  Belt precondition and
#      available destroyers were mutually exclusive: unsatisfiable by
#      construction, not merely unexercised.
#        HOW IT GOT PAST A PROBE THAT EXISTED FOR THIS: the old probe
#      (ProbeA2WouldHaveFired) tests whether the NAIVE guard becomes
#      satisfiable, i.e. whether the SITUATION arises.  It does not test
#      whether the BELTED ACTION fires.  Those differ exactly where it
#      matters, because the naive guard wants vaNode # VaTruth and the belt
#      refuses precisely that.  A non-vacuity probe must name the ACTION.
#        What this run still honestly proves: the four roll theorems hold
#      under a roller with the A2 constant set.  A statement about the
#      ROLLER, not about A2.
#   13j'. FlintReplicationA2Armed.cfg + FlintA2ProbeArmed.cfg  THE PAIR THAT
#      REPLACES IT.  One constant differs (UncontrolledTgtDeath = TRUE) and
#      MaintEnabled = FALSE, which is the SHIPPED DEFAULT path — OnDelete is
#      emitted only inside the drainRoll conditional, default false — and
#      which makes the roll theorems vacuous rather than suppressed.  The
#      belt HOLDS over the complete graph and the probe VIOLATES in three
#      states (Init -> TgtDie -> AgentBootReconcile).
#        THE STANDING RULE: no cfg may claim to exercise A2 without a paired
#      ProbeA2Fires run the gate requires TLC to VIOLATE.  A green safety
#      run and its non-vacuity probe prove nothing apart.
#   13k-m. FlintReplicationUncontrolled{Blind,A1,A2}.cfg  THE DESTROYER
#      NOBODY CAN REFUSE.  Until now class-3 destruction lived ONLY inside
#      RollStart, gated on the roller's own arms — so every F62/F63 run was
#      asking "can flint's roller destroy a composition?" about a feature
#      that is OFF BY DEFAULT and which, in that default configuration,
#      declines to act at all (plan_roll returns Blocked when !on_delete,
#      maint_roll.rs:248).  The chart emits updateStrategy: OnDelete only
#      inside the maintenance.drainRoll.enabled conditional
#      (node.yaml:13-24), so the shipped DaemonSet takes k8s's RollingUpdate
#      default and a routine `helm upgrade` rolls every node pod, killing
#      tgts under live consumers — with OOM kills, kubelet restarts,
#      evictions, node-image upgrades and GitOps syncs arriving by the same
#      door.  Fixes B and B' cannot reach ANY of it.
#      Blind: green, but SCOPED — with the collapse-event detector as the
#      ONLY trigger nothing recovers. NOT a statement about the shipped
#      world: the layer-2 repair needs no seeded state, and
#      UncontrolledStrike.cfg shows it recovering with A1 off, A2 off and no
#      bounces. (non-vacuity: FlintA2ProbeDeath.cfg must VIOLATE
#      ProbeTgtDeathReachable, so the death really happens).
#      A1: must FIND the recovery. A1 has ALREADY SHIPPED; it makes the
#      COLLAPSE-EVENT path reachable, which is a second independent trigger
#      alongside the strike-based repair — not, as first written here, the
#      only one.
#      A2: must FIND the recovery with MaxBounces=0 — no bounce, no pod
#      delete, no unstage, so the consumer is never disrupted, and the cost
#      is one tgt restart per NODE regardless of how many consumers that
#      node hosts.  Contrast B', whose cost is one relocation per CONSUMER
#      POD, with re-relocation as the campaign advances.
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

# Per-run wall-clock profile. The gate's cost grows with every tranche and
# the cost is NOT evenly spread — a couple of runs dominate. Recorded for
# every run whether it passes or fails (a failing run's cost matters too),
# and summarised at the end, sorted. Override the location with
# TLA_PROFILE_FILE; set TLA_PROFILE=0 to skip the summary.
PROFILE_FILE=${TLA_PROFILE_FILE:-$(mktemp -t tlaprofile)}
export PROFILE_FILE
: > "$PROFILE_FILE"

run_tlc() { # <module> <cfg>
  # Per-cfg -metadir: TLC's default scratch dir is named by wall-clock
  # second, so two fast runs starting within the same second collide and
  # the later one aborts before checking anything.
  local t0 rc=0
  t0=$(date +%s)
  java -XX:+UseParallelGC -cp "$JAR" tlc2.TLC -workers auto \
    -metadir "states/${2%.cfg}" -config "$2" "$1.tla" 2>&1 || rc=$?
  # stdout is CAPTURED by the callers, so the timing goes to a file, never
  # to stdout — printing it here would corrupt every pass/fail match below.
  printf '%s\t%s\n' "$(( $(date +%s) - t0 ))" "$2" >> "$PROFILE_FILE"
  return $rc
}

# Called on the way out (success or failure) so a red gate still profiles.
print_profile() {
  [ "${TLA_PROFILE:-1}" = "0" ] && return 0
  [ -s "$PROFILE_FILE" ] || return 0
  local total n
  total=$(awk -F'\t' '{s+=$1} END{print s+0}' "$PROFILE_FILE")
  n=$(wc -l < "$PROFILE_FILE" | tr -d ' ')
  echo ""
  echo "── gate profile: ${n} runs, ${total}s of TLC (wall clock, sequential) ──"
  sort -rn -k1,1 "$PROFILE_FILE" | awk -F'\t' -v tot="$total" '
    { c++ }
    c<=12 { printf "  %6ds  %5.1f%%  %s\n", $1, ($1/tot)*100, $2; shown+=$1 }
    END { if (c>12) printf "  (%d further runs, %ds total)\n", c-12, tot-shown }'
  # Cheap-run tail: how much of the gate is runs that cost ~nothing.
  awk -F'\t' '$1<=2 {c++; s+=$1} END{ if (c) printf "  %d runs at <=2s (%ds total) — the cheap tail\n", c, s }' "$PROFILE_FILE"
}
trap print_profile EXIT

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

# F61, found LIVE on runao 2026-07-30 (drill 3.14's first ever run) because no
# property in this module could fail on it: a wedged roll leaves the volume
# perfectly healthy, so the four maintenance properties all held — and
# MaintenanceEventuallyLifts held VACUOUSLY, since the wedge never mints a
# mark. The fairness comment declined "the campaign completes" as a theorem
# on the grounds that STARTING a drain is operator-paced; correct about
# pacing, but it also erased any way to tell "not started" from "cannot
# finish". RollProcessedNodeRolls obligates the ROLLER instead.
liveness_mutation_run FlintReplication FlintReplicationRollWedge.cfg "roll-progress mutation (MaintProcessedGate=FALSE: the shipped predicate gates the pod delete on a MARK, so a node whose drain legitimately marks nothing — the local half, unattached, or no legs — can never be rolled: F61's livelock)"
strict_run FlintReplication FlintReplicationRollProcessed.cfg "roll-progress strict (the F61 fix: eligibility = the drain PASS ran AND the leg is out of the raid, or is a local-half leg with a survivor behind it)"

# ---------------------------------------------------------------------------
# F62 — the raid-composition lifetime tranche.  Read the first run's label
# twice: the world it mutates is not the shipped code, it is the code AS
# FIXED BY F61.  The composition became an object with its own lifetime, and
# that alone turned the run directly above (green, and the one that blessed
# the F61 fix) into a permanent outage.  F61's bug was load-bearing.
# ---------------------------------------------------------------------------
mutation_run FlintReplication FlintReplicationRaidLifetime.cfg "raid-lifetime mutation (F61-fixed code + RaidLifetimeArm: the roll deletes the pod hosting the composition, the tgt dies with it, and zero real failures produce a permanent outage with both legs healthy and recorded in_sync)" "Inv_PlannedRollNeverCausesOutage"
liveness_mutation_run FlintReplication FlintReplicationRaidLost.cfg "raid-lifetime liveness (nothing re-creates it: Assemble is the only creator, kubelet stages only what it believes UNSTAGED, and a tgt death clears nothing)"
mutation_run FlintReplication FlintReplicationRaidFenceAB.cfg "strict-fence A/B, bug side (the post-F61 roller restarts a local half's tgt while it serves — why Inv_MaintFenceHolds needs its LocalLegs carve-out)" "Inv_MaintFenceStrict"
strict_run FlintReplication FlintReplicationRaidRefuse.cfg "F62 fix B strict (refuse + surface the local-consumer node: the outage is PREVENTED not repaired, the campaign still converges, and the fence recovers full strength with no carve-out)"
strict_run FlintReplication FlintReplicationRaidReconcile.cfg "F62 repair A2 with an INSTANTANEOUS attacher (VaCanLag=FALSE — the world where A2 looks safe, kept as the diagnostic that attributes the A2 tranche's hazard to the attacher's LAG and not to A2 itself; establishes recovery only, never safety)"

# The trigger half, A/B.  The repair path is not missing, it is UNREACHABLE:
# CollapseEvent::Lost needs data_path_raid_seen.contains(pv), and that HashSet
# dies with the same process that takes the composition.
liveness_mutation_run FlintReplication FlintReplicationRaidSeenBlind.cfg "F62a trigger mutation (shipped detector: whole repair chain armed and willing, blinded by one un-rehydrated HashSet)"
strict_run FlintReplication FlintReplicationRaidSeenFixed.cfg "F62a repair A1 strict (rehydrate data_path_raid_seen from the STAGED set — not from live SPDK, which reads empty exactly when it matters — and the shipped bounce-and-restage chain suffices)"

# ---------------------------------------------------------------------------
# Consumer mobility (2026-07-29) — forced by the runap live gate, not by the
# model.  The F62 tranche held LocalLegs CONSTANT, so a refused node was
# refused in every reachable state and "refuses forever" was
# indistinguishable from "re-examines every tick".  The shipped code does the
# second; the model only demanded the first.  A model WEAKER than its
# implementation is the direction that lets a regression land green.
#
# Making the consumer mobile also found F63 — a hole in fix B's own fix: fix B
# filtered the pending-SELECTION path and left the marked-node COMPLETION path
# untouched, so a consumer relocating in the one-tick window between a drain
# and its pod delete got the pod deleted anyway, destroying the composition.
# ---------------------------------------------------------------------------
mutation_run FlintReplication FlintReplicationRefusalClears.cfg "refusal-clears reachability (shipped gate reads the LIVE condition: TLC must FIND a node that was refused, whose consumer then left, and which then ROLLED — the 14s behaviour measured on runap)" "Inv_RefusalNeverClears"
strict_run FlintReplication FlintReplicationRefusalSticky.cfg "refusal-sticky A/B, bug side (gate reads the REMEMBERED maintSkipped set: the refusal is permanent, the node keeps an old driver forever, and Inv_RefusalNeverClears HOLDS — that green is the bug)"
strict_run FlintReplication FlintReplicationMobileStrict.cfg "mobile-consumer strict (the roller acts DURING a relocation — the unattached window I had only ASSERTED was harmless; full state space, every roll invariant armed, and the roll-while-relocating state verified REACHABLE so the green is not vacuous)"

# ---------------------------------------------------------------------------
# THE A2 TRANCHE (2026-07-29).  Two findings, and the first one is about this
# file rather than about the code.
#
# 1. `raidHost` was a SCALAR for the whole F62/F63 cycle, so "two compositions
#    exist" was UNREPRESENTABLE, so FlintReplicationRaidReconcile.cfg went
#    green against A2 on a hazard it was structurally unable to see — and that
#    green was cited as "A2 is modelled".  The pod-layer tranche's lesson for
#    the third time: THE ABSTRACTION WAS THE BUG, two independent creators of
#    one object.  Here they are NodeStage and the agent's own boot pass.
#
# 2. Class-3 destruction existed ONLY inside RollStart, an action gated on the
#    roller's arms.  So every F62/F63 run asked "can flint's roller destroy a
#    composition?" — about a feature that is OFF BY DEFAULT and which, in that
#    default configuration, refuses to act at all (plan_roll returns Blocked
#    when !on_delete).  The chart emits updateStrategy: OnDelete only inside
#    the drainRoll.enabled conditional, so the shipped DaemonSet takes k8s's
#    RollingUpdate default: a routine `helm upgrade` rolls every node pod and
#    kills tgts under live consumers, with OOM kills, kubelet restarts and
#    evictions arriving by the same door.  Fixes B and B' cannot reach ANY of
#    it.  UncontrolledTgtDeath makes that path expressible.
# ---------------------------------------------------------------------------
mutation_run FlintReplication FlintReplicationA2Naive.cfg "A2 naive (the only input the implementation has is the VA, and node_agent.rs:3219 documents the ublk reaper's reason for existing as cleaning up what a STALE VA made it rebuild — here the same staleness assembles a raid on a node the consumer has left, in 4 states)" "Inv_A2AssemblesOnlyAtTruth"
mutation_run FlintReplication FlintReplicationA2SoleOwner.cfg "A2 + sole-ownership belt (the belt everyone reaches for first, REFUTED: it is check-then-act, so A2 defeats it by going FIRST and the legitimate NodeStage supplies the second host afterwards)" "Inv_A2AssemblesOnlyAtTruth"
strict_run FlintReplication FlintReplicationA2Staging.cfg "the roll theorems with the A2 arm set — NOT a test of A2 (RELABELLED 2026-07-30: this run was headed THE DELIVERABLE and cited for the local-staging belt, but AgentBootReconcile fires ZERO times in it over the complete 1,261,953-state graph — UncontrolledTgtDeath=FALSE leaves only destroyers that CLEAR staging, while the belt requires staging SET, so the guard is unsatisfiable by construction. What it still honestly proves is that the four roll theorems hold under a roller with the A2 constant set)"
strict_run FlintReplication FlintReplicationA2Armed.cfg "A2 + local-staging belt strict, ACTUALLY EXERCISED — the run A2Staging was supposed to be (UncontrolledTgtDeath=TRUE arms the class-3 death the belt exists to admit; MaintEnabled=FALSE because the SHIPPED DEFAULT path has no roller, which also makes the roll theorems vacuous rather than suppressed. Safety only: the two liveness properties are dropped, one vacuous without a roller and temporal checking being ~94% of the flagship run's cost)"
mutation_run FlintA2Probe FlintA2ProbeArmed.cfg "A2-fires non-vacuity probe for A2Armed (a2Created has exactly ONE writer, so a violation is a witness that AgentBootReconcile really executes: Init -> TgtDie -> AgentBootReconcile. THE STANDING RULE — no cfg may claim to exercise A2 without this pairing, because a green safety run and its non-vacuity probe prove nothing apart)" "ProbeA2Fires"

# The destroyer nobody can refuse, A/B/C.  The green run is the indictment.
#
# THE PROBE RUNS FIRST, and it did not run at all until 2026-07-31: the
# Blind green below CITES FlintA2ProbeDeath.cfg as its non-vacuity licence
# (and so does the catalogue at the top of this file), but no invocation
# ever executed it.  A green whose licence is a run the gate does not
# perform is exactly the thing FlintA2Probe.tla:24-25 declares a standing
# rule against — asserted inside the gate that declares it.
mutation_run FlintA2Probe FlintA2ProbeDeath.cfg "tgt-death reachability probe for UncontrolledBlind (raidLostOnce has one writer, so a violation witnesses the class-3 death actually occurring; without it the Blind green could mean 'nothing ever died' rather than 'nothing could recover')" "ProbeTgtDeathReachable"

strict_run FlintReplication FlintReplicationUncontrolledBlind.cfg "uncontrolled tgt death, UNREPAIRED (a routine helm upgrade in the DEFAULT configuration; Inv_RaidRecoveryUnreachable HOLDS = the volume can never come back, and the tgt death is verified REACHABLE via FlintA2ProbeDeath.cfg so the green is a real permanent outage and not a vacuous one)"
mutation_run FlintReplication FlintReplicationUncontrolledA1.cfg "uncontrolled tgt death repaired by A1 (ALREADY SHIPPED, and in the default configuration the only thing standing between a routine helm upgrade and a permanent outage — fixes B and B' cannot reach this path)" "Inv_RaidRecoveryUnreachable"
mutation_run FlintReplication FlintReplicationUncontrolledA2.cfg "uncontrolled tgt death recovered with the A2 arm set — ⚠️ CONFOUNDED A/B, DO NOT CITE AS 'A2 RECOVERS' (2026-07-30: the required violation is produced by Assemble, not AgentBootReconcile — trace Init -> TgtDie -> Assemble. FlintReplication.tla:1633 is (RaidLifetimeArm => (~staged \\/ RaidReconcileArm)), so the A2 constant ALSO relaxes ordinary NodeStage on a still-staged volume and the A/B against UncontrolledBlind moves two things at once. The credited recoverer is the stronger, UNBELTED one. De-confounding this needs a separate constant for the staged-reassemble relaxation — owed, tracked in the A2 doc)" "Inv_RaidRecoveryUnreachable"
mutation_run FlintReplication FlintReplicationUncontrolledStrike.cfg "the SHIPPED periodic repair suffices — the run that corrected this tranche (A1 off, A2 off, zero bounces: detect_lost_data_paths -> repair_data_path needs NO seeded state, so UncontrolledBlind's green scopes to the COLLAPSE-DETECTOR path only and never to the shipped world; note repair_data_path already carries the is_staged_here belt this tranche derived for A2 from first principles)" "Inv_RaidRecoveryUnreachable"

# ---------------------------------------------------------------------------
# THE ADOPT (2026-07-29, added on the question "does the fix cause flapping?").
# `ensure_raid1_bdev` (driver.rs:3105) REUSES any raid of this name in state
# "online" without comparing its base set to the one NodeStage intended. That
# is correct today — NodeStage is the only creator, so the object it finds is
# one it built from the same PV replica record — and becomes a hazard the
# moment A2 is a second creator. Three runs decide the code:
#   blind      -> the adopt happens (NodeStage inherits A2's object whole)
#   validated  -> the adopt is closed and the two creators FIGHT: A2 rebuilds
#                 what validation deleted, NodeStage's own create then hits
#                 EEXIST and its 3-attempt retry gives up — "RAID bdev did not
#                 converge after 3 attempts (phantom kept re-appearing)", a
#                 string the code already has. A remedy that DELETES the other
#                 creator's object can be undone by that creator.
#   belted     -> the local-staging belt closes it by REFUSING instead, so
#                 there is no object to fight over and no cycle to enter.
# Verdict: the ensure_raid1_bdev change is defence in depth, NOT a prerequisite.
# ---------------------------------------------------------------------------
mutation_run FlintReplication FlintReplicationA2AdoptBlind.cfg "A2 adopt, unguarded (NodeStage short-circuits onto A2's composition and inherits serving/writerSet/lineage whole — members chosen by a boot-time snapshot taken while the volume was somebody else's; the F44/F46 adopt-or-mint family by a new road)" "Inv_NoAdoptOfA2Composition"
mutation_run FlintReplication FlintReplicationA2AdoptValidated.cfg "the VALIDATING fix FLAPS (its remedy is to DELETE the other creator's object, so A2 puts it back and NodeStage's create hits EEXIST — stated as an ORDER not a count, because the count form was answered with build/build/delete and proved nothing)" "Inv_NoValidateFlap"
strict_run FlintReplication FlintReplicationA2AdoptBelted.cfg "A2 adopt + local-staging belt strict, ARM SET BUT UNREACHED (RELABELLED 2026-07-30: same single cause as A2Staging — UncontrolledTgtDeath=FALSE, so over the complete graph NEITHER AgentBootReconcile NOR AssembleAdopt fires and both adopt theorems were green on a state space with no A2 composition to adopt)"
strict_run FlintReplication FlintReplicationA2AdoptArmed.cfg "A2 adopt + local-staging belt strict, ACTUALLY EXERCISED (class-3 death armed; the belt closes the adopt by REFUSING rather than deleting, so it cannot buy safety at the price of a create/delete loop — this is the run that had to show that, and now does)"
mutation_run FlintA2Probe FlintA2ProbeAdoptArmed.cfg "A2-fires non-vacuity probe for A2AdoptArmed (NECESSARY, NOT SUFFICIENT and deliberately recorded as such: it witnesses A2 firing, but a dedicated AssembleAdopt-fired probe needs a ghost this module lacks — adoptedA2 cannot serve, being false both when the adopt never runs and when it runs on a non-A2 composition, so probing it would be probing Inv_NoAdoptOfA2Composition itself. The adopt theorems are PARTIALLY gated until that ghost exists)" "ProbeA2Fires"

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

# Cutover pod layer (the double-creator race: two independent creators of
# one bare pod, no mutual exclusion anywhere).
mutation_run FlintReplication FlintReplicationBouncePod.cfg "double-creator race (the liveness reconciler recreates the server INSIDE the detach wait — the bounce is silently defeated, standby stays parked)" "Inv_BounceNotSilentlyDefeated"

strict_run FlintReplication FlintReplicationBouncePodFixed.cfg "single-creator strict (ReconcilerBelt: no recreate while a bounce window is open; the volume still converges)"

mutation_run FlintReplication FlintReplicationBounceTimeout.cfg "detach-timeout door (the bouncer defeats its OWN wait: await_detached only warns — so the reconciler fix alone is insufficient)" "Inv_BounceNotSilentlyDefeated"

mutation_run FlintReplication FlintReplicationBouncePlanner.cfg "two-planner disjointness, pre-fix world (plan_cutover honours none of plan_hot_rejoin's filters — a teardown whose purpose the stage admission will refuse)" "Inv_NoDoomedBounce"

strict_run FlintReplication FlintReplicationBouncePlannerScoped.cfg "two-planner disjointness, SHIPPED world (per-leg suppression already closes it — the planner filter is NOT owed)"

# The belt's own liveness — the gap a CODE review exposed that the model could
# not express (WriterLimbo makes indefinite limbo reachable).
liveness_mutation_run FlintReplication FlintReplicationBounceStarve.cfg "unbounded-belt starvation (RefusalBounded=FALSE: a flapping writer never becomes honestly excusable, so the safety belt starves the terminal remediation forever)"

strict_run FlintReplication FlintReplicationBounceBounded.cfg "bounded-refusal strict (the shipped bound: the remediation is never starved by its own belt, at no cost in safety)"

strict_run FlintClaims FlintClaims.cfg "claims strict (two processes, Lease + marker grace; F50/F53 layer)"

# ---- the kube-DS tooth (2026-07-30). THE ANTI-CROSS-NODE GUARD. ----
# Not a claim that a readinessProbe is safe — a claim about WHICH predicate
# it may compute. The design's whole safety rests on "no cross-node term,
# ever", which was a comment a reviewer had to remember. These four make it
# MECHANICAL: any future predicate that reddens a LIVE pod fails the gate.
#
# The pair that matters is the last one. "selfLive" is self-scoped and
# carries no cross-node term — it is simply not LATCHING — i.e. it is the
# predicate a careful reviewer would write believing they had followed the
# design. It fails in 51 states.
#
# SCOPE LIMIT, STATED SO THIS IS NOT OVER-CITED: all four pin
# UncontrolledTgtDeath = FALSE. So these runs say the LATCHING predicate never
# reddens a live pod; they say NOTHING about what the latch does when the data
# path dies UNDER it after latching. It cannot re-redden — a separate
# adversarial run found the stale green in 1,496 states — which is craft rule 8
# and a TRUE property of the proposed probe, not a modelling artifact. Arming
# UncontrolledTgtDeath here would NOT settle it either: TgtDie conflates process
# death with composition destruction, so that green would be void the way the
# scalar-raidHost green was void. The honest claim is about WHICH PREDICATE may
# be computed, never that a readinessProbe is safe to ship.
#
# Lean by construction: RaidLifetimeArm/UncontrolledTgtDeath FALSE and two
# legs, because the claim is about ProbeEval alone. Carrying the raid
# lifetime pushed one arm past 18M states while contributing nothing to it —
# a cfg should carry the arms its claim needs and no more.
strict_run   FlintReplication FlintReplicationKubeToothSocket.cfg  "kube-DS tooth: shipped socket probe never reddens a live pod"
strict_run   FlintReplication FlintReplicationKubeToothLatched.cfg "kube-DS tooth: the LATCHING self-scoped predicate never reddens a live pod"
mutation_run FlintReplication FlintReplicationKubeToothVolume.cfg   "kube-DS tooth mutation (ReadyScope=volume: a CROSS-NODE term)"      "Inv_ProbeNeverReddensLive"
mutation_run FlintReplication FlintReplicationKubeToothSelfLive.cfg "kube-DS tooth mutation (ReadyScope=selfLive: self-scoped but NOT latching)" "Inv_ProbeNeverReddensLive"

mutation_run FlintClaims FlintClaimsNoGrace.cfg "F50 mutation (MarkerGrace=FALSE: scrub under a live window, cold admission)" "Inv_NoColdAdmission"

strict_run FlintClaims FlintClaimsNoLeader.cfg "no-leader strict (F53 world: safety never depended on the singleton)"

strict_run FlintSnapshots FlintSnapshots.cfg "snapshots strict (full ordered walk, blobstore relink)"

mutation_run FlintSnapshots FlintSnapshotsSplit.cfg      "delta-split mutation (WalkFull=FALSE)"       "Inv_SessionFaithful"
mutation_run FlintSnapshots FlintSnapshotsOrder.cfg      "walk-order mutation (OrderedWalk=FALSE)"     "Inv_SessionFaithful"
mutation_run FlintSnapshots FlintSnapshotsBareDelete.cfg "bare-delete mutation (RelinkOnDelete=FALSE)" "Inv_SessionFaithful"

# ---- the pNFS truncate gate (2026-07-31). ----
# The one correctness invariant the pNFS layer holds in its OWN hands: the
# window between the MDS stub's size changing and N data servers being cut.
#
# READ THIS BEFORE CITING ANY RUN BELOW. The gate's own theorem holds. The
# theorem that matters to a user — no client is ever SERVED content past the
# MDS size — still does NOT hold on shipped code, and the shipped cfg
# deliberately does not list it. Audit C1/C2/C3 (malformed recall, scored as
# an ack) and C6 (non-atomic grant) are FIXED; what remains is that
# server-side revocation cannot bind a client the recall never reached. C5 is
# fixed, so a session's other bound transports are now tried, but a client with
# no live back-channel at all is still unreachable — that needs the DS to
# refuse, not the MDS to ask. See
# spdk-csi-driver/docs/f65-truncate-does-not-recall-held-layouts.md.
strict_run   FlintTruncate FlintTruncate.cfg               "truncate gate strict, SHIPPED — Theorem 1 only (Inv_NoStaleServe is NOT claimed; see the cfg)"
mutation_run FlintTruncate FlintTruncateBlindClear.cfg     "blind-clear mutation (GateClearGuarded=FALSE: a shallower confirm lifts a deeper cut's mark)" "Inv_ClearImpliesFlushed"

# The REFUTED half. Keeping the min looks like the other load-bearing piece and
# is not: overwriting only ever RAISES the mark, and the mark can only rise on a
# SETATTR that also raised the size, so the exposure is unreachable. Kept as a
# run so the claim cannot be quietly re-asserted later.
strict_run   FlintTruncate FlintTruncateMarkOverwrite.cfg  "min-keeping refutation (MarkKeepsMin=FALSE still holds — safety is carried by clear_truncate_dirty_if alone)"

# Three INDEPENDENT ways Inv_NoStaleServe is lost. Each isolates one cause, so
# fixing one and watching its run go green is a real signal rather than a
# guess. All three must keep FAILING until their cause is repaired.
mutation_run FlintTruncate FlintTruncateHeldLayout.cfg     "F65 itself (RecallOnTruncate=FALSE: no recall at all, so the gate covers only clients without a layout yet)" "Inv_NoStaleServe"
mutation_run FlintTruncate FlintTruncateLostRecall.cfg     "unreached-recall residual (RecallReaches=FALSE: server-side revocation does not bind a client the callback never reached; C5 is fixed so a session's other transports are tried, but a client with NO live back-channel still cannot be bound)" "Inv_NoStaleServe"
mutation_run FlintTruncate FlintTruncateGrantRace.cfg      "audit C6 REGRESSION (PublishRecheck=FALSE: without the publish-time recheck a grant escapes both the gate and the recall snapshot)" "Inv_NoStaleServe"

# The TARGET state, not the current one. What closing F65 requires, stated as a
# green so the remaining work has a specification. Cite it as a goal; citing it
# as a property of shipped code would be exactly the mistake the audit caught.
strict_run   FlintTruncate FlintTruncateNoStaleServe.cfg   "truncate target state (recall fires AND is honoured AND the publish rechecks) — Inv_NoStaleServe holds"

# ---- the block-layout extent allocator, tranche 1 (2026-08-09). ----
# docs/plans/pnfs-block-layout-design.md §9. Models code that DOES NOT EXIST
# YET — the module and its runs come first, and the allocator must be
# implemented against them. The theorem is the WRITE: a block-layout client
# holds raw LBAs and writes them with the MDS nowhere on the path, so a stale
# holder after reuse corrupts the NEW owner's bytes. Both stale theorems are
# deliberately absent from the shipped cfg (FenceReaches=FALSE until spdk-tgt
# reservation enforcement is proven on real hardware — LostFence is the run
# that keeps that honest), exactly the FlintTruncate.cfg pattern.
#
# TRANCHE-1 FINDING, made before any code: the design's grant-side belts
# (PublishRecheck / RecallBlocksGrant) CANNOT close the grant-vs-reclaim
# races — a grant published after the reclaim's holder snapshot escapes it,
# and the reclaim can complete-and-free between that grant's insert and any
# recheck. Nothing at grant-time can see a freed block either (the free
# destroys the very rows the transaction validates against). Safety belongs
# to the FREE side: the free transaction re-validates the grants table
# (FreeRevalidates). StaleSnapshotFree pins the refuted sketch permanently,
# MarkOverwrite-style; the grant-side arms return in a liveness tranche as
# progress belts.
strict_run   FlintExtents FlintExtents.cfg "extents shipped-design strict (all belts ON incl. FreeRequiresDelivered and the merge arm — both stale theorems claimed in the fences-CAN-fail world with quiescent-merge gen coarsening live; FenceReaches stays FALSE because the code's preempt arm is best-effort)"
mutation_run FlintExtents FlintExtentsMergeHeld.cfg "merge-quiescence mutation (MergeChecksHolders=FALSE: the merge coarsens gen under a live unfenced grant — the exact clause of Inv_RecallCompletesBeforeReuse, and why the code's merge window demands ZERO grant rows on every row it swallows)" "Inv_RecallCompletesBeforeReuse"
strict_run   FlintExtents FlintExtentsMergeMin.cfg "merge gen-choice A/B (MergeTakesMin=TRUE, full theorem stack: GREEN = the machine-checked REDUNDANCY verdict — under quiescence + the shipped belts the merged-gen choice is safety-irrelevant at block granularity; the code keeps MAX as free-list monotonicity hygiene, a cross-incarnation property this representation cannot state)"
mutation_run FlintExtents FlintExtentsReuseUnderGrant.cfg "reuse-under-grant mutation (RecallBeforeReuse=FALSE: the F65-of-extents — freed under a live grant, re-granted, and the old holder's raw NVMe write lands in the new owner's bytes)" "Inv_NoStaleExtentWrite"
mutation_run FlintExtents FlintExtentsGrantOverlap.cfg "grant-overlap mutation (GrantsExclusive=FALSE: §8's PK-does-not-police-overlap landmine — two two-step windows race to one free block and both publish)" "Inv_NoConflictingGrants"
mutation_run FlintExtents FlintExtentsStaleSnapshotFree.cfg "stale-snapshot-free mutation (FreeRevalidates=FALSE: THE TRANCHE-1 FINDING — the doc's original grant-side-belts-only design, refuted and pinned; the free trusts the reclaim's start-time holder snapshot and frees under a grant the snapshot never saw)" "Inv_RecallCompletesBeforeReuse"
mutation_run FlintExtents FlintExtentsLostFence.cfg "lost-fence A/B (FreeRequiresDelivered=FALSE, otherwise the shipped constants: a fence that silently fails to land while the MDS frees anyway — the single-flag mutation licensing the shipped cfg's graduated theorems, pinned permanently)" "Inv_NoStaleExtentWrite"
mutation_run FlintExtents FlintExtentsTgtAmnesia.cfg "tgt-amnesia mutation (PersistReservations=FALSE with FenceReaches=TRUE: §5's 'PTPL is mandatory' with teeth — the fence lands, spdk-tgt restarts, the exclusion dies with the process, single-flag attributable)" "Inv_NoStaleExtentWrite"
strict_run   FlintExtents FlintExtentsTarget.cfg "extents TARGET state (FenceReaches + PTPL on top of the shipped belts: both stale theorems hold) — cite as a goal ONLY; as a property of shipped code it would be exactly the mistake the F65 audit caught"

# Non-vacuity probes (the A2Probe standing rule: no strict green is citable
# without a paired probe TLC must VIOLATE, and a probe names the ACTION via a
# single-writer ghost, never the situation).
mutation_run FlintExtentsProbe FlintExtentsProbeReuse.cfg "extents reuse probe (GrantInsert really re-cycles a previously-owned block in the shipped state space — without this the strict green could mean nothing was ever reused)" "ProbeReuseFires"
mutation_run FlintExtentsProbe FlintExtentsProbeFence.cfg "extents fence probe (Fence really executes — a state space where every holder politely returns never exercises the machinery the module exists for)" "ProbeFenceFires"
mutation_run FlintExtentsProbe FlintExtentsProbeRestart.cfg "extents tgt-restart probe (TgtRestart really occurs in the shipped world, so the strict green holds ACROSS a restart and TgtAmnesia is an A/B over one flag, not over reachability)" "ProbeTgtRestarts"
mutation_run FlintExtentsProbe FlintExtentsProbeResnapshot.cfg "extents resnapshot probe (the retry re-read really finds holders the snapshot missed — the exact world FreeRevalidates refuses frees in; without this its green is a statement about a disabled guard)" "ProbeResnapshotGrows"
mutation_run FlintExtentsProbe FlintExtentsProbeDelivered.cfg "extents delivered-free probe (ReclaimComplete really frees blocks a fenced+CONFIRMED holder still held in the graduated shipped world — without this the shipped green could be the quarantine world wearing the flip's label)" "ProbeDeliveredFreeFires"
mutation_run FlintExtentsProbe FlintExtentsProbeMerge.cfg "extents merge probe (Merge really coarsens — unequal gens, quiescent blocks — in the shipped state space; without this the merge-armed green says nothing about merging)" "ProbeMergeFires"

# ---- extents tranche 2 (2026-08-09, same day): LAYOUTCOMMIT, size, scrub. ----
# Behind CommitEnabled=FALSE in every tranche-1 cfg — bit-identical by
# construction, VERIFIED by distinct-count match on the flagship (1,958,126
# before and after). Three more spec corrections, same species as tranche 1's
# (a harm the mechanism cannot produce, or a predicate legal behaviour
# violates): SizeCommitCoupled is TRANSACTIONAL (hole-filling makes the
# sketched state predicate false on legal behaviour); ForgedCommit violates
# its own theorem, never NoStaleExtentWrite (a commit writes no bytes);
# BlindCommit is renamed BlindProvision (the hazard is scrub-at-allocation —
# deleted-data resurrection, not cross-tenant: reuse is intra-volume).
# AND ONE SELF-CORRECTION: adding the committed state broke tranche 1's
# GrantInsert `occupied` predicate ("provisional /\ held" stopped seeing
# committed-and-held blocks — two live grants overlapped in 6 states); a
# predicate enumerating states goes stale the day the state set grows, so it
# now reads "not free /\ held". The Rust transaction had it right already.
strict_run   FlintExtents FlintExtentsCommit.cfg "extents commit strict (CommitEnabled + all three commit belts on the shipped base: size never advances without its range promotion, no invalid commit applies, no fresh provisional extent serves a prior incarnation's bytes)"
mutation_run FlintExtents FlintExtentsUngatedSize.cfg "half-stub mutation (CommitGatesSize=FALSE: the size half of LAYOUTCOMMIT lands while the range half refuses — F67's silent-zeros lineage, and exactly what shipping the size path before the extent path would produce)" "Inv_SizeCommitCoupled"
mutation_run FlintExtents FlintExtentsForgedCommit.cfg "forged-commit mutation (CommitChecksGen=FALSE: reservations fence only the NVMe data path, the NFS control path stays open, and an unvalidated LAYOUTCOMMIT promotes extents the committer no longer owns)" "Inv_NoForgedCommit"
mutation_run FlintExtents FlintExtentsBlindProvision.cfg "unscrubbed-provision mutation (ProvisionalInvisible=FALSE: a fresh INVALID extent on a reused range still carries the previous incarnation's bytes — deleted-data resurrection; the code belt is extent_alloc's needs_scrub contract)" "Inv_NoPriorOwnerDisclosure"
mutation_run FlintExtentsProbe FlintExtentsProbeCommit.cfg "extents commit probe (LayoutCommit really executes in the commit-strict world)" "ProbeCommitFires"
mutation_run FlintExtentsProbe FlintExtentsProbeCommitAfterReturn.cfg "commit-grace probe (a LAYOUTCOMMIT validated through graceG — the client returned its layout first, which is what Linux does)" "ProbeCommitAfterReturn"
strict_run   FlintExtentsProbe FlintExtentsNoCommitGrace.cfg "commit-grace A/B (CommitGraceEnabled=FALSE reproduces the shipped-until-2026-08-11 world: no behaviour can reach a commit-after-return, so the probe HOLDS — this is what makes the probe run above mean something)"
mutation_run FlintExtents FlintExtentsQuarantineBlindRelease.cfg "quarantine-sweep A/B (QuarantineChecksDelivered=FALSE: the sweep hands a PARKED range back to the allocator without re-checking that its remembered holders are confirmed-excluded — LostFence's corruption arriving by the other door, the free correctly refused at reclaim time taken later without the check)" "Inv_NoStaleExtentWrite"
mutation_run FlintExtents FlintExtentsQuarantineVisible.cfg "quarantine-isolation A/B (QuarantineIsolated=FALSE: the parked range keeps its extents row instead of moving to the third table, so it looks like an ORPHAN — allocated, no live holder — and the grant path re-hands it to a new owner at its old generation. THE ANSWER TO CONSTRAINT 5: the trace that was filed as an open hole in the two-step grant window was the model's abstraction, not the design's)" "Inv_RecallCompletesBeforeReuse"
mutation_run FlintExtentsProbe FlintExtentsProbeQuarantineRelease.cfg "quarantine-sweep probe (a PARKED range really is released once its exclusion is confirmed — without this the shipped green is compatible with nothing ever being quarantined, or nothing ever swept: the leak wearing the sweep's label)" "ProbeQuarantineReleaseFires"
mutation_run FlintExtentsProbe FlintExtentsProbeTruncate.cfg "extents truncate probe (TruncateStart really executes — and with it the committed-block reclaim path it alone unlocks)" "ProbeTruncateFires"

# ---- admission tranche (2026-08-10): the same-node zombie, machine-checked.
# FlintAdmission.tla models the block-layout ADMISSION layer around the lease
# sweep: per-Host-NQN allow-list, durable fence records, the volume-wide
# EA-RO, and the sweep's fence -> delivered-gated revoke -> auto-unfence.
# The tranche's job is to state EXACTLY which assumption carries safety after
# the auto-unfence reopens the door: the shipped green (same-host successor,
# door wide open) rests on ClientHonorsLease — the kernel discarding layout
# state at lease expiry, which every userspace write must traverse.  The
# Zombie run is the documented same-node residual AS A COUNTEREXAMPLE (a
# frozen-past-its-lease client writes a successor's re-granted extent);
# CrossHost proves the eviction barrier real at per-host granularity even
# against that client.  Scope limits (sweep path only; no zombie
# self-re-admission; always-evict coarsening) are argued in the module head.
strict_run   FlintAdmission FlintAdmission.cfg "admission strict (shipped world: client honours lease, same-host successor — the door is open and the invariant holds anyway)"
mutation_run FlintAdmission FlintAdmissionZombie.cfg "same-node zombie mutation (ClientHonorsLease=FALSE + same-host readmit: the sweep's auto-unfence + NQN re-admission lets a frozen client's stale mapping write the successor's extent — THE residual, machine-checked)" "Inv_NoStaleDeviceWrite"
strict_run   FlintAdmission FlintAdmissionCrossHost.cfg "admission cross-host strict (ClientHonorsLease=FALSE but the successor is elsewhere: the per-host eviction barrier alone holds the line)"
mutation_run FlintAdmission FlintAdmissionProbeSweep.cfg "admission sweep probe (the fence->revoke chain really runs in the shipped state space)" "ProbeSweepFires"
mutation_run FlintAdmission FlintAdmissionProbeReadmit.cfg "admission readmit probe (the same-host door is really walked through post-sweep — the shipped green exercised the exact door the Zombie run proves dangerous)" "ProbeReadmitFires"

# ---- the block-tier serving-composition machine (2026-08-12). ----
# docs/plans/pnfs-block-layout-design.md §12 "Replication for the block
# tier".  Models code that DOES NOT EXIST YET — the 17-agent replication
# review's verdict was that SPDK raid1 is the disk arm only, and the missing
# arbiter is an MDS-CAS'd serving-target record [epoch, composer] enforced at
# the survivor's leg-export, the composer's serving lease, and the export
# mouth.  The victim class FlintReplication has no vocabulary for is modelled
# here: pNFS block clients holding DIRECT nvme-tcp sessions to a composer the
# MDS cannot reach ("part" is MDS-unreachable, client-reachable — the
# fallible dead-vs-partitioned verdict is built into PromoteCAS's guard).
#
# FOUR FINDINGS BEFORE ANY CODE (module header carries the full argument):
# (1) eviction must not precede the lease horizon (severing a still-acking
# zombie's fan-in manufactures silent loss — the order is CAS -> horizon ->
# evict -> assemble -> replay -> redirect); (2) the review's three-arm
# arbiter had no answer to a DEGRADED-WINDOW failover — electing the stale
# leg discards every acked solo write, so promotion needs ElectInSync +
# DegradeBarrier (FlintReplication's GateStrict + RecordBarrier arriving at
# the serving-target record; stock raid1 acks on any-one-leg and records the
# miss async, bdev_raid.c:705-718, 2440-2444, so flint interposes on leg
# failure); (3) an epoch-valid fence confirmation is not yet a free license —
# the deposed composer's fan-in still reaches the surviving leg until
# assembly, under the COMPOSER'S inter-tgt hostnqn no per-client preempt can
# distinguish (FreeWaitsActive); (4) the review's "delivered_unix keyed by
# epoch" is the WRONG ENFORCEMENT POINT — a legally-freed range's victim
# re-attaches to the survivor unfenced because PTPL never travels, and no
# keying closes a free that was legal when it happened.  The belt is the
# export MOUTH (FenceReplayOnAssemble: allow-list = admissions minus fenced,
# converged before the listener exists, fail-closed), and under it the
# keying is machine-checked redundant (EpochKeyedToo explores the IDENTICAL
# 102,962-state graph as the shipped strict cfg — the guard is never the
# deciding guard anywhere).
strict_run   FlintComposition FlintComposition.cfg "composition strict (all belts on, DeliveredEpochKeyed=FALSE as shipped: one full failover with a fallible verdict, a zombie composer, direct-session clients, and every theorem holding)"
strict_run   FlintComposition FlintCompositionEpochKeyedToo.cfg "composition epoch-keyed A/B (DeliveredEpochKeyed=TRUE, full theorem stack: GREEN = the machine-checked REDUNDANCY verdict — with the fence replay at the export mouth and the free waiting on ActiveNew, the review's schema refinement changes nothing; identical distinct-state count is the receipt)"
mutation_run FlintComposition FlintCompositionSkew.cfg "synchrony-axiom price (DeadmanCertain=FALSE: the late self-suspend serves stale READS after the survivor is active — writes already contained by eviction + the degrade barrier; the documented residual of leasing without consensus)" "Inv_NoStaleServe"
mutation_run FlintComposition FlintCompositionLegGate.cfg "eviction mutation in the skew world (LegAdmissionGate=FALSE where the deadman can be late — the base world is where eviction stops being redundant: the zombie fans an innocent write INTO the serving leg; with eviction ON divergence is unreachable there, which is the attribution)" "Inv_NoDivergentServing"
mutation_run FlintComposition FlintCompositionDeadman.cfg "dead-man mutation (DeadmanGate=FALSE: the partitioned composer serves an abandoned lineage FOREVER — the local leg and the read path are what no admission gate can reach, and why the arbiter is three parts)" "Inv_NoStaleServe"
mutation_run FlintComposition FlintCompositionAssembly.cfg "record-is-the-only-door mutation (RecordAssemblyOnly=FALSE: the healed composer's reconciler re-converges the same subnqn+NGUID over the stale leg and serves it — the review's 'nothing tears down A's export' finding as a counterexample)" "Inv_NoStaleServe"
mutation_run FlintComposition FlintCompositionElectStale.cfg "election-gate mutation (ElectInSync=FALSE: the degraded-window failover — promote the leg the record already knows is stale and the assembly discards every acked solo write; with the gate ON the volume waits for the good leg instead, availability spent on durability)" "Inv_NoDoomedAck"
mutation_run FlintComposition FlintCompositionDegradeBlind.cfg "mark-then-degrade mutation (DegradeBarrier=FALSE = stock raid1's actual ack path: the un-marked solo ack leaves the election gate reading insync for a leg already missing acked bytes — the barrier is what makes ElectInSync's data honest)" "Inv_NoDoomedAck"
mutation_run FlintComposition FlintCompositionFreeEarly.cfg "free-waits-active mutation (FreeWaitsActive=FALSE: an epoch-current, honestly-keyed confirmation licenses a free in the CAS-to-assembly interregnum, and the victim's writes arrive at the surviving leg under the composer's inter-tgt hostnqn)" "Inv_NoStaleExtentWrite"
mutation_run FlintComposition FlintCompositionNoReplay.cfg "fence-replay mutation (FenceReplayOnAssemble=FALSE: a client fenced, confirmed and LEGALLY freed at epoch 1 re-attaches to the survivor unfenced — PTPL never travels, and no delivered-keying closes a free that was legal; the belt is the export mouth, fail-closed)" "Inv_NoStaleExtentWrite"
mutation_run FlintComposition FlintCompositionProbeFailover.cfg "composition failover probe (a COMPLETE promotion — CAS, horizon, evict, assemble — really occurs in the strict state space; without this every green could mean the guards deadlock it)" "ProbeFailoverCompletes"
mutation_run FlintComposition FlintCompositionProbeZombie.cfg "composition zombie probe (a deposed composer really accepts a write — the divergence green means 'contained', not 'never fired')" "ProbeZombieWriteReachable"
mutation_run FlintComposition FlintCompositionProbeFree.cfg "composition post-failover free probe (a fenced holder's range really is freed and re-granted after a failover under every belt — the quarantine tranche's vacuity lesson, not repeated)" "ProbePostFailoverFreeReachable"

echo "TLA GATE PASSED"
