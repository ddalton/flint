# F56 — partial expand fan-out × §5 chase = permanent size livelock

**Found 2026-07-28 by the formal-models evaluation (no cluster, no drill —
the interleaving walk that asked "does a mid-fan-out leg loss converge?"),
confirmed by sim composition test, fixed same day (catch-up size
alignment). Live gate owed: drill 2.10 kill-mid-fan-out variant.**

## The wedge

A replicated volume expands by fanning `bdev_lvol_resize` across every
leg under the C2 belt ("all replicas in_sync"). If one leg's node dies —
or its agent is just unreachable — **inside the fan-out window**, the
survivors are left at the NEW size while the lost leg holds the OLD, and
the system then livelocks on four individually-correct mechanisms:

1. **The C2 belt** refuses every expand retry while the leg is
   stale/standby (`ExpandRefusedReplicasNotInSync`) — so nothing ever
   re-drives the missing resize. `resize_lvol` has no caller outside the
   expand paths.
2. **The §5 chase** heals the returning leg's *content* but not its
   *size*: `revert_head` re-creates the head as a clone of the replica's
   **own local base-epoch snapshot** — pre-expand size — and the chase
   copies deltas into it.
3. **The admission size guard** (F43 item #8, `catchup.rs`) then defers
   the short head before `record_in_sync` — correctly; admitting it
   would shrink the device — as an "ordinary deferral: the replica keeps
   chasing". It keeps chasing at the same size forever.
4. **The retention pin** — taken by the chase to protect its own
   foundation — prevents epoch GC from retiring the base epoch, which is
   the one event that would have forced the *self-healing* path: the
   §9-5 full build sizes its empty head **from the source head exactly**
   (`catchup.rs` `revert_head_to_empty`). The escape hatch is pinned
   shut by a correct mechanism.

Net: permanent redundancy loss (r2 serving as r1), a never-completing
expansion, and a revert→chase→defer loop burning copy bandwidth every
reassembly tick. No data loss — the guards holding is them working as
designed. PV `spec.capacity` never moves (external-resizer bumps it only
after `ControllerExpandVolume` succeeds), so even manual leg replacement
keeps converging on the OLD size estimate until an operator intervenes
by hand (`resize_lvol` via the node agent, or deleting the replica).

## Why three live campaigns never saw it

The 2.10 degraded-refusal drill tests the **safe order**: degrade first,
then expand — the belt refuses *before* any resize, no divergence is
ever created, and the drill (correctly) passes with self-completion
after rejoin. The wedge needs the failure *inside* the seconds-wide
fan-out window: fail one leg's resize, then lose the leg before the
resizer's retry lands. runak's 2.9 "concurrent expand" was exonerated
because its expand never even acquired the claim.

## Why replacement does NOT need a fix

A freshly-placed replacement leg is write-virgin → full-build path →
head recreated at the **source's** size regardless of the placeholder
lvol (which `replica_replace` sizes from the stale PV capacity and the
full build deletes). The only residue is the placement room-check using
the pre-expand capacity — advisory for thin provisioning; not changed.
The wedge is exclusive to the **returning** leg (§5 chase, base epoch
intact) — precisely the common case: spot reclaim + return, brief agent
outage, the mid-expand transient itself.

## Fix — grow-only size alignment in catch-up (`align_dst_head_size`)

Size divergence is just another form of staleness, so catch-up owns
healing it. After the §5 revert (or the resumed write-virgin head) picks
the live head — and again as a belt in `admit_one_standby` before its
final chase — the head is probed against the copy source and grown to
match via `bdev_lvol_resize` (`StandbyHeadGrown` event):

- **Grow-only by construction** (fires only when dst < src; a longer leg
  is capped by SPDK at the raid's data size).
- **Pre-attach**, so the copy attachment never races the nvmf resize
  AEN chain.
- **Unknown sizes pass through untouched** and a **failed grow defers**
  — the admission size guard remains the belt either way, exactly as
  before. The guard's "unreachable today" comment is true again for the
  expansion case.

Expansion itself is untouched: the C2 belt's ordering stays load-bearing
and live-validated.

## Verification

- **Sim composition** (`catchup.rs`
  `f56_partial_expand_then_stale_leg_converges_end_to_end`): the full
  loop — partial fan-out (survivor grows, leg agent dead), belt refusal,
  stale mark, §5 chase with alignment, admission, expand retry
  completes on every leg by live uuid. **Ran RED against pre-fix code**
  (the livelock demonstrated: `StandbyAdmissionDeferred` with zero
  admissions after a completed chase); green with the fix.
- **Belt integrity** (`admission_still_defers_when_the_size_alignment_grow_fails`):
  a failed grow defers, never admits short.
- **Fan-out unit coverage** (`expand.rs`, extracted from `main.rs` for
  the sim tier): belt-before-any-resize, partial application pinned
  (the F56 precondition), live-uuid addressing, shrink/no-op guards.
- **Formal**: FlintReplication size-dimension tranche (`legSize` per
  leg + `raidSize` device high-water mark behind `ExpandEnabled`; gate
  runs 5n–5p). The ExpandWedge run (SizeHeal=FALSE, the shipped code)
  independently rediscovered the wedge as a required liveness lasso —
  blackhole mid-fan-out, belt passes on the lagging record, survivor
  grows, device grows, hot-rejoin at old size, chase-defer forever. The
  strict run proves the fixed world converges (`ExpansionCompletes`,
  the module's first per-leg progress theorem) and the guard mutation
  proves F43 item #8 is load-bearing (`Inv_NoDeviceShrink`). Building
  the property also flushed out: a ghost-epoch model bug (EpochCut cut
  the acked ledger, not held content), the missing release-on-deferral
  of the admission claim, the same-class-claimant WF trap (the resizer
  is now modeled with strong fairness — the persistent-retrier
  abstraction), and **candidate F57** (below). See `formal/README.md`.

## Candidate F57 (surfaced by the model, unfixed)

The strict run's honest escape hatch documents a neighboring gap: a
STANDBY whose node dies parks forever. The only standby→stale demotion
is chase-source exhaustion (`ReplicaChaseSourcesExhausted`), the
raid-health monitor marks only raid *members*, and `replica_replace`
filters on `Stale` (and on `hot_rejoin.is_none()`), so a dead
mid-rebuild standby is neither demoted nor replaced — the volume sits
at reduced redundancy until an operator intervenes. Needs a sim repro
and a fix (demote a standby on node-gone evidence, mirroring the
stale path); tracked for the next cycle.

## Ops notes (pre-fix versions)

Symptom signature: `ExpandRefusedReplicasNotInSync` events repeating
alongside `StandbyAdmissionDeferred` naming a size mismatch
("standby head is NB but its copy source is MB"). Manual escape:
`POST /api/volumes/resize_lvol` on the lagging leg's node agent with the
survivors' size (grow-only!), or delete the replica record entry to
force re-placement (full build sizes from the source).
