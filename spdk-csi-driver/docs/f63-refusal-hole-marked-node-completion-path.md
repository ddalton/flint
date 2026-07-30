# F63 — the consumer can arrive DURING the roll, through the hole in F62's fix

Status: **FOUND BY TLC 2026-07-29, within the hour of F62's fix B being
live-gated on runap. FIXED same session** (`maint_roll.rs`, 19 maint_roll
tests). `maintenance.drainRoll.enabled` remains OFF.

## The one-sentence version

F62's fix B filtered the pending-**selection** path in `plan_roll` and left the
marked-node **completion** path untouched, so a consumer relocating in the
one-tick window between a node's drain and its pod delete gets the pod deleted
anyway — F62, straight through the gap in F62's own fix.

## Why it is reachable

`plan_roll` has two ways to reach `DeletePod`:

```rust
// (1) COMPLETION — finish the node already mid-roll
if let Some(node) = view.marked_nodes.first() {
    ...
    Some(p) if !p.current_rev => RollStep::DeletePod { ... },   // ← no refusal check
}
// (2) SELECTION — pick the next pending node
let mut pending = view.pods.iter()
    .filter(|p| !p.current_rev)
    .filter(|p| !view.local_consumer_nodes.iter().any(|n| n == &p.node_name)); // ← fix B
```

Fix B was applied to (2) only. Path (1) fires for any node holding a live
suppression mark, which is exactly a node between its drain and its delete —
one 60s tick in the shipped cadence, and every campaign passes through it once
per fenced node. A spot reclaim, an eviction, a descheduler or an ordinary
reschedule landing the consumer on that node in that window is enough.

The measured campaign on runap took **one tick** between drain and delete
(`03:34:28` drain → `03:35:28` delete), so the window is not theoretical; it is
simply narrow, which is the worst kind of bug to find by testing.

## What the two obvious answers each cost

| step | consequence |
|---|---|
| `DeletePod` (shipped before this fix) | the tgt dies, the composition dies with it, nothing re-creates it — **F62** |
| keep the marks | `renew_marks` runs on the blocked/refused paths, so a LIVE roller renews this node's suppression **forever** while refusing to roll it: the already-drained leg parks at reduced redundancy permanently — the **MaintPark lasso** (10f), re-created by the refusal |

Neither is acceptable, and the second is the one that would have shipped
quietly: the volume stays up, so nothing alarms, while redundancy is silently
gone for good.

## The fix

Abandon the node:

```rust
Some(p) if !p.current_rev
        && view.local_consumer_nodes.iter().any(|n| n == node) =>
    RollStep::ClearMarks { node: node.clone() },
```

Lift the suppression so hot-rejoin readmits the leg, and let the standing
`RollStep::Refused` report name the node — it is pending and local, so it
appears there on a later tick with its operator-facing event. The volume
returns to full redundancy and the node keeps its old revision until the
consumer moves, which is the documented refusal contract.

## How it was found, and why that matters

By making the consumer **mobile** in the model — constant `LocalLegs` became
variable `localLegs` plus a `RelocateConsumer` action, behind `ConsumerMobile`.
The F62 tranche had held the consumer fixed, which was defensible for the
safety question it was asking and hid this entirely: with `LocalLegs` constant,
no reachable state has a consumer arriving on a node mid-roll.

The trigger was `MaintenanceEventuallyLifts`, an *existing* property, failing
once mobility made the parked-leg state reachable.

This is the exact converse of F62's discovery. F62 was found by running the
code, in a blind spot the model could not see. F63 was found by the model, in a
window a drill would hit only by luck — one tick out of a multi-minute
campaign, requiring a reschedule to coincide with it. Neither tier would have
found both.

## Two model-fidelity bugs the same tranche forced

Both are the same mistake, made twice in one tranche: **bookkeeping keyed on a
remembered event instead of the live condition.**

1. **The eligibility gate** read the remembered `maintSkipped` set, making a
   refusal permanent. The shipped code rebuilds `local_consumer_nodes` from the
   gather every tick — which is why runap rolled the refused node 14 seconds
   after its consumer left. The model was **weaker than the implementation**,
   the direction that lets a regression land green.
2. **The refusal surfacing** happened only on the skip path, and
   `MaintDrainSkip` requires `l \notin processed`. So a node drained while
   remotely consumed, onto which the consumer then relocated, was refused but
   never recorded — poisoning the in-flight gate
   `processed \subseteq (rolled \cup maintSkipped)` so that no further node
   could drain at all. A campaign killed by its own bookkeeping.

Worth a standing check on this module: any set consulted for a decision should
be re-derived from observation, not remembered, unless there is a stated reason.

## Gate

`FlintReplicationRefusalClears.cfg` / `FlintReplicationRefusalSticky.cfg` —
an A/B on `Inv_RefusalNeverClears`, stated as **reachability** because every
liveness form of the question measured the environment instead of the roller
(a dead leg, the freshness gate's correct Defer, an oscillating consumer, and
finally the drain belt refusing the last serving member). Shipped gate must
VIOLATE it; the remembered gate must HOLD it, and that green is the bug.

A note on the sticky side: it carries `Inv_RefusalNeverClears` alone, because
with the outage theorem also armed it fails on *that* first. That is a second,
separate indictment of a remembered gate — a memory has a window before it is
written, and in that window the gate fails to refuse at all.
