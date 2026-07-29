# Maintenance drain — the csi-node roll landmine fix (model-first design)

Status: **ORCHESTRATION HALF IMPLEMENTED + SIM-SWEPT 2026-07-28** (same
day as the formal gate); live gate = drill 3.14, owed. The LOCAL half
(staged-device continuity) remains design-only. The formal work landed
ahead of the code, deliberately, in the S2 pattern: every
orchestration-level safety and liveness question below is answered by a
TLC run in `formal/` (the maintenance tranche of `FlintReplication.tla`),
not by prose. The kernel-level half is explicitly NOT modelable and is
drill-gated instead.

## Implementation map (2026-07-28, 945 lib tests)

- `maint_roll.rs` — the roller: `plan_roll` (pure planner mirroring the
  model's actions one-to-one; the RollBarrier counterexample is a unit
  test), `drain_leg` (record round → verify ARMED → graceful
  `bdev_raid_remove_base_bdev`; never outruns a refusing record),
  `maint_roll_tick` (level-triggered: one step per tick — Drain /
  DeletePod / ClearMarks — resumable from observable state alone),
  mark renewal each tick (the lease), `KubeRollOps` (DS updateStrategy +
  ControllerRevision latest-hash vs pod `controller-revision-hash`).
  Kill switch `FLINT_MAINT_DRAIN` (default ON, opt-out semantics; the
  roller is additionally inert unless the DS runs OnDelete).
- `replica_sync.rs` — the suppression mark REFINED from the design's
  "leased annotation" to a per-replica record FIELD (`maint_drain`,
  RFC3339 lease deadline): the record already lives in a PV annotation
  behind a resourceVersion-guarded CAS, so `drain_for_maintenance`
  (stale-mark = writer-set exit + mark, refusing window-claimed and
  last-in-sync legs) is genuinely ONE record round — the atomicity the
  model's MaintDrain assumes. `maint_drain_live` enforces TTL at the
  READERS: expired or unparseable reads as absent (SuppressExpire —
  fail toward readmission, never toward permanent suppression).
- Exclusion doors, all reading the mark: catch-up's chase + rebuild
  dispatch (`catchup.rs`), `resolve`'s target choice + `plan_hot_rejoin`
  (`maint_suppressed` view field) in `hot_rejoin.rs`, and the
  record-layer belt in `mark_hot_rejoin_intent` (one door for every
  intent path).
- `volume_claims.rs` — `OP_MAINT_DRAIN`, Resolver class (operator time
  must not lose the reacquisition race to catch-up's timer — the F43
  lasso shape).
- Chart: `maintenance.drainRoll.enabled` (DEFAULT OFF until drill 3.14)
  sets the DS `updateStrategy: OnDelete` + `FLINT_MAINT_DRAIN`; RBAC
  adds read-only `apps/daemonsets` + `controllerrevisions`.
- Tests: planner table, kill-switch parse, drain/lease/belt record
  tests, catch-up + rejoin exclusion A/B (live vs expired mark), and
  `sim_crash_sweep_maint_drain_roll_recovers` — acceptance gate 2:
  crash at every RPC boundary of drain → clear, re-driven roller +
  normal machinery converge to in_sync, no stranded marks/markers,
  chains stay trees. The fake SPDK layer gained a faithful
  `bdev_raid_remove_base_bdev` (slot NULLED, not shrunk — the F48
  note's observed shape, which is also what makes the re-drive
  idempotent).
- Follow-up (deliberate): `bdev_raid_remove_base_bdev` is not yet in
  `guarded_destroy`'s `GUARDED_METHODS`; the drain holds the record-level
  belt (armed check) and the data-plane belt (never remove the last
  configured base). Adding a boundary verdict is the consistent next
  move once the drill validates the flow.

## Problem

A csi-node DaemonSet roll restarts `spdk-tgt` on every node in sequence.
Each restart is a **planned data-plane outage** on that node, and the
raid cannot tell it from a failure. The landmine has been tripped live
repeatedly (v1.12.0 "landmine-hit-MDS-PVC", the topology-placement
nodes-first rollout, every helm upgrade that rolls the DS), and the
operational recipe — restart consumers afterwards, scale-cycle
Deployments — has been carried in memory since v1.10.0. The graceful
spdk-tgt recovery work (v1.15.0) fixed single-process restarts; a full
node-DS roll still trips it.

Two things have made the roll strictly more dangerous since:

1. **P4 made detection fast.** Dead-target timeouts now fault a silent
   member out in ~30-40s. A DS-roll restart (pod delete → schedule →
   image → tgt up → lvstore rescan) plausibly exceeds that, so on
   current bits a routine roll doesn't just stall writes — it
   *manufactures* a degrade → stale-mark → hot-rejoin cycle per node.
2. **Sequential rolls compose into a total outage.** k8s
   `maxUnavailable=1` serializes PODS, judged by pod-readiness. It knows
   nothing of raid membership. Roll node A, wait for its pod to be
   Ready, roll node B — while A's leg is still stale, un-readmitted. At
   r=2 that removes the last serving leg. TLC finds this as a 5-step
   counterexample with **zero real failures**
   (`FlintReplicationRollUnfenced.cfg`).

The wedge family is also connected: the operational recipe for a wedged
DS roll — *delete the Node object to unblock it* — is precisely the
false-evidence world the Resurrect mutation
(`EvidenceStrict=FALSE`) proves dangerous. A roll protocol that cannot
wedge retires that recipe.

## Design

Three guards, each proven **separately necessary** by a mutation run:

1. **The fence (drain-before-restart).** Before a node's tgt goes down,
   every leg it serves is gracefully drained: one CAS per volume, under
   the R2 claim — quiesce-free removal from the serving raid (survivors
   continue at a new incarnation), stale-mark, writer-set prune, and a
   **suppression mark** stamped on the leg. No detection wait, no write
   stall, no EIO on remote consumers: the raid never sees a silent
   member. The restart then touches only non-serving legs.
   Readmission after the restart is the *normal* hot-rejoin machinery
   (kept payload, guaranteed shared ancestry — the drained leg was
   in-sync at removal, so catch-up is a delta, not a rebuild).
2. **The barrier (readmitted, not pod-ready).** The roll may proceed to
   the next node only at FULL redundancy: every leg in-sync + serving +
   responsive. Pod-readiness is not the gate — the barrier needs raid
   state that kubelet does not have, which is why this must be a
   flint-side orchestrator and cannot be delegated to the DS controller.
3. **The lease (marks die with their holder).** The suppression mark
   excludes the leg from readmission planning (hot-rejoin, catch-up,
   admission, and the F43 yield predicate all skip it). It is leased:
   a live roller clears it after the restart; a dead roller's mark
   expires by TTL. Unleased, a roller death parks the drained leg at
   reduced redundancy forever — the F43 parked standby re-created by a
   maintenance flag (`FlintReplicationRollLease.cfg` finds the lasso).

**Rejected alternative: teach P4 about maintenance.** Suppressing
dead-target detection during a roll window closes the churn but opens a
worse hole: maintenance windows are exactly when real failures hide.
Spot reclaim during a campaign is not hypothetical — runab lost its CP
mid-campaign, runam lost a replacement worker within 2 minutes, and this
tranche's own deep run found the drained-leg-dies-mid-roll
counterexample unprompted. The drain design needs **no P4 changes at
all**: detection stays always-on, and the fence keeps planned restarts
out of its blast radius structurally. The model states this as
`Inv_MaintFenceHolds` (a serving leg's tgt is never down for a planned
restart) — with the fence, `RaidDeconfigure`'s inability to distinguish
planned from failed never matters.

## The two halves

**The orchestration half (modeled).** Everything above — the fence,
barrier, lease, and their interaction with P4 detection, replacement,
hot rejoin, and the R2 claims — lives in `FlintReplication.tla`'s
maintenance tranche and is machine-checked.

**The local half (NOT modeled — empirical).** A consumer whose pod is
co-located with the rolling tgt loses its staged block device when the
tgt exits; that is the EIO in the original landmine memory, and it is
kernel-level mechanics below the record abstraction. The 2026-07-06
root-cause analysis (runn) already named the shape: staged volumes are
tgt PROCESS RUNTIME STATE — NodeStage-created subsystems are not
re-published on boot, and the consumer ext4 journal-aborts to permanent
EIO in the gap. (A spdk-tgt DaemonSet split was considered then and
correctly rejected as partial: it stops driver-bump rolls from touching
the tgt but does nothing for the tgt's own restarts.) The durable fix
decomposes by backend:

1. **nvmeof-backed staging: restart-survivability.**
   (a) reconcile-on-boot re-publish of staged volumes' subsystems with
   IDENTICAL NQNs, and (b) initiator ride-through inside
   `ctrl_loss_tmo` — which requires the shutdown to look like
   connection LOSS, not a graceful namespace delete (a graceful delete
   fails fast; a loss retries into the re-published target). Note the
   P4 interplay cuts the other way here: the DEAD-target timeouts are
   tuned for legs; the local staging path's ride-through window must
   comfortably contain a tgt restart.
2. **ublk-backed staging: ublk user recovery** (`UBLK_F_USER_RECOVERY`
   + SPDK's recovery support) — ublk devices die WITH the process, so
   ride-through does not exist for them without kernel-side recovery.
   Needs an investigation spike: SPDK version gates, recovery-window
   bounds, interaction with the F9 ublk construction guard.
3. **Planned relocation for heads**: for a node hosting the
   flint-nfs-server (the RWX head), treat the roll as a relocation —
   the cutover bounce path S2 deliberately retained exists for exactly
   this. DrainGate (F55) bounds the client cost; F52 prewarm bounds the
   reconnect.
4. **Automated consumer scale-cycle** (today's manual recipe, made
   automatic and ordered): the honest fallback for whatever (1)/(2) do
   not cover.

The live drill gates the composed whole, not just the modeled half.

## What the models verify

| claim | where verified |
|---|---|
| Today's world is broken by design, not by tuning: a routine DS roll with ZERO real failures drives the volume to `serving = {}` (P4 faults the rolled leg, the next roll follows pod-readiness) | `FlintReplicationRollUnfenced.cfg` (`MaintFence=FALSE`): TLC violates `Inv_PlannedRollNeverCausesOutage` in 5 steps |
| The fence alone is insufficient — the barrier is separately necessary: with a drain but a pod-ready barrier, the next drain removes the last serving leg while the previous one is still stale | `FlintReplicationRollBarrier.cfg` (`MaintBarrier=FALSE`): same invariant, different path (drain → roll → clear → drain) |
| The suppression mark must be leased: a dead roller's mark on a LIVE leg otherwise never lifts and the volume parks at reduced redundancy forever | `FlintReplicationRollLease.cfg` (`MaintLease=FALSE`): temporal counterexample to `MaintenanceEventuallyLifts` |
| With all three guards: rolls never cause an outage, never stall writes, never trigger spurious replacement (strict evidence — a rolling node produces no node_gone), and every invariant (durability, divergence, generations, evidence) holds across roll × failure × claim interleavings | `FlintReplicationMaint.cfg` (3-leg breadth) + `FlintReplicationMaintDeep.cfg` (2-leg content depth: torn writes, scrub, zombies, roller death mid-campaign) |
| The F43 arbitration survives maintenance: suppressed legs are excluded from `WarmWaiting` (neither admittable nor yield-forcing), and `AdmissionNotStarved` still holds with rolls in the mix | both strict maintenance runs |
| A mark on a truly DEAD leg is inert and exempt: the deep run's first counterexample (drain a leg, spot-reclaim its node AND every rebuild source) forced the per-leg, death-escaped statement of the lifts property — `Replace` clears marks with the identity swap when a source exists; when none exists the volume is Deferred and the mark gates nothing | `MaintenanceEventuallyLifts` as stated + the trace in this tranche's history |
| The implementation's RECORD-ONLY barrier is safe — but only WITH the unconditional ground-truth belt. The belt-less first run found silent loss in 7 states: a drain armed on a record lagging the raid (survivor deconfigured, recovered, not yet stale-marked) prunes the sole serving leg from the writer set. The belt (`serving \ {l} # {}`, probe-first in code) closes it; the strict run then licenses the record-only shortcut as availability-cost-only | `FlintReplicationRollRecordBarrier.cfg` (strict, must hold) + `maint_drain_ground_truth_belt_beats_a_stale_record` (the TLC trace as a unit test) |
| A wedged restart (the roll's pod never returns — kubelet's fairness dropped) degrades exactly one leg's availability: every invariant holds and the volume stays writable on the survivor; the parked mark is the honest operational state | `FlintReplicationRollWedged.cfg` (`SpecWedgedKubelet`, strict) |
| The barrier's necessity, stated sharply: with the belt stopping the direct outage, a pod-ready barrier still ERODES redundancy at ≥3 legs (two legs out of service, zero failures — one failure from outage). Planned maintenance with the real barrier never has more than ONE leg out | `Inv_PlannedRollBoundedImpact` in the strict runs; the RollBarrier mutation must violate it at 3 legs |
| **The two-roller race (2026-07-29, F59 candidate): one-node-at-a-time and the barrier were PLANNER-only (gather-snapshot reads), so a deposed-but-alive roller's in-flight drain — the lease is one in-process check per tick, while a tick's RPC work is unbounded vs a 15s lease — landed after the new leader's drain marked (or drained-rolled-CLEARED) a different node: two legs out with zero failures.  The fix moves BOTH rules into `drain_for_maintenance`, where the rv-guarded retry re-runs them race-proof: refuse unless no other leg carries a live mark AND every other leg is record-InSync.  A marks-only belt provably loses (TLC found the clear-then-commit erosion).  The roller's lease buys pacing, not safety** | `FlintReplicationRollerRace.cfg` (gate ON, must violate `Inv_PlannedRollBoundedImpact`), `RollerRaceUngated.cfg` (same, no leadership), `RollerRaceFixed.cfg` (belt ON, NO gate — strict, must hold) + `drain_refuses_while_another_leg_is_marked_or_unhealed` (the belts as a unit test) |

Run everything: `scripts/check-tla.sh` (forty-three TLC runs as of
2026-07-29, ~11min total — the cutover tranche's two-failure budget is the bulk of it). The split is deliberate: TLC's temporal checking is
near-sequential, so the 3-leg breadth run checks INVARIANTS only (853k
states, ~7s) and all liveness lives in the 2-leg depth run (~40s) —
every maintenance liveness corner is 2-leg-reachable. The first cut
checked the per-leg lifts property at 3 legs and cost minutes for no
additional coverage.

## Implementation sketch (next cycle)

- **Roller**: a controller-side maintenance orchestrator (same
  singleton/lease discipline as the existing orchestrators — the
  F50/F53 one-orchestrator invariant applies). DS `updateStrategy:
  OnDelete`; the roller deletes csi-node pods node-by-node, gated by the
  barrier.
- **Drain**: per volume with a leg on the target node — Resolver-class
  R2 claim, then the graceful remove + `mark_stale` +
  `prune_writers` + suppression mark in one record round
  (`replica_sync.rs` has every primitive; the drain composes them).
- **Suppression mark**: a leased annotation (roller renews; TTL
  expiry), read by `hot_rejoin.rs`'s planner and `catchup.rs`'s
  reconciler as an exclusion — the same shape as the existing
  assembly-blocked marker.
- **Barrier**: all volumes with legs on ANY node full-redundancy before
  the next pod delete; any real failure mid-campaign pauses the roll
  (the model treats this as the barrier simply not being satisfied).
- **Kill switch**: `FLINT_MAINT_DRAIN` (default ON when shipped),
  opt-out semantics matching `FLINT_RWX_INPLACE_ADMISSION`.
- **Local half**: the ublk-recovery spike, then whichever of the three
  options survives contact with a real kernel.
- Non-goals: multi-node parallel rolls (serialization is load-bearing),
  CP nodes (no legs), replacing cutover for relocation.

## Acceptance gates

1. Formal gate green (**done — this commit**).
2. Crash-sweep the drain path in the sim harness (crash at every RPC
   boundary of drain → suppress → restart → clear → rejoin; chains stay
   trees, no leg lost to a half-drain).
3. New live drill **3.14 rolling-restart-under-load**: helm-roll the
   csi-node DS under pg writes on RWO + RWX volumes. PASS = zero EIO,
   zero consumer pod restarts, zero P4 dead-target deconfigures (the
   fence held — stale-marks come from the DRAIN, gracefully), per-node
   write-stall bounded by the drain budget (seconds), redundancy
   restored between nodes (the barrier observed in the claims log),
   campaign completes, ledger zero acked loss. The drill must also
   kill the roller mid-campaign once and watch the lease clear the
   mark (the M3 scenario, live).
4. Regression: 2.9 canary + one spot-reclaim-during-roll run (the deep
   run's counterexample scenario — verify the pause-and-replace path).

## Open questions folded in deliberately

- Does the drain want a quiesce for the final in-flight writes, or is
  the graceful remove at a raid-incarnation boundary already clean?
  (The model treats the drain as one CAS; the sim sweep answers the
  code-level question.)
- ublk recovery semantics under the F9 construction guard — does a
  recovering device look like a construction in progress?
- Expansion interplay: an orchestrated expand quiesce racing a drain —
  same claim domain serializes them (the S2 open question, shared).
