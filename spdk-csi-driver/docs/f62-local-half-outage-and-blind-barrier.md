# F62 — the local half destroys the serving raid, and the barrier does not notice

Status: **FOUND LIVE 2026-07-30 on runao (driver `1.22.0-rc5`), immediately
after fixing F61.** Not fixed. **`maintenance.drainRoll.enabled` must stay
OFF — and F61's fix ALONE makes a roll strictly more dangerous, not less.**

## The uncomfortable headline

F61 was a livelock: a node whose drain marked nothing could never be rolled,
so the campaign spun forever. Safe, but the DaemonSet never converged.

Fixing F61 let the pod delete through — and the very first time it did, on a
node hosting a serving raid, it took the volume down permanently.

**F61's bug was load-bearing.** The wedge was the only thing preventing the
un-implemented local half from being exercised. Removing the wedge without
implementing the local half converts a silent stall into a silent outage.

## What was measured

Sequence on runao, one RWX volume (`flint-r2`, pg-0 writing continuously,
NFS server on `runao-aws-4` together with one of the two legs):

```
01:41:21  [MAINT] node drained — deleting csi-node pod (RollStart) node=runao-aws-4
          (drained=0 — consumer == node, so the drain was SKIPPED by design;
           MaintenanceLocalConsumer had already been emitted)
01:41:51  new csi-node pod created on runao-aws-4
01:41:56  its containers start — spdk-tgt is a FRESH process
01:41:45  pg-0's last postgres log line ("checkpoint starting"), then silence
01:42:21  [MAINT] node drained — deleting csi-node pod ... node=runao-cp-1
          ^ the roller ADVANCED, one tick later, barrier NOT blocking
```

State afterwards, stable for >5 minutes with no self-healing:

| probe | result |
|---|---|
| `bdev_raid_get_bdevs` on the server's node | `{"result":[]}` — **raid gone** |
| same on the other node | `{"result":[]}` — not relocated, destroyed |
| `bdev_get_bdevs` on the server's node | 9 bdevs — the lvols survived (on disk) |
| nfs server pod | Running, `restarts=0`, alive with a dead backing device |
| pg-0 | `1/2 Running`, `restarts=0` — **hung**, not crashed |
| acked ledger | stalled 315s and counting — writes gone |
| sync record | **both legs still `in_sync`** |

pg-0 never restarted and never PANICked because the NFS mount is `hard`: the
client blocks indefinitely rather than erroring. That makes this failure
mode *quieter* than an EIO, not milder — `kubectl get pod` shows Running,
and only the readiness probe and the ledger reveal it.

## Two distinct defects

### F62a — nothing re-runs the raid assembly after a tgt restart

Verified against the SPDK source (`~/github/spdk` @ `bb2b757ac`,
v26.05.1-pre) rather than assumed, because the first framing of this —
"SPDK cannot persist a raid" — is **wrong**.

SPDK *can*: `module/bdev/raid/bdev_raid.c:3411` has `raid_bdev_examine_sb`,
an examine-based auto-assembly path that reconstructs a raid from
superblocks on its member bdevs when they reappear.

flint **opts out on purpose** — `driver.rs:3159` passes
`"superblock": false`, and the comment there gives two hard reasons:

* the superblock is "the root of the §3 phantom-assembly hazard class"
  (auto-assembly resurrecting a stale raid that then squats on the lvol —
  the F47/F49 EPERM family), and
* it "shifted the filesystem 1 MiB into every base lvol, which made
  snapshots/clones unmountable raw and silently formatted volumes restored
  from multi-replica snapshots" — a live regression on 2026-06-12.

So enabling superblocks is NOT the fix; it trades this outage for a
data-corruption class that has already bitten.

flint's design is that **raids are ephemeral and re-created at every
NodeStage from the PV replica record**. The actual gap is that nothing
re-runs that assembly when the tgt restarts *underneath an already-staged
volume*: kubelet does not re-NodeStage a volume it believes is staged, and
here the NFS server never lost its mount, so nothing triggered a restage.
The lvols survived (9 bdevs present); only the raid — the runtime
composition — was gone, and its one creator was never called again.

That points at the fix the v1.10.0 note already sketched as option (1):
a node-agent **reconcile-on-boot** that re-creates the raids for volumes
staged on that node, from the records it already reads, with identical
naming. Not a superblock.

### F62b — the barrier is blind to the damage it just caused

The barrier is documented as "raid-aware, not pod-ready" and the model
carries `BarrierRaidAware = TRUE`. But in code the barrier's evidence is the
**sync record** (`insync_by_node` + obstruction), and the record still said
both legs `in_sync` — because nothing had stale-marked anything. No leg
failed; the *raid* was destroyed out from under a healthy record.

So the barrier permitted the next node one tick later. On a larger fleet
this composes exactly like the unfenced roll TLC rejected: each node's roll
destroys one more serving raid, and the barrier waves the campaign through
because every record still looks perfect.

This is the same lesson as the RecordBarrier hardening pass (2026-07-28),
which found silent loss because "every record-level check passes on the
lying record" — and the fix there was to probe ground truth before the
record round. The barrier kept the record-level evidence.

## Three destroyers, and the one that matters

The framing that finally made this modellable is not *what* destroys the raid
but **whether kubelet still believes the volume is staged afterwards** —
because only a destroyer that clears that belief has an inverse.

| # | destroyer | staged after | inverse |
|---|---|---|---|
| 1 | consumer pod deleted → `node_unstage_volume` (`main.rs:3582`) → `teardown_volume_spdk_state` step 2 → `bdev_raid_delete` (`driver.rs:3494`) | **FALSE** | next attach re-creates it |
| 2 | node destroyed → consumer relocates → NodeStage on the **new** host | **FALSE** | re-created elsewhere (the raid host is MOBILE — the F42/drill-2.5 self-heal family) |
| 3 | the csi-node pod's tgt dies, node and consumer stay put | **TRUE** | **none** |

Cases 1 and 2 are one equivalence class from the volume's point of view,
which is why modelling F62 needs no mobile-consumer dimension. Case 3 is
F62, alone: no RPC is issued, no base is removed, no leg faults, no record
is written — and because nothing clears `staged`, NodeStage is never called
again and the sole creator of the composition stays disabled forever.

### SPDK's own coupling (verified, not assumed)

A raid bdev with nothing healthy under it is not a thing that serves, but the
mechanism is not what one might guess — there is **no demotion to a direct
lvol**, and SPDK has no such path:

* `raid1.c:622` — `.base_bdevs_constraint = {CONSTRAINT_MIN_BASE_BDEVS_OPERATIONAL, 1}`.
  raid1's floor is **one**, so at a single surviving base the raid stays a
  raid1, degraded and serving.
* `bdev_raid.c:2069-2074` (`raid_bdev_remove_base_bdev_done`) — each removal
  decrements `num_base_bdevs_operational`, and when it falls *below* the floor
  it calls `raid_bdev_deconfigure()`: the raid destroys itself.

So at 1 base the composition lives; at 0 it self-destructs. This is why the
barrier's ground-truth probe (fix C) counts **configured bases ≥ 1** rather
than asking for full membership.

## What the model says now

Seven runs, added to `scripts/check-tla.sh` (52 → 59). The composition is a
first-class object — `raidHost`, `staged`, `raidSeen` — gated behind
`RaidLifetimeArm` so all 52 pre-F62 runs keep their exact behavior graphs and
their gate cost.

| run | arm | verdict |
|---|---|---|
| `RaidLifetime` | F61-fixed code + arm | `Inv_PlannedRollNeverCausesOutage` **violated in 4 states** |
| `RaidLost` | same, liveness | `RaidEventuallyReassembled` **violated** — permanent |
| `RaidFenceAB` | refusal off | `Inv_MaintFenceStrict` **violated** (the A/B bug side) |
| `RaidRefuse` | fix B | **green** — outage prevented, campaign converges, fence at full strength |
| `RaidReconcile` | repair A2 | **green** on liveness |
| `RaidSeenBlind` | shipped detector | `RaidEventuallyReassembled` **violated** |
| `RaidSeenFixed` | repair A1 | **green** |

The `RaidLifetime` counterexample is four states long and is the runao
measurement line for line: `MaintDrainSkip(l2)` marks nothing →
`RollStart(l2)` → `raidHost = "none"`, `serving = {}`, **`staged` still
TRUE**, **`state = (l1 :> "insync" @@ l2 :> "insync")`**. The uncomfortable
part is that its cfg is `FlintReplicationRollProcessed.cfg` with one constant
flipped — a run that was green, and that blessed the F61 fix.

Three conclusions worth separating:

1. **F61's fix alone is worse than F61's bug.** The livelock was the only
   thing keeping the un-implemented local half unexercised. `MaintLocalRefuse`
   is not optional polish.
2. **A repair is not a prevention.** `RaidReconcile` deliberately does *not*
   carry `Inv_PlannedRollNeverCausesOutage`, and cannot: A2 recovers the
   volume after the outage. Only refusal keeps it up. Both are wanted —
   refusal for the planned roll, a repair for the unplanned tgt deaths no
   orchestrator can refuse.
3. **A1 suffices for recovery.** One rehydration call restores a repair path
   that already exists, which is what makes it the first thing to code.

### Two model defects found on the way

Neither is about F62; both were hiding behind it.

* **`BouncePlannable` required `serving # {}`** — "something to tear down".
  Right for the admission arm, exactly backwards for the data-path arm, which
  fires *because* the data path is gone. The code has no raid-membership term
  there at all (`cutover.rs:485-489`: *"The bounce IS the remediation — a
  restage rebuilds the raid from the in-sync replicas"*). The guard disabled
  the remediation in precisely the state it exists to remediate, and only
  looked right while `serving` doubled as "the volume is up".
* **`AgentFlag` had no fairness at all** (only `AgentClear` did). So TLC was
  always free to decline to flag, and **no run in this module could ever have
  concluded the data-path repair chain works** — its liveness was
  unfalsifiable in both directions. `FairnessAgent` now carries it, at SF:
  per-leg WF is defeated by an environment that alternates which leg is
  blackholed, and a periodic poller does fire against a flapping environment.

Both are the same species as F62 itself — a guard that silently disables
progress, with no property asking.

## Disposition

1. **`maintenance.drainRoll.enabled` stays OFF** until the fixes below are
   live-gated by drill 3.14. F61's fix alone is worse than nothing.
2. **F61's fix is NOT reverted** — the livelock is a real defect and its
   tooth (`FlintReplicationRollWedge.cfg`) stays in the gate.

### Landed (986 lib tests)

**A1 — the detector's trigger** (`node_agent.rs`, in
`rehydrate_exports_from_ground_truth`). `data_path_raid_seen` is now seeded
for every volume whose VolumeAttachment names this node, on the same
predicate and in the same block as the neighbouring `expected_ublk` seed.

That neighbour is the part that stings. Its comment already reads:

> …the CONSUMER-side ublk chain (raid → ublk disk) is seeded here, or an
> agent restart forgets it entirely (2u/2.2b: **csi-node pod delete on the
> RAID host left the volume dead** until the 60s monitor's 3-strike repair)

So this exact scenario was found and fixed once, for the ublk half, and
`data_path_raid_seen` was left a fresh empty `HashSet` beside it. And the
seed-from-records-not-from-SPDK lesson was learned a second time, by F8:
*"a pod-level restart leaves BOTH the registry and the target empty — seeding
from live subsystems finds nothing."* Seeding this set from the live raid list
would read empty in precisely the situation that matters.

Note the ublk seed alone **cannot** recover F62 — it maps an id to a raid
bdev that no longer exists — which is exactly why runao stayed down.

**B — refuse and surface** (`maint_roll.rs`). New `RollView.local_consumer_nodes`
(recomputed every tick, so the roller stays resumable) and a new terminal
`RollStep::Refused { nodes }`. `plan_roll` skips such nodes when choosing the
next pending one, so every other node still converges, and when only refusals
remain it returns `Refused` rather than `Idle` — reporting "done" with nodes
still on the old revision would be F61's silent give-up in better manners.
The tick logs the set and emits `MaintenanceNodeRefused` per affected volume.

The predicate is deliberately narrower than `nothing_to_drain`, which also
holds nodes with unattached volumes or no legs at all: there is no
composition there to lose, and rolling them is *required* for the DaemonSet
to converge. Refusing all of `nothing_to_drain` would rebuild F61. It also
does not ask whether the node holds a leg — a node consuming a volume whose
legs are all remote still hosts the raid, over NVMe-oF.

**C — the barrier probes ground truth** (`maint_roll.rs`, `gather_volume_maint`).
For each volume with a consumer, `bdev_raid_get_bdevs` on that consumer and a
configured-base count ≥ 1 (SPDK's own floor). Absent raid, zero configured
bases, or an unprobeable consumer all become barrier obstructions — refusing
on an unprobeable view is what `drain_leg` already does, and blocking a
campaign is recoverable where destroying a volume is not.

### Modelled and deferred

**A2 — the agent re-creates the composition on boot** for volumes its records
say are staged here (the v1.10.0 note's option 1). Green in
`FlintReplicationRaidReconcile.cfg`, and the *only* fix that helps an
**unplanned** tgt death, which no orchestrator can refuse. Deferred because
TLC shows A1 already restores recovery and A2 is the larger change; it wants
its own tranche and its own careful look at the phantom-assembly class.
Explicitly **not** a superblock: flint passes `"superblock": false` on
purpose, and enabling it trades this outage for a data-corruption class that
has already bitten.

## Why neither the model nor the drills caught it earlier

- The model **cannot** see it: the local half is explicitly out of scope,
  and `MaintDrainSkip` (added for F61) leaves `serving` unchanged, so TLC
  believes rolling a local leg is harmless bookkeeping.
- The drills **could not reach** it: F61's wedge meant the pod delete never
  happened, so no drill had ever restarted a tgt under a live serving raid.
  Fixing the livelock was the precondition for measuring the gap.
