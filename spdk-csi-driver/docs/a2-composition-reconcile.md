# A2 — the agent re-creates the composition on boot

Status: **MODELLED 2026-07-29** (`formal/FlintReplication.tla`, A2 tranche;
gate 62 → 68 runs). **NOT CODED.** This document is the design and the
model's verdict on it, written before any implementation, deliberately.

## Why this and not B′

The question that started it: with F62 and F63 closed, a node hosting a
consumer is *refused* by the roller, so `maintenance.drainRoll.enabled` still
cannot go on — the campaign does not converge unattended. Two candidates.

**B′** — the roller relocates the consumer itself, then rolls the node.
**A2** — the agent re-creates the composition after its tgt returns.

B′ was the earlier recommendation. It is wrong, for two independent reasons.

### 1. B′'s cost scales with consumer density; A2's does not

`local_consumer_nodes` is node-granular — `maint_roll.rs:762-771` builds it
with `.any()`, so one consumed volume on a node and fifty look identical to
the planner. To clear the refusal, B′ must move **every** consumer off that
node.

Measured unit: ~95s of write stall per relocation (runap, one observation).
For a node hosting N consumers:

| | wall clock | blast radius |
|---|---|---|
| sequential | N × 95s per node | one app at a time |
| parallel | ~95s per node | **N apps stall simultaneously** |

Neither respects what the design's own theorem promises.
`Inv_PlannedRollBoundedImpact` says a planned roll costs at most one degraded
leg; a synchronized N-app stall is not that in any sense an operator cares
about.

And relocation **amplifies**: evicted pods land on other pending nodes, and
the roller advances name-sorted (`pending.sort_by(node_name)`), so a pod
evicted off `aws-1` onto `aws-5` is evicted *again* when `aws-5`'s turn
arrives — O(N²) relocations for a fleet of N in the worst case. Constraining
relocations to already-rolled nodes bounds it, and fix B conveniently supplies
the seed set (it converges every node without a local consumer first), but on
a densely-packed cluster phase 1 rolls nothing and there is no seed.

A2's cost is **one tgt restart per node, independent of how many consumers
that node hosts**.

### 2. B′ cannot reach the path that actually matters

Fixes B and B′ are properties of flint's roller. In the shipped default
configuration that roller is not in the picture at all:

- `maintenance.drainRoll.enabled` defaults **false** (`values.yaml`).
- The chart emits `updateStrategy: OnDelete` **only inside that conditional**
  (`templates/node.yaml:13-24`). So the shipped DaemonSet takes k8s's
  **RollingUpdate** default, and a routine `helm upgrade` that touches the
  csi-node pod template rolls every node pod — killing spdk-tgt under whatever
  consumers are there, one node after another.
- `plan_roll` stands down anyway when the strategy is not OnDelete:
  `if !view.on_delete { return RollStep::Blocked }` (`maint_roll.rs:248`).

The chart's own comment states the intent — OnDelete exists "instead of the
k8s RollingUpdate default whose pod-readiness gate the formal model refutes (a
routine roll can take a volume fully down with zero real failures)" — and that
protection is off by default.

Add OOM kills, kubelet restarts, node-pressure evictions, node-image upgrades
and GitOps syncs. **None of these consult flint, and none can be refused.**
Only an agent-side repair has an inverse for them.

B′ also cannot be complete on its own terms: a consumer that cannot be
evicted must still be refused, and a single-replica StatefulSet with a
PodDisruptionBudget of `minAvailable: 1` is *designed* to refuse eviction —
which is the shape of flint's flagship consumer, a database.

## What the model did not know, and why that matters more than the code

Two structural gaps, both of which made earlier greens worthless.

**The composition was a scalar.** Through the whole F62/F63 cycle the model
had `raidHost \in Legs \cup {"remote", "none"}`. A2's hazard is a **second
creator**, so the state "two compositions exist over these lvols" is the one
thing it most needs to check — and a scalar cannot represent it.
`FlintReplicationRaidReconcile.cfg` went green against A2 on a hazard it was
structurally unable to see, and that green was then cited as "A2 is modelled".
It was not. `raidHosts` is now a set; every creator except A2 assigns a
singleton, so a cardinality violation is attributable to A2 by construction.

This is the third instance of one lesson in this codebase — the pod-layer
tranche recorded it as *the abstraction was the bug*, two independent creators
of the nfs pod. Here the two creators are NodeStage and the agent's boot pass.

**Class-3 destruction lived only inside `RollStart`.** So every F62/F63 run
asked "can flint's roller destroy a composition?" — about a feature that is
off by default and declines to act in that configuration. `TgtDie(l)` now
exists, gated on no maintenance state whatsoever, and the default path is
expressible for the first time.

## What the tranche found

### Finding 1 — the naive A2 assembles on a node the consumer has left

A2's only available input is the VolumeAttachment. That is the *right* input —
it survives the agent's death where a local `HashSet` does not (F8's lesson),
and it is the predicate A1's seed already trusts:
`va_map.get(&pv_name) == Some(&self.node_name)` (`node_agent.rs:3279`).

But it lags, and the implementation says so in its own words.
`node_agent.rs:3219` gives the ublk reaper's reason for existing:

> A disk that is attributable but not desired is a leak — e.g. the local disk
> **a stale VA made us rebuild after the consumer moved away** — and the fast
> detector would otherwise resurrect it forever.

So rebuild-from-a-stale-VA is not a hypothesis. It is observed behaviour with
a garbage collector already written for it.

`FlintReplicationA2Naive.cfg` — TLC finds it in **four states**:

| state | |
|---|---|
| 1 | consumer on `l2`; composition on `l2`; VA names `l2`; kubelet staged at `l2` |
| 3 | `RelocateConsumer` — NodeUnstage runs (`stagedAt = "none"`), composition destroyed, `localLegs = {}` — **but the VA still names `l2`** |
| 4 | `AgentBootReconcile` on `l2` — VA says `l2`, nothing assembled there, guard satisfied → **assembles a raid on a node with no consumer** |

The asymmetry that earns A2 its own tranche: on the single-replica path that
staleness leaks a ublk disk and the reaper takes it. Over a raid it produces a
phantom composition, and no reaper undoes what a phantom is adopted into.

### Finding 2 — the obvious belt is refuted

"Refuse to assemble if any other host already holds a composition" is the
first thing anyone reaches for, and it is implementable today — fix C already
calls `bdev_raid_get_bdevs` against another node.

It does not work. It is **check-then-act**, so A2 defeats it by going *first*:
assemble on the stale node while nothing is held anywhere, and the legitimate
NodeStage — which has no such belt and should not need one — supplies the
second host afterwards. `FlintReplicationA2SoleOwner.cfg` still finds the
violation. Kept as a permanent run so the refutation is evidence rather than a
paragraph of reasoning.

### Finding 3 — the belt that works is a predicate the F62 analysis already named

**Assemble only where kubelet also still believes the volume staged.**

The F62 doc already identified `staged` as *the discriminator* between the
three destroyers — only one that clears it has an inverse — and then did not
use it:

| | `stagedAt` | A2 should |
|---|---|---|
| class-3 tgt death | `= me` (kubelet still believes it staged here) | **fire** |
| relocation | `≠ me` (NodeUnstage ran; the new host restaged) | **refuse** |

It admits exactly the case A2 exists for and refuses exactly the case that
manufactures a phantom — with no cluster-wide probe, no lease, no superblock,
and nothing remembered across the restart. It is *local* ground truth, which
is the property F63's two fidelity bugs were both about not having.

`FlintReplicationA2Staging.cfg` is green on the full invariant set with the
dangerous situation verified reachable (`FlintA2ProbeFires.cfg` must violate
`ProbeA2WouldHaveFired`) — so the belt is doing real work, not standing next
to a disabled action.

One detail the model had to get right: **A2 leaves `staged`/`stagedAt`
untouched.** A node agent re-creating an SPDK object does not participate in
kubelet's bookkeeping and cannot alter it. An earlier draft had A2 write
`stagedAt' = vaNode`, which would let it satisfy this very belt with its own
previous action — self-justifying bookkeeping, the same defect as F63's two,
caught here by reading a counterexample rather than by a property failing.

### Finding 4 — what actually recovers an uncontrolled tgt death

Four runs over `TgtDie`, no roller involved:

| cfg | trigger | result | meaning |
|---|---|---|---|
| `UncontrolledBlind` | collapse-event detector only | `Inv_RaidRecoveryUnreachable` **holds** | **scoped**: with that as the only trigger, nothing recovers |
| `UncontrolledA1` | A1's seeded detector | **violated** | the collapse-event path recovers it |
| `UncontrolledStrike` | shipped periodic repair | **violated** | recovers with A1 off, A2 off, **zero bounces** |
| `UncontrolledA2` | A2 at boot, `MaxBounces = 0` | **violated** | recovers with no bounce, pod delete or unstage |

Non-vacuity: `FlintA2ProbeDeath.cfg` must violate `ProbeTgtDeathReachable`, so
the death really happens in the green run.

**A correction, because this document originally got it wrong.** The first
version of this section read "A1 is the only thing standing between a routine
`helm upgrade` and a permanent outage," citing `UncontrolledBlind`'s green.
That was an overstatement produced by a model carrying only one of the two
triggers. `data_path_raid_seen` gates the **collapse event**
(`raid_collapse_verdict`'s `previously_seen`); the **layer-2 in-place repair**
needs no seeded state at all — its gate is `strikes >= threshold` on live
observations, and it calls `repair_data_path`, which reassembles exactly as
NodeStage would. `UncontrolledStrike` is the run that says so.

So `UncontrolledBlind`'s green scopes to the collapse-detector path and must
not be cited as "the shipped code cannot recover."

**What the correction does *not* soften is latency**, and that turned out to
hide a real defect — see the starvation section below.

**The belt was already shipped.** `repair_data_path` refuses unless
`is_staged_here(volume_handle)` reads kubelet's own staging directory. That is
*exactly* the predicate this tranche derived for A2 from first principles,
already written on the adjacent path, with a comment naming the same hazard:
*"VA lingering mid-detach?"*. So the shipped repair is already the safe shape,
and **A2 differs from it only in its trigger.**

## Correcting the harm analysis

The obvious claim about two compositions — two guests writing the same lvols
and diverging — is **wrong**, and I asserted it before checking. At the moment
A2 creates a phantom, that node has no consumer issuing I/O: the pod left,
which is *why* the VA was stale. A raid1 with no opener is passive.

The two harms that survive reading the code:

1. **The phantom is a control-plane lie, and it lies to the belt this cycle
   installed.** Fix C's barrier probes `bdev_raid_get_bdevs` on the consumer
   and requires configured bases ≥ 1. A phantom answers that probe exactly
   like a healthy composition.

2. **A later NodeStage adopts it without validation.** `ensure_raid1_bdev`
   (`driver.rs:3105`) reuses any raid of that name in state `online` —
   *"already ONLINE (N base(s) configured) — reusing"* — and compares nothing
   against the base set NodeStage intended; the count it reads goes into the
   log line and nowhere else. `raid_name` derives from the volume handle
   alone, so the phantom is **name-identical** to the raid the real host
   needs. A consumer returning to that node inherits a composition whose
   members were chosen by a boot-time snapshot of `UpInSync` taken while the
   volume was somebody else's — the adopt-or-mint family (F44/F46) reached by
   a new road.

## The adopt, modelled — and the fix that would have been wrong

The paragraph above originally ended "if A2 lands, `ensure_raid1_bdev`'s
ONLINE-reuse must validate the base set first." **Modelling it says don't.**

`AssembleAdopt` and `AssembleValidate` are the two arms of the code's
converge branch. Ordinary `Assemble` cannot express the first, because it
requires `serving = {}` and an online raid has at least one configured base —
so the short-circuit needed its own action.

| run | arm | adopt | flap |
|---|---|---|---|
| `A2AdoptBlind` | naive A2 + shipped reuse | **reachable** | — |
| `A2AdoptValidated` | naive A2 + validate-and-recreate | closed | **reachable** |
| `A2AdoptBelted` | local-staging belt + shipped reuse | closed | closed |

**Why validation is worse than the hole it closes.** Its remedy is to *delete*
the other creator's object, so that creator puts it back:

| state | |
|---|---|
| 2 | consumer leaves `l2`; raid destroyed there; VA lags at `l2` |
| 3 | A2 builds a phantom on `l2` off the stale VA |
| 4 | consumer **returns** to `l2` |
| 5 | NodeStage validates → **deletes** the phantom |
| 6 | A2 **rebuilds it** — the VA still says `l2` |

At state 6 NodeStage sits between its own delete and its create, and the object
is back. Its `bdev_raid_create` then hits `EEXIST`, and `MAX_ATTEMPTS = 3`
runs out: *"RAID bdev did not converge after 3 attempts (phantom kept
re-appearing)"* — a string the code already has, for the superblock-examine
version of exactly this fight. So the failure mode is not churn, it is **a
failed NodeStage**, which is a volume that will not mount.

**The belt closes the adopt by refusing rather than deleting**, so there is no
object to fight over and no cycle to enter. `A2AdoptBelted` holds both theorems
over the full state space.

**Verdict: the `ensure_raid1_bdev` change is defence in depth, not a
prerequisite — and it must not ship on its own.** If it is ever added, it needs
the belt underneath it.

A note on how that conclusion was reached, because the first version of it was
wrong. `Inv_NoValidateFlap` began as a *count* — `a2Builds >= 2 /\
validateDeletes >= 1` — and TLC satisfied it with build, build, delete: two
builds and a delete, no cycle at all. Violable for a reason other than the
mechanism, which the pod-layer tranche's rule says proves nothing about the
mechanism. Restated as an *order* (A2 rebuilding at a host validation had
deleted from) it produces the trace above.

### What is still not modelled here

`serving` is global, so the model is exact only while one composition exists at
a time — which is what these traces do, but it means "no reachable trace"
should be read with that scope. Per-host serving would be the honest
generalisation and is a larger refactor.

## The starvation defect, and what A2 is actually worth

Step 0 of the plan was to verify two premises before writing A2. Both moved.

### USER_RECOVERY: consumer continuity is already engineered

In ublk mode spdk_tgt is **deliberately SIGKILLed** on pod stop
(`templates/node.yaml:114-129`): a graceful SIGTERM fini `STOP_DEV`s every ublk
disk, *deleting the kernel gendisks live mounts sit on* (drill 1u/1.9b). Run as
a child of a shell PID 1 whose TERM trap kills it, the devices **quiesce** under
`UBLK_F_USER_RECOVERY` and the next agent recovers them in place, mounts intact.

So the missing piece was never the consumer — it is only that the raid *beneath*
the quiesced device gets rebuilt, since `ublk_recover_disk` needs its
`bdev_name` to exist.

**Operational corollary: never `--force` delete a csi-node pod during a roll.**
That converts the safe quiesce into the DEAD case (`is_dead_ublk_device_error`,
seen on runy2 under a wedged containerd and on runz after an in-flight-roll
force delete), which costs the mount.

### The repair was starved by pass ordering — during exactly the failure it repairs

`detect_lost_data_paths` was the **seventh of nine sequential passes** in one
60s task, each bounded at 300s, and its repair needs several consecutive ticks
of strikes. Two things compound:

1. The two heaviest passes ahead of it (`reconcile_replica_targets`,
   `rehydrate_exports_from_ground_truth`) make cross-node RPCs whose own client
   timeout is **also 300s** — so they stall longest exactly when a peer's tgt is
   dead or mid-roll.
2. `interval` cannot make the loop reentrant: the next tick cannot start until
   all nine passes return. **Strike cadence was the whole loop's duration, not
   60s** — so moving the pass earlier in the chain would not have fixed it.

A nominal ~3-minute repair becomes tens of minutes behind a couple of stalled
peers. That is a plausible explanation for runao's observed ">5 minutes, no
self-heal" — unproven, since that cluster is gone, but it makes the observation
expected rather than surprising, and it is a defect either way.

**Fixed 2026-07-30** (989 lib tests): the detector runs in **its own task** on
its own interval; the monitor's pass budget is **split** into cross-node (300s,
unchanged) and local (30s, since 300s on a unix-socket RPC is not a bound but
300s of the tick handed to a hung local call); and the **repair threshold is 2
while the flag stays 3** — the repair is idempotent, lock-serialised and
staged-here-gated, whereas flagging can end in a pod bounce. The asymmetry is
pinned by `cutover::repair_due` and the test
`repair_fires_a_tick_before_the_flag`.

The global `FLINT_NODE_AGENT_HTTP_TIMEOUT_SECS` was deliberately **not** lowered:
that client also serves CSI provisioning, where a timeout turns a slow operation
into a failed one.

### So what does A2 buy?

Not "recovery becomes possible" — that was the overstatement. What it changes:

- **Latency: strikes × cadence → zero.** The agent knows at boot which volumes
  it owes a composition; it does not have to *observe* the absence N times.
- **A far simpler predicate.** The detector *infers* "a raid is owed here" from
  `attached && !raid_present`, which needs the VA present and attached, the PV
  list, a correct raid-name derivation, and to survive four separate `continue`
  guards (RWX user PV, single-replica, single-survivor direct serve, degraded
  direct homes). A2 reads one local file.
- **It lands inside the consumer's patience window**, so the quiesced device is
  recovered before the mount's tolerance is spent — continuity by design rather
  than by the repair happening to be prompt.
- **It is what lets fix B be relaxed** from "refuse" to "proceed", which is the
  only route to convergence on a saturated fleet.

What it does not buy: nothing for a raid lost while the agent stays up (that is
the detector, now responsive), and nothing for a mount that already landed DEAD.

## The peer-availability grace — A2 must not assemble degraded

Found 2026-07-30, by hand, while working out what an all-at-once upgrade costs.
It is a hazard A2 *introduces*, so it belongs in the design rather than in the
residual list.

### The strike delay is doing a second job nobody designed it for

`create_raid_from_replicas` sets `min_required = 1` (`driver.rs:2437`).
Assembly does not wait for full membership — whatever attaches is what serves.
And when exactly one leg attaches on a multi-replica volume it does not build a
raid at all:

```
base_bdevs.len() == 1 && total_replicas > 1
  → "SINGLE-SURVIVOR DIRECT SERVE ... (no raid layer)"
  → stamps chert.us/degraded-direct              driver.rs:2511-2530
```

Redundancy is gone, writes resume on one leg, and the leg that was merely *late*
is now genuinely divergent — it needs catch-up, or a full rebuild if no shared
epoch survives retention (`catchup.rs:1801`).

Whether that fires is a race between two independent agents: node A's repair
against node B's `rehydrate_exports_from_ground_truth`. Today A loses only if B
is more than ~120s late, because the 2-strike debounce holds A back. **That is
an accident.** The strikes exist to avoid repairing an in-flight stage — the
comment at the site says exactly that — not to wait for peers.

A2 fires at boot with no strikes. In a rolling upgrade that is harmless: peers
were never down. In an **all-at-once** upgrade — the correct procedure for a
version-incompatible change, since it is the only one with no mixed-version
window — every agent boots at once and every A2 races every peer's export
rehydration simultaneously.

Note the perversity. All-at-once is the *correctness-safest* path: simultaneous
death means no leg takes writes another misses, so nothing goes `Stale` and full
membership is guaranteed available. It is also the path on which A2 would most
reliably throw that membership away.

### The requirement

> A2 must never assemble a membership smaller than the one that would have been
> available had it waited — unless waiting has already been given up on.

Two failure modes pull in opposite directions:

- assemble too early → degraded-direct storm (**safety**: silent redundancy loss)
- wait for full membership unconditionally → a genuinely dead peer blocks
  recovery forever, which is the F61 livelock class rebuilt by another road
  (**liveness**)

So the grace must be a **deadline, not a condition**, and the fall-through must
be *exactly* today's behaviour rather than a refusal.

### The design — ⚠️ SUPERSEDED 2026-07-30, the same day it was written

**A new knob was the wrong answer, and the grace is not missing.** The F36c
freshness gate already *is* this grace, and it is already shipped:

- it runs **inside `create_raid_from_replicas`** (`freshness_gate.rs`), on the
  same function, *before* `min_required = 1` and before the direct-serve branch;
- its own module doc states the decision as *"given the recorded last-writer set
  and evidence about each missing writer's availability, decide whether to
  **assemble, defer this tick, or serve**"*;
- it classifies a missing writer on a Ready node as transient
  (`LegAvailability::NodeReady`) and defers on a **180 s** deadline
  (`FLINT_F36C_DEFER_SECS`) — *longer* than the 120 s proposed below;
- it states this section's own principle back at it: *"a PERMANENTLY lost writer
  must never manufacture an outage."*
- and `repair_data_path` inherits all of it, because it reassembles *"exactly as
  NodeStage would."*

So `FLINT_A2_PEER_GRACE_SECS` would install a **second, weaker, independent
deadline for a question F36c already owns** — precisely the setter/reader
asymmetry this codebase congratulates itself on having structurally eliminated
elsewhere. Nothing shipped (`grep -rn 'PEER_GRACE' src/` = 0 hits), so nothing
has to be un-shipped. **Drop it.**

### What is actually unguarded

The hazard is real; the diagnosis was one layer off. F36c fails **open** in
three places, none of which has any concurrency content — they are missing
branches, not missing orderings:

1. **The record-load error** — `driver.rs:1877` logs *"staging without it"* and
   continues with `record = None`, which empties the writer set, empties
   `missing`, and returns `Proceed`. One inconsistent read silently disables the
   entire gate.
2. **An absent `writer_set`** — same path to `Proceed` with no evidence
   consulted at all.
3. **The ratchet, and it is the worst of the three.** `mark_stale`
   (`replica_sync.rs:391-397`) prunes the leg from the writer set, and its own
   comment names it: *"the ONLY writer-set removal path besides the wholesale
   assembly stamp (a too-small set is the F36c loss vector)."* So the 180 s
   defer is spendable **once per loss** — the leg it would have deferred for is
   no longer in the set to defer on. A rolling upgrade spends it.

### The fix, relocated

Put the guard **at the branch**, not inside A2: at `driver.rs:2511`, where
`base_bdevs.len() == 1 && total_replicas > 1` is about to become a direct serve,
probe each `unavailable_replicas` entry (live there, carrying `node_name`)
against **live** node availability and defer if any is transient.

Keyed on live state rather than the persisted writer set, it survives all three
fail-open paths *and* the ratchet, and — because it sits at the branch — it is
inherited by **all three callers** of the reassembly path rather than by A2
alone. That designs the race out instead of verifying it.

The retained principles, which were right: a **deadline, not a condition** (or a
dead peer blocks recovery forever — the F61 livelock class by another road), a
**ceiling, not a floor** (all peers present assembles immediately), and
`total_replicas == 1` never waits.

### What the model must gain before this can be verified

The grace is **not representable in the module as it stands**, and that is the
load-bearing sentence here.

`FlintReplication.tla` models a leg as live-or-not. It has no notion of a leg
that is *alive but not yet exported* — which is the entire window this design is
about. Assembly is all-or-nothing over `UpInSync`, so partial membership is not
a state the model can occupy: degraded-direct has no representation, and neither
does the choice to hold rather than take it.

By this project's own rule — a scalar is an assertion; the abstraction was the
bug, twice — **modelling the grace against the current abstraction would produce
a green that means nothing**, because the hazard it excludes cannot be reached.
Two additions are the minimum: a per-leg `exported` flag, and an assembly able to
produce a proper subset of the expected membership.

The invariant, stated so it does not condition on the arm it evaluates:

```tla
Inv_A2NoEarlyDegrade ==
  A2Arm =>
    \A h \in raidHosts :
      (servedMembership[h] # ExpectedMembership) => graceExpired
```

with a paired mutation (`A2PeerGraceArm = FALSE`) that must FIND the
degraded-direct assembly — without it the strict run proves nothing.

**This claim survived the 2026-07-30 audit, and it was tested rather than
assumed.** The tempting cheap route — "no new variables needed, `rolling` is
already the per-leg *tgt down, exports gone* bit that `Responsive()` reads" — is
refuted by its own world. In the configuration that corresponds to the shipped
default path (`MaintEnabled = FALSE`, so `rolling` is permanently `{}`), a probe
for the degraded repair returns **no error over a complete 162-state graph**:
the model would report the hand-found hazard as *impossible*. `rolling`'s only
non-empty writer is `RollStart`, which requires `MaintEnabled` — the
off-by-default roller. Any counterexample obtained that way would be a true
statement about the OnDelete path offered as evidence about the RollingUpdate
one.

**And one fidelity defect must be fixed first, or the extension produces a void
RED — the mirror of the void green this doc now records.** `StrikeRepair` and
`AgentBootReconcile` both commit `serving' = UpInSync` with `writerSet`,
`raidGen`, `legGen` and the gate variables all `UNCHANGED`. They carry **no F36c
conjunct at all**, while the shipped `repair_data_path` runs the whole gate by
calling `create_raid_from_replicas`. So the model's repairs are strictly more
permissive than the code's, and the first counterexample the extension produced
could be a counterexample to the model's shortcut rather than to flint.

**Decision, 2026-07-30: not building this now.** The extension is only worth its
cost if A2 ships, and A2 has been retired as the next fix (below). The guard
moved to `driver.rs:2511` instead, where it needs no model because it removes
the branch rather than reasoning about the interleaving.

## Implementation sketch — NOT SCHEDULED (A2 retired 2026-07-30, see below)

Kept because the shape is right if A2 is ever revived. In
`rehydrate_exports_from_ground_truth` (`node_agent.rs`), the
`replica_count > 1` branch currently only seeds detectors (A1). Extend it:

1. Guard on `va_map.get(&pv_name) == Some(&self.node_name)` — already there.
2. **Add the belt**: this node's kubelet-level staging still names the volume.
   Local, observable without the API server, nothing remembered.
3. ~~Apply the peer-availability grace.~~ **Superseded** — the grace belongs at
   `driver.rs:2511`, not here, and F36c already owns the deadline. A2 gets it
   for free by reassembling through `create_raid_from_replicas`.
4. Attach legs (local lvol + remote NVMe-oF), then `bdev_raid_create` with
   `"superblock": false` — never a superblock; that trades this outage for the
   §3 phantom-assembly class plus the 1 MiB payload shift that silently
   formatted restored snapshots (2026-06-12).
5. Serve only what the records vouch for as `in_sync` — assembling a leg the
   records do not vouch for is the phantom class by another route.
   ⚠️ This doc previously said `Inv_A2NeverServesUnvouched` "is in the gate
   waiting for it". **It is not**: defined once in the module, present in **0 of
   74 cfgs**. Never checked, and asserting a safety net that does not exist is
   how a void green gets cited as a proof.
6. Re-create the ublk chain, as the single-replica path already does.

## ⚠️ THE A2 RUNS WERE VOID, AND A2 IS RETIRED AS THE NEXT FIX

**2026-07-30.** The runs this document cited did not exercise A2 at all.

`FlintReplicationA2Staging.cfg` — headed *"THE DELIVERABLE"* — never fires
`AgentBootReconcile`, over its complete state graph (7,375,538 generated /
1,261,953 distinct / **0 left on queue**). Verified directly: `ProbeA2Fires ==
a2Created = {}` HOLDS, and `a2Created` has exactly one writer.
`FlintReplicationA2AdoptBelted.cfg` is the same, and worse: neither
`AgentBootReconcile` nor `AssembleAdopt` fires, so both adopt theorems were
green on a state space containing no A2 composition to adopt.

**One cause for both.** A2 answers a class-3 destroyer, which by definition
leaves kubelet's `staged` belief SET, and the belt requires exactly that. Both
cfgs pin `UncontrolledTgtDeath = FALSE`, leaving only destroyers that CLEAR
staging. Belt precondition and reachable destroyers were mutually exclusive:
**unsatisfiable by construction**, not merely unexercised.

**And a probe built to catch precisely this missed it.** `ProbeA2WouldHaveFired`
asks whether the *situation* arises — whether the naive guard becomes
satisfiable. It never asks whether the *belted action fires*. Those diverge
exactly where it matters, because the naive guard wants `vaNode # VaTruth` and
the belt refuses precisely that. **A non-vacuity probe must name the ACTION, not
the SITUATION.**

**Third occurrence in this workstream.** Two creators of the nfs pod; a scalar
`raidHost` making A2's hazard unrepresentable; and now an action unreachable in
the runs that cite it. Different mechanisms, identical symptom — a green read as
a proof.

**What replaces them.** `FlintReplicationA2Armed.cfg` flips the one constant,
sets `MaintEnabled = FALSE` (the shipped default path has no roller, which also
makes the roll theorems vacuous rather than suppressed), and is paired with
`FlintA2ProbeArmed.cfg`, which the gate **requires TLC to violate**. The belt
holds over the complete graph and the probe fires in three states: `Init →
TgtDie → AgentBootReconcile`. Same for the adopt. **The belt is, at last,
genuinely validated — and only now.**

**Still confounded, deliberately left with a warning rather than fixed.**
`FlintReplicationUncontrolledA2.cfg`'s required violation is produced by
`Assemble`, not by A2 (`Init → TgtDie → Assemble`). `FlintReplication.tla:1633`
reads `(RaidLifetimeArm => (~staged \/ RaidReconcileArm))`, so the A2 constant
*also* relaxes ordinary NodeStage on a still-staged volume and the A/B moves two
things at once. De-confounding needs a separate constant and a 74-cfg edit;
since A2 is retired, the run carries an explicit DO-NOT-CITE label instead.

**Why A2 is retired.** Not refuted — *unvalidated*, and its value collapsed
independently:

- `repair_data_path` already carries the `is_staged_here` belt A2 was designed
  around. **A2 differs only in its trigger.**
- The starvation fix cut the latency gap to ~120 s. A2 was competing with "many
  minutes"; now it competes with two ticks.
- The peer-availability finding **inverts** the remainder: A2's whole benefit is
  firing sooner, and firing sooner is what walks into degraded-direct.

It was ranked first on the strength of `UncontrolledA2` — the confounded run.
Remove that and nothing puts it at the top.

## What this does NOT establish

Recorded so it is not cited as covered:

- **No export layer — and this is now a known requirement, not a residual.**
  The reassembly race (leg exports arriving relative to the raid create) is
  unmodelled, and the peer-availability grace above is precisely a design that
  turns on it. Live on runap we got the good side of the race. Until the module
  gains a per-leg `exported` flag and an assembly that can produce a proper
  subset of the expected membership, **no A2 run can say anything about the
  grace**, and any run that appears to would be void.
- **The belt is verified in the model, not on a cluster.** No drill has run
  A2, because A2 does not exist. A live gate belongs with the code. (And until
  2026-07-30 it was not verified in the model either — see the void section.)
- ~~**`ensure_raid1_bdev`'s adopt path is analysed but not modelled.**~~
  **Closed** — `AssembleAdopt` / `AssembleValidate` and the three-run matrix
  above. It overturned the requirement this list previously recorded: the
  validating fix flaps, so the belt is the fix and validation is optional.
  ⚠️ **Partially reopened:** the adopt theorems are now gated on
  `A2AdoptArmed`, but its probe witnesses only that *A2* fires. A dedicated
  "`AssembleAdopt` fired" probe needs a ghost the module lacks — `adoptedA2`
  cannot serve, being false both when the adopt never runs and when it runs on
  a non-A2 composition, so probing it would be probing
  `Inv_NoAdoptOfA2Composition` itself. **Treat the adopt theorems as PARTIALLY
  gated.**
- **`Inv_A2NeverServesUnvouched` is in 0 of 74 cfgs.** Defined, never checked.
  Listed here because this document previously asserted the opposite.
- **Bounce arms are not combined with A2.** The four bounce destroyers clear
  `raidHosts` wholesale, which in an A2-armed cfg could tidy away a phantom
  and mask it; the model says so at the site. Narrow those to
  `\ {HostFor(localLegs)}` before combining the tranches.
