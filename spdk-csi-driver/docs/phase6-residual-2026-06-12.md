# Phase 6 — Tier-1 residual measurement and cutover validation

**Date:** 2026-06-12 (single session, follows `e2e-campaign-2026-06-12.md`)
**Cluster:** trove-provisioned `runh`, 4× AWS spot workers + 1 CP (one worker
replaced mid-session after a spot retirement), Kubernetes v1.34.9, chart
`flint-csi-driver-chart:1.2.0`, driver `:latest` dev builds from `main`
(bounded umount + orphan sweep + pin-until-admission + pin-advance).
**Knobs:** `FLINT_EPOCH_SCHEDULER=enabled` (30 s), `FLINT_CATCHUP=enabled`,
`FLINT_CUTOVER=enabled` — cutover's first cluster run ever.
**Workload:** 2-replica RWO volume (`numReplicas: 2` SC), Deployment with a
2 s fsync writer, PV annotated `flint.csi.storage.io/rejoin-bounce: enabled`.
**Method:** kill the replica-hosting flint node pod (spdk-tgt dies with it),
track the PV sync record + workload pods at 10–20 s granularity, reconstruct
from controller logs and PV events.

## Headline numbers

The fully autonomous heal (round 3, raid live on the consumer, bounce
rescheduled cross-node):

| T+ | event |
|---|---|
| 0:00 | replica leg killed |
| 0:57 | health monitor marks `stale` (60 s tick) |
| 1:17 | catch-up → `standby`, pin at the revert base |
| ~1:50 | chase advances; pin advances with the mark; epochs hold at K=6 |
| 2:09 | cutover bounces the workload (52 s after standby-ready) |
| ~2:50 | reassembly on the new node; fenced final delta; admitted **no rebuild** |
| 2:58 | `in_sync`, pin released, epochs settle at K |

**Tier-1 residual, effective bounce: ~3 minutes**, composed almost entirely
of detection latency (three independent 60 s ticks: health monitor,
catch-up, cutover planner) — data movement is seconds at this volume size.
The floor is tunable: tighter ticks buy a sub-minute residual at the cost of
control-plane chatter.

**Tier-1 residual, ineffective bounce: unbounded.** When the scheduler
re-places the workload pod on the same node (round 2: a single-pod
Deployment with no spread constraints — a coin flip or worse), kubelet
reuses the staged raid, no reassembly happens, no admission runs, and the
standby chases indefinitely. The verifier emits `CutoverIneffective` at the
900 s cooldown (validated live, correct diagnosis text) and retries — same
odds each round. Round 2 needed a manual cordon (standing in for the §6
scheduling escalation, deliberately unimplemented) before the retry bounce
admitted in 44 s. **The residual distribution is bimodal: ~3 min or
cooldown-multiples, decided by the scheduler.**

## What was validated live (first time for each)

- **Cutover RWO bounce** (`rejoin-bounce` annotation): plans within one tick
  of standby-readiness, bounces only the claim's pods, and the post-bounce
  verification correctly distinguishes success from no-op.
- **`CutoverIneffective`** path end-to-end, including retry eligibility.
- **Admission at reassembly** on three distinct topologies: remote node,
  the standby's own node (local attach + `clear_head_sb`), and the
  fresh-replacement node — all "in_sync, no rebuild".
- **Pin lifecycle (both 2026-06-12 fixes)**: held through standby (no GC
  grind — zero retention warnings over a 15-minute standby soak), advanced
  with the chase mark (epochs never exceeded K+1; before the advance fix
  the same scenario grew 23 epochs in 18 minutes), released at admission.
- **Bounded unstage umount (campaign bug #4 fix)** against a genuinely dead
  mount during recovery — returned promptly, node plugin stayed live,
  restage completed.
- **Data integrity** across five failure/restage cycles: every acknowledged
  write before a data-path death survived; writes EIO'd into a dead mount
  were lost (expected; the writer saw the errors).

## Bugs found (the phase's yield)

1. **Consumer-side spdk-tgt restart leaves the workload on EIO and the
   control plane blind.** A node-pod roll on the consumer destroys the
   assembled raid and the volume's frontend; the mounted filesystem returns
   I/O errors. Nothing detects it: the health monitor's stale predicate
   requires an *online raid missing a base* — with no raid at all it never
   fires. The record stayed `in_sync` and the epoch scheduler kept cutting
   snapshots of the frozen image for ~15 minutes until manually noticed;
   recovery was a workload pod bounce (restage rebuilt the raid; all
   pre-death data intact). Needed: the node agent (or health monitor)
   should treat *attached volume with no raid bdev on the attachment's
   node* as degraded — emit the event, and plausibly feed the same bounce
   machinery cutover uses. Until then, every consumer-node driver upgrade
   is a silent outage for its staged volumes.
2. **Dashboard backend crash-loops when a registered node is unreachable**
   (spot retirement): disk-fetch and orphan-scan paths hang on
   unreachable-host timeouts long enough that the liveness probe kills the
   backend repeatedly. Needs bounded per-node timeouts (the parallel fetch
   has them; the orphan scan does not).
3. **Dashboard frontend silently falls back to mock data** when the backend
   is down (502) — the same fallback family `c20711e` removed for disk
   status survives at the top-level dashboard fetch. An outage rendered as
   healthy-looking fake data is worse than an error page.

## Scale assessment (hundreds of volumes)

The per-volume machinery is sound at scale: independent state machines in
PV annotations, idempotent/convergent operations, resumable copies. What is
missing is flow control above it, none of which Tier 2 changes:

- **No global catch-up concurrency cap**: a returned node hosting replicas
  of N volumes triggers N concurrent bulk copies onto one disk.
- **Bounce coordination**: many ready standbys → many workload bounces in a
  window; ineffective bounces retry forever at scale (the bimodal residual
  multiplied); the scheduling escalation stops being optional.
- **API-server load**: node-agent reconcile (and the orphan sweep) full-list
  PVs every 60 s per node; the epoch scheduler loop already showed cadence
  stretch (60 s effective vs 30 s configured) at single-volume scale.
- **Fleet observability**: event timelines don't aggregate; lag/pin-age/
  epoch-count metrics (§6, deferred) become prerequisites — an epoch-list
  length metric would have caught the pin-advance bug by itself.

## Tier-2 decision input (§9-6 → §9-7)

Tier 1 with an effective bounce: ~3 min residual, one workload restart per
heal. Tier 1's failure mode: scheduler-dependent unbounded residual, fixable
with scheduling escalation at the cost of more cutover machinery. Tier 2
(`skip_rebuild` hot rejoin) deletes the bounce apparatus entirely — no
workload restart, scheduler-independent, residual = detection ticks + final
delta + a quiesce measured in metadata ops. The catch-up/epoch machinery is
identical under both; Tier 2 replaces only the admission transport.

**Recommendation:** implement the cheap Tier-1 hardening now (consumer
blindness fix above; pod anti-affinity hint or cordon-lite escalation on
`CutoverIneffective`), and proceed with the Tier-2 patch evaluation —
the bimodal residual and the per-heal workload restart are structural to
Tier 1, both disappear with the one verified primitive, and the §7 patch
shape is already traced. RWX (NFS-pod bounce, detach-awaited — should not
have the same-node mode) remains to be validated before the RWX story
rides on cutover.
