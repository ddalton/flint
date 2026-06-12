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
   control plane blind.** *(Layers 0+1 fixed and cluster-validated later
   the same day — see "Consumer-blindness fix" below.)* A node-pod roll on the consumer destroys the
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

## Consumer-blindness fix (landed same day, layers 0+1 of 4)

Designing the fix surfaced a deeper bug and a layered plan. An in-place
repair experiment (hand-rebuilding the consumer raid + export after a
lone spdk-tgt kill) failed productively, teaching three things: disk
attach ran only at DRIVER-container startup, so a lone spdk-tgt restart
(liveness kill, OOM) bricked the node's entire storage — replicas
included — until the whole pod was recreated (5.5 min observed just to
reload, and only with help); a PARTIAL export rebuild is worse than none
(a listener over a namespace-less subsystem makes the kernel initiator
conclude the namespace is deleted and kill the device — listener must go
last, which the convergent export module already orders); and the
namespace-identity question (does the kernel reattach a rebuilt
namespace with a new UUID?) remains open, needing deterministic NGUIDs
pinned at stage before it can be retested.

- **Layer 0 — storage-baseline reconcile (landed):** the 30 s discovery
  loop detects the disk-count collapse and re-runs discovery with
  auto-recovery, re-attaching initialized disks and reloading the
  lvstore; reconcile re-exports on its next tick. Validated live: lone
  spdk-tgt kill → baseline recovered in **50 s** (previously: bricked
  indefinitely).
- **Layer 1 — detection (landed):** the 60 s monitor tick flags any
  volume ATTACHED to this node whose raid bdev is missing — the case
  the health monitor's stale predicate (online-raid-missing-a-base)
  cannot see. Three consecutive strikes (rides out in-flight stages) →
  `flint.csi.storage.io/data-path-lost: <node>` PV annotation +
  `VolumeDataPathLost` Warning with the remediation in the message; the
  flagging node clears it (+`VolumeDataPathRestored`) when the raid
  returns or the attachment leaves. Validated live: flag at T+3m07s,
  bounce → restage → flag self-cleared, data intact (the workload
  STALLED rather than EIO'd this time — layer 0 restored the target
  fast enough that the kernel kept queueing within its reconnect
  window).
- **Layer 2 — in-place repair (landed same day):** when detection
  confirms a loss (3 strikes), the agent rebuilds the raid (the same
  sync-record-aware assembly NodeStage uses) and re-exports the
  loopback subsystem via the convergent module — same NQN, stable
  serial, and a **pinned namespace identity** (`stable_ns_identity`:
  deterministic UUID/NGUID from the volume id, set at stage and at
  repair) so the kernel initiator revalidates the namespace and
  reattaches. Guards: refuses ublk frontends (device node dies with
  spdk; restage only), refuses volumes kubelet does not have staged
  here (`vol_data.json` check — a lingering mid-detach VA must not
  spawn a zombie raid; observed live during a handover before the
  guard existed, self-resolved by the in-flight unstage), and tears
  its raid back down if the attachment leaves mid-repair. Detection
  flags only when repair fails, and keeps retrying every tick.
  **Validated end-to-end with forced direct I/O**: lone spdk-tgt kill
  → repair at T+2m23s → 16 KiB `O_DIRECT` write + synced append/read-
  back through the rebuilt path succeeded in the SAME pod, zero
  restarts — a ~3-minute I/O stall the workload slept through.
  Validation lessons, recorded for posterity: (a) cached reads lie —
  two earlier "successful" rounds were page-cache illusions over a
  dead mount; only direct I/O or synced write-then-read-back proves a
  data path; (b) **the pinning migration is itself a hazard for
  volumes staged pre-pinning**: re-exporting a namespace with changed
  identity under a still-connected kernel controller makes the kernel
  delete the old device node (journal abort under any mount on it)
  and mint a new one — each existing volume needs one full
  detach/restage cycle (or a workload bounce) to cross onto pinned
  identity safely.
- **Layer 3 — bounce fallback (landed same day):** the cutover loop
  consumes the annotation for what repair can't reach (ublk frontends,
  aborted filesystems, expired reconnect windows; also reachable via
  the `FLINT_DATA_PATH_REPAIR=disabled` escape hatch). The planner's
  data-path branch bypasses the standby/lag gates — the bounce IS the
  remediation, restage rebuilds from in-sync replicas — with the same
  policy split (RWX: NFS-pod bounce; RWO: `rejoin-bounce` opt-in,
  otherwise surfaced for the operator). A 90 s debounce keeps a
  transient repair failure from costing a bounce. Verification judges
  these attempts by the annotation clearing (the agent clears it when
  the raid is back), with the cooldown and a distinct
  `CutoverIneffective` diagnosis. Validated live with repair disabled:
  flag at T+2m33s → bounce → same-node reuse defeated it →
  `CutoverIneffective` (correct message) → cordon + retry bounce →
  cross-node restage → flag cleared 16 s later → `CutoverSucceeded`
  ("data path restored") → direct-I/O probe green. **The same-node
  reschedule race defeats data-path bounces exactly as it defeats
  admission bounces** — closed the same day by the scheduling
  escalation below.

## Scheduling escalation (landed same day)

Every bounce now applies a self-expiring NoSchedule taint
(`flint.csi.storage.io/bounce`) to the bounced workload's node before
deleting pods, so the replacement cannot reuse the dead/stale staged
volume. A taint rather than cordon (operator cordon state is never
touched) or pod anti-affinity (RWO replacements come from the
workload's own controller template, which flint cannot mutate). The
application time is encoded in the taint value, so expiry
(`FLINT_CUTOVER_TAINT_SECS`, default 120 s) survives controller
restarts — the tick sweeps expired taints even with escalation
disabled (`FLINT_CUTOVER_ESCALATION=disabled`). On a cluster with no
alternative node the taint still works: it outlives kubelet's unstage,
so even a same-node landing must restage. Validated live with repair
disabled and NO manual cordon: flag → taint at the debounce boundary →
replacement steered off-node → restage → flag cleared **on the first
bounce attempt** → taint auto-expired and swept. The bimodal residual
is closed: bounces are now deterministic for both admission and
data-path cases. As an unplanned bonus, the post-validation DaemonSet
roll restarted the consumer's spdk-tgt mid-probe: a 16 KiB `O_DIRECT`
write stalled 148 s, the re-enabled in-place repair rebuilt the path,
and the write completed — no error, no restart — the full layer stack
riding through a routine driver upgrade.

**Recommendation:** implement the cheap Tier-1 hardening now (consumer
blindness layers 2+3 above; pod anti-affinity hint or cordon-lite escalation on
`CutoverIneffective`), and proceed with the Tier-2 patch evaluation —
the bimodal residual and the per-heal workload restart are structural to
Tier 1, both disappear with the one verified primitive, and the §7 patch
shape is already traced. RWX (NFS-pod bounce, detach-awaited — should not
have the same-node mode) remains to be validated before the RWX story
rides on cutover.
