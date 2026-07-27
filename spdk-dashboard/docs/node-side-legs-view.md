# Node-side legs view — read-only replica visibility per node

**Status:** proposed 2026-07-26, not scheduled. Read-only dashboard work;
no correctness surface, gates nothing and is gated by nothing. Split out of
`spdk-csi-driver/docs/f43-rwx-replacement-admission.md` — it surfaced while
discussing node lifecycle but shares no code path or ordering constraint with
the R2 claim-arbitration scope.

## The gap is directional, not total

The volume→replica direction is already served. `VolumeDetailAPI.tsx:101-150`
renders a per-replica table — node, sync state, last epoch, epoch lag vs
current, since, reason — projected from the PV `replica-sync-state` record by
`spdk_dashboard_backend_minimal.rs:1061-1100`.

The node→legs direction stops at **aggregate counts**. `NodeSummary`
(`spdk_dashboard_backend_minimal.rs:285-304`) carries `volumes_total`,
`replicas_out_of_sync`, `volumes_not_healthy`, `capacity_gb`, `allocated_gb`;
`NodesFleetView.tsx:74-84` builds a volumes-by-node map for filtering, and
`NodeDetailView.tsx` renders a filtered volume list. So the dashboard can say
*"2 replicas out of sync on this node"* but not **which legs, in what state, or
whether this node holds the last `in_sync` copy of anything.**

## Why that specific question matters

"What do I lose if I terminate or drain this node?" is what every node
lifecycle action turns on, and it is the one question the dashboard cannot
answer today — throughout the 2026-07 attach/detach campaign it was answered by
hand, reading PV annotations.

It is also the natural progress surface for the `out-of-service` taint feed
(v1.20.0 item #3 in the F43 doc): the taint is the trigger, this view is how an
operator watches legs drain off a node.

## Scope — two additive pieces

**1. Per-leg rows on node detail.** The volume-side columns, inverted (volume,
sync state, last epoch, lag, since, reason), plus the derived flag that carries
the actual blast radius: *this node holds the only `in_sync` replica of volume
X*. Computable from the record already projected — `:1100` filters the
`in_sync` set.

**2. Space attribution rollup.** `allocated_gb` is one number today; split it by
category — heads, retained epoch snapshots, user snapshots, orphan-sweep
candidates. Per-lvol consumed bytes already exist (`:2415-2420`,
`allocated_clusters * cluster_size`); the work is classification via the
existing flint-shape parsers in `orphan_sweep.rs` plus a rollup. Answers "what
is eating capacity on this node", which has no answer at any layer today.

Both are projections over records the backend already parses: no new RPCs, no
new node-agent calls.

## TODO — investigate UX of the affected pages before adding rows

The three surfaces this touches (`NodesFleetView.tsx`, `NodeDetailView.tsx`,
`VolumeDetailAPI.tsx`) predate most of the design-system work and have grown by
accretion. **Review them as pages before bolting another table onto node
detail** — the risk is that a legs table lands next to the existing volume list
and the disk detail without any of them being the obvious place to look.

Open questions worth answering first:

- Node detail already renders per-disk and per-volume sections plus a filtered
  volume list. Where do legs belong relative to those, and is the
  volume-list/legs-list distinction legible to an operator, or are they the
  same thing shown twice?
- The blast-radius flag (*last `in_sync` copy*) is the highest-value datum on
  the page. Does it want to be a row-level chip, a page-level banner, or a
  fleet-view column so it is visible **before** drilling in?
- `NodesFleetView` is the problems-first list. Should "holds a last-copy leg"
  be a first-class condition there alongside the existing health rollup?
- Space attribution is a second table's worth of data. Chart, breakdown bar, or
  expandable rows — and does it belong on node detail at all, or on the disks
  surface?
- Check consistency with the design-system waves in `improvement-plan.md`
  (type ramp, Chip adoption, semantic colors, Button primitive) — these pages
  should come out of this work more consistent, not less.

## Read-only by design — do NOT add per-replica action buttons

Settled 2026-07-26. Recorded here with the derivation so it is not reopened.

The mechanism a "delete/rebuild this replica" control would expose **already
exists de facto**: deleting a head lvol on an attached volume self-heals in
~2 minutes (monitor mark-stale → catch-up delta rebuild *in place*, cheap
because deleting the head leaves its parent epoch snapshots intact, so
`select_base_epoch` still finds a shared base). Every justification for
surfacing it fails on inspection:

- **Suspected divergence** — no scrub or checksum exists, so the operator has
  no signal that would ever motivate the click; genuine divergence already
  self-marks stale (`node_agent.rs:4231`).
- **Unstick a wedged catch-up** — the automation already self-escapes: an
  uncovered base emits `ReplicaCatchupBaseUncovered` and falls back to the
  thin-aware full build (`catchup.rs:1615-1637`), as does no-shared-history.
- **Reclaim space** — it does the opposite. A full build replays the source
  lineage and leaves the old chain orphaned until epoch GC (`catchup.rs:1053`),
  so footprint rises before it falls; user snapshots are preserved by design
  (§11). A delta rebuild only resets the head's post-epoch allocation, bounded
  by the epoch interval.
- **Relocate a leg** — a different feature, blocked on R2 claim arbitration and
  on an acked-tail story. raid1 cannot hold a transient N+1 leg
  (`bdev_raid_add_base_bdev` returns `-EINVAL` "no empty slot found",
  `bdev_raid.c:3681`; the slot array is `calloc(num_base_bdevs)` at `:1599` and
  no grow RPC exists in v26.05), so relocate is necessarily retire-then-add
  with a real degraded window.

Net effect of shipping the button anyway: it spends a genuine redundancy window
for nothing measurable. Diagnosis belongs in the dashboard; mutation stays with
the orchestrators.
