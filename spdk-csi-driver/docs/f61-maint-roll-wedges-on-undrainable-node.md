# F61 — the maintenance roll WEDGES on any node with no drainable legs

Status: **FOUND LIVE 2026-07-30 on runao (driver `1.22.0-rc4`), first ever
execution of drill 3.14.** Not fixed. `maintenance.drainRoll.enabled` must
stay **OFF**. This is a liveness bug in shipped code, in the one area the
formal model deliberately does not cover.

## Symptom

A csi-node roll driven by the maintenance roller never converges. The
roller logs, once per 60s tick, forever:

```
[MAINT] drain pass complete (pod delete on a later tick) node=runao-aws-4 drained=0
```

The node's pod is never deleted and stays on the old ControllerRevision
indefinitely. Observed on runao: `runao-aws-4` still on `7d8b677456`
(created 23:43:11Z) at 01:00:56Z, ~18 minutes and 3+ ticks after the
roller reached it, while `runao-aws-1785372779` had already rolled to
`6b8b764b59`.

## Root cause

`plan_roll` (maint_roll.rs) can only reach `RollStep::DeletePod` through
the marked-node branch:

```rust
if let Some(node) = view.marked_nodes.first() {
    ...
    Some(p) if !p.current_rev => RollStep::DeletePod { .. },
    ...
}
// No marks: pick the next pending node, behind the barrier.
...
RollStep::Drain { node: next.node_name.clone() }   // <- the ONLY other outcome
```

The drain stamps a suppression mark **per drained volume**. A node whose
volumes are *all* skipped by the drain therefore produces **zero marks**,
so `marked_nodes` never contains it, so the planner takes the no-marks
path — which unconditionally returns `Drain`. The next tick drains
nothing again. Livelock.

The drain skips a volume in three cases, all deliberate:

1. **`consumer == node`** — the serving raid lives on the rolling node
   (the local half). Emits `MaintenanceLocalConsumer` and `continue`s.
2. **unattached volume** — no serving raid to drain from.
3. **no legs on the node at all** — nothing to iterate.

So the wedge is *guaranteed*, not incidental, whenever a pending node
matches. Two shapes hit it on essentially every cluster:

- **The node hosting an RWX serving raid.** For any RWX volume the
  consumer IS a node in the fleet, so that node can never be rolled.
  This is what runao hit.
- **A control plane running the csi-node DS with no initialized disks.**
  It has a pod (so it is "pending" after a template change) and no legs
  (so the drain stamps nothing). Note `pending` is sorted by node name,
  so whichever such node sorts first wedges the campaign for everyone
  behind it.

## Why the model missed it

`docs/maintenance-drain-csi-node-roll.md` states the local half is
explicitly **not modelled** ("the kernel-level half is explicitly NOT
modelable and is drill-gated instead"). The consequence was subtler than
intended: it is not only the *data-path* continuity that went unmodelled
but the *planner's control flow* for a node whose drain is a no-op. In
`FlintReplication.tla` every leg node has legs and `MaintDrain` always
marks, so `marked_nodes` is never empty for a pending node and the
DeletePod step is always reachable. The liveness properties hold in that
world. There is no state in the model where a node is pending *and*
undrainable — the abstraction had no room for one.

This is the same lesson as the pod-layer tranche, one level down: **the
abstraction was the bug.** There the missing concept was a second creator
of the nfs pod; here it is a node whose drain legitimately does nothing.

## Blast radius

- The DS never converges: an operator's `helm upgrade` silently never
  finishes on the affected nodes. `kubectl rollout status` is not even
  available as a signal, because OnDelete rolls do not report progress.
- **It is not "the affected node", it is the WHOLE FLEET** — measured on
  runap 2026-07-29, which reproduced F61 independently on rc4 with a
  sharper signature than runao's. `plan_roll` picks `pending.first()` from
  a name-sorted list and returns `Drain` for it forever, so one
  undrainable node starves every node behind it in the campaign order:

  ```
  03:23:19  [MAINT] Maintenance roller started
  03:23:19  [MAINT] drain pass complete … node=runap-aws-1 drained=0
  03:24:19  [MAINT] drain pass complete … node=runap-aws-1 drained=0
  …8 consecutive ticks, one per 60s, same node, zero pods rolled…
  ```

  All five csi-node pods sat on the previous revision for the whole
  window while the template said otherwise. `runap-aws-1` happened to be
  both the alphabetically-first node and the volume's consumer, which is
  what makes the trace so legible — but the ordering is incidental and
  any undrainable node blocks everything after it.

  This is also the sharpest argument for the SHAPE of the F62 fix: the
  refusal must SKIP the node (`RollStep::Refused` + the planner filtering
  it out of `pending`) rather than block on it. A refusal that blocked
  would reproduce exactly the trace above with better manners.
- It is the **wedge the design claimed to retire**. The doc's argument
  against the "delete the Node object to unblock a wedged roll" recipe
  was that "a roll protocol that cannot wedge retires that recipe". This
  protocol can wedge, for a different reason than the old one.
- **Safe, though.** Nothing is drained, nothing is restarted, redundancy
  is untouched. On runao pg-0 stayed 2/2 with 0 restarts throughout. The
  failure mode is "nothing happens", not "data is lost".

## Fix sketch (not implemented)

The planner needs a route to `DeletePod` for a node that is pending and
has nothing to drain. Options, roughly in order of preference:

1. **Make "drained" an explicit per-node conclusion, not an inference
   from marks.** After a drain pass, record that the node was *processed*
   (even with `drained=0`) and let `plan_roll` key DeletePod on that
   rather than on `marked_nodes`. Marks stay what they are for: excluding
   legs from readmission.
2. Add `undrainable_nodes` to `RollView` (pending, and every volume on it
   skipped) and allow DeletePod for them directly. Cheap, but adds a
   second predicate the model must learn.
3. Exclude the DS from CP/legless nodes entirely (`nodeAffinity`), which
   removes shape 2 but **not** shape 1 — the RWX-serving-node case is the
   important one and remains.

Whichever is chosen, the model needs a node that is pending and
undrainable, plus a liveness property that the campaign terminates:
something like `RollCampaignCompletes == <>[](pendingNodes = {})` under a
fairness assumption, checked in a world where at least one node's drain
is a no-op. Today no run can fail that way.

## Drill note

Drill 3.14's campaign-completion assertion catches this correctly — it is
the assertion that fails:

```
3.14 FAIL (campaign): the roll never reached one new revision with all node
pods Ready inside ${ROLL_BUDGET}s — a roller that cannot finish is the wedge
the design set out to retire
```

The drill also cannot measure the **local half** until this is fixed: the
pod delete never happens, so the tgt under the serving raid is never
restarted, so staged-device continuity is never exercised. F61 blocks
that measurement.
