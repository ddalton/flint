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

### Finding 4 — in the default configuration, A1 is the only thing holding

Three runs over `TgtDie`, no roller involved:

| cfg | repair | result | meaning |
|---|---|---|---|
| `UncontrolledBlind` | none | `Inv_RaidRecoveryUnreachable` **holds** | no interleaving recovers the volume — **permanently down** |
| `UncontrolledA1` | A1 | **violated** | the seeded detector's ladder recovers it |
| `UncontrolledA2` | A2, `MaxBounces = 0` | **violated** | recovered with no bounce, no pod delete, no unstage |

The green one is the indictment (non-vacuity: `FlintA2ProbeDeath.cfg` must
violate `ProbeTgtDeathReachable`, so the death really happens).

**A1 has already shipped, and this says it is doing far more than "supporting"
work** — in the default configuration it is the only thing standing between a
routine `helm upgrade` and a permanent outage. That is the load-bearing result
for disposition, and it is an argument for leaving the drain-roll flag off
rather than rushing it on.

A2's row is the one that never disrupts the consumer: A1's path is a repair
(strike → flag → controller data-path arm → `BounceNfsPod` → NodeStage), so
the volume goes down and comes back. A2 re-creates in place.

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

## Implementation sketch

In `rehydrate_exports_from_ground_truth` (`node_agent.rs`), the
`replica_count > 1` branch currently only seeds detectors (A1). Extend it:

1. Guard on `va_map.get(&pv_name) == Some(&self.node_name)` — already there.
2. **Add the belt**: this node's kubelet-level staging still names the volume.
   Local, observable without the API server, nothing remembered.
3. Attach legs (local lvol + remote NVMe-oF), then `bdev_raid_create` with
   `"superblock": false` — never a superblock; that trades this outage for the
   §3 phantom-assembly class plus the 1 MiB payload shift that silently
   formatted restored snapshots (2026-06-12).
4. Serve only what the records vouch for as `in_sync` — assembling a leg the
   records do not vouch for is the phantom class by another route
   (`Inv_A2NeverServesUnvouched` is in the gate waiting for it).
5. Re-create the ublk chain, as the single-replica path already does.

## What this does NOT establish

Recorded so it is not cited as covered:

- **No export layer.** The reassembly race — leg exports arriving relative to
  the raid create — is unmodelled. Live on runap we got the good side of it.
- **The belt is verified in the model, not on a cluster.** No drill has run
  A2, because A2 does not exist. A live gate belongs with the code.
- ~~**`ensure_raid1_bdev`'s adopt path is analysed but not modelled.**~~
  **Closed** — `AssembleAdopt` / `AssembleValidate` and the three-run matrix
  above. It overturned the requirement this list previously recorded: the
  validating fix flaps, so the belt is the fix and validation is optional.
- **Bounce arms are not combined with A2.** The four bounce destroyers clear
  `raidHosts` wholesale, which in an A2-armed cfg could tidy away a phantom
  and mask it; the model says so at the site. Narrow those to
  `\ {HostFor(localLegs)}` before combining the tranches.
